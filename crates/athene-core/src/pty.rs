use crate::{
    events::{Engine, Event},
    tmux,
    types::SessionId,
};
use anyhow::Result;
use std::sync::Arc;
use tokio::sync::mpsc;

fn fifo_path(session_id: &str) -> String {
    format!("/tmp/athene-{session_id}.fifo")
}

/// Wire up PTY streaming for an already-running tmux session:
///
/// 1. Creates a FIFO at `/tmp/athene-{session_id}.fifo`.
/// 2. Opens the FIFO non-blocking for reading so `open()` returns immediately.
/// 3. Tells tmux to pipe pane output to the FIFO via `cat`.
/// 4. Spawns a task that reads the FIFO with `AsyncFd` and emits `Event::TerminalOutput`.
/// 5. Opens the pane's TTY device for writing.
/// 6. Spawns a task that forwards bytes from the mpsc input channel to the TTY.
/// 7. Registers the input sender with the engine so the WebSocket terminal can use it.
///
/// Wire up PTY streaming for an already-running tmux session.
///
/// `cols` and `rows` must match the caller's `TerminalState` dimensions.
/// The tmux session is resized to these dimensions before `capture_pane` so
/// the captured escape sequences reference the same column positions as the
/// TerminalState grid — mismatches cause text to appear at wrong horizontal
/// offsets.
pub async fn start_streaming(
    engine:     Arc<Engine>,
    session_id: SessionId,
    tmux_id:    &str,
    cols:       u16,
    rows:       u16,
) -> Result<()> {
    // Cancel any previous FIFO reader for this session before setting up a new
    // one.  This prevents the old reader from emitting stale events after the
    // TerminalState has been cleared, which caused the garbled-terminal bug on
    // every re-navigation.
    let cancel_rx = engine.register_stream(session_id.clone()).await;

    // ── FIFO setup ────────────────────────────────────────────────────────────
    let path = fifo_path(&session_id);
    let _ = std::fs::remove_file(&path);

    let status = tokio::process::Command::new("mkfifo")
        .arg(&path)
        .status()
        .await?;
    anyhow::ensure!(status.success(), "mkfifo {path} failed");

    let fifo_file = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)?
    };

    tmux::pipe_pane(tmux_id, &path).await?;

    // ── Force SIGWINCH via bounce resize ──────────────────────────────────────
    // Unlike Zed (which owns a direct PTY stream and never needs a snapshot),
    // we reconnect to an already-running tmux session.  We must force a full
    // repaint so the FIFO receives a clean screen — no capture_pane needed.
    //
    // A "bounce" resize (cols→cols-1→cols) guarantees SIGWINCH even when the
    // terminal was already at `cols` rows.  The process redraws twice and
    // settles at the correct size.  The FIFO stream is the single source of
    // truth for the TerminalState — no snapshot races, no ordering conflicts.
    let bounce = cols.saturating_sub(1).max(2);
    let _ = tmux::resize_window(tmux_id, bounce, rows).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    let _ = tmux::resize_window(tmux_id, cols, rows).await;

    // --- Output task: FIFO → Event::TerminalOutput ---
    let engine_out = engine.clone();
    let sid_out    = session_id.clone();
    let path_out   = path.clone();
    tokio::spawn(async move {
        use tokio::io::{unix::AsyncFd, Interest};

        let async_fd =
            match AsyncFd::with_interest(fifo_file, Interest::READABLE) {
                Ok(fd) => fd,
                Err(e) => {
                    tracing::error!("AsyncFd setup for {sid_out}: {e}");
                    return;
                }
            };

        let mut buf = vec![0u8; 4096];
        // Pin cancel_rx so we can use it in select! across iterations.
        tokio::pin!(cancel_rx);
        loop {
            tokio::select! {
                biased;
                // Stop immediately when a new start_streaming is called for this session.
                _ = &mut cancel_rx => break,
                guard_result = async_fd.readable() => {
                    let mut guard = match guard_result {
                        Ok(g) => g,
                        Err(_) => break,
                    };
                    let result = guard.try_io(|inner| {
                        use std::io::Read;
                        inner.get_ref().read(&mut buf)
                    });
                    match result {
                        Ok(Ok(0)) => break,
                        Ok(Ok(n)) => {
                            engine_out.emit(Event::TerminalOutput {
                                session_id: sid_out.clone(),
                                bytes:      buf[..n].to_vec(),
                            });
                        }
                        Ok(Err(e)) => {
                            tracing::error!("FIFO read {sid_out}: {e}");
                            break;
                        }
                        Err(_would_block) => {}
                    }
                }
            }
        }
        let _ = std::fs::remove_file(&path_out);
        tracing::info!("PTY stream ended for {sid_out}");
    });

    // --- Input task: mpsc channel → tmux paste-buffer (master PTY path) ---
    // Writing to the slave TTY sends data as application OUTPUT, not as INPUT.
    // To send INPUT to the process running in the pane we must go through
    // tmux's master PTY fd, which we can't open directly.  `paste-buffer`
    // is the supported tmux mechanism: it writes bytes to the master fd
    // exactly as if the user typed them.  Two tmux commands are issued in one
    // invocation via `\;` to keep subprocess overhead to a single fork.
    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    engine.register_pty_writer(session_id.clone(), input_tx).await;

    let tmux_id_input = tmux_id.to_string();
    tokio::spawn(async move {
        let mut counter = 0u64;
        while let Some(bytes) = input_rx.recv().await {
            let buf  = format!("ath-in-{}-{counter}", &tmux_id_input);
            let tmp  = format!("/tmp/ath-in-{}-{counter}.tmp", &tmux_id_input);
            counter += 1;

            if std::fs::write(&tmp, &bytes).is_err() { continue; }

            let _ = tokio::process::Command::new("tmux")
                .args(["load-buffer", "-b", &buf, &tmp, ";",
                       "paste-buffer", "-b", &buf, "-t", &tmux_id_input, "-d"])
                .kill_on_drop(true)
                .output()
                .await;

            let _ = std::fs::remove_file(&tmp);
        }
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{events::Engine, store::Store, tmux};
    use std::sync::Arc;
    use tempfile::tempdir;
    use tokio::time::{sleep, Duration};

    fn tmux_available() -> bool {
        std::process::Command::new("tmux")
            .args(["-V"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn unique_id() -> String {
        format!(
            "pt-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        )
    }

    fn test_engine() -> Arc<Engine> {
        let store =
            Arc::new(Store::open(tempdir().unwrap().keep().join("t.db")).unwrap());
        Engine::new(store)
    }

    async fn collect_output(
        rx: &mut tokio::sync::broadcast::Receiver<crate::events::Event>,
        session_id: &str,
        timeout_ms: u64,
    ) -> Vec<u8> {
        let sid = session_id.to_string();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        let mut all = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() { break; }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(crate::events::Event::TerminalOutput { session_id, bytes }))
                    if session_id == sid =>
                {
                    all.extend_from_slice(&bytes);
                }
                Ok(_) => {}
                Err(_) => break, // deadline reached
            }
        }
        all
    }

    #[tokio::test]
    async fn streaming_round_trip() {
        if !tmux_available() { return; }

        let id     = unique_id();
        let engine = test_engine();
        let mut rx = engine.subscribe();

        tmux::create_session(&id, "/tmp", "bash", &[]).await.unwrap();
        sleep(Duration::from_millis(300)).await;
        start_streaming(engine.clone(), id.clone(), &id, 80, 24).await.unwrap();
        sleep(Duration::from_millis(200)).await;

        if let Some(w) = engine.get_pty_writer(&id).await {
            let _ = w.send(b"echo athene-test\r".to_vec());
        }

        let out = collect_output(&mut rx, &id, 3000).await;
        tmux::kill_session(&id).await.unwrap();
        assert!(out.windows(11).any(|w| w == b"athene-test"), "output: {:?}", String::from_utf8_lossy(&out));
    }

    /// Verifies that input sent via the PTY writer reaches the running process
    /// and causes visible output — specifically testing the paste-buffer path
    /// used for TUI interaction (Enter, arrow keys, selection menus).
    #[tokio::test]
    async fn tui_input_interaction() {
        if !tmux_available() { return; }

        let id     = unique_id();
        let engine = test_engine();
        let mut rx = engine.subscribe();

        // Run a simple bash select menu to simulate TUI interaction.
        // `select` displays numbered options and reads a number from stdin.
        let cmd = "bash -c 'select x in yes no; do echo \"chose:$x\"; break; done'";
        tmux::create_session(&id, "/tmp", cmd, &[]).await.unwrap();
        sleep(Duration::from_millis(400)).await;
        start_streaming(engine.clone(), id.clone(), &id, 80, 24).await.unwrap();
        sleep(Duration::from_millis(300)).await;

        // Send "1\r" to select option 1 ("yes")
        if let Some(w) = engine.get_pty_writer(&id).await {
            let _ = w.send(b"1\r".to_vec());
        }

        let out = collect_output(&mut rx, &id, 4000).await;
        tmux::kill_session(&id).await.unwrap();
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("chose:yes"), "TUI selection via paste-buffer failed. output: {text}");
    }
}
