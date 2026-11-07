//! Pure crew-feed message validation — std-only on purpose.
//!
//! The live pane is fed by JSON posts in the hub's `crew` room, but the
//! domain rules (which names count as which crew member, which statuses are
//! real) have nothing to do with Tauri or the hub. They live here, in a
//! module with no external imports, so the tests run with a standalone
//! `rustc --test` — the same pattern as `allowlist.rs`. The Tauri side
//! (`crew.rs`) extracts the three string fields from the hub message and
//! hands them to [`build_crew_update`]; this module does the judging.

/// The crew roster, in pane order. Fixed — John picked these six names.
pub const ROSTER: &[&str] = &[
    "Surveyor 1",
    "Code Ranger 1",
    "Code Reviewer",
    "Quartermaster",
    "Sapper 1",
    "Artificer 1",
];

/// What a crew member is doing, as the pane shows it.
///
/// `Idle` is a pane state, not a post status — a post can never set it
/// (idle comes from the done/failed display expiring, or from never having
/// posted). It lives here so the pane state and the post vocabulary share
/// one enum instead of two parallel ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrewStatus {
    Idle,
    Working,
    Done,
    Failed,
}

/// A validated crew-room post, ready to apply to the pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrewUpdate {
    /// Canonical roster name (not the shorthand the post used).
    pub agent: &'static str,
    pub status: CrewStatus,
    /// Short label of what the agent is doing (may be empty).
    pub task: String,
}

/// Match a posted agent name to the roster, leniently.
///
/// Accepts the full name ("Code Ranger 1", any casing) and unambiguous
/// shorthands: "Ranger 1" and "Reviewer" (suffixes), "Surveyor" and
/// "Code Ranger" (prefixes). An input that could be two crew members
/// ("1", "Code") matches nothing — guessing wrong is worse than ignoring
/// the post.
pub fn match_crew_name(name: &str) -> Option<&'static str> {
    let want = name.trim().to_ascii_lowercase();
    if want.is_empty() {
        return None;
    }
    // Full name first (case-insensitive).
    if let Some(found) = ROSTER
        .iter()
        .copied()
        .find(|r| r.to_ascii_lowercase() == want)
    {
        return Some(found);
    }
    // Otherwise the input must be a prefix or a suffix of exactly one
    // roster name.
    let mut matches = ROSTER.iter().copied().filter(|r| {
        let lower = r.to_ascii_lowercase();
        lower.starts_with(&want) || lower.ends_with(&want)
    });
    let found = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    Some(found)
}

/// Validate a crew post's fields into an update, or `None` to ignore it.
///
/// `agent` and `status` are required; `task` is optional and defaults to
/// empty. Unknown agent names and unknown statuses are ignored, not errors
/// — a chat room is not a place to crash over.
pub fn build_crew_update(
    agent: Option<&str>,
    status: Option<&str>,
    task: Option<&str>,
) -> Option<CrewUpdate> {
    let agent = match_crew_name(agent?)?;
    let status = status?.trim().to_ascii_lowercase();
    let status = match status.as_str() {
        "working" => CrewStatus::Working,
        "done" => CrewStatus::Done,
        "failed" => CrewStatus::Failed,
        _ => return None,
    };
    Some(CrewUpdate {
        agent,
        status,
        task: task.unwrap_or("").to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test names follow the Ranger convention: method_setup_expectedOutcome,
    // with given/when/then sections.

    #[test]
    fn match_crew_name_with_exact_name_returns_that_agent() {
        // GIVEN a full roster name:
        let name = "Code Ranger 1";

        // WHEN it is matched,
        // THEN it resolves to itself:
        assert_eq!(match_crew_name(name), Some("Code Ranger 1"));
    }

    #[test]
    fn match_crew_name_with_case_insensitive_exact_name_returns_that_agent() {
        // GIVEN a full roster name in the wrong case:
        let name = "code ranger 1";

        // WHEN it is matched,
        // THEN the match is case-insensitive:
        assert_eq!(match_crew_name(name), Some("Code Ranger 1"));
    }

    #[test]
    fn match_crew_name_with_short_suffix_returns_that_agent() {
        // GIVEN the shorthand John named ("Ranger 1"):
        let name = "Ranger 1";

        // WHEN it is matched,
        // THEN it resolves to Code Ranger 1 (the only name ending that way):
        assert_eq!(match_crew_name(name), Some("Code Ranger 1"));
    }

    #[test]
    fn match_crew_name_with_bare_suffix_returns_that_agent() {
        // GIVEN a bare role word ("Reviewer"):
        let name = "Reviewer";

        // WHEN it is matched,
        // THEN it resolves to Code Reviewer:
        assert_eq!(match_crew_name(name), Some("Code Reviewer"));
    }

    #[test]
    fn match_crew_name_with_prefix_returns_that_agent() {
        // GIVEN a first-word shorthand ("Surveyor"):
        let name = "Surveyor";

        // WHEN it is matched,
        // THEN it resolves to Surveyor 1 (the only name starting that way):
        assert_eq!(match_crew_name(name), Some("Surveyor 1"));
    }

    #[test]
    fn match_crew_name_with_ambiguous_suffix_returns_none() {
        // GIVEN a shorthand four crew members share ("1"):
        let name = "1";

        // WHEN it is matched,
        // THEN it matches nothing — guessing wrong is worse than ignoring it:
        assert_eq!(match_crew_name(name), None);
    }

    #[test]
    fn match_crew_name_with_ambiguous_prefix_returns_none() {
        // GIVEN a prefix two crew members share ("Code"):
        let name = "Code";

        // WHEN it is matched,
        // THEN it matches nothing:
        assert_eq!(match_crew_name(name), None);
    }

    #[test]
    fn match_crew_name_with_unknown_name_returns_none() {
        // GIVEN a name that is not on the roster:
        let name = "Bob";

        // WHEN it is matched,
        // THEN it matches nothing:
        assert_eq!(match_crew_name(name), None);
    }

    #[test]
    fn match_crew_name_with_empty_string_returns_none() {
        // GIVEN an empty (or whitespace-only) name:
        let name = "   ";

        // WHEN it is matched,
        // THEN it matches nothing:
        assert_eq!(match_crew_name(name), None);
    }

    #[test]
    fn build_crew_update_with_working_status_and_task_returns_update() {
        // GIVEN a complete working post using a shorthand name:
        let agent = Some("Ranger 1");
        let status = Some("working");
        let task = Some("fixing the build");

        // WHEN it is validated,
        // THEN it resolves to the canonical name with the task attached:
        assert_eq!(
            build_crew_update(agent, status, task),
            Some(CrewUpdate {
                agent: "Code Ranger 1",
                status: CrewStatus::Working,
                task: "fixing the build".to_string(),
            })
        );
    }

    #[test]
    fn build_crew_update_with_done_status_returns_done() {
        // GIVEN a done post:
        let agent = Some("Quartermaster");
        let status = Some("done");
        let task = Some("nightly run");

        // WHEN it is validated,
        // THEN the status is Done:
        assert_eq!(
            build_crew_update(agent, status, task),
            Some(CrewUpdate {
                agent: "Quartermaster",
                status: CrewStatus::Done,
                task: "nightly run".to_string(),
            })
        );
    }

    #[test]
    fn build_crew_update_with_failed_status_returns_failed() {
        // GIVEN a failed post:
        let agent = Some("Sapper 1");
        let status = Some("failed");
        let task = Some("web UI build");

        // WHEN it is validated,
        // THEN the status is Failed:
        assert_eq!(
            build_crew_update(agent, status, task),
            Some(CrewUpdate {
                agent: "Sapper 1",
                status: CrewStatus::Failed,
                task: "web UI build".to_string(),
            })
        );
    }

    #[test]
    fn build_crew_update_with_unknown_status_returns_none() {
        // GIVEN a post with a status that is not in the vocabulary:
        let agent = Some("Ranger 1");
        let status = Some("sleeping");
        let task = Some("napping");

        // WHEN it is validated,
        // THEN it is ignored:
        assert_eq!(build_crew_update(agent, status, task), None);
    }

    #[test]
    fn build_crew_update_with_unknown_agent_returns_none() {
        // GIVEN a post naming someone who is not crew:
        let agent = Some("Bob");
        let status = Some("working");
        let task = Some("something");

        // WHEN it is validated,
        // THEN it is ignored:
        assert_eq!(build_crew_update(agent, status, task), None);
    }

    #[test]
    fn build_crew_update_with_missing_task_defaults_to_empty() {
        // GIVEN a post with no task label:
        let agent = Some("Artificer 1");
        let status = Some("working");
        let task = None;

        // WHEN it is validated,
        // THEN it still applies, with an empty task:
        assert_eq!(
            build_crew_update(agent, status, task),
            Some(CrewUpdate {
                agent: "Artificer 1",
                status: CrewStatus::Working,
                task: String::new(),
            })
        );
    }

    #[test]
    fn build_crew_update_with_missing_agent_returns_none() {
        // GIVEN a post with no agent field:
        let agent = None;
        let status = Some("working");
        let task = Some("something");

        // WHEN it is validated,
        // THEN it is ignored:
        assert_eq!(build_crew_update(agent, status, task), None);
    }

    #[test]
    fn build_crew_update_with_missing_status_returns_none() {
        // GIVEN a post with no status field:
        let agent = Some("Ranger 1");
        let status = None;
        let task = Some("something");

        // WHEN it is validated,
        // THEN it is ignored:
        assert_eq!(build_crew_update(agent, status, task), None);
    }
}
