//! Model providers for the termide coding agent.
//!
//! The first and, for local models, usually the only provider is
//! [`OpenAiCompatProvider`]: streaming chat completions as served by
//! llama.cpp, Ollama, vLLM, omlx, OpenRouter and most gateways. Vendor quirks
//! are expressed as data in [`Compat`] rather than as code paths.
//!
//! HTTP is blocking `ureq` on the agent's worker thread, consistent with the
//! rest of termide (no async runtime); cancellation is polled between SSE
//! lines.

mod openai;
mod sse;

pub use openai::{Compat, OpenAiCompatProvider, RetryPolicy};
