//! Shared harness: a real `ninox-ptyd` process on a tempdir socket.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ninox_ptyd::{PtydClient, ScreenSnapshot, SpawnSpec};

pub const BIN: &str = env!("CARGO_BIN_EXE_ninox-ptyd");

pub struct TestHost {
    pub dir: tempfile::TempDir,
    pub socket: PathBuf,
    pub checkpoints: PathBuf,
    pub pid: u32,
}

impl TestHost {
    pub async fn start() -> Self {
        Self::start_with(true).await
    }

    pub async fn start_with(checkpoints: bool) -> Self {
        // Short base path: macOS caps unix socket paths at 104 bytes.
        let dir = tempfile::Builder::new().prefix("ptyd").tempdir_in("/tmp").unwrap();
        let socket = dir.path().join("s.sock");
        let ck = dir.path().join("ck");
        let mut argv = vec![BIN.to_string(), "--socket".into(), socket.display().to_string()];
        if checkpoints {
            argv.extend(["--checkpoint-dir".into(), ck.display().to_string()]);
        } else {
            argv.push("--no-checkpoints".into());
        }
        let log = dir.path().join("ptyd.log");
        let c = PtydClient::connect_or_spawn(&socket, "test", &argv, Some(log), Duration::from_secs(10))
            .await
            .expect("host starts");
        let pid = c.host_identity().0;
        Self { dir, socket, checkpoints: ck, pid }
    }

    pub async fn client(&self) -> PtydClient {
        PtydClient::connect(&self.socket, "test").await.expect("connect")
    }

    pub fn log(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("ptyd.log")).unwrap_or_default()
    }
}

impl Drop for TestHost {
    fn drop(&mut self) {
        // Never leave a detached host behind, even when a test panics.
        unsafe {
            libc::kill(self.pid as i32, libc::SIGTERM);
        }
    }
}

pub fn spec(pane: &str, script: &str) -> SpawnSpec {
    SpawnSpec {
        pane: pane.into(),
        argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
        cwd: "/tmp".into(),
        env: vec![],
        env_remove: vec![],
        cols: 80,
        rows: 24,
    }
}

/// Poll the pane's screen until `pred` holds; panics with the last screen.
pub async fn wait_screen(c: &mut PtydClient, pane: &str, scrollback: usize, pred: impl Fn(&str) -> bool) -> ScreenSnapshot {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let s = c.screen(pane, scrollback).await.expect("screen");
        if pred(&s.to_plain_text()) {
            return s;
        }
        if Instant::now() > deadline {
            panic!("timed out waiting for screen; last:\n{}", s.to_plain_text());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

pub async fn wait_until<F, Fut>(what: &str, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f().await {
        if Instant::now() > deadline {
            panic!("timed out waiting for {what}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

pub fn pid_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

pub fn socket_exists(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}
