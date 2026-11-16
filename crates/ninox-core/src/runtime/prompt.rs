//! Agent input-prompt detection shared by every [`super::SessionBackend`]:
//! both tmux (`capture-pane -p`) and ptyd (`ScreenSnapshot::to_plain_text`)
//! hand these helpers the pane's visible screen as plain text.

/// A burst of injected characters makes Claude Code's TUI enter paste
/// handling; an Enter arriving before that settles is swallowed and the
/// message sits unsubmitted in the input box. Wait this long before Enter.
pub(crate) const SEND_SUBMIT_DELAY_MS: u64 = 300;
/// After Enter, re-check delivery this many times, this far apart,
/// re-sending Enter whenever the message is still visible at the prompt.
pub(crate) const SEND_VERIFY_ATTEMPTS: u32 = 3;
pub(crate) const SEND_VERIFY_DELAY_MS: u64 = 500;

/// The trimmed content after the pane's last `❯` input-prompt line, with the
/// input box's right border/padding stripped (`  msg   │` → `msg`). `None`
/// when no prompt line is visible at all (plain shell, alt-screen app) —
/// distinct from `Some("")`, an empty-but-present input box. Shared by
/// [`message_stuck_at_prompt`] and `wake_idle_session`.
pub(crate) fn prompt_line_content(pane: &str) -> Option<&str> {
    const PROMPT: char = '❯';
    let line = pane.lines().rev().find(|l| l.contains(PROMPT))?;
    let after = &line[line.rfind(PROMPT).unwrap() + PROMPT.len_utf8()..];
    // Strip the input box's right border and padding: `  msg   │`.
    Some(after.trim().trim_end_matches('│').trim())
}

/// Whether a message we just injected is sitting unsubmitted in the target
/// pane's input box. Looks at the last line holding the `❯` input prompt:
/// stuck means it shows a `[Pasted text #N]` attachment marker or the
/// message text itself (prefix-matched both ways, since the box truncates
/// long lines). Anything else after the prompt — empty box, a placeholder
/// hint, a human's half-typed message — is NOT ours to submit, so this
/// stays false and no retry Enter is ever sent at it.
pub(crate) fn message_stuck_at_prompt(pane: &str, text: &str) -> bool {
    let Some(content) = prompt_line_content(pane) else {
        return false;
    };
    if content.is_empty() {
        return false;
    }
    if content.starts_with("[Pasted text") {
        return true;
    }
    let first_line = text.lines().next().unwrap_or("").trim();
    if first_line.is_empty() {
        return false;
    }
    if content.starts_with(first_line) {
        return true;
    }
    // Truncated stuck message: the box cuts long lines at the pane edge,
    // so the visible content is a leading fragment of the message. Require
    // it to be long enough to be distinctive — every reaction starts with
    // `[Ninox]`, and a human who has typed `[` (or `[Ninox] C`) when this
    // check runs must not have their unfinished input Enter'd for them. A
    // genuinely truncated line is pane-width, far above this floor.
    const MIN_TRUNCATED_MATCH_CHARS: usize = 10;
    content.chars().count() >= MIN_TRUNCATED_MATCH_CHARS && first_line.starts_with(content)
}

/// Marker text for `wake_idle_session`'s idle-wake nudge — never the
/// actual message, which (when the opt-in file-based inbox is enabled) is
/// delivered entirely through the Stop/UserPromptSubmit hooks' JSON output,
/// not the keystroke. Kept short and distinctive so it can never collide
/// with real content a human might be mid-typing.
pub(crate) const IDLE_WAKE_NUDGE: &str = ".";

/// Whether the pane's prompt currently holds ONLY the idle-wake nudge
/// marker — nothing added, nothing removed. Used both right before sending
/// Enter and (implicitly, by its absence) to detect that Enter already
/// submitted cleanly.
pub(crate) fn nudge_still_alone_at_prompt(pane: &str) -> bool {
    prompt_line_content(pane) == Some(IDLE_WAKE_NUDGE)
}

/// How often `wait_for_input_prompt` re-checks the pane while waiting.
pub(crate) const PROMPT_POLL_DELAY_MS: u64 = 500;
