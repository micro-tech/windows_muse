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
//! - Output posted back is capped at ~7500 bytes (the hub caps messages at
//!   8 KB), truncated on a UTF-8 char boundary.

use crate::config::{load_hub_config, HubConfig};
use crate::terminal::run_agent_command_capture;
use serde_json::Value;
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
                    handle_agent_command(&client, &config, &app, command);
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
fn handle_agent_command(
    client: &reqwest::blocking::Client,
    config: &HubConfig,
    app: &AppHandle,
    command: &str,
) {
    let reply = match run_agent_command_capture(app, command, MAX_POST_BYTES) {
        Ok(output) => format!("$ {}\n{}", command, output),
        Err(reason) => format!("$ {}\nrejected: {}", command, reason),
    };

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
