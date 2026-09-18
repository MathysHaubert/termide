//! Configuration of MCP servers, `ai/mcp.toml`: one table per server.
//!
//! The shape is data the agent directories carry, so it lives here beside
//! the other directory contents; the client that speaks to a server is the
//! `termide-agent-mcp` crate.
//!
//! ```toml
//! [github]
//! command = "npx"
//! args = ["-y", "@modelcontextprotocol/server-github"]
//! env = { GITHUB_PERSONAL_ACCESS_TOKEN = "$GITHUB_TOKEN" }
//! tools = ["search_issues", "get_issue"]
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The file, at any level of the `ai` directory.
pub const MCP_FILE: &str = "mcp.toml";

/// One MCP server started over stdio.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Program to run.
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment for the server process. `$NAME` and `${NAME}` in a value
    /// are replaced from termide's own environment, so a token stays out of
    /// the file.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Working directory of the server; the panel's when absent.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Tools to take from the server, by their names there; all of them
    /// when absent. Every tool costs its schema in each request, so a server
    /// with dozens is worth narrowing.
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    /// Seconds to wait for the server to start and for one tool call.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// `false` at a higher level switches off a server a lower level defines.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_timeout() -> u64 {
    60
}

fn default_enabled() -> bool {
    true
}

/// Replace `$NAME` and `${NAME}` with `lookup(NAME)`; an unknown name
/// becomes empty. `$$` is a literal dollar.
pub fn expand_env(value: &str, lookup: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let tail = &rest[i + 1..];
        if let Some(after) = tail.strip_prefix('$') {
            out.push('$');
            rest = after;
        } else if let Some(after) = tail.strip_prefix('{') {
            match after.find('}') {
                Some(end) => {
                    out.push_str(&lookup(&after[..end]).unwrap_or_default());
                    rest = &after[end + 1..];
                }
                None => {
                    out.push('$');
                    rest = tail;
                }
            }
        } else {
            let end = tail
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(tail.len());
            if end == 0 {
                out.push('$');
                rest = tail;
            } else {
                out.push_str(&lookup(&tail[..end]).unwrap_or_default());
                rest = &tail[end..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_placeholders_expand_from_the_lookup() {
        let lookup = |name: &str| match name {
            "TOKEN" => Some("t0k".to_string()),
            "HOME" => Some("/home/u".to_string()),
            _ => None,
        };
        assert_eq!(expand_env("$TOKEN", lookup), "t0k");
        assert_eq!(
            expand_env("${HOME}/bin:$HOME", lookup),
            "/home/u/bin:/home/u"
        );
        assert_eq!(expand_env("x$MISSING-y", lookup), "x-y");
        assert_eq!(expand_env("cost $$5 $", lookup), "cost $5 $");
        assert_eq!(expand_env("${unterminated", lookup), "${unterminated");
    }

    #[test]
    fn a_server_table_parses_with_defaults() {
        let servers: BTreeMap<String, McpServerConfig> = toml::from_str(
            "[github]\ncommand = \"npx\"\nargs = [\"-y\", \"server\"]\n\n[off]\ncommand = \"x\"\nenabled = false\n",
        )
        .unwrap();
        let github = &servers["github"];
        assert_eq!(github.args, ["-y", "server"]);
        assert_eq!(github.timeout_secs, 60);
        assert!(github.enabled && github.tools.is_none());
        assert!(!servers["off"].enabled);
    }
}
