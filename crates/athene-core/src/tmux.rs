use anyhow::{Context, Result};
use tokio::process::Command;

/// Metadata about a running tmux session from `list-sessions`.
#[derive(Debug, Clone)]
pub struct TmuxSession {
    pub id:         String,
    pub created_ms: i64,
    pub pid:        Option<u32>,
    pub tty:        Option<String>,
}

/// Run a tmux subcommand and return trimmed stdout.
async fn run(args: &[&str]) -> Result<String> {
    let out = Command::new("tmux")
        .args(args)
        .kill_on_drop(true)
        .output()
        .await
        .context("tmux not found — install tmux (brew install tmux / apt install tmux)")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("tmux {:?} failed: {}", args, stderr.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// Run tmux; swallow errors and return empty string on failure.
/// Logs warnings for debugging; does not propagate errors.
async fn run_best_effort(args: &[&str]) -> String {
    match run(args).await {
        Ok(result) => result,
        Err(e) => {
            tracing::warn!("tmux {:?} failed (ignored): {}", args, e);
            String::new()
        }
    }
}

/// Shell-quote a string to prevent injection in tmux commands.
/// Wraps the string in single quotes and escapes interior single quotes.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Create a detached tmux session.  Kills a stale session with the same name
/// if one exists, then hides the status bar so the terminal widget is clean.
pub async fn create_session(
    id:        &str,
    workspace: &str,
    cmd:       &str,
    env:       &[(&str, &str)],
) -> Result<()> {
    // Build -e KEY=VALUE pairs
    let env_pairs: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let mut extra: Vec<&str> = Vec::new();
    for pair in &env_pairs {
        extra.push("-e");
        extra.push(pair.as_str());
    }

    let mut base = vec!["new-session", "-d", "-s", id, "-c", workspace];
    base.extend_from_slice(&extra);
    base.push(cmd);

    for attempt in 0..2u8 {
        match run(&base).await {
            Ok(_) => break,
            Err(e) if attempt == 0 && e.to_string().contains("duplicate session") => {
                run_best_effort(&["kill-session", "-t", id]).await;
            }
            Err(e) => return Err(e),
        }
    }

    // Best-effort: hide the tmux status bar so the terminal widget isn't cluttered.
    if let Err(e) = run(&["set-option", "-t", id, "status", "off"]).await {
        tracing::warn!("failed to hide tmux status bar: {}", e);
    }
    Ok(())
}

/// Kill a tmux session.  Succeeds even if the session doesn't exist.
pub async fn kill_session(id: &str) -> Result<()> {
    match run(&["kill-session", "-t", id]).await {
        Ok(_) => Ok(()),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("no server running")
                || msg.contains("can't find session")
                || msg.contains("session not found")
                || msg.contains("no sessions")
            {
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

/// Returns `true` if a tmux session with this name is currently running.
pub async fn has_session(id: &str) -> bool {
    run(&["has-session", "-t", id]).await.is_ok()
}

/// List every live tmux session.
pub async fn list_sessions() -> Result<Vec<TmuxSession>> {
    let raw = run_best_effort(&[
        "list-sessions",
        "-F",
        "#{session_name}\t#{session_created}\t#{pane_pid}\t#{pane_tty}",
    ])
    .await;
    Ok(raw
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|line| {
            let mut cols = line.splitn(4, '\t');
            let id  = cols.next()?.to_string();
            let sec = cols.next().and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
            let pid = cols.next().and_then(|s| s.parse::<u32>().ok());
            let tty = cols.next().map(str::to_string).filter(|s| !s.is_empty());
            Some(TmuxSession { id, created_ms: sec * 1000, pid, tty })
        })
        .collect())
}

/// Return the tty device path (e.g. `/dev/ttys003`) for the session's active pane.
pub async fn get_pane_tty(id: &str) -> Result<Option<String>> {
    let out = run(&["list-panes", "-t", id, "-F", "#{pane_tty}"]).await?;
    Ok(out
        .lines()
        .next()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty()))
}

/// Start piping pane output to `dest_path` (regular file, not FIFO).
/// The flag `-o` means "only start a new pipe if none is running".
pub async fn pipe_pane(id: &str, dest_path: &str) -> Result<()> {
    run(&["pipe-pane", "-o", "-t", id, &format!("cat > {}", shell_quote(dest_path))]).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmux_available() -> bool {
        std::process::Command::new("tmux")
            .args(["-V"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn unique_id() -> String {
        format!(
            "test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        )
    }

    #[tokio::test]
    async fn create_and_has_and_kill() {
        if !tmux_available() { return; }
        let id = unique_id();
        create_session(&id, "/tmp", "sleep 30", &[]).await.unwrap();
        assert!(has_session(&id).await);
        kill_session(&id).await.unwrap();
        assert!(!has_session(&id).await);
    }

    #[tokio::test]
    async fn list_includes_created() {
        if !tmux_available() { return; }
        let id = unique_id();
        create_session(&id, "/tmp", "sleep 30", &[]).await.unwrap();
        let sessions = list_sessions().await.unwrap();
        assert!(sessions.iter().any(|s| s.id == id));
        kill_session(&id).await.unwrap();
    }

    #[tokio::test]
    async fn get_pane_tty_returns_dev_path() {
        if !tmux_available() { return; }
        let id = unique_id();
        create_session(&id, "/tmp", "sleep 30", &[]).await.unwrap();
        let tty = get_pane_tty(&id).await.unwrap();
        assert!(tty.map(|t| t.starts_with("/dev/")).unwrap_or(false));
        kill_session(&id).await.unwrap();
    }
}
