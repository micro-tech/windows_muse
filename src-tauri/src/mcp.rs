//! Minimal MCP client (stdio JSON-RPC 2.0) for the hub watcher's
//! `how: "mcp"` path (chat-command MCP spec §5).
//!
//! Why a hand-rolled client instead of the `rmcp` SDK: this app is a
//! small sync/blocking Tauri app (blocking reqwest, plain threads, no
//! async runtime). `rmcp` is async (tokio) and would pull in a runtime
//! plus many transitive dependencies for a three-call protocol. The MCP
//! stdio transport is newline-delimited JSON-RPC 2.0, and this client
//! only needs three calls (initialize, tools/list, tools/call), so a
//! sync client using only std + serde_json (both already dependencies)
//! is simpler, has zero new compile risk on Windows, and matches the
//! app's existing sync style.
//!
//! Protocol (MCP stdio, newline-delimited JSON-RPC 2.0):
//! 1. `initialize` request → the server returns its protocol version
//!    and capabilities.
//! 2. `notifications/initialized` notification (no reply expected).
//! 3. `tools/list` request → the tools the server offers (used to
//!    verify the named tool exists before calling it).
//! 4. `tools/call` request → the tool result (text content blocks).
//!
//! The caller bounds the wait with a thread + `recv_timeout` (see
//! `run_mcp_call_timed` in hub.rs); this client itself is blocking.
//! A `time` budget bounds how long the watcher WAITS for the tool
//! call — it does NOT kill a runaway MCP server child (same caveat as
//! the JSON-dispatch `time` budget; a real kill is a follow-up).
//! The server's stderr is discarded (`Stdio::null`): stderr is the
//! conventional MCP server log channel, and a server that logs
//! heavily would fill a piped stderr buffer and deadlock the call.
//! After the call, stdin is closed (EOF) and the child is given a
//! brief grace period to exit; a server that ignores EOF is left
//! running (documented above).

use crate::config::McpServerConfig;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Duration;

/// Default wall-clock budget for an MCP tool call (seconds).
pub const DEFAULT_MCP_TIMEOUT_SECS: u64 = 60;

/// A validated MCP call (envelope parsed, fields checked).
#[derive(Debug, Clone)]
pub struct McpCall {
    /// MCP server name, as configured on this agent.
    pub server: String,
    /// Tool name on that server.
    pub tool: String,
    /// Tool arguments (always an object; defaults to `{}`).
    pub args: Value,
    /// Wall-clock budget for the tool call.
    pub timeout: Duration,
}

/// Validate an MCP envelope (the parsed JSON object after `!`).
/// Returns the call to make, or `Err(reason)` for a message that must
/// be rejected without running anything. Validation order (first
/// failure wins):
/// - `to` missing/not a string → "mcp needs a to field" (required for
///   mcp — no broadcast MCP calls, so a tool never runs twice).
/// - `server` missing/not a string → "mcp needs a server name".
/// - `tool` missing/not a string → "mcp needs a tool name".
/// - `scope` missing → "mcp needs scope \"read\"". `scope == "read"`
///   → ok. A `write:` scope → "mcp writes are not enabled yet".
///   Any other scope → "mcp scope must be \"read\"".
/// - `args` present but not an object → "mcp args must be an object".
///   Absent → `{}`.
/// - `time` present and a positive integer → the budget. Absent or
///   malformed → the 60s default (a malformed `time` is ignored, not
///   a rejection — same leniency as the command path).
///
/// Note: addressing (`to` == this agent's name) is already checked by
/// the watcher before this runs — a `to` naming someone else never
/// reaches here. This only checks that `to` is PRESENT.
pub fn resolve_mcp_command(json: &Value) -> Result<McpCall, String> {
    let Some(obj) = json.as_object() else {
        return Err("mcp message must be a JSON object".to_string());
    };
    let Some(_to) = obj.get("to").and_then(|v| v.as_str()) else {
        return Err("mcp needs a to field".to_string());
    };
    let Some(server) = obj.get("server").and_then(|v| v.as_str()) else {
        return Err("mcp needs a server name".to_string());
    };
    let Some(tool) = obj.get("tool").and_then(|v| v.as_str()) else {
        return Err("mcp needs a tool name".to_string());
    };
    match obj.get("scope").and_then(|v| v.as_str()) {
        Some("read") => {}
        Some(s) if s.starts_with("write:") => {
            return Err("mcp writes are not enabled yet".to_string());
        }
        Some(_) => return Err("mcp scope must be \"read\"".to_string()),
        None => return Err("mcp needs scope \"read\"".to_string()),
    }
    let args = match obj.get("args") {
        Some(args) => {
            if !args.is_object() {
                return Err("mcp args must be an object".to_string());
            }
            args.clone()
        }
        None => Value::Object(serde_json::Map::new()),
    };
    let timeout = obj
        .get("time")
        .and_then(|t| t.as_u64())
        .filter(|&s| s > 0)
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(DEFAULT_MCP_TIMEOUT_SECS));
    Ok(McpCall {
        server: server.to_string(),
        tool: tool.to_string(),
        args,
        timeout,
    })
}

/// Call one MCP tool on a stdio MCP server and return its text result.
///
/// Spawns the server (`command` + `args`), then speaks
/// newline-delimited JSON-RPC 2.0 on its stdin/stdout: `initialize`,
/// `notifications/initialized`, `tools/list` (to verify the tool is
/// offered), `tools/call`. Returns the tool result's text content
/// (concatenated text blocks), or `Err(reason)` if anything fails
/// (spawn, transport, protocol, unknown tool, tool error).
///
/// Blocking: the caller bounds the wait (see `run_mcp_call_timed` in
/// hub.rs). After the call, stdin is closed (EOF) and the child is
/// given a brief grace period to exit (see `shutdown_child`).
pub fn call_mcp_tool(
    server_name: &str,
    server: &McpServerConfig,
    tool: &str,
    args: &Value,
) -> Result<String, String> {
    let mut child = Command::new(&server.command)
        .args(&server.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| format!("failed to start mcp server '{}': {}", server.command, err))?;

    let result = call_mcp_tool_inner(&mut child, server_name, tool, args);
    shutdown_child(&mut child);
    result
}

fn call_mcp_tool_inner(
    child: &mut Child,
    server_name: &str,
    tool: &str,
    args: &Value,
) -> Result<String, String> {
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "mcp server stdin not available".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "mcp server stdout not available".to_string())?;
    let mut writer = BufWriter::new(stdin);
    let mut reader = BufReader::new(stdout);

    // 1. initialize — the server returns its protocol version and
    // capabilities. We accept whatever version it offers (we only use
    // the three basic calls, which are stable across versions).
    let init = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "muse-windows-app", "version": "0.1.0"}
        }
    });
    write_request(&mut writer, &init)?;
    let _init_resp = read_response(&mut reader, 1)?;

    // 2. initialized notification (no id, no reply expected).
    let notif = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
    write_request(&mut writer, &notif)?;

    // 3. tools/list — verify the named tool is actually offered before
    // calling it (a stale config read list is not proof the server has it).
    let list = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}});
    write_request(&mut writer, &list)?;
    let list_resp = read_response(&mut reader, 2)?;
    let tools = list_resp
        .get("result")
        .and_then(|r| r.get("tools"))
        .and_then(|t| t.as_array())
        .ok_or_else(|| "mcp server tools/list returned no tools".to_string())?;
    if !tools
        .iter()
        .any(|t| t.get("name").and_then(|n| n.as_str()) == Some(tool))
    {
        return Err(format!(
            "unknown tool '{}' on server '{}'",
            tool, server_name
        ));
    }

    // 4. tools/call — the tool result is text content blocks.
    let call = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {"name": tool, "arguments": args}
    });
    write_request(&mut writer, &call)?;
    let call_resp = read_response(&mut reader, 3)?;
    let result = call_resp
        .get("result")
        .ok_or_else(|| "mcp server tools/call returned no result".to_string())?;
    let text = extract_text(result);
    if result.get("isError").and_then(|v| v.as_bool()) == Some(true) {
        return Err(if text.is_empty() {
            format!("mcp tool '{}' returned an error", tool)
        } else {
            text
        });
    }
    Ok(text)
}

/// Write one newline-delimited JSON-RPC request to the server's stdin.
fn write_request(writer: &mut BufWriter<ChildStdin>, req: &Value) -> Result<(), String> {
    let line =
        serde_json::to_string(req).map_err(|err| format!("mcp request encode failed: {}", err))?;
    writer
        .write_all(line.as_bytes())
        .and_then(|_| writer.write_all(b"\n"))
        .and_then(|_| writer.flush())
        .map_err(|err| format!("mcp write to server failed: {}", err))
}

/// Read one newline-delimited JSON-RPC response, checking it matches
/// the request id and carries no `error` object. Requests are sent
/// sequentially and the server replies in order, so the next line is
/// always the reply to the last request — an id mismatch means the
/// stream is out of sync.
fn read_response(reader: &mut BufReader<ChildStdout>, want_id: u64) -> Result<Value, String> {
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|err| format!("mcp read from server failed: {}", err))?;
    if line.is_empty() {
        return Err("mcp server closed stdout (EOF)".to_string());
    }
    let resp: Value = serde_json::from_str(&line)
        .map_err(|err| format!("mcp server returned bad JSON: {}", err))?;
    if let Some(err) = resp.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error");
        return Err(format!("mcp server error: {}", msg));
    }
    match resp.get("id").and_then(|i| i.as_u64()) {
        Some(id) if id == want_id => Ok(resp),
        Some(id) => Err(format!(
            "mcp response id {} does not match request {}",
            id, want_id
        )),
        None => Err("mcp response has no id".to_string()),
    }
}

/// Concatenate the text of all `{type: "text", text: "..."}` content
/// blocks in a tool result. Non-text blocks are ignored (v1 only
/// returns text to the room).
fn extract_text(result: &Value) -> String {
    let mut out = String::new();
    if let Some(content) = result.get("content").and_then(|c| c.as_array()) {
        for block in content {
            if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(text);
                }
            }
        }
    }
    out
}

/// Give the server a brief grace period to exit after stdin EOF, then
/// stop waiting. Most stdio servers exit promptly on EOF; a server
/// that ignores EOF is left running — the `time` budget bounds the
/// watcher's wait, not the child (see the module docs). This only runs
/// on the normal (non-timeout) path; on a timeout the spawned thread
/// is abandoned with the child.
fn shutdown_child(child: &mut Child) {
    for _ in 0..20 {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(_) => return,
        }
    }
}
