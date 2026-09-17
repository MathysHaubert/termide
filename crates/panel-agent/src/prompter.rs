//! Permission prompts across the thread boundary.
//!
//! The agent thread blocks inside `before_tool_call` until the user answers.
//! [`ChannelPrompter`] ships the request to the panel over a channel and
//! waits for the reply, waking up regularly to notice an abort so a
//! cancelled run does not hang on an unanswered prompt.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use termide_agent_core::{CancelToken, PermissionAnswer, PermissionPrompter, PermissionRequest};

/// One outstanding prompt: the request and the channel for its answer.
pub struct PermissionEnvelope {
    pub id: u64,
    pub request: PermissionRequest,
    pub reply: Sender<PermissionAnswer>,
}

pub struct ChannelPrompter {
    tx: Sender<PermissionEnvelope>,
    cancel: CancelToken,
    next_id: u64,
}

/// Build a prompter and the receiver the panel polls from `tick()`.
pub fn channel(cancel: CancelToken) -> (ChannelPrompter, Receiver<PermissionEnvelope>) {
    let (tx, rx) = mpsc::channel();
    (
        ChannelPrompter {
            tx,
            cancel,
            next_id: 0,
        },
        rx,
    )
}

impl PermissionPrompter for ChannelPrompter {
    fn ask(&mut self, request: &PermissionRequest) -> PermissionAnswer {
        let (reply, answer) = mpsc::channel();
        self.next_id += 1;
        let envelope = PermissionEnvelope {
            id: self.next_id,
            request: request.clone(),
            reply,
        };
        if self.tx.send(envelope).is_err() {
            // The panel is gone; nobody can approve anything.
            return PermissionAnswer::Deny;
        }
        loop {
            match answer.recv_timeout(Duration::from_millis(100)) {
                Ok(answer) => return answer,
                Err(RecvTimeoutError::Timeout) if self.cancel.is_cancelled() => {
                    return PermissionAnswer::Deny;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return PermissionAnswer::Deny,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use termide_agent_core::ToolCall;

    fn request() -> PermissionRequest {
        PermissionRequest {
            tool: "bash".into(),
            subject: "git push".into(),
            call: ToolCall {
                id: "c".into(),
                name: "bash".into(),
                arguments: json!({ "command": "git push" }),
            },
            suggested_pattern: "git push *".into(),
        }
    }

    #[test]
    fn answer_travels_back_and_abort_denies() {
        let cancel = CancelToken::new();
        let (mut prompter, rx) = channel(cancel.clone());

        let worker = std::thread::spawn({
            let request = request();
            move || prompter.ask(&request)
        });
        let envelope = rx.recv().unwrap();
        assert_eq!(envelope.id, 1);
        assert_eq!(envelope.request.subject, "git push");
        envelope.reply.send(PermissionAnswer::AllowSession).unwrap();
        assert_eq!(worker.join().unwrap(), PermissionAnswer::AllowSession);

        let (mut prompter, rx) = channel(cancel.clone());
        let worker = std::thread::spawn({
            let request = request();
            move || prompter.ask(&request)
        });
        let _pending = rx.recv().unwrap();
        cancel.cancel();
        assert_eq!(worker.join().unwrap(), PermissionAnswer::Deny);
    }

    #[test]
    fn dropped_panel_denies() {
        let (mut prompter, rx) = channel(CancelToken::new());
        drop(rx);
        assert_eq!(prompter.ask(&request()), PermissionAnswer::Deny);
    }
}
