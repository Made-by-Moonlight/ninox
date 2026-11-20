//! End to end against a fake `claude` in a real (test-socket) tmux session:
//! the plan request lands in the orchestrator's pane, the plan change is
//! detected, and the reset relaunches the same session id fresh with a kick-off
//! prompt, the plan waiting in a snapshot for the session-setup hook. Ignored by default (needs tmux); run with
//! `cargo test -p ninox-ui-shared --test reset_from_plan_e2e -- --ignored --nocapture`.

use std::sync::Arc;
use std::time::Duration;

use ninox_core::{
    config::{AppConfig, SendMechanism},
    store::Store,
    types::{ActivityState, Orchestrator, Session, SessionStatus},
};
use ninox_ui_shared::reset_from_plan::{Phase, ResetFlow};

fn row(id: &str, orch: Option<&str>, ws: &str) -> Session {
    Session {
        id: id.into(), orchestrator_id: orch.map(Into::into), name: id.into(), repo: String::new(),
        status: SessionStatus::Working, agent_type: "claude-code".into(),
        cost_usd: 0.0, started_at: 0, pr_number: None, pr_id: None,
        workspace_path: Some(ws.into()), pid: None,
        model: Some("claude-opus-5-5".into()), observed_model: None, context_tokens: None,
        brain_reads: Vec::new(), brain_write: None,
        context_used_pct: None, context_total_tokens: None, context_window_size: None,
        claude_session_id: None, summary: None,
        terminal_at: None, gate_status: None, merged_at: None,
        activity: Default::default(), activity_note: None, activity_since: None,
        machine_id: None, worker_type: None, success_criteria: Vec::new(),
        completion_outcome: None, self_completed_at: None, cleanup_result: None, cleanup_at: None,
    }
}

async fn until(what: &str, mut f: impl FnMut() -> bool) {
    for _ in 0..100 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test]
#[ignore = "needs tmux; drives a fake claude"]
async fn reset_from_plan_relaunches_fresh_with_the_plan_and_keeps_workers() {
    let iso = ninox_core::test_isolation::isolate();
    let root = tempfile::tempdir().unwrap();
    let r = root.path();
    let (home, bin, ws, e2e) = (r.join("home"), r.join("bin"), r.join("ws"), r.join("e2e"));
    for d in [&home, &bin, &ws, &e2e] {
        std::fs::create_dir_all(d).unwrap();
    }
    let script = format!(
        "#!/bin/sh\n{{ echo ARGV-BEGIN; for a in \"$@\"; do printf '%s\\n' \"$a\"; done; echo ARGV-END; }} >> {e}/argv.log\n\
         printf '\\342\\235\\257 '\nwhile IFS= read -r line; do printf '%s\\n' \"$line\" >> {e}/input.log; printf '\\n\\342\\235\\257 '; done\n",
        e = e2e.display()
    );
    let fake = bin.join("claude");
    std::fs::write(&fake, script).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    // Seeded the way a real orchestrator workspace is, so the reset takes the hook path.
    std::fs::create_dir_all(ws.join(".claude")).unwrap();
    std::fs::write(
        ws.join(".claude/settings.local.json"),
        r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"'/b/ninox' session-setup"}]}]}}"#,
    )
    .unwrap();
    // Scratch HOME so trust seeding never touches the real ~/.claude.json.
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("CLAUDE_CONFIG_DIR", home.join(".claude"));
        std::env::set_var("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()));
    }

    let tag = format!("e2e{}", std::process::id());
    let (orch, worker) = (format!("{tag}-orch"), format!("{tag}-w1"));
    let store = Arc::new(Store::open(r.join("t.db")).unwrap());
    store.upsert_orchestrator(&Orchestrator { id: orch.clone(), name: orch.clone(), created_at: 0 }).unwrap();
    let mut orch_row = row(&orch, None, ws.to_str().unwrap());
    orch_row.success_criteria = vec![ninox_core::worker_type::SuccessCriterion::PrMerged];
    store.upsert_session(&orch_row).unwrap();
    store.upsert_session(&row(&worker, Some(&orch), ws.to_str().unwrap())).unwrap();
    let plan = ws.join("PLAN.md");
    std::fs::write(&plan, "# Plan\n- before the request\n").unwrap();
    store.register_orchestrator_plan(&orch, plan.to_str().unwrap(), 1).unwrap();

    let mut config = AppConfig::default();
    // An absolute path: a tmux server that is already running keeps its own PATH.
    let mut spec = config.registry().spec("claude-code");
    spec.binary = Some(fake.to_string_lossy().into_owned());
    config.harnesses.insert("claude-code".into(), spec);
    config.messaging = Some(ninox_core::config::MessagingConfig { mechanism: SendMechanism::Keystrokes });

    // The "old conversation": a live orchestrator pane running the fake claude.
    let engine = ninox_core::events::Engine::new(store.clone());
    let session = store.get_session(&orch).unwrap().unwrap();
    let req = ninox_ui_shared::restart::refile_request(&store, session, true, &config, 1).unwrap();
    let first_conversation = req.claude_session_id.clone();
    ninox_ui_shared::spawn_util::relaunch_in_place(engine.clone(), req, &config, false).await.unwrap();
    until("the first launch", || std::fs::read_to_string(e2e.join("argv.log")).is_ok()).await;
    assert!(ninox_core::runtime::wait_for_input_prompt(&orch, Duration::from_secs(20)).await);

    // Step 1: the plan request reaches the pane.
    let mut flow = ResetFlow::start(store.clone(), &config, &orch).await.unwrap().with_timing(Duration::from_secs(600), Duration::from_secs(0));
    until("the plan request in the pane", || {
        std::fs::read_to_string(e2e.join("input.log")).is_ok_and(|s| s.contains("[ninox system]") && s.contains("PLAN.md"))
    })
    .await;
    println!("INPUT RECEIVED BY THE ORCHESTRATOR PANE:\n{}", std::fs::read_to_string(e2e.join("input.log")).unwrap());
    assert_eq!(flow.poll(), Phase::Waiting, "plan unchanged and still working");

    // The orchestrator updates its plan and goes idle.
    let big = format!("# Plan\n- decision: ship the reset\n- live worker: w1 (PR #1)\n{}", "- it's a \"quoted\" $(not run) `line` with {model}\n".repeat(500));
    assert!(big.len() > 20_000);
    std::fs::write(&plan, &big).unwrap();
    let mut s = store.get_session(&orch).unwrap().unwrap();
    s.activity = ActivityState::Idle;
    store.upsert_session(&s).unwrap();
    assert_eq!(flow.poll(), Phase::Ready);
    println!("PLAN CHANGE DETECTED: phase = Ready");

    // Step 2: confirm.
    let report = flow.confirm(&config).await.unwrap();
    assert_eq!(report.old_conversation.as_deref(), Some(first_conversation.as_str()));
    let argv = std::fs::read_to_string(e2e.join("argv.log")).unwrap();
    let launches: Vec<&str> = argv.split("ARGV-BEGIN\n").filter(|l| !l.is_empty()).collect();
    println!("FRESH LAUNCH ARGV (first 700 chars of {} bytes):\n{}", launches.last().unwrap().len(), launches.last().unwrap().chars().take(700).collect::<String>());
    assert_eq!(launches.len(), 2, "{argv}");
    let fresh = launches[1];
    assert!(!fresh.contains("--resume"), "{fresh}");
    assert!(fresh.contains("--session-id") && !fresh.contains(&first_conversation), "{fresh}");
    assert!(fresh.contains("Continue from your plan."), "the fresh launch keeps a kick-off prompt so it starts a turn: {fresh}");
    assert!(!fresh.contains("decision: ship the reset"), "the plan no longer rides on the launch: {fresh}");
    // The fake claude runs no hooks, so the snapshot the session-setup hook would deliver is still on disk.
    let snapshot = std::fs::read_to_string(ninox_core::session_setup::reset_snapshot_path(&AppConfig::sessions_dir(), &orch)).unwrap();
    assert!(snapshot.contains("continuing as orchestrator") && snapshot.contains("decision: ship the reset") && snapshot.contains("PLAN.md"), "{snapshot}");
    assert!(snapshot.contains("it's a \"quoted\" $(not run) `line` with {model}"), "plan text is preserved byte for byte");
    assert!(snapshot.len() > 20_000, "the whole 20 KB plan is in the snapshot");
    assert!(!ws.parent().unwrap().join("e2e").join("ran").exists());

    let after = store.get_session(&orch).unwrap().unwrap();
    assert_ne!(after.claude_session_id.as_deref(), Some(first_conversation.as_str()));
    assert_eq!(
        after.success_criteria,
        vec![ninox_core::worker_type::SuccessCriterion::PrMerged],
        "a relaunch keeps the criteria the session-setup hook serves"
    );
    assert_eq!(store.retired_conversations(&orch).unwrap()[0].0, first_conversation);
    let w = store.get_session(&worker).unwrap().unwrap();
    assert_eq!(w.orchestrator_id.as_deref(), Some(orch.as_str()), "worker still parented");
    println!("WORKER {} STILL PARENTED TO {}; OLD CONVERSATION {} RETIRED", worker, orch, first_conversation);

    let _ = ninox_core::runtime::kill_session(&orch).await;
    drop(iso);
}
