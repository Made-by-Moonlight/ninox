//! Adversarial end-to-end confirmation (user-request spec test #5): drives
//! a real, compiled `nx` (ninox-tui) process inside a real tmux pane via
//! `tmux send-keys` — the same technique a malicious or confused peer agent
//! process could use to inject keystrokes, since nothing at the pty layer
//! distinguishes an injected byte from one a human typed.
//!
//! Task 12's reviewer already proved by hand-tracing
//! `crates/ninox-tui/src/tui/user_requests.rs` that a `sync` row has no
//! reachable code path to `Store::answer_user_request`: `draft_key`'s
//! `Enter` arm is the only place `Action::AnswerUserRequest` is built, and
//! `user_requests_draft` is only ever set to `Some` by
//! `open_selected_request`'s `Async` arm. These two tests confirm that
//! structural proof through the real keymap, terminal, and pty stack rather
//! than by re-reading the code.
//!
//! Requires a `tmux` binary; each test checks for one at runtime and skips
//! (prints and returns, still passing) rather than failing the build when
//! it's absent, since CI environments may not ship `tmux`.

use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use ninox_core::store::{Store, UserRequestRow};
use ninox_core::types::{UserRequestMode, UserRequestStatus};

fn tmux_available() -> bool {
    Command::new("tmux").arg("-V").output().map(|o| o.status.success()).unwrap_or(false)
}

/// Single-quote shell-escape, matching `ninox_core::tmux`'s own convention
/// (that module is private to `ninox-core`, so this test — which shells out
/// to the real `tmux` binary directly rather than adding a dependency on
/// it — keeps its own small copy).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn run(socket: &str, args: &[&str]) {
    let mut cmd = Command::new("tmux");
    cmd.arg("-L").arg(socket).args(args);
    let status = cmd.status().unwrap_or_else(|e| panic!("tmux {args:?} failed to run: {e}"));
    assert!(status.success(), "tmux {args:?} exited non-zero");
}

/// A dedicated tmux server (private `-L` socket) hosting one session that
/// runs the real `nx` binary against an isolated test store. Owns an
/// ephemeral `TcpListener` bound to the port handed to `nx` so
/// `ninox_core::daemon::ensure_daemon`'s `port_in_use` probe reports
/// `AlreadyRunning` and never spawns a second, real, detached `ninox
/// --headless` daemon process as a side effect of this test.
///
/// Isolation note: `ninox_core::tmux::config_path()` (the file
/// `dispatch()` writes on every bare `nx` launch via
/// `tmux::write_server_config()`) and `AppConfig::ninox_bin_dir()` (where
/// `install_wrappers()`/`install_self_shim()` write) are keyed off
/// `dirs::config_dir()`/`dirs::data_local_dir()`, NOT off
/// `NINOX_TMUX_SOCKET` — that var only redirects which `-L` socket actual
/// tmux *session* commands target (`tmux::socket()`), a separate concern
/// from where the config *file* lands. The real isolation for the
/// file-write side is overriding `HOME` for the child process, since on
/// macOS both of those `dirs::` functions resolve under `$HOME/Library/
/// Application Support`. `_fake_home` keeps that throwaway directory alive
/// for the child's lifetime. `NINOX_TMUX_SOCKET` and `NINOX_PTYD_SOCKET`
/// are set too, matching `docs/terminal-native-runtime-testing.md`'s "0.
/// Isolated first" convention, so that even if the TUI did attempt a tmux
/// session op or a ptyd connect (it shouldn't here — no `Session` rows are
/// seeded), neither would reach the real `-L ninox` server or the real
/// `ptyd.sock`.
struct TmuxTui {
    socket: String,
    session: String,
    _port_guard: TcpListener,
    _fake_home: tempfile::TempDir,
}

impl TmuxTui {
    fn spawn(test_name: &str, db_path: &Path) -> Self {
        let socket = format!("ninox-test-{}-{test_name}", std::process::id());
        let session = "nx".to_string();

        let port_guard = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
        let port = port_guard.local_addr().unwrap().port();

        let fake_home = tempfile::tempdir().expect("fake HOME for isolation");
        // Doesn't exist under the fake HOME either, so `AppConfig::load()`
        // falls back to defaults rather than reading anything real.
        let no_config = fake_home.path().join("no-such-config.toml");
        let ptyd_socket = fake_home.path().join("ptyd.sock");
        let tmux_socket_override = format!("ninox-test-inner-{}-{test_name}", std::process::id());

        let nx_bin = env!("CARGO_BIN_EXE_nx");
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        let inner = format!(
            "{} --db {} --port {port}",
            shell_quote(nx_bin),
            shell_quote(db_path.to_str().unwrap()),
        );
        let shell_cmd = format!("{shell} -l -c {}", shell_quote(&inner));

        run(
            &socket,
            &[
                "new-session", "-d", "-s", &session, "-x", "100", "-y", "40",
                "-e", &format!("HOME={}", fake_home.path().display()),
                "-e", &format!("NINOX_CONFIG={}", no_config.display()),
                "-e", &format!("NINOX_PTYD_SOCKET={}", ptyd_socket.display()),
                "-e", &format!("NINOX_TMUX_SOCKET={tmux_socket_override}"),
                &shell_cmd,
            ],
        );

        Self { socket, session, _port_guard: port_guard, _fake_home: fake_home }
    }

    /// One `tmux send-keys` invocation with these key/literal arguments,
    /// e.g. `&["u"]` or `&["-l", "approved"]` or `&["Enter"]`.
    fn send(&self, args: &[&str]) {
        let mut full = vec!["send-keys", "-t", self.session.as_str()];
        full.extend_from_slice(args);
        run(&self.socket, &full);
    }

    /// The pane's current rendered content — the `Store` is the source of
    /// truth for whether a row was answered; this only proves what the
    /// screen showed.
    fn capture(&self) -> String {
        let out = Command::new("tmux")
            .args(["-L", &self.socket, "capture-pane", "-t", &self.session, "-p"])
            .output();
        match out {
            Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
            Err(e) => format!("<capture-pane failed: {e}>"),
        }
    }

    /// Polls the pane until it shows `needle`; panics with the last
    /// capture after 15s (arbitrary, generous for a cold debug build).
    fn wait_for(&self, needle: &str, what: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            let pane = self.capture();
            if pane.contains(needle) {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "never reached {what}; pane:\n{pane}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for TmuxTui {
    fn drop(&mut self) {
        let _ = Command::new("tmux").args(["-L", &self.socket, "kill-server"]).status();
    }
}

fn seed_store(db_path: &Path, mode: UserRequestMode) -> (Store, String) {
    let store = Store::open(db_path).unwrap();
    let id = format!("ur-{mode:?}").to_lowercase();
    store
        .insert_user_request(&UserRequestRow {
            id: id.clone(),
            requester_session_id: "worker-1".to_string(),
            prompt: "merge now?".to_string(),
            mode,
            status: UserRequestStatus::Pending,
            answer: None,
            answered_at: None,
            answer_delivered_at: None,
            created_at: 0,
        })
        .unwrap();
    (store, id)
}

/// Opens the pending-requests screen and selects the (only) row: `u` from
/// the board's sidebar focus (the default focus/view at launch — see
/// `command()`'s `KeyCode::Char('u') => st.switch_view(View::UserRequests)`
/// in `crates/ninox-tui/src/tui/state.rs`), then `Enter` on row 0 (the
/// default `user_requests_selected`).
fn open_and_select(tui: &TmuxTui) {
    // Waits on what the pane shows rather than a fixed sleep: a cold
    // first launch can take well over a second to draw.
    tui.wait_for("pending user request", "the board's footer badge (startup + first refresh)");
    tui.send(&["u"]);
    tui.wait_for("1 pending request", "the pending-requests screen");
    tui.send(&["Enter"]);
    std::thread::sleep(Duration::from_millis(400));
}

#[test]
fn tmux_pane_injection_cannot_answer_a_sync_request() {
    if !tmux_available() {
        eprintln!("skipping tmux_pane_injection_cannot_answer_a_sync_request: tmux not available");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("t.db");
    let (store, id) = seed_store(&db_path, UserRequestMode::Sync);

    let tui = TmuxTui::spawn("sync", &db_path);
    open_and_select(&tui);

    // A confused/malicious peer could keep injecting bytes here — there is
    // simply nothing for them to land on, since a `Sync` row never opens
    // the draft input. Drive a plausible "try to answer anyway" sequence.
    tui.send(&["-l", "yes"]);
    std::thread::sleep(Duration::from_millis(300));
    tui.send(&["Enter"]);
    std::thread::sleep(Duration::from_millis(600));

    let row = store.get_user_request(&id).unwrap().expect("row must still exist");
    let pane = tui.capture();
    assert_eq!(
        row.status,
        UserRequestStatus::Pending,
        "a sync row must be unanswerable via injected tmux keystrokes; pane:\n{pane}",
    );
    assert!(row.answer.is_none());
    // Proves the TUI genuinely reached the requests screen and rendered the
    // sync row read-only — a TUI that crashed on startup would leave the row
    // Pending too.
    assert!(pane.contains("REQUESTS") && pane.contains("SYNC") && pane.contains("merge now?"), "pane:\n{pane}");
    assert!(pane.contains("sync requests can only be answered from ninox-gui"), "pane:\n{pane}");
}

#[test]
fn tmux_pane_injection_can_answer_an_async_request_the_documented_weaker_tier() {
    if !tmux_available() {
        eprintln!(
            "skipping tmux_pane_injection_can_answer_an_async_request_the_documented_weaker_tier: tmux not available"
        );
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("t.db");
    let (store, id) = seed_store(&db_path, UserRequestMode::Async);

    let tui = TmuxTui::spawn("async", &db_path);
    open_and_select(&tui);

    // `Enter` above opened the draft input (the `Async` arm of
    // `open_selected_request`); type an answer and submit it.
    tui.send(&["-l", "approved"]);
    std::thread::sleep(Duration::from_millis(300));
    tui.send(&["Enter"]);
    std::thread::sleep(Duration::from_millis(800));

    let row = store.get_user_request(&id).unwrap().expect("row must still exist");
    assert_eq!(
        row.status,
        UserRequestStatus::Answered,
        "an async row IS answerable via injected tmux keystrokes — the documented weaker tier; pane:\n{}",
        tui.capture()
    );
    assert_eq!(row.answer.as_deref(), Some("approved"));
}
