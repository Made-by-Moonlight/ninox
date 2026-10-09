//! `attach` bridge on a PTY owned by the test, the way the Iced app runs
//! `ninox pane attach` on its own PTY and reads the output.

mod common;

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};

struct Bridge {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    output: Arc<Mutex<Vec<u8>>>,
}

fn start_bridge(socket: &std::path::Path, pane: &str, extra: &[&str], cols: u16, rows: u16) -> Bridge {
    let pair = native_pty_system().openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }).unwrap();
    let mut cmd = CommandBuilder::new(BIN);
    cmd.args(["attach", "--socket", socket.to_str().unwrap()]);
    cmd.args(extra);
    cmd.arg(pane);
    let child = pair.slave.spawn_command(cmd).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let output = Arc::new(Mutex::new(Vec::new()));
    let out = Arc::clone(&output);
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            out.lock().unwrap().extend_from_slice(&buf[..n]);
        }
    });
    let writer = pair.master.take_writer().unwrap();
    Bridge { child, master: pair.master, writer, output }
}

impl Bridge {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).to_string()
    }

    async fn wait_output(&self, needle: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !self.text().contains(needle) {
            assert!(std::time::Instant::now() < deadline, "bridge output lacks {needle:?}: {:?}", self.text());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn wait_exit(&mut self) -> u32 {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.exit_code();
            }
            assert!(std::time::Instant::now() < deadline, "bridge did not exit");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

#[tokio::test]
async fn bridge_repaints_forwards_input_resizes_and_detaches() {
    let h = TestHost::start().await;
    let mut c = h.client().await;
    c.spawn(spec("a", "printf 'greeting\\n'; exec cat")).await.unwrap();
    wait_screen(&mut c, "a", 0, |t| t.contains("greeting")).await;

    let mut b = start_bridge(&h.socket, "a", &[], 100, 30);
    b.wait_output("greeting").await;
    // Alternate screen entered for an interactive (detachable) session.
    assert!(b.text().contains("\x1b[?1049h"));
    // Initial size propagated.
    wait_until("initial resize", || async {
        let mut c = ninox_ptyd::PtydClient::connect(&h.socket, "t").await.unwrap();
        let i = c.info("a").await.unwrap().unwrap();
        (i.cols, i.rows) == (100, 30)
    })
    .await;

    // Input goes through raw mode untouched (cat echoes it back).
    b.writer.write_all(b"typed-input\r").unwrap();
    wait_screen(&mut c, "a", 0, |t| t.contains("typed-input")).await;
    b.wait_output("typed-input").await;

    // Resizing the bridge's PTY follows through to the pane.
    b.master.resize(PtySize { rows: 20, cols: 70, pixel_width: 0, pixel_height: 0 }).unwrap();
    wait_until("resize after SIGWINCH", || async {
        let mut c = ninox_ptyd::PtydClient::connect(&h.socket, "t").await.unwrap();
        let i = c.info("a").await.unwrap().unwrap();
        (i.cols, i.rows) == (70, 20)
    })
    .await;

    // Detach chord: Ctrl-Space (NUL), then d.
    b.writer.write_all(b"\0d").unwrap();
    assert_eq!(b.wait_exit().await, 0);
    let out = b.text();
    assert!(out.contains("\x1b[?2004l") && out.contains("\x1b[?1049l"), "modes reset on detach");
    // The pane is untouched by the detach.
    assert!(c.info("a").await.unwrap().unwrap().alive);
}

#[tokio::test]
async fn embedded_bridge_ends_when_pane_exits() {
    let h = TestHost::start().await;
    let mut c = h.client().await;
    c.spawn(spec("e", "echo embedded; read x; exit 5")).await.unwrap();
    wait_screen(&mut c, "e", 0, |t| t.contains("embedded")).await;

    let mut b = start_bridge(&h.socket, "e", &["--no-detach"], 80, 24);
    b.wait_output("embedded").await;
    assert!(!b.text().contains("\x1b[?1049h"), "embedded bridge leaves the screen choice to its host");
    // With detaching disabled, NUL is just input.
    b.writer.write_all(b"\0d\r").unwrap();
    assert_eq!(b.wait_exit().await, 105, "PaneExited(5)");
}

#[tokio::test]
async fn bridge_follows_the_pane_through_a_live_upgrade() {
    let h = TestHost::start().await;
    let mut c = h.client().await;
    c.spawn(spec("u", "stty -echo; echo before-upgrade; exec cat")).await.unwrap();
    wait_screen(&mut c, "u", 0, |t| t.contains("before-upgrade")).await;
    let old_pid = c.host_identity().0;
    drop(c);

    let mut b = start_bridge(&h.socket, "u", &["--no-detach"], 80, 24);
    b.wait_output("before-upgrade").await;
    let mut successor = std::process::Command::new(BIN)
        .args(["--socket", h.socket.to_str().unwrap(), "--no-checkpoints", "--takeover"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    wait_until("old host exits", || async { !pid_alive(old_pid) }).await;

    b.writer.write_all(b"after-upgrade\r").unwrap();
    b.wait_output("after-upgrade").await;
    assert!(b.child.try_wait().unwrap().is_none(), "bridge still attached: {:?}", b.text());

    let mut c = h.client().await;
    assert_eq!(c.host_identity().0, successor.id());
    c.shutdown().await.unwrap();
    let _ = successor.wait();
    assert_ne!(b.wait_exit().await, 0);
}

#[tokio::test]
async fn attach_to_unknown_pane_fails() {
    let h = TestHost::start().await;
    let mut b = start_bridge(&h.socket, "ghost", &[], 80, 24);
    assert_eq!(b.wait_exit().await, 1);
    b.wait_output("NotFound").await;
}
