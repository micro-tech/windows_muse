//! Live crew feed for the left sub-agent pane.
//!
//! The pane shows John's crew — Surveyor 1, Code Ranger 1, Code Reviewer,
//! Quartermaster, Sapper 1, Artificer 1 — and lights up whichever one is
//! working. The feed is the chat hub's `crew` room: when Cobble jr dispatches
//! a crew member it posts `{"agent": "<name>", "status": "working"|"done"|
//! "failed", "task": "<short label>"}` there, and this module applies the
//! post to the pane state and emits a `crew-update` event the shell renders.
//!
//! The app only READS the room. Posts come from Cobble jr at runtime (the
//! sandbox never sends them — see the README contract).
//!
//! Tolerance is the whole point: the room is a chat room, so malformed
//! posts, unknown agents, unknown statuses, and posts from senders outside
//! the allowed list are ignored silently. Nothing here can crash the pane.

use crate::config::HubConfig;
use crate::crew_parse::{build_crew_update, CrewStatus, CrewUpdate, ROSTER};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

/// How long a done/failed state stays visible before the row goes idle again.
const DONE_DISPLAY: Duration = Duration::from_secs(10);

/// One row of the left sub-agent pane.
#[derive(Debug, Clone, Serialize)]
pub struct AgentRow {
    pub name: String,
    pub status: String,
    pub task: String,
    pub working: bool,
}

#[derive(Debug, Clone)]
struct CrewAgentState {
    status: CrewStatus,
    task: String,
    since: Instant,
}

impl Default for CrewAgentState {
    fn default() -> Self {
        Self {
            status: CrewStatus::Idle,
            task: String::new(),
            since: Instant::now(),
        }
    }
}

/// The pane's live state: one entry per crew member that has posted.
/// Missing entries read as idle — a hub that is unconfigured or a room that
/// is empty shows the roster idle, exactly like the scaffold did.
#[derive(Default)]
pub struct CrewState {
    agents: Mutex<HashMap<String, CrewAgentState>>,
}

impl CrewState {
    /// Apply a validated update, stamping it with the current time so the
    /// idle reset can tell a stale update from a newer one. Returns the
    /// stamp, for the caller to hand to [`schedule_idle_reset`].
    pub fn apply(&self, update: CrewUpdate) -> Instant {
        let since = Instant::now();
        // A poisoned mutex still yields its map (single-insert operations
        // have no multi-step invariant to protect), so take it either way.
        let mut agents = self
            .agents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        agents.insert(
            update.agent.to_string(),
            CrewAgentState {
                status: update.status,
                task: update.task,
                since,
            },
        );
        since
    }

    /// Rows for the pane, in roster order. A done/failed row older than
    /// [`DONE_DISPLAY`] reads as idle here too (belt to the delayed reset's
    /// braces — the pane never shows a stale "done" even if the reset
    /// thread is late).
    pub fn rows(&self) -> Vec<AgentRow> {
        let agents = self
            .agents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        rows_from(&agents)
    }
}

/// Build pane rows from an agent map. Kept separate so the idle-reset thread
/// can render the same rows without re-locking.
fn rows_from(agents: &HashMap<String, CrewAgentState>) -> Vec<AgentRow> {
    ROSTER
        .iter()
        .map(|name| {
            let (status, task, working) = match agents.get(*name) {
                Some(state) if state.status == CrewStatus::Working => {
                    ("working", state.task.clone(), true)
                }
                Some(state)
                    if matches!(state.status, CrewStatus::Done | CrewStatus::Failed)
                        && state.since.elapsed() < DONE_DISPLAY =>
                {
                    (
                        if state.status == CrewStatus::Done {
                            "done"
                        } else {
                            "failed"
                        },
                        state.task.clone(),
                        false,
                    )
                }
                _ => ("idle", String::new(), false),
            };
            AgentRow {
                name: name.to_string(),
                status: status.to_string(),
                task,
                working,
            }
        })
        .collect()
}

/// After a done/failed update, put the row back to idle once [`DONE_DISPLAY`]
/// has passed. Runs on a throwaway thread so the poll loop never waits on
/// it. The reset only fires if the row still holds this exact update — a
/// newer post (working again, or a newer done/failed) wins and the stale
/// reset does nothing.
fn schedule_idle_reset(app: AppHandle, agent: &'static str, since: Instant) {
    thread::spawn(move || {
        thread::sleep(DONE_DISPLAY);
        let Some(state) = app.try_state::<CrewState>() else {
            return;
        };
        let rows = {
            let mut agents = state
                .agents
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let still_same = agents
                .get(agent)
                .map(|s| s.since == since && s.status != CrewStatus::Working)
                .unwrap_or(false);
            if still_same {
                agents.insert(agent.to_string(), CrewAgentState::default());
            }
            rows_from(&agents)
        };
        let _ = app.emit("crew-update", rows);
    });
}

/// Handle one message from the `crew` room.
///
/// Only posts from allowed senders (the same list as the command watcher —
/// the hub stamps the sender from the token, so a random room member cannot
/// light up the pane) move anything, and only well-formed JSON with a known
/// agent and a known status. Everything else is ignored silently.
pub fn handle_crew_post(app: &AppHandle, config: &HubConfig, message: &Value) {
    let sender = message.get("sender").and_then(|s| s.as_str()).unwrap_or("");
    if !config.allowed_senders.iter().any(|s| s == sender) {
        return;
    }
    let Some(body) = message.get("body").and_then(|b| b.as_str()) else {
        return;
    };
    let Ok(post) = serde_json::from_str::<Value>(body) else {
        return;
    };
    let Some(update) = build_crew_update(
        post.get("agent").and_then(|a| a.as_str()),
        post.get("status").and_then(|s| s.as_str()),
        post.get("task").and_then(|t| t.as_str()),
    ) else {
        return;
    };
    let Some(state) = app.try_state::<CrewState>() else {
        return;
    };
    let status = update.status;
    let agent = update.agent;
    let since = state.apply(update);
    if matches!(status, CrewStatus::Done | CrewStatus::Failed) {
        schedule_idle_reset(app.clone(), agent, since);
    }
    let _ = app.emit("crew-update", state.rows());
}
