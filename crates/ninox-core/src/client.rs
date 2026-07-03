//! A hidden tmux client: `tmux attach` running on a PTY pair the app owns.
//! From tmux's perspective this is a normal terminal, so attach triggers a
//! full repaint, PTY resize drives window size (window-size latest), and
//! keyboard bytes written to the master are real terminal input.

use crate::events::{Engine, Event};
use crate::types::SessionId;
use anyhow::{Context, Result};
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use std::sync::Arc;
use tokio::sync::mpsc;

pub struct AttachedClient {
    input:  mpsc::UnboundedSender<Vec<u8>>,
    master: Box<dyn MasterPty + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
}

impl AttachedClient {
    /// Spawn `argv` (from `tmux::attach_args`) on a fresh PTY sized
    /// cols x rows. Output is emitted as `Event::ClientOutput`; exactly one
    /// `Event::ClientClosed` follows when the process exits.
    pub fn spawn(
        engine:     Arc<Engine>,
        session_id: SessionId,
        argv:       Vec<String>,
        cols:       u16,
        rows:       u16,
    ) -> Result<Self> {
        anyhow::ensure!(!argv.is_empty(), "attach argv must not be empty");
        let pair = native_pty_system()
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .context("openpty for attached client")?;

        let mut cmd = CommandBuilder::new(&argv[0]);
        cmd.args(&argv[1..]);
        cmd.env("TERM", "xterm-256color");
        let mut child = pair.slave.spawn_command(cmd).context("spawn tmux attach")?;
        let mut killer = child.clone_killer();
        drop(pair.slave);

        // Acquire both PTY handles before spawning any thread. Once a
        // thread exists it owns `child` and is the sole emitter of the
        // exactly-once `ClientClosed` event; if we spawned the reader
        // first and then failed to get the writer, a caller retry after
        // our Err would leave that thread running and orphan a second
        // reader on top of it. So: get both handles, and if either is
        // missing, kill+reap the child ourselves since no thread exists
        // yet to do it.
        let reader = pair.master.try_clone_reader();
        let writer = pair.master.take_writer();
        let (mut reader, mut writer) = match (reader, writer) {
            (Ok(r), Ok(w)) => (r, w),
            (r, w) => {
                // Never leak the attach process: no thread owns it yet.
                let _ = killer.kill();
                let _ = child.wait();
                r.context("clone PTY reader")?;
                w.context("take PTY writer")?;
                unreachable!("one of reader/writer must have errored");
            }
        };

        // Reader thread: PTY master → ClientOutput events. Blocking reads on
        // a dedicated thread; Engine::emit is sync so no runtime needed here.
        let engine_out = engine.clone();
        let sid = session_id.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match std::io::Read::read(&mut reader, &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => engine_out.emit(Event::ClientOutput {
                        session_id: sid.clone(),
                        bytes:      buf[..n].to_vec(),
                    }),
                }
            }
            let _ = child.wait();
            engine_out.emit(Event::ClientClosed { session_id: sid });
        });

        // Writer thread: mpsc → PTY master (real keyboard input path).
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        std::thread::spawn(move || {
            use std::io::Write;
            while let Some(bytes) = input_rx.blocking_recv() {
                if writer.write_all(&bytes).is_err() { break; }
                let _ = writer.flush();
            }
        });

        Ok(Self { input: input_tx, master: pair.master, killer })
    }

    pub fn write(&self, bytes: Vec<u8>) {
        let _ = self.input.send(bytes);
    }

    /// A cloneable sender for the input path — handed to the emulator's
    /// event proxy so query responses (DSR, DA, kitty) reach the PTY.
    pub fn input_sender(&self) -> mpsc::UnboundedSender<Vec<u8>> {
        self.input.clone()
    }

    /// Resize the client PTY; tmux follows via `window-size latest` and
    /// repaints — no explicit resize-window call needed.
    pub fn resize(&self, cols: u16, rows: u16) {
        let _ = self.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
    }
}

impl Drop for AttachedClient {
    fn drop(&mut self) {
        let _ = self.killer.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{events::Event, store::Store, tmux};
    use std::sync::Arc;
    use tempfile::tempdir;
    use tokio::time::{sleep, Duration};

    fn tmux_available() -> bool {
        std::process::Command::new("tmux").args(["-V"]).output()
            .map(|o| o.status.success()).unwrap_or(false)
    }

    fn unique_id() -> String {
        format!("ac-{}", std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_millis())
    }

    fn test_engine() -> Arc<crate::events::Engine> {
        let store = Arc::new(Store::open(tempdir().unwrap().keep().join("t.db")).unwrap());
        crate::events::Engine::new(store)
    }

    async fn collect_client_output(
        rx: &mut tokio::sync::broadcast::Receiver<Event>,
        session_id: &str,
        timeout_ms: u64,
    ) -> Vec<u8> {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        let mut all = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() { break; }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(Event::ClientOutput { session_id: sid, bytes })) if sid == session_id => {
                    all.extend_from_slice(&bytes);
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        all
    }

    #[tokio::test]
    async fn attach_repaints_and_round_trips_input() {
        if !tmux_available() { return; }
        let id = unique_id();
        let engine = test_engine();
        let mut rx = engine.subscribe();

        tmux::create_session(&id, "/tmp", "bash", &[]).await.unwrap();
        sleep(Duration::from_millis(300)).await;

        let argv = tmux::attach_args(&id).await;
        let client = AttachedClient::spawn(engine.clone(), id.clone(), argv, 100, 30).unwrap();

        // Attach must repaint the current screen (shell prompt) unprompted.
        let repaint = collect_client_output(&mut rx, &id, 2000).await;
        assert!(!repaint.is_empty(), "attach should trigger a full repaint");

        // Input written to the client PTY reaches the inner shell.
        client.write(b"echo attach-round-trip\r".to_vec());
        let out = collect_client_output(&mut rx, &id, 3000).await;
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("attach-round-trip"), "echo output missing: {text}");

        drop(client);
        tmux::kill_session(&id).await.unwrap();
    }

    #[tokio::test]
    async fn resize_drives_tmux_window_size() {
        if !tmux_available() { return; }
        let id = unique_id();
        let engine = test_engine();
        tmux::create_session(&id, "/tmp", "sleep 30", &[]).await.unwrap();
        sleep(Duration::from_millis(300)).await;

        let argv = tmux::attach_args(&id).await;
        let client = AttachedClient::spawn(engine.clone(), id.clone(), argv, 97, 41).unwrap();
        sleep(Duration::from_millis(500)).await;

        // window-size latest: the window follows the (only) client's PTY size.
        let out = tokio::process::Command::new("tmux")
            .args(["-L", "ninox", "display-message", "-p", "-t", &id, "#{window_width}x#{window_height}"])
            .output().await.unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "97x41");

        client.resize(80, 24);
        sleep(Duration::from_millis(500)).await;
        let out = tokio::process::Command::new("tmux")
            .args(["-L", "ninox", "display-message", "-p", "-t", &id, "#{window_width}x#{window_height}"])
            .output().await.unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "80x24");

        drop(client);
        tmux::kill_session(&id).await.unwrap();
    }

    #[tokio::test]
    async fn shift_enter_csi_u_reaches_kitty_enabled_app() {
        if !tmux_available() { return; }
        let id = unique_id();
        let engine = test_engine();
        let mut rx = engine.subscribe();

        // Inner app enables the kitty keyboard protocol (CSI > 1 u), then
        // echoes every byte it receives with escapes made visible. With
        // extended-keys always + csi-u format, tmux must forward our
        // Shift+Enter encoding to it intact.
        let cmd = r#"bash -c 'stty -echo -icanon; printf "\033[>1u"; cat -v'"#;
        tmux::create_session(&id, "/tmp", cmd, &[]).await.unwrap();
        sleep(Duration::from_millis(400)).await;

        let argv = tmux::attach_args(&id).await;
        let client = AttachedClient::spawn(engine.clone(), id.clone(), argv, 100, 30).unwrap();
        sleep(Duration::from_millis(400)).await;

        client.write(b"\x1b[13;2u".to_vec()); // Shift+Enter, CSI-u encoded
        let out = collect_client_output(&mut rx, &id, 3000).await;
        let text = String::from_utf8_lossy(&out);
        // cat -v renders ESC as ^[ — the CSI-u sequence must survive intact.
        assert!(text.contains("^[[13;2u"),
                "Shift+Enter did not reach the kitty-enabled app as CSI-u: {text}");

        drop(client);
        tmux::kill_session(&id).await.unwrap();
    }

    #[tokio::test]
    async fn client_closed_emitted_when_session_killed() {
        if !tmux_available() { return; }
        let id = unique_id();
        let engine = test_engine();
        let mut rx = engine.subscribe();
        tmux::create_session(&id, "/tmp", "sleep 30", &[]).await.unwrap();
        sleep(Duration::from_millis(300)).await;

        let argv = tmux::attach_args(&id).await;
        let _client = AttachedClient::spawn(engine.clone(), id.clone(), argv, 80, 24).unwrap();
        sleep(Duration::from_millis(300)).await;
        tmux::kill_session(&id).await.unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_millis(3000);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(!remaining.is_zero(), "ClientClosed never arrived");
            if let Ok(Ok(Event::ClientClosed { session_id })) =
                tokio::time::timeout(remaining, rx.recv()).await
            {
                if session_id == id { break; }
            }
        }
    }
}
