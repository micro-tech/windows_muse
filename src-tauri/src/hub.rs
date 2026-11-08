//! Chat-hub watchers: agent commands for the bottom terminal, and the crew
//! feed for the left sub-agent pane.
//!
//! The bottom terminal is agent-runnable: one loop polls the chat hub's
//! `command` room (the same room Helix's chat-command loop watches), and when
//! a message arrives from an allowed sender (cobble-jr, john) starting with
//! `!`, it runs the rest of the message as an agent command in the terminal
//! and POSTs the output back to the room.
//!
//! The left pane is fed the same way: a second loop polls the hub's `crew`
//! room, and each JSON post there (`{"agent": ..., "status": ..., "task": ...}`)
//! lights up (or clears) one crew member in the pane — see `crew.rs` for the
//! contract. Both rooms share one poll loop (`poll_room`) and one config, so
//! a single hub token covers both; each room gets its own thread, cursor, and
//! seed flag, so a slow command in one room never starves the other.
//!
//! Why polling, not the hub's long-poll `chat_watch`: a 5-second poll is the
//! simplest correct loop for a scaffold, and it needs no persistent connection
//! handling. Upgrading to long-poll is a noted follow-up in the README.
//!
//! Safety notes, all deliberate:
//! - The hub token comes from `config.toml` in the app config dir (0600 on
//!   Unix; the user's own profile dir on Windows). No token is ever in the
//!   repo, in code, or in chat. If the config is missing or the token is a
//!   placeholder, the watchers stay disabled and the UI says so.
//! - On the FIRST poll each watcher seeds its cursor to the newest message id
//!   WITHOUT handling anything, so restarting the app never re-runs old
//!   commands (the same cursor-seed discipline Helix uses) — and never
//!   lights up the pane from a stale `crew` post.
//! - Agent commands go through the same allowlist as the terminal module, so
//!   a hub message can only run a read-only allowlisted command.
//! - Addressing: a structured `!` message carrying a `to` field is for one
//!   agent only — if it names someone else (per `agent_name` in the
//!   config, default `"john"`), this watcher ignores it entirely: no run,
//!   no reply, no terminal display. Plain `!` commands and broadcasts
//!   (no `to`) run as before.
//! - JSON dispatch: a structured `!` message is dispatched on its JSON
//!   envelope (spec §3) — `how == "command"` runs the `command` field
//!   (the raw JSON is never run as a shell command); `how == "llm"` is
//!   rejected (the app has no LLM backends); `how == "mcp"` invokes an
//!   MCP tool on a configured stdio MCP server (see `mcp.rs` — v1 is
//!   read-only, `to` is required, and the result is posted back chunked
//!   `[n/m]` citing the hub message id). Anything else falls through
//!   to the plain path and runs the raw text. `time` (integer seconds)
//!   bounds how long the watcher waits for a JSON command — it does not
//!   kill the process (see `run_agent_command_timed`).
//! - Output posted back is capped at ~7500 bytes (the hub caps messages at
//!   8 KB), truncated on a UTF-8 char boundary.

use crate::config::{load_hub_config, HubConfig};
use crate::terminal::run_agent_command_capture;
use serde_json::Value;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

/// How often the watchers poll the hub.
const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// How long to wait before retrying when the hub config is missing.
const CONFIG_RETRY_INTERVAL: Duration = Duration::from_secs(10);
/// Maximum output bytes posted back to the hub (hub message cap is 8 KB).
const MAX_POST_BYTES: usize = 7500;

/// Status the UI shows for the hub watcher. Stored in app state and served by
/// the `get_hub_status` command.
#[derive(Debug, Clone, serde::Serialize)]
pub struct HubStatus {
    /// True when a config file with a real token was found.
    pub configured: bool,
    /// Last poll error, if any (for the UI to show why it is not working).
    pub last_error: Option<String>,
}

/// The watcher's shared status, kept in app state.
pub type HubStatusState = std::sync::Mutex<HubStatus>;

/// Start the hub watchers on background threads. Returns immediately; the
/// loops run for the life of the app.
pub fn start_hub_watcher(app: AppHandle) {
    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            set_status(
                &app,
                false,
                Some(format!("failed to build HTTP client: {}", err)),
            );
            return;
        }
    };

    // The command watcher owns the hub status the UI shows (configured /
    // last error). The crew watcher does not touch it: a crew fetch failing
    // is indistinguishable from an empty room to the pane (it just stays
    // idle), and the same hub and config back both rooms, so the command
    // thread's status already says whether the hub is reachable.
    start_command_watcher(app.clone(), client.clone());
    start_crew_watcher(app, client);
}

/// The `command`-room watcher: runs `!` commands from allowed senders in
/// the bottom terminal and posts the output back.
fn start_command_watcher(app: AppHandle, client: reqwest::blocking::Client) {
    thread::spawn(move || {
        // Cursor: the newest message id seen. Seeded on the first poll without
        // handling anything, so a restart never re-runs old commands.
        let mut last_seen_id: Option<u64> = None;
        let mut seeded = false;

        loop {
            let Some(config) = load_hub_config() else {
                set_status(
                    &app,
                    false,
                    Some(
                        "hub not configured — add hub_url and hub_token to config.toml".to_string(),
                    ),
                );
                thread::sleep(CONFIG_RETRY_INTERVAL);
                continue;
            };
            set_status(&app, true, None);

            let result = poll_room(
                &client,
                &config,
                "command",
                &mut last_seen_id,
                &mut seeded,
                &mut |message| {
                    let sender = message.get("sender").and_then(|s| s.as_str()).unwrap_or("");
                    let text = message.get("body").and_then(|b| b.as_str()).unwrap_or("");
                    if !config.allowed_senders.iter().any(|s| s == sender) {
                        return;
                    }
                    let Some(command) = text.strip_prefix('!').map(str::trim) else {
                        return;
                    };
                    if command.is_empty() {
                        return;
                    }
                    // Addressing (spec §4 step 4): a structured `!` message
                    // carrying a `to` field is for one agent only. If it
                    // names someone else, ignore it entirely — no run, no
                    // reply, no terminal display. Plain `!` commands (not
                    // JSON) and broadcasts (no `to`) run as before.
                    if let Ok(json) = serde_json::from_str::<Value>(command) {
                        if let Some(to) = json.get("to").and_then(|v| v.as_str()) {
                            if to != config.agent_name {
                                return;
                            }
                        }
                    }
                    handle_agent_command(&client, &config, &app, command, message_id(message));
                },
            );
            match result {
                Ok(()) => set_status(&app, true, None),
                Err(err) => set_status(&app, true, Some(err)),
            }
            thread::sleep(POLL_INTERVAL);
        }
    });
}

/// The `crew`-room watcher: applies JSON crew posts to the left pane.
/// A missing config just leaves the pane idle (the roster with no working
/// agent) — the hub-status dot already says "not configured" via the
/// command watcher.
fn start_crew_watcher(app: AppHandle, client: reqwest::blocking::Client) {
    thread::spawn(move || {
        // Cursor: the newest message id seen. Seeded on the first poll without
        // handling anything, so a restart never lights the pane from stale posts.
        let mut last_seen_id: Option<u64> = None;
        let mut seeded = false;

        loop {
            let Some(config) = load_hub_config() else {
                thread::sleep(CONFIG_RETRY_INTERVAL);
                continue;
            };
            let _ = poll_room(
                &client,
                &config,
                "crew",
                &mut last_seen_id,
                &mut seeded,
                &mut |message| {
                    crate::crew::handle_crew_post(&app, &config, message);
                },
            );
            thread::sleep(POLL_INTERVAL);
        }
    });
}

fn set_status(app: &AppHandle, configured: bool, last_error: Option<String>) {
    if let Some(state) = app.try_state::<HubStatusState>() {
        if let Ok(mut status) = state.lock() {
            *status = HubStatus {
                configured,
                last_error: last_error.clone(),
            };
        }
    }
    let _ = app.emit(
        "hub-status",
        HubStatus {
            configured,
            last_error,
        },
    );
}

/// One poll cycle for one room: fetch new messages since the cursor, advance
/// the cursor, and call `handle` for each new message.
///
/// On the FIRST poll the cursor is seeded to the newest message id WITHOUT
/// calling `handle`, so restarting the app never replays old room traffic
/// (the same cursor-seed discipline Helix uses).
fn poll_room(
    client: &reqwest::blocking::Client,
    config: &HubConfig,
    room: &str,
    last_seen_id: &mut Option<u64>,
    seeded: &mut bool,
    handle: &mut dyn FnMut(&Value),
) -> Result<(), String> {
    let mut url = format!(
        "{}/api/messages?room={}&limit=50",
        config.hub_url.trim_end_matches('/'),
        room
    );
    if let Some(since) = *last_seen_id {
        url.push_str(&format!("&since={}", since));
    }

    let response = client
        .get(&url)
        .bearer_auth(&config.hub_token)
        .send()
        .map_err(|err| format!("hub GET failed: {}", err))?;

    if !response.status().is_success() {
        return Err(format!("hub GET returned {}", response.status()));
    }
    let body: Value = response
        .json()
        .map_err(|err| format!("hub GET returned bad JSON: {}", err))?;

    // The hub returns either `{ "messages": [...] }` or a bare array; accept
    // both so a shape change does not kill the watcher.
    let messages: &[Value] = match body.as_array() {
        Some(messages) => messages,
        None => body
            .get("messages")
            .and_then(|m| m.as_array())
            .map(|m| m.as_slice())
            .unwrap_or(&[]),
    };

    let mut max_id = *last_seen_id;
    for message in messages {
        let Some(id) = message_id(message) else {
            continue;
        };
        if let Some(last) = max_id {
            if id <= last {
                continue;
            }
        }
        max_id = Some(id);

        // First poll: seed the cursor, do not handle anything.
        if !*seeded {
            continue;
        }

        handle(message);
    }

    if !*seeded {
        *seeded = true;
    }
    *last_seen_id = max_id;
    Ok(())
}

/// Extract a numeric message id, accepting either a JSON number or a numeric
/// string (the hub has returned both shapes over time).
fn message_id(message: &Value) -> Option<u64> {
    match message.get("id") {
        Some(Value::Number(n)) => n.as_u64(),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    }
}

/// Run one agent command and post the result back to the `command` room.
///
/// A single execution does both jobs: lines stream into the bottom terminal
/// as they arrive (tagged `[agent]`, so John sees what the agent ran) and the
/// combined output is what gets posted.
///
/// `command` is the text after `!` (trimmed). It is dispatched on its JSON
/// envelope first (see `agent_command_reply`): a JSON `how == "command"`
/// runs the `command` field; `how == "llm"` is rejected; a JSON
/// `how == "mcp"` invokes an MCP tool (see `handle_mcp_command`) and its
/// result is posted back chunked `[n/m]`, citing `message_id`; anything
/// else falls through to the plain path and runs the raw text as a shell
/// command, as before.
fn handle_agent_command(
    client: &reqwest::blocking::Client,
    config: &HubConfig,
    app: &AppHandle,
    command: &str,
    message_id: Option<u64>,
) {
    // MCP dispatch: a JSON envelope with how == "mcp" is an MCP tool call,
    // not a shell command. (The `to` check already ran in the watcher:
    // a `to` naming someone else never reaches here, and a missing `to`
    // is a rejection inside the MCP handler, not a broadcast.)
    if let Ok(json) = serde_json::from_str::<Value>(command) {
        if json.get("how").and_then(|h| h.as_str()) == Some("mcp") {
            handle_mcp_command(client, config, app, &json, message_id);
            return;
        }
    }
    let reply = agent_command_reply(app, command);
    post_agent_reply(client, config, app, &reply);
}

/// Handle a `how: "mcp"` envelope (MCP spec §3/§5): validate, look up
/// the server, check the read-tool allowlist, invoke the tool, and post
/// the result (or a rejection) back to the `command` room, chunked
/// `[n/m]` at 7500 bytes, citing the hub message id. Reports never
/// start with `!`.
///
/// Validation order (first failure wins, nothing runs): envelope fields
/// (`resolve_mcp_command`), then server lookup, then the read-tool
/// allowlist, then the tool call itself (which also verifies via
/// `tools/list` that the server actually offers the tool).
fn handle_mcp_command(
    client: &reqwest::blocking::Client,
    config: &HubConfig,
    app: &AppHandle,
    json: &Value,
    message_id: Option<u64>,
) {
    let header = mcp_header(json, message_id);
    let call = match crate::mcp::resolve_mcp_command(json) {
        Ok(call) => call,
        Err(reason) => {
            post_chunked_reply(
                client,
                config,
                app,
                &format!("{}\nrejected: {}", header, reason),
            );
            return;
        }
    };
    let Some(server_cfg) = config.mcp.servers.get(&call.server) else {
        post_chunked_reply(
            client,
            config,
            app,
            &format!("{}\nrejected: unknown mcp server '{}'", header, call.server),
        );
        return;
    };
    if !server_cfg.read_tools.iter().any(|t| t == &call.tool) {
        post_chunked_reply(
            client,
            config,
            app,
            &format!(
                "{}\nrejected: tool '{}' is not a read tool",
                header, call.tool
            ),
        );
        return;
    }
    let text = match run_mcp_call_timed(
        &call.server,
        server_cfg,
        &call.tool,
        &call.args,
        call.timeout,
    ) {
        McpRun::Done(output) => format!("{}\n{}", header, output),
        McpRun::Rejected(msg) => format!("{}\nrejected: {}", header, msg),
        McpRun::TimedOut(secs) => format!(
            "{}\ntimed out after {}s (mcp call still running)",
            header, secs
        ),
    };
    post_chunked_reply(client, config, app, &text);
}

/// The first line of an MCP reply: `mcp <server>.<tool> (msg <id>)`.
/// Server/tool come from the envelope JSON when present as strings
/// (best effort — validation may have rejected before they were
/// checked); the hub message id is cited when known, so the sender can
/// correlate call → result.
fn mcp_header(json: &Value, message_id: Option<u64>) -> String {
    let server = json.get("server").and_then(|s| s.as_str());
    let tool = json.get("tool").and_then(|t| t.as_str());
    let mut header = String::from("mcp");
    match (server, tool) {
        (Some(s), Some(t)) => header.push_str(&format!(" {}.{}", s, t)),
        (Some(s), None) => header.push_str(&format!(" {}", s)),
        _ => {}
    }
    if let Some(id) = message_id {
        header.push_str(&format!(" (msg {})", id));
    }
    header
}

/// The result of an MCP tool call, for the reply back to the room.
enum McpRun {
    /// The tool ran and returned text.
    Done(String),
    /// The call failed (unknown tool, transport/protocol error, tool
    /// error). The message is a plain error (no "rejected:" prefix;
    /// the handler adds it).
    Rejected(String),
    /// The `time` budget expired while waiting. The MCP server child
    /// may still be running — this bounds the watcher's wait, it does
    /// not kill the child (same caveat as `AgentRun::TimedOut`; a real
    /// kill is a follow-up).
    TimedOut(u64),
}

/// Invoke an MCP tool, bounded by a wall-clock budget.
///
/// The call runs on a spawned thread and the watcher waits at most
/// `timeout`; on expiry it reports a timeout and moves on, leaving a
/// runaway MCP server to finish in the background. (The MCP client
/// itself is blocking; the timeout is enforced here, not in the
/// client — same pattern as `run_agent_command_timed`.)
fn run_mcp_call_timed(
    server_name: &str,
    server: &crate::config::McpServerConfig,
    tool: &str,
    args: &Value,
    timeout: Duration,
) -> McpRun {
    let server_name = server_name.to_string();
    let server = server.clone();
    let tool = tool.to_string();
    let args = args.clone();
    let (tx, rx) = mpsc::channel::<Result<String, String>>();
    thread::spawn(move || {
        let result = crate::mcp::call_mcp_tool(&server_name, &server, &tool, &args);
        let _ = tx.send(result);
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(text)) => McpRun::Done(text),
        Ok(Err(msg)) => McpRun::Rejected(msg),
        Err(mpsc::RecvTimeoutError::Timeout) => McpRun::TimedOut(timeout.as_secs()),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            McpRun::Rejected("mcp call ended without a result".to_string())
        }
    }
}

/// Post a reply back to the `command` room, chunked into `[n/m]` parts
/// of at most MAX_POST_BYTES of content each (the hub caps messages at
/// 8 KB). Each part is prefixed `[n/m] `. Used by the MCP path, whose
/// tool results can exceed one message. A failed POST stops the
/// remaining parts (and is reported in the terminal) so a partial
/// result is not spammed across the room.
fn post_chunked_reply(
    client: &reqwest::blocking::Client,
    config: &HubConfig,
    app: &AppHandle,
    text: &str,
) {
    let chunks = chunk_text(text, MAX_POST_BYTES);
    let total = chunks.len();
    let post_url = format!("{}/api/messages", config.hub_url.trim_end_matches('/'));
    for (i, chunk) in chunks.iter().enumerate() {
        let body = format!("[{}/{}] {}", i + 1, total, chunk);
        let payload = serde_json::json!({
            "room": "command",
            "body": body,
        });
        if let Err(err) = client
            .post(&post_url)
            .bearer_auth(&config.hub_token)
            .json(&payload)
            .send()
        {
            let _ = app.emit(
                "terminal-output",
                crate::terminal::TerminalOutputLine {
                    stream: "stderr",
                    line: format!("[hub] failed to post result: {}", err),
                    source: "agent",
                },
            );
            break;
        }
    }
}

/// Split text into chunks of at most `max_bytes` bytes, never splitting
/// a UTF-8 character. Always returns at least one chunk (possibly
/// empty, when the text is empty).
fn chunk_text(text: &str, max_bytes: usize) -> Vec<&str> {
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + max_bytes).min(text.len());
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            // A single char wider than max_bytes (cannot happen with a
            // sane max_bytes); take one char to guarantee progress.
            end = start
                + text[start..]
                    .chars()
                    .next()
                    .map(|c| c.len_utf8())
                    .unwrap_or(1);
            end = end.min(text.len());
        }
        chunks.push(&text[start..end]);
        start = end;
    }
    if chunks.is_empty() {
        chunks.push("");
    }
    chunks
}

/// Run one agent command and build the reply body to post back to the
/// `command` room.
///
/// Dispatch (spec §3/§4):
/// - JSON with `how == "command"`: run the `command` field through the
///   allowlist + executor. A missing/non-string `command` field is a
///   rejection; nothing runs. `time` (integer seconds) bounds the wait.
/// - JSON with `how == "llm"`: rejection — the app has no LLM backends.
/// - Anything else (not JSON, or JSON without a valid `how`): the plain
///   path — run the raw text as a shell command, as before.
///
/// The `$` line echoes what actually ran: the extracted command for a
/// JSON command, the raw text otherwise (and for a dispatch rejection,
/// the raw text, so the sender can see what was rejected).
fn agent_command_reply(app: &AppHandle, command: &str) -> String {
    match resolve_agent_command(command) {
        Ok((to_run, timeout)) => match run_agent_command_timed(app, &to_run, timeout) {
            AgentRun::Done(output) => format!("$ {}\n{}", to_run, output),
            // The capture's Err already reads "rejected: ..." — post it
            // as-is. (The old code prepended a second "rejected: " here.)
            AgentRun::Rejected(msg) => format!("$ {}\n{}", to_run, msg),
            AgentRun::TimedOut(secs) => format!(
                "$ {}\ntimed out after {}s (command still running)",
                to_run, secs
            ),
        },
        Err(reason) => format!("$ {}\nrejected: {}", command, reason),
    }
}

/// Resolve a `!` message into the shell command to run and an optional
/// wall-clock budget. Returns `Err(reason)` for a message that must be
/// rejected without running anything.
///
/// - Not JSON, or JSON that is not an object, or JSON without a valid
///   `how` → the plain path: run the raw text as a shell command.
/// - JSON `how == "command"` → the `command` field (string, required);
///   `time` (integer seconds, if present and a positive integer) is the
///   budget. A malformed `time` is ignored (no budget), not a rejection —
///   the command still runs, just unbounded like a plain command.
/// - JSON `how == "llm"` → rejection (the app has no LLM backends).
/// - `which` and `scope` are ignored: the app only runs locally, and its
///   allowlist is the enforcement.
///
/// Note: the watcher already parsed this text once for the `to` check.
/// Parsing again here is cheap (≤ 8 KB) and keeps addressing separate
/// from dispatch.
fn resolve_agent_command(command: &str) -> Result<(String, Option<Duration>), String> {
    let Ok(json) = serde_json::from_str::<Value>(command) else {
        return Ok((command.to_string(), None));
    };
    let Some(obj) = json.as_object() else {
        return Ok((command.to_string(), None));
    };
    match obj.get("how").and_then(|h| h.as_str()) {
        Some("command") => {
            let Some(cmd) = obj.get("command").and_then(|c| c.as_str()) else {
                return Err("command JSON needs a command string".to_string());
            };
            let timeout = obj
                .get("time")
                .and_then(|t| t.as_u64())
                .filter(|&s| s > 0)
                .map(Duration::from_secs);
            Ok((cmd.to_string(), timeout))
        }
        Some("llm") => Err("llm tasks are Helix-only — the app has no LLM backends".to_string()),
        _ => Ok((command.to_string(), None)),
    }
}

/// The result of running one agent command, for the reply back to the room.
enum AgentRun {
    /// Ran to completion; the combined output.
    Done(String),
    /// The allowlist rejected it (nothing ran). The message already reads
    /// "rejected: ..." (it comes from `run_agent_command_capture`).
    Rejected(String),
    /// The `time` budget expired while waiting. The child process is still
    /// running in the background — this bounds the watcher's wait, it does
    /// not kill the process. The executor (`terminal.rs`) has no timeout
    /// mechanism and no kill handle reaches this module; a real kill is a
    /// `terminal.rs` follow-up.
    TimedOut(u64),
}

/// Run an agent command, optionally bounded by a wall-clock budget.
///
/// With no budget this is exactly `run_agent_command_capture`. With a
/// budget the capture runs on a spawned thread and the watcher waits at
/// most that long; on expiry it reports a timeout and moves on, leaving
/// the runaway command to finish in the background (its output still
/// streams to the terminal via the events the capture emits).
fn run_agent_command_timed(app: &AppHandle, command: &str, timeout: Option<Duration>) -> AgentRun {
    let Some(timeout) = timeout else {
        return match run_agent_command_capture(app, command, MAX_POST_BYTES) {
            Ok(output) => AgentRun::Done(output),
            Err(msg) => AgentRun::Rejected(msg),
        };
    };
    let app = app.clone();
    let command = command.to_string();
    let (tx, rx) = mpsc::channel::<Result<String, String>>();
    thread::spawn(move || {
        let result = run_agent_command_capture(&app, &command, MAX_POST_BYTES);
        let _ = tx.send(result);
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(output)) => AgentRun::Done(output),
        Ok(Err(msg)) => AgentRun::Rejected(msg),
        Err(mpsc::RecvTimeoutError::Timeout) => AgentRun::TimedOut(timeout.as_secs()),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            AgentRun::Rejected("rejected: command thread ended without a result".to_string())
        }
    }
}

/// Post a reply back to the `command` room. A failed POST is reported in
/// the terminal (stderr) so the failure is visible instead of silent.
fn post_agent_reply(
    client: &reqwest::blocking::Client,
    config: &HubConfig,
    app: &AppHandle,
    reply: &str,
) {
    let post_url = format!("{}/api/messages", config.hub_url.trim_end_matches('/'));
    let payload = serde_json::json!({
        "room": "command",
        "body": reply,
    });
    if let Err(err) = client
        .post(&post_url)
        .bearer_auth(&config.hub_token)
        .json(&payload)
        .send()
    {
        let _ = app.emit(
            "terminal-output",
            crate::terminal::TerminalOutputLine {
                stream: "stderr",
                line: format!("[hub] failed to post result: {}", err),
                source: "agent",
            },
        );
    }
}
