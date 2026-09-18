//! Anthropic Messages API over blocking HTTP.
//!
//! A native provider for an Anthropic subscription, beside the
//! OpenAI-compatible one that already covers omlx, OpenAI and OpenRouter. The
//! wire shape is different enough — a top-level `system`, content-block
//! messages, tool results carried inside a user turn, a distinct SSE event
//! stream — to warrant its own provider rather than another `Compat` dialect.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{json, Map, Value};
use termide_agent_core::{
    now_millis, AssistantContent, AssistantMessage, CancelToken, Message, ModelInfo, Provider,
    Request, StopReason, StreamEvent, ThinkingLevel, ToolCall, Usage,
};

use crate::retry::{with_retries, Failure, RetryPolicy};

/// The Messages API version pinned in the request header.
const API_VERSION: &str = "2023-06-01";
/// The public API root; overridable for a gateway.
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

pub struct AnthropicProvider {
    name: String,
    base_url: String,
    api_key: Option<String>,
    pub retry: RetryPolicy,
    pub read_timeout: Duration,
    pub connect_timeout: Duration,
    agent: ureq::Agent,
}

impl AnthropicProvider {
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self::with_base_url(name, DEFAULT_BASE_URL)
    }

    #[must_use]
    pub fn with_base_url(name: impl Into<String>, base_url: impl Into<String>) -> Self {
        let connect_timeout = Duration::from_secs(15);
        let read_timeout = Duration::from_secs(120);
        let base = base_url.into();
        let base = base.trim_end_matches('/');
        let base = base
            .trim_end_matches("/v1")
            .trim_end_matches('/')
            .to_string();
        Self {
            name: name.into(),
            base_url: if base.is_empty() {
                DEFAULT_BASE_URL.to_string()
            } else {
                base
            },
            api_key: None,
            retry: RetryPolicy::default(),
            read_timeout,
            connect_timeout,
            agent: build_agent(connect_timeout, read_timeout),
        }
    }

    #[must_use]
    pub fn with_api_key(mut self, key: Option<String>) -> Self {
        self.api_key = key.filter(|k| !k.is_empty());
        self
    }

    #[must_use]
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    #[must_use]
    pub fn with_timeouts(mut self, connect: Duration, read: Duration) -> Self {
        self.connect_timeout = connect;
        self.read_timeout = read;
        self.agent = build_agent(connect, read);
        self
    }

    /// The JSON body for `request`, exposed for tests and debugging.
    #[must_use]
    pub fn build_body(&self, request: &Request<'_>) -> Value {
        let mut body = Map::new();
        body.insert("model".into(), json!(request.model.id));
        body.insert("max_tokens".into(), json!(request.model.max_tokens));
        body.insert("stream".into(), json!(true));
        if !request.system_prompt.is_empty() {
            body.insert("system".into(), json!(request.system_prompt));
        }
        body.insert("messages".into(), json!(convert_messages(request.messages)));
        if !request.tools.is_empty() {
            let tools: Vec<Value> = request
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "name": tool.name,
                        "description": tool.description,
                        "input_schema": tool.parameters,
                    })
                })
                .collect();
            body.insert("tools".into(), Value::Array(tools));
        }
        if request.model.reasoning {
            if let Some(budget) = thinking_budget(request.thinking, request.model.max_tokens) {
                body.insert(
                    "thinking".into(),
                    json!({ "type": "enabled", "budget_tokens": budget }),
                );
            }
        }
        Value::Object(body)
    }

    fn attempt(
        &self,
        body: &str,
        model: &str,
        on_event: &mut dyn FnMut(StreamEvent),
        cancel: &CancelToken,
    ) -> Result<AssistantMessage, Failure> {
        use std::io::{BufRead, BufReader};

        let url = format!("{}/v1/messages", self.base_url);
        let mut http = self
            .agent
            .post(&url)
            .set("content-type", "application/json")
            .set("accept", "text/event-stream")
            .set("anthropic-version", API_VERSION);
        if let Some(key) = &self.api_key {
            http = http.set("x-api-key", key);
        }
        let response = match http.send_string(body) {
            Ok(response) => response,
            Err(ureq::Error::Status(code, response)) => {
                let text = response.into_string().unwrap_or_default();
                return Err(Failure {
                    message: format!("HTTP {code}: {}", error_text(&text)),
                    retryable: matches!(code, 408 | 409 | 425 | 429 | 500..=599),
                });
            }
            Err(ureq::Error::Transport(transport)) => {
                return Err(Failure {
                    message: format!("transport error: {transport}"),
                    retryable: true,
                });
            }
        };

        let mut acc = Accumulator::default();
        let mut reader = BufReader::new(response.into_reader());
        let mut line = String::new();
        loop {
            if cancel.is_cancelled() {
                return Ok(acc.finish(&self.name, model, Some(StopReason::Aborted), on_event));
            }
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) if cancel.is_cancelled() => {
                    return Ok(acc.finish(&self.name, model, Some(StopReason::Aborted), on_event));
                }
                Ok(_) => {}
                Err(error) => {
                    if acc.received_content {
                        let mut partial =
                            acc.finish(&self.name, model, Some(StopReason::Error), on_event);
                        partial.error_message = Some(format!("stream interrupted: {error}"));
                        return Ok(partial);
                    }
                    return Err(Failure {
                        message: format!("stream error: {error}"),
                        retryable: true,
                    });
                }
            }
            let Some(data) = sse_data(line.trim_end()) else {
                continue;
            };
            match serde_json::from_str::<Value>(data) {
                Ok(event) => {
                    if event.get("type").and_then(Value::as_str) == Some("error") {
                        let message = error_text(&event.to_string());
                        if acc.received_content {
                            let mut partial =
                                acc.finish(&self.name, model, Some(StopReason::Error), on_event);
                            partial.error_message = Some(message);
                            return Ok(partial);
                        }
                        return Err(Failure {
                            message,
                            retryable: false,
                        });
                    }
                    acc.feed(&event, on_event);
                }
                Err(error) => log::warn!("skipping malformed SSE event ({error}): {data}"),
            }
        }
        Ok(acc.finish(&self.name, model, None, on_event))
    }
}

impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn endpoint(&self) -> Option<String> {
        let rest = self
            .base_url
            .split_once("://")
            .map_or(self.base_url.as_str(), |(_, rest)| rest);
        let host = rest.split('/').next().unwrap_or("");
        (!host.is_empty()).then(|| host.to_string())
    }

    /// `GET /v1/models`. Anthropic does not report a context window there, so
    /// the picker keeps whatever is configured.
    fn list_models(&self) -> Result<Vec<ModelInfo>, String> {
        let url = format!("{}/v1/models?limit=1000", self.base_url);
        let mut http = self
            .agent
            .get(&url)
            .set("accept", "application/json")
            .set("anthropic-version", API_VERSION);
        if let Some(key) = &self.api_key {
            http = http.set("x-api-key", key);
        }
        let body = match http.call() {
            Ok(response) => response
                .into_string()
                .map_err(|error| format!("cannot read the model list: {error}"))?,
            Err(ureq::Error::Status(code, response)) => {
                let text = response.into_string().unwrap_or_default();
                return Err(format!("HTTP {code}: {}", error_text(&text)));
            }
            Err(ureq::Error::Transport(transport)) => {
                return Err(format!("transport error: {transport}"));
            }
        };
        parse_model_list(&body)
    }

    fn stream(
        &self,
        request: &Request<'_>,
        on_event: &mut dyn FnMut(StreamEvent),
        cancel: &CancelToken,
    ) -> AssistantMessage {
        let body = self.build_body(request).to_string();
        let model = request.model.id.as_str();
        with_retries(
            &self.name,
            model,
            self.retry,
            cancel,
            on_event,
            |on_event| self.attempt(&body, model, on_event, cancel),
        )
    }
}

/// Turn the transcript into Anthropic messages: assistant blocks as they are,
/// and tool results gathered into the user turn that must carry them, with
/// consecutive results merged so each assistant tool-use turn is answered by
/// exactly one following user turn.
fn convert_messages(messages: &[Message]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut pending_results: Vec<Value> = Vec::new();
    let flush = |out: &mut Vec<Value>, results: &mut Vec<Value>| {
        if !results.is_empty() {
            out.push(json!({ "role": "user", "content": std::mem::take(results) }));
        }
    };
    for message in messages {
        match message {
            Message::ToolResult(result) => {
                pending_results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": result.tool_call_id,
                    "content": result.plain_text(),
                    "is_error": result.is_error,
                }));
            }
            Message::User(user) => {
                flush(&mut out, &mut pending_results);
                out.push(json!({
                    "role": "user",
                    "content": [{ "type": "text", "text": user.plain_text() }],
                }));
            }
            Message::Assistant(assistant) => {
                flush(&mut out, &mut pending_results);
                let mut blocks: Vec<Value> = Vec::new();
                for block in &assistant.content {
                    match block {
                        AssistantContent::Text { text } if !text.is_empty() => {
                            blocks.push(json!({ "type": "text", "text": text }));
                        }
                        AssistantContent::ToolCall(call) => {
                            blocks.push(json!({
                                "type": "tool_use",
                                "id": call.id,
                                "name": call.name,
                                "input": call.arguments,
                            }));
                        }
                        // Prior thinking is dropped: replaying it needs the
                        // original signature, which the transcript does not keep.
                        _ => {}
                    }
                }
                if !blocks.is_empty() {
                    out.push(json!({ "role": "assistant", "content": blocks }));
                }
            }
        }
    }
    flush(&mut out, &mut pending_results);
    out
}

/// The thinking budget for a level, always leaving room for the answer.
fn thinking_budget(level: ThinkingLevel, max_tokens: u64) -> Option<u64> {
    let budget = match level {
        ThinkingLevel::Off => return None,
        ThinkingLevel::Minimal => 1024,
        ThinkingLevel::Low => 4096,
        ThinkingLevel::Medium => 8192,
        ThinkingLevel::High => 16384,
    };
    // The API requires max_tokens to exceed the budget; keep a quarter for
    // the reply at least, and never go below the 1024 floor.
    let cap = max_tokens.saturating_mul(3) / 4;
    Some(budget.min(cap.max(1024)))
}

/// One SSE `data:` line's payload; `None` for `event:`, comments and blanks.
fn sse_data(line: &str) -> Option<&str> {
    let data = line.strip_prefix("data:")?.trim_start();
    (!data.is_empty()).then_some(data)
}

#[derive(Debug, Default)]
struct PartialBlock {
    kind: String,
    id: String,
    name: String,
    text: String,
    input: String,
}

/// Builds the assistant message from the Messages API event stream, keyed by
/// content-block index.
#[derive(Debug, Default)]
struct Accumulator {
    blocks: HashMap<usize, PartialBlock>,
    order: Vec<usize>,
    stop_reason: Option<String>,
    usage: Usage,
    received_content: bool,
}

impl Accumulator {
    fn feed(&mut self, event: &Value, on_event: &mut dyn FnMut(StreamEvent)) {
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                if let Some(usage) = event.get("message").and_then(|m| m.get("usage")) {
                    self.usage.input = u64_at(usage, "input_tokens");
                    self.usage.cache_read = u64_at(usage, "cache_read_input_tokens");
                    self.usage.cache_write = u64_at(usage, "cache_creation_input_tokens");
                }
            }
            Some("content_block_start") => {
                let index = u64_at(event, "index") as usize;
                let block = event.get("content_block");
                let kind = block
                    .and_then(|b| b.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let mut partial = PartialBlock {
                    kind: kind.clone(),
                    ..PartialBlock::default()
                };
                if kind == "tool_use" {
                    partial.id = block
                        .and_then(|b| b.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    partial.name = block
                        .and_then(|b| b.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    self.received_content = true;
                    on_event(StreamEvent::ToolCallStart {
                        id: partial.id.clone(),
                        name: partial.name.clone(),
                    });
                }
                self.order.push(index);
                self.blocks.insert(index, partial);
            }
            Some("content_block_delta") => {
                let index = u64_at(event, "index") as usize;
                let Some(delta) = event.get("delta") else {
                    return;
                };
                let partial = self.blocks.entry(index).or_default();
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(text) = delta.get("text").and_then(Value::as_str) {
                            self.received_content = true;
                            partial.text.push_str(text);
                            on_event(StreamEvent::TextDelta(text.to_string()));
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(text) = delta.get("thinking").and_then(Value::as_str) {
                            self.received_content = true;
                            partial.text.push_str(text);
                            on_event(StreamEvent::ThinkingDelta(text.to_string()));
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(fragment) = delta.get("partial_json").and_then(Value::as_str) {
                            partial.input.push_str(fragment);
                            on_event(StreamEvent::ToolCallDelta {
                                id: partial.id.clone(),
                                arguments: fragment.to_string(),
                            });
                        }
                    }
                    _ => {}
                }
            }
            Some("content_block_stop") => {
                let index = u64_at(event, "index") as usize;
                if let Some(partial) = self.blocks.get(&index) {
                    if partial.kind == "tool_use" {
                        on_event(StreamEvent::ToolCallEnd {
                            id: partial.id.clone(),
                        });
                    }
                }
            }
            Some("message_delta") => {
                if let Some(reason) = event
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    self.stop_reason = Some(reason.to_string());
                }
                if let Some(usage) = event.get("usage") {
                    let output = u64_at(usage, "output_tokens");
                    if output > 0 {
                        self.usage.output = output;
                    }
                }
            }
            _ => {}
        }
    }

    fn finish(
        self,
        provider: &str,
        model: &str,
        stop_override: Option<StopReason>,
        _on_event: &mut dyn FnMut(StreamEvent),
    ) -> AssistantMessage {
        let mut content = Vec::new();
        let mut has_calls = false;
        for index in &self.order {
            let Some(partial) = self.blocks.get(index) else {
                continue;
            };
            match partial.kind.as_str() {
                "thinking" if !partial.text.is_empty() => {
                    content.push(AssistantContent::Thinking {
                        text: partial.text.clone(),
                    })
                }
                "text" if !partial.text.is_empty() => content.push(AssistantContent::Text {
                    text: partial.text.clone(),
                }),
                "tool_use" if !partial.name.is_empty() => {
                    has_calls = true;
                    content.push(AssistantContent::ToolCall(ToolCall {
                        id: partial.id.clone(),
                        name: partial.name.clone(),
                        arguments: parse_arguments(&partial.input),
                    }));
                }
                _ => {}
            }
        }
        let stop_reason = stop_override.unwrap_or(match self.stop_reason.as_deref() {
            Some("max_tokens") => StopReason::Length,
            Some("tool_use") => StopReason::ToolUse,
            _ if has_calls => StopReason::ToolUse,
            _ => StopReason::Stop,
        });
        AssistantMessage {
            content,
            stop_reason,
            usage: self.usage,
            provider: provider.to_string(),
            model: model.to_string(),
            error_message: None,
            timestamp: now_millis(),
        }
    }
}

fn u64_at(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// Tool arguments: the accumulated JSON, or an empty object when the model
/// sent none (a no-argument tool) or something unparseable.
fn parse_arguments(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return json!({});
    }
    serde_json::from_str(raw).unwrap_or_else(|_| json!({}))
}

fn build_agent(connect: Duration, read: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(connect)
        .timeout_read(read)
        .user_agent(concat!("termide-agent/", env!("CARGO_PKG_VERSION")))
        .build()
}

fn parse_model_list(body: &str) -> Result<Vec<ModelInfo>, String> {
    let value: Value =
        serde_json::from_str(body).map_err(|error| format!("model list is not JSON: {error}"))?;
    let items = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| "model list has no `data` array".to_string())?;
    let mut models: Vec<ModelInfo> = items
        .iter()
        .filter_map(|item| {
            let id = item.get("id").and_then(Value::as_str)?;
            Some(ModelInfo {
                id: id.to_string(),
                context_window: None,
            })
        })
        .collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    models.dedup_by(|a, b| a.id == b.id);
    Ok(models)
}

/// The human-readable message from `{"error": {"message": ...}}`, else the
/// body trimmed.
fn error_text(body: &str) -> String {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let message = parsed
        .as_ref()
        .and_then(|v| v.get("error").unwrap_or(v).get("message"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let text = message.unwrap_or_else(|| body.trim().to_string());
    if text.is_empty() {
        "no error body".to_string()
    } else {
        text.chars().take(500).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use termide_agent_core::{
        AssistantMessage, ModelSpec, ToolCall, ToolResultMessage, ToolSpec, UserMessage,
    };

    fn model(reasoning: bool) -> ModelSpec {
        ModelSpec {
            provider: "anthropic".into(),
            id: "claude-x".into(),
            context_window: 200_000,
            max_tokens: 4096,
            reasoning,
        }
    }

    fn feed_all(events: &[Value]) -> (AssistantMessage, Vec<StreamEvent>) {
        let mut acc = Accumulator::default();
        let mut seen = Vec::new();
        for event in events {
            acc.feed(event, &mut |e| seen.push(e));
        }
        (
            acc.finish("anthropic", "claude-x", None, &mut |e| seen.push(e)),
            seen,
        )
    }

    #[test]
    fn the_body_carries_system_tools_and_thinking_in_the_messages_shape() {
        let provider = AnthropicProvider::new("anthropic").with_api_key(Some("k".into()));
        let call = ToolCall {
            id: "tu_1".into(),
            name: "read".into(),
            arguments: json!({ "path": "a.rs" }),
        };
        let messages = vec![
            Message::User(UserMessage::text("hi")),
            Message::Assistant(AssistantMessage {
                content: vec![
                    AssistantContent::Thinking { text: "hmm".into() },
                    AssistantContent::Text {
                        text: "reading".into(),
                    },
                    AssistantContent::ToolCall(call.clone()),
                ],
                stop_reason: StopReason::ToolUse,
                usage: Usage::default(),
                provider: "anthropic".into(),
                model: "claude-x".into(),
                error_message: None,
                timestamp: 0,
            }),
            Message::ToolResult(ToolResultMessage::text(&call, "contents")),
        ];
        let tools = vec![ToolSpec {
            name: "read".into(),
            description: "read a file".into(),
            parameters: json!({ "type": "object" }),
        }];
        let request = Request {
            model: &model(true),
            system_prompt: "be terse",
            messages: &messages,
            tools: &tools,
            thinking: ThinkingLevel::Medium,
        };
        let body = provider.build_body(&request);
        assert_eq!(body["system"], json!("be terse"));
        assert_eq!(body["max_tokens"], json!(4096));
        assert_eq!(body["thinking"]["type"], json!("enabled"));
        assert_eq!(body["thinking"]["budget_tokens"], json!(3072)); // capped at 3/4 of max
        assert_eq!(
            body["tools"][0]["input_schema"],
            json!({ "type": "object" })
        );

        let wire = body["messages"].as_array().unwrap();
        // user, assistant (thinking dropped, text + tool_use), tool_result user
        assert_eq!(wire.len(), 3);
        assert_eq!(
            wire[0],
            json!({ "role": "user", "content": [{ "type": "text", "text": "hi" }] })
        );
        let blocks = wire[1]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2, "thinking is not replayed");
        assert_eq!(blocks[0], json!({ "type": "text", "text": "reading" }));
        assert_eq!(blocks[1]["type"], json!("tool_use"));
        assert_eq!(blocks[1]["input"], json!({ "path": "a.rs" }));
        assert_eq!(wire[2]["role"], json!("user"));
        assert_eq!(wire[2]["content"][0]["type"], json!("tool_result"));
        assert_eq!(wire[2]["content"][0]["tool_use_id"], json!("tu_1"));

        // No thinking when the model is not a reasoning one.
        let plain = provider.build_body(&Request {
            model: &model(false),
            ..request.clone()
        });
        assert!(plain.get("thinking").is_none());
    }

    #[test]
    fn the_event_stream_becomes_text_a_tool_call_and_usage() {
        let events = vec![
            json!({ "type": "message_start", "message": { "usage": { "input_tokens": 12, "cache_read_input_tokens": 3 } } }),
            json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }),
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "Hello" } }),
            json!({ "type": "content_block_stop", "index": 0 }),
            json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "tool_use", "id": "tu_9", "name": "read" } }),
            json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "input_json_delta", "partial_json": "{\"path\":" } }),
            json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "input_json_delta", "partial_json": "\"a.rs\"}" } }),
            json!({ "type": "content_block_stop", "index": 1 }),
            json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" }, "usage": { "output_tokens": 7 } }),
            json!({ "type": "message_stop" }),
        ];
        let (message, seen) = feed_all(&events);
        assert_eq!(message.plain_text(), "Hello");
        assert_eq!(message.stop_reason, StopReason::ToolUse);
        assert_eq!(message.usage.input, 12);
        assert_eq!(message.usage.cache_read, 3);
        assert_eq!(message.usage.output, 7);
        let calls: Vec<&ToolCall> = message.tool_calls().collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "tu_9");
        assert_eq!(calls[0].arguments, json!({ "path": "a.rs" }));
        assert!(seen.contains(&StreamEvent::TextDelta("Hello".into())));
        assert!(seen.contains(&StreamEvent::ToolCallStart {
            id: "tu_9".into(),
            name: "read".into()
        }));
        assert!(seen.contains(&StreamEvent::ToolCallEnd { id: "tu_9".into() }));
    }

    #[test]
    fn consecutive_tool_results_merge_into_one_user_turn() {
        let a = ToolCall {
            id: "u1".into(),
            name: "read".into(),
            arguments: json!({}),
        };
        let b = ToolCall {
            id: "u2".into(),
            name: "read".into(),
            arguments: json!({}),
        };
        let messages = vec![
            Message::ToolResult(ToolResultMessage::text(&a, "one")),
            Message::ToolResult(ToolResultMessage::text(&b, "two")),
        ];
        let wire = convert_messages(&messages);
        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0]["role"], json!("user"));
        assert_eq!(wire[0]["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn base_url_normalises_and_endpoint_is_the_host() {
        let p = AnthropicProvider::with_base_url("anthropic", "https://api.anthropic.com/v1/");
        assert_eq!(p.endpoint().as_deref(), Some("api.anthropic.com"));
        let g = AnthropicProvider::with_base_url("anthropic", "https://gw.example/proxy");
        assert_eq!(g.endpoint().as_deref(), Some("gw.example"));
    }
}
