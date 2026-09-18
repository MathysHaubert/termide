//! Transport retry shared by the providers: a stream attempt is retried with
//! exponential backoff only while it has produced no content, so a partial
//! answer is never silently duplicated.

use std::time::Duration;

use termide_agent_core::{AssistantMessage, CancelToken, StopReason, StreamEvent};

/// A failed attempt: `retryable` decides whether another one may follow.
pub struct Failure {
    pub message: String,
    pub retryable: bool,
}

/// How many times a request may be retried before content arrives, and how
/// long to wait first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_secs(1),
        }
    }
}

/// Run `attempt` until it succeeds, fails unretryably, or the attempts run
/// out. Between tries it emits a `Retry` event and sleeps, waking often to
/// notice a cancel. `attempt` gets the live event sink for its deltas.
pub fn with_retries(
    provider: &str,
    model: &str,
    policy: RetryPolicy,
    cancel: &CancelToken,
    on_event: &mut dyn FnMut(StreamEvent),
    mut attempt: impl FnMut(&mut dyn FnMut(StreamEvent)) -> Result<AssistantMessage, Failure>,
) -> AssistantMessage {
    let max_attempts = policy.max_attempts.max(1);
    let mut n = 1;
    loop {
        if cancel.is_cancelled() {
            return AssistantMessage::failed(provider, model, StopReason::Aborted, "aborted");
        }
        match attempt(on_event) {
            Ok(message) => return message,
            Err(failure) if failure.retryable && n < max_attempts => {
                let delay = policy.base_delay * 2u32.saturating_pow(n - 1);
                log::warn!(
                    "{provider} request failed (attempt {n}/{max_attempts}): {}; retrying in {delay:?}",
                    failure.message
                );
                on_event(StreamEvent::Retry {
                    attempt: n,
                    max_attempts,
                    delay_ms: delay.as_millis() as u64,
                    error: failure.message,
                });
                if !sleep_unless_cancelled(delay, cancel) {
                    return AssistantMessage::failed(
                        provider,
                        model,
                        StopReason::Aborted,
                        "aborted",
                    );
                }
                n += 1;
            }
            Err(failure) => {
                return AssistantMessage::failed(
                    provider,
                    model,
                    StopReason::Error,
                    failure.message,
                )
            }
        }
    }
}

/// Sleep in slices so an abort is noticed; `false` if cancelled.
pub fn sleep_unless_cancelled(total: Duration, cancel: &CancelToken) -> bool {
    let slice = Duration::from_millis(50);
    let mut slept = Duration::ZERO;
    while slept < total {
        if cancel.is_cancelled() {
            return false;
        }
        let step = slice.min(total - slept);
        std::thread::sleep(step);
        slept += step;
    }
    !cancel.is_cancelled()
}
