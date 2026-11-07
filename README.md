# Muse for Windows

A Tauri v2 (Rust) desktop app that wraps [muse.ai](https://muse.ai) for Windows — built because reading the web app meant running the browser at 130% zoom. It is a window plus a font control, not new Muse features: the main view is muse.ai itself, with two additions John asked for — a left pane showing sub-agents (grok-cli's layout, with a little spinner on any agent that is working) and a terminal pane along the bottom that Muse or a Muse sub-agent can run commands in.

## Layout

```
┌──────────────────────────────────────────────────────┐
│ Muse                    [A−] [———●———] [A+] [130%]  │  ← header: zoom control + hub status
├──────────┬───────────────────────────────────────────┤
│ Agents   │                                           │
│ • Surveyor 1      idle                              │
│ • Code Ranger 1   working  (spinner)                 │  ← muse.ai, in its own WebView
│ • Code Reviewer   idle                              │
│ • Quartermaster   idle                              │
├──────────┴───────────────────────────────────────────┤
│ Terminal — ASUS                                      │
│ PS> hostname                                         │  ← bottom terminal
│ ...                                                  │
└──────────────────────────────────────────────────────┘
```

## Why a child WebView (not an iframe)

muse.ai sends `X-Frame-Options: SAMEORIGIN` and CSP `frame-ancestors 'self'` (verified with `curl -I https://muse.ai` on 2026-11-06), so an `<iframe>` in the app shell is blocked. The main view is therefore a real child WebView navigated to https://muse.ai, created with Tauri's `Window::add_child` (behind Tauri's `unstable` feature — see `src-tauri/Cargo.toml`) and `.auto_resize()`. The shell (left pane, terminal, zoom controls) lives in the main window's own WebView, which loads the local `src/index.html`. The shell measures the main-view area and tells Rust its rectangle (`set_muse_view_rect`), so the child WebView sits exactly over it.

## The bottom terminal

- **John typing:** commands run as-is (his own machine, his own commands).
- **Agent commands:** Muse or a Muse sub-agent sends a command through the chat hub's `command` room (same as Helix's chat-command loop). The app's hub watcher polls the room, and on a message from an allowed sender (`cobble-jr`, `john`) starting with `!`, it runs the command in the bottom terminal and POSTs the output back to the room. Output is capped at ~7500 bytes (the hub caps messages at 8 KB).
- **Safety:** agent commands go through an allowlist (`src-tauri/src/allowlist.rs`) — read-only / diagnostic commands only, starting from the Helix chat-command ASUS allowlist (`hostname`, `whoami`, `Get-Process`, `Get-Service`, `Get-ComputerInfo`, `systeminfo`) plus a few read-only basics. Any token containing a shell operator or quote (`; | & $ \` " ' < > ( ) { }`) is rejected before anything runs. Human-typed commands are not allowlisted.

## Hub setup (agent commands)

The watcher needs a hub token. It lives ONLY in an on-machine config file — never in the repo, never in chat.

1. Copy `config.example.toml` to the app config directory:
   - Windows: `%APPDATA%\muse-windows-app\config.toml`
   - Linux: `~/.config/muse-windows-app/config.toml`
2. Replace `PASTE_YOUR_HUB_TOKEN_HERE` with a hub token minted for this app.
3. Restart the app. The header shows `hub: watching command room` when it is live, or `hub: not configured` until then.

If the config is missing or the token is a placeholder, the watcher stays disabled and everything else (muse.ai view, terminal, zoom) still works.

## Zoom control

The header has A− / A+ buttons, a slider (50%–300%), and a reset. It sets the muse.ai WebView zoom (not the shell's) and persists it in `settings.json` in the same config directory, so the app opens at your saved zoom. Default is 100%.

## Sub-agent pane — TODO (John decides what feeds it)

The left pane currently renders a static example list (Surveyor 1, Code Ranger 1, Code Reviewer, Quartermaster) so the layout and the working spinner are visible. The feed options are:

- Helix sub-agents via the chat hub,
- grok-cli agents,
- the Muse activity.

Pick one and the `get_agents` command in `src-tauri/src/main.rs` gets wired to it. Do not invent a data source in the meantime.

## Build and run (on the ASUS, Windows)

Prerequisites:

- Rust via rustup (https://rustup.rs) — `cargo` and `rustc` on PATH.
- Tauri CLI v2: `cargo install tauri-cli --version "^2"`
- WebView2 — preinstalled on Windows 10/11 (no action needed).

Then, from the repo root:

```
cargo tauri dev        # run it (serves the static files in src/)
cargo tauri build      # build the installer (NSIS) into src-tauri/target/release/bundle/
```

`cargo tauri dev` serves the plain HTML/CSS/JS in `src/` — there is no bundler and no npm step. The shell uses Tauri's global JS API (`window.__TAURI__`), enabled by `"withGlobalTauri": true` in `tauri.conf.json`.

## Project layout

```
muse-windows-app/
  README.md
  config.example.toml      # hub config template (placeholder token only)
  src/                     # app shell (plain HTML/CSS/JS, no bundler)
    index.html
    styles.css
    app.js
  src-tauri/
    Cargo.toml             # tauri v2 (+ "unstable" for Window::add_child)
    tauri.conf.json
    build.rs
    src/
      main.rs              # entry: child WebView, zoom, Tauri commands
      allowlist.rs         # agent-command allowlist + metachar rejection (+ tests)
      config.rs            # hub config + settings (zoom) load/save
      terminal.rs          # run commands, stream output to the terminal
      hub.rs               # chat-hub command-room watcher
```

## Known limitations / follow-ups

- **Tauri `unstable` feature:** `Window::add_child` (the child WebView API) is behind Tauri's `unstable` feature. If Tauri stabilizes it, drop the flag from `src-tauri/Cargo.toml`. If the child WebView misbehaves on Windows, the fallback is a second window for muse.ai instead of a child WebView.
- **Hub watcher polls** every 5 seconds. Upgrading to the hub's long-poll `chat_watch` is a follow-up.
- **No JSON envelope (yet):** the watcher handles `!` + plain command text through the allowlist. Helix's `!` + JSON envelope (`how`/`which`/`time`/`scope`) is not parsed here — a follow-up if the app should accept structured tasks.
- **Terminal is a simple scrollback + input line**, not a full terminal emulator. xterm.js is the upgrade path if the simple terminal is not enough (it would replace the scrollback div, not the Rust backend).
- **Terminal height is fixed (220px).** A draggable splitter between the main view and the terminal is a follow-up.
- **The scaffold has not been built on Windows yet.** The sandbox `cargo check` could not complete (Tauri on Linux needs webkit2gtk system libraries, which are not installed there). The first real verification is `cargo tauri dev` on the ASUS — the Rust code is platform-agnostic except the cfg-gated shell choice (PowerShell on Windows, `sh` elsewhere).
