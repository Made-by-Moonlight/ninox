//! Worker activity reporting: the single write path behind
//! `ninox worker-status` (explicit `set` and the Claude Code
//! `UserPromptSubmit`/`Stop`/`PreToolUse`/`PermissionRequest` hook verbs).
//! All transition rules live in [`apply_activity`] — the CLI entry points in
//! `main.rs` are dumb pipes.
//!
//! The writer is a short-lived external process (like `ninox statusline`),
//! so the GUI learns about these writes via the poller's external-write
//! re-broadcast, not an event emitted here.

use crate::store::Store;
use crate::types::{ActivityState, Session, SessionId};
use anyhow::Result;
use std::path::Path;

/// What triggered an activity write. Hook events carry their own fixed
/// semantics; only `Explicit` lets the caller pick a state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivityEvent {
    /// `ninox worker-status set <state> [--note …]` — always applied.
    Explicit { state: ActivityState, note: Option<String> },
    /// `UserPromptSubmit` fired: a turn is starting.
    HookPrompt,
    /// `Stop` fired. `turn_continues` means the caller knows the agent
    /// isn't actually stopping — e.g. pending inbox messages mean the
    /// sibling `inbox drain-stop` hook will block the Stop and the agent
    /// keeps working on the injected instructions.
    HookStop { turn_continues: bool },
    /// `PreToolUse` fired for a synchronous interactive tool
    /// (`AskUserQuestion`, `ExitPlanMode`) that's about to show the human a
    /// prompt mid-turn. Applies like `Explicit` *except* over an explicit,
    /// self-declared `Blocked`, which it never overwrites (see
    /// `apply_activity`'s rules).
    HookAwaitingInput { tool_name: String },
    /// `PermissionRequest` fired: Claude Code is about to ask for (or
    /// auto-deny) permission to run `tool_name` with `tool_input`. Same
    /// explicit-`Blocked` exception as `HookAwaitingInput`.
    HookAwaitingPermission { tool_name: String, tool_input: serde_json::Value },
}

/// Marks an `activity_note` as written by [`ActivityEvent::HookAwaitingInput`]
/// or [`ActivityEvent::HookAwaitingPermission`] rather than an operator's own
/// `ninox worker-status set blocked --note …`. Claude Code cannot fire `Stop`
/// while a synchronous interactive tool call is still waiting on a human, so
/// `Stop` firing at all is proof such a prompt already resolved — unlike an
/// explicit self-declared `Blocked`, which has no such liveness guarantee and
/// must survive `Stop` until a human (or the orchestrator) addresses it. This
/// prefix is how [`apply_activity`] tells the two apart without a dedicated
/// persisted field: a deliberate trade-off against a schema migration, with
/// one known sharp edge — an operator whose own `--note` happens to start
/// with this exact prefix would be misclassified as hook-driven. Picked to
/// be distinctive enough that's unlikely in practice.
const HOOK_AWAITING_MARKER: &str = "[awaiting] ";

fn is_hook_awaiting_note(note: &str) -> bool {
    note.starts_with(HOOK_AWAITING_MARKER)
}

/// Whether `session` is currently an *explicit*, self-declared `Blocked` —
/// i.e. `Blocked` with no [`HOOK_AWAITING_MARKER`] note. This must never be
/// overwritten by a hook-driven awaiting event (which would silently lose
/// the operator's declaration) or cleared by `HookStop` (which exists to
/// preserve it past turn end in the first place).
fn is_explicit_blocked(session: &Session) -> bool {
    session.activity == ActivityState::Blocked
        && !session.activity_note.as_deref().is_some_and(is_hook_awaiting_note)
}

/// Build the `activity_note` for a `PermissionRequest` hook: pulls a short,
/// human-readable detail out of common `tool_input` shapes (`command` for
/// Bash, `file_path`/`path` for file and search tools) and falls back to the
/// compact JSON, truncated so one runaway field can't blow up the note.
fn format_permission_note(tool_name: &str, tool_input: &serde_json::Value) -> String {
    const MAX_CHARS: usize = 80;
    let detail = tool_input.get("command")
        .or_else(|| tool_input.get("file_path"))
        .or_else(|| tool_input.get("path"))
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| tool_input.to_string());
    let detail = if detail.chars().count() > MAX_CHARS {
        let truncated: String = detail.chars().take(MAX_CHARS).collect();
        format!("{truncated}…")
    } else {
        detail
    };
    // Keep it single-line and free of backticks so an arbitrary command
    // (e.g. containing its own backtick or newline) can't make the note
    // read as if it ended early or spanned lines.
    let detail = detail.replace(['\n', '\r'], " ").replace('`', "'");
    format!("{HOOK_AWAITING_MARKER}permission: {tool_name} `{detail}`")
}

/// Apply `event` to the session's activity fields. Returns the updated row
/// when a write occurred, `None` when nothing was written (unknown or
/// terminal session, or a no-op transition).
///
/// Rules:
/// - `Explicit` always applies; the note is replaced (or cleared) as given.
/// - `HookPrompt` sets `Working` — a new turn supersedes a stale `Blocked`.
///   If already `Working` it's a no-op, so a self-reported working note
///   isn't clobbered by the next prompt.
/// - `HookStop` sets `Idle` from `Working`/`Unknown`/a hook-driven `Blocked`
///   (see [`is_explicit_blocked`]) but never overwrites an *explicit*
///   `Blocked` — an agent that declared itself blocked mid-turn stays
///   blocked past turn end, which is the whole point of declaring it.
/// - `HookAwaitingInput`/`HookAwaitingPermission` behave like `Explicit`
///   `Blocked` *except* they never overwrite an explicit `Blocked` either —
///   doing so would both lose the operator's note and (since the new note
///   would carry the hook marker) make the next `Stop` wrongly clear it.
/// - `activity_since` resets only when the state value actually changes,
///   mirroring `GateStatus::since`.
/// - The note is cleared on any state change that doesn't bring its own.
pub fn apply_activity(
    store: &Store,
    session_id: &str,
    event: ActivityEvent,
    now: i64,
) -> Result<Option<Session>> {
    let Some(mut session) = store.get_session(session_id)? else {
        return Ok(None);
    };
    if session.status.is_terminal() {
        return Ok(None);
    }
    let (state, note) = match event {
        ActivityEvent::Explicit { state, note } => (state, note),
        ActivityEvent::HookPrompt | ActivityEvent::HookStop { turn_continues: true } => {
            if session.activity == ActivityState::Working {
                return Ok(None);
            }
            (ActivityState::Working, None)
        }
        ActivityEvent::HookStop { turn_continues: false } => {
            if session.activity == ActivityState::Idle {
                return Ok(None);
            }
            // A hook-driven "awaiting" Blocked (tagged via
            // HOOK_AWAITING_MARKER) is cleared like any other state — see
            // the marker's doc comment for why that's safe. An explicit,
            // self-declared Blocked is preserved exactly as before.
            if is_explicit_blocked(&session) {
                return Ok(None);
            }
            (ActivityState::Idle, None)
        }
        ActivityEvent::HookAwaitingInput { tool_name } => {
            // Never clobber an explicit Blocked -- it's meant to survive
            // until a human addresses it, and overwriting it here would also
            // make the next Stop wrongly clear it (the new note would carry
            // the hook marker).
            if is_explicit_blocked(&session) {
                return Ok(None);
            }
            (ActivityState::Blocked, Some(format!("{HOOK_AWAITING_MARKER}input: {tool_name}")))
        }
        ActivityEvent::HookAwaitingPermission { tool_name, tool_input } => {
            if is_explicit_blocked(&session) {
                return Ok(None);
            }
            (ActivityState::Blocked, Some(format_permission_note(&tool_name, &tool_input)))
        }
    };
    if session.activity == state && session.activity_note == note {
        return Ok(None);
    }
    if session.activity != state {
        session.activity_since = Some(now);
    }
    session.activity = state;
    session.activity_note = note;
    // Column-targeted write, never a full-row upsert: this runs in a
    // short-lived process concurrent with the poller/GUI, and writing the
    // whole (possibly stale) row back could revert their fresher fields or
    // resurrect a just-deleted session. `false` = the row vanished or went
    // terminal between the read above and this write — drop the update.
    if !store.update_session_activity(
        &session.id, session.activity, session.activity_note.as_deref(), session.activity_since,
    )? {
        return Ok(None);
    }
    Ok(Some(session))
}

/// Identify which session a `worker-status` invocation belongs to.
///
/// Order: the `NINOX_SESSION` env value when it names a live row (the spawn
/// plumbing re-exports it on every respawn), else the deepest ancestor of
/// `cwd` matching a non-terminal session's `workspace_path` — the same
/// durable correlation `ninox statusline` uses, so a worker relaunched by
/// hand inside its worktree still resolves.
pub fn resolve_session_id(
    store: &Store,
    env_session: Option<&str>,
    cwd: Option<&Path>,
) -> Result<Option<SessionId>> {
    if let Some(id) = env_session {
        // The env var is baked into the tmux session, so it can outlive its
        // row's liveness — a terminal row must not resolve, or callers would
        // "successfully" report activity nobody can see.
        if store.get_session(id)?.is_some_and(|s| !s.status.is_terminal()) {
            return Ok(Some(id.to_string()));
        }
    }
    let Some(cwd) = cwd else { return Ok(None) };
    let sessions = store.list_sessions()?;
    for ancestor in cwd.ancestors() {
        let ancestor = ancestor.to_string_lossy();
        // `list_sessions` orders by started_at DESC, so the first hit is the
        // newest incarnation of a reused workspace.
        let hit = sessions.iter()
            .filter(|s| !s.status.is_terminal())
            .find(|s| s.workspace_path.as_deref() == Some(ancestor.as_ref()));
        if let Some(s) = hit {
            return Ok(Some(s.id.clone()));
        }
    }
    Ok(None)
}

/// The substring identifying ninox's own Stop-hook command — see
/// `messaging::STOP_HOOK_MARKER` for why the probe matches our specific
/// command rather than "some Stop hook exists".
const STATUS_HOOK_MARKER: &str = "worker-status hook-stop";

/// Whether `workspace`'s `.claude/settings.json` carries ninox's
/// worker-status activity hooks. False means the session *can't report*
/// (worktree predates the feature, checked-in settings.json, non-Claude
/// harness) — the UI renders that distinctly from "idle". Missing file or
/// unparsable JSON degrade to false; this is a best-effort capability
/// probe, mirroring `messaging`'s inbox probe.
pub fn worker_status_hooks_installed(workspace: &Path) -> bool {
    let settings_path = workspace.join(".claude").join("settings.json");
    let Ok(raw) = std::fs::read_to_string(&settings_path) else {
        return false;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return false;
    };
    let Some(stop_groups) = json.get("hooks").and_then(|h| h.get("Stop")).and_then(|s| s.as_array()) else {
        return false;
    };
    stop_groups.iter().any(|group| {
        group.get("hooks").and_then(|h| h.as_array()).is_some_and(|hooks| {
            hooks.iter().any(|hook| {
                hook.get("command")
                    .and_then(|c| c.as_str())
                    .is_some_and(|c| c.contains(STATUS_HOOK_MARKER))
            })
        })
    })
}

/// Extract `cwd` from a Claude Code hook stdin payload. Malformed or
/// unexpected JSON yields `None` — hook handlers must never fail.
pub fn hook_payload_cwd(payload: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(payload)
        .ok()?
        .pointer("/cwd")?
        .as_str()
        .map(String::from)
}

/// Extract `tool_name` from a `PreToolUse`/`PermissionRequest` hook stdin
/// payload. Malformed or unexpected JSON yields `None` — hook handlers must
/// never fail.
pub fn hook_payload_tool_name(payload: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(payload)
        .ok()?
        .pointer("/tool_name")?
        .as_str()
        .map(String::from)
}

/// Extract `tool_input` from a `PreToolUse`/`PermissionRequest` hook stdin
/// payload. Malformed or unexpected JSON yields `None` — hook handlers must
/// never fail.
pub fn hook_payload_tool_input(payload: &str) -> Option<serde_json::Value> {
    serde_json::from_str::<serde_json::Value>(payload)
        .ok()?
        .pointer("/tool_input")
        .cloned()
}

/// Build a Claude Code hook-entry JSON object invoking `ninox worker-status
/// <cmd>` — one of the `hook-prompt`/`hook-stop`/`hook-needs-input`/
/// `hook-permission` verbs above. `ninox_bin_quoted` is the ninox binary
/// path, already single-quoted by the caller (the default macOS config dir
/// contains a space, so an unquoted path would be split into two argv
/// tokens by the shell that ultimately runs this command). `timeout` is in
/// SECONDS, not milliseconds, per Claude Code's hook contract — 5 gives
/// these fast, local-filesystem-only handlers a generous margin without
/// risking an accidental multi-minute hang on a hook error.
///
/// Shared by the two places that seed these hooks — worker worktrees
/// (`spawn_util::ensure_statusline_settings`) and orchestrator roots
/// (`orchestrator_root::setup_orchestrator_root`) — so the command shape
/// can't drift between the two install sites.
pub fn hook_entry(ninox_bin_quoted: &str, cmd: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "command",
        "command": format!("{ninox_bin_quoted} {cmd}"),
        "timeout": 5
    })
}

/// Tool names that can actually prompt for permission. Left unmatched, a
/// `PermissionRequest` hook entry fires on *every* tool call (Read, Grep,
/// TodoWrite, …), not just ones that could ever need a human's okay — a
/// subprocess-plus-sqlite round trip per call, fleet-wide. `AskUserQuestion`/
/// `ExitPlanMode` are deliberately excluded: those are covered by the
/// `PreToolUse` `hook-needs-input` entry, not by `PermissionRequest`.
///
/// Shared by the same two install sites as [`hook_entry`], for the same
/// reason: the worker and orchestrator `PermissionRequest` entries must stay
/// scoped identically, not drift into one being broader than the other.
pub const PERMISSION_REQUEST_MATCHER: &str = "Bash|Write|Edit|MultiEdit|NotebookEdit|WebFetch";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SessionStatus;
    use tempfile::tempdir;

    fn test_store() -> Store {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.db");
        std::mem::forget(dir);
        Store::open(path).unwrap()
    }

    fn seed(store: &Store, id: &str, workspace: Option<&str>, status: SessionStatus, started_at: i64) {
        store.upsert_session(&Session {
            id: id.into(), orchestrator_id: None, name: id.into(),
            repo: "o/r".into(), status,
            agent_type: "claude-code".into(), cost_usd: 0.0, started_at,
            pr_number: None, pr_id: None,
            workspace_path: workspace.map(String::from), pid: None,
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None,
            gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
        }).unwrap();
    }

    fn activity_of(store: &Store, id: &str) -> (ActivityState, Option<String>, Option<i64>) {
        let s = store.get_session(id).unwrap().unwrap();
        (s.activity, s.activity_note, s.activity_since)
    }

    #[test]
    fn hook_prompt_sets_working_from_any_non_working_state() {
        let store = test_store();
        seed(&store, "s1", None, SessionStatus::Working, 0);
        apply_activity(&store, "s1", ActivityEvent::Explicit {
            state: ActivityState::Blocked, note: Some("stuck".into()),
        }, 100).unwrap();

        let updated = apply_activity(&store, "s1", ActivityEvent::HookPrompt, 200).unwrap();
        assert!(updated.is_some(), "prompt must supersede blocked");
        assert_eq!(activity_of(&store, "s1"), (ActivityState::Working, None, Some(200)));
    }

    #[test]
    fn hook_prompt_is_a_noop_when_already_working_preserving_note_and_since() {
        let store = test_store();
        seed(&store, "s1", None, SessionStatus::Working, 0);
        apply_activity(&store, "s1", ActivityEvent::Explicit {
            state: ActivityState::Working, note: Some("implementing X".into()),
        }, 100).unwrap();

        let updated = apply_activity(&store, "s1", ActivityEvent::HookPrompt, 200).unwrap();
        assert!(updated.is_none(), "already-working prompt must be a no-op");
        assert_eq!(
            activity_of(&store, "s1"),
            (ActivityState::Working, Some("implementing X".into()), Some(100)),
        );
    }

    #[test]
    fn hook_stop_downgrades_working_and_unknown_to_idle() {
        let store = test_store();
        seed(&store, "s1", None, SessionStatus::Working, 0);
        // From Unknown (never reported): Stop is still a real signal.
        apply_activity(&store, "s1", ActivityEvent::HookStop { turn_continues: false }, 100).unwrap();
        assert_eq!(activity_of(&store, "s1").0, ActivityState::Idle);

        apply_activity(&store, "s1", ActivityEvent::HookPrompt, 200).unwrap();
        apply_activity(&store, "s1", ActivityEvent::HookStop { turn_continues: false }, 300).unwrap();
        assert_eq!(activity_of(&store, "s1"), (ActivityState::Idle, None, Some(300)));
    }

    #[test]
    fn hook_stop_never_overwrites_an_explicit_blocked() {
        let store = test_store();
        seed(&store, "s1", None, SessionStatus::Working, 0);
        apply_activity(&store, "s1", ActivityEvent::Explicit {
            state: ActivityState::Blocked, note: Some("waiting on review of #12".into()),
        }, 100).unwrap();

        let updated = apply_activity(&store, "s1", ActivityEvent::HookStop { turn_continues: false }, 200).unwrap();
        assert!(updated.is_none(), "stop must not clear a declared blocked state");
        assert_eq!(
            activity_of(&store, "s1"),
            (ActivityState::Blocked, Some("waiting on review of #12".into()), Some(100)),
        );
    }

    #[test]
    fn hook_stop_on_a_continuing_turn_marks_working_not_idle() {
        // An inbox drain-stop that blocks the Stop keeps the agent working on
        // the injected instructions — the sibling status hook must record
        // that as Working, not spend the whole continuation shown as idle.
        let store = test_store();
        seed(&store, "s1", None, SessionStatus::Working, 0);
        apply_activity(&store, "s1", ActivityEvent::Explicit {
            state: ActivityState::Blocked, note: Some("stuck".into()),
        }, 100).unwrap();

        apply_activity(&store, "s1", ActivityEvent::HookStop { turn_continues: true }, 200).unwrap();
        assert_eq!(
            activity_of(&store, "s1"),
            (ActivityState::Working, None, Some(200)),
            "a continuing turn supersedes even Blocked, like a fresh prompt does",
        );
    }

    #[test]
    fn explicit_set_with_same_state_updates_note_but_keeps_since() {
        let store = test_store();
        seed(&store, "s1", None, SessionStatus::Working, 0);
        apply_activity(&store, "s1", ActivityEvent::Explicit {
            state: ActivityState::Blocked, note: Some("first".into()),
        }, 100).unwrap();
        apply_activity(&store, "s1", ActivityEvent::Explicit {
            state: ActivityState::Blocked, note: Some("second".into()),
        }, 200).unwrap();
        assert_eq!(
            activity_of(&store, "s1"),
            (ActivityState::Blocked, Some("second".into()), Some(100)),
        );
    }

    #[test]
    fn apply_activity_skips_missing_and_terminal_sessions() {
        let store = test_store();
        assert!(apply_activity(&store, "ghost", ActivityEvent::HookPrompt, 100).unwrap().is_none());

        seed(&store, "dead", None, SessionStatus::Terminated, 0);
        let updated = apply_activity(&store, "dead", ActivityEvent::HookPrompt, 100).unwrap();
        assert!(updated.is_none(), "a stale hook firing during teardown must not touch the row");
        assert_eq!(activity_of(&store, "dead").0, ActivityState::Unknown);
    }

    #[test]
    fn resolve_prefers_env_session_when_the_row_exists() {
        let store = test_store();
        seed(&store, "s1", Some("/ws/one"), SessionStatus::Working, 0);
        let id = resolve_session_id(&store, Some("s1"), Some(Path::new("/somewhere/else"))).unwrap();
        assert_eq!(id.as_deref(), Some("s1"));
    }

    #[test]
    fn resolve_falls_back_to_workspace_ancestor_when_env_row_is_gone() {
        let store = test_store();
        seed(&store, "s1", Some("/ws/one"), SessionStatus::Working, 0);
        let id = resolve_session_id(
            &store, Some("purged-id"), Some(Path::new("/ws/one/src/deep")),
        ).unwrap();
        assert_eq!(id.as_deref(), Some("s1"), "cwd inside the worktree must resolve");
    }

    #[test]
    fn resolve_rejects_a_terminal_env_row_rather_than_reporting_into_it() {
        // NINOX_SESSION can outlive its row's liveness (the env var is baked
        // into the tmux session). A terminal row must not resolve — callers
        // would otherwise "successfully" write activity nobody can see.
        let store = test_store();
        seed(&store, "dead", Some("/ws/dead"), SessionStatus::Terminated, 0);
        assert_eq!(
            resolve_session_id(&store, Some("dead"), None).unwrap(),
            None,
        );
        // …but the cwd fallback still applies when it names a live session.
        seed(&store, "live", Some("/ws/live"), SessionStatus::Working, 10);
        let id = resolve_session_id(&store, Some("dead"), Some(Path::new("/ws/live"))).unwrap();
        assert_eq!(id.as_deref(), Some("live"));
    }

    #[test]
    fn resolve_ignores_terminal_sessions_and_prefers_newest_on_reused_workspace() {
        let store = test_store();
        seed(&store, "old", Some("/ws/one"), SessionStatus::Terminated, 0);
        seed(&store, "new", Some("/ws/one"), SessionStatus::Working, 500);
        let id = resolve_session_id(&store, None, Some(Path::new("/ws/one"))).unwrap();
        assert_eq!(id.as_deref(), Some("new"));
    }

    #[test]
    fn resolve_returns_none_when_nothing_matches() {
        let store = test_store();
        seed(&store, "s1", Some("/ws/one"), SessionStatus::Working, 0);
        assert_eq!(resolve_session_id(&store, None, Some(Path::new("/elsewhere"))).unwrap(), None);
        assert_eq!(resolve_session_id(&store, None, None).unwrap(), None);
    }

    #[test]
    fn hooks_installed_probe_requires_our_specific_command() {
        let dir = tempdir().unwrap();
        let claude = dir.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        let settings = claude.join("settings.json");

        // No settings file at all.
        assert!(!worker_status_hooks_installed(dir.path()));

        // A Stop hook exists but it isn't ours (e.g. a checked-in
        // lint-on-stop) — must not count.
        std::fs::write(&settings, r#"{"hooks":{"Stop":[{"hooks":[
            {"type":"command","command":"lint --on-stop"}]}]}}"#).unwrap();
        assert!(!worker_status_hooks_installed(dir.path()));

        // Unparsable JSON degrades to false, never errors.
        std::fs::write(&settings, "{nope").unwrap();
        assert!(!worker_status_hooks_installed(dir.path()));

        // Our command present (alongside an inbox sibling) — counts.
        std::fs::write(&settings, r#"{"hooks":{"Stop":[{"hooks":[
            {"type":"command","command":"'/path/ninox' inbox drain-stop"},
            {"type":"command","command":"'/path/ninox' worker-status hook-stop"}]}]}}"#).unwrap();
        assert!(worker_status_hooks_installed(dir.path()));
    }

    #[test]
    fn hook_payload_cwd_is_tolerant_of_malformed_input() {
        assert_eq!(
            hook_payload_cwd(r#"{"session_id":"abc","cwd":"/ws/one"}"#),
            Some("/ws/one".into()),
        );
        assert_eq!(hook_payload_cwd("not json at all"), None);
        assert_eq!(hook_payload_cwd(r#"{"cwd": 42}"#), None);
        assert_eq!(hook_payload_cwd(""), None);
    }

    #[test]
    fn hook_awaiting_input_marks_blocked_with_the_tool_name() {
        let store = test_store();
        seed(&store, "s1", None, SessionStatus::Working, 0);
        apply_activity(&store, "s1", ActivityEvent::HookPrompt, 100).unwrap();

        let updated = apply_activity(
            &store, "s1",
            ActivityEvent::HookAwaitingInput { tool_name: "AskUserQuestion".into() },
            200,
        ).unwrap();
        assert!(updated.is_some());
        let (state, note, since) = activity_of(&store, "s1");
        assert_eq!(state, ActivityState::Blocked);
        assert_eq!(note.as_deref(), Some("[awaiting] input: AskUserQuestion"));
        assert_eq!(since, Some(200));
    }

    #[test]
    fn hook_awaiting_permission_describes_the_tool_call() {
        let store = test_store();
        seed(&store, "s1", None, SessionStatus::Working, 0);

        let updated = apply_activity(
            &store, "s1",
            ActivityEvent::HookAwaitingPermission {
                tool_name: "Bash".into(),
                tool_input: serde_json::json!({"command": "rm -rf node_modules"}),
            },
            100,
        ).unwrap();
        assert!(updated.is_some());
        let (state, note, _) = activity_of(&store, "s1");
        assert_eq!(state, ActivityState::Blocked);
        assert_eq!(
            note.as_deref(),
            Some("[awaiting] permission: Bash `rm -rf node_modules`"),
        );
    }

    #[test]
    fn hook_awaiting_permission_falls_back_to_raw_input_without_a_known_field() {
        let store = test_store();
        seed(&store, "s1", None, SessionStatus::Working, 0);

        apply_activity(
            &store, "s1",
            ActivityEvent::HookAwaitingPermission {
                tool_name: "WebFetch".into(),
                tool_input: serde_json::json!({"url": "https://example.com"}),
            },
            100,
        ).unwrap();
        let (_, note, _) = activity_of(&store, "s1");
        assert_eq!(
            note.as_deref(),
            Some(r#"[awaiting] permission: WebFetch `{"url":"https://example.com"}`"#),
        );
    }

    #[test]
    fn hook_awaiting_permission_truncates_a_long_command() {
        let store = test_store();
        seed(&store, "s1", None, SessionStatus::Working, 0);
        let long_command = "x".repeat(200);

        apply_activity(
            &store, "s1",
            ActivityEvent::HookAwaitingPermission {
                tool_name: "Bash".into(),
                tool_input: serde_json::json!({"command": long_command}),
            },
            100,
        ).unwrap();
        let (_, note, _) = activity_of(&store, "s1");
        let note = note.unwrap();
        assert!(
            note.chars().count() < 200,
            "a long command field must be truncated like the JSON-fallback branch is: {note}",
        );
        assert!(note.ends_with('…') || note.ends_with("…`"), "note: {note}");
    }

    #[test]
    fn hook_awaiting_permission_sanitizes_backticks_and_newlines_in_the_detail() {
        let store = test_store();
        seed(&store, "s1", None, SessionStatus::Working, 0);

        apply_activity(
            &store, "s1",
            ActivityEvent::HookAwaitingPermission {
                tool_name: "Bash".into(),
                tool_input: serde_json::json!({"command": "echo `whoami`\nls"}),
            },
            100,
        ).unwrap();
        let (_, note, _) = activity_of(&store, "s1");
        assert_eq!(
            note.as_deref(),
            Some("[awaiting] permission: Bash `echo 'whoami' ls`"),
            "a backtick or newline in the command must not break the note's own backtick-quoting",
        );
    }

    #[test]
    fn hook_prompt_supersedes_an_awaiting_input_state() {
        // "the next UserPromptSubmit ... should supersede it like any other
        // state" — no special-casing needed, HookPrompt already overwrites
        // anything but Working.
        let store = test_store();
        seed(&store, "s1", None, SessionStatus::Working, 0);
        apply_activity(&store, "s1", ActivityEvent::HookAwaitingInput {
            tool_name: "AskUserQuestion".into(),
        }, 100).unwrap();

        apply_activity(&store, "s1", ActivityEvent::HookPrompt, 200).unwrap();
        assert_eq!(activity_of(&store, "s1"), (ActivityState::Working, None, Some(200)));
    }

    #[test]
    fn hook_stop_clears_a_hook_driven_awaiting_state() {
        // Unlike an explicit Blocked, a hook-driven "awaiting" state must
        // clear on the very next Stop: Claude Code cannot fire Stop while a
        // synchronous interactive tool call (AskUserQuestion/ExitPlanMode,
        // or a permission prompt) is still outstanding, so Stop firing at
        // all is proof the prompt already resolved.
        let store = test_store();
        seed(&store, "s1", None, SessionStatus::Working, 0);
        apply_activity(&store, "s1", ActivityEvent::HookAwaitingPermission {
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command": "rm -rf node_modules"}),
        }, 100).unwrap();

        let updated = apply_activity(&store, "s1", ActivityEvent::HookStop { turn_continues: false }, 200).unwrap();
        assert!(updated.is_some(), "stop must clear a resolved hook-driven awaiting state");
        assert_eq!(activity_of(&store, "s1"), (ActivityState::Idle, None, Some(200)));
    }

    #[test]
    fn hook_stop_still_preserves_an_explicit_blocked_alongside_awaiting_support() {
        // Regression guard: adding the awaiting-state clear path must not
        // weaken the original explicit-Blocked guarantee.
        let store = test_store();
        seed(&store, "s1", None, SessionStatus::Working, 0);
        apply_activity(&store, "s1", ActivityEvent::Explicit {
            state: ActivityState::Blocked, note: Some("waiting on review of #12".into()),
        }, 100).unwrap();

        let updated = apply_activity(&store, "s1", ActivityEvent::HookStop { turn_continues: false }, 200).unwrap();
        assert!(updated.is_none(), "stop must not clear a declared blocked state");
        assert_eq!(
            activity_of(&store, "s1"),
            (ActivityState::Blocked, Some("waiting on review of #12".into()), Some(100)),
        );
    }

    #[test]
    fn an_awaiting_input_event_does_not_clobber_an_explicit_blocked() {
        // An operator's explicit Blocked must survive to Stop unchanged (see
        // the test above) -- which only holds if a hook-driven awaiting
        // event arriving *first* doesn't overwrite it with a marked note
        // that the next Stop would then (correctly, for a hook-driven note)
        // clear. Without this guard the explicit declaration is lost the
        // moment any interactive tool call happens mid-turn.
        let store = test_store();
        seed(&store, "s1", None, SessionStatus::Working, 0);
        apply_activity(&store, "s1", ActivityEvent::Explicit {
            state: ActivityState::Blocked, note: Some("waiting on review of #12".into()),
        }, 100).unwrap();

        let updated = apply_activity(&store, "s1", ActivityEvent::HookAwaitingInput {
            tool_name: "AskUserQuestion".into(),
        }, 200).unwrap();
        assert!(updated.is_none(), "an awaiting-input signal must not overwrite an explicit blocked");
        assert_eq!(
            activity_of(&store, "s1"),
            (ActivityState::Blocked, Some("waiting on review of #12".into()), Some(100)),
        );

        // And it still survives the following Stop, same as if the
        // awaiting-input event had never fired.
        let updated = apply_activity(&store, "s1", ActivityEvent::HookStop { turn_continues: false }, 300).unwrap();
        assert!(updated.is_none());
        assert_eq!(
            activity_of(&store, "s1"),
            (ActivityState::Blocked, Some("waiting on review of #12".into()), Some(100)),
        );
    }

    #[test]
    fn an_awaiting_permission_event_does_not_clobber_an_explicit_blocked() {
        let store = test_store();
        seed(&store, "s1", None, SessionStatus::Working, 0);
        apply_activity(&store, "s1", ActivityEvent::Explicit {
            state: ActivityState::Blocked, note: Some("waiting on review of #12".into()),
        }, 100).unwrap();

        let updated = apply_activity(&store, "s1", ActivityEvent::HookAwaitingPermission {
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command": "rm -rf node_modules"}),
        }, 200).unwrap();
        assert!(updated.is_none(), "an awaiting-permission signal must not overwrite an explicit blocked");
        assert_eq!(
            activity_of(&store, "s1"),
            (ActivityState::Blocked, Some("waiting on review of #12".into()), Some(100)),
        );
    }

    #[test]
    fn hook_payload_tool_name_is_tolerant_of_malformed_input() {
        assert_eq!(
            hook_payload_tool_name(r#"{"tool_name":"Bash","tool_input":{}}"#),
            Some("Bash".into()),
        );
        assert_eq!(hook_payload_tool_name("not json"), None);
        assert_eq!(hook_payload_tool_name(r#"{"tool_name": 42}"#), None);
        assert_eq!(hook_payload_tool_name(""), None);
    }

    #[test]
    fn hook_payload_tool_input_extracts_the_object() {
        assert_eq!(
            hook_payload_tool_input(r#"{"tool_name":"Bash","tool_input":{"command":"ls"}}"#),
            Some(serde_json::json!({"command": "ls"})),
        );
        assert_eq!(hook_payload_tool_input("not json"), None);
        assert_eq!(hook_payload_tool_input(r#"{"tool_name":"Bash"}"#), None);
    }
}
