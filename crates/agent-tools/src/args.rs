//! Argument extraction shared by the tools. Every failure is a plain message
//! the model can act on.

use std::path::{Path, PathBuf};

use termide_agent_core::{ToolCall, ToolContext};

pub(crate) fn required_str<'a>(call: &'a ToolCall, key: &str) -> Result<&'a str, String> {
    match call.arguments.get(key) {
        Some(value) => value
            .as_str()
            .ok_or_else(|| format!("`{key}` must be a string")),
        None => Err(format!("missing required argument `{key}`")),
    }
}

pub(crate) fn optional_u64(call: &ToolCall, key: &str) -> Result<Option<u64>, String> {
    match call.arguments.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("`{key}` must be a non-negative integer")),
    }
}

pub(crate) fn optional_bool(call: &ToolCall, key: &str) -> Result<bool, String> {
    match call.arguments.get(key) {
        None | Some(serde_json::Value::Null) => Ok(false),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| format!("`{key}` must be a boolean")),
    }
}

/// Resolve a tool path argument against the working directory.
pub(crate) fn resolve_path(ctx: &ToolContext, raw: &str) -> Result<PathBuf, String> {
    if raw.trim().is_empty() {
        return Err("`path` must not be empty".to_string());
    }
    let path = Path::new(raw);
    Ok(if path.is_absolute() {
        path.to_path_buf()
    } else {
        ctx.cwd.join(path)
    })
}
