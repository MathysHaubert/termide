//! Worker-thread host for an [`Agent`], following termide's background
//! pipeline convention: `std::thread` plus `mpsc`, polled from `tick()`.
//!
//! Prompts travel over a channel and run one at a time. Steering, follow-up
//! and abort must reach a loop that is blocked inside `run`, so they go
//! out-of-band through the shared [`QueueHandle`] and [`CancelToken`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;

use crate::agent::{Agent, AgentEvent, Hooks, QueueHandle};
use crate::cancel::CancelToken;
use crate::message::UserMessage;

/// Why a prompt was not accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptError {
    /// A run is in progress; queue the text with `steer` or `follow_up`.
    Busy,
    /// The worker thread is gone.
    Stopped,
}

impl std::fmt::Display for PromptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => f.write_str("agent is busy"),
            Self::Stopped => f.write_str("agent runtime has stopped"),
        }
    }
}

impl std::error::Error for PromptError {}

enum WorkerCommand {
    Prompt(UserMessage),
    Shutdown,
}

/// Owns the worker thread that runs the agent.
pub struct AgentRuntime {
    commands: Sender<WorkerCommand>,
    events: Receiver<AgentEvent>,
    queues: QueueHandle,
    cancel: CancelToken,
    busy: Arc<AtomicBool>,
    worker: Option<JoinHandle<Agent>>,
}

impl AgentRuntime {
    /// Move `agent` onto a new thread. `hooks` run on that thread.
    #[must_use]
    pub fn spawn(agent: Agent, hooks: Box<dyn Hooks>) -> Self {
        Self::spawn_with_cancel(agent, hooks, CancelToken::new())
    }

    /// Like [`AgentRuntime::spawn`], sharing `cancel` with anything else that
    /// must notice an abort (a blocking permission prompt, for example).
    #[must_use]
    pub fn spawn_with_cancel(
        mut agent: Agent,
        mut hooks: Box<dyn Hooks>,
        cancel: CancelToken,
    ) -> Self {
        let (commands, command_rx) = mpsc::channel::<WorkerCommand>();
        let (event_tx, events) = mpsc::channel::<AgentEvent>();
        let queues = agent.queues();
        let busy = Arc::new(AtomicBool::new(false));

        let worker_cancel = cancel.clone();
        let worker_busy = busy.clone();
        let worker = std::thread::Builder::new()
            .name("termide-agent".into())
            .spawn(move || {
                while let Ok(command) = command_rx.recv() {
                    match command {
                        WorkerCommand::Prompt(prompt) => {
                            agent.run(prompt, hooks.as_mut(), &worker_cancel, &mut |event| {
                                // A closed receiver means the UI dropped the
                                // runtime; the run finishes on its own.
                                let _ = event_tx.send(event);
                            });
                            worker_busy.store(false, Ordering::Release);
                        }
                        WorkerCommand::Shutdown => break,
                    }
                }
                agent
            })
            .expect("spawn agent worker thread");

        Self {
            commands,
            events,
            queues,
            cancel,
            busy,
            worker: Some(worker),
        }
    }

    /// Start a run. Fails with [`PromptError::Busy`] while one is active;
    /// callers then choose between `steer` and `follow_up`.
    pub fn prompt(&self, message: UserMessage) -> Result<(), PromptError> {
        if self.worker.is_none() {
            return Err(PromptError::Stopped);
        }
        if self
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(PromptError::Busy);
        }
        self.cancel.reset();
        self.commands
            .send(WorkerCommand::Prompt(message))
            .map_err(|_| {
                self.busy.store(false, Ordering::Release);
                PromptError::Stopped
            })
    }

    /// Queue a message for the next turn boundary of the active run.
    pub fn steer(&self, message: UserMessage) {
        self.queues.steer(message);
    }

    /// Queue a message for when the active run would otherwise stop.
    pub fn follow_up(&self, message: UserMessage) {
        self.queues.follow_up(message);
    }

    /// Drop queued messages, returning `(steering, follow_up)`.
    pub fn clear_queue(&self) -> (Vec<UserMessage>, Vec<UserMessage>) {
        self.queues.clear()
    }

    /// Ask the active run to stop at its next check. No-op while idle.
    pub fn abort(&self) {
        if self.is_busy() {
            self.cancel.cancel();
        }
    }

    #[must_use]
    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn queues(&self) -> &QueueHandle {
        &self.queues
    }

    /// The token `abort` sets; shared with prompters that block the run.
    #[must_use]
    pub fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    /// Non-blocking: the next event, if any. Call from `tick()` until it
    /// returns `None`.
    #[must_use]
    pub fn try_recv(&self) -> Option<AgentEvent> {
        // Disconnected means the worker is gone; there is nothing more to
        // read either way.
        self.events.try_recv().ok()
    }

    /// Everything queued so far, without blocking.
    #[must_use]
    pub fn drain(&self) -> Vec<AgentEvent> {
        std::iter::from_fn(|| self.try_recv()).collect()
    }

    /// Stop the worker after the current run and get the agent back with its
    /// transcript. Returns `None` if the worker panicked.
    pub fn shutdown(mut self) -> Option<Agent> {
        self.cancel.cancel();
        let _ = self.commands.send(WorkerCommand::Shutdown);
        self.worker.take().and_then(|worker| worker.join().ok())
    }
}

impl Drop for AgentRuntime {
    fn drop(&mut self) {
        // Let the worker exit on its own; joining here could block the UI
        // thread behind a long tool call.
        self.cancel.cancel();
        let _ = self.commands.send(WorkerCommand::Shutdown);
    }
}

impl std::fmt::Debug for AgentRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentRuntime")
            .field("busy", &self.is_busy())
            .field("queues", &self.queues.lens())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use super::*;
    use crate::agent::test_support::*;
    use crate::agent::NoHooks;
    use crate::message::{AssistantMessage, StopReason};
    use crate::provider::{Provider, Request, StreamEvent};
    use crate::tool::ToolRegistry;

    fn wait_for_end(runtime: &AgentRuntime) -> Vec<AgentEvent> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut events = Vec::new();
        while Instant::now() < deadline {
            events.extend(runtime.drain());
            if events.iter().any(|e| matches!(e, AgentEvent::AgentEnd)) {
                return events;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("agent did not finish: {events:?}");
    }

    fn wait_until_idle(runtime: &AgentRuntime) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while runtime.is_busy() {
            assert!(Instant::now() < deadline, "runtime stayed busy");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn prompt_runs_on_the_worker_and_events_arrive_over_the_channel() {
        let provider = Arc::new(ScriptedProvider::new(vec![text_reply("hi there")]));
        let agent = Agent::new(
            provider,
            ToolRegistry::new(),
            model(),
            PathBuf::from("/tmp"),
        );
        let runtime = AgentRuntime::spawn(agent, Box::new(NoHooks));

        runtime.prompt(UserMessage::text("hello")).unwrap();
        let events = wait_for_end(&runtime);
        wait_until_idle(&runtime);

        assert!(
            events.contains(&AgentEvent::MessageUpdate(StreamEvent::TextDelta(
                "hi there".into()
            )))
        );
        let agent = runtime.shutdown().expect("worker returns the agent");
        assert_eq!(roles(agent.messages()), vec!["user", "assistant"]);
    }

    /// Blocks inside `stream` until the test releases it, so the runtime is
    /// observably busy.
    struct GatedProvider {
        gate: Mutex<Option<mpsc::Receiver<()>>>,
        cancelled: Arc<AtomicBool>,
    }

    impl Provider for GatedProvider {
        fn name(&self) -> &str {
            "gated"
        }
        fn stream(
            &self,
            request: &Request<'_>,
            _on_event: &mut dyn FnMut(StreamEvent),
            cancel: &CancelToken,
        ) -> AssistantMessage {
            if let Some(gate) = self.gate.lock().unwrap().take() {
                let _ = gate.recv();
            }
            if cancel.is_cancelled() {
                self.cancelled.store(true, Ordering::Release);
                return AssistantMessage::failed(
                    "gated",
                    &request.model.id,
                    StopReason::Aborted,
                    "aborted",
                );
            }
            text_reply("released")
        }
    }

    #[test]
    fn second_prompt_while_busy_is_rejected_and_abort_reaches_the_provider() {
        let (release, gate) = mpsc::channel::<()>();
        let cancelled = Arc::new(AtomicBool::new(false));
        let provider = Arc::new(GatedProvider {
            gate: Mutex::new(Some(gate)),
            cancelled: cancelled.clone(),
        });
        let agent = Agent::new(
            provider,
            ToolRegistry::new(),
            model(),
            PathBuf::from("/tmp"),
        );
        let runtime = AgentRuntime::spawn(agent, Box::new(NoHooks));

        runtime.prompt(UserMessage::text("first")).unwrap();
        assert!(runtime.is_busy());
        assert_eq!(
            runtime.prompt(UserMessage::text("second")),
            Err(PromptError::Busy)
        );

        runtime.steer(UserMessage::text("queued"));
        assert_eq!(runtime.queues().lens(), (1, 0));
        let (steering, _) = runtime.clear_queue();
        assert_eq!(steering.len(), 1);

        runtime.abort();
        release.send(()).unwrap();
        let events = wait_for_end(&runtime);
        wait_until_idle(&runtime);

        assert!(cancelled.load(Ordering::Acquire));
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::MessageEnd(crate::Message::Assistant(a)) if a.stop_reason == StopReason::Aborted
        )));
        assert!(runtime.prompt(UserMessage::text("third")).is_ok());
        wait_for_end(&runtime);
    }

    #[test]
    fn abort_while_idle_does_not_poison_the_next_run() {
        let provider = Arc::new(ScriptedProvider::new(vec![text_reply("fine")]));
        let agent = Agent::new(
            provider,
            ToolRegistry::new(),
            model(),
            PathBuf::from("/tmp"),
        );
        let runtime = AgentRuntime::spawn(agent, Box::new(NoHooks));

        runtime.abort();
        runtime.prompt(UserMessage::text("go")).unwrap();
        let events = wait_for_end(&runtime);

        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::MessageEnd(crate::Message::Assistant(a)) if a.stop_reason == StopReason::Stop
        )));
    }
}
