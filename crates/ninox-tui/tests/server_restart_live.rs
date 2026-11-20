//! Scripted live check that a server restart keeps running sessions, on
//! both runtime backends. Launches the real binary as a scratch daemon, so
//! it is ignored by default:
//!
//!     cargo test -p ninox-tui --test server_restart_live -- --ignored --nocapture
//!
//! Everything is scratch: `HOME` (so even paths that ignore
//! `NINOX_CONFIG`, like the wrapper bin dir, land in it), config, db, data
//! dir, brain, tmux server (`TMUX_TMPDIR`), ptyd socket, and a random port
//! in 40000–60000 that is checked to be free first.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ninox_core::{
    runtime::{self, Backend},
    store::Store,
    types::{Session, SessionStatus},
};
use ninox_ui_shared::server_restart::{self, LiveOps, Target, Timings};

const BIN: &str = env!("CARGO_BIN_EXE_ninox-tui");

fn free_scratch_port() -> u16 {
    let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos();
    (0..200u32)
        .map(|i| 40000 + ((seed.wrapping_add(i.wrapping_mul(7919))) % 20000) as u16)
        .find(|p| std::net::TcpListener::bind(("127.0.0.1", *p)).is_ok())
        .expect("a free port in 40000-60000")
}

fn session(id: &str, workspace: &Path, pid: u32) -> Session {
    Session {
        id: id.into(), orchestrator_id: None, name: id.into(), repo: String::new(),
        status: SessionStatus::Working, agent_type: "claude-code".into(), cost_usd: 0.0, started_at: 0,
        pr_number: None, pr_id: None, workspace_path: Some(workspace.to_string_lossy().into_owned()),
        pid: Some(pid), model: None, observed_model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None, context_used_pct: None,
        context_total_tokens: None, context_window_size: None, claude_session_id: None, summary: None,
        terminal_at: None, gate_status: None, merged_at: None, activity: Default::default(),
        activity_note: None, activity_since: None, machine_id: None, worker_type: None,
        success_criteria: Vec::new(), completion_outcome: None, self_completed_at: None,
        cleanup_result: None, cleanup_at: None,
    }
}

/// Isolates this process's own environment, which the daemon it spawns
/// inherits. Must run before any other thread exists.
fn scratch_env(home: &Path, short: &Path) {
    for (k, _) in std::env::vars() {
        if k.starts_with("NINOX_") || k == "GITHUB_TOKEN" || k == "GH_TOKEN" || k == "TMUX" || k == "TMUX_PANE" {
            // SAFETY: single-threaded at this point; see the doc comment.
            unsafe { std::env::remove_var(k) };
        }
    }
    let vars: [(&str, PathBuf); 6] = [
        ("HOME", home.to_path_buf()),
        ("NINOX_CONFIG", home.join("config.toml")),
        ("NINOX_DATA_DIR", home.join("data")),
        ("NINOX_BRAIN", home.join("brain")),
        ("TMUX_TMPDIR", short.to_path_buf()),
        (ninox_ptyd::SOCKET_ENV, short.join("p.sock")),
    ];
    for (k, v) in vars {
        // SAFETY: as above.
        unsafe { std::env::set_var(k, v) };
    }
    // The test binary always uses tmux socket `ninox-test`; the daemon must
    // see the same server.
    // SAFETY: as above.
    unsafe { std::env::set_var(ninox_core::tmux::TMUX_SOCKET_ENV, "ninox-test") };
    // So daemon.log carries the poller's lines the restore check reads.
    // SAFETY: as above.
    unsafe { std::env::set_var("RUST_LOG", "info") };
}

#[test]
#[ignore = "launches a scratch daemon, tmux server and ptyd host"]
fn sessions_survive_a_server_restart() {
    // Kept on failure for its daemon.log; removed at the end on success.
    let home = tempfile::tempdir().unwrap().keep();
    let short = tempfile::Builder::new().prefix("nxr").tempdir_in(std::env::temp_dir()).unwrap().keep();
    eprintln!("scratch: {} {}", home.display(), short.display());
    let home = home.as_path();
    let short = short.as_path();
    scratch_env(home, short);
    let port = free_scratch_port();
    assert!((40000..60000).contains(&port) && port != 8080);
    let db = home.join("data").join("ninox.db");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    std::fs::write(
        home.join("config.toml"),
        format!(
            "port = {port}\norchestrator_root = \"{root}\"\n\n[brain]\npath = \"{brain}\"\n",
            root = home.join("orch").display(),
            brain = home.join("brain").display(),
        ),
    )
    .unwrap();
    let _iso = ninox_core::test_isolation::isolate_with_config(home.join("config.toml"));

    let cleanup = Cleanup { port, db: db.clone(), short: short.to_path_buf(), home: home.to_path_buf() };
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let store = Arc::new(Store::open(&db).unwrap());
        runtime::ensure_ptyd_host(Path::new(BIN)).await.expect("scratch ptyd host");

        let mut panes = Vec::new();
        for (id, backend) in [("live-tmux", Backend::Tmux), ("live-ptyd", Backend::Ptyd)] {
            runtime::create_session(backend, id, &home.to_string_lossy(), "sleep 600", &[]).await.unwrap();
            let pid = runtime::Runtime::current().for_session(id).await.session_pid(id).await.expect("pane pid");
            assert_eq!(runtime::Runtime::current().for_session(id).await.kind(), backend);
            store.upsert_session(&session(id, home, pid)).unwrap();
            panes.push((id, pid));
        }

        let target = Target { port, db_path: db.clone(), ninox_bin: PathBuf::from(BIN) };
        ninox_core::daemon::spawn_headless(port, &target.ninox_bin, &db).unwrap();
        let first = wait_for_info(port).await.expect("scratch daemon came up");
        assert_eq!(first.port, port);

        let ops = LiveOps { store: Some(Arc::clone(&store)) };
        let out = server_restart::restart(&target, &ops, Timings::default(), |p| eprintln!("{}", p.label()))
            .await
            .unwrap_or_else(|e| panic!("restart failed: {e}\n{}", log(&db)));
        eprintln!("{}", out.summary());
        assert_eq!(out.old.as_ref().map(|o| o.pid), Some(first.pid));
        assert_ne!(out.new.pid, first.pid);
        assert!(!ninox_core::lifecycle::probe::is_pid_alive(first.pid), "old daemon exited");

        // Long enough for the new daemon's poller to run a cycle against
        // the panes, so a wrongly-marked Interrupted would show up.
        tokio::time::sleep(Duration::from_secs(8)).await;
        for (id, pid) in &panes {
            assert!(ninox_core::lifecycle::probe::is_pid_alive(*pid), "{id}: pane process {pid} survived");
            assert!(runtime::has_session(id).await, "{id}: runtime still hosts it");
            let row = store.get_session(id).unwrap().unwrap();
            assert!(!row.status.is_terminal() && row.status != SessionStatus::Interrupted, "{id}: {:?}", row.status);
        }
        assert_eq!(server_restart::live_session_count(&store).await, 2);
        let daemon_log = log(&db);
        eprintln!("--- daemon.log ---\n{daemon_log}");
        assert_eq!(daemon_log.matches("ninox daemon starting").count(), 2, "one startup line per daemon:\n{daemon_log}");
        assert!(
            daemon_log.contains("daemon exiting: stop accepted on the control socket"),
            "the restart's stop must say why:\n{daemon_log}"
        );
        for marker in ["restor", "relaunch"] {
            assert!(!daemon_log.to_lowercase().contains(marker), "unexpected `{marker}` in daemon.log:\n{daemon_log}");
        }

    });
    drop(cleanup);
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(short);
}

/// Tears the scratch fleet down even when an assertion fails; the scratch
/// dirs themselves are only removed on success.
struct Cleanup {
    port:  u16,
    db:    PathBuf,
    short: PathBuf,
    home:  PathBuf,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let _ = ninox_core::server_control::request_shutdown(
                &ninox_core::server_control::control_socket_path(self.port, &self.db),
            )
            .await;
            for id in ["live-tmux", "live-ptyd"] {
                let _ = runtime::kill_session(id).await;
            }
            if let Ok(mut c) = ninox_ptyd::client::PtydClient::connect(&ninox_ptyd::socket_path(), "test").await {
                let _ = c.shutdown().await;
            }
        });
        let uid = std::os::unix::fs::MetadataExt::uid(&std::fs::metadata(&self.home).unwrap());
        let tmux_socket = self.short.join(format!("tmux-{uid}")).join("ninox-test");
        let _ = std::process::Command::new("tmux").arg("-S").arg(&tmux_socket).arg("kill-server").status();
    }
}

async fn wait_for_info(port: u16) -> Option<ninox_core::server_control::ServerInfo> {
    for _ in 0..300 {
        if let Some(info) = server_restart::fetch_info(port).await {
            return Some(info);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    None
}

fn log(db: &Path) -> String {
    std::fs::read_to_string(db.parent().unwrap().join("daemon.log")).unwrap_or_default()
}
