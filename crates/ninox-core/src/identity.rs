//! Session self-identification (`ninox whoami`) — a live projection of "what
//! am I", rebuilt fresh from the store on every call, never cached. See
//! `docs/superpowers/specs/2026-10-05-session-self-identification-design.md`.
//!
//! Caller resolution mostly inherits worker-grade trust (spec §4):
//! [`worker_status::resolve_session_id`]'s env-var-then-cwd-ancestor match is
//! enough to tell a session its own identity, since misreporting a worker's
//! *own* identity back to itself discloses nothing a worker doesn't already
//! know. The one case that needs the stronger check
//! `orchestrator_auth::authorize_orchestrator` uses for other privileged
//! reads is disclosing an *orchestrator's* identity: every worker already
//! carries its parent's id in `NINOX_ORCHESTRATOR_ID`, so without this, a
//! worker could set `NINOX_SESSION=$NINOX_ORCHESTRATOR_ID` and read its
//! orchestrator's full identity (workspace, branch, PR, model) merely by
//! claiming its id — see [`resolve_verified`]. An unmanaged or terminated
//! session gets an explicit [`Resolution`] variant, never a guessed identity
//! or silence (spec §5) — including when the env var names a session we
//! positively know is terminated: [`resolve_for_caller`] checks that
//! directly, rather than letting a cwd-ancestor match on a *different*, live
//! session that happens to share the terminated one's `workspace_path` paper
//! over it.

use crate::capabilities::{self, Audience};
use crate::config::AppConfig;
use crate::orchestrator_auth;
use crate::store::Store;
use crate::types::Session;
use crate::worker_status;
use anyhow::Result;
use std::path::Path;

/// A session's live role descriptor, assembled fresh from the store.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionIdentity {
    pub session_id:     String,
    pub is_orchestrator: bool,
    /// The orchestrator that owns this session (its `orchestrator_id`), or,
    /// for an orchestrator itself, its own id. `None` for a standalone
    /// worker spawned without an orchestrator.
    pub owner:          Option<String>,
    pub workspace_path: Option<String>,
    pub branch:         Option<String>,
    pub pr_number:      Option<u64>,
    pub agent_type:     String,
    pub model:          Option<String>,
    /// Enabled capability names for this session's audience — doubles as
    /// "usable commands" (spec §1: registry entries serve both purposes).
    pub capabilities:   Vec<&'static str>,
}

/// The result of resolving a caller's identity: a live, managed session, or
/// one of the two explicit "nothing to disclose" states — never silence.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolution {
    Identity(SessionIdentity),
    /// No row at all for the resolved (or attempted) id.
    Unmanaged { session_id: Option<String> },
    /// A row exists but its status is terminal.
    Terminated { session_id: String, at: Option<i64> },
}

/// Shared budget for the compact hook-reminder text — deliberately smaller
/// than `inbox::MAX_HOOK_PAYLOAD_CHARS` (10,000): the reminder rides the same
/// `UserPromptSubmit` response as inbox messages and must stay terse by
/// default (spec Risks table).
pub const MAX_IDENTITY_REMINDER_CHARS: usize = 2_000;

/// Resolve the calling process's own identity.
///
/// `caller_type` is the caller's own `NINOX_CALLER_TYPE` — only consulted
/// (and only when it reads `"orchestrator"`) to decide whether an
/// orchestrator target needs the live pane-identity cross-check below; see
/// [`resolve_verified`].
///
/// The env var is checked directly, once, before any cwd fallback: a session
/// it names that is confirmed *terminated* is reported as `Terminated`
/// immediately, never superseded by a cwd-ancestor match landing on a
/// different, live session that happens to share the terminated one's
/// `workspace_path` (a stray process from the old session, still sitting in
/// a now-reused worktree directory). Only when the env var names nothing at
/// all does resolution fall back to [`worker_status::resolve_session_by_cwd`]
/// — the same durable cwd correlation `resolve_session_id` uses, so a worker
/// relaunched by hand inside its worktree still resolves. This also means
/// the env-var row is fetched at most once here (never re-fetched by
/// [`resolve`]), which matters since this fires on every turn when
/// `[role_rehydration]` is enabled.
pub async fn resolve_for_caller(
    store: &Store,
    config: &AppConfig,
    env_session: Option<&str>,
    cwd: Option<&Path>,
    caller_type: Option<&str>,
) -> Result<Resolution> {
    let claimed = match env_session {
        Some(id) => store.get_session(id)?,
        None => None,
    };
    if let Some(session) = &claimed {
        if session.status.is_terminal() {
            return Ok(Resolution::Terminated {
                session_id: session.id.clone(),
                at: session.terminal_at,
            });
        }
    }
    let resolved = match claimed {
        Some(session) => Some(session),
        None => worker_status::resolve_session_by_cwd(store, cwd)?,
    };
    match resolved {
        Some(session) => resolve_verified(store, config, session, caller_type).await,
        None => Ok(Resolution::Unmanaged { session_id: env_session.map(str::to_string) }),
    }
}

/// Discloses `session`'s identity, except when it names an *orchestrator*
/// whose ownership the caller can't verify — the fix for the spoofing gap
/// described on this module's docs: a worker claiming
/// `NINOX_SESSION=$NINOX_ORCHESTRATOR_ID` must not be able to read its
/// orchestrator's identity merely because the row exists and is live.
///
/// Verification reuses [`orchestrator_auth::authorize_orchestrator`] — the
/// same pane-bound, process-ancestry-confirmed check other privileged
/// orchestrator reads require — rather than a second, weaker copy of it.
/// `caller_type` is checked first (cheap: no tmux call) since a worker's env
/// never carries `NINOX_CALLER_TYPE=orchestrator` (see `main::worker_env_vars`
/// vs `main::orchestrator_env_vars`), so the overwhelmingly common
/// "worker/no-claim" case never pays for a live pane query; only a caller
/// that actually claims to be an orchestrator does. An orchestrator whose
/// ownership can't be verified this way resolves as `Unmanaged` — the same
/// "nothing to disclose" response as an unknown id, never a narrowed-down
/// identity (spec §5).
async fn resolve_verified(
    store: &Store,
    config: &AppConfig,
    session: Session,
    caller_type: Option<&str>,
) -> Result<Resolution> {
    if store.is_orchestrator(&session.id)? {
        let runtime = if caller_type == Some("orchestrator") {
            crate::runtime::current_private_pane_identity().await.ok().flatten()
        } else {
            None
        };
        if orchestrator_auth::authorize_orchestrator(store, Some(&session.id), caller_type, runtime.as_ref())
            .is_err()
        {
            return Ok(Resolution::Unmanaged { session_id: Some(session.id) });
        }
    }
    identity_for_session(store, config, session)
}

/// Resolve a known session id's identity directly — no caller verification,
/// so only for trusted/internal lookups. [`resolve_for_caller`] is the
/// caller-facing, spoof-resistant entry point.
pub fn resolve(store: &Store, config: &AppConfig, session_id: &str) -> Result<Resolution> {
    let Some(session) = store.get_session(session_id)? else {
        return Ok(Resolution::Unmanaged { session_id: Some(session_id.to_string()) });
    };
    identity_for_session(store, config, session)
}

fn identity_for_session(store: &Store, config: &AppConfig, session: Session) -> Result<Resolution> {
    if session.status.is_terminal() {
        return Ok(Resolution::Terminated {
            session_id: session.id,
            at: session.terminal_at,
        });
    }

    let is_orchestrator = store.is_orchestrator(&session.id)?;
    let audience = if is_orchestrator { Audience::Orchestrator } else { Audience::Worker };
    let owner = if is_orchestrator {
        Some(session.id.clone())
    } else {
        session.orchestrator_id.clone()
    };
    let branch = store.fleet_record(&session.id)?.and_then(|r| r.branch);
    let capabilities: Vec<&'static str> = capabilities::enabled_for(audience, config)
        .map(|c| c.name)
        .collect();

    Ok(Resolution::Identity(SessionIdentity {
        session_id: session.id,
        is_orchestrator,
        owner,
        workspace_path: session.workspace_path,
        branch,
        pr_number: session.pr_number,
        agent_type: session.agent_type,
        model: session.model,
        capabilities,
    }))
}

/// Human-readable rendering for direct `ninox whoami` use.
pub fn render_text(resolution: &Resolution) -> String {
    match resolution {
        Resolution::Identity(id) => {
            let kind = if id.is_orchestrator { "orchestrator" } else { "worker" };
            let mut lines = vec![
                format!("session:      {}", id.session_id),
                format!("kind:         {kind}"),
                format!("owner:        {}", id.owner.as_deref().unwrap_or("(none)")),
                format!("workspace:    {}", id.workspace_path.as_deref().unwrap_or("(none)")),
                format!("branch:       {}", id.branch.as_deref().unwrap_or("(none)")),
                format!("pr:           {}", id.pr_number.map(|n| n.to_string()).unwrap_or_else(|| "(none)".to_string())),
                format!("agent:        {}", id.agent_type),
                format!("model:        {}", id.model.as_deref().unwrap_or("(none)")),
            ];
            lines.push(format!("capabilities: {}", id.capabilities.join(", ")));
            lines.join("\n")
        }
        Resolution::Unmanaged { session_id: Some(id) } =>
            format!("ninox is not managing this session (`{id}`)."),
        Resolution::Unmanaged { session_id: None } =>
            "ninox is not managing this session (no resolvable session id — \
             NINOX_SESSION is unset and the current directory matches no known \
             workspace).".to_string(),
        Resolution::Terminated { session_id, at: Some(at) } =>
            format!("this session (`{session_id}`) was terminated at {at} (unix ms)."),
        Resolution::Terminated { session_id, at: None } =>
            format!("this session (`{session_id}`) was terminated."),
    }
}

/// Machine-readable rendering for `ninox whoami --json`.
pub fn render_json(resolution: &Resolution) -> serde_json::Value {
    match resolution {
        Resolution::Identity(id) => serde_json::json!({
            "managed":      true,
            "sessionId":    id.session_id,
            "kind":         if id.is_orchestrator { "orchestrator" } else { "worker" },
            "owner":        id.owner,
            "workspacePath": id.workspace_path,
            "branch":       id.branch,
            "prNumber":     id.pr_number,
            "agentType":    id.agent_type,
            "model":        id.model,
            "capabilities": id.capabilities,
        }),
        Resolution::Unmanaged { session_id } => serde_json::json!({
            "managed":   false,
            "reason":    "unmanaged",
            "sessionId": session_id,
        }),
        Resolution::Terminated { session_id, at } => serde_json::json!({
            "managed":       false,
            "reason":        "terminated",
            "sessionId":     session_id,
            "terminatedAt":  at,
        }),
    }
}

/// Compact single-line reminder for the `UserPromptSubmit`/`SessionStart`
/// hook payloads, truncated to [`MAX_IDENTITY_REMINDER_CHARS`]. Unlike the
/// inbox drain hooks (silent when nothing is pending), this always has
/// something honest to say — including for `Unmanaged`/`Terminated` — so it
/// never returns an empty string (spec §5).
pub fn render_reminder(resolution: &Resolution) -> String {
    let text = match resolution {
        Resolution::Identity(id) => {
            let kind = if id.is_orchestrator { "orchestrator" } else { "worker" };
            format!(
                "[ninox] You are session `{}` ({kind}), owned by `{}`, workspace `{}`, branch `{}`, \
                 PR {}, capabilities: {}.",
                id.session_id,
                id.owner.as_deref().unwrap_or("(none)"),
                id.workspace_path.as_deref().unwrap_or("(none)"),
                id.branch.as_deref().unwrap_or("(none)"),
                id.pr_number.map(|n| format!("#{n}")).unwrap_or_else(|| "(none)".to_string()),
                id.capabilities.join(", "),
            )
        }
        other => format!("[ninox] {}", render_text(other)),
    };
    crate::util::truncate_chars(&text, MAX_IDENTITY_REMINDER_CHARS)
}

/// Whether the identity-hook's `UserPromptSubmit` sibling entry and
/// `SessionStart` event should be installed — the `whoami` capability's own
/// registry gate (`[role_rehydration].enabled`), looked up once here so the
/// two install sites ([`push_prompt_hook`]/[`set_session_start_hook`], used
/// by `ninox_app::spawn_util::ensure_statusline_settings` and
/// `orchestrator_root::setup_orchestrator_root`) share one source of truth
/// instead of each re-reading the config field — or worse, each being handed
/// a hand-threaded bool — directly.
fn hooks_enabled(config: &AppConfig) -> bool {
    capabilities::REGISTRY
        .iter()
        .find(|c| c.name == "whoami")
        .is_some_and(|c| (c.enabled)(config))
}

/// Append the identity-hook's `UserPromptSubmit` sibling entry to
/// `prompt_hooks` when [`hooks_enabled`] — a no-op otherwise. Shared by the
/// worker-worktree and orchestrator-root install sites so the entry can't
/// drift between them. Call before building the `hooks` JSON object that
/// embeds `prompt_hooks`; pair with [`set_session_start_hook`] afterward.
pub fn push_prompt_hook(config: &AppConfig, ninox_bin_quoted: &str, prompt_hooks: &mut Vec<serde_json::Value>) {
    if hooks_enabled(config) {
        prompt_hooks.push(crate::worker_status::hook_entry(ninox_bin_quoted, "identity-hook prompt"));
    }
}

/// Set `hooks["SessionStart"]` to the identity-hook's `SessionStart` event
/// when [`hooks_enabled`] — a no-op otherwise, leaving `SessionStart` unset
/// (both install sites' tests assert on that as the "disabled" state). The
/// sibling of [`push_prompt_hook`]; call after `hooks` is built.
pub fn set_session_start_hook(config: &AppConfig, ninox_bin_quoted: &str, hooks: &mut serde_json::Value) {
    if hooks_enabled(config) {
        hooks["SessionStart"] = serde_json::json!([{
            "hooks": [crate::worker_status::hook_entry(ninox_bin_quoted, "identity-hook session-start")]
        }]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Orchestrator, Session, SessionStatus};
    use tempfile::tempdir;

    fn test_store() -> Store {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.db");
        std::mem::forget(dir);
        Store::open(path).unwrap()
    }

    fn seed_session(store: &Store, id: &str, orchestrator_id: Option<&str>, status: SessionStatus) {
        store.upsert_session(&Session {
            id: id.into(), orchestrator_id: orchestrator_id.map(String::from), name: id.into(),
            repo: "o/r".into(), status,
            agent_type: "claude-code".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None,
            workspace_path: Some(format!("/work/{id}")), pid: None,
            model: Some("claude-sonnet-5-5".into()), context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None,
            gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        }).unwrap();
    }

    #[test]
    fn orchestrator_session_resolves_itself_as_owner() {
        let store = test_store();
        seed_session(&store, "orch-1", None, SessionStatus::Working);
        store.upsert_orchestrator(&Orchestrator { id: "orch-1".into(), name: "orch-1".into(), created_at: 0 }).unwrap();

        let resolution = resolve(&store, &AppConfig::default(), "orch-1").unwrap();
        let Resolution::Identity(id) = resolution else { panic!("expected Identity, got {resolution:?}") };
        assert!(id.is_orchestrator);
        assert_eq!(id.owner.as_deref(), Some("orch-1"));
    }

    #[test]
    fn worker_session_resolves_its_orchestrator_as_owner() {
        let store = test_store();
        seed_session(&store, "worker-1", Some("orch-1"), SessionStatus::Working);

        let resolution = resolve(&store, &AppConfig::default(), "worker-1").unwrap();
        let Resolution::Identity(id) = resolution else { panic!("expected Identity, got {resolution:?}") };
        assert!(!id.is_orchestrator);
        assert_eq!(id.owner.as_deref(), Some("orch-1"));
    }

    #[test]
    fn standalone_worker_with_no_orchestrator_has_no_owner() {
        let store = test_store();
        seed_session(&store, "worker-1", None, SessionStatus::Working);

        let resolution = resolve(&store, &AppConfig::default(), "worker-1").unwrap();
        let Resolution::Identity(id) = resolution else { panic!("expected Identity, got {resolution:?}") };
        assert_eq!(id.owner, None);
    }

    #[test]
    fn missing_row_resolves_unmanaged_with_the_attempted_id() {
        let store = test_store();
        let resolution = resolve(&store, &AppConfig::default(), "ghost").unwrap();
        assert_eq!(resolution, Resolution::Unmanaged { session_id: Some("ghost".into()) });
    }

    #[test]
    fn terminal_row_resolves_terminated_with_its_timestamp() {
        let store = test_store();
        seed_session(&store, "worker-1", Some("orch-1"), SessionStatus::Terminated);
        store.upsert_session(&{
            let mut s = store.get_session("worker-1").unwrap().unwrap();
            s.terminal_at = Some(12345);
            s
        }).unwrap();

        let resolution = resolve(&store, &AppConfig::default(), "worker-1").unwrap();
        assert_eq!(resolution, Resolution::Terminated { session_id: "worker-1".into(), at: Some(12345) });
    }

    /// `worker_status::resolve_session_id` deliberately refuses to resolve a
    /// terminal row via the env var — `resolve_for_caller` must still report
    /// `Terminated`, not fall through to `Unmanaged`, by checking the
    /// env-claimed row directly.
    #[tokio::test]
    async fn resolve_for_caller_reports_terminated_even_though_resolve_session_id_wont() {
        let store = test_store();
        seed_session(&store, "worker-1", Some("orch-1"), SessionStatus::Terminated);

        let resolution = resolve_for_caller(&store, &AppConfig::default(), Some("worker-1"), None, None).await.unwrap();
        assert_eq!(resolution, Resolution::Terminated { session_id: "worker-1".into(), at: None });
    }

    #[tokio::test]
    async fn resolve_for_caller_with_no_env_and_no_cwd_match_is_unmanaged_with_no_id() {
        let store = test_store();
        let resolution = resolve_for_caller(&store, &AppConfig::default(), None, None, None).await.unwrap();
        assert_eq!(resolution, Resolution::Unmanaged { session_id: None });
    }

    #[tokio::test]
    async fn resolve_for_caller_resolves_a_plain_workers_own_identity() {
        // The common, cheap path: no orchestrator involved, so no caller_type
        // is needed and no verification gate applies.
        let store = test_store();
        seed_session(&store, "worker-1", Some("orch-1"), SessionStatus::Working);

        let resolution = resolve_for_caller(&store, &AppConfig::default(), Some("worker-1"), None, None).await.unwrap();
        let Resolution::Identity(id) = resolution else { panic!("expected Identity, got {resolution:?}") };
        assert_eq!(id.session_id, "worker-1");
    }

    /// The exact spoofing gap this module's docs describe: every worker
    /// already carries its parent's id in `NINOX_ORCHESTRATOR_ID`, so a
    /// worker claiming `NINOX_SESSION=$NINOX_ORCHESTRATOR_ID` must not read
    /// its orchestrator's identity merely because the row exists and is
    /// live — a worker's real env never carries
    /// `NINOX_CALLER_TYPE=orchestrator` (see `main::worker_env_vars` vs
    /// `main::orchestrator_env_vars`), so `caller_type: None` here is exactly
    /// what such a spoofing worker's real environment would supply.
    #[tokio::test]
    async fn worker_cannot_read_orchestrator_identity_by_claiming_its_session_id() {
        let store = test_store();
        seed_session(&store, "orch-1", None, SessionStatus::Working);
        store.upsert_orchestrator(&Orchestrator { id: "orch-1".into(), name: "orch-1".into(), created_at: 0 }).unwrap();
        seed_session(&store, "worker-1", Some("orch-1"), SessionStatus::Working);

        let resolution = resolve_for_caller(&store, &AppConfig::default(), Some("orch-1"), None, None).await.unwrap();
        assert_eq!(resolution, Resolution::Unmanaged { session_id: Some("orch-1".into()) });
    }

    /// Even a caller that *does* claim `NINOX_CALLER_TYPE=orchestrator` must
    /// not get the orchestrator's identity without a genuine, verified
    /// runtime behind it (there is none in a bare test process) — the
    /// `caller_type` string alone is not evidence, same as
    /// `orchestrator_auth::authorize_orchestrator` already requires for
    /// every other privileged orchestrator read.
    #[tokio::test]
    async fn claiming_orchestrator_caller_type_alone_is_not_enough() {
        let store = test_store();
        seed_session(&store, "orch-1", None, SessionStatus::Working);
        store.upsert_orchestrator(&Orchestrator { id: "orch-1".into(), name: "orch-1".into(), created_at: 0 }).unwrap();

        let resolution = resolve_for_caller(
            &store, &AppConfig::default(), Some("orch-1"), None, Some("orchestrator"),
        ).await.unwrap();
        assert_eq!(resolution, Resolution::Unmanaged { session_id: Some("orch-1".into()) });
    }

    /// A terminated session's stray process, still sitting in cwd inside a
    /// worktree path that's since been reused by a different, live session,
    /// must report `Terminated` for the *claimed* id — never silently
    /// substitute the other, live session's identity just because its
    /// `workspace_path` happens to match the cwd ancestor scan.
    #[tokio::test]
    async fn terminated_env_claim_is_not_superseded_by_a_cwd_match_on_a_different_live_session() {
        let store = test_store();
        seed_session(&store, "old", Some("orch-1"), SessionStatus::Terminated);
        seed_session(&store, "new", Some("orch-1"), SessionStatus::Working);
        // `seed_session` gives each id its own `/work/{id}` workspace_path --
        // force the reused-workspace collision the bug depends on.
        store.upsert_session(&{
            let mut s = store.get_session("new").unwrap().unwrap();
            s.workspace_path = Some("/work/old".into());
            s
        }).unwrap();

        let resolution = resolve_for_caller(
            &store, &AppConfig::default(), Some("old"), Some(Path::new("/work/old")), None,
        ).await.unwrap();
        assert_eq!(resolution, Resolution::Terminated { session_id: "old".into(), at: None });
    }

    #[test]
    fn capability_slice_differs_by_audience() {
        let store = test_store();
        seed_session(&store, "orch-1", None, SessionStatus::Working);
        store.upsert_orchestrator(&Orchestrator { id: "orch-1".into(), name: "orch-1".into(), created_at: 0 }).unwrap();
        seed_session(&store, "worker-1", Some("orch-1"), SessionStatus::Working);

        let config = AppConfig::default();
        let Resolution::Identity(orch) = resolve(&store, &config, "orch-1").unwrap() else { panic!() };
        let Resolution::Identity(worker) = resolve(&store, &config, "worker-1").unwrap() else { panic!() };
        assert!(orch.capabilities.contains(&"spawn-worker"));
        assert!(!worker.capabilities.contains(&"spawn-worker"));
        assert!(worker.capabilities.contains(&"brain"));
    }

    #[test]
    fn render_text_covers_every_resolution_variant() {
        assert!(render_text(&Resolution::Unmanaged { session_id: Some("ghost".into()) }).contains("ghost"));
        assert!(render_text(&Resolution::Unmanaged { session_id: None }).contains("not managing"));
        assert!(render_text(&Resolution::Terminated { session_id: "s1".into(), at: Some(5) }).contains("terminated"));
        assert!(render_text(&Resolution::Terminated { session_id: "s1".into(), at: None }).contains("terminated"));
    }

    #[test]
    fn render_json_marks_unmanaged_and_terminated_as_not_managed() {
        let unmanaged = render_json(&Resolution::Unmanaged { session_id: Some("ghost".into()) });
        assert_eq!(unmanaged["managed"], false);
        assert_eq!(unmanaged["reason"], "unmanaged");

        let terminated = render_json(&Resolution::Terminated { session_id: "s1".into(), at: Some(5) });
        assert_eq!(terminated["managed"], false);
        assert_eq!(terminated["reason"], "terminated");
        assert_eq!(terminated["terminatedAt"], 5);
    }

    #[test]
    fn render_reminder_never_empty_and_within_budget_for_every_variant() {
        let store = test_store();
        seed_session(&store, "worker-1", Some("orch-1"), SessionStatus::Working);
        let identity = resolve(&store, &AppConfig::default(), "worker-1").unwrap();

        for resolution in [
            identity,
            Resolution::Unmanaged { session_id: Some("ghost".into()) },
            Resolution::Unmanaged { session_id: None },
            Resolution::Terminated { session_id: "s1".into(), at: Some(5) },
        ] {
            let reminder = render_reminder(&resolution);
            assert!(!reminder.is_empty());
            assert!(reminder.chars().count() <= MAX_IDENTITY_REMINDER_CHARS);
        }
    }

    #[test]
    fn render_reminder_truncates_to_the_budget() {
        let store = test_store();
        seed_session(&store, "worker-1", Some("orch-1"), SessionStatus::Working);
        let mut identity = match resolve(&store, &AppConfig::default(), "worker-1").unwrap() {
            Resolution::Identity(id) => id,
            other => panic!("expected Identity, got {other:?}"),
        };
        identity.workspace_path = Some("x".repeat(MAX_IDENTITY_REMINDER_CHARS * 2));

        let reminder = render_reminder(&Resolution::Identity(identity));
        assert_eq!(reminder.chars().count(), MAX_IDENTITY_REMINDER_CHARS);
    }
}
