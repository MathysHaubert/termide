//! The model provider contract.
//!
//! A provider turns a [`Request`] into one streamed [`AssistantMessage`].
//! Streaming deltas go to a callback so the UI can paint text as it arrives;
//! the returned message is authoritative and is what enters the transcript.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cancel::CancelToken;
use crate::message::{AssistantMessage, Message};

/// How much reasoning effort to request from a model that supports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingLevel {
    #[default]
    Off,
    Minimal,
    Low,
    Medium,
    High,
}

/// A model as configured by the user, independent of the provider wire format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSpec {
    /// Provider name the model is served by, e.g. `"openai-compatible"`.
    pub provider: String,
    /// Model id as the provider expects it.
    pub id: String,
    /// Context window in tokens; drives compaction thresholds.
    pub context_window: u64,
    /// Upper bound for output tokens per response.
    pub max_tokens: u64,
    /// Whether the model exposes a thinking/reasoning channel.
    #[serde(default)]
    pub reasoning: bool,
}

/// One entry of a provider's model list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    /// Model id as the provider expects it.
    pub id: String,
    /// Context window in tokens when the endpoint reports it (vLLM and omlx
    /// do as `max_model_len`; Ollama and llama.cpp do not).
    pub context_window: Option<u64>,
}

/// Tool description in the shape model APIs expect: name, description and a
/// JSON Schema for the arguments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// Everything a provider needs for one model call.
#[derive(Debug, Clone)]
pub struct Request<'a> {
    pub model: &'a ModelSpec,
    pub system_prompt: &'a str,
    pub messages: &'a [Message],
    pub tools: &'a [ToolSpec],
    pub thinking: ThinkingLevel,
}

/// Incremental piece of an assistant message, for live rendering only.
///
/// Consumers must not rebuild the message from deltas; the provider returns
/// the complete [`AssistantMessage`] when the stream ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    TextDelta(String),
    ThinkingDelta(String),
    ToolCallStart {
        id: String,
        name: String,
    },
    /// A fragment of the raw JSON arguments for the call with this id.
    ToolCallDelta {
        id: String,
        arguments: String,
    },
    ToolCallEnd {
        id: String,
    },
    /// The request failed before any content arrived and will be retried
    /// after `delay_ms`.
    Retry {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error: String,
    },
}

/// A model backend.
///
/// `stream` never fails: transport errors, HTTP errors and cancellation are
/// reported through [`AssistantMessage::stop_reason`] and
/// [`AssistantMessage::error_message`]. Implementations poll `cancel` between
/// chunks and return a message with [`crate::StopReason::Aborted`] when it is
/// set.
pub trait Provider: Send + Sync {
    /// Stable provider name, recorded on every assistant message.
    fn name(&self) -> &str;

    fn stream(
        &self,
        request: &Request<'_>,
        on_event: &mut dyn FnMut(StreamEvent),
        cancel: &CancelToken,
    ) -> AssistantMessage;

    /// Where the models are served, for the status line: a host and port, a
    /// gateway's name. `None` when there is nothing useful to show.
    fn endpoint(&self) -> Option<String> {
        None
    }

    /// The models the endpoint serves, for a picker. Blocking; call it off
    /// the UI thread. The default says the provider cannot enumerate them,
    /// and callers fall back to a typed id.
    fn list_models(&self) -> Result<Vec<ModelInfo>, String> {
        Err("this provider cannot list its models".to_string())
    }
}
