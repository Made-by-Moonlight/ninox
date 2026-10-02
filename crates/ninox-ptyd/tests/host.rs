//! End-to-end tests against a real host process and real child processes.

mod common;

use std::io::Write as _;
use std::time::{Duration, Instant};

use common::*;
use ninox_ptyd::client::{is_not_found, HostError};
use ninox_ptyd::{checkpoint, ErrorCode, Event, PtydClient, SubscribeMode};

#[tokio::test]
async fn spawn_list_info_screen() {
    let h = TestHost::start().await;
    let mut c = h.client().await;
    let pid = c.spawn(spec("p1", "printf 'hello\\nworld'; printf '\\033]2;my title\\007'; sleep 30")).await.unwrap();
    assert!(pid > 1);
    let s = wait_screen(&mut c, "p1", 0, |t| t.contains("world")).await;
    assert_eq!(s.lines.len(), 24);
    assert_eq!(s.cols, 80);
    assert!(s.to_plain_text().starts_with("hello\nworld\n"));
    assert!(s.seq > 0);

    wait_until("title", || async { PtydClient::connect(&h.socket, "t").await.unwrap().info("p1").await.unwrap().unwrap().title.is_some() }).await;
    let info = c.info("p1").await.unwrap().unwrap();
    assert_eq!(info.pid, pid);
    assert!(info.alive);
    assert_eq!(info.title.as_deref(), Some("my title"));
    assert_eq!(info.cwd, "/tmp");
    assert!(info.last_output_ms >= info.created_ms);
    assert!(info.seq >= s.seq);

    let list = c.list().await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].pane, "p1");

    assert!(c.info("nope").await.unwrap().is_none());
    let e = c.screen("nope", 0).await.unwrap_err();
    assert!(is_not_found(&e));

    // Same id while running is refused.
    let e = c.spawn(spec("p1", "true")).await.unwrap_err();
    assert_eq!(e.downcast_ref::<HostError>().unwrap().0.code, ErrorCode::AlreadyExists);
    // Bad cwd is a spawn failure, not a silent fallback.
    let mut bad = spec("p2", "true");
    bad.cwd = "/definitely/not/here".into();
    let e = c.spawn(bad).await.unwrap_err();
    assert_eq!(e.downcast_ref::<HostError>().unwrap().0.code, ErrorCode::SpawnFailed);
    c.shutdown().await.unwrap();
}

#[tokio::test]
async fn write_reaches_the_process() {
    let h = TestHost::start().await;
    let mut c = h.client().await;
    c.spawn(spec("cat", "stty -echo; echo ready; exec cat")).await.unwrap();
    wait_screen(&mut c, "cat", 0, |t| t.contains("ready")).await;
    c.write("cat", b"ping-123\r").await.unwrap();
    wait_screen(&mut c, "cat", 0, |t| t.contains("ping-123")).await;
}

#[tokio::test]
async fn history_matches_capture_pane_semantics() {
    let h = TestHost::start().await;
    let mut c = h.client().await;
    let mut sp = spec("h", "i=1; while [ $i -le 100 ]; do echo \"line $i\"; i=$((i+1)); done; sleep 30");
    sp.rows = 10;
    c.spawn(sp).await.unwrap();
    wait_screen(&mut c, "h", 0, |t| t.contains("line 100")).await;
    let info = c.info("h").await.unwrap().unwrap();
    // 100 lines + the cursor line on a 10-row screen.
    assert_eq!(info.history_size, 91);

    let hs = info.history_size as i64;
    let all = String::from_utf8(c.history("h", -hs, -1).await.unwrap()).unwrap();
    let lines: Vec<&str> = all.lines().collect();
    assert_eq!(lines.len(), 91);
    assert_eq!(lines[0], "line 1");
    assert_eq!(lines[90], "line 91");

    // Visible screen starts at 0; out-of-range bounds clamp like tmux.
    let top = String::from_utf8(c.history("h", 0, 0).await.unwrap()).unwrap();
    assert_eq!(top, "line 92\n");
    let clamped = String::from_utf8(c.history("h", -10_000, 10_000).await.unwrap()).unwrap();
    assert_eq!(clamped.lines().count(), 101);

    let s = c.screen("h", 5).await.unwrap();
    assert_eq!(s.scrollback_len, 5);
    assert_eq!(s.lines.len(), 15);
    assert!(s.to_plain_text().starts_with("line 87\n"));
}

#[tokio::test]
async fn resize_reaches_pty_and_emulator() {
    let h = TestHost::start().await;
    let mut c = h.client().await;
    c.spawn(spec("r", "echo ready; read x; stty size; sleep 30")).await.unwrap();
    wait_screen(&mut c, "r", 0, |t| t.contains("ready")).await;
    c.resize("r", 50, 20).await.unwrap();
    c.write("r", b"\r").await.unwrap();
    let s = wait_screen(&mut c, "r", 0, |t| t.contains("20 50")).await;
    assert_eq!((s.cols, s.rows), (50, 20));
    assert_eq!(s.lines.len(), 20);
    let info = c.info("r").await.unwrap().unwrap();
    assert_eq!((info.cols, info.rows), (50, 20));
}

#[tokio::test]
async fn exit_code_kept_until_kill() {
    let h = TestHost::start().await;
    let mut c = h.client().await;
    c.spawn(spec("x", "echo bye; exit 7")).await.unwrap();
    wait_until("exit", || async {
        let mut c = PtydClient::connect(&h.socket, "t").await.unwrap();
        !c.info("x").await.unwrap().unwrap().alive
    })
    .await;
    let info = c.info("x").await.unwrap().unwrap();
    assert_eq!(info.exit_code, Some(7));
    // Dead panes stay listed with their final screen.
    assert!(c.screen("x", 0).await.unwrap().to_plain_text().contains("bye"));
    assert_eq!(c.list().await.unwrap().len(), 1);

    c.kill("x").await.unwrap();
    assert!(c.list().await.unwrap().is_empty());
    // Idempotent, and unknown panes are fine.
    c.kill("x").await.unwrap();
    c.kill("never-existed").await.unwrap();

    // A killed or exited id can be reused.
    c.spawn(spec("x", "exit 3")).await.unwrap();
    wait_until("second exit", || async {
        let mut c = PtydClient::connect(&h.socket, "t").await.unwrap();
        c.info("x").await.unwrap().unwrap().exit_code == Some(3)
    })
    .await;
    c.spawn(spec("x", "sleep 30")).await.unwrap();
    assert!(c.info("x").await.unwrap().unwrap().alive);
}

#[tokio::test]
async fn kill_escalates_to_sigkill_for_the_whole_group() {
    let h = TestHost::start().await;
    let mut c = h.client().await;
    // The shell ignores SIGHUP, and a background child sits in its group.
    let pid = c.spawn(spec("k", "trap '' HUP; sleep 100 & echo started; wait")).await.unwrap();
    wait_screen(&mut c, "k", 0, |t| t.contains("started")).await;
    c.kill("k").await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(pid_alive(pid), "SIGHUP alone should not have killed a HUP-ignoring shell");
    let deadline = Instant::now() + Duration::from_secs(6);
    while pid_alive(pid) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!pid_alive(pid), "process survived Kill");
    // The background sleep was in the same group and must be gone too.
    let out = std::process::Command::new("pgrep").args(["-g", &pid.to_string()]).output().unwrap();
    assert!(out.stdout.is_empty(), "group members survived: {:?}", String::from_utf8_lossy(&out.stdout));
}

#[tokio::test]
async fn raw_subscription_repaints_then_streams() {
    let h = TestHost::start().await;
    let mut c = h.client().await;
    c.spawn(spec("raw", "printf 'first'; read x; printf 'second\\033[6n'; read y; exit 4")).await.unwrap();
    wait_screen(&mut c, "raw", 0, |t| t.contains("first")).await;

    let mut sub = PtydClient::subscribe(&h.socket, "raw", SubscribeMode::Raw).await.unwrap();
    let (ev, bytes) = sub.next().await.unwrap().unwrap();
    assert!(matches!(ev, Event::Output { .. }));
    let repaint = String::from_utf8_lossy(&bytes).to_string();
    assert!(repaint.contains("\x1b[2J") && repaint.contains("first"), "{repaint:?}");

    c.write("raw", b"\r").await.unwrap();
    let mut streamed = Vec::new();
    while !String::from_utf8_lossy(&streamed).contains("second") {
        let (ev, bytes) = tokio::time::timeout(Duration::from_secs(5), sub.next()).await.unwrap().unwrap().unwrap();
        assert!(matches!(ev, Event::Output { .. }));
        streamed.extend_from_slice(&bytes);
    }
    // The DSR query was answered by the host and not forwarded.
    assert!(!String::from_utf8_lossy(&streamed).contains("\x1b[6n"));
    c.write("raw", b"\r").await.unwrap();
    loop {
        let (ev, _) = tokio::time::timeout(Duration::from_secs(5), sub.next()).await.unwrap().unwrap().unwrap();
        if let Event::Exited { code, .. } = ev {
            assert_eq!(code, Some(4));
            break;
        }
    }
    assert!(sub.next().await.unwrap().is_none(), "raw stream closes after exit");
}

#[tokio::test]
async fn frames_subscription_notifies_changes_and_exit() {
    let h = TestHost::start().await;
    let mut c = h.client().await;
    c.spawn(spec("f", "stty -echo; echo one; read x; echo two; read y")).await.unwrap();
    wait_screen(&mut c, "f", 0, |t| t.contains("one")).await;
    let mut sub = PtydClient::subscribe(&h.socket, "f", SubscribeMode::Frames).await.unwrap();
    let Event::ScreenChanged { seq: first, .. } = sub.next().await.unwrap().unwrap().0 else { panic!("expected initial ScreenChanged") };

    c.write("f", b"\r").await.unwrap();
    let next = tokio::time::timeout(Duration::from_secs(5), sub.next()).await.unwrap().unwrap().unwrap().0;
    let Event::ScreenChanged { seq, .. } = next else { panic!("expected ScreenChanged, got {next:?}") };
    assert!(seq > first);
    let s = wait_screen(&mut c, "f", 0, |t| t.contains("two")).await;
    assert!(s.seq >= seq);

    c.write("f", b"\r").await.unwrap();
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), sub.next()).await.unwrap().unwrap().unwrap().0;
        if let Event::Exited { code, .. } = ev {
            assert_eq!(code, Some(0));
            break;
        }
    }
    // Frames streams stay open for dead panes and close when killed.
    c.kill("f").await.unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(5), sub.next()).await.unwrap().unwrap().is_none());
}

#[tokio::test]
async fn frames_are_coalesced() {
    let h = TestHost::start().await;
    let mut c = h.client().await;
    c.spawn(spec("busy", "read x; i=0; while [ $i -lt 3000 ]; do echo \"tick $i\"; i=$((i+1)); done; read y")).await.unwrap();
    let mut sub = PtydClient::subscribe(&h.socket, "busy", SubscribeMode::Frames).await.unwrap();
    sub.next().await.unwrap();
    c.write("busy", b"\r").await.unwrap();
    let start = Instant::now();
    let mut events = 0;
    wait_screen(&mut c, "busy", 0, |t| t.contains("tick 2999")).await;
    let elapsed = start.elapsed();
    while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(100), sub.next()).await.map(|r| r.ok().flatten()) {
        events += 1;
    }
    let max = (elapsed.as_millis() / 16) as usize + 3;
    assert!(events <= max, "{events} ScreenChanged in {elapsed:?} (max {max})");
    assert!(events >= 1);
}

#[tokio::test]
async fn submit_uses_bracketed_paste_then_enter() {
    let h = TestHost::start().await;
    let mut c = h.client().await;
    // cat -v makes the paste markers visible; canonical mode only delivers
    // the line once the separate CR arrives.
    c.spawn(spec("bp", "stty -echo; printf '\\033[?2004h'; echo ready; exec cat -v")).await.unwrap();
    wait_screen(&mut c, "bp", 0, |t| t.contains("ready")).await;
    wait_until("bracketed paste mode", || async {
        let mut c = PtydClient::connect(&h.socket, "t").await.unwrap();
        c.screen("bp", 0).await.unwrap().modes.bracketed_paste
    })
    .await;
    c.submit("bp", "hello world", true).await.unwrap();
    wait_screen(&mut c, "bp", 0, |t| t.contains("^[[200~hello world^[[201~")).await;

    c.spawn(spec("plain", "stty -echo; echo ready; exec cat -v")).await.unwrap();
    wait_screen(&mut c, "plain", 0, |t| t.contains("ready")).await;
    c.submit("plain", "no brackets", true).await.unwrap();
    let s = wait_screen(&mut c, "plain", 0, |t| t.contains("no brackets")).await;
    assert!(!s.to_plain_text().contains("200~"));
}

#[tokio::test]
async fn env_and_env_remove_are_honoured() {
    // Inherited by the host spawned below (and harmlessly by others).
    std::env::set_var("PTYD_TEST_LEAK", "leaked");
    let h = TestHost::start().await;
    let mut c = h.client().await;
    let mut sp = spec("env", "echo \"leak=${PTYD_TEST_LEAK:-unset} mine=$MINE term=$TERM\"; sleep 30");
    sp.env_remove = vec!["PTYD_TEST_LEAK".into()];
    sp.env = vec![("MINE".into(), "yes".into())];
    c.spawn(sp).await.unwrap();
    wait_screen(&mut c, "env", 0, |t| t.contains("leak=unset mine=yes term=xterm-256color")).await;

    let sp = spec("env2", "echo \"leak=${PTYD_TEST_LEAK:-unset}\"; sleep 30");
    c.spawn(sp).await.unwrap();
    wait_screen(&mut c, "env2", 0, |t| t.contains("leak=leaked")).await;
}

#[tokio::test]
async fn host_survives_bad_clients() {
    let h = TestHost::start().await;
    let mut c = h.client().await;
    c.spawn(spec("s", "echo alive; sleep 30")).await.unwrap();

    // Disconnect mid-frame.
    let mut s = std::os::unix::net::UnixStream::connect(&h.socket).unwrap();
    s.write_all(&100u32.to_le_bytes()).unwrap();
    s.write_all(b"partial").unwrap();
    drop(s);
    // Absurd length prefix.
    let mut s = std::os::unix::net::UnixStream::connect(&h.socket).unwrap();
    s.write_all(&u32::MAX.to_le_bytes()).unwrap();
    drop(s);
    // Garbage header after a valid handshake gets BadRequest, connection lives on.
    let mut raw = tokio::net::UnixStream::connect(&h.socket).await.unwrap();
    let hello = serde_json::json!({"id": 1, "request": {"type": "hello", "version": ninox_ptyd::PROTOCOL_VERSION, "client": "raw"}});
    ninox_ptyd::codec::write_frame(&mut raw, &hello, &[]).await.unwrap();
    ninox_ptyd::codec::read_frame(&mut raw).await.unwrap().unwrap();
    let bogus = serde_json::json!({"id": 2, "request": {"type": "no_such_request"}});
    ninox_ptyd::codec::write_frame(&mut raw, &bogus, &[]).await.unwrap();
    let reply: ninox_ptyd::HostFrame = ninox_ptyd::codec::read_frame(&mut raw).await.unwrap().unwrap().parse_header().unwrap();
    match reply {
        ninox_ptyd::HostFrame::Reply { id: 2, result: ninox_ptyd::ReplyResult::Err(e) } => assert_eq!(e.code, ErrorCode::BadRequest),
        other => panic!("unexpected {other:?}"),
    }
    // Subscriber that vanishes.
    let sub = PtydClient::subscribe(&h.socket, "s", SubscribeMode::Raw).await.unwrap();
    drop(sub);
    // Requests before Hello are refused.
    let mut raw2 = tokio::net::UnixStream::connect(&h.socket).await.unwrap();
    ninox_ptyd::codec::write_frame(&mut raw2, &serde_json::json!({"id": 9, "request": {"type": "list"}}), &[]).await.unwrap();
    let reply: ninox_ptyd::HostFrame = ninox_ptyd::codec::read_frame(&mut raw2).await.unwrap().unwrap().parse_header().unwrap();
    assert!(matches!(reply, ninox_ptyd::HostFrame::Reply { id: 9, result: ninox_ptyd::ReplyResult::Err(_) }));

    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut c2 = h.client().await;
    assert_eq!(c2.list().await.unwrap().len(), 1);
    wait_screen(&mut c, "s", 0, |t| t.contains("alive")).await;
    assert!(pid_alive(h.pid));
}

#[tokio::test]
async fn version_mismatch_is_reported() {
    let h = TestHost::start().await;
    let mut raw = tokio::net::UnixStream::connect(&h.socket).await.unwrap();
    let hello = serde_json::json!({"id": 1, "request": {"type": "hello", "version": 999, "client": "future"}});
    ninox_ptyd::codec::write_frame(&mut raw, &hello, &[]).await.unwrap();
    let reply: ninox_ptyd::HostFrame = ninox_ptyd::codec::read_frame(&mut raw).await.unwrap().unwrap().parse_header().unwrap();
    match reply {
        ninox_ptyd::HostFrame::Reply { result: ninox_ptyd::ReplyResult::Err(e), .. } => assert_eq!(e.code, ErrorCode::VersionMismatch),
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn second_host_refuses_and_stale_socket_is_replaced() {
    let h = TestHost::start().await;
    let out = std::process::Command::new(BIN)
        .args(["--socket", h.socket.to_str().unwrap(), "--no-checkpoints"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("already running"), "{}", String::from_utf8_lossy(&out.stderr));
    // The first host is unaffected.
    h.client().await.list().await.unwrap();

    // A socket file nobody listens on is stale and gets replaced.
    let dir = tempfile::Builder::new().prefix("ptyd").tempdir_in("/tmp").unwrap();
    let sock = dir.path().join("s.sock");
    drop(std::os::unix::net::UnixListener::bind(&sock).unwrap());
    assert!(socket_exists(&sock));
    let argv = vec![BIN.to_string(), "--socket".into(), sock.display().to_string(), "--no-checkpoints".into()];
    let mut c = PtydClient::connect_or_spawn(&sock, "t", &argv, None, Duration::from_secs(10)).await.unwrap();
    c.list().await.unwrap();
    c.shutdown().await.unwrap();
    wait_until("socket removed on shutdown", || async { !socket_exists(&sock) }).await;
}

#[tokio::test]
async fn shutdown_kills_panes_and_exits() {
    let h = TestHost::start().await;
    let mut c = h.client().await;
    let pid = c.spawn(spec("a", "sleep 100")).await.unwrap();
    c.shutdown().await.unwrap();
    wait_until("host exit", || async { !pid_alive(h.pid) }).await;
    wait_until("pane exit", || async { !pid_alive(pid) }).await;
    assert!(!socket_exists(&h.socket));
}

#[tokio::test]
async fn checkpoints_are_written_and_loadable() {
    let h = TestHost::start().await;
    let mut c = h.client().await;
    c.spawn(spec("ck", "echo checkpoint-me; read x; echo final-words; exit 0")).await.unwrap();
    wait_screen(&mut c, "ck", 0, |t| t.contains("checkpoint-me")).await;
    let dir = h.checkpoints.clone();
    wait_until("periodic checkpoint", || {
        let dir = dir.clone();
        async move { checkpoint::load(&dir, "ck").is_some_and(|k| k.screen.to_plain_text().contains("checkpoint-me")) }
    })
    .await;
    c.write("ck", b"\r").await.unwrap();
    wait_until("exit checkpoint", || {
        let dir = dir.clone();
        async move { checkpoint::load(&dir, "ck").is_some_and(|k| k.screen.to_plain_text().contains("final-words")) }
    })
    .await;
    let k = checkpoint::load(&dir, "ck").unwrap();
    assert_eq!(k.pane, "ck");
    assert!(k.saved_ms > 0);
    checkpoint::remove(&dir, "ck");
    assert!(checkpoint::load(&dir, "ck").is_none());
}

#[tokio::test]
async fn checkpoints_disabled() {
    let h = TestHost::start_with(false).await;
    let mut c = h.client().await;
    c.spawn(spec("n", "echo x; exit 0")).await.unwrap();
    wait_screen(&mut c, "n", 0, |t| t.contains('x')).await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(!h.checkpoints.exists());
}

#[tokio::test]
async fn spawned_host_is_not_a_descendant_of_its_spawner() {
    let h = TestHost::start().await;
    let out = std::process::Command::new("/bin/ps").args(["-o", "ppid=", "-p", &h.pid.to_string()]).output().unwrap();
    let ppid: u32 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
    assert_ne!(ppid, std::process::id(), "host must be reparented away from whoever spawned it");
}

#[tokio::test]
async fn host_survives_parent_terminal_hangup() {
    let h = TestHost::start().await;
    // The host ignores SIGHUP (it is a session leader without a terminal,
    // but a stray SIGHUP must still be harmless).
    unsafe {
        libc::kill(h.pid as i32, libc::SIGHUP);
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(pid_alive(h.pid));
    h.client().await.list().await.unwrap();
}

#[tokio::test]
async fn live_upgrade_hands_panes_to_successor() {
    let h = TestHost::start().await;
    let mut c = h.client().await;
    let (old_pid, epoch) = c.host_identity();
    let pid = c.spawn(spec("up", "stty -echo; printf '\\033[?2004h'; echo before; exec cat")).await.unwrap();
    wait_screen(&mut c, "up", 0, |t| t.contains("before")).await;
    drop(c);

    let log = std::fs::File::create(h.dir.path().join("successor.log")).unwrap();
    let mut successor = std::process::Command::new(BIN)
        .args(["--socket", h.socket.to_str().unwrap(), "--checkpoint-dir", h.checkpoints.to_str().unwrap(), "--takeover"])
        .stdin(std::process::Stdio::null())
        .stderr(log)
        .spawn()
        .unwrap();
    wait_until("old host exits", || async { !pid_alive(old_pid) }).await;
    let mut c = h.client().await;
    let (new_pid, new_epoch) = c.host_identity();
    assert_eq!(new_pid, successor.id());
    assert_eq!(new_epoch, epoch, "epoch survives a live upgrade");

    let info = c.info("up").await.unwrap().unwrap();
    assert_eq!(info.pid, pid);
    assert!(info.alive);
    assert!(pid_alive(pid), "agent process survived the upgrade");
    let s = c.screen("up", 0).await.unwrap();
    assert!(s.to_plain_text().contains("before"), "{}", s.to_plain_text());
    assert!(s.modes.bracketed_paste, "input modes carried over");

    // The inherited fd is live in both directions.
    c.write("up", b"after\r").await.unwrap();
    wait_screen(&mut c, "up", 0, |t| t.contains("after")).await;

    // Adopted panes can still be killed; exit is noticed without waitpid.
    c.kill("up").await.unwrap();
    wait_until("adopted pane gone", || async { !pid_alive(pid) }).await;
    c.shutdown().await.unwrap();
    let _ = successor.wait();
    let _ = std::fs::read_to_string(h.dir.path().join("successor.log"));
}

/// Every cell its own colour run: ~10 replay bytes per cell, so each pane's
/// full scrollback replays to more than one frame may carry.
#[tokio::test]
async fn live_upgrade_survives_a_busy_fleet() {
    let h = TestHost::start_with(false).await;
    let mut c = h.client().await;
    let script = "stty -echo; line=$(printf '\\033[31ma\\033[32mb%.0s' $(seq 100)); \
                  yes \"$line\" | head -n 12000; echo flood-done; exec cat";
    for pane in ["busy1", "busy2"] {
        let mut s = spec(pane, script);
        s.cols = 200;
        c.spawn(s).await.unwrap();
    }
    for pane in ["busy1", "busy2"] {
        wait_screen(&mut c, pane, 0, |t| t.contains("flood-done")).await;
    }
    let (old_pid, _) = c.host_identity();
    drop(c);

    let mut successor = std::process::Command::new(BIN)
        .args(["--socket", h.socket.to_str().unwrap(), "--no-checkpoints", "--takeover"])
        .stdin(std::process::Stdio::null())
        .stderr(std::fs::File::create(h.dir.path().join("successor.log")).unwrap())
        .spawn()
        .unwrap();
    wait_until("old host exits", || async { !pid_alive(old_pid) }).await;
    let mut c = h.client().await;
    assert_eq!(c.host_identity().0, successor.id(), "{}", h.log());
    for pane in ["busy1", "busy2"] {
        let info = c.info(pane).await.unwrap().unwrap();
        assert!(info.alive);
        assert!(info.history_size > 0, "some scrollback carried over");
        wait_screen(&mut c, pane, 0, |t| t.contains("flood-done")).await;
    }
    c.shutdown().await.unwrap();
    let _ = successor.wait();
}

#[tokio::test]
async fn takeover_without_running_host_starts_fresh() {
    let dir = tempfile::Builder::new().prefix("ptyd").tempdir_in("/tmp").unwrap();
    let sock = dir.path().join("s.sock");
    let mut child = std::process::Command::new(BIN)
        .args(["--socket", sock.to_str().unwrap(), "--no-checkpoints", "--takeover"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    wait_until("fresh host", || {
        let sock = sock.clone();
        async move { PtydClient::connect(&sock, "t").await.is_ok() }
    })
    .await;
    PtydClient::connect(&sock, "t").await.unwrap().shutdown().await.unwrap();
    assert!(child.wait().unwrap().success());
}

#[tokio::test]
async fn takeover_host_survives_terminal_signals_but_not_sigterm() {
    let dir = tempfile::Builder::new().prefix("ptyd").tempdir_in("/tmp").unwrap();
    let sock = dir.path().join("s.sock");
    let mut host = std::process::Command::new(BIN)
        .args(["--socket", sock.to_str().unwrap(), "--no-checkpoints", "--takeover"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    wait_until("takeover host", || {
        let sock = sock.clone();
        async move { PtydClient::connect(&sock, "t").await.is_ok() }
    })
    .await;
    let mut c = PtydClient::connect(&sock, "t").await.unwrap();
    let agent = c.spawn(spec("a", "exec sleep 30")).await.unwrap();
    let victim = c.spawn(spec("v", "exec sleep 30")).await.unwrap();

    for sig in [libc::SIGINT, libc::SIGQUIT, libc::SIGTSTP] {
        unsafe { libc::kill(host.id() as i32, sig) };
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut c = PtydClient::connect(&sock, "t").await.expect("host still serving");
    assert!(c.info("a").await.unwrap().unwrap().alive);
    assert!(pid_alive(agent));

    // Panes still get default SIGINT handling (nothing ignored leaked in).
    unsafe { libc::kill(victim as i32, libc::SIGINT) };
    wait_until("pane dies of SIGINT", || {
        let sock = sock.clone();
        async move {
            let mut c = PtydClient::connect(&sock, "t").await.unwrap();
            !c.info("v").await.unwrap().unwrap().alive
        }
    })
    .await;

    unsafe { libc::kill(host.id() as i32, libc::SIGTERM) };
    assert!(host.wait().unwrap().success());
    wait_until("pane killed by shutdown", || async { !pid_alive(agent) }).await;
}

#[tokio::test]
async fn aborted_handoff_leaves_old_host_working() {
    let h = TestHost::start().await;
    let mut c = h.client().await;
    c.spawn(spec("keep", "stty -echo; echo ready; exec cat")).await.unwrap();
    wait_screen(&mut c, "keep", 0, |t| t.contains("ready")).await;

    // A "successor" that asks for the panes and then vanishes.
    let mut raw = tokio::net::UnixStream::connect(&h.socket).await.unwrap();
    let hello = serde_json::json!({"id": 1, "request": {"type": "hello", "version": ninox_ptyd::PROTOCOL_VERSION, "client": "flaky"}});
    ninox_ptyd::codec::write_frame(&mut raw, &hello, &[]).await.unwrap();
    ninox_ptyd::codec::read_frame(&mut raw).await.unwrap().unwrap();
    ninox_ptyd::codec::write_frame(&mut raw, &serde_json::json!({"id": 2, "request": {"type": "handoff"}}), &[]).await.unwrap();
    ninox_ptyd::codec::read_frame(&mut raw).await.unwrap().unwrap();
    drop(raw);

    // Readers are thawed: output written after the abort still arrives.
    c.write("keep", b"still-here\r").await.unwrap();
    wait_screen(&mut c, "keep", 0, |t| t.contains("still-here")).await;
    assert!(pid_alive(h.pid));
}

#[tokio::test]
async fn oversized_request_does_not_poison_the_connection() {
    let h = TestHost::start().await;
    let mut c = h.client().await;
    c.spawn(spec("big", "sleep 30")).await.unwrap();
    let huge = vec![b'x'; ninox_ptyd::MAX_FRAME_BYTES + 1];
    assert!(c.write("big", &huge).await.is_err());
    assert_eq!(c.list().await.unwrap().len(), 1);
}
