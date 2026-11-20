//! Why orchestrators never reported their context window, and the fix.
//!
//! Claude Code reads `.claude/settings.local.json` from the directory it
//! starts in, never a parent. The fake `claude` here models exactly that: it
//! looks for a statusLine in `./.claude/settings.local.json` and records what
//! it found. Where it found one, the test runs what `ninox statusline` does
//! with Claude Code's payload (`apply_update`).
//!
//! Orchestrator "old" starts the way every orchestrator did before the fix: in
//! a subdirectory of a root that carries the statusLine, with nothing of its
//! own. Orchestrator "new" is launched through `relaunch_in_place`, which now
//! seeds the orchestrator's own workspace. Ignored by default (needs tmux);
//! run with `cargo test -p ninox-ui-shared --test orchestrator_statusline_e2e
//! -- --ignored --nocapture`, with a scratch HOME.

use std::sync::Arc;
use std::time::Duration;

use ninox_core::{
    config::AppConfig,
    lifecycle::statusline::{apply_update, parse_payload},
    store::Store,
    types::{Orchestrator, Session, SessionStatus},
};

fn row(id: &str, ws: &str) -> Session {
    Session {
        id: id.into(), orchestrator_id: None, name: id.into(), repo: String::new(),
        status: SessionStatus::Working, agent_type: "claude-code".into(),
        cost_usd: 0.0, started_at: 0, pr_number: None, pr_id: None,
        workspace_path: Some(ws.into()), pid: None,
        model: Some("claude-opus-5-5".into()), observed_model: None, context_tokens: Some(99_000),
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
async fn an_orchestrator_reports_context_only_when_its_own_workspace_has_the_statusline() {
    let _iso = ninox_core::test_isolation::isolate();
    let scratch = tempfile::tempdir().unwrap();
    let r = scratch.path();
    let (home, bin, e2e, root) = (r.join("home"), r.join("bin"), r.join("e2e"), r.join("orchestrators"));
    for d in [&home, &bin, &e2e, &root] {
        std::fs::create_dir_all(d).unwrap();
    }
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("CLAUDE_CONFIG_DIR", home.join(".claude"));
    }
    let fake = bin.join("claude");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\nname=$(basename \"$PWD\")\n\
             if [ -f .claude/settings.local.json ] && grep -q statusLine .claude/settings.local.json; then echo configured > {e}/$name.status; else echo none > {e}/$name.status; fi\n\
             printf '\\342\\235\\257 '\nwhile IFS= read -r line; do :; done\n",
            e = e2e.display()
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let mut config = AppConfig { orchestrator_root: Some(root.clone()), ..AppConfig::default() };
    let mut spec = config.registry().spec("claude-code");
    spec.binary = Some(fake.to_string_lossy().into_owned());
    config.harnesses.insert("claude-code".into(), spec);

    // The root, as `setup_orchestrator_root` has always seeded it.
    ninox_core::orchestrator_root::setup_orchestrator_root(&root, &config, "/bin/ninox", "/cfg.toml").await.unwrap();
    assert!(root.join(".claude/settings.local.json").exists());

    let tag = format!("e2e{}", std::process::id());
    let (old, new) = (format!("{tag}-old"), format!("{tag}-new"));
    let store = Arc::new(Store::open(r.join("t.db")).unwrap());
    let engine = ninox_core::events::Engine::new(store.clone());
    for id in [&old, &new] {
        let ws = root.join(id);
        std::fs::create_dir_all(&ws).unwrap();
        store.upsert_orchestrator(&Orchestrator { id: id.clone(), name: id.clone(), created_at: 0 }).unwrap();
        store.upsert_session(&row(id, ws.to_str().unwrap())).unwrap();
    }

    // "old": started the way orchestrators were, nothing in its own workspace.
    ninox_core::runtime::create_session(
        ninox_core::runtime::configured_backend(), &old, root.join(&old).to_str().unwrap(),
        &format!("{}", fake.display()), &[],
    )
    .await
    .unwrap();

    // "new": launched through the shared relaunch path.
    let session = store.get_session(&new).unwrap().unwrap();
    let req = ninox_ui_shared::restart::refile_request(&store, session, true, &config, 1).unwrap();
    ninox_ui_shared::spawn_util::relaunch_in_place(engine, req, &config, false).await.unwrap();

    until("both fake claudes to start", || {
        e2e.join(format!("{old}.status")).exists() && e2e.join(format!("{new}.status")).exists()
    })
    .await;
    let status = |id: &str| std::fs::read_to_string(e2e.join(format!("{id}.status"))).unwrap().trim().to_string();
    assert_eq!(status(&old), "none", "before the fix: nothing in the orchestrator's own workspace, the root's statusLine is not read");
    assert_eq!(status(&new), "configured", "after the fix: the orchestrator's own settings.local.json carries the statusLine");
    assert!(!root.join(&old).join(".claude/settings.local.json").exists());

    // What each statusLine, where configured, would deliver.
    for id in [&old, &new] {
        if status(id) == "configured" {
            let ws = root.join(id);
            let payload = format!(
                r#"{{"workspace":{{"current_dir":"{}"}},"model":{{"id":"claude-opus-5-5"}},"context_window":{{"used_percentage":44,"total_input_tokens":440000,"context_window_size":1000000}}}}"#,
                ws.display()
            );
            assert!(apply_update(&store, &parse_payload(&payload)).unwrap());
        }
    }
    let (o, n) = (store.get_session(&old).unwrap().unwrap(), store.get_session(&new).unwrap().unwrap());
    assert_eq!(o.context_used_pct, None, "before: still no context reading");
    assert_eq!((n.context_used_pct, n.context_window_size), (Some(44.0), Some(1_000_000)), "after: percentage and window populated");
    println!("BEFORE (old orchestrator): statusLine seen = {}, context_used_pct = {:?}", status(&old), o.context_used_pct);
    println!("AFTER  (new orchestrator): statusLine seen = {}, context_used_pct = {:?}, window = {:?}", status(&new), n.context_used_pct, n.context_window_size);

    for id in [&old, &new] {
        let _ = ninox_core::runtime::kill_session(id).await;
    }
}
