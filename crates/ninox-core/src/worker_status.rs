//! Worker activity reporting: the single write path behind
//! `ninox worker-status` (explicit `set` and the Claude Code
//! `UserPromptSubmit`/`Stop` hook verbs). All transition rules live in
//! [`apply_activity`] — the CLI entry points in `main.rs` are dumb pipes.
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
/// - `HookStop` sets `Idle` from `Working`/`Unknown` but never overwrites
///   `Blocked` — an agent that declared itself blocked mid-turn stays
///   blocked past turn end, which is the whole point of declaring it.
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
            if session.activity == ActivityState::Blocked
                || session.activity == ActivityState::Idle
            {
                return Ok(None);
            }
            (ActivityState::Idle, None)
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
}
