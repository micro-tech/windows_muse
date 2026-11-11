#![windows_subsystem = "windows"]
//! Muse Windows app — Tauri v2 entry point.
//!
//! One window, three panes (the layout John asked for, modeled on grok-cli's
//! TUI):
//! - a LEFT pane in the app shell listing sub-agents, with a spinner on any
//!   agent that is working;
//! - a MAIN view showing muse.ai;
//! - a TERMINAL pane along the BOTTOM, where John types commands and where
//!   the agent (Muse / a Muse sub-agent) can also run commands via the chat
//!   hub's `command` room.
//!
//! The main view is a CHILD WebView, not an `<iframe>`: muse.ai sends
//! `X-Frame-Options: SAMEORIGIN` and CSP `frame-ancestors 'self'`, so framing
//! it is blocked. The child WebView is a real top-level WebView navigated to
//! https://muse.ai, created with `Window::add_child` (Tauri's `unstable`
//! feature — see Cargo.toml) and `.auto_resize()` so it tracks window
//! resizes. The shell (left pane, terminal, zoom controls) lives in the main
//! window's own WebView, which loads the local `src/index.html`.
//!
//! The shell measures the main-view area and tells Rust its rectangle
//! (`set_muse_view_rect`), so the child WebView sits exactly over it.

mod allowlist;
mod config;
mod crew;
mod crew_parse;
mod hub;
mod mcp;
mod terminal;

use config::{clamp_zoom, load_settings, save_settings};
use crew::{AgentRow, CrewState};
use hub::{start_hub_watcher, HubStatus, HubStatusState};
use std::sync::Mutex;
use tauri::{webview::WebviewBuilder, Emitter, LogicalPosition, LogicalSize, Manager, WebviewUrl};
use terminal::{run_terminal_command_streaming, CommandSource};

/// Shared app state.
struct AppState {
    /// Last main-view rectangle reported by the shell, in logical (CSS) px.
    /// Kept so a resize can re-apply it; the shell re-sends it on layout
    /// changes anyway.
    muse_rect: Mutex<Option<(f64, f64, f64, f64)>>,
}

/// The sub-agent list the left pane renders.
///
/// Live, not static: the rows come from the crew state, which the `crew`
/// watcher in `hub.rs` keeps up to date from the chat hub's `crew` room.
/// With no hub config (or an empty room) every row reads idle, so the pane
/// still shows the roster exactly as the scaffold did.
#[tauri::command]
fn get_agents(crew: tauri::State<'_, CrewState>) -> Vec<AgentRow> {
    crew.rows()
}

/// Position (and size) the muse.ai child WebView over the shell's main-view
/// area. The shell calls this on load and whenever the layout changes
/// (ResizeObserver on the main-view element), passing logical (CSS) pixels —
/// Tauri handles the scale factor, so no DPI math is needed here.
#[tauri::command]
fn set_muse_view_rect(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
) -> Result<(), String> {
    if let Ok(mut rect) = state.muse_rect.lock() {
        *rect = Some((x, y, width, height));
    }
    let Some(muse) = app.get_webview("muse") else {
        return Err("muse webview not found".to_string());
    };
    muse.set_position(LogicalPosition::new(x, y))
        .map_err(|err| err.to_string())?;
    muse.set_size(LogicalSize::new(width.max(50.0), height.max(50.0)))
        .map_err(|err| err.to_string())?;
    Ok(())
}

/// Set the muse.ai WebView zoom (0.5–3.0), persist it, and return the applied
/// value. This is the control John asked for — he reads his browser at 130%,
/// so the app opens at his saved zoom instead.
#[tauri::command]
fn set_zoom(app: tauri::AppHandle, zoom: f64) -> Result<f64, String> {
    let zoom = clamp_zoom(zoom);
    if let Some(muse) = app.get_webview("muse") {
        muse.set_zoom(zoom).map_err(|err| err.to_string())?;
    }
    let mut settings = load_settings();
    settings.zoom = zoom;
    save_settings(&settings);
    Ok(zoom)
}

/// The saved zoom level (1.0 when nothing is saved yet).
#[tauri::command]
fn get_zoom() -> f64 {
    clamp_zoom(load_settings().zoom)
}

/// Run a command in the bottom terminal.
///
/// `source` is `"human"` (John typing — no allowlist, his own command) or
/// `"agent"` (an agent-invoked command — allowlisted and metachar-checked in
/// `terminal.rs`). Output streams back as `terminal-output` events.
#[tauri::command]
fn run_terminal_command(
    app: tauri::AppHandle,
    command: String,
    source: String,
) -> Result<(), String> {
    let source = match source.as_str() {
        "agent" => CommandSource::Agent,
        _ => CommandSource::Human,
    };
    run_terminal_command_streaming(app, command, source)
}

/// Hub watcher status for the UI (configured? last error?).
#[tauri::command]
fn get_hub_status(state: tauri::State<'_, HubStatusState>) -> HubStatus {
    state
        .lock()
        .map(|status| status.clone())
        .unwrap_or(HubStatus {
            configured: false,
            last_error: None,
        })
}

fn main() {
    tauri::Builder::default()
        .manage(AppState {
            muse_rect: Mutex::new(None),
        })
        .manage(HubStatusState::new(HubStatus {
            configured: false,
            last_error: None,
        }))
        .manage(CrewState::default())
        .invoke_handler(tauri::generate_handler![
            get_agents,
            set_muse_view_rect,
            set_zoom,
            get_zoom,
            run_terminal_command,
            get_hub_status,
        ])
        .setup(|app| {
            // NOTE: the child WebView is attached to the plain `Window`, not
            // the `WebviewWindow` — `Window::add_child` (tauri `unstable`
            // feature) exists only on `Window`; `WebviewWindow` exposes no
            // way to reach its inner window, so fetch the window directly.
            let window = app.get_window("main").ok_or("main window not found")?;

            // Default main-view rectangle (left pane 280px, header 48px,
            // bottom terminal 220px). The shell measures the real rectangle
            // on load and corrects this via set_muse_view_rect.
            let inner = window
                .inner_size()
                .unwrap_or(tauri::PhysicalSize::new(1280, 800));
            let left = 280.0;
            let top = 48.0;
            let bottom = 220.0;
            let width = (inner.width as f64 - left).max(100.0);
            let height = (inner.height as f64 - top - bottom).max(100.0);

            // The muse.ai child WebView. `.auto_resize()` keeps it tracking
            // window resizes; the shell's set_muse_view_rect corrects the
            // exact position whenever the layout changes.
            let muse = window.add_child(
                WebviewBuilder::new(
                    "muse",
                    WebviewUrl::External("https://muse.ai".parse().unwrap()),
                )
                .auto_resize(),
                LogicalPosition::new(left, top),
                LogicalSize::new(width, height),
            )?;

            // Open at John's saved zoom (1.0 the first time).
            muse.set_zoom(clamp_zoom(load_settings().zoom))?;

            // The hub watcher runs for the life of the app. If no hub config
            // exists yet it simply reports "not configured" and retries.
            start_hub_watcher(app.handle().clone());

            // Let the shell know the app is ready (it then measures the
            // main-view rectangle and sends it via set_muse_view_rect).
            let _ = app.emit("app-ready", ());

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running the Muse Windows app");
}
