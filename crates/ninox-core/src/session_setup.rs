//! Session setup delivered by a Claude Code `SessionStart` hook instead of
//! launch prompts, appended system prompts or files the agent must read.
//!
//! The hook (`ninox session-setup`) runs on every `source` Claude Code
//! reports (`startup`, `resume`, `clear`, `compact`) and prints
//! `additionalContext` built from live state, so a restart, a resume, a
//! `/clear` and a reset-from-plan all hand the orchestrator the same role.
//! Harnesses without hooks keep their launch prompt or file delivery; see
//! [`crate::harness::HarnessRegistry::supports_session_hooks`].

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::capabilities::{self, Audience, Capability};
use crate::config::AppConfig;
use crate::store::Store;

/// First input of a reset orchestrator under claude-code. A hook only adds
/// context and never starts a turn, so the launch still needs a prompt to
/// make the fresh conversation start working; the plan itself arrives
/// through the hook.
pub const RESET_KICKOFF_PROMPT: &str = "Continue from your plan.";

/// Arbitrary; a snapshot nobody consumed within this long belongs to a
/// launch that never reached its hook, and must not leak into a later one.
const SNAPSHOT_MAX_AGE: Duration = Duration::from_secs(10 * 60);

/// The one-shot file carrying a reset's plan snapshot to the next
/// `startup` of `session_id`.
pub fn reset_snapshot_path(sessions_dir: &Path, session_id: &str) -> PathBuf {
    sessions_dir.join(format!("{session_id}.reset-plan.md"))
}

/// Whether `workspace`'s own `settings.local.json` runs the session-setup
/// hook (the file Claude Code reads from the directory it starts in).
pub fn hook_installed(workspace: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(workspace.join(".claude").join(crate::claude_settings::LOCAL_SETTINGS)) else {
        return false;
    };
    let Ok(settings) = serde_json::from_str::<serde_json::Value>(&text) else { return false };
    settings["hooks"]["SessionStart"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|group| group["hooks"].as_array().into_iter().flatten())
        .any(|hook| hook["command"].as_str().is_some_and(|c| c.trim_end().ends_with("session-setup")))
}

/// Whether session setup for a session of `harness` in `workspace` reaches it
/// through the hook. When the hook could not be installed (a git-tracked
/// `settings.local.json`, an unparsable one) the launch-time delivery stays,
/// so the role is never lost.
pub fn delivered_by_hook(config: &AppConfig, harness: &str, workspace: &Path) -> bool {
    config.registry().supports_session_hooks(harness) && hook_installed(workspace)
}

/// What the hook prints for a hook-capable orchestrator, or `None` when
/// `session_id` is not an orchestrator (nothing to say; workers follow).
pub async fn orchestrator_context(
    store: &Store,
    config: &AppConfig,
    session_id: &str,
    source: &str,
    ninox_bin: &str,
    sessions_dir: &Path,
) -> Option<String> {
    if !store.is_orchestrator(session_id).unwrap_or(false) {
        return None;
    }
    let custom = matches!(store.custom_workspace(session_id), Ok(Some(_)));
    let caps: Vec<&Capability> = capabilities::enabled_for(Audience::Orchestrator, config).collect();
    let root = config.resolved_orchestrator_root();
    let mut out = crate::orchestrator_root::role_text(
        &caps,
        &|name| {
            if custom {
                Path::new(".claude/skills").join(crate::custom_workspace::skill_dir_name(name)).join("SKILL.md")
            } else {
                root.join(".claude").join("skills").join(name).join("SKILL.md")
            }
        },
        ninox_bin,
    );

    let spawner = store.get_session(session_id).ok().flatten().and_then(|s| s.orchestrator_id);
    out.push_str(&orchestrator_footer(session_id, spawner.as_deref()));

    out.push_str(&identity_note(store, config, session_id, Some("orchestrator")).await);

    if source == "startup" {
        if let Some(snapshot) = take_reset_snapshot(sessions_dir, session_id) {
            out.push_str("\n---\n");
            out.push_str(&snapshot);
            out.push('\n');
            return Some(out);
        }
    }
    if matches!(source, "clear" | "compact") {
        if let Ok(Some(plan)) = store.get_orchestrator_plan(session_id) {
            out.push_str(&format!(
                "\n---\nYour registered plan doc is {}. Re-read it before acting; it is your source of truth \
                 after losing context.\n",
                plan.file_path
            ));
        }
    }
    Some(out)
}

/// The identity reminder as its own paragraph, when `[role_rehydration]` is
/// on and the session resolves; empty otherwise.
async fn identity_note(store: &Store, config: &AppConfig, session_id: &str, caller_type: Option<&str>) -> String {
    if !crate::identity::hooks_enabled(config) {
        return String::new();
    }
    match crate::identity::resolve_for_caller(store, config, Some(session_id), None, caller_type).await {
        Ok(resolution) => format!("\n{}\n", crate::identity::render_reminder(&resolution)),
        Err(_) => String::new(),
    }
}

/// What the hook prints for a spawned worker: its channels back to its
/// orchestrator and its Definition of done, from the stored session row, so
/// they survive `/clear`, compaction and restarts. `None` for a session with
/// no orchestrator: a standalone session gets its Definition of done in its
/// first message instead, so serving it here too would duplicate it.
pub async fn worker_context(store: &Store, config: &AppConfig, session_id: &str) -> Option<String> {
    let session = store.get_session(session_id).ok().flatten()?;
    let orchestrator = session.orchestrator_id.as_deref()?;
    let mut out = String::new();
    out.push_str(worker_footer(session_id, orchestrator, config.pr_watch.enabled).trim_start());
    out.push_str(&crate::lifecycle::reactions::format_definition_of_done(&session.success_criteria));
    out.push_str(&identity_note(store, config, session_id, None).await);
    Some(out)
}

/// What the hook prints for `session_id`, whichever kind of session it is.
pub async fn session_context(
    store: &Store,
    config: &AppConfig,
    session_id: &str,
    source: &str,
    ninox_bin: &str,
    sessions_dir: &Path,
) -> Option<String> {
    match orchestrator_context(store, config, session_id, source, ninox_bin, sessions_dir).await {
        Some(text) => Some(text),
        None if store.is_orchestrator(session_id).unwrap_or(false) => None,
        None => worker_context(store, config, session_id).await,
    }
}

/// The snapshot written for this session's reset, consumed so a later
/// `startup` never sees it again. Stale files are discarded unread.
fn take_reset_snapshot(sessions_dir: &Path, session_id: &str) -> Option<String> {
    let path = reset_snapshot_path(sessions_dir, session_id);
    let fresh = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age <= SNAPSHOT_MAX_AGE);
    let text = fresh.then(|| std::fs::read_to_string(&path).ok()).flatten();
    let _ = std::fs::remove_file(&path);
    text.filter(|t| !t.trim().is_empty())
}

/// Who the orchestrator is, that it coordinates rather than implements, and
/// (when spawned by another orchestrator) the channel back to it.
pub fn orchestrator_footer(id: &str, spawner: Option<&str>) -> String {
    let mut footer = format!(
        "\n---\n\
         Ninox orchestrator `{id}`\n\n\
         **Role:** you are an orchestrator, not a worker. Spawn workers \
         (`ninox spawn`) for the brief you are given — never implement it yourself.\n",
    );
    if let Some(spawner) = spawner {
        footer.push_str(&format!(
            "\nSpawned by orchestrator `{spawner}`. Report back when the work is \
             done or you need a decision:\n\
             ```bash\n\
             ninox send {spawner} \"<your message>\"\n\
             ```\n",
        ));
    }
    footer
}

/// The context footer of a spawned worker: its own session id, its
/// orchestrator's id, the channels back to the orchestrator, and the
/// one-worker-one-PR scope rule. Ends with the `ninox capabilities`
/// bootstrap line. When `pr_watch_enabled` (mirrors
/// `AppConfig.pr_watch.enabled`) it also tells the worker to register PR
/// watches instead of polling `gh`.
pub fn worker_footer(id: &str, orch_id: &str, pr_watch_enabled: bool) -> String {
    let mut footer = format!(
        "\n\n---\n\
         Ninox session `{id}` · orchestrator `{orch_id}`\n\n\
         **Goal:** complete the task and open a pull request.\n\n\
         **Scope:** one worker, one task, one pull request. If you discover \
         additional work outside this task, do not do it and do not open \
         another PR — hand it to the orchestrator instead:\n\
         ```bash\n\
         ninox request-work \"<description of the additional work>\"\n\
         ```\n\
         To message the orchestrator (e.g. when stuck or when the PR is open):\n\
         ```bash\n\
         ninox send {orch_id} \"<your message>\"\n\
         ```\n\
         Text you write in your own pane never reaches the orchestrator: \
         every read-back, question and report must go through `ninox send`. \
         It prints a receipt; `queued, target unreachable` (exit 75) means \
         Ninox will retry, so do not resend.\n\
         Report back with `ninox send` when (a) you are blocked and need \
         a decision, or (b) the PR is open. You finish yourself with \
         `ninox complete`, as described in the Definition of done.",
    );
    if pr_watch_enabled {
        footer.push_str(
            "\n\nInstead of polling gh for PR/CI status, register watches: \
             `ninox open --pr <url>` (notifications are delivered to you), \
             `ninox close --pr <url>` when done.",
        );
    }
    // `--worker`: unfiltered output would also list the orchestrator-only
    // skills, including spawn-worker telling the reader to delegate work
    // instead of doing it — exactly the wrong instruction for a worker.
    footer.push_str(
        "\n\nRun `ninox capabilities --worker` to list what ninox can currently do.",
    );
    footer
}

/// The `SessionStart` JSON a hook prints for `context`.
pub fn hook_output(context: &str) -> serde_json::Value {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": context,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Orchestrator, Session, SessionStatus};

    struct Fixture {
        _iso: crate::test_isolation::Isolation,
        _dir: tempfile::TempDir,
        store: Store,
        sessions: PathBuf,
    }

    fn fixture() -> Fixture {
        let iso = crate::test_isolation::isolate();
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("t.db")).unwrap();
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        store
            .upsert_session(&Session {
                id: "orch".into(), orchestrator_id: Some("parent".into()), name: "orch".into(), repo: "o/r".into(),
                status: SessionStatus::Working, agent_type: "claude-code".into(), cost_usd: 0.0, started_at: 0,
                pr_number: None, pr_id: None, workspace_path: Some("/w".into()), pid: None, model: None,
                observed_model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
                context_used_pct: None, context_total_tokens: None, context_window_size: None,
                claude_session_id: None, summary: None, terminal_at: None, gate_status: None, merged_at: None,
                activity: Default::default(), activity_note: None, activity_since: None, machine_id: None,
                worker_type: None, success_criteria: Vec::new(), completion_outcome: None,
                self_completed_at: None, cleanup_result: None, cleanup_at: None,
            })
            .unwrap();
        store.upsert_orchestrator(&Orchestrator { id: "orch".into(), name: "orch".into(), created_at: 0 }).unwrap();
        Fixture { _iso: iso, _dir: dir, store, sessions }
    }

    async fn ctx(f: &Fixture, id: &str, source: &str) -> Option<String> {
        orchestrator_context(&f.store, &AppConfig::default(), id, source, "ninox", &f.sessions).await
    }

    #[tokio::test]
    async fn every_source_carries_role_skills_and_spawner() {
        let f = fixture();
        for source in ["startup", "resume", "clear", "compact"] {
            let text = ctx(&f, "orch", source).await.unwrap();
            assert!(text.contains("# Ninox Orchestrator"), "{source}: {text}");
            assert!(text.contains("spawn-worker/SKILL.md"), "{source}");
            assert!(text.contains("ninox capabilities --orchestrator"), "{source}");
            assert!(text.contains("Ninox orchestrator `orch`") && text.contains("ninox send parent"), "{source}");
        }
    }

    #[tokio::test]
    async fn a_custom_workspace_orchestrator_gets_namespaced_skill_paths() {
        let f = fixture();
        f.store.set_custom_workspace("orch", "/w").unwrap();
        let text = ctx(&f, "orch", "startup").await.unwrap();
        assert!(text.contains(".claude/skills/ninox-spawn-worker/SKILL.md"), "{text}");
    }

    #[test]
    fn hook_installed_reads_the_workspaces_own_settings() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!hook_installed(dir.path()));
        let claude = dir.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        let file = claude.join("settings.local.json");
        std::fs::write(&file, "not json").unwrap();
        assert!(!hook_installed(dir.path()));
        std::fs::write(&file, r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"bin/sync pull"}]}]}}"#).unwrap();
        assert!(!hook_installed(dir.path()));
        std::fs::write(&file, r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"'/b/ninox' session-setup"}]}]}}"#).unwrap();
        assert!(hook_installed(dir.path()));
    }

    fn add_worker(f: &Fixture, id: &str, parent: Option<&str>, criteria: Vec<crate::worker_type::SuccessCriterion>) {
        let mut s = f.store.get_session("orch").unwrap().unwrap();
        s.id = id.into();
        s.name = id.into();
        s.orchestrator_id = parent.map(Into::into);
        s.success_criteria = criteria;
        f.store.upsert_session(&s).unwrap();
    }

    #[tokio::test]
    async fn a_worker_gets_its_footer_and_definition_of_done_on_every_source() {
        use crate::worker_type::SuccessCriterion;
        let f = fixture();
        add_worker(&f, "w1", Some("orch"), vec![SuccessCriterion::PrMerged]);
        for source in ["startup", "resume", "clear", "compact"] {
            let text = session_context(&f.store, &AppConfig::default(), "w1", source, "ninox", &f.sessions).await.unwrap();
            assert!(text.starts_with("---\nNinox session `w1` · orchestrator `orch`"), "{source}: {text}");
            assert!(text.contains("ninox send orch") && text.contains("ninox capabilities --worker"), "{source}");
            assert!(text.contains("**Definition of done:**") && text.contains("ninox complete"), "{source}");
            assert!(!text.contains("# Ninox Orchestrator"), "{source}: a worker gets no orchestrator role");
        }
    }

    #[tokio::test]
    async fn a_session_without_an_orchestrator_gets_nothing_from_the_worker_hook() {
        use crate::worker_type::SuccessCriterion;
        let f = fixture();
        for (id, criteria) in [("solo", Vec::new()), ("solo-with-criteria", vec![SuccessCriterion::ReportSent])] {
            add_worker(&f, id, None, criteria);
            assert!(session_context(&f.store, &AppConfig::default(), id, "startup", "ninox", &f.sessions).await.is_none(), "{id}");
        }
    }

    #[tokio::test]
    async fn only_an_orchestrator_gets_context() {
        let f = fixture();
        assert!(ctx(&f, "nobody", "startup").await.is_none());
    }

    #[tokio::test]
    async fn the_reset_snapshot_is_delivered_once_on_startup_only() {
        let f = fixture();
        let path = reset_snapshot_path(&f.sessions, "orch");
        std::fs::write(&path, "SNAPSHOT-OF-PLAN").unwrap();
        assert!(!ctx(&f, "orch", "resume").await.unwrap().contains("SNAPSHOT-OF-PLAN"));
        assert!(path.exists(), "resume must not consume it");
        assert!(ctx(&f, "orch", "startup").await.unwrap().contains("SNAPSHOT-OF-PLAN"));
        assert!(!path.exists());
        assert!(!ctx(&f, "orch", "startup").await.unwrap().contains("SNAPSHOT-OF-PLAN"));
    }

    #[tokio::test]
    async fn a_stale_snapshot_is_discarded_unread() {
        let f = fixture();
        let path = reset_snapshot_path(&f.sessions, "orch");
        std::fs::write(&path, "OLD").unwrap();
        let old = SystemTime::now() - SNAPSHOT_MAX_AGE - Duration::from_secs(60);
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(old).unwrap();
        assert!(!ctx(&f, "orch", "startup").await.unwrap().contains("OLD"));
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn clear_and_compact_point_at_the_registered_plan() {
        let f = fixture();
        f.store.register_orchestrator_plan("orch", "/p/PLAN.md", 1).unwrap();
        for source in ["clear", "compact"] {
            assert!(ctx(&f, "orch", source).await.unwrap().contains("/p/PLAN.md"), "{source}");
        }
        assert!(!ctx(&f, "orch", "startup").await.unwrap().contains("/p/PLAN.md"));
    }
}
