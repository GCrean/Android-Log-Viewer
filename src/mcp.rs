//! Model Context Protocol server: newline-delimited JSON-RPC 2.0 over stdio and/or TCP.
//!
//! Transport threads parse requests and answer the protocol-level methods themselves.
//! `tools/call` is forwarded to the GUI thread through a channel and the thread blocks
//! until the view replies.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub type Reply = Sender<Result<Value, String>>;

pub struct UiRequest {
    pub tool: String,
    pub args: Value,
    pub reply: Reply,
}

pub const DEFAULT_TCP_ADDR: &str = "127.0.0.1:7878";
const SUPPORTED_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Default)]
pub struct McpConfig {
    pub stdio: bool,
    pub tcp: Option<String>,
}

/// Starts the requested transports and returns the queue the GUI should drain.
pub fn start(config: McpConfig) -> Receiver<UiRequest> {
    let (tx, rx) = channel::<UiRequest>();
    if config.stdio {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            let stdout = Arc::new(Mutex::new(std::io::stdout()));
            serve(stdin.lock(), stdout, &tx);
        });
    }
    if let Some(addr) = config.tcp {
        match TcpListener::bind(&addr) {
            Ok(listener) => {
                eprintln!("MCP server listening on tcp://{addr}");
                std::thread::spawn(move || {
                    for stream in listener.incoming().flatten() {
                        let tx = tx.clone();
                        std::thread::spawn(move || {
                            let Ok(read) = stream.try_clone() else { return };
                            serve(BufReader::new(read), Arc::new(Mutex::new(stream)), &tx);
                        });
                    }
                });
            }
            Err(e) => eprintln!("MCP: could not listen on {addr}: {e}"),
        }
    }
    rx
}

fn serve<R: BufRead, W: Write>(reader: R, writer: Arc<Mutex<W>>, ui: &Sender<UiRequest>) {
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = handle_message(&line, ui) {
            let mut w = writer.lock().unwrap();
            if writeln!(w, "{response}").and_then(|_| w.flush()).is_err() {
                break;
            }
        }
    }
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Returns the JSON-RPC response line, or `None` for notifications.
pub fn handle_message(line: &str, ui: &Sender<UiRequest>) -> Option<String> {
    let msg: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return Some(error_response(Value::Null, -32700, &format!("parse error: {e}")).to_string()),
    };
    if msg.is_array() {
        return Some(error_response(Value::Null, -32600, "batch requests are not supported").to_string());
    }
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(Value::Null);

    // Notifications (no id) and stray responses get no reply.
    let id = id?;
    if method.is_empty() {
        return None;
    }

    let result = match method {
        "initialize" => {
            let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
            let version = SUPPORTED_VERSIONS.iter().find(|v| **v == asked).unwrap_or(&SUPPORTED_VERSIONS[0]);
            json!({
                "protocolVersion": version,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "log-viewer", "version": env!("CARGO_PKG_VERSION") },
                "instructions": "Controls a running Log Viewer window: stream Android logcat over ADB, open/save log files, filter and read the log. Line indexes are positions within the currently filtered (visible) list."
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tools() }),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("").to_string();
            if !tools().iter().any(|t| t["name"] == name.as_str()) {
                return Some(error_response(id, -32602, &format!("unknown tool: {name}")).to_string());
            }
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            let (reply, wait) = channel();
            if ui.send(UiRequest { tool: name, args, reply }).is_err() {
                return Some(error_response(id, -32603, "application is shutting down").to_string());
            }
            match wait.recv_timeout(CALL_TIMEOUT) {
                Ok(Ok(v)) => tool_result(&v, false),
                Ok(Err(e)) => tool_result(&Value::String(e), true),
                Err(RecvTimeoutError::Timeout) => tool_result(&Value::String("timed out".into()), true),
                Err(RecvTimeoutError::Disconnected) => tool_result(&Value::String("request dropped".into()), true),
            }
        }
        _ => return Some(error_response(id, -32601, &format!("method not found: {method}")).to_string()),
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string())
}

fn tool_result(value: &Value, is_error: bool) -> Value {
    let text = match value {
        Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    };
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": { "type": "object", "properties": properties, "required": required }
    })
}

pub fn tools() -> Vec<Value> {
    let levels = ["Verbose", "Debug", "Info", "Warn", "Error", "Fatal"];
    vec![
        tool("get_state", "Current source, streaming/paused state, line counts per level, filter, selection, theme and status message.", json!({}), &[]),
        tool("list_devices", "Rescan and list ADB devices (serial, state, model).", json!({}), &[]),
        tool(
            "connect_device",
            "Run `adb connect` for a network device, then return the refreshed device list.",
            json!({ "address": { "type": "string", "description": "host or host:port, e.g. 192.168.1.20:5555" } }),
            &["address"],
        ),
        tool(
            "start_stream",
            "Clear the log and start streaming logcat from a device. Uses the selected device when `serial` is omitted.",
            json!({ "serial": { "type": "string" } }),
            &[],
        ),
        tool("stop_stream", "Stop the live logcat stream (keeps the captured lines).", json!({}), &[]),
        tool("set_paused", "Pause or resume the live stream.", json!({ "paused": { "type": "boolean" } }), &["paused"]),
        tool("open_file", "Load a log file from disk, replacing the current log.", json!({ "path": { "type": "string" } }), &["path"]),
        tool(
            "save_file",
            "Write log lines to a file. By default only the lines that pass the current filter.",
            json!({ "path": { "type": "string" }, "all": { "type": "boolean", "description": "Save every line, ignoring the filter." } }),
            &["path"],
        ),
        tool("clear_logs", "Remove all lines from the view.", json!({}), &[]),
        tool(
            "set_filter",
            "Change the filter. Only the fields you pass are changed.",
            json!({
                "query": { "type": "string", "description": "Search text; empty clears it." },
                "regex": { "type": "boolean" },
                "case_sensitive": { "type": "boolean" },
                "levels": { "type": "array", "items": { "type": "string", "enum": levels }, "description": "Levels to show; all others are hidden." }
            }),
            &[],
        ),
        tool(
            "get_logs",
            "Read lines from the filtered log. Returns up to `limit` lines starting at `offset`, or the newest lines with `from_end`.",
            json!({
                "offset": { "type": "integer", "minimum": 0, "default": 0 },
                "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 100 },
                "from_end": { "type": "boolean", "description": "Count `offset` back from the newest line instead of from the oldest." }
            }),
            &[],
        ),
        tool("select_line", "Select (and scroll to) a line by its index in the filtered list; returns the line.", json!({ "index": { "type": "integer", "minimum": 0 } }), &["index"]),
        tool("set_theme", "Switch the colour theme.", json!({ "mode": { "type": "string", "enum": ["auto", "light", "dark"] } }), &["mode"]),
        tool("set_follow", "Enable or disable auto-scroll to the newest line.", json!({ "follow": { "type": "boolean" } }), &["follow"]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(line: &str) -> Value {
        let (tx, _rx) = channel();
        serde_json::from_str(&handle_message(line, &tx).unwrap()).unwrap()
    }

    #[test]
    fn initialize_and_list() {
        let r = call(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}"#);
        assert_eq!(r["result"]["protocolVersion"], "2024-11-05");
        let r = call(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#);
        assert!(r["result"]["tools"].as_array().unwrap().len() >= 10);
    }

    #[test]
    fn notifications_and_errors() {
        let (tx, _rx) = channel();
        assert!(handle_message(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#, &tx).is_none());
        let r = call(r#"{"jsonrpc":"2.0","id":3,"method":"nope"}"#);
        assert_eq!(r["error"]["code"], -32601);
        let r = call("garbage");
        assert_eq!(r["error"]["code"], -32700);
        let r = call(r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"bogus"}}"#);
        assert_eq!(r["error"]["code"], -32602);
    }
}
