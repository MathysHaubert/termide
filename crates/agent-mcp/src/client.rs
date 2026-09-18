//! JSON-RPC 2.0 over stdio, one line per message, as the MCP stdio
//! transport specifies. Blocking I/O on two threads (stdout, stderr) with
//! replies handed over channels, so a request can wait with a timeout and
//! notice a cancelled run.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use termide_agent_core::{expand_env, CancelToken, McpServerConfig};

/// The protocol revision requested; servers answer with the one they speak.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// A tool as `tools/list` describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

type Pending = Arc<Mutex<HashMap<u64, Sender<Result<Value, String>>>>>;
type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

pub struct McpClient {
    writer: SharedWriter,
    pending: Pending,
    next_id: AtomicU64,
    timeout: Duration,
    server_name: Mutex<String>,
    child: Mutex<Option<Child>>,
}

impl McpClient {
    /// Start the server process and speak to it over its stdin/stdout;
    /// stderr lines go to the log.
    pub fn spawn(name: &str, config: &McpServerConfig) -> Result<Self, String> {
        let mut command = Command::new(&config.command);
        command
            .args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in &config.env {
            command.env(key, expand_env(value, |var| std::env::var(var).ok()));
        }
        if let Some(cwd) = &config.cwd {
            command.current_dir(cwd);
        }
        let mut child = command
            .spawn()
            .map_err(|error| format!("cannot start {}: {error}", config.command))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        if let Some(stderr) = child.stderr.take() {
            let server = name.to_string();
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    log::debug!("mcp {server}: {line}");
                }
            });
        }
        let client = Self::from_streams(stdout, stdin, Duration::from_secs(config.timeout_secs));
        *client.child.lock().unwrap() = Some(child);
        Ok(client)
    }

    /// Speak over any pair of streams (tests, other transports).
    pub fn from_streams(
        reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
        timeout: Duration,
    ) -> Self {
        let writer: SharedWriter = Arc::new(Mutex::new(Box::new(writer)));
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let reader_pending = Arc::clone(&pending);
        let reply_writer = Arc::clone(&writer);
        std::thread::spawn(move || read_loop(reader, reader_pending, reply_writer));
        Self {
            writer,
            pending,
            next_id: AtomicU64::new(1),
            timeout,
            server_name: Mutex::new(String::new()),
            child: Mutex::new(None),
        }
    }

    /// The `initialize` handshake; returns the server's declared name.
    pub fn initialize(&self) -> Result<String, String> {
        let result = self.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "termide", "version": env!("CARGO_PKG_VERSION") }
            }),
            None,
        )?;
        self.notify("notifications/initialized", json!({}))?;
        let name = result["serverInfo"]["name"]
            .as_str()
            .unwrap_or("")
            .to_string();
        *self.server_name.lock().unwrap() = name.clone();
        Ok(name)
    }

    /// Every tool the server offers, following `nextCursor` pages.
    pub fn list_tools(&self) -> Result<Vec<McpToolInfo>, String> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = match &cursor {
                Some(cursor) => json!({ "cursor": cursor }),
                None => json!({}),
            };
            let result = self.request("tools/list", params, None)?;
            for tool in result["tools"].as_array().into_iter().flatten() {
                let Some(name) = tool["name"].as_str() else {
                    continue;
                };
                tools.push(McpToolInfo {
                    name: name.to_string(),
                    description: tool["description"].as_str().unwrap_or("").to_string(),
                    input_schema: tool
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
                });
            }
            match result["nextCursor"].as_str() {
                Some(next) if !next.is_empty() => cursor = Some(next.to_string()),
                _ => return Ok(tools),
            }
        }
    }

    /// Call a tool; the text of the result's content blocks and whether the
    /// server flagged it as an error.
    pub fn call_tool(
        &self,
        name: &str,
        arguments: &Value,
        cancel: &CancelToken,
    ) -> Result<(String, bool), String> {
        let arguments = if arguments.is_object() {
            arguments.clone()
        } else {
            json!({})
        };
        let result = self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
            Some(cancel),
        )?;
        let mut text = String::new();
        for block in result["content"].as_array().into_iter().flatten() {
            let piece = match block["type"].as_str() {
                Some("text") => block["text"].as_str().unwrap_or("").to_string(),
                Some("image") => format!(
                    "[image {}]",
                    block["mimeType"].as_str().unwrap_or("of unknown type")
                ),
                Some("resource") => block["resource"]["text"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        format!(
                            "[resource {}]",
                            block["resource"]["uri"].as_str().unwrap_or("")
                        )
                    }),
                other => format!("[{} content]", other.unwrap_or("unknown")),
            };
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&piece);
        }
        Ok((text, result["isError"].as_bool().unwrap_or(false)))
    }

    /// Send a request and wait for its reply, up to the timeout, giving up
    /// early when `cancel` is set.
    pub fn request(
        &self,
        method: &str,
        params: Value,
        cancel: Option<&CancelToken>,
    ) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if let Err(error) = self.write(&message) {
            self.pending.lock().unwrap().remove(&id);
            return Err(error);
        }
        let outcome = wait(&rx, self.timeout, cancel);
        if outcome.is_err() {
            self.pending.lock().unwrap().remove(&id);
        }
        outcome.map_err(|error| format!("{method}: {error}"))
    }

    pub fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        self.write(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    fn write(&self, message: &Value) -> Result<(), String> {
        let mut line = message.to_string();
        line.push('\n');
        let mut writer = self.writer.lock().unwrap();
        writer
            .write_all(line.as_bytes())
            .and_then(|()| writer.flush())
            .map_err(|error| format!("cannot write to the server: {error}"))
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn wait(
    rx: &Receiver<Result<Value, String>>,
    timeout: Duration,
    cancel: Option<&CancelToken>,
) -> Result<Value, String> {
    let deadline = Instant::now() + timeout;
    loop {
        if cancel.is_some_and(CancelToken::is_cancelled) {
            return Err("aborted".to_string());
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(format!("no reply within {} s", timeout.as_secs()));
        }
        match rx.recv_timeout(left.min(Duration::from_millis(50))) {
            Ok(reply) => return reply,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err("the server closed the connection".to_string())
            }
        }
    }
}

/// Deliver replies to their requests; answer a server's own requests with
/// "method not found" (termide offers no roots or sampling); log the rest.
fn read_loop(reader: impl Read, pending: Pending, writer: SharedWriter) {
    for line in BufReader::new(reader).lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(&line) {
            Ok(message) => message,
            Err(error) => {
                log::debug!("mcp: skipping a line that is not JSON ({error})");
                continue;
            }
        };
        let id = message["id"].as_u64();
        match (id, message.get("method")) {
            (Some(id), None) => {
                let reply = if let Some(error) = message.get("error") {
                    Err(format!(
                        "{} (code {})",
                        error["message"].as_str().unwrap_or("error"),
                        error["code"]
                    ))
                } else {
                    Ok(message["result"].clone())
                };
                if let Some(tx) = pending.lock().unwrap().remove(&id) {
                    let _ = tx.send(reply);
                }
            }
            (Some(id), Some(method)) => {
                log::debug!("mcp: declining server request {method}");
                let reply = json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": { "code": -32601, "message": "method not supported by termide" }
                });
                let mut line = reply.to_string();
                line.push('\n');
                let mut writer = writer.lock().unwrap();
                let _ = writer
                    .write_all(line.as_bytes())
                    .and_then(|()| writer.flush());
            }
            (None, Some(method)) => log::debug!("mcp: notification {method}"),
            (None, None) => {}
        }
    }
    // The server is gone: every waiting request learns it now.
    pending.lock().unwrap().clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::pipe;

    /// A server on the other end of two pipes: answers `initialize`,
    /// `tools/list` (two pages) and `tools/call`, and asks one question of
    /// its own to see it declined.
    fn fake_server() -> McpClient {
        let (to_server_rx, to_server_tx) = pipe().unwrap();
        let (from_server_rx, from_server_tx) = pipe().unwrap();
        std::thread::spawn(move || {
            let mut out = from_server_tx;
            let reply = |out: &mut std::io::PipeWriter, value: Value| {
                writeln!(out, "{value}").unwrap();
            };
            for line in BufReader::new(to_server_rx).lines().map_while(Result::ok) {
                let message: Value = serde_json::from_str(&line).unwrap();
                let id = message["id"].clone();
                match message["method"].as_str() {
                    Some("initialize") => {
                        assert_eq!(message["params"]["clientInfo"]["name"], "termide");
                        reply(
                            &mut out,
                            json!({ "jsonrpc": "2.0", "id": 99, "method": "roots/list" }),
                        );
                        reply(
                            &mut out,
                            json!({ "jsonrpc": "2.0", "id": id, "result": {
                            "protocolVersion": PROTOCOL_VERSION,
                            "serverInfo": { "name": "fake", "version": "0" } } }),
                        );
                    }
                    Some("notifications/initialized") => {}
                    Some("tools/list") => {
                        if message["params"]["cursor"].is_null() {
                            reply(
                                &mut out,
                                json!({ "jsonrpc": "2.0", "id": id, "result": {
                                "tools": [{ "name": "echo", "description": "Echo", "inputSchema": { "type": "object" } }],
                                "nextCursor": "p2" } }),
                            );
                        } else {
                            reply(
                                &mut out,
                                json!({ "jsonrpc": "2.0", "id": id, "result": {
                                "tools": [{ "name": "fail" }] } }),
                            );
                        }
                    }
                    Some("tools/call") => {
                        let name = message["params"]["name"].as_str().unwrap();
                        if name == "echo" {
                            let text = message["params"]["arguments"]["text"].clone();
                            reply(
                                &mut out,
                                json!({ "jsonrpc": "2.0", "id": id, "result": {
                                "content": [{ "type": "text", "text": text }, { "type": "image", "mimeType": "image/png" }] } }),
                            );
                        } else if name == "fail" {
                            reply(
                                &mut out,
                                json!({ "jsonrpc": "2.0", "id": id, "result": {
                                "content": [{ "type": "text", "text": "boom" }], "isError": true } }),
                            );
                        } else {
                            reply(
                                &mut out,
                                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32602, "message": "unknown tool" } }),
                            );
                        }
                    }
                    other => {
                        // The decline of our roots/list request arrives here as
                        // a reply with no method.
                        assert!(other.is_none(), "{message}");
                        assert_eq!(message["error"]["code"], -32601);
                    }
                }
            }
        });
        McpClient::from_streams(from_server_rx, to_server_tx, Duration::from_secs(5))
    }

    #[test]
    fn handshake_listing_and_calls_round_trip() {
        let client = fake_server();
        assert_eq!(client.initialize().unwrap(), "fake");
        let tools = client.list_tools().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["echo", "fail"]);
        assert_eq!(tools[1].input_schema["type"], "object");

        let cancel = CancelToken::new();
        let (text, is_error) = client
            .call_tool("echo", &json!({ "text": "hi" }), &cancel)
            .unwrap();
        assert_eq!(text, "hi\n[image image/png]");
        assert!(!is_error);
        let (text, is_error) = client.call_tool("fail", &json!({}), &cancel).unwrap();
        assert_eq!((text.as_str(), is_error), ("boom", true));
        let error = client.call_tool("nope", &json!({}), &cancel).unwrap_err();
        assert!(error.contains("unknown tool"), "{error}");

        cancel.cancel();
        let aborted = client.call_tool("echo", &json!({}), &cancel).unwrap_err();
        assert!(aborted.contains("aborted"), "{aborted}");
    }

    #[test]
    fn a_silent_server_times_out_and_a_closed_one_is_reported() {
        let (_keep_rx, to_server_tx) = pipe().unwrap();
        let (from_server_rx, from_server_tx) = pipe().unwrap();
        let client =
            McpClient::from_streams(from_server_rx, to_server_tx, Duration::from_millis(120));
        let error = client.request("ping", json!({}), None).unwrap_err();
        assert!(error.contains("no reply within"), "{error}");
        drop(from_server_tx);
        std::thread::sleep(Duration::from_millis(50));
        let error = client.request("ping", json!({}), None).unwrap_err();
        assert!(
            error.contains("closed") || error.contains("no reply"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_real_process_over_stdio_answers() {
        let script = r#"while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2024-11-05","serverInfo":{"name":"sh","version":"0"}}}\n' "$id";;
    *'"tools/list"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"env","description":"Env","inputSchema":{"type":"object"}}]}}\n' "$id";;
    *'"tools/call"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"%s"}]}}\n' "$id" "$GREETING";;
  esac
done"#;
        let config = McpServerConfig {
            command: "sh".into(),
            args: vec!["-c".into(), script.into()],
            env: [("GREETING".to_string(), "hi-$USER_FOR_TEST".to_string())].into(),
            cwd: None,
            tools: None,
            timeout_secs: 5,
            enabled: true,
        };
        std::env::set_var("USER_FOR_TEST", "tester");
        let client = McpClient::spawn("sh", &config).unwrap();
        assert_eq!(client.initialize().unwrap(), "sh");
        assert_eq!(client.list_tools().unwrap()[0].name, "env");
        let (text, _) = client
            .call_tool("env", &json!({}), &CancelToken::new())
            .unwrap();
        assert_eq!(text, "hi-tester");
    }
}
