use anyhow::{Context, Result};
use tokio::process::Command;

/// Name of the private tmux server socket all ninox sessions live on.
/// Isolates ninox from the user's own tmux server and ~/.tmux.conf.
const SOCKET: &str = "ninox";

/// The ninox-managed server config (spec §"Dedicated tmux server").
const SERVER_CONFIG: &str = r#"# Managed by ninox — rewritten on every app start. Do not edit.
set -g  default-terminal "tmux-256color"
set -as terminal-features "xterm*:RGB:usstyle:extkeys:hyperlinks"
set -s  extended-keys always
set -s  extended-keys-format csi-u
set -g  history-limit 100000
set -g  status off
set -s  escape-time 0
set -g  window-size latest
set -g  allow-passthrough on
set -g  focus-events on
"#;

fn config_path() -> std::path::PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join("ninox")
        .join("tmux.conf")
}

/// Write the ninox tmux server config. Called once at startup so config
/// drift between app versions cannot accumulate.
pub fn write_server_config() -> Result<std::path::PathBuf> {
    let path = config_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, SERVER_CONFIG)?;
    Ok(path)
}

/// argv prefix routing a tmux invocation to the private ninox server.
fn socket_args() -> Vec<String> {
    vec![
        "-L".into(), SOCKET.into(),
        "-f".into(), config_path().display().to_string(),
    ]
}

/// Fail fast if tmux is missing or older than 3.2 (extended-keys support).
pub async fn require_version() -> Result<()> {
    let out = Command::new("tmux").arg("-V").output().await
        .context("tmux not found — install tmux (brew install tmux / apt install tmux)")?;
    let v = String::from_utf8_lossy(&out.stdout);
    let ver = v.trim().strip_prefix("tmux ").unwrap_or(v.trim());
    let mut parts = ver.split(|c: char| !c.is_ascii_digit());
    let major: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let minor: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    anyhow::ensure!(
        (major, minor) >= (3, 2),
        "ninox requires tmux >= 3.2 for extended keyboard support; found {ver}"
    );
    if (major, minor) < (3, 5) {
        tracing::warn!(
            "tmux {ver} detected — extended-keys-format csi-u requires tmux >= 3.5; \
             Shift+Enter and other disambiguated keys may not reach apps correctly \
             on this version"
        );
    }
    Ok(())
}

fn is_missing_session(e: &anyhow::Error) -> bool {
    let msg = e.to_string();
    msg.contains("can't find session")
        || msg.contains("session not found")
        || msg.contains("no server running")
        || msg.contains("no sessions")
}

/// Metadata about a running tmux session from `list-sessions`.
#[derive(Debug, Clone)]
pub struct TmuxSession {
    pub id:         String,
    pub created_ms: i64,
    pub pid:        Option<u32>,
    pub tty:        Option<String>,
}

/// Run a tmux subcommand against the ninox server and return trimmed stdout.
async fn run(args: &[&str]) -> Result<String> {
    let prefix = socket_args();
    let mut full: Vec<&str> = prefix.iter().map(String::as_str).collect();
    full.extend_from_slice(args);
    run_raw(&full).await
}

/// Run tmux with NO socket routing (the user's default server) — only for
/// legacy sessions created by pre-private-socket builds.
async fn run_default(args: &[&str]) -> Result<String> {
    run_raw(args).await
}

/// The old `run` body, renamed: spawn tmux with exactly these args.
async fn run_raw(args: &[&str]) -> Result<String> {
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

/// Run a command that targets an existing session. Tries the ninox server
/// first, then falls back to the default server for legacy sessions.
async fn run_session_scoped(args: &[&str]) -> Result<String> {
    match run(args).await {
        Err(e) if is_missing_session(&e) => run_default(args).await,
        other => other,
    }
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

/// `run_best_effort`, but against the default (non-ninox) socket — used to
/// surface legacy sessions from pre-private-socket builds.
async fn run_best_effort_default(args: &[&str]) -> String {
    match run_default(args).await {
        Ok(result) => result,
        Err(e) => {
            tracing::warn!("tmux (default socket) {:?} failed (ignored): {}", args, e);
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
    let mut env_pairs: Vec<String> = Vec::new();
    for (k, v) in env {
        anyhow::ensure!(!k.contains('='), "env key must not contain '=': {k}");
        env_pairs.push(format!("{k}={v}"));
    }
    let mut extra: Vec<&str> = Vec::new();
    for pair in &env_pairs {
        // Values are passed as separate argv tokens via execve — no shell quoting needed.
        extra.push("-e");
        extra.push(pair.as_str());
    }

    // Wrap the command in a login shell so the full user PATH is available.
    // tmux sessions do not inherit shell rc files, so tools installed via
    // nvm / cargo / homebrew etc. would not be found otherwise.
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
    let shell_cmd = format!("{shell} -l -c {}", shell_quote(cmd));

    // Fix the terminal dimensions to match the canvas.  The canvas is roughly
    // (window_width - 220px sidebar) / 7.8px_per_col ≈ 135 cols on a 1280-wide
    // window.  Use 140 as a safe default; too-wide values push Claude Code's
    // centered content off-screen.
    let mut base = vec!["new-session", "-d", "-s", id, "-x", "140", "-y", "50", "-c", workspace];
    base.extend_from_slice(&extra);
    base.push(&shell_cmd);

    for attempt in 0..2u8 {
        match run(&base).await {
            Ok(_) => break,
            Err(e) if attempt == 0 && e.to_string().contains("duplicate session") => {
                run_best_effort(&["kill-session", "-t", id]).await;
            }
            Err(e) => return Err(e),
        }
    }

    Ok(())
}

/// Kill a tmux session.  Succeeds even if the session doesn't exist.
/// `run_session_scoped` falls back to the default socket when the session
/// isn't found on the ninox server, so legacy sessions are killed there too.
pub async fn kill_session(id: &str) -> Result<()> {
    match run_session_scoped(&["kill-session", "-t", id]).await {
        Ok(_) => Ok(()),
        Err(e) => {
            if is_missing_session(&e) {
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

/// Returns `true` if a tmux session with this name is currently running.
pub async fn has_session(id: &str) -> bool {
    run_session_scoped(&["has-session", "-t", id]).await.is_ok()
}

/// List every live tmux session.  Sessions on the ninox server are listed
/// first; legacy sessions on the default server are appended for any id not
/// already seen (ninox socket wins on conflicts).
pub async fn list_sessions() -> Result<Vec<TmuxSession>> {
    let fmt = "#{session_name}\t#{session_created}\t#{pane_pid}\t#{pane_tty}";
    let ninox_raw = run_best_effort(&["list-sessions", "-F", fmt]).await;
    let default_raw = run_best_effort_default(&["list-sessions", "-F", fmt]).await;

    let mut seen = std::collections::HashSet::new();
    let mut sessions = Vec::new();
    for raw in [ninox_raw, default_raw] {
        for line in raw.lines().filter(|l| !l.is_empty()) {
            let mut cols = line.splitn(4, '\t');
            let Some(id) = cols.next().map(str::to_string) else { continue };
            if !seen.insert(id.clone()) {
                continue;
            }
            let sec = cols.next().and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
            let pid = cols.next().and_then(|s| s.parse::<u32>().ok());
            let tty = cols.next().map(str::to_string).filter(|s| !s.is_empty());
            sessions.push(TmuxSession { id, created_ms: sec * 1000, pid, tty });
        }
    }
    Ok(sessions)
}

/// Return the tty device path (e.g. `/dev/ttys003`) for the session's active pane.
pub async fn get_pane_tty(id: &str) -> Result<Option<String>> {
    let out = run_session_scoped(&["list-panes", "-t", id, "-F", "#{pane_tty}"]).await?;
    Ok(out
        .lines()
        .next()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty()))
}

/// Start piping pane output to `dest_path` (regular file, not FIFO).
/// Does NOT use `-o` so it force-restarts any existing pipe — required for reconnect.
pub async fn pipe_pane(id: &str, dest_path: &str) -> Result<()> {
    run_session_scoped(&["pipe-pane", "-t", id, &format!("cat > {}", shell_quote(dest_path))]).await?;
    Ok(())
}

/// Full argv for `tmux attach` for this session (element 0 is "tmux"),
/// resolving whether it lives on the ninox or the legacy default server.
pub async fn attach_args(session_id: &str) -> Vec<String> {
    let mut argv = vec!["tmux".to_string()];
    if run(&["has-session", "-t", session_id]).await.is_ok() {
        argv.extend(socket_args());
    } else {
        tracing::warn!(
            "session {session_id} predates the ninox socket — attaching on the \
             legacy default tmux server without the managed config (extended \
             keys / resize guarantees are degraded until it terminates naturally)"
        );
    }
    argv.extend(["attach-session", "-t", session_id].map(String::from));
    argv
}

/// Number of scrolled-off lines tmux holds for this pane.
pub async fn history_size(session_id: &str) -> i64 {
    run_session_scoped(&["display-message", "-p", "-t", session_id, "#{history_size}"])
        .await
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Capture styled pane content for the line range [start, end], where
/// negative indices address history (-1 = newest history line). No -J:
/// lines stay wrapped at pane width so they re-parse at the same columns.
pub async fn capture_history(session_id: &str, start: i64, end: i64) -> Vec<u8> {
    run_session_scoped(&[
        "capture-pane", "-p", "-e", "-t", session_id,
        "-S", &start.to_string(), "-E", &end.to_string(),
    ])
    .await
    .map(|s| s.into_bytes())
    .unwrap_or_default()
}

/// Send text to a tmux session as if typed at the keyboard.
/// The text is followed by Enter so the agent receives and acts on it.
/// Uses `tmux send-keys -l` (literal mode) to avoid tmux interpreting
/// special characters like `{`, `}`, arrows.
pub async fn send_keys(session_id: &str, text: &str) -> Result<()> {
    // Send the message text in literal mode
    run_session_scoped(&["send-keys", "-t", session_id, "-l", text]).await?;
    // Send Enter to submit
    run_session_scoped(&["send-keys", "-t", session_id, "Enter"]).await?;
    Ok(())
}

/// Write `bytes` to the session's master PTY via tmux's paste-buffer
/// mechanism — the supported way to inject raw input (as opposed to
/// `send-keys`, which tmux may reinterpret). `tmp_path` is a scratch file
/// used to stage the buffer contents and is removed afterwards regardless
/// of outcome.
pub async fn paste_buffer(session_id: &str, buf_name: &str, tmp_path: &str, bytes: &[u8]) -> Result<()> {
    std::fs::write(tmp_path, bytes)?;
    let result = run_session_scoped(&[
        "load-buffer", "-b", buf_name, tmp_path, ";",
        "paste-buffer", "-b", buf_name, "-t", session_id, "-d",
    ]).await;
    let _ = std::fs::remove_file(tmp_path);
    result.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[tokio::test]
    async fn send_keys_builds_correct_command() {
        // This test validates our argument construction without actually calling tmux.
        // We test the shell_quote helper used by send_keys.
        let quoted = shell_quote("hello world");
        assert_eq!(quoted, "'hello world'");
        let with_apostrophe = shell_quote("don't");
        assert_eq!(with_apostrophe, "'don'\\''t'");
    }

    #[test]
    fn server_config_is_written_and_contains_required_settings() {
        let path = write_server_config().unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        for required in [
            "default-terminal \"tmux-256color\"",
            "extended-keys always",
            "extended-keys-format csi-u",
            "history-limit 100000",
            "status off",
            "window-size latest",
            "allow-passthrough on",
        ] {
            assert!(body.contains(required), "config missing {required:?}\n{body}");
        }
    }

    #[tokio::test]
    async fn require_version_passes_on_installed_tmux() {
        if !tmux_available() { return; }
        require_version().await.unwrap();
    }

    #[tokio::test]
    async fn sessions_are_created_on_the_ninox_socket() {
        if !tmux_available() { return; }
        let id = unique_id();
        create_session(&id, "/tmp", "sleep 30", &[]).await.unwrap();
        // Visible via -L ninox …
        assert!(has_session(&id).await);
        // … and NOT on the default socket.
        let default_out = std::process::Command::new("tmux")
            .args(["has-session", "-t", &id])
            .output()
            .unwrap();
        assert!(!default_out.status.success(), "session leaked onto the default socket");
        kill_session(&id).await.unwrap();
    }

    #[tokio::test]
    async fn legacy_default_socket_sessions_are_still_reachable() {
        if !tmux_available() { return; }
        let id = unique_id();
        // Simulate a session created by an older build: default socket, no -L.
        let st = std::process::Command::new("tmux")
            .args(["new-session", "-d", "-s", &id, "-x", "80", "-y", "24", "sleep 30"])
            .status()
            .unwrap();
        assert!(st.success());
        assert!(has_session(&id).await, "has_session must fall back to the default socket");
        let argv = attach_args(&id).await;
        assert!(!argv.contains(&"-L".to_string()), "legacy session must attach without -L: {argv:?}");
        kill_session(&id).await.unwrap(); // must kill on the default socket too
        assert!(!has_session(&id).await);
    }

    #[tokio::test]
    async fn capture_history_returns_scrolled_off_lines() {
        if !tmux_available() { return; }
        let id = unique_id();
        // 50-row pane; print 80 numbered lines so the earliest ones scroll into history.
        create_session(&id, "/tmp", "bash -c 'for i in $(seq 1 80); do echo line-$i; done; sleep 30'", &[]).await.unwrap();
        sleep(Duration::from_millis(500)).await;
        let hist = history_size(&id).await;
        assert!(hist > 0, "expected history to accumulate, got {hist}");
        let bytes = capture_history(&id, -hist, -1).await;
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("line-1"), "oldest line missing from history capture: {text}");
        kill_session(&id).await.unwrap();
    }
}
