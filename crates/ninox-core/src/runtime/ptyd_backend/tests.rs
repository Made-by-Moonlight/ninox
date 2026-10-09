use std::collections::VecDeque;

use super::*;

const INSTANT: Timing = Timing {
    submit_delay: Duration::ZERO,
    verify_delay: Duration::ZERO,
    poll_delay:   Duration::ZERO,
};

const EMPTY_PROMPT: &str = "● done\n╭────╮\n│ ❯                │\n╰────╯";

/// Returns `screens` in order (the last one repeats) and records input.
struct ScriptedPane {
    screens: VecDeque<String>,
    typed:   Vec<String>,
    writes:  Vec<Vec<u8>>,
}

impl ScriptedPane {
    fn new(screens: &[&str]) -> Self {
        Self { screens: screens.iter().map(|s| s.to_string()).collect(), typed: Vec::new(), writes: Vec::new() }
    }
    fn enters(&self) -> usize {
        self.writes.iter().filter(|w| w.as_slice() == b"\r").count()
    }
}

#[async_trait]
impl PaneIo for ScriptedPane {
    async fn visible_text(&mut self) -> String {
        if self.screens.len() > 1 {
            self.screens.pop_front().unwrap()
        } else {
            self.screens.front().cloned().unwrap_or_default()
        }
    }
    async fn type_text(&mut self, text: &str) -> Result<()> {
        self.typed.push(text.to_string());
        Ok(())
    }
    async fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.writes.push(bytes.to_vec());
        Ok(())
    }
}

fn info(pane: &str, pid: u32, alive: bool) -> PaneInfo {
    PaneInfo {
        pane: pane.into(),
        pid,
        cols: 140,
        rows: 50,
        alive,
        exit_code: None,
        created_ms: 1_700_000_000_000,
        last_output_ms: 0,
        title: None,
        cwd: "/tmp".into(),
        seq: 0, history_size: 0,
    }
}

#[test]
fn spawn_spec_runs_a_login_shell_with_the_tmux_env_plus_pane_identity() {
    let spec = spawn_spec(
        "worker-1",
        "/ws",
        "export PATH='/bin':\"$PATH\"; claude",
        &[("NINOX_SESSION", "worker-1"), ("TERM", "dumb")],
        Path::new("/data/ninox/ptyd.sock"),
        "/bin/zsh",
    )
    .unwrap();

    assert_eq!(spec.pane, "worker-1");
    assert_eq!(spec.cwd, "/ws");
    assert_eq!(spec.argv, ["/bin/zsh", "-l", "-c", "export PATH='/bin':\"$PATH\"; claude"]);
    assert_eq!((spec.cols, spec.rows), (140, 50));
    let env = |k: &str| spec.env.iter().filter(|(key, _)| key == k).map(|(_, v)| v.as_str()).collect::<Vec<_>>();
    assert_eq!(env("NINOX_SESSION"), ["worker-1"]);
    assert_eq!(env("NINOX_PANE_ID"), ["worker-1"]);
    assert_eq!(env("NINOX_PTYD_SOCKET"), ["/data/ninox/ptyd.sock"]);
    assert_eq!(env("TERM"), ["xterm-256color"], "the pane's TERM is fixed, never duplicated");
    assert_eq!(env("COLORTERM"), ["truecolor"]);
    for var in [
        "CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT", "CLAUDE_CODE_SESSION_ID", "CLAUDE_CODE_CHILD_SESSION",
        "CLAUDE_CODE_MESSAGING_TOKEN", "CODEX_THREAD_ID", "TMUX", "TMUX_PANE", "CLAUDE_CONFIG_DIR",
        "NINOX_CONFIG", "NINOX_BRAIN", "NINOX_CLAUDE_PROJECTS_DIR",
    ] {
        assert!(spec.env_remove.iter().any(|v| v == var), "{var} must be removed");
    }
}

#[test]
fn spawn_spec_rejects_env_keys_containing_equals() {
    assert!(spawn_spec("x", "/", "true", &[("A=B", "c")], Path::new("/s"), "/bin/sh").is_err());
}

#[test]
fn identity_requires_a_live_pane_and_process_ancestry() {
    let live = info("orch", 4242, true);
    let identity = identity_for("orch", Some(&live), 77, |pid| pid == 4242).unwrap();
    assert_eq!(identity.physical_tmux_name, "orch");
    assert_eq!(identity.pane_id, "ptyd:orch");
    assert_eq!(identity.pane_pid, 4242);
    assert_eq!(identity.pane_created_at, 1_700_000_000_000);
    assert_eq!(identity.server_epoch, "77");

    assert_eq!(identity_for("orch", Some(&live), 77, |_| false), None, "forged NINOX_PANE_ID");
    assert_eq!(identity_for("orch", None, 77, |_| true), None, "pane unknown to the host");
    assert_eq!(identity_for("orch", Some(&info("orch", 4242, false)), 77, |_| true), None, "exited pane");
    assert_eq!(identity_for("orch", Some(&info("other", 4242, true)), 77, |_| true), None);
}

#[test]
fn identity_ancestry_must_not_pass_through_the_host() {
    // Host 10 was started from inside orchestrator pane O (root 5); worker
    // pane W (root 20) is the host's child, so worker process 30 descends
    // from O's root only by way of the host.
    let parents: std::collections::HashMap<u32, u32> = [(30, 20), (20, 10), (10, 6), (6, 5), (5, 1), (7, 5)].into();
    assert!(!reaches_before(&parents, 30, 5, 10), "worker spoofing the orchestrator");
    assert!(reaches_before(&parents, 30, 20, 10), "worker in its own pane");
    assert!(reaches_before(&parents, 7, 5, 10), "orchestrator process in its own pane");
    assert!(reaches_before(&parents, 5, 5, 10), "the pane root itself");
    assert!(!reaches_before(&parents, 99, 5, 10), "unknown pid");
}

#[tokio::test]
async fn send_types_then_submits_once_when_the_prompt_clears() {
    let mut pane = ScriptedPane::new(&[EMPTY_PROMPT]);
    send_verified(&mut pane, "w", "[Ninox] hello there", INSTANT).await.unwrap();
    assert_eq!(pane.typed, ["[Ninox] hello there"]);
    assert_eq!(pane.enters(), 1);
}

#[tokio::test]
async fn send_re_enters_while_the_message_is_stuck_then_succeeds() {
    let stuck = "│ ❯ [Pasted text #1 +4 lines]   │";
    let mut pane = ScriptedPane::new(&[stuck, stuck, EMPTY_PROMPT]);
    send_verified(&mut pane, "w", "[Ninox] multi\nline", INSTANT).await.unwrap();
    assert_eq!(pane.enters(), 3);
}

#[tokio::test]
async fn send_errors_when_retries_are_exhausted() {
    let mut pane = ScriptedPane::new(&["│ ❯ [Ninox] never leaves   │"]);
    let err = send_verified(&mut pane, "w", "[Ninox] never leaves", INSTANT).await.unwrap_err();
    assert!(err.to_string().contains("still unsubmitted"), "{err}");
    assert_eq!(pane.enters(), 1 + SEND_VERIFY_ATTEMPTS as usize);
}

#[tokio::test]
async fn wake_nudges_an_empty_prompt_and_submits_it() {
    let nudged = "│ ❯ .                │";
    let mut pane = ScriptedPane::new(&[EMPTY_PROMPT, nudged]);
    wake_idle(&mut pane, INSTANT).await.unwrap();
    assert_eq!(pane.writes, [b".".to_vec(), b"\r".to_vec()]);
}

#[tokio::test]
async fn wake_never_touches_a_human_typing() {
    let mut pane = ScriptedPane::new(&["│ ❯ half a thought   │"]);
    wake_idle(&mut pane, INSTANT).await.unwrap();
    assert!(pane.writes.is_empty());

    let mut pane = ScriptedPane::new(&[EMPTY_PROMPT, "│ ❯ .and more │"]);
    wake_idle(&mut pane, INSTANT).await.unwrap();
    assert_eq!(pane.writes, [b".".to_vec()], "Enter withheld once a human typed after the nudge");
}

#[tokio::test]
async fn wake_submits_a_leftover_nudge() {
    let mut pane = ScriptedPane::new(&["│ ❯ .                │"]);
    wake_idle(&mut pane, INSTANT).await.unwrap();
    assert_eq!(pane.writes, [b"\r".to_vec()]);
}

#[tokio::test]
async fn wait_prompt_returns_when_the_prompt_is_drawn_or_times_out() {
    let mut pane = ScriptedPane::new(&["Welcome", "Loading…", EMPTY_PROMPT]);
    assert!(wait_prompt(&mut pane, Duration::from_secs(5), INSTANT).await);

    let mut pane = ScriptedPane::new(&["$ "]);
    assert!(!wait_prompt(&mut pane, Duration::from_millis(20), INSTANT).await);
}

#[test]
fn attach_and_host_argv_use_the_ninox_binary() {
    let backend = PtydBackend::new(PathBuf::from("/data/ninox/ptyd.sock"), "/opt/ninox".into());
    assert_eq!(backend.host_argv(), ["/opt/ninox", "ptyd"]);
    assert_eq!(backend.log_path(), PathBuf::from("/data/ninox/ptyd.log"));
}

#[tokio::test]
async fn attach_args_bridge_through_ninox_pane_attach() {
    let backend = PtydBackend::new(PathBuf::from("/nonexistent/ptyd.sock"), "/opt/ninox".into());
    assert_eq!(backend.attach_args("w").await, ["/opt/ninox", "pane", "attach", "w"]);
}

#[tokio::test]
async fn an_absent_host_knows_no_panes_and_kills_succeed() {
    let dir = tempfile::tempdir().unwrap();
    let backend = PtydBackend::new(dir.path().join("ptyd.sock"), "/opt/ninox".into());
    assert!(!backend.knows("w").await);
    assert!(!backend.has_session("w").await);
    assert!(backend.list_sessions().await.unwrap().is_empty());
    backend.kill_session("w").await.unwrap();
}

#[tokio::test]
async fn an_absent_host_answers_but_a_stale_socket_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("ptyd.sock");
    let backend = PtydBackend::new(socket.clone(), "/nonexistent/ninox".into());
    assert!(backend.answering().await, "no socket: no host ever ran, so no ptyd pane exists");
    backend.ensure_answering(false).await.expect("nothing to start when ptyd isn't configured");

    // A socket path nothing listens on (stale after a reboot, or mid-takeover).
    let _listener_gone = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    drop(_listener_gone);
    assert!(!backend.answering().await);
}

#[test]
fn viewer_pane_ids_round_trip_and_are_recognised() {
    use crate::runtime::{is_viewer_pane, parse_viewer_pane, viewer_pane_id};
    let id = viewer_pane_id(4242, "fix-login");
    assert_eq!(id, "tmux-view:4242:fix-login");
    assert!(is_viewer_pane(&id));
    assert_eq!(parse_viewer_pane(&id), Some((4242, "fix-login")));
    assert!(!is_viewer_pane("fix-login"));
    assert_eq!(parse_viewer_pane("tmux-view:nope:x"), None);
    assert_eq!(parse_viewer_pane("tmux-view:12:"), None);
}

#[test]
fn live_sessions_skip_viewer_and_dead_panes() {
    let viewer = crate::runtime::viewer_pane_id(1, "w");
    let panes = vec![info("w", 10, true), info(&viewer, 11, true), info("gone", 12, false)];
    let ids: Vec<String> = live_sessions(panes).into_iter().map(|s| s.id).collect();
    assert_eq!(ids, ["w"]);
}

#[test]
fn viewer_spec_strips_tmux_nesting_vars() {
    let spec = viewer_spawn_spec("tmux-view:1:w", vec!["tmux".into(), "attach".into()], 100, 30);
    assert!(spec.env_remove.iter().any(|k| k == "TMUX"));
    assert!(spec.env.iter().any(|(k, v)| k == "TERM" && v == "xterm-256color"));
    assert_eq!((spec.cols, spec.rows), (100, 30));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_real_host_hides_viewer_panes_from_every_session_path() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("ptyd.sock");
    let host = tokio::spawn(ninox_ptyd::run_host(socket.clone(), None));
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut client = loop {
        match PtydClient::connect(&socket, "test").await {
            Ok(c) => break c,
            Err(e) if std::time::Instant::now() > deadline => panic!("host never came up: {e}"),
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    };
    let spec = |pane: &str| SpawnSpec {
        pane: pane.into(),
        argv: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
        cwd: "/".into(),
        env: vec![],
        env_remove: vec![],
        cols: 80,
        rows: 24,
    };
    let viewer = crate::runtime::viewer_pane_id(std::process::id(), "w1");
    client.spawn(spec("w1")).await.unwrap();
    client.spawn(spec(&viewer)).await.unwrap();

    let backend = PtydBackend::new(socket.clone(), "/opt/ninox".into());
    assert!(backend.knows("w1").await);
    assert!(!backend.knows(&viewer).await, "a viewer must never route a session to ptyd");
    assert!(!backend.has_session(&viewer).await);
    assert_eq!(backend.session_pid(&viewer).await, None);
    let ids: Vec<String> = backend.list_sessions().await.unwrap().into_iter().map(|s| s.id).collect();
    assert_eq!(ids, ["w1"]);

    client.shutdown().await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(5), host).await;
}
