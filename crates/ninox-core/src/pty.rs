use crate::{
    events::{Engine, Event},
    tmux,
    types::SessionId,
};
use anyhow::Result;
use std::sync::Arc;
use tokio::sync::mpsc;

fn fifo_path(session_id: &str) -> String {
    format!("/tmp/ninox-{session_id}.fifo")
}

/// Wire up output streaming and input for an already-running session on
/// whichever runtime hosts it (see [`crate::runtime`]). The browser
/// WebSocket route and background-session monitoring consume the resulting
/// `Event::TerminalOutput`s and PTY writer.
pub async fn start_streaming(
    engine:     Arc<Engine>,
    session_id: SessionId,
    pane_id:    &str,
) -> Result<()> {
    crate::runtime::start_streaming(engine, session_id, pane_id).await
}

/// [`start_streaming`] for a tmux session:
///
/// 1. Creates a FIFO at `/tmp/ninox-{session_id}.fifo`.
/// 2. Opens the FIFO non-blocking for reading so `open()` returns immediately.
/// 3. Tells tmux to pipe pane output to the FIFO via `cat`.
/// 4. Spawns a task that reads the FIFO with `AsyncFd` and emits `Event::TerminalOutput`.
/// 5. Opens the pane's TTY device for writing.
/// 6. Spawns a task that forwards bytes from the mpsc input channel to the TTY.
/// 7. Registers the input sender with the engine so the WebSocket terminal can use it.
///
/// The native app no longer renders from this tap — `AttachedClient` (a
/// hidden tmux client over a real PTY) is the source of truth for on-screen
/// sessions and repaints the whole screen on attach. This FIFO/pipe-pane tap
/// now exists solely for the browser WebSocket route and background-session
/// monitoring.
pub(crate) async fn start_tmux_streaming(
    engine:     Arc<Engine>,
    session_id: SessionId,
    tmux_id:    &str,
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
            let buf = format!("ath-in-{}-{counter}", tmux_id_input);
            let tmp = format!("/tmp/ath-in-{}-{counter}.tmp", tmux_id_input);
            counter += 1;

            let _ = tmux::paste_buffer(&tmux_id_input, &buf, &tmp, &bytes).await;
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
        // Millis alone collide when parallel test threads start within the
        // same tick, producing duplicate tmux session names; a per-process
        // counter guarantees uniqueness regardless of clock resolution (see
        // the same fix in tmux.rs's and client.rs's unique_id).
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!(
            "pt-{}-{}-{n}",
            std::process::id(),
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

    /// Bounds a wait that should take well under a second; a loaded
    /// machine only makes it slower, so this only catches a hang.
    const PTY_WAIT: Duration = Duration::from_secs(30);

    /// Streamed output for `session_id` up to the first occurrence of
    /// `needle`, or everything seen before [`PTY_WAIT`] ran out.
    async fn collect_until(
        rx: &mut tokio::sync::broadcast::Receiver<crate::events::Event>,
        session_id: &str,
        needle: &[u8],
    ) -> Vec<u8> {
        let deadline = tokio::time::Instant::now() + PTY_WAIT;
        let mut all = Vec::new();
        while !all.windows(needle.len()).any(|w| w == needle) {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Ok(crate::events::Event::TerminalOutput { session_id: sid, bytes })) if sid == session_id => {
                    all.extend_from_slice(&bytes);
                }
                Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) | Err(_) => break,
            }
        }
        all
    }

    /// The pane's input writer, which `start_streaming` registers as it
    /// comes up.
    async fn pty_writer(engine: &Engine, session_id: &str) -> tokio::sync::mpsc::UnboundedSender<Vec<u8>> {
        let deadline = tokio::time::Instant::now() + PTY_WAIT;
        loop {
            if let Some(w) = engine.get_pty_writer(session_id).await {
                return w;
            }
            assert!(tokio::time::Instant::now() < deadline, "no pty writer for {session_id} within {PTY_WAIT:?}");
            sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn streaming_round_trip() {
        if !tmux_available() { return; }

        let id     = unique_id();
        let engine = test_engine();
        let mut rx = engine.subscribe();

        tmux::create_session(&id, "/tmp", "bash", &[]).await.unwrap();
        start_streaming(engine.clone(), id.clone(), &id).await.unwrap();
        // Only running the command prints `ninox-2`; the terminal's echo of
        // what was typed shows `ninox-$((1+1))`.
        pty_writer(&engine, &id).await.send(b"echo ninox-$((1+1))\r".to_vec()).unwrap();

        let out = collect_until(&mut rx, &id, b"ninox-2").await;
        tmux::kill_session(&id).await.unwrap();
        assert!(out.windows(7).any(|w| w == b"ninox-2"), "output: {:?}", String::from_utf8_lossy(&out));
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
        start_streaming(engine.clone(), id.clone(), &id).await.unwrap();

        // "1\r" selects option 1 ("yes"); bash reads it once `select` asks.
        pty_writer(&engine, &id).await.send(b"1\r".to_vec()).unwrap();

        let out = collect_until(&mut rx, &id, b"chose:yes").await;
        tmux::kill_session(&id).await.unwrap();
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("chose:yes"), "TUI selection via paste-buffer failed. output: {text}");
    }
}
