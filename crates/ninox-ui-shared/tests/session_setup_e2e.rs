//! Session setup arrives through the `SessionStart` hook, not launch prompts.
//!
//! The fake `claude` here models what Claude Code does at the start of a
//! session: it reads `./.claude/settings.local.json`, runs every
//! `SessionStart` hook with `{"source": ...}` on stdin, and keeps the
//! `additionalContext` each one printed. It also records whether the launch
//! command gave it a first prompt, because a hook only adds context and never
//! starts a turn. Everything it reads was seeded by ninox itself, and the
//! launch commands come from the same planners the real restart and reset use.
//! Runs under a scratch HOME and `test_isolation`, with no tmux.

use std::path::{Path, PathBuf};
use std::sync::Once;

use ninox_core::{
    config::AppConfig,
    store::Store,
    types::{Orchestrator, Session, SessionStatus},
};
use ninox_ui_shared::{cli::session_setup_output, reset_from_plan::prepare_reset, restart};

const NINOX_BIN: &str = "/scratch/bin/ninox";

fn scratch_home() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let home = tempfile::tempdir().unwrap().keep();
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("CLAUDE_CONFIG_DIR", home.join(".claude"));
        }
    });
}

fn row(id: &str, parent: Option<&str>, ws: &Path) -> Session {
    Session {
        id: id.into(), orchestrator_id: parent.map(Into::into), name: id.into(), repo: String::new(),
        status: SessionStatus::Working, agent_type: "claude-code".into(),
        cost_usd: 0.0, started_at: 0, pr_number: None, pr_id: None,
        workspace_path: Some(ws.to_string_lossy().into_owned()), pid: None,
        model: Some("claude-opus-5-5".into()), observed_model: None, context_tokens: None,
        brain_reads: Vec::new(), brain_write: None,
        context_used_pct: None, context_total_tokens: None, context_window_size: None,
        claude_session_id: Some("conv-1".into()), summary: None,
        terminal_at: None, gate_status: None, merged_at: None,
        activity: Default::default(), activity_note: None, activity_since: None,
        machine_id: None, worker_type: None, success_criteria: Vec::new(),
        completion_outcome: None, self_completed_at: None, cleanup_result: None, cleanup_at: None,
    }
}

struct Started {
    /// `additionalContext` of every `SessionStart` hook that ran.
    contexts: Vec<String>,
    /// The first prompt on the launch command line, which is what makes a
    /// session start a turn on its own.
    first_prompt: Option<String>,
}

impl Started {
    fn all(&self) -> String {
        self.contexts.join("\n")
    }
}

struct World {
    _iso: ninox_core::test_isolation::Isolation,
    _dir: tempfile::TempDir,
    db: PathBuf,
    store: Store,
    config: AppConfig,
    sessions_dir: PathBuf,
}

impl World {
    fn new() -> Self {
        scratch_home();
        let iso = ninox_core::test_isolation::isolate();
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.db");
        let store = Store::open(&db).unwrap();
        let config = AppConfig::default();
        let sessions_dir = AppConfig::sessions_dir();
        std::fs::create_dir_all(&sessions_dir).unwrap();
        Self { _iso: iso, _dir: dir, db, store, config, sessions_dir }
    }

    fn add_orchestrator(&self, id: &str, parent: Option<&str>, ws: &Path) {
        self.store.upsert_orchestrator(&Orchestrator { id: id.into(), name: id.into(), created_at: 0 }).unwrap();
        self.store.upsert_session(&row(id, parent, ws)).unwrap();
    }

    /// What Claude Code does when it starts in `cwd` for `session`.
    async fn claude_starts(&self, cwd: &Path, session: &str, source: &str, launch_cmd: &str) -> Started {
        let settings: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(cwd.join(".claude/settings.local.json")).unwrap()).unwrap();
        let mut contexts = Vec::new();
        for group in settings["hooks"]["SessionStart"].as_array().into_iter().flatten() {
            for hook in group["hooks"].as_array().into_iter().flatten() {
                let command = hook["command"].as_str().unwrap();
                if !command.ends_with("session-setup") {
                    continue;
                }
                assert!(command.contains("/ninox"), "{command}");
                let stdin = serde_json::json!({ "source": source, "session_id": "conv", "cwd": cwd }).to_string();
                let Some(out) =
                    session_setup_output(&stdin, &self.db, Some(session), NINOX_BIN, &self.config, &self.sessions_dir).await
                else {
                    continue;
                };
                let out: serde_json::Value = serde_json::from_str(&out).unwrap();
                assert_eq!(out["hookSpecificOutput"]["hookEventName"], "SessionStart");
                contexts.push(out["hookSpecificOutput"]["additionalContext"].as_str().unwrap().to_string());
            }
        }
        let first_prompt = launch_cmd
            .split_once(" -- ")
            .map(|(_, p)| p.trim().trim_matches('\'').to_string());
        Started { contexts, first_prompt }
    }
}

fn assert_has_role(text: &str, what: &str) {
    assert!(text.contains("# Ninox Orchestrator"), "{what}: no role\n{text}");
    assert!(text.contains("spawn-worker/SKILL.md"), "{what}: no skills pointer\n{text}");
    assert!(text.contains("ninox capabilities --orchestrator"), "{what}: no capabilities line\n{text}");
}

async fn custom_workspace_world(w: &World, root: &Path, ws: &Path) {
    std::fs::create_dir_all(ws).unwrap();
    w.add_orchestrator("o1", Some("boss"), ws);
    w.store.set_custom_workspace("o1", ws.to_str().unwrap()).unwrap();
    let report = ninox_core::custom_workspace::install(ws, root, &w.config, "claude-code", NINOX_BIN, "/scratch/cfg.toml").await;
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
}

/// Launch, restart (fresh and resumed), `/clear` and `/compact` of a custom
/// workspace orchestrator all hand it its role, skills and identity, with no
/// `--append-system-prompt` anywhere.
#[tokio::test]
async fn a_custom_workspace_orchestrator_gets_its_role_from_the_hook_on_every_source() {
    let w = World::new();
    let scratch = tempfile::tempdir().unwrap();
    let (root, ws) = (scratch.path().join("root"), scratch.path().join("project"));
    custom_workspace_world(&w, &root, &ws).await;

    let session = w.store.get_session("o1").unwrap().unwrap();
    let fresh = restart::refile_plan(&w.store, &session, true, &w.config, "u1").unwrap();
    let resumed = restart::resume_plan(&w.store, &session, true, &w.config).unwrap();
    assert!(!fresh.base_cmd.contains("--append-system-prompt"), "{}", fresh.base_cmd);
    assert!(!resumed.base_cmd.contains("--append-system-prompt"), "{}", resumed.base_cmd);

    for (source, cmd) in [
        ("startup", &fresh.base_cmd),
        ("resume", &resumed.base_cmd),
        ("clear", &resumed.base_cmd),
        ("compact", &resumed.base_cmd),
    ] {
        let started = w.claude_starts(&ws, "o1", source, cmd).await;
        let text = started.all();
        assert_has_role(&text, source);
        assert!(text.contains(".claude/skills/ninox-spawn-worker/SKILL.md"), "{source}: namespaced path\n{text}");
        assert!(text.contains("Ninox orchestrator `o1`") && text.contains("ninox send boss"), "{source}: identity\n{text}");
        println!("--- SessionStart source={source} additionalContext ---\n{text}\n");
    }
}

/// Orchestrators in ninox's own root get the same role the same way.
#[tokio::test]
async fn an_ordinary_orchestrator_gets_the_same_role_from_the_hook() {
    let w = World::new();
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path().join("orchestrators");
    let sub = root.join("atlas");
    std::fs::create_dir_all(&sub).unwrap();
    ninox_core::orchestrator_root::setup_orchestrator_root(&root, &w.config, NINOX_BIN, "/scratch/cfg.toml").await.unwrap();
    ninox_core::orchestrator_root::seed_orchestrator_workspace(&sub, &w.config, NINOX_BIN).unwrap();
    w.add_orchestrator("atlas", None, &sub);

    for source in ["startup", "resume", "clear", "compact"] {
        let text = w.claude_starts(&sub, "atlas", source, "claude --session-id 'x'").await.all();
        assert_has_role(&text, source);
        assert!(text.contains("Ninox orchestrator `atlas`"), "{source}\n{text}");
        assert!(!text.contains("Spawned by"), "{source}: top-level orchestrator has no spawner\n{text}");
    }
}

/// A reset orchestrator starts a turn on its own (the launch keeps a
/// kick-off prompt, since a hook starts none) and the hook hands it the plan
/// as it was when the reset was prepared, not the live file.
#[tokio::test]
async fn a_reset_orchestrator_starts_a_turn_and_is_given_the_plan_snapshot_by_the_hook() {
    let w = World::new();
    let scratch = tempfile::tempdir().unwrap();
    let (root, ws) = (scratch.path().join("root"), scratch.path().join("project"));
    custom_workspace_world(&w, &root, &ws).await;
    let plan = scratch.path().join("PLAN.md");
    std::fs::write(&plan, "# Plan\n- SNAPSHOT: finish the reset work\n").unwrap();
    w.store.register_orchestrator_plan("o1", plan.to_str().unwrap(), 1).unwrap();

    let prepared = prepare_reset(&w.store, &w.config, "o1", 5).unwrap();
    std::fs::write(&plan, "# Plan\n- LIVE: written by the old conversation after the snapshot\n").unwrap();

    let cmd = prepared.request.plan.base_cmd.clone();
    assert!(!cmd.contains("--append-system-prompt") && !cmd.contains("SNAPSHOT"), "{cmd}");
    let started = w.claude_starts(&ws, "o1", "startup", &cmd).await;
    assert_eq!(
        started.first_prompt.as_deref(),
        Some("Continue from your plan."),
        "the launch must hand the session a first prompt, or it idles: {cmd}"
    );
    let text = started.all();
    assert_has_role(&text, "reset startup");
    assert!(text.contains("SNAPSHOT: finish the reset work"), "{text}");
    assert!(!text.contains("LIVE: written by the old conversation"), "{text}");
    println!("--- reset startup: first prompt {:?}; additionalContext ---\n{text}\n", started.first_prompt);

    let again = w.claude_starts(&ws, "o1", "startup", &cmd).await.all();
    assert!(!again.contains("SNAPSHOT"), "the snapshot is delivered once");

    let cleared = w.claude_starts(&ws, "o1", "clear", &cmd).await.all();
    assert_has_role(&cleared, "clear after reset");
    assert!(cleared.contains(plan.to_str().unwrap()), "after /clear it is pointed at the registered plan\n{cleared}");
}

/// In a user's folder ninox writes only `.claude/settings.local.json` and
/// `ninox-*` skills; the user's own files and hooks survive.
#[tokio::test]
async fn the_user_workspace_keeps_its_own_files_and_hooks() {
    let w = World::new();
    let scratch = tempfile::tempdir().unwrap();
    let (root, ws) = (scratch.path().join("root"), scratch.path().join("project"));
    std::fs::create_dir_all(ws.join(".claude")).unwrap();
    let user_files = [
        ("CLAUDE.md", "# mine\n"),
        ("AGENTS.md", "# mine too\n"),
        (".claude/settings.json", "{\"model\":\"x\"}\n"),
    ];
    for (name, body) in user_files {
        std::fs::write(ws.join(name), body).unwrap();
    }
    std::fs::write(
        ws.join(".claude/settings.local.json"),
        r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"bin/sync pull"}]}]}}"#,
    )
    .unwrap();
    custom_workspace_world(&w, &root, &ws).await;

    for (name, body) in user_files {
        assert_eq!(std::fs::read_to_string(ws.join(name)).unwrap(), body, "{name} was touched");
    }
    let settings = std::fs::read_to_string(ws.join(".claude/settings.local.json")).unwrap();
    assert!(settings.contains("bin/sync pull") && settings.contains("session-setup"), "{settings}");
    let started = w.claude_starts(&ws, "o1", "startup", "claude").await;
    assert_has_role(&started.all(), "startup");
}

/// With `NINOX_E2E_BIN` pointing at a built `ninox`/`nx`, the real process
/// (dispatch, stdin payload, env) prints what the in-process hook does.
#[tokio::test]
async fn the_real_binary_prints_the_same_hook_output() {
    let Some(bin) = std::env::var_os("NINOX_E2E_BIN") else {
        eprintln!("NINOX_E2E_BIN not set; skipping the real-binary check");
        return;
    };
    let w = World::new();
    let scratch = tempfile::tempdir().unwrap();
    let (root, ws) = (scratch.path().join("root"), scratch.path().join("project"));
    custom_workspace_world(&w, &root, &ws).await;
    // Under NINOX_TEST_ISOLATION the child refuses to resolve the real default root.
    let child_config = scratch.path().join("child-config.toml");
    std::fs::write(&child_config, format!("orchestrator_root = {:?}\n", root.display().to_string())).unwrap();
    for source in ["startup", "resume", "clear", "compact"] {
        let payload = serde_json::json!({ "source": source }).to_string();
        let expected = session_setup_output(&payload, &w.db, Some("o1"), NINOX_BIN, &w.config, &w.sessions_dir).await.unwrap();
        let mut child = std::process::Command::new(&bin)
            .arg("--db").arg(&w.db)
            .arg("session-setup")
            .env("NINOX_SESSION", "o1")
            .env("NINOX_BIN", NINOX_BIN)
            .env("NINOX_DATA_DIR", &w.sessions_dir)
            .env("NINOX_CONFIG", &child_config)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        std::io::Write::write_all(&mut child.stdin.take().unwrap(), payload.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let got: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        let want: serde_json::Value = serde_json::from_str(&expected).unwrap();
        assert_eq!(got, want, "{source}");
        println!("real binary, source={source}: {} bytes of additionalContext", got["hookSpecificOutput"]["additionalContext"].as_str().unwrap().len());
    }
}

/// A spawned worker's footer and Definition of done arrive through the hook
/// seeded in its real worktree, on every source, and the worker is not given
/// the orchestrator's role.
#[tokio::test]
async fn a_worker_gets_its_footer_and_definition_of_done_from_the_hook_on_every_source() {
    let w = World::new();
    let scratch = tempfile::tempdir().unwrap();
    let repo = scratch.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    for args in [&["init", "-q"][..], &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "i"]] {
        assert!(std::process::Command::new("git").arg("-C").arg(&repo).args(args).status().unwrap().success());
    }
    let worktree = ninox_ui_shared::spawn_util::create_worker_worktree(repo.to_str().unwrap(), "w1", false, &w.config).await.unwrap();
    let worktree = PathBuf::from(worktree);
    assert!(ninox_core::session_setup::delivered_by_hook(&w.config, "claude-code", &worktree));

    w.add_orchestrator("boss", None, &repo);
    let mut worker = row("w1", Some("boss"), &worktree);
    worker.success_criteria = vec![ninox_core::worker_type::SuccessCriterion::PrMerged];
    w.store.upsert_session(&worker).unwrap();

    for source in ["startup", "resume", "clear", "compact"] {
        let started = w.claude_starts(&worktree, "w1", source, "claude --session-id 'x' -- 'the task'").await;
        let text = started.all();
        assert!(text.contains("Ninox session `w1` · orchestrator `boss`") && text.contains("ninox send boss"), "{source}\n{text}");
        assert!(text.contains("**Definition of done:**") && text.contains("ninox complete"), "{source}\n{text}");
        assert!(!text.contains("# Ninox Orchestrator"), "{source}");
        assert_eq!(started.first_prompt.as_deref(), Some("the task"), "the worker's own task stays its launch prompt");
    }
}
