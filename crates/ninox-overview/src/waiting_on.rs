//! A human-readable "Waiting on" phrase for a `Blocked` session's
//! `activity_note` (MLOPS-5339 slice 5 item 2) — the GUI detail panel
//! previously showed the raw hook-written note verbatim (e.g.
//! `[awaiting] input: AskUserQuestion`), exposing Claude Code's internal
//! tool name instead of anything a human reading the panel would
//! understand.

use ninox_core::worker_status::{classify_blocked, BlockedKind};

/// Must match `ninox_core::worker_status::format_permission_note`'s own
/// `"{HOOK_AWAITING_MARKER}permission: "` prefix exactly — that function is
/// private to its module, so this is a literal-string coupling rather than
/// a shared constant.
const PERMISSION_PREFIX: &str = "[awaiting] permission: ";

/// Pulls the backtick-quoted detail (a command/path/JSON fragment) out of a
/// `[awaiting] permission: <tool> \`<detail>\`` note, dropping the leading
/// tool name so the raw Claude Code tool identifier never surfaces.
fn permission_detail(note: &str) -> Option<&str> {
    let rest = note.strip_prefix(PERMISSION_PREFIX)?;
    let start = rest.find('`')? + 1;
    let end = start + rest[start..].rfind('`')?;
    (end > start).then(|| &rest[start..end])
}

/// Human-readable "Waiting on" text for a blocked session, or `None` when
/// there's nothing to show. Never surfaces a raw Claude Code tool name
/// (`AskUserQuestion`, `Bash`, …).
pub fn waiting_on_text(note: Option<&str>) -> Option<String> {
    let note = note.map(str::trim).filter(|n| !n.is_empty())?;
    match classify_blocked(Some(note)) {
        BlockedKind::AskingQuestion => Some("Waiting for your answer in the pane.".to_string()),
        BlockedKind::AwaitingPermission => match permission_detail(note) {
            Some(detail) => Some(format!("Needs permission to run: {detail}")),
            None => Some("Needs your permission to continue.".to_string()),
        },
        BlockedKind::Plain => Some(note.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::waiting_on_text;

    #[test]
    fn asking_a_question_gets_a_human_phrase_not_the_raw_tool_name() {
        let text = waiting_on_text(Some("[awaiting] input: AskUserQuestion")).unwrap();
        assert!(!text.contains("AskUserQuestion"));
        assert_eq!(text, "Waiting for your answer in the pane.");
    }

    #[test]
    fn awaiting_permission_shows_the_command_detail_not_the_tool_name() {
        let text = waiting_on_text(Some("[awaiting] permission: Bash `rm -rf node_modules`")).unwrap();
        assert!(!text.contains("Bash"));
        assert!(text.contains("rm -rf node_modules"));
    }

    #[test]
    fn an_explicit_operator_note_passes_through_unchanged() {
        assert_eq!(
            waiting_on_text(Some("waiting on schema migration")).as_deref(),
            Some("waiting on schema migration")
        );
    }

    #[test]
    fn none_and_blank_notes_show_nothing() {
        assert_eq!(waiting_on_text(None), None);
        assert_eq!(waiting_on_text(Some("   ")), None);
    }

    #[test]
    fn a_permission_note_missing_its_backtick_detail_falls_back_to_a_human_phrase_not_a_panic() {
        let text = waiting_on_text(Some("[awaiting] permission: Bash")).unwrap();
        assert!(!text.contains("Bash"));
    }
}
