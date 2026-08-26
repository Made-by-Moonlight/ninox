use ninox_core::{
    store::Store,
    types::{Orchestrator, OrchestratorRuntimeIdentity, Session, SessionStatus},
};
use serde_json::Value;
use std::{
    path::Path,
    process::{Command, Output},
    time::{Duration, Instant},
};
use tempfile::tempdir;

fn session(id: &str, orchestrator_id: &str, workspace: &Path) -> Session {
    Session {
        id: id.into(),
        orchestrator_id: Some(orchestrator_id.into()),
        name: id.into(),
        repo: String::new(),
        status: SessionStatus::Working,
        agent_type: "cursor-agent".into(),
        cost_usd: 0.0,
        started_at: 1,
        pr_number: None,
        pr_id: None,
        workspace_path: Some(workspace.to_string_lossy().into_owned()),
        pid: None,
        model: None,
        context_tokens: None,
        catalogue_path: None,
        context_used_pct: None,
        context_total_tokens: None,
        context_window_size: None,
        claude_session_id: None,
        summary: None,
        terminal_at: None,
        gate_status: None,
    }
}

fn seed_orchestrator(db: &Path, id: &str) -> Store {
    let store = Store::open(db).unwrap();
    store
        .upsert_orchestrator(&Orchestrator {
            id: id.into(),
            name: id.into(),
            created_at: 0,
        })
        .unwrap();
    store
}

fn persist_orchestrator_runtime(db: &Path, workspace: &Path, tmux_tmp: &Path, pane_pid: u32) {
    let store = Store::open(db).unwrap();
    let mut runtime = session("orch", "orch", workspace);
    runtime.orchestrator_id = None;
    runtime.pid = Some(pane_pid);
    store.upsert_session(&runtime).unwrap();
    let pane = Command::new("tmux")
        .env("TMUX_TMPDIR", tmux_tmp)
        .args([
            "-L",
            "ninox",
            "list-panes",
            "-t",
            "=orch",
            "-F",
            "#{pane_id}|#{pid}|#{session_created}",
        ])
        .output()
        .unwrap();
    assert!(pane.status.success());
    let pane = String::from_utf8(pane.stdout).unwrap();
    let mut fields = pane.trim().split('|');
    let pane_id = fields.next().unwrap().to_string();
    let server_pid = fields.next().unwrap();
    let session_created = fields.next().unwrap();
    let elapsed_secs = Command::new("/bin/ps")
        .args(["-o", "etime=", "-p", &pane_pid.to_string()])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|elapsed| {
            let parts = elapsed.trim().split(':').collect::<Vec<_>>();
            match parts.as_slice() {
                [minutes, seconds] => {
                    Some(minutes.parse::<i64>().ok()? * 60 + seconds.parse::<i64>().ok()?)
                }
                [hours, minutes, seconds] => Some(
                    hours.parse::<i64>().ok()? * 3600
                        + minutes.parse::<i64>().ok()? * 60
                        + seconds.parse::<i64>().ok()?,
                ),
                _ => None,
            }
        })
        .unwrap_or(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    store
        .register_orchestrator_runtime(&OrchestratorRuntimeIdentity {
            orchestrator_id: "orch".into(),
            runtime_id: "test-runtime".into(),
            server_epoch: format!("{server_pid}:{session_created}"),
            physical_tmux_name: "orch".into(),
            pane_id,
            root_pid: pane_pid,
            root_created_at: now - elapsed_secs * 1000,
            registered_at: now,
        })
        .unwrap();
}

fn assert_envelope(output: &Output, ok: bool, code: Option<&str>) -> Value {
    assert!(
        output.stderr.is_empty(),
        "workers CLI must not mix diagnostics into stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!("stdout is not one JSON value: {error}: {:?}", output.stdout)
    });
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["command"], "workers");
    assert_eq!(value["ok"], ok);
    match code {
        Some(code) => assert_eq!(value["error"]["code"], code),
        None => assert!(value["error"].is_null()),
    }
    value
}

fn run_direct(db: &Path, action: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ninox"))
        .arg("--db")
        .arg(db)
        .arg("workers")
        .args(action)
        .env("NINOX_ORCHESTRATOR_ID", "orch")
        .env("NINOX_CALLER_TYPE", "orchestrator")
        .output()
        .unwrap()
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn wait_for_status(path: &Path) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(status) = std::fs::read_to_string(path) {
            if let Ok(status) = status.parse() {
                return status;
            }
        }
        assert!(Instant::now() < deadline, "workers CLI did not complete");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn run_inside_orchestrator(db: &Path, action: &str) -> Output {
    if !Command::new("tmux")
        .arg("-V")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        panic!("tmux is required for workers CLI boundary tests");
    }

    let root = tempdir().unwrap();
    let tmux_tmp = root.path().join("tmux");
    let home = root.path().join("home");
    let config = root.path().join("config");
    let data = root.path().join("data");
    std::fs::create_dir_all(&tmux_tmp).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&data).unwrap();
    let stdout = root.path().join("stdout");
    let stderr = root.path().join("stderr");
    let status = root.path().join("status");
    let start = root.path().join("start");
    let command = format!(
        "while [ ! -e {} ]; do sleep 0.01; done; \
         {} --db {} workers {action} > {} 2> {}; printf '%s' $? > {}",
        shell_quote(&start.to_string_lossy()),
        shell_quote(env!("CARGO_BIN_EXE_ninox")),
        shell_quote(&db.to_string_lossy()),
        shell_quote(&stdout.to_string_lossy()),
        shell_quote(&stderr.to_string_lossy()),
        shell_quote(&status.to_string_lossy()),
    );

    let launched = Command::new("tmux")
        .env("TMUX_TMPDIR", &tmux_tmp)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &config)
        .args(["-L", "ninox", "new-session", "-d", "-s", "orch"])
        .args(["-e", "NINOX_ORCHESTRATOR_ID=orch"])
        .args(["-e", "NINOX_CALLER_TYPE=orchestrator"])
        .arg("-e")
        .arg(format!("NINOX_DATA_DIR={}", data.display()))
        .arg(command)
        .output()
        .unwrap();
    assert!(
        launched.status.success(),
        "launch orchestrator: {}",
        String::from_utf8_lossy(&launched.stderr)
    );

    let pane_pid = Command::new("tmux")
        .env("TMUX_TMPDIR", &tmux_tmp)
        .args([
            "-L",
            "ninox",
            "list-panes",
            "-t",
            "=orch",
            "-F",
            "#{pane_pid}",
        ])
        .output()
        .unwrap();
    assert!(pane_pid.status.success());
    let pane_pid = String::from_utf8(pane_pid.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    persist_orchestrator_runtime(db, &home, &tmux_tmp, pane_pid);
    std::fs::write(&start, "").unwrap();

    let status_code = wait_for_status(&status);
    let _ = Command::new("tmux")
        .env("TMUX_TMPDIR", &tmux_tmp)
        .args(["-L", "ninox", "kill-server"])
        .output();

    Output {
        status: std::process::ExitStatus::from_raw(status_code << 8),
        stdout: std::fs::read(stdout).unwrap_or_default(),
        stderr: std::fs::read(stderr).unwrap_or_default(),
    }
}

#[test]
fn worker_created_pane_inside_orchestrator_session_is_denied() {
    let root = tempdir().unwrap();
    let db = root.path().join("ninox.db");
    seed_orchestrator(&db, "orch");
    let tmux_tmp = root.path().join("tmux");
    let home = root.path().join("home");
    let config = root.path().join("config");
    let stdout = root.path().join("stdout");
    let stderr = root.path().join("stderr");
    let status = root.path().join("status");
    for path in [&tmux_tmp, &home, &config] {
        std::fs::create_dir_all(path).unwrap();
    }

    let launched = Command::new("tmux")
        .env("TMUX_TMPDIR", &tmux_tmp)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &config)
        .args(["-L", "ninox", "new-session", "-d", "-s", "orch", "sleep 30"])
        .output()
        .unwrap();
    assert!(launched.status.success());
    let pane_pid = Command::new("tmux")
        .env("TMUX_TMPDIR", &tmux_tmp)
        .args([
            "-L",
            "ninox",
            "list-panes",
            "-t",
            "=orch",
            "-F",
            "#{pane_pid}",
        ])
        .output()
        .unwrap();
    let pane_pid = String::from_utf8(pane_pid.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    persist_orchestrator_runtime(&db, &home, &tmux_tmp, pane_pid);

    let command = format!(
        "{} --db {} workers list > {} 2> {}; printf '%s' $? > {}",
        shell_quote(env!("CARGO_BIN_EXE_ninox")),
        shell_quote(&db.to_string_lossy()),
        shell_quote(&stdout.to_string_lossy()),
        shell_quote(&stderr.to_string_lossy()),
        shell_quote(&status.to_string_lossy()),
    );
    let split = Command::new("tmux")
        .env("TMUX_TMPDIR", &tmux_tmp)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &config)
        .args(["-L", "ninox", "split-window", "-d", "-t", "orch:0"])
        .args(["-e", "NINOX_ORCHESTRATOR_ID=orch"])
        .args(["-e", "NINOX_CALLER_TYPE=orchestrator"])
        .arg(command)
        .output()
        .unwrap();
    assert!(
        split.status.success(),
        "split worker pane: {}",
        String::from_utf8_lossy(&split.stderr)
    );

    let status_code = wait_for_status(&status);
    let output = Output {
        status: std::process::ExitStatus::from_raw(status_code << 8),
        stdout: std::fs::read(stdout).unwrap_or_default(),
        stderr: std::fs::read(stderr).unwrap_or_default(),
    };
    let _ = Command::new("tmux")
        .env("TMUX_TMPDIR", &tmux_tmp)
        .args(["-L", "ninox", "kill-server"])
        .output();

    assert!(!output.status.success());
    assert_envelope(&output, false, Some("authorization_failed"));
}

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;

#[test]
fn forged_worker_environment_is_denied_with_one_json_envelope() {
    let root = tempdir().unwrap();
    let db = root.path().join("ninox.db");
    seed_orchestrator(&db, "orch");

    let output = run_direct(&db, &["list"]);

    assert!(!output.status.success());
    assert_envelope(&output, false, Some("authorization_failed"));
}

#[test]
fn database_failure_is_one_json_envelope() {
    let root = tempdir().unwrap();
    let db = root.path().join("database-is-a-directory");
    std::fs::create_dir(&db).unwrap();

    let output = run_direct(&db, &["list"]);

    assert!(!output.status.success());
    assert_envelope(&output, false, Some("database_error"));
}

#[test]
fn not_found_is_one_json_envelope_and_nonzero() {
    let root = tempdir().unwrap();
    let db = root.path().join("ninox.db");
    seed_orchestrator(&db, "orch");

    let output = run_inside_orchestrator(&db, "inspect missing");

    assert!(!output.status.success());
    assert_envelope(&output, false, Some("not_found"));
}

#[test]
fn live_orchestrator_runtime_can_list_with_one_json_envelope() {
    let root = tempdir().unwrap();
    let db = root.path().join("ninox.db");
    seed_orchestrator(&db, "orch");

    let output = run_inside_orchestrator(&db, "list");

    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope = assert_envelope(&output, true, None);
    assert!(envelope["data"].is_array());
}

#[test]
fn isolated_cursor_and_claude_workers_keep_stable_inspection_contracts() {
    let root = tempdir().unwrap();
    let db = root.path().join("ninox.db");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let store = seed_orchestrator(&db, "orch");
    for (id, harness, harness_session_id) in [
        ("cursor", "cursor-agent", None),
        ("claude", "claude-code", Some("claude-session")),
    ] {
        let mut worker = session(id, "orch", &workspace);
        worker.agent_type = harness.into();
        worker.claude_session_id = harness_session_id.map(str::to_string);
        store.upsert_session(&worker).unwrap();
        let incarnation = store
            .prepare_worker_incarnation(id, Some("orch"), 1, workspace.to_str().unwrap(), false, 3)
            .unwrap();
        assert!(store
            .bind_worker_incarnation(
                id,
                &incarnation.incarnation_id,
                workspace.to_str().unwrap(),
                workspace.to_str().unwrap(),
                None,
            )
            .unwrap());
    }

    let output = run_inside_orchestrator(&db, "list");

    assert!(output.status.success());
    let envelope = assert_envelope(&output, true, None);
    let workers = envelope["data"].as_array().unwrap();
    assert_eq!(workers.len(), 2);
    for worker in workers {
        assert_eq!(worker["schema_version"], 1);
        assert_eq!(worker["runtime"]["state"], "missing");
        assert!(worker["incarnation_id"].is_string());
        assert!(worker["workspace"]["path"].is_string());
    }
    let cursor = workers
        .iter()
        .find(|worker| worker["session_id"] == "cursor")
        .unwrap();
    assert_eq!(cursor["harness"], "cursor-agent");
    assert!(cursor["harness_session_id"].is_null());
    let claude = workers
        .iter()
        .find(|worker| worker["session_id"] == "claude")
        .unwrap();
    assert_eq!(claude["harness"], "claude-code");
    assert_eq!(claude["harness_session_id"], "claude-session");
}

#[test]
fn partial_batch_failure_is_one_json_envelope_and_nonzero() {
    let root = tempdir().unwrap();
    let db = root.path().join("ninox.db");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let store = seed_orchestrator(&db, "orch");
    store
        .upsert_session(&session("present", "orch", &workspace))
        .unwrap();

    let output = run_inside_orchestrator(&db, "finalize present missing");

    assert!(!output.status.success());
    let envelope = assert_envelope(&output, false, Some("partial_failure"));
    let results = envelope["data"]["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(
        results.iter().filter(|result| result["ok"] == true).count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|result| result["ok"] == false)
            .count(),
        1
    );
}

#[test]
fn spawning_worker_without_runtime_is_retained_fail_safe() {
    let root = tempdir().unwrap();
    let db = root.path().join("ninox.db");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let store = seed_orchestrator(&db, "orch");
    let mut spawning = session("spawning", "orch", &workspace);
    spawning.status = SessionStatus::Spawning;
    store.upsert_session(&spawning).unwrap();

    let output = run_inside_orchestrator(&db, "finalize spawning");

    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope = assert_envelope(&output, true, None);
    assert_eq!(envelope["data"]["results"][0]["ok"], true);
    assert_eq!(
        envelope["data"]["results"][0]["result"]["outcome"],
        "finalized"
    );
}
