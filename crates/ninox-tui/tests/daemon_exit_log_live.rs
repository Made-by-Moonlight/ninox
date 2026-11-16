//! Scripted live check that a headless daemon says why it stopped, for the
//! stops it can see (SIGTERM) and the one it can't (SIGKILL, reported by the
//! next start). Launches the real binary as a scratch daemon, so it is
//! ignored by default:
//!
//!     cargo test -p ninox-tui --test daemon_exit_log_live -- --ignored --nocapture
//!
//! Everything is scratch: `HOME`, config, db, data dir, brain, ptyd socket
//! and a random port in 40000–60000 that is checked to be free first. Only
//! pids this test spawned are ever signalled.

use std::path::Path;
use std::time::Duration;

use ninox_core::lifecycle::probe::is_pid_alive;
use ninox_ui_shared::server_restart;

const BIN: &str = env!("CARGO_BIN_EXE_ninox-tui");

fn free_scratch_port() -> u16 {
    let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos();
    (0..200u32)
        .map(|i| 40000 + ((seed.wrapping_add(i.wrapping_mul(7919))) % 20000) as u16)
        .find(|p| std::net::TcpListener::bind(("127.0.0.1", *p)).is_ok())
        .expect("a free port in 40000-60000")
}

/// Only ever called with a pid this test spawned.
fn signal(name: &str, pid: u32) {
    assert!(std::process::Command::new("kill").arg(format!("-{name}")).arg(pid.to_string()).status().unwrap().success());
}

fn log(db: &Path) -> String {
    std::fs::read_to_string(ninox_core::daemon_log::log_path(db)).unwrap_or_default()
}

async fn wait_for(what: &str, db: &Path, mut done: impl FnMut() -> bool) {
    for _ in 0..300 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("{what}\n--- daemon.log ---\n{}", log(db));
}

#[test]
#[ignore = "launches scratch daemons"]
fn a_daemon_logs_why_it_stopped() {
    let home = tempfile::tempdir().unwrap();
    let short = tempfile::Builder::new().prefix("nxd").tempdir_in(std::env::temp_dir()).unwrap();
    for (k, _) in std::env::vars() {
        if k.starts_with("NINOX_") || k == "GITHUB_TOKEN" || k == "GH_TOKEN" || k == "TMUX" || k == "TMUX_PANE" {
            // SAFETY: single test in this binary, no other threads yet.
            unsafe { std::env::remove_var(k) };
        }
    }
    let port = free_scratch_port();
    assert!((40000..60000).contains(&port) && port != 8080);
    let db = home.path().join("data").join("ninox.db");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "port = {port}\norchestrator_root = \"{}\"\n\n[brain]\npath = \"{}\"\n",
            home.path().join("orch").display(),
            home.path().join("brain").display()
        ),
    )
    .unwrap();
    for (k, v) in [
        ("HOME", home.path().to_path_buf()),
        ("CLAUDE_CONFIG_DIR", home.path().join(".claude")),
        ("NINOX_CONFIG", home.path().join("config.toml")),
        ("NINOX_DATA_DIR", home.path().join("data")),
        ("NINOX_BRAIN", home.path().join("brain")),
        ("TMUX_TMPDIR", short.path().to_path_buf()),
        (ninox_ptyd::SOCKET_ENV, short.path().join("p.sock")),
    ] {
        // SAFETY: as above.
        unsafe { std::env::set_var(k, v) };
    }
    // SAFETY: as above. No RUST_LOG: the default filter is what's under test.
    unsafe { std::env::remove_var("RUST_LOG") };

    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let bin = Path::new(BIN);

        ninox_core::daemon::spawn_headless(port, bin, &db).unwrap();
        let mut info = None;
        for _ in 0..300 {
            info = server_restart::fetch_info(port).await;
            if info.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let first = info.expect("scratch daemon came up").pid;
        assert!(log(&db).contains("ninox daemon starting"), "default level logs the startup line:\n{}", log(&db));

        signal("TERM", first);
        wait_for("daemon exits on SIGTERM", &db, || !is_pid_alive(first)).await;
        wait_for("exit reason logged", &db, || log(&db).contains("daemon exiting: received SIGTERM")).await;
        assert!(!db.parent().unwrap().join("daemon.pid").exists(), "a logged exit clears the marker");

        ninox_core::daemon::spawn_headless(port, bin, &db).unwrap();
        let second = loop {
            if let Some(i) = server_restart::fetch_info(port).await {
                break i.pid;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        };
        signal("KILL", second);
        wait_for("daemon dies on SIGKILL", &db, || !is_pid_alive(second)).await;

        ninox_core::daemon::spawn_headless(port, bin, &db).unwrap();
        let third = loop {
            if let Some(i) = server_restart::fetch_info(port).await {
                break i.pid;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        };
        let text = log(&db);
        assert!(
            text.contains(&format!("previous daemon (pid {second}) ended without logging an exit")),
            "the next start must report the unlogged death:\n{text}"
        );
        signal("TERM", third);
        wait_for("last daemon exits", &db, || !is_pid_alive(third)).await;
        let lines = text.lines().filter(|l| l.contains("ninox daemon starting")).count();
        assert_eq!(lines, 3, "stderr and the file are the same log, so no line is doubled:\n{text}");
    });
}
