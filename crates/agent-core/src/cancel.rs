//! Cooperative cancellation shared between the UI thread and a running agent.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A cloneable flag polled by providers and tools between units of work.
///
/// Cancellation is cooperative: setting the flag does not interrupt a
/// blocking read, it only asks the next check to stop. A provider must poll
/// it between stream chunks and a tool between output chunks.
#[derive(Debug, Clone, Default)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
}

impl CancelToken {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation of the current run.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Release);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }

    /// Clear the flag before a new run starts.
    pub fn reset(&self) {
        self.flag.store(false, Ordering::Release);
    }
}
