//! Bottom-terminal command execution.
//!
//! Commands run on the machine the app is running on (the ASUS, on Windows)
//! via `std::process::Command`. The shell is PowerShell on Windows
//! (`powershell.exe -NoProfile -NonInteractive -Command <cmd>`) and `sh -c`
//! elsewhere — the shell choice is the only platform-specific branch, and it
//! is cfg-gated so the rest of the module stays platform-agnostic.
//!
//! Two entry points, one for each source of commands:
//! - [`run_terminal_command_streaming`] — the interactive terminal. Spawns a
//!   thread that runs the command and streams stdout/stderr lines back to the
//!   frontend as `terminal-output` events, then a `terminal-exit` event.
//! - [`run_agent_command_capture`] — the hub watcher. Runs an agent command
//!   on the watcher's thread (blocking), streaming lines to the terminal as
//!   they arrive AND returning the combined output, so the watcher can POST it
//!   back to the hub. One execution does both jobs — the command is never run
//!   twice.
//!
//! Human-typed commands are the user's own and run without an allowlist.
//! Agent commands go through the allowlist in `allowlist.rs` first.

use crate::allowlist::{check_agent_command, AllowlistDecision};
use serde::Serialize;
use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use tauri::{AppHandle, Emitter};

/// Who issued a terminal command. Human-typed commands are the user's own and
/// run without an allowlist; agent commands go through [`check_agent_command`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandSource {
    Human,
    Agent,
}

/// One streamed output line, sent to the frontend as a `terminal-output` event.
#[derive(Debug, Clone, Serialize)]
pub struct TerminalOutputLine {
    /// `"stdout"` or `"stderr"`.
    pub stream: &'static str,
    /// The line of output (without its trailing newline).
    pub line: String,
    /// Who issued the command that produced this line.
    pub source: &'static str,
}

/// Terminal exit, sent to the frontend as a `terminal-exit` event.
#[derive(Debug, Clone, Serialize)]
pub struct TerminalExit {
    /// Process exit code (`None` when the process was killed by a signal).
    pub code: Option<i32>,
    /// Who issued the command.
    pub source: &'static str,
}

/// Build the platform shell command for a command string.
///
/// Windows: PowerShell with `-NoProfile -NonInteractive` (no user profile
/// scripts, no interactive prompts — an agent command must never hang waiting
/// for input). Other platforms: `sh -c`.
fn build_shell_command(command: &str) -> Command {
    #[cfg(target_os = "windows")]
    {
        let mut cmd = Command::new("powershell.exe");
        cmd.args(["-NoProfile", "-NonInteractive", "-Command", command]);
        cmd
    }
    #[cfg(not(target_os = "windows"))]
    {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", command]);
        cmd
    }
}

fn source_label(source: CommandSource) -> &'static str {
    match source {
        CommandSource::Human => "human",
        CommandSource::Agent => "agent",
    }
}

/// Run a command on the calling thread, streaming each output line to the
/// frontend as it arrives and returning the exit code plus the combined
/// output. This is the single core both entry points use — one execution,
/// streamed AND captured, never run twice.
fn run_command_blocking(
    app: &AppHandle,
    command: &str,
    source: CommandSource,
) -> (Option<i32>, String) {
    let label = source_label(source);
    let mut child = match build_shell_command(command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            let line = format!("failed to start command: {}", err);
            let _ = app.emit(
                "terminal-output",
                TerminalOutputLine {
                    stream: "stderr",
                    line: line.clone(),
                    source: label,
                },
            );
            return (None, line);
        }
    };

    // One reader thread per stream, so a chatty stderr cannot block stdout.
    // Lines are forwarded to the frontend as they arrive and collected for
    // the combined output.
    // Both streams are boxed as `Box<dyn Read + Send>`: `ChildStdout` and
    // `ChildStderr` are different types, so they cannot share one array
    // unboxed — but both implement `Read`, and the box is `Send`, so the
    // reader threads take them either way.
    let (tx, rx) = mpsc::channel::<TerminalOutputLine>();
    let mut readers = Vec::new();
    for (stream_name, stream) in [
        (
            "stdout",
            child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
        ),
        (
            "stderr",
            child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
        ),
    ] {
        let Some(stream) = stream else {
            continue;
        };
        let tx = tx.clone();
        readers.push(thread::spawn(move || {
            let reader = BufReader::new(stream);
            for line in reader.lines() {
                let line = match line {
                    Ok(line) => line,
                    Err(_) => break,
                };
                let _ = tx.send(TerminalOutputLine {
                    stream: stream_name,
                    line,
                    source: label,
                });
            }
        }));
    }
    drop(tx);

    let mut combined = String::new();
    for output_line in rx {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str(&output_line.line);
        let _ = app.emit("terminal-output", output_line);
    }
    for reader in readers {
        let _ = reader.join();
    }

    let code = child.wait().ok().and_then(|status| status.code());
    (code, combined)
}

/// Run a command in a background thread, streaming output to the frontend.
/// Returns immediately; output arrives as `terminal-output` events and
/// completion as a `terminal-exit` event. This is what the interactive
/// terminal's Tauri command calls.
pub fn run_terminal_command_streaming(
    app: AppHandle,
    command: String,
    source: CommandSource,
) -> Result<(), String> {
    if source == CommandSource::Agent {
        if let AllowlistDecision::Rejected { reason } = check_agent_command(&command) {
            let line = TerminalOutputLine {
                stream: "stderr",
                line: format!("[agent] rejected: {}", reason),
                source: "agent",
            };
            let _ = app.emit("terminal-output", line);
            return Err(reason);
        }
    }

    let label = source_label(source);
    thread::spawn(move || {
        let (code, _) = run_command_blocking(&app, &command, source);
        let _ = app.emit(
            "terminal-exit",
            TerminalExit {
                code,
                source: label,
            },
        );
    });

    Ok(())
}

/// Run an agent command to completion on the calling thread and return its
/// combined output, streaming lines to the terminal as they arrive.
///
/// Used by the hub watcher (which runs on its own background thread, so
/// blocking here does not freeze the UI). Applies the same allowlist check; a
/// rejected command returns an error string, which the watcher posts back as
/// the rejection. Output is capped at `max_bytes` (UTF-8-safe truncation)
/// because the hub caps messages at 8 KB.
pub fn run_agent_command_capture(
    app: &AppHandle,
    command: &str,
    max_bytes: usize,
) -> Result<String, String> {
    if let AllowlistDecision::Rejected { reason } = check_agent_command(command) {
        return Err(format!("rejected: {}", reason));
    }

    let (code, combined) = run_command_blocking(app, command, CommandSource::Agent);
    let output = if combined.is_empty() {
        format!("(no output, exit code {})", code.unwrap_or(-1))
    } else {
        combined
    };
    Ok(truncate_utf8_safe(&output, max_bytes))
}

/// Truncate a string to at most `max_bytes` bytes without splitting a
/// multi-byte character (a byte slice across a `─` or an emoji would panic
/// the same way the RustRecon UTF-8 bug did — floor to a char boundary).
pub fn truncate_utf8_safe(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let end = text.floor_char_boundary(max_bytes);
    format!(
        "{}…\n[output truncated to {} bytes]",
        &text[..end],
        max_bytes
    )
}
