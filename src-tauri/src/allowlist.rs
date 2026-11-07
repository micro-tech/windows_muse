//! Agent-command allowlist for the bottom terminal.
//!
//! The terminal has two sources of commands. Human-typed commands (John at the
//! keyboard) are his own — no allowlist, the shell runs them as-is. Agent-invoked
//! commands (from the chat hub's `command` room, i.e. Muse or a Muse sub-agent)
//! go through this module first: the command name must be on the allowlist and
//! no token may contain a shell operator or quote. That is the same spirit as
//! Helix's chat-command executor (hardcoded allowlist + metachar rejection),
//! because an agent-invoked command must never be able to chain, redirect, or
//! substitute its way into something the allowlist didn't approve.

/// Commands an agent may invoke. Read-only / diagnostic only — nothing here
/// writes, deletes, installs, or changes state. Starts from the Helix
/// chat-command ASUS allowlist, plus a few read-only basics.
pub const ALLOWED_COMMANDS: &[&str] = &[
    // Helix chat-command ASUS allowlist
    "hostname",
    "whoami",
    "get-process",
    "get-service",
    "get-computerinfo",
    "systeminfo",
    // Read-only basics
    "get-date",
    "get-childitem",
    "dir",
    "echo",
    "ping",
    "ipconfig",
    "tasklist",
];

/// Characters that must never appear in an agent-invoked command token.
/// These are the shell operators and quoting characters that let one command
/// become two, redirect output, or substitute another command's output.
/// Backslash and forward slash are deliberately ALLOWED: they are path and
/// flag characters (`C:\Users\john`, `ipconfig /all`) and PowerShell does not
/// treat backslash as an escape character (backtick is, and it is forbidden).
const FORBIDDEN_CHARS: &[char] = &[
    ';', '|', '&', '$', '`', '"', '\'', '<', '>', '(', ')', '{', '}', '\n', '\r',
];

/// The outcome of checking an agent-invoked command.
#[derive(Debug, PartialEq, Eq)]
pub enum AllowlistDecision {
    /// The command may run as-is.
    Allowed,
    /// The command must not run; `reason` is a human-readable explanation
    /// (shown in the terminal and posted back to the hub).
    Rejected { reason: String },
}

/// Check an agent-invoked command string against the allowlist.
///
/// Splits on whitespace, checks the first token (case-insensitive) against
/// [`ALLOWED_COMMANDS`], then checks every token for forbidden characters.
/// Returns [`AllowlistDecision::Allowed`] only if both pass.
pub fn check_agent_command(command: &str) -> AllowlistDecision {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return AllowlistDecision::Rejected {
            reason: "empty command".to_string(),
        };
    }

    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    let name = tokens[0].to_ascii_lowercase();

    if !ALLOWED_COMMANDS.contains(&name.as_str()) {
        return AllowlistDecision::Rejected {
            reason: format!("command '{}' is not on the agent allowlist", tokens[0]),
        };
    }

    for token in &tokens {
        if let Some(bad) = token.chars().find(|c| FORBIDDEN_CHARS.contains(c)) {
            return AllowlistDecision::Rejected {
                reason: format!("token '{}' contains forbidden character '{}'", token, bad),
            };
        }
    }

    AllowlistDecision::Allowed
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test names follow the Ranger convention: method_setup_expectedOutcome,
    // with given/when/then sections.

    #[test]
    fn check_agent_command_with_plain_hostname_is_allowed() {
        // GIVEN an agent command that is just an allowlisted name:
        let command = "hostname";

        // WHEN it is checked,
        // THEN it is allowed:
        assert_eq!(check_agent_command(command), AllowlistDecision::Allowed);
    }

    #[test]
    fn check_agent_command_with_mixed_case_name_is_allowed() {
        // GIVEN an allowlisted cmdlet in PowerShell's usual casing:
        let command = "Get-Process";

        // WHEN it is checked,
        // THEN the match is case-insensitive and it is allowed:
        assert_eq!(check_agent_command(command), AllowlistDecision::Allowed);
    }

    #[test]
    fn check_agent_command_with_simple_flags_is_allowed() {
        // GIVEN an allowlisted cmdlet with simple flags and a value:
        let command = "Get-Process -Name chrome";

        // WHEN it is checked,
        // THEN it is allowed (flags and values are fine as long as they
        // carry no shell operators):
        assert_eq!(check_agent_command(command), AllowlistDecision::Allowed);
    }

    #[test]
    fn check_agent_command_with_semicolon_is_rejected() {
        // GIVEN an agent command that chains a second command:
        let command = "hostname; Remove-Item C:\\Users";

        // WHEN it is checked,
        // THEN it is rejected for the semicolon:
        assert!(matches!(
            check_agent_command(command),
            AllowlistDecision::Rejected { .. }
        ));
    }

    #[test]
    fn check_agent_command_with_pipe_is_rejected() {
        // GIVEN an agent command that pipes output somewhere:
        let command = "hostname | Out-File pwned.txt";

        // WHEN it is checked,
        // THEN it is rejected for the pipe:
        assert!(matches!(
            check_agent_command(command),
            AllowlistDecision::Rejected { .. }
        ));
    }

    #[test]
    fn check_agent_command_with_subexpression_is_rejected() {
        // GIVEN an agent command that substitutes another command's output:
        let command = "$(whoami)";

        // WHEN it is checked,
        // THEN it is rejected (the $ is forbidden, and the name is not
        // allowlisted either — either reason is a rejection):
        assert!(matches!(
            check_agent_command(command),
            AllowlistDecision::Rejected { .. }
        ));
    }

    #[test]
    fn check_agent_command_with_unknown_name_is_rejected() {
        // GIVEN a command whose name is not on the allowlist:
        let command = "Remove-Item C:\\Users\\john";

        // WHEN it is checked,
        // THEN it is rejected for the name, before any token is examined:
        assert!(matches!(
            check_agent_command(command),
            AllowlistDecision::Rejected { .. }
        ));
    }

    #[test]
    fn check_agent_command_with_empty_string_is_rejected() {
        // GIVEN an empty command:
        let command = "   ";

        // WHEN it is checked,
        // THEN it is rejected as empty:
        assert!(matches!(
            check_agent_command(command),
            AllowlistDecision::Rejected { .. }
        ));
    }
}
