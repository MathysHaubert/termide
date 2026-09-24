//! The Chrome DevTools Protocol over `--remote-debugging-pipe`: the browser
//! reads commands from its descriptor 3 and writes replies and events to its
//! descriptor 4, each message a JSON object followed by a NUL byte. Nothing
//! listens on a port, so no other process can attach to the browser.
//!
//! Sessions are flat (`Target.attachToTarget` with `flatten`): a command for a
//! page carries its `sessionId` at the top level. Events are not delivered;
//! callers poll page state instead, which keeps this a request/reply channel.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use termide_agent_core::CancelToken;

type Pending = Arc<Mutex<HashMap<u64, Sender<Result<Value, String>>>>>;

/// How often a blocked request looks at the cancel token.
const CANCEL_POLL: Duration = Duration::from_millis(100);

pub struct Cdp {
    writer: Mutex<File>,
    pending: Pending,
    next_id: AtomicU64,
    closed: Arc<AtomicBool>,
}

impl Cdp {
    /// Speak over the parent's ends of the two pipes: `commands` is written,
    /// `replies` is read on a thread of its own.
    pub fn new(commands: File, replies: File) -> Self {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let reader_pending = Arc::clone(&pending);
        let reader_closed = Arc::clone(&closed);
        std::thread::spawn(move || read_loop(replies, reader_pending, reader_closed));
        Self {
            writer: Mutex::new(commands),
            pending,
            next_id: AtomicU64::new(1),
            closed,
        }
    }

    /// Whether the browser end of the pipe has gone away.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    /// Send `method` (to the browser, or to a page when `session` is given)
    /// and wait for its result. A protocol error, a timeout, a cancelled run
    /// and a dead browser all come back as `Err`.
    pub fn call(
        &self,
        method: &str,
        params: Value,
        session: Option<&str>,
        timeout: Duration,
        cancel: Option<&CancelToken>,
    ) -> Result<Value, String> {
        if self.is_closed() {
            return Err("the browser has exited".into());
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut message = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session {
            message["sessionId"] = json!(session);
        }
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let mut bytes = message.to_string().into_bytes();
        bytes.push(0);
        let written = {
            let mut writer = self.writer.lock().unwrap();
            writer.write_all(&bytes).and_then(|()| writer.flush())
        };
        if let Err(error) = written {
            self.pending.lock().unwrap().remove(&id);
            return Err(format!("cannot write to the browser: {error}"));
        }

        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                self.pending.lock().unwrap().remove(&id);
                return Err(format!("{method} timed out"));
            }
            match rx.recv_timeout(left.min(CANCEL_POLL)) {
                Ok(result) => return result,
                Err(RecvTimeoutError::Timeout) => {
                    if cancel.is_some_and(CancelToken::is_cancelled) {
                        self.pending.lock().unwrap().remove(&id);
                        return Err("cancelled".into());
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err("the browser has exited".into());
                }
            }
        }
    }
}

fn read_loop(replies: File, pending: Pending, closed: Arc<AtomicBool>) {
    let mut reader = BufReader::new(replies);
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        match reader.read_until(0, &mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if buffer.last() == Some(&0) {
            buffer.pop();
        }
        let Ok(message) = serde_json::from_slice::<Value>(&buffer) else {
            continue;
        };
        // Events have no id; nothing waits for them.
        let Some(id) = message.get("id").and_then(Value::as_u64) else {
            continue;
        };
        let Some(tx) = pending.lock().unwrap().remove(&id) else {
            continue;
        };
        let result = match message.get("error") {
            Some(error) => Err(error["message"]
                .as_str()
                .unwrap_or("protocol error")
                .to_string()),
            None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
        };
        let _ = tx.send(result);
    }
    closed.store(true, Ordering::Relaxed);
    // Wake every waiter: dropping the senders disconnects their receivers.
    pending.lock().unwrap().clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake browser on the far end of two pipes: answers every command with
    /// its method name, and an error for `Fail.me`.
    #[cfg(unix)]
    fn fake_browser() -> Cdp {
        use std::io::Read;
        use std::os::fd::FromRawFd;

        let pipe = || {
            let mut fds = [0; 2];
            assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
            unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
        };
        let (browser_in, commands) = pipe();
        let (replies, mut browser_out) = pipe();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(browser_in);
            let mut buffer = Vec::new();
            while reader.read_until(0, &mut buffer).unwrap_or(0) > 0 {
                buffer.pop();
                let message: Value = serde_json::from_slice(&buffer).unwrap();
                buffer.clear();
                let id = message["id"].clone();
                let method = message["method"].as_str().unwrap_or("");
                let reply = if method == "Fail.me" {
                    json!({ "id": id, "error": { "code": -1, "message": "no such thing" } })
                } else if method == "Hang.up" {
                    continue;
                } else {
                    json!({ "id": id, "result": { "method": method, "session": message["sessionId"] } })
                };
                // An event first, which the client must skip.
                let event = json!({ "method": "Page.loadEventFired", "params": {} });
                let mut bytes = event.to_string().into_bytes();
                bytes.push(0);
                bytes.extend(reply.to_string().into_bytes());
                bytes.push(0);
                browser_out.write_all(&bytes).unwrap();
            }
            let _ = reader.read(&mut [0; 1]);
        });
        Cdp::new(commands, replies)
    }

    #[cfg(unix)]
    #[test]
    fn replies_are_matched_to_requests_and_events_skipped() {
        let cdp = fake_browser();
        let second = Duration::from_secs(1);
        let result = cdp
            .call("Browser.getVersion", json!({}), None, second, None)
            .unwrap();
        assert_eq!(result["method"], "Browser.getVersion");
        let result = cdp
            .call("Page.navigate", json!({}), Some("S1"), second, None)
            .unwrap();
        assert_eq!(result["session"], "S1");
        let error = cdp
            .call("Fail.me", json!({}), None, second, None)
            .unwrap_err();
        assert_eq!(error, "no such thing");
    }

    #[cfg(unix)]
    #[test]
    fn a_cancelled_call_returns_promptly() {
        let cdp = fake_browser();
        let cancel = CancelToken::new();
        cancel.cancel();
        let started = Instant::now();
        let error = cdp
            .call(
                "Hang.up",
                json!({}),
                None,
                Duration::from_secs(10),
                Some(&cancel),
            )
            .unwrap_err();
        assert_eq!(error, "cancelled");
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
