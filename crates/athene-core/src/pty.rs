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
pub async fn start_streaming(
    engine:     Arc<Engine>,
    session_id: SessionId,
    tmux_id:    &str,
) -> Result<()> {
    let path = fifo_path(&session_id);

    // Remove stale FIFO from a previous session with this ID.
    let _ = std::fs::remove_file(&path);

    // Create the FIFO.
    let status = tokio::process::Command::new("mkfifo")
        .arg(&path)
        .status()
        .await?;
    anyhow::ensure!(status.success(), "mkfifo {path} failed");

    // Open the FIFO for non-blocking read.  O_NONBLOCK means `open()` returns
    // immediately even though tmux hasn't opened the write end yet.
    let fifo_file = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)?
    };

    // Tell tmux to stream pane output into the FIFO.
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
        loop {
            let mut guard = match async_fd.readable().await {
                Ok(g) => g,
                Err(_) => break,
            };
            let result = guard.try_io(|inner| {
                use std::io::Read;
                inner.get_ref().read(&mut buf)
            });
            match result {
                Ok(Ok(0)) => break, // EOF: tmux session died / pipe-pane stopped
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
                Err(_would_block) => {} // guard cleared; wait for next readable()
            }
        }
        let _ = std::fs::remove_file(&path_out);
        tracing::info!("PTY stream ended for {sid_out}");
    });

    // --- Input task: mpsc channel → TTY write ---
    let tty_path = tmux::get_pane_tty(tmux_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no TTY found for tmux session {tmux_id}"))?;

    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    engine.register_pty_writer(session_id.clone(), input_tx).await;

    tokio::spawn(async move {
        while let Some(bytes) = input_rx.recv().await {
            let tty = tty_path.clone();
            // Open fresh each write: TTY file is tiny, handles are cheap,
            // and keeping one open risks SIGPIPE if the pane restarts.
            tokio::task::spawn_blocking(move || {
                use std::io::Write;
                use std::os::unix::fs::OpenOptionsExt;
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_NOCTTY)
                    .open(&tty)
                {
                    let _ = f.write_all(&bytes);
                }
            })
            .await
            .ok();
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
            Arc::new(Store::open(tempdir().unwrap().into_path().join("t.db")).unwrap());
        Engine::new(store)
    }

    #[tokio::test]
    async fn streaming_round_trip() {
        if !tmux_available() { return; }

        let id     = unique_id();
        let engine = test_engine();
        let mut rx = engine.subscribe();

        tmux::create_session(&id, "/tmp", "bash", &[]).await.unwrap();
        // Give bash a moment to start before piping.
        sleep(Duration::from_millis(300)).await;
        start_streaming(engine.clone(), id.clone(), &id).await.unwrap();

        // Send a command through the PTY writer.
        sleep(Duration::from_millis(200)).await;
        if let Some(w) = engine.get_pty_writer(&id).await {
            let _ = w.send(b"echo athene-test\n".to_vec());
        }

        // Wait up to 3 s for TerminalOutput containing "athene-test".
        let found = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Ok(crate::events::Event::TerminalOutput { session_id, bytes }) =
                    rx.recv().await
                {
                    if session_id == id
                        && bytes.windows(11).any(|w| w == b"athene-test")
                    {
                        return true;
                    }
                }
            }
        })
        .await
        .unwrap_or(false);

        tmux::kill_session(&id).await.unwrap();
        assert!(found, "never received 'athene-test' in TerminalOutput");
    }
}
