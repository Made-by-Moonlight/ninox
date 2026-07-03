# Native Terminal Rendering Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the ninox terminal render like a native terminal by turning the app into a real tmux client (spec: `docs/superpowers/specs/2026-07-03-native-terminal-rendering-design.md`).

**Architecture:** For each on-screen session the app spawns `tmux attach` on a hidden PTY and feeds the master side into the existing alacritty `Term`, so tmux's grid + history are the single source of truth. This replaces the frame-diff scrollback heuristic (`extra_history`), the paste-buffer input path, and the bounce-resize replay hack. All ninox sessions move to a private tmux server (`-L ninox`) with a ninox-managed config that enables extended keys (the Shift+Enter fix), truecolor, and a 100k history. Scrollback is fetched on demand via `capture-pane`. The renderer gets measured font metrics, theme palettes, cursor shapes, and text styles.

**Tech Stack:** Rust workspace (`ninox-core`, `ninox-app`), iced 0.13 canvas, alacritty_terminal 0.26, tmux ≥ 3.2 (3.6a locally), new deps: `portable-pty` (ninox-core), `ttf-parser` (ninox-app), bundled JetBrains Mono.

**Execution notes:**
- Work in a git worktree per the user's workflow: `git worktree add .claude/worktrees/feat-native-terminal-rendering -b feat/native-terminal-rendering` and make all changes inside it.
- Run tests with `cargo test -p <crate>` from the worktree root. tmux-gated integration tests skip silently when tmux is absent; tmux IS available locally, so they must pass, not skip.
- `cargo clippy --workspace` must stay clean after every task.

## Global Constraints

- tmux minimum version **3.2**; private socket name **`ninox`**; config rewritten at startup to `~/.config/ninox/tmux.conf` (via `dirs::config_dir()`).
- tmux config contents are fixed by the spec (see Task 1) — do not add or remove lines without updating the spec.
- `history-limit 100000`; scrollback fetch chunk size 300 lines.
- Attach clients advertise `TERM=xterm-256color`.
- Legacy sessions (default tmux socket) must keep working: session-scoped tmux commands fall back to the default socket when the session isn't on the ninox socket.
- The browser WebSocket route (`ninox-server/src/routes/terminal.rs`) and `pipe-pane` taps must keep working unchanged.
- Terminal font is bundled JetBrains Mono; Nerd Font PUA fallback (0xE000–0xF8FF → "Symbols Nerd Font Mono") is preserved.
- Conventional commits; no co-author lines.

---

### Task 1: Private tmux server (config, socket routing, legacy fallback, history helpers)

**Files:**
- Modify: `crates/ninox-core/src/tmux.rs`
- Modify: `crates/ninox-app/src/main.rs` (startup hook — near the engine creation in the GUI path AND before CLI subcommand dispatch, so `ninox spawn`/`send` also hit the right server)

**Interfaces:**
- Produces (used by later tasks):
  - `pub fn write_server_config() -> anyhow::Result<std::path::PathBuf>`
  - `pub async fn require_version() -> anyhow::Result<()>` (Err if tmux < 3.2 or missing, message names the installed version)
  - `pub async fn attach_args(session_id: &str) -> Vec<String>` — full argv (element 0 = `"tmux"`) resolving legacy socket
  - `pub async fn history_size(session_id: &str) -> i64`
  - `pub async fn capture_history(session_id: &str, start: i64, end: i64) -> Vec<u8>` — `capture-pane -p -e -S <start> -E <end>` (no `-J`)
  - All existing `tmux.rs` functions now run against the ninox socket, with session-scoped fallback to the default socket.

- [ ] **Step 1: Write failing tests** (append to the `tests` module in `tmux.rs`)

```rust
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
    // 5-row pane; print 30 numbered lines so 1..≈24 land in history.
    create_session(&id, "/tmp", "bash -c 'for i in $(seq 1 30); do echo line-$i; done; sleep 30'", &[]).await.unwrap();
    sleep(Duration::from_millis(500)).await;
    let hist = history_size(&id).await;
    assert!(hist > 0, "expected history to accumulate, got {hist}");
    let bytes = capture_history(&id, -hist, -1).await;
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("line-1"), "oldest line missing from history capture: {text}");
    kill_session(&id).await.unwrap();
}
```

Note: `create_session` uses fixed `-x 140 -y 50`; for the history test that still works (30 lines < 50 rows would NOT scroll). Change the history test to `seq 1 80` so output exceeds 50 rows.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox-core tmux -- --nocapture`
Expected: FAIL — `write_server_config`, `require_version`, `attach_args`, `history_size`, `capture_history` not found.

- [ ] **Step 3: Implement in `tmux.rs`**

Add at the top (after imports):

```rust
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
    Ok(())
}

fn is_missing_session(e: &anyhow::Error) -> bool {
    let msg = e.to_string();
    msg.contains("can't find session")
        || msg.contains("session not found")
        || msg.contains("no server running")
        || msg.contains("no sessions")
}
```

Rework the runners:

```rust
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
```

Then update call sites inside `tmux.rs`:
- `create_session`: keep using `run` (ninox socket only). Delete the trailing `set-option -t id status off` block — the server config handles it.
- `kill_session`, `has_session`, `get_pane_tty`, `pipe_pane`, `resize_window`, `capture_pane`, `send_keys`: switch their `run(...)` calls to `run_session_scoped(...)`. `has_session` becomes:

```rust
pub async fn has_session(id: &str) -> bool {
    run_session_scoped(&["has-session", "-t", id]).await.is_ok()
}
```

- `kill_session`: after the ninox-socket attempt, also best-effort `run_default(&["kill-session", "-t", id])` so legacy sessions actually die (`run_session_scoped` only falls back when ninox reports missing-session, which it does here, so `run_session_scoped` alone is sufficient — keep the existing error-tolerant match around it).
- `list_sessions`: run the `list-sessions -F …` line twice — once via `run_best_effort`-on-ninox and once via a default-socket equivalent — and concatenate the outputs before parsing (dedupe by id, ninox socket wins).
- `run_best_effort` stays but now wraps the new `run` (ninox socket).

New helpers:

```rust
/// Full argv for `tmux attach` for this session (element 0 is "tmux"),
/// resolving whether it lives on the ninox or the legacy default server.
pub async fn attach_args(session_id: &str) -> Vec<String> {
    let mut argv = vec!["tmux".to_string()];
    if run(&["has-session", "-t", session_id]).await.is_ok() {
        argv.extend(socket_args());
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
```

In `crates/ninox-app/src/main.rs`, at the top of `main()` (before CLI dispatch and GUI startup):

```rust
if let Err(e) = ninox_core::tmux::write_server_config() {
    eprintln!("failed to write tmux config: {e}");
}
```

and in the GUI startup path (where the engine is created), add a hard version gate:

```rust
if let Err(e) = rt.block_on(ninox_core::tmux::require_version()) {
    eprintln!("{e}");
    std::process::exit(1);
}
```

(Adapt to how main.rs actually enters async — if the GUI path is already inside a tokio runtime, just `.await` it.)

- [ ] **Step 4: Run tests**

Run: `cargo test -p ninox-core tmux -- --nocapture`
Expected: PASS (all new tests, plus existing `create_and_has_and_kill`, `list_includes_created`, etc. now on the ninox socket).

Also run: `cargo test -p ninox-core` (the pty.rs tests must still pass — they go through `create_session`/`pipe_pane`, now socket-routed).

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-core/src/tmux.rs crates/ninox-app/src/main.rs
git commit -m "feat(core): private ninox tmux server with managed config and legacy-socket fallback"
```

---

### Task 2: AttachedClient — the hidden tmux client on a PTY

**Files:**
- Create: `crates/ninox-core/src/client.rs`
- Modify: `crates/ninox-core/src/events.rs` (two new Event variants)
- Modify: `crates/ninox-core/src/lib.rs` (add `pub mod client;`)
- Modify: `crates/ninox-core/Cargo.toml` (add `portable-pty = "0.8"`)

**Interfaces:**
- Consumes: `tmux::attach_args` (Task 1), `Engine::emit`.
- Produces:
  - `Event::ClientOutput { session_id: SessionId, bytes: Vec<u8> }` — the tmux client's rendering stream
  - `Event::ClientClosed { session_id: SessionId }` — emitted once when the client process exits
  - `pub struct AttachedClient` with:
    - `pub fn spawn(engine: Arc<Engine>, session_id: SessionId, argv: Vec<String>, cols: u16, rows: u16) -> anyhow::Result<AttachedClient>` (synchronous — caller resolves `argv` via `tmux::attach_args` beforehand)
    - `pub fn write(&self, bytes: Vec<u8>)`
    - `pub fn input_sender(&self) -> tokio::sync::mpsc::UnboundedSender<Vec<u8>>`
    - `pub fn resize(&self, cols: u16, rows: u16)`
    - `Drop` kills the child process.

- [ ] **Step 1: Add Event variants** in `events.rs`:

```rust
    TerminalOutput { session_id: SessionId, bytes: Vec<u8> },
    /// Rendering stream from an attached tmux client (AttachedClient).
    ClientOutput   { session_id: SessionId, bytes: Vec<u8> },
    /// The attached tmux client process exited (detach, kill, server gone).
    ClientClosed   { session_id: SessionId },
```

- [ ] **Step 2: Write failing test** (in `client.rs`, gated like the pty.rs tests — reuse the same `tmux_available`, `unique_id`, `test_engine`, `collect` helper shapes):

```rust
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
```

Remove the placeholder `history_size` lines in `resize_drives_tmux_window_size` — they're not needed; the test body above them is complete without them. (Do not ship dead lines.)

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p ninox-core client -- --nocapture`
Expected: FAIL — module/struct don't exist.

- [ ] **Step 4: Implement `client.rs`**

```rust
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
        let killer = child.clone_killer();
        drop(pair.slave);

        // Reader thread: PTY master → ClientOutput events. Blocking reads on
        // a dedicated thread; Engine::emit is sync so no runtime needed here.
        let mut reader = pair.master.try_clone_reader().context("clone PTY reader")?;
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
        let mut writer = pair.master.take_writer().context("take PTY writer")?;
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
```

Add to `Cargo.toml` `[dependencies]`: `portable-pty = "0.8"`. Add `pub mod client;` to `lib.rs` (match the existing module list style).

- [ ] **Step 5: Run tests**

Run: `cargo test -p ninox-core client -- --nocapture`
Expected: PASS (3 tests). If `resize_drives_tmux_window_size` is flaky on timing, bump the post-resize sleep to 800ms — do not weaken the assertions.

- [ ] **Step 6: Commit**

```bash
git add crates/ninox-core/src/client.rs crates/ninox-core/src/events.rs crates/ninox-core/src/lib.rs crates/ninox-core/Cargo.toml Cargo.lock
git commit -m "feat(core): AttachedClient hidden tmux client over a PTY with ClientOutput/ClientClosed events"
```

---

### Task 3: Simplify TerminalState — delete the diff heuristic, forward emulator replies

**Files:**
- Modify: `crates/ninox-app/src/components/terminal.rs`
- Delete: `crates/ninox-app/src/components/testdata/spinner_thinking_capture.bin`, `crates/ninox-app/src/components/testdata/claude_overflow_capture.bin`

**Interfaces:**
- Consumes: nothing new.
- Produces (used by Task 5/6):
  - `TerminalState::new(cols: u16, rows: u16, reply: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>) -> TerminalState` — `reply` receives emulator-generated responses (`PtyWrite`)
  - `TerminalState::process(&mut self, bytes: &[u8])` — now a plain parser advance
  - `TerminalState { pub term, pub cache, parser }` — `extra_history`/`extra_offset` GONE
  - `EventProxy(Option<UnboundedSender<Vec<u8>>>)` forwards `Event::PtyWrite`

- [ ] **Step 1: Write the failing test** (replace the tests module's heuristic tests; keep `process_advances_cursor` and `process_ansi_no_panic`, updated to the new constructor):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_advances_cursor() {
        let mut s = TerminalState::new(80, 24, None);
        s.process(b"hello");
        assert_eq!(s.term.grid().cursor.point.column.0, 5);
    }

    #[test]
    fn process_ansi_no_panic() {
        let mut s = TerminalState::new(80, 24, None);
        s.process(b"\x1b[31mred\x1b[0m");
    }

    #[test]
    fn emulator_query_responses_are_forwarded_to_reply_channel() {
        // The inner app (via tmux) queries the terminal — e.g. DSR 6 (cursor
        // position report). The emulator's answer must reach the reply
        // channel; dropping it hangs TUIs that wait for the response.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut s = TerminalState::new(80, 24, Some(tx));
        s.process(b"\x1b[6n"); // Device Status Report: cursor position
        let reply = rx.try_recv().expect("DSR must produce a reply");
        assert_eq!(reply, b"\x1b[1;1R".to_vec());
    }

    #[test]
    fn kitty_keyboard_query_is_answered() {
        // Claude Code probes kitty keyboard support with CSI ? u. A reply
        // is what makes Shift+Enter negotiation work end-to-end.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut s = TerminalState::new(80, 24, Some(tx));
        s.process(b"\x1b[?u");
        let reply = rx.try_recv().expect("kitty query must produce a reply");
        assert!(reply.starts_with(b"\x1b[?"), "unexpected kitty reply: {reply:?}");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox-app terminal -- --nocapture`
Expected: FAIL — `new` has the wrong arity, old heuristic tests reference deleted items once you start cutting; the two new tests fail to compile.

- [ ] **Step 3: Implement**

In `terminal.rs`:

1. Replace `EventProxy`:

```rust
/// Forwards emulator-generated replies (cursor position reports, device
/// attributes, kitty keyboard responses) back to the PTY. `None` (tests,
/// sessions with no attached client) silently drops them.
#[derive(Clone)]
pub struct EventProxy(Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>);

impl alacritty_terminal::event::EventListener for EventProxy {
    fn send_event(&self, event: alacritty_terminal::event::Event) {
        if let alacritty_terminal::event::Event::PtyWrite(text) = event {
            if let Some(tx) = &self.0 {
                let _ = tx.send(text.into_bytes());
            }
        }
    }
}
```

2. Delete: `MAX_EXTRA_HISTORY`, the `extra_history` and `extra_offset` fields (and their doc comments), `capture_evicted_content`, `detect_shift`, `viewport_rows`, `extra_line`, and the whole splitting body of `process`. New `process`:

```rust
    /// Feed raw bytes from the attached tmux client into the emulator.
    pub fn process(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
        self.cache.clear();
    }
```

3. New constructor (move it out of the `#[cfg(test)]`-adjacent impl block into the main impl, since production code now calls it):

```rust
    pub fn new(
        cols:  u16,
        rows:  u16,
        reply: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    ) -> Self {
        use alacritty_terminal::term::{Config, test::TermSize};
        let size = TermSize::new(cols as usize, rows as usize);
        let config = Config { kitty_keyboard: true, ..Config::default() };
        let term = Term::new(config, &size, EventProxy(reply));
        Self { term, cache: Cache::new(), parser: Processor::new() }
    }
```

4. `scroll`, `scroll_to_bottom`, `is_scrolled_back`: reduce to native-grid-only for now (Task 6 replaces them with the scrollback provider):

```rust
    pub fn scroll(&mut self, delta: i32) {
        use alacritty_terminal::grid::Scroll;
        self.term.grid_mut().scroll_display(Scroll::Delta(delta));
        self.cache.clear();
    }

    pub fn scroll_to_bottom(&mut self) {
        use alacritty_terminal::grid::Scroll;
        self.term.grid_mut().scroll_display(Scroll::Bottom);
        self.cache.clear();
    }

    pub fn is_scrolled_back(&self) -> bool {
        self.term.grid().display_offset() > 0
    }
```

5. In `draw()`: delete the `total_offset`/`extra_history` branch (the `if logical_line < -history_size { … }` block and the `history_size`/`total_offset` locals); render with `let logical_line = row as i32 - display_offset;` only.

6. Fix the one production call site that constructs `TerminalState` (`app.rs:928`, `Event::TerminalOutput` arm) by passing `None` for now — Task 5 rewires it properly.

7. Delete the two `testdata/*.bin` files and the `CLAUDE_OVERFLOW_CAPTURE` constant.

- [ ] **Step 4: Run tests**

Run: `cargo test -p ninox-app -- --nocapture` and `cargo clippy -p ninox-app`
Expected: PASS / clean. The app still compiles and runs with the OLD pipe-pane pipeline (unchanged behavior at runtime; the heuristic is simply gone, so TUI scrollback is temporarily worse until Task 5/6 land — acceptable mid-branch state).

- [ ] **Step 5: Commit**

```bash
git add -A crates/ninox-app/src/components/
git commit -m "refactor(native-app): delete frame-diff scrollback heuristic; forward emulator query replies"
```

---

### Task 4: Mode-aware input encoder (Shift+Enter fix, bracketed paste)

**Files:**
- Create: `crates/ninox-app/src/input.rs`
- Modify: `crates/ninox-app/src/main.rs` or the module root that declares `mod app;` — add `mod input;` alongside it (wherever `mod app;` lives)
- Modify: `crates/ninox-app/src/app.rs` — delete `key_to_terminal_bytes` (its callers are rewired in Task 5, but swap the existing `RawKey` call site to the new function NOW to keep the build green)

**Interfaces:**
- Consumes: `alacritty_terminal::term::TermMode`.
- Produces:
  - `pub fn encode_key(key: &iced::keyboard::Key, modifiers: iced::keyboard::Modifiers, text: Option<&str>, mode: &TermMode) -> Option<Vec<u8>>`
  - `pub fn encode_paste(text: &str, mode: &TermMode) -> Vec<u8>`
  - `pub fn encode_wheel(lines_up: i32, col: usize, row: usize, mode: &TermMode) -> Option<Vec<u8>>` — Some when the inner app wants the wheel (mouse mode / alternate scroll), None when ninox scrollback should handle it

**Encoding rules** (ninox always talks to ninox-managed tmux with `extended-keys always`, so modified functional keys are ALWAYS CSI-u encoded — tmux downgrades them for apps that didn't opt in; that's exactly what a native extended-keys terminal does):

- [ ] **Step 1: Write the failing tests** (bottom of `input.rs`):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::term::TermMode;
    use iced::keyboard::{key::Named, Key, Modifiers};

    fn enc(key: Key, m: Modifiers, text: Option<&str>, mode: TermMode) -> Option<Vec<u8>> {
        encode_key(&key, m, text, &mode)
    }

    #[test]
    fn plain_enter_is_cr() {
        assert_eq!(enc(Key::Named(Named::Enter), Modifiers::empty(), None, TermMode::empty()),
                   Some(b"\r".to_vec()));
    }

    #[test]
    fn shift_enter_is_csi_u() {
        // THE multi-line-input fix: distinguishable from plain Enter.
        assert_eq!(enc(Key::Named(Named::Enter), Modifiers::SHIFT, None, TermMode::empty()),
                   Some(b"\x1b[13;2u".to_vec()));
    }

    #[test]
    fn ctrl_enter_and_alt_enter_are_csi_u() {
        assert_eq!(enc(Key::Named(Named::Enter), Modifiers::CTRL, None, TermMode::empty()),
                   Some(b"\x1b[13;5u".to_vec()));
        assert_eq!(enc(Key::Named(Named::Enter), Modifiers::ALT, None, TermMode::empty()),
                   Some(b"\x1b[13;3u".to_vec()));
    }

    #[test]
    fn arrows_respect_app_cursor_mode() {
        assert_eq!(enc(Key::Named(Named::ArrowUp), Modifiers::empty(), None, TermMode::empty()),
                   Some(b"\x1b[A".to_vec()));
        assert_eq!(enc(Key::Named(Named::ArrowUp), Modifiers::empty(), None, TermMode::APP_CURSOR),
                   Some(b"\x1bOA".to_vec()));
    }

    #[test]
    fn modified_arrows_use_xterm_modifier_encoding() {
        // Shift+Up = CSI 1;2A regardless of APP_CURSOR (xterm behavior).
        assert_eq!(enc(Key::Named(Named::ArrowUp), Modifiers::SHIFT, None, TermMode::APP_CURSOR),
                   Some(b"\x1b[1;2A".to_vec()));
    }

    #[test]
    fn ctrl_letters_are_caret_codes() {
        assert_eq!(enc(Key::Character("c".into()), Modifiers::CTRL, Some("c"), TermMode::empty()),
                   Some(vec![0x03]));
    }

    #[test]
    fn alt_character_gets_esc_prefix() {
        assert_eq!(enc(Key::Character("b".into()), Modifiers::ALT, Some("b"), TermMode::empty()),
                   Some(b"\x1bb".to_vec()));
    }

    #[test]
    fn plain_text_passes_through() {
        assert_eq!(enc(Key::Character("~".into()), Modifiers::SHIFT, Some("~"), TermMode::empty()),
                   Some(b"~".to_vec()));
    }

    #[test]
    fn shift_tab_is_backtab() {
        assert_eq!(enc(Key::Named(Named::Tab), Modifiers::SHIFT, None, TermMode::empty()),
                   Some(b"\x1b[Z".to_vec()));
    }

    #[test]
    fn paste_is_bracketed_only_when_mode_set() {
        assert_eq!(encode_paste("a\nb", &TermMode::empty()), b"a\nb".to_vec());
        assert_eq!(encode_paste("a\nb", &TermMode::BRACKETED_PASTE),
                   b"\x1b[200~a\nb\x1b[201~".to_vec());
    }

    #[test]
    fn wheel_goes_to_app_only_in_mouse_mode() {
        assert_eq!(encode_wheel(1, 5, 3, &TermMode::empty()), None);
        // SGR mouse wheel-up at 1-based col 6, row 4.
        assert_eq!(
            encode_wheel(1, 5, 3, &(TermMode::MOUSE_MODE | TermMode::SGR_MOUSE)),
            Some(b"\x1b[<64;6;4M".to_vec())
        );
        assert_eq!(
            encode_wheel(-1, 5, 3, &(TermMode::MOUSE_MODE | TermMode::SGR_MOUSE)),
            Some(b"\x1b[<65;6;4M".to_vec())
        );
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox-app input -- --nocapture`
Expected: FAIL — module doesn't exist.

- [ ] **Step 3: Implement `input.rs`**

```rust
//! Keyboard/mouse/paste → terminal byte encoding, honoring the modes the
//! inner application negotiated (read from the live alacritty Term).
//!
//! Modified functional keys are always emitted in kitty CSI-u form: ninox
//! only ever talks to its own tmux server (extended-keys always), which
//! forwards them to applications that requested them and downgrades them
//! for applications that didn't — identical to a native extended-keys
//! terminal. Legacy default-socket sessions may not understand CSI-u; they
//! degrade exactly as they did before this feature existed.

use alacritty_terminal::term::TermMode;
use iced::keyboard::{key::Named, Key, Modifiers};

/// xterm/kitty modifier parameter: 1 + bitfield(shift=1, alt=2, ctrl=4, super=8).
fn modifier_param(m: Modifiers) -> u32 {
    1 + (m.shift() as u32)
        + ((m.alt() as u32) << 1)
        + ((m.control() as u32) << 2)
        + ((m.logo() as u32) << 3)
}

/// kitty CSI-u codepoint for functional keys that need disambiguation.
fn functional_code(key: &Key) -> Option<u32> {
    Some(match key {
        Key::Named(Named::Enter)     => 13,
        Key::Named(Named::Escape)    => 27,
        Key::Named(Named::Backspace) => 127,
        Key::Named(Named::Tab)       => 9,
        _ => return None,
    })
}

pub fn encode_key(
    key:       &Key,
    modifiers: Modifiers,
    text:      Option<&str>,
    mode:      &TermMode,
) -> Option<Vec<u8>> {
    let mods = modifier_param(modifiers);

    // Modified functional keys → CSI-u. Shift+Tab keeps its classic
    // backtab encoding (universally understood; CSI-u tab is not).
    if mods > 1 && !(matches!(key, Key::Named(Named::Tab)) && modifiers == Modifiers::SHIFT) {
        if let Some(code) = functional_code(key) {
            return Some(format!("\x1b[{code};{mods}u").into_bytes());
        }
    }

    // Ctrl+letter → caret notation (Ctrl+A=0x01 … Ctrl+Z=0x1A, Ctrl+[=ESC …).
    if modifiers.control() {
        if let Key::Character(c) = key {
            if let Some(ch) = c.chars().next() {
                let b = match ch {
                    'a'..='z' => Some(vec![(ch as u8) - b'a' + 1]),
                    'A'..='Z' => Some(vec![(ch as u8) - b'A' + 1]),
                    '['       => Some(b"\x1b".to_vec()),
                    '\\'      => Some(b"\x1c".to_vec()),
                    ']'       => Some(b"\x1d".to_vec()),
                    '^' | '6' => Some(b"\x1e".to_vec()),
                    '_'       => Some(b"\x1f".to_vec()),
                    _ => None,
                };
                if b.is_some() { return b; }
            }
        }
    }

    // Arrows: modified → xterm CSI 1;<mods><ABCD>; plain → mode-sensitive.
    let arrow = |letter: char| -> Vec<u8> {
        if mods > 1 {
            format!("\x1b[1;{mods}{letter}").into_bytes()
        } else if mode.contains(TermMode::APP_CURSOR) {
            format!("\x1bO{letter}").into_bytes()
        } else {
            format!("\x1b[{letter}").into_bytes()
        }
    };

    let bytes: Vec<u8> = match key {
        Key::Named(Named::Enter)      => b"\r".to_vec(),
        Key::Named(Named::Escape)     => b"\x1b".to_vec(),
        Key::Named(Named::Backspace)  => b"\x7f".to_vec(),
        Key::Named(Named::Delete)     => b"\x1b[3~".to_vec(),
        Key::Named(Named::Tab) if modifiers.shift() => b"\x1b[Z".to_vec(),
        Key::Named(Named::Tab)        => b"\t".to_vec(),
        Key::Named(Named::ArrowUp)    => arrow('A'),
        Key::Named(Named::ArrowDown)  => arrow('B'),
        Key::Named(Named::ArrowRight) => arrow('C'),
        Key::Named(Named::ArrowLeft)  => arrow('D'),
        Key::Named(Named::Home)       => b"\x1b[H".to_vec(),
        Key::Named(Named::End)        => b"\x1b[F".to_vec(),
        Key::Named(Named::PageUp)     => b"\x1b[5~".to_vec(),
        Key::Named(Named::PageDown)   => b"\x1b[6~".to_vec(),
        // Alt+char → ESC prefix; otherwise prefer `text` (shift-resolved).
        Key::Character(c) => {
            let base = text.map(|t| t.as_bytes().to_vec())
                           .unwrap_or_else(|| c.as_str().as_bytes().to_vec());
            if modifiers.alt() {
                let mut v = b"\x1b".to_vec();
                v.extend(base);
                v
            } else {
                base
            }
        }
        _ => text.map(|t| t.as_bytes().to_vec()).unwrap_or_default(),
    };
    if bytes.is_empty() { None } else { Some(bytes) }
}

/// Wrap pasted text in bracketed-paste markers when the app asked for them.
pub fn encode_paste(text: &str, mode: &TermMode) -> Vec<u8> {
    if mode.contains(TermMode::BRACKETED_PASTE) {
        let mut v = b"\x1b[200~".to_vec();
        v.extend(text.as_bytes());
        v.extend_from_slice(b"\x1b[201~");
        v
    } else {
        text.as_bytes().to_vec()
    }
}

/// SGR-encode a wheel event for the inner app, or None if ninox's own
/// scrollback should consume the wheel. col/row are 0-based cells.
pub fn encode_wheel(lines_up: i32, col: usize, row: usize, mode: &TermMode) -> Option<Vec<u8>> {
    if !mode.intersects(TermMode::MOUSE_MODE) {
        return None;
    }
    let button = if lines_up > 0 { 64 } else { 65 };
    Some(format!("\x1b[<{button};{};{}M", col + 1, row + 1).into_bytes())
}
```

Then in `app.rs`: delete `key_to_terminal_bytes` (lines ~167-229) and change the `Message::RawKey` handler to build the full mode and call the new encoder:

```rust
                    let mode = state.terminals.get(session_id)
                        .map(|t| *t.term.mode())
                        .unwrap_or_else(alacritty_terminal::term::TermMode::empty);
                    let Some(bytes) = crate::input::encode_key(&key, modifiers, text.as_deref(), &mode)
                        else { return Task::none(); };
```

(keep the existing `get_pty_writer` send for now — Task 5 redirects it to the attached client).

- [ ] **Step 4: Run tests**

Run: `cargo test -p ninox-app -- --nocapture && cargo clippy -p ninox-app`
Expected: PASS / clean.

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-app/src/input.rs crates/ninox-app/src/app.rs crates/ninox-app/src/main.rs
git commit -m "feat(native-app): mode-aware input encoder with CSI-u modified keys and bracketed paste"
```

---

### Task 5: Wire the app to attached clients

**Files:**
- Modify: `crates/ninox-app/src/app.rs` (App state, NavigateSession, RawKey, WindowResized/MouseReleased, handle_engine_event, new messages)
- Modify: `crates/ninox-core/src/pty.rs` (drop the bounce-resize + cols/rows params from `start_streaming`)

**Interfaces:**
- Consumes: `AttachedClient` (Task 2), `TerminalState::new(cols, rows, reply)` (Task 3), `encode_key`/`encode_paste` (Task 4), `tmux::attach_args`.
- Produces:
  - `App.clients: HashMap<SessionId, ninox_core::client::AttachedClient>` — at most one per on-screen session
  - `Message::ClientAttach { session_id: SessionId, argv: Vec<String> }` (Clone-able; carries resolved attach argv)
  - `ninox_core::pty::start_streaming(engine, session_id, tmux_id) -> Result<()>` (new signature: no cols/rows, no bounce)

- [ ] **Step 1: Rework `pty.rs` `start_streaming`**

Remove the `cols: u16, rows: u16` parameters and delete the "Force SIGWINCH via bounce resize" block (lines 64-76) entirely — attach repaint replaces it, and `resize-window` would switch tmux to manual window sizing, fighting `window-size latest`. Update the doc comment: the FIFO tap now exists for the browser WebSocket route and background-session monitoring only. Fix the two tests in `pty.rs` (drop the `80, 24` args). Keep the paste-buffer input task — the WebSocket route still uses `engine.get_pty_writer`.

- [ ] **Step 2: Add App state + messages** (`app.rs`)

To the `App` struct (near `terminals: HashMap<...>`):

```rust
    /// One hidden tmux client per on-screen session (the "view"). Dropping
    /// an entry kills the client process; the session itself stays detached
    /// and running.
    clients: HashMap<SessionId, ninox_core::client::AttachedClient>,
    /// Sessions that already burned their one automatic reattach after an
    /// unexpected ClientClosed. Cleared on navigation.
    reattach_attempted: std::collections::HashSet<SessionId>,
```

(initialize both in `App::new`). New Message variant:

```rust
    /// Attach argv resolved — spawn the hidden tmux client for this session.
    ClientAttach { session_id: SessionId, argv: Vec<String> },
```

- [ ] **Step 3: Rewrite `Message::NavigateSession`**

Replace the body (app.rs:404-456) with:

```rust
            Message::NavigateSession(id) => {
                state.view = View::SessionDetail {
                    session_id: id.clone(),
                    panel: DetailPanel::default(),
                };
                // Drop every client that is no longer on screen — the tmux
                // sessions stay detached and running.
                state.clients.retain(|sid, _| sid == &id);
                state.reattach_attempted.clear();
                // Fresh view: kill any previous client + emulator for this
                // session; attach repaints the whole screen into clean state.
                state.clients.remove(&id);
                state.terminals.remove(&id);
                Self::resize_terminals(state);

                let engine = state.engine.clone();
                Task::future(async move {
                    if !ninox_core::tmux::has_session(&id).await {
                        if let Ok(Some(mut s)) = engine.store.get_session(&id) {
                            s.status = ninox_core::types::SessionStatus::Terminated;
                            let _ = engine.store.upsert_session(&s);
                            engine.emit(ninox_core::events::Event::SessionUpdated(s));
                        }
                        return Message::Noop;
                    }
                    // Keep the pipe-pane tap alive for the WS route/monitoring.
                    if let Err(e) = ninox_core::pty::start_streaming(engine.clone(), id.clone(), &id).await {
                        tracing::warn!("pipe-pane tap for {id}: {e}");
                    }
                    let argv = ninox_core::tmux::attach_args(&id).await;
                    Message::ClientAttach { session_id: id, argv }
                })
            }
```

- [ ] **Step 4: Handle `Message::ClientAttach`** (new arm in `apply`)

```rust
            Message::ClientAttach { session_id, argv } => {
                // Only attach if the user is still looking at this session.
                let viewing = matches!(&state.view,
                    View::SessionDetail { session_id: sid, .. } if sid == &session_id);
                if !viewing { return Task::none(); }

                let (cols, rows) = (state.terminal_cols, state.terminal_rows);
                match ninox_core::client::AttachedClient::spawn(
                    state.engine.clone(), session_id.clone(), argv, cols, rows,
                ) {
                    Ok(client) => {
                        // Fresh emulator wired to the client so query replies
                        // (DSR/DA/kitty) flow back to tmux.
                        state.terminals.insert(
                            session_id.clone(),
                            crate::components::terminal::TerminalState::new(
                                cols, rows, Some(client.input_sender()),
                            ),
                        );
                        state.clients.insert(session_id.clone(), client);
                        // The client was spawned at the background size; the
                        // active panel may want a different one — reflow and
                        // push the real size to the client PTY.
                        let resized = Self::resize_terminals(state);
                        if let Some((_, c, r)) = resized.iter().find(|(sid, ..)| sid == &session_id) {
                            if let Some(client) = state.clients.get(&session_id) {
                                client.resize(*c, *r);
                            }
                        }
                    }
                    Err(e) => tracing::error!("attach client for {session_id}: {e}"),
                }
                Task::none()
            }
```

- [ ] **Step 5: Route output, input, resize, and close through the client**

1. `handle_engine_event`: change the `Event::TerminalOutput` arm to a no-op comment (the tap now only feeds the WS route) and add:

```rust
            Event::TerminalOutput { .. } => {
                // Raw pane tap — consumed by the browser WS route, not the app.
            }

            Event::ClientOutput { session_id, bytes } => {
                if let Some(term) = state.terminals.get_mut(&session_id) {
                    term.process(&bytes);
                }
            }

            Event::ClientClosed { session_id } => {
                let viewing = matches!(&state.view,
                    View::SessionDetail { session_id: sid, .. } if sid == &session_id);
                state.clients.remove(&session_id);
                state.terminals.remove(&session_id);
                // One automatic reattach for unexpected deaths (tmux server
                // restart); repeated failures fall through to the
                // "Terminal connecting…" placeholder.
                if viewing && state.reattach_attempted.insert(session_id.clone()) {
                    let engine = state.engine.clone();
                    return Task::future(async move {
                        if !ninox_core::tmux::has_session(&session_id).await {
                            return Message::Noop;
                        }
                        let argv = ninox_core::tmux::attach_args(&session_id).await;
                        Message::ClientAttach { session_id, argv }
                    });
                }
            }
```

Note `handle_engine_event` currently returns `Task::none()` at the bottom; restructure it to `return` tasks from arms (change the match to produce a `Task<Message>` like `apply` does).

2. `Message::RawKey`: replace the `get_pty_writer` send with the client write, and add Cmd/Ctrl+V paste:

```rust
                    // Paste: Cmd+V (macOS) / Ctrl+Shift+V.
                    let is_paste = matches!(&key, iced::keyboard::Key::Character(c)
                            if c.as_str().eq_ignore_ascii_case("v"))
                        && (modifiers.logo() || (modifiers.control() && modifiers.shift()));
                    if is_paste {
                        if let Ok(mut cb) = arboard::Clipboard::new() {
                            if let Ok(pasted) = cb.get_text() {
                                let payload = crate::input::encode_paste(&pasted, &mode);
                                if let Some(client) = state.clients.get(session_id) {
                                    client.write(payload);
                                }
                            }
                        }
                        return Task::none();
                    }
                    let Some(bytes) = crate::input::encode_key(&key, modifiers, text.as_deref(), &mode)
                        else { return Task::none(); };
                    if let Some(client) = state.clients.get(session_id) {
                        client.write(bytes);
                    }
                    Task::none()
```

(the whole handler becomes synchronous — no `Task::future`, no `engine.get_pty_writer`).

3. `resize_terminals` callers: in `Message::WindowResized` and `Message::MouseReleased`, replace the `tmux::resize_window` loops with client resizes (synchronous):

```rust
                for (sid, cols, rows) in resized {
                    if let Some(client) = state.clients.get(&sid) {
                        client.resize(cols, rows);
                    }
                }
                Task::none()
```

Remove the `resize_window` call from the panel-switch handler if one exists (grep: `rg 'resize_window' crates/ninox-app/` must return zero hits after this task). Then delete `pub async fn resize_window` from `tmux.rs`.

4. `Message::ScrollTerminal` / `JumpToLatest` handlers: route wheel through the encoder first:

```rust
            Message::ScrollTerminal { session_id, delta } => {
                if let Some(term) = state.terminals.get_mut(&session_id) {
                    let mode = *term.term.mode();
                    if let Some(bytes) = crate::input::encode_wheel(delta, 0, 0, &mode) {
                        if let Some(client) = state.clients.get(&session_id) {
                            for _ in 0..delta.unsigned_abs() { client.write(bytes.clone()); }
                        }
                    } else {
                        term.scroll(delta);
                    }
                }
                Task::none()
            }
```

- [ ] **Step 6: Build, test, clippy**

Run: `cargo test --workspace && cargo clippy --workspace`
Expected: PASS / clean. (`pty.rs` tests updated for the new signature.)

- [ ] **Step 7: Manually smoke-test the live app**

Run the app (`cargo run` — check `README.md`/`main.rs` for the exact invocation), spawn or open a session running `claude`, and verify:
- The terminal paints on navigate (attach repaint, no bounce).
- Typing works; **Shift+Enter inserts a newline in Claude Code's input box** instead of submitting.
- Navigating away and back repaints correctly.
- `tmux -L ninox list-clients` shows exactly one client while viewing, zero after navigating to the fleet board.

Report what you observed — do not claim success without doing this.

- [ ] **Step 8: Commit**

```bash
git add crates/ninox-app/src/app.rs crates/ninox-core/src/pty.rs crates/ninox-core/src/tmux.rs
git commit -m "feat(native-app): render sessions through attached tmux clients with direct PTY input"
```

---

### Task 6: Scrollback provider (tmux history on demand)

**Files:**
- Create: `crates/ninox-app/src/components/scrollback.rs`
- Modify: `crates/ninox-app/src/components/terminal.rs` (TerminalState gains a `Scrollback`; draw blends history lines)
- Modify: `crates/ninox-app/src/app.rs` (fetch task + `HistoryFetched` message)
- Modify: the components module root (add `pub mod scrollback;` next to the existing `pub mod terminal;` declaration)

**Interfaces:**
- Consumes: `tmux::history_size`, `tmux::capture_history` (Task 1).
- Produces:
  - `pub struct StyledCell { pub c: char, pub fg: alacritty_terminal::vte::ansi::Color, pub bg: alacritty_terminal::vte::ansi::Color, pub flags: alacritty_terminal::term::cell::Flags }`
  - `pub type StyledLine = Vec<StyledCell>;`
  - `pub fn parse_capture(bytes: &[u8], cols: u16) -> Vec<StyledLine>` — parse `capture-pane -e` output into styled lines via a throwaway emulator
  - `pub struct Scrollback` with `offset: usize`, `lines: VecDeque<StyledLine>`, `fetched_to: i64` (most negative tmux index fetched), `top_reached: bool`, `fetch_pending: bool`, and:
    - `pub fn line_above(&self, n: usize) -> Option<&StyledLine>` — n=0 is the line just above the live screen
    - `pub fn scroll_up(&mut self, delta: usize) -> bool` — returns true when a fetch is needed
    - `pub fn scroll_down(&mut self, delta: usize)`
    - `pub fn absorb(&mut self, older: Vec<StyledLine>, fetched_to: i64, top_reached: bool)`
  - `Message::HistoryFetched { session_id: SessionId, bytes: Vec<u8>, fetched_to: i64, top_reached: bool }`
  - `TerminalState::scroll(&mut self, delta: i32) -> bool` — true = caller must fetch more history
  - Fetch chunk size: `pub const FETCH_CHUNK: i64 = 300;`

- [ ] **Step 1: Write the failing tests** (in `scrollback.rs`):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_capture_preserves_text_and_color() {
        // Two lines as capture-pane -e emits them: SGR + text + \n.
        let bytes = b"\x1b[31mred line\x1b[0m\nplain line\n";
        let lines = parse_capture(bytes, 40);
        assert_eq!(lines.len(), 2);
        let text: String = lines[0].iter().map(|c| c.c).collect();
        assert_eq!(text.trim_end(), "red line");
        use alacritty_terminal::vte::ansi::{Color, NamedColor};
        assert_eq!(lines[0][0].fg, Color::Named(NamedColor::Red));
        let text1: String = lines[1].iter().map(|c| c.c).collect();
        assert_eq!(text1.trim_end(), "plain line");
    }

    #[test]
    fn parse_capture_drops_trailing_blank_padding() {
        // The throwaway grid is taller than the content; blank tail rows
        // must not become phantom history lines.
        let lines = parse_capture(b"only\n", 40);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn scroll_bookkeeping_requests_fetch_at_cache_edge() {
        let mut sb = Scrollback::default();
        // Empty cache: any scroll up needs a fetch.
        assert!(sb.scroll_up(3));
        assert_eq!(sb.offset, 0, "offset must not exceed cached lines");

        sb.absorb(vec![vec![]; 100], -100, false);
        assert!(!sb.scroll_up(50), "within cache: no fetch needed");
        assert_eq!(sb.offset, 50);
        assert!(sb.scroll_up(60), "beyond cache: fetch needed");
        assert_eq!(sb.offset, 100, "clamped to cached lines");

        sb.scroll_down(30);
        assert_eq!(sb.offset, 70);
        sb.scroll_down(1000);
        assert_eq!(sb.offset, 0);
    }

    #[test]
    fn top_reached_stops_fetch_requests() {
        let mut sb = Scrollback::default();
        sb.absorb(vec![vec![]; 10], -10, true);
        assert!(!sb.scroll_up(500), "no more history exists; no fetch");
        assert_eq!(sb.offset, 10);
    }

    #[test]
    fn line_above_indexes_newest_first() {
        let mut sb = Scrollback::default();
        let mk = |ch: char| vec![StyledCell {
            c: ch,
            fg: alacritty_terminal::vte::ansi::Color::Named(alacritty_terminal::vte::ansi::NamedColor::Foreground),
            bg: alacritty_terminal::vte::ansi::Color::Named(alacritty_terminal::vte::ansi::NamedColor::Background),
            flags: alacritty_terminal::term::cell::Flags::empty(),
        }];
        // Oldest-first storage: a then b; b is directly above the screen.
        sb.absorb(vec![mk('a'), mk('b')], -2, true);
        assert_eq!(sb.line_above(0).unwrap()[0].c, 'b');
        assert_eq!(sb.line_above(1).unwrap()[0].c, 'a');
        assert!(sb.line_above(2).is_none());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox-app scrollback -- --nocapture`
Expected: FAIL — module doesn't exist.

- [ ] **Step 3: Implement `scrollback.rs`**

```rust
//! On-demand scrollback backed by tmux pane history (the source of truth).
//! Lines are fetched in chunks via `capture-pane -e`, parsed once into
//! styled cells, and cached for the lifetime of the view.

use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::Color;
use std::collections::VecDeque;

pub const FETCH_CHUNK: i64 = 300;

#[derive(Debug, Clone, PartialEq)]
pub struct StyledCell {
    pub c:     char,
    pub fg:    Color,
    pub bg:    Color,
    pub flags: Flags,
}

pub type StyledLine = Vec<StyledCell>;

/// Parse `capture-pane -e` output (SGR-styled text, \n separated) into
/// styled lines by replaying it through a throwaway emulator at pane width.
pub fn parse_capture(bytes: &[u8], cols: u16) -> Vec<StyledLine> {
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::{Column, Line};

    let n_lines = bytes.iter().filter(|&&b| b == b'\n').count().max(1);
    let mut state = crate::components::terminal::TerminalState::new(
        cols, n_lines.min(u16::MAX as usize) as u16, None,
    );
    // capture-pane emits bare \n; the emulator needs \r\n to reset columns.
    let mut feed = Vec::with_capacity(bytes.len() + n_lines);
    for &b in bytes {
        if b == b'\n' { feed.push(b'\r'); }
        feed.push(b);
    }
    state.process(&feed);

    let grid = state.term.grid();
    let rows = grid.screen_lines();
    let mut out = Vec::with_capacity(n_lines.min(rows));
    for row in 0..rows.min(n_lines) {
        let line = Line(row as i32);
        let mut cells: StyledLine = (0..grid.columns())
            .map(|col| {
                let cell = &grid[line][Column(col)];
                StyledCell { c: cell.c, fg: cell.fg, bg: cell.bg, flags: cell.flags }
            })
            .collect();
        // Trim trailing default-blank cells so rendering can skip them.
        while cells.last().map_or(false, |c| c.c == ' ' || c.c == '\0') {
            cells.pop();
        }
        out.push(cells);
    }
    out
}

/// Cached history + scroll position for one terminal view.
#[derive(Default)]
pub struct Scrollback {
    /// Cached history lines, oldest first.
    pub lines: VecDeque<StyledLine>,
    /// How many lines above the live screen the view is scrolled. 0 = live.
    pub offset: usize,
    /// Most negative tmux history index fetched so far (0 = nothing yet).
    pub fetched_to: i64,
    /// All available history has been fetched.
    pub top_reached: bool,
    /// A capture-pane fetch is in flight; don't issue another.
    pub fetch_pending: bool,
}

impl Scrollback {
    /// n=0 → the line directly above the live screen.
    pub fn line_above(&self, n: usize) -> Option<&StyledLine> {
        let len = self.lines.len();
        if n < len { self.lines.get(len - 1 - n) } else { None }
    }

    /// Scroll up by `delta`; clamps to cached lines. Returns true when the
    /// caller should fetch an older chunk (cache edge hit, more exists).
    pub fn scroll_up(&mut self, delta: usize) -> bool {
        let want = self.offset + delta;
        self.offset = want.min(self.lines.len());
        want > self.lines.len() && !self.top_reached && !self.fetch_pending
    }

    pub fn scroll_down(&mut self, delta: usize) {
        self.offset = self.offset.saturating_sub(delta);
    }

    /// Prepend an older chunk fetched from tmux.
    pub fn absorb(&mut self, older: Vec<StyledLine>, fetched_to: i64, top_reached: bool) {
        for line in older.into_iter().rev() {
            self.lines.push_front(line);
        }
        self.fetched_to = fetched_to;
        self.top_reached = top_reached;
        self.fetch_pending = false;
    }
}
```

- [ ] **Step 4: Wire into `TerminalState` and `draw`** (`terminal.rs`)

1. Add field `pub scrollback: crate::components::scrollback::Scrollback` (init `Default::default()` in `new`).
2. Replace the Task-3 interim scroll methods:

```rust
    /// Scroll by `delta` lines (positive = up). Returns true when older
    /// history must be fetched from tmux.
    pub fn scroll(&mut self, delta: i32) -> bool {
        let needs_fetch = if delta > 0 {
            self.scrollback.scroll_up(delta as usize)
        } else {
            self.scrollback.scroll_down((-delta) as usize);
            false
        };
        self.cache.clear();
        needs_fetch
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scrollback.offset = 0;
        self.cache.clear();
    }

    pub fn is_scrolled_back(&self) -> bool {
        self.scrollback.offset > 0
    }
```

3. In `draw()`: compose the viewport as history-above + live-below. At the top of the row loop:

```rust
            let offset = self.state.scrollback.offset as i32;
            for row in 0..rows {
                use alacritty_terminal::index::{Column, Line};
                let logical = row as i32 - offset;
                let y = row as f32 * cell_h;

                if logical < 0 {
                    // History line fetched from tmux: (-logical - 1) above screen.
                    let Some(cells) = self.state.scrollback.line_above((-logical - 1) as usize)
                        else { continue };
                    for (col, cell) in cells.iter().enumerate().take(cols) {
                        // Reuse the same per-cell drawing as live cells below,
                        // with is_cursor = false (cursor never draws in history).
                        …
                    }
                    continue;
                }
                let line = Line(logical);
                // …existing live-cell loop, using `line`…
```

Extract the per-cell background+glyph drawing into a private helper so history and live rows share it (signature: `fn draw_cell(frame, x, y, cell_w, cell_h, font_size, c, fg_color, bg_color, is_cursor, is_selected, colors, ansi, term_bg, term_fg, cursor_color)` — adjust to what Task 8 needs; at this task it can mirror the current inline logic). Cursor draw: skip when `offset > 0`.

- [ ] **Step 5: Wire fetching in `app.rs`**

New message: `HistoryFetched { session_id: SessionId, bytes: Vec<u8>, fetched_to: i64, top_reached: bool }`.

`ScrollTerminal` handler (extending Task 5's version): when `term.scroll(delta)` returns true:

```rust
                        term.scrollback.fetch_pending = true;
                        let from = term.scrollback.fetched_to; // 0 on first fetch
                        let sid = session_id.clone();
                        return Task::future(async move {
                            use crate::components::scrollback::FETCH_CHUNK;
                            let total = ninox_core::tmux::history_size(&sid).await;
                            let end = from - 1;              // next line above cache
                            let start = (from - FETCH_CHUNK).max(-total);
                            if end < -total || total == 0 {
                                return Message::HistoryFetched {
                                    session_id: sid, bytes: Vec::new(),
                                    fetched_to: from, top_reached: true,
                                };
                            }
                            let bytes = ninox_core::tmux::capture_history(&sid, start, end).await;
                            Message::HistoryFetched {
                                session_id: sid, bytes,
                                fetched_to: start, top_reached: start <= -total,
                            }
                        });
```

`HistoryFetched` handler:

```rust
            Message::HistoryFetched { session_id, bytes, fetched_to, top_reached } => {
                if let Some(term) = state.terminals.get_mut(&session_id) {
                    use alacritty_terminal::grid::Dimensions;
                    let cols = term.term.grid().columns() as u16;
                    let lines = crate::components::scrollback::parse_capture(&bytes, cols);
                    term.scrollback.absorb(lines, fetched_to, top_reached);
                    term.cache.clear();
                }
                Task::none()
            }
```

`JumpToLatest` keeps calling `scroll_to_bottom()`.

- [ ] **Step 6: Run tests + clippy**

Run: `cargo test -p ninox-app && cargo clippy --workspace`
Expected: PASS / clean.

- [ ] **Step 7: Manual verification**

In the running app, open a session, generate >100 lines of output (`seq 1 200` in a shell session, or a long Claude conversation), scroll up past the screen top — history appears in order, keeps loading in chunks as you scroll, "Jump to latest" returns to live. Quit the app, reopen, navigate to the session, scroll up: **history is still there** (it lives in tmux). Report observations.

- [ ] **Step 8: Commit**

```bash
git add crates/ninox-app/src/components/ crates/ninox-app/src/app.rs
git commit -m "feat(native-app): tmux-backed scrollback fetched on demand via capture-pane"
```

---

### Task 7: Measured font metrics (bundle JetBrains Mono)

**Files:**
- Create: `crates/ninox-app/assets/fonts/JetBrainsMono-Regular.ttf`, `JetBrainsMono-Bold.ttf`, `JetBrainsMono-Italic.ttf`, `JetBrainsMono-BoldItalic.ttf` (downloaded)
- Modify: `crates/ninox-app/src/main.rs` (register fonts), `crates/ninox-app/src/components/terminal.rs` (`cell_size`, glyph font), `crates/ninox-app/Cargo.toml` (add `ttf-parser = "0.25"`)

**Interfaces:**
- Produces:
  - `pub const TERM_FONT: iced::Font` (family "JetBrains Mono") replacing `iced::Font::MONOSPACE` in terminal drawing
  - `pub fn cell_size(font_size: f32) -> (f32, f32)` — same signature, now measured from font tables (callers in `app.rs` unchanged)

- [ ] **Step 1: Download the fonts**

```bash
cd crates/ninox-app/assets/fonts
for v in Regular Bold Italic BoldItalic; do
  curl -fsSLo JetBrainsMono-$v.ttf \
    "https://github.com/JetBrains/JetBrainsMono/raw/v2.304/fonts/ttf/JetBrainsMono-$v.ttf"
done
ls -la  # four ttf files, each > 100KB
```

(JetBrains Mono is OFL-1.1 — bundling is fine; add `assets/fonts/OFL.txt` from the same repo: `curl -fsSLo OFL.txt https://raw.githubusercontent.com/JetBrains/JetBrainsMono/v2.304/OFL.txt`.)

- [ ] **Step 2: Write the failing test** (in `terminal.rs` tests):

```rust
    #[test]
    fn cell_size_comes_from_font_metrics() {
        let (w, h) = cell_size(13.0);
        // JetBrains Mono: advance 600/1000 upem → width exactly 0.6em.
        assert!((w - 13.0 * 0.6).abs() < 0.01, "width {w}");
        // Height = (ascender - descender + line_gap)/upem — sane range, and
        // NOT the old hardcoded 1.4 approximation.
        assert!(h > 13.0 * 1.1 && h < 13.0 * 1.5, "height {h}");
        assert!((h - 13.0 * 1.4).abs() > 0.01, "height must be measured, not the 1.4 guess");
    }
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p ninox-app cell_size -- --nocapture`
Expected: FAIL — h == 18.2 (the 1.4 guess).

- [ ] **Step 4: Implement**

In `terminal.rs`:

```rust
pub const TERM_FONT_BYTES: &[u8] =
    include_bytes!("../../assets/fonts/JetBrainsMono-Regular.ttf");

pub const TERM_FONT: iced::Font = iced::Font {
    family:  iced::font::Family::Name("JetBrains Mono"),
    weight:  iced::font::Weight::Normal,
    stretch: iced::font::Stretch::Normal,
    style:   iced::font::Style::Normal,
};

/// Monospace cell size (width, height) in pixels, measured once from the
/// bundled font's tables — canvas drawing, hit-testing, and PTY sizing all
/// derive from this so they can never drift apart.
pub fn cell_size(font_size: f32) -> (f32, f32) {
    use std::sync::OnceLock;
    static RATIOS: OnceLock<(f32, f32)> = OnceLock::new();
    let (w, h) = *RATIOS.get_or_init(|| {
        let face = ttf_parser::Face::parse(TERM_FONT_BYTES, 0)
            .expect("bundled terminal font parses");
        let upem = face.units_per_em() as f32;
        let advance = face
            .glyph_index('M')
            .and_then(|g| face.glyph_hor_advance(g))
            .expect("monospace advance") as f32;
        let height = (face.ascender() as f32 - face.descender() as f32
            + face.line_gap() as f32).max(upem);
        (advance / upem, height / upem)
    });
    (font_size * w, font_size * h)
}
```

Adjust the relative path in `include_bytes!` to the actual layout (`terminal.rs` is at `src/components/`, so `../../assets/fonts/…`). In both glyph-drawing sites replace `iced::Font::MONOSPACE` with `TERM_FONT`.

In `main.rs`, next to the existing Nerd Font registration (line ~317-323), register all four faces:

```rust
        .font(include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf").as_slice())
        .font(include_bytes!("../assets/fonts/JetBrainsMono-Bold.ttf").as_slice())
        .font(include_bytes!("../assets/fonts/JetBrainsMono-Italic.ttf").as_slice())
        .font(include_bytes!("../assets/fonts/JetBrainsMono-BoldItalic.ttf").as_slice())
```

Add `ttf-parser = "0.25"` to `crates/ninox-app/Cargo.toml`.

- [ ] **Step 5: Run tests, then look at the app**

Run: `cargo test -p ninox-app && cargo clippy -p ninox-app`
Expected: PASS. Launch the app and confirm glyphs land on the grid (no horizontal drift on a full-width line of `M`s — run `printf 'M%.0s' {1..80}; echo` in a session). Cell metrics changed, so tmux panes get slightly different rows/cols — confirm resize still tracks the window.

- [ ] **Step 6: Commit**

```bash
git add crates/ninox-app/assets/fonts crates/ninox-app/src crates/ninox-app/Cargo.toml Cargo.lock
git commit -m "feat(native-app): bundle JetBrains Mono and measure cell metrics from font tables"
```

---

### Task 8: Renderer fidelity — styles, cursor shapes, theme palette

**Files:**
- Modify: `crates/ninox-app/src/theme.rs` (add `ansi: [Color; 16]` to `ColorScheme`)
- Modify: `crates/ninox-app/src/components/terminal.rs` (draw: bold/italic/underline variants/strikeout/dim/inverse/hidden; cursor shapes; palette param)
- Modify: `crates/ninox-app/src/components/session_detail.rs` (pass `s.ansi` into `TerminalWidget`)

**Interfaces:**
- Consumes: `TERM_FONT` (Task 7), `StyledCell` (Task 6 — history cells render with the same style pipeline).
- Produces:
  - `ColorScheme.ansi: [iced::Color; 16]`
  - `ansi_to_iced(color, colors, ansi: &[IcedColor; 16], bg, fg) -> IcedColor` (new `ansi` param; `DEFAULT_PALETTE` deleted)
  - `TerminalWidget.ansi: [IcedColor; 16]` field

- [ ] **Step 1: Write failing tests** (terminal.rs tests):

```rust
    #[test]
    fn ansi_to_iced_uses_theme_palette_for_named_colors() {
        use alacritty_terminal::vte::ansi::{Color, NamedColor};
        let colors = alacritty_terminal::term::color::Colors::default();
        let mut ansi = [IcedColor::BLACK; 16];
        ansi[1] = IcedColor::from_rgb8(0x12, 0x34, 0x56); // themed "red"
        let out = ansi_to_iced(
            Color::Named(NamedColor::Red), &colors, &ansi,
            IcedColor::BLACK, IcedColor::WHITE,
        );
        assert_eq!(out, IcedColor::from_rgb8(0x12, 0x34, 0x56));
    }

    #[test]
    fn every_theme_defines_a_full_palette() {
        for scheme in [crate::theme::light(), crate::theme::dark(), crate::theme::warm_dark()] {
            // 16 distinct-ish entries; at minimum not all default black.
            assert!(scheme.ansi.iter().any(|c| *c != iced::Color::BLACK));
            assert_eq!(scheme.ansi.len(), 16);
        }
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox-app -- ansi_to_iced theme_defines --nocapture`
Expected: FAIL — wrong arity / missing field.

- [ ] **Step 3: Implement**

1. `theme.rs`: add `pub ansi: [Color; 16],` to `ColorScheme` and populate each variant. Warm-dark keeps the current Gruvbox values (move them from `DEFAULT_PALETTE`); dark and light get palettes that read well on their `terminal_bg`:

```rust
// warm_dark (current Gruvbox values, unchanged):
ansi: [
    color!(0x282828), color!(0xcc241d), color!(0x98971a), color!(0xd79921),
    color!(0x458588), color!(0xb16286), color!(0x689d6a), color!(0xa89984),
    color!(0x928374), color!(0xfb4934), color!(0xb8bb26), color!(0xfabd2f),
    color!(0x83a598), color!(0xd3869b), color!(0x8ec07c), color!(0xebdbb2),
],
// dark (cool navy terminal_bg 0x0a1020):
ansi: [
    color!(0x1a2233), color!(0xf87171), color!(0x4ade80), color!(0xfbbf24),
    color!(0x60a5fa), color!(0xc084fc), color!(0x22d3ee), color!(0xcbd5e1),
    color!(0x475569), color!(0xfca5a5), color!(0x86efac), color!(0xfde047),
    color!(0x93c5fd), color!(0xd8b4fe), color!(0x67e8f9), color!(0xf1f5f9),
],
// light (terminal_bg is dark navy 0x1e2b4a, so palette stays dark-bg tuned):
ansi: [
    color!(0x2a3655), color!(0xef6b6b), color!(0x5fd68a), color!(0xf0c24a),
    color!(0x6f9df7), color!(0xc490f0), color!(0x55d3e0), color!(0xd5dcef),
    color!(0x5a6a92), color!(0xf79a9a), color!(0x8fe8b0), color!(0xf7d97e),
    color!(0x9dbcfa), color!(0xd9b6f5), color!(0x8ce4ee), color!(0xf2f5fd),
],
```

2. `terminal.rs`: delete `DEFAULT_PALETTE` and `named_to_iced`; rewrite `ansi_to_iced`:

```rust
pub fn ansi_to_iced(
    color:  Color,
    colors: &alacritty_terminal::term::color::Colors,
    ansi:   &[IcedColor; 16],
    bg:     IcedColor,
    fg:     IcedColor,
) -> IcedColor {
    match color {
        Color::Named(named) => {
            if let Some(rgb) = colors[named] {
                return rgb_to_iced(rgb);
            }
            let idx = named as usize;
            if idx < 16 { return ansi[idx]; }
            match named {
                NamedColor::Foreground | NamedColor::BrightForeground => fg,
                NamedColor::Background => bg,
                _ => fg,
            }
        }
        Color::Spec(rgb) => rgb_to_iced(rgb),
        Color::Indexed(idx) => {
            if let Some(rgb) = colors[idx as usize] {
                return rgb_to_iced(rgb);
            }
            if idx < 16 {
                ansi[idx as usize]
            } else if idx < 232 {
                let n = idx - 16;
                let b = (n % 6) * 51;
                let g = ((n / 6) % 6) * 51;
                let r = (n / 36) * 51;
                IcedColor::from_rgb8(r, g, b)
            } else {
                let v = 8 + (idx - 232) * 10;
                IcedColor::from_rgb8(v, v, v)
            }
        }
    }
}
```

3. `TerminalWidget`: add `pub ansi: [IcedColor; 16],`; `session_detail.rs` passes `ansi: s.ansi` (where `s` is the scheme; array is `Copy`).

4. In the shared cell-draw helper (from Task 6), implement flags:

```rust
    use alacritty_terminal::term::cell::Flags;

    // Resolve colors, then apply attribute transforms.
    let mut fg = ansi_to_iced(cell_fg, colors, &self.ansi, term_bg, term_fg);
    let mut bg = ansi_to_iced(cell_bg, colors, &self.ansi, term_bg, term_fg);
    if flags.contains(Flags::INVERSE) { std::mem::swap(&mut fg, &mut bg); }
    if flags.contains(Flags::DIM)     { fg.a *= 0.6; }
    if flags.contains(Flags::HIDDEN)  { fg = bg; }

    let font = if (0xE000..=0xF8FF).contains(&(ch as u32)) {
        NERD_FONT
    } else {
        iced::Font {
            weight: if flags.intersects(Flags::BOLD) { iced::font::Weight::Bold }
                    else { iced::font::Weight::Normal },
            style:  if flags.contains(Flags::ITALIC) { iced::font::Style::Italic }
                    else { iced::font::Style::Normal },
            ..TERM_FONT
        }
    };
```

and after the glyph, decoration strokes (all `Path::line` + `frame.stroke` with `Stroke::default().with_width(1.0).with_color(fg)`):

```rust
    let baseline = y + cell_h - 2.0;
    if flags.contains(Flags::UNDERLINE) {
        stroke_line(frame, x, baseline, x + cell_w, baseline, fg);
    }
    if flags.contains(Flags::DOUBLE_UNDERLINE) {
        stroke_line(frame, x, baseline - 2.0, x + cell_w, baseline - 2.0, fg);
        stroke_line(frame, x, baseline,       x + cell_w, baseline,       fg);
    }
    if flags.contains(Flags::UNDERCURL) {
        // Two-segment zigzag per cell — reads as a curl at terminal sizes.
        stroke_line(frame, x, baseline, x + cell_w / 2.0, baseline - 2.0, fg);
        stroke_line(frame, x + cell_w / 2.0, baseline - 2.0, x + cell_w, baseline, fg);
    }
    if flags.contains(Flags::STRIKEOUT) {
        let mid = y + cell_h * 0.55;
        stroke_line(frame, x, mid, x + cell_w, mid, fg);
    }
```

with the tiny helper:

```rust
fn stroke_line(frame: &mut Frame, x1: f32, y1: f32, x2: f32, y2: f32, color: IcedColor) {
    let path = Path::line(iced::Point::new(x1, y1), iced::Point::new(x2, y2));
    frame.stroke(&path, iced::widget::canvas::Stroke::default().with_width(1.0).with_color(color));
}
```

5. Cursor shapes — replace the block-only cursor with:

```rust
    use alacritty_terminal::vte::ansi::CursorShape;
    let cursor_style = term.cursor_style(); // shape + blink from DECSCUSR
    // (draw only when offset == 0 and the cell is the cursor cell)
    match cursor_style.shape {
        CursorShape::Block => { /* filled rect + inverted glyph color — existing behavior */ }
        CursorShape::Beam => {
            frame.fill(&Path::rectangle(iced::Point::new(x, y), Size::new(2.0, cell_h)), cursor_color);
        }
        CursorShape::Underline => {
            frame.fill(&Path::rectangle(iced::Point::new(x, y + cell_h - 2.0), Size::new(cell_w, 2.0)), cursor_color);
        }
        CursorShape::HollowBlock => {
            stroke_line(frame, x, y, x + cell_w, y, cursor_color);
            stroke_line(frame, x, y + cell_h, x + cell_w, y + cell_h, cursor_color);
            stroke_line(frame, x, y, x, y + cell_h, cursor_color);
            stroke_line(frame, x + cell_w, y, x + cell_w, y + cell_h, cursor_color);
        }
        CursorShape::Hidden => {}
    }
```

Only apply the "invert glyph color" treatment for Block; beam/underline draw the glyph normally. Blink: skip animation (steady cursor) — a timer subscription is not worth the redraw churn; note this deviation in the commit message.

- [ ] **Step 4: Run tests + look at the result**

Run: `cargo test -p ninox-app && cargo clippy --workspace`
Expected: PASS / clean.

Manual: in a session run
`printf '\e[1mbold\e[0m \e[3mitalic\e[0m \e[4munder\e[0m \e[4:3mcurl\e[0m \e[9mstrike\e[0m \e[2mdim\e[0m \e[7minv\e[0m \e[38;2;255;100;0mtruecolor\e[0m\n'`
and confirm each renders distinctly. Switch themes and confirm ANSI colors change. Run `claude` and confirm its UI colors/cursor look like a native terminal.

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-app/src
git commit -m "feat(native-app): full text-style rendering, cursor shapes, theme-driven ANSI palette"
```

---

### Task 9: Cleanup, full verification, PR

**Files:**
- Modify: whatever the greps below surface; no new code.

- [ ] **Step 1: Dead-code sweep**

Verify (each must return nothing, or delete the stragglers):
```bash
rg 'extra_history|extra_offset|detect_shift|viewport_rows|capture_evicted' crates/
rg 'resize_window' crates/
rg 'bounce' crates/ninox-core/src/pty.rs
rg 'MONOSPACE' crates/ninox-app/src/components/terminal.rs
rg 'DEFAULT_PALETTE' crates/
```
`tmux::capture_pane` (visible-screen variant) is now unused by the app — check `rg 'capture_pane\b' crates/` and delete the function if only tests reference it (delete those tests too).

- [ ] **Step 2: Full test suite + clippy**

Run: `cargo test --workspace && cargo clippy --workspace -- -D warnings`
Expected: PASS, zero warnings. tmux-gated tests run for real (tmux is installed).

- [ ] **Step 3: End-to-end verification against the spec's goals**

Drive the real app and check each original complaint, in one session running `claude`:
1. **Multi-line input:** Shift+Enter inserts a newline in the input box; Enter submits.
2. **Scrollback:** during a long response, wheel-scroll up — ordered, duplicate-free history; keeps loading as you scroll; jump-to-latest works; after app restart the same history is still scrollable.
3. **Rendering:** no garbled frames on navigate/resize/split-drag; colors/styles/cursor match running the same session in a real terminal (`tmux -L ninox attach -t <id>` from iTerm side-by-side is the reference).
4. **Multi-session:** two sessions (orchestrator + worker); switch between them repeatedly; `tmux -L ninox list-clients` never shows more than the on-screen client; background session keeps producing WS-visible output.

Record what you actually observed for each item.

- [ ] **Step 4: Update the design brief**

`docs/design-brief.md` section 2 describes the terminal panel — update the wording to mention the attached-client architecture in one sentence (it currently says "xterm-like grid canvas"). Keep it brief.

- [ ] **Step 5: Commit + PR**

```bash
git add -A
git commit -m "chore(native-app): remove dead rendering paths after attached-client migration"
```

Then follow the user's PR workflow: push (`gh auth switch --user slievr` first), open a PR titled `feat(native-app): native terminal rendering via attached tmux clients` with a body summarizing the architecture change (no test plan, no "Generated with Claude Code" annotation; run `now-playing` and append its output as the last line if it prints a track), then spawn a reviewer per the completing-a-feature workflow.
