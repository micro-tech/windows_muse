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
│ • Sapper 1        idle                              │
│ • Artificer 1     idle                              │
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

## Sub-agent pane — live crew feed

The left pane shows John's crew — Surveyor 1, Code Ranger 1, Code Reviewer, Quartermaster, Sapper 1, Artificer 1 — and lights up whichever one is working. The feed is the chat hub's `crew` room, using the same config and token as the command watcher (one token covers both rooms).

### The contract (for whoever posts to the room)

A post in the `crew` room is a JSON object as the message body:

```json
{"agent": "Code Ranger 1", "status": "working", "task": "fixing the build"}
```

- `agent` — a crew name. The full name works, any casing; so do unambiguous shorthands: `"Ranger 1"` for Code Ranger 1, `"Reviewer"` for Code Reviewer, `"Surveyor"` for Surveyor 1. An input that could be two crew members (`"1"`, `"Code"`) matches nothing — guessing wrong is worse than ignoring the post. Unknown names are ignored.
- `status` — one of `"working"`, `"done"`, `"failed"`. Anything else is ignored.
- `task` — a short label of what the agent is doing, shown under the name while it works (and briefly while it shows done/failed). Optional; missing means empty.
- Only posts from allowed senders (`cobble-jr`, `john` — the same list as the command watcher) move the pane. The hub stamps the sender from the token, so a random room member cannot light it up.

Behavior: a `"working"` post shows the spinner on that agent plus the task label. A `"done"`/`"failed"` post clears the spinner and shows the done/failed state for 10 seconds, then the row goes back to idle. If the hub is not configured, or the room is empty, the pane shows the whole roster idle. Malformed posts are ignored silently — the room is a chat room, not a place to crash over. On startup the watcher seeds its cursor to the newest message, so a stale post from before launch never lights up the pane.

Posts come from Cobble jr at runtime: when it dispatches a crew member it posts `working` with the task label, then `done` or `failed` when that member finishes. The app only reads the room — it never posts there.

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

## Installing (on the ASUS, Windows)

The repo ships a small standalone Rust installer (`installer/`) that copies the built app into `%LOCALAPPDATA%\muse-windows-app` — no admin required. It creates Desktop + Start Menu shortcuts, writes a fresh `config.toml` from `config.example.toml` only if you don't already have one (an existing config is never overwritten, so your hub token is safe), copies itself into the install dir, and registers an uninstaller under Settings → Apps → Muse for Windows.

1. Build the app (release):
   ```
   cargo tauri build
   ```
   This puts `muse-windows-app.exe` in `src-tauri/target/release/`.
2. Build the installer:
   ```
   cargo build --release --manifest-path installer/Cargo.toml
   ```
   (or `cd installer && cargo build --release`) — this produces `installer/target/release/muse-installer.exe`.
3. Put `muse-installer.exe` in the same folder as `muse-windows-app.exe` and run it:
   ```
   muse-installer.exe
   ```
   Or point it at the exe explicitly:
   ```
   muse-installer.exe --from C:\path\to\muse-windows-app.exe
   ```
4. Edit `%APPDATA%\muse-windows-app\config.toml` and replace `PASTE_YOUR_HUB_TOKEN_HERE` with your hub token (the installer never writes a real token). Restart the app.

Flags:
- `--silent` / `-s` — no prompts, defaults (install dir, both shortcuts, reinstall over an existing install).
- `--uninstall` — removes the install dir, both shortcuts, and the uninstaller registration. Your `config.toml` (hub token) is left alone — delete it by hand if you want it gone.
- `--force` / `-f` — reinstall over an existing install without asking (interactive mode only; silent already overwrites).
- `--from <path>` — path to the `muse-windows-app.exe` to install. Without it, the installer looks next to itself, then at the repo's `src-tauri/target/release/`.

To uninstall later: `muse-installer.exe --uninstall`, or Settings → Apps → Muse for Windows → Uninstall.

## Project layout

```
muse-windows-app/
  README.md
  config.example.toml      # hub config template (placeholder token only)
  installer/               # standalone Rust installer (muse-installer.exe, no admin)
    Cargo.toml
    src/main.rs
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
      crew.rs              # crew pane state + `crew`-room posts -> `crew-update` events
      crew_parse.rs        # crew message validation + name matching, std-only (+ tests)
      terminal.rs          # run commands, stream output to the terminal
      hub.rs               # chat-hub watchers: `command` room + `crew` room
```

## Known limitations / follow-ups

- **Tauri `unstable` feature:** `Window::add_child` (the child WebView API) is behind Tauri's `unstable` feature. If Tauri stabilizes it, drop the flag from `src-tauri/Cargo.toml`. If the child WebView misbehaves on Windows, the fallback is a second window for muse.ai instead of a child WebView.
- **Hub watchers poll** every 5 seconds — both the `command` room and the `crew` room. Upgrading to the hub's long-poll `chat_watch` is a follow-up.
- **Crew done/failed shows for 10 seconds**, then the row goes back to idle. A newer post for the same agent always wins over a pending reset.
- **No JSON envelope (yet):** the watcher handles `!` + plain command text through the allowlist. Helix's `!` + JSON envelope (`how`/`which`/`time`/`scope`) is not parsed here — a follow-up if the app should accept structured tasks.
- **Terminal is a simple scrollback + input line**, not a full terminal emulator. xterm.js is the upgrade path if the simple terminal is not enough (it would replace the scrollback div, not the Rust backend).
- **Terminal height is fixed (220px).** A draggable splitter between the main view and the terminal is a follow-up.
- **The scaffold has not been built on Windows yet.** The sandbox `cargo check` could not complete (Tauri on Linux needs webkit2gtk system libraries, which are not installed there). The first real verification is `cargo tauri dev` on the ASUS — the Rust code is platform-agnostic except the cfg-gated shell choice (PowerShell on Windows, `sh` elsewhere).
