//! Async client for `ninox-ptyd`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use tokio::net::UnixStream;

use tokio::io::AsyncWriteExt;

use crate::codec::read_frame;
use crate::protocol::*;

/// Upper bound on one request/response round trip. Every host operation is
/// local and short; hitting this means the host is wedged.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// One request/response connection. Not `Clone`; callers that share it wrap
/// it in a `tokio::sync::Mutex`, or just connect per operation (cheap: a
/// local socket connect + `Hello`).
pub struct PtydClient {
    stream: UnixStream,
    next_id: u64,
    host_pid: u32,
    epoch_ms: u64,
    /// Set when a request failed mid-flight (I/O error or timeout): the
    /// stream may be mid-frame, so it is never reused.
    broken: bool,
}

/// An event stream for one pane (a dedicated connection after `Subscribe`).
pub struct Subscription {
    stream: UnixStream,
}

impl PtydClient {
    /// Connect and handshake. Fails fast (no retry) if nothing listens.
    pub async fn connect(socket: &Path, client_name: &str) -> Result<Self> {
        let stream = UnixStream::connect(socket)
            .await
            .with_context(|| format!("connect to ninox-ptyd at {}", socket.display()))?;
        let mut client = Self { stream, next_id: 1, host_pid: 0, epoch_ms: 0, broken: false };
        let hello = Request::Hello { version: PROTOCOL_VERSION, client: client_name.to_string() };
        match client.request(hello, &[]).await? {
            (Reply::Hello { host_pid, epoch_ms, .. }, _) => {
                client.host_pid = host_pid;
                client.epoch_ms = epoch_ms;
                Ok(client)
            }
            (other, _) => bail!("unexpected handshake reply {other:?}"),
        }
    }

    /// Connect, spawning `host_argv` (e.g. `[ninox, ptyd]`) detached in its
    /// own session with stdio to `log` if nothing listens, then polling until
    /// the socket answers or `timeout` elapses.
    pub async fn connect_or_spawn(
        socket: &Path,
        client_name: &str,
        host_argv: &[String],
        log: Option<PathBuf>,
        timeout: Duration,
    ) -> Result<Self> {
        crate::check_socket_path(socket)?;
        match Self::connect(socket, client_name).await {
            Ok(c) => return Ok(c),
            // A live host that rejected us (e.g. version mismatch) must not
            // be "fixed" by spawning a second one.
            Err(e) if e.downcast_ref::<HostError>().is_some() => return Err(e),
            Err(_) => {}
        }
        // The log is appended to across host runs: only what this spawn
        // writes says why it failed.
        let log_start = log.as_deref().and_then(|l| std::fs::metadata(l).ok()).map_or(0, |m| m.len());
        spawn_detached(host_argv, log.as_deref())?;
        let hint = log.as_ref().map(|l| format!(" (see {})", l.display())).unwrap_or_default();
        let deadline = tokio::time::Instant::now() + timeout;
        let mut last_err;
        loop {
            match Self::connect(socket, client_name).await {
                Ok(c) => return Ok(c),
                Err(e) if e.downcast_ref::<HostError>().is_some() => return Err(e),
                Err(e) => last_err = e,
            }
            if let Some(log) = log.as_deref().filter(|l| host_failure_since(l, log_start).is_some()) {
                // The report's `Caused by:` lines may still be on their way.
                tokio::time::sleep(Duration::from_millis(150)).await;
                let cause = host_failure_since(log, log_start).unwrap_or_default();
                bail!("ninox-ptyd failed to start: {cause}{hint}");
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(last_err.context(format!("ninox-ptyd did not come up within {timeout:?}{hint}")));
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// (host_pid, epoch_ms) from the handshake. The epoch identifies a host
    /// incarnation; it changes when the host restarts. A live upgrade
    /// (`run_host_takeover`) keeps the epoch, since the panes survive it.
    pub fn host_identity(&self) -> (u32, u64) {
        (self.host_pid, self.epoch_ms)
    }

    async fn request(&mut self, request: Request, payload: &[u8]) -> Result<(Reply, Vec<u8>)> {
        if self.broken {
            bail!("ninox-ptyd connection is broken; reconnect");
        }
        let id = self.next_id;
        self.next_id += 1;
        let frame = ClientFrame { id, request };
        // Encode up front: an oversized request is the caller's error and
        // must not poison the connection.
        let encoded = crate::codec::encode_frame(&frame, payload).context("encode ninox-ptyd request")?;
        let result = tokio::time::timeout(REQUEST_TIMEOUT, async {
            self.stream.write_all(&encoded).await.context("send to ninox-ptyd")?;
            loop {
                let raw = read_frame(&mut self.stream)
                    .await
                    .context("read from ninox-ptyd")?
                    .ok_or_else(|| anyhow!("ninox-ptyd closed the connection"))?;
                let host: HostFrame = raw.parse_header().context("decode ninox-ptyd reply")?;
                match host {
                    HostFrame::Reply { id: rid, result } if rid == id => {
                        return match result {
                            ReplyResult::Ok(r) => Ok((r, raw.payload)),
                            ReplyResult::Err(e) => Err(anyhow::Error::new(HostError(e))),
                        };
                    }
                    // Stale reply (an earlier request timed out) or a stray
                    // event: skip.
                    _ => continue,
                }
            }
        })
        .await
        .map_err(|_| anyhow!("ninox-ptyd request timed out after {REQUEST_TIMEOUT:?}"))
        .and_then(|r| r);
        if let Err(e) = &result {
            if e.downcast_ref::<HostError>().is_none() {
                self.broken = true;
            }
        }
        result
    }

    async fn expect_ok(&mut self, request: Request, payload: &[u8]) -> Result<()> {
        match self.request(request, payload).await? {
            (Reply::Ok, _) => Ok(()),
            (other, _) => bail!("unexpected reply {other:?}"),
        }
    }

    pub async fn spawn(&mut self, spec: SpawnSpec) -> Result<u32> {
        match self.request(Request::Spawn(spec), &[]).await? {
            (Reply::Spawned { pid }, _) => Ok(pid),
            (other, _) => bail!("unexpected reply {other:?}"),
        }
    }
    pub async fn kill(&mut self, pane: &str) -> Result<()> {
        self.expect_ok(Request::Kill { pane: pane.into() }, &[]).await
    }
    pub async fn write(&mut self, pane: &str, bytes: &[u8]) -> Result<()> {
        self.expect_ok(Request::Write { pane: pane.into() }, bytes).await
    }
    pub async fn submit(&mut self, pane: &str, text: &str, enter: bool) -> Result<()> {
        self.expect_ok(Request::Submit { pane: pane.into(), text: text.into(), enter }, &[]).await
    }
    pub async fn resize(&mut self, pane: &str, cols: u16, rows: u16) -> Result<()> {
        self.expect_ok(Request::Resize { pane: pane.into(), cols, rows }, &[]).await
    }
    pub async fn list(&mut self) -> Result<Vec<PaneInfo>> {
        match self.request(Request::List, &[]).await? {
            (Reply::Panes { panes }, _) => Ok(panes),
            (other, _) => bail!("unexpected reply {other:?}"),
        }
    }
    /// `Ok(None)` when the pane is unknown.
    pub async fn info(&mut self, pane: &str) -> Result<Option<PaneInfo>> {
        match self.request(Request::Info { pane: pane.into() }, &[]).await {
            Ok((Reply::Info { pane }, _)) => Ok(Some(pane)),
            Ok((other, _)) => bail!("unexpected reply {other:?}"),
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }
    pub async fn screen(&mut self, pane: &str, scrollback: usize) -> Result<ScreenSnapshot> {
        match self.request(Request::Screen { pane: pane.into(), scrollback }, &[]).await? {
            (Reply::Screen { screen }, _) => Ok(screen),
            (other, _) => bail!("unexpected reply {other:?}"),
        }
    }
    pub async fn history(&mut self, pane: &str, start: i64, end: i64) -> Result<Vec<u8>> {
        match self.request(Request::History { pane: pane.into(), start, end }, &[]).await? {
            (Reply::History, payload) => Ok(payload),
            (other, _) => bail!("unexpected reply {other:?}"),
        }
    }
    pub async fn shutdown(&mut self) -> Result<()> {
        self.expect_ok(Request::Shutdown, &[]).await
    }

    /// Open a second connection to `socket` and subscribe it to `pane`.
    pub async fn subscribe(socket: &Path, pane: &str, mode: SubscribeMode) -> Result<Subscription> {
        Self::subscribe_with(socket, pane, mode, false).await
    }

    /// [`Self::subscribe`] with the protocol's `follow_size` option.
    pub async fn subscribe_with(socket: &Path, pane: &str, mode: SubscribeMode, follow_size: bool) -> Result<Subscription> {
        let mut c = Self::connect(socket, "subscription").await?;
        c.expect_ok(Request::Subscribe { pane: pane.into(), mode, follow_size }, &[]).await?;
        Ok(Subscription { stream: c.stream })
    }

    /// Turn this connection into the raw stream of a handoff (used by
    /// `run_host_takeover`).
    pub(crate) async fn into_handoff_stream(mut self) -> Result<UnixStream> {
        self.expect_ok(Request::Handoff, &[]).await?;
        Ok(self.stream)
    }
}

impl Subscription {
    /// Next event with its payload (raw bytes for `Output`, empty otherwise).
    /// `Ok(None)` when the host closed the stream.
    pub async fn next(&mut self) -> Result<Option<(Event, Vec<u8>)>> {
        loop {
            let Some(raw) = read_frame(&mut self.stream).await? else { return Ok(None) };
            match raw.parse_header::<HostFrame>()? {
                HostFrame::Event(ev) => return Ok(Some((ev, raw.payload))),
                HostFrame::Reply { .. } => continue,
            }
        }
    }
}

/// True when `e` is the host's `NotFound`.
pub fn is_not_found(e: &anyhow::Error) -> bool {
    e.downcast_ref::<HostError>().is_some_and(|h| h.0.code == ErrorCode::NotFound)
}

/// The fatal error a host that exited during startup wrote to `log` after
/// byte `start`, as one line. Several clients racing to start the host share
/// the log, so a loser's "another ninox-ptyd …" report is skipped: it means a
/// host exists, not that startup failed.
fn host_failure_since(log: &Path, start: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(log).ok()?;
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut tail = Vec::new();
    f.take(64 * 1024).read_to_end(&mut tail).ok()?;
    fatal_errors(&String::from_utf8_lossy(&tail)).into_iter().find(|e| !e.starts_with(crate::host::ANOTHER_HOST))
}

/// Every `Error: …` report a `main() -> anyhow::Result` printed on exit
/// (message, then an optional indented `Caused by:` list), each folded into
/// one line.
fn fatal_errors(output: &str) -> Vec<String> {
    let mut reports: Vec<Vec<String>> = Vec::new();
    let mut in_causes = false;
    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(message) = line.strip_prefix("Error: ") {
            reports.push(vec![message.trim().to_string()]);
            in_causes = false;
        } else if trimmed == "Caused by:" {
            in_causes = !reports.is_empty();
        } else if in_causes && line.starts_with(char::is_whitespace) && !trimmed.is_empty() {
            let cause = trimmed.split_once(": ").filter(|(n, _)| n.parse::<u32>().is_ok()).map_or(trimmed, |(_, c)| c);
            reports.last_mut().expect("in_causes implies a report").push(cause.to_string());
        } else if !trimmed.is_empty() {
            in_causes = false;
        }
    }
    reports.into_iter().map(|parts| parts.join(": ")).collect()
}

/// Start the host detached from the caller: double-forked, so it is
/// reparented to init/launchd rather than staying a descendant of whatever
/// pane spawned it (pane identity checks rely on agents not sharing an
/// ancestor with the host); in its own session, so the caller's terminal
/// closing or job-control signals never reach it; stdin from /dev/null,
/// stdout/stderr appended to `log` (or /dev/null).
fn spawn_detached(argv: &[String], log: Option<&Path>) -> Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let (prog, args) = argv.split_first().ok_or_else(|| anyhow!("empty ninox-ptyd argv"))?;
    let (out, errs) = match log {
        Some(path) => {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .with_context(|| format!("open ptyd log {}", path.display()))?;
            let f2 = f.try_clone()?;
            (Stdio::from(f), Stdio::from(f2))
        }
        None => (Stdio::null(), Stdio::null()),
    };
    let mut cmd = Command::new(prog);
    cmd.args(args).stdin(Stdio::null()).stdout(out).stderr(errs);
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            // The intermediate exits at once; the grandchild execs the host.
            // `spawn` still reports an exec failure: the grandchild inherits
            // std's close-on-exec error pipe.
            match libc::fork() {
                -1 => Err(std::io::Error::last_os_error()),
                0 => Ok(()),
                _ => libc::_exit(0),
            }
        });
    }
    let mut child = cmd.spawn().with_context(|| format!("spawn ninox-ptyd ({prog})"))?;
    let _ = child.wait();
    Ok(())
}

/// Typed error for `ReplyResult::Err`, recoverable via `anyhow::Error::downcast_ref`.
#[derive(Debug, Clone)]
pub struct HostError(pub ErrorBody);

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ptyd {:?}: {}", self.0.code, self.0.message)
    }
}

impl std::error::Error for HostError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fatal_reports_fold_their_causes_into_one_line_each() {
        let out = "2026-11-02T10:00:00Z  INFO starting\nError: bind /x/ptyd.sock\n\nCaused by:\n    path must be shorter than SUN_LEN\n";
        assert_eq!(fatal_errors(out), ["bind /x/ptyd.sock: path must be shorter than SUN_LEN"]);
        let numbered = "Error: a\n\nCaused by:\n    0: b\n    1: c\nError: lone\n  INFO later\n";
        assert_eq!(fatal_errors(numbered), ["a: b: c", "lone"]);
        assert!(fatal_errors("INFO all good\n").is_empty());
    }

    #[test]
    fn a_host_that_lost_the_startup_race_is_not_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("ptyd.log");
        let lost = format!("Error: {} is already running (lock /x/ptyd.sock.lock is held)\n", crate::host::ANOTHER_HOST);
        std::fs::write(&log, &lost).unwrap();
        assert_eq!(host_failure_since(&log, 0), None, "a racing client's host is (being) started by someone else");

        std::fs::write(&log, format!("{lost}Error: bind /x/ptyd.sock\n\nCaused by:\n    Permission denied\n")).unwrap();
        assert_eq!(host_failure_since(&log, 0).as_deref(), Some("bind /x/ptyd.sock: Permission denied"));
    }

    #[tokio::test]
    async fn a_host_that_dies_on_startup_reports_its_own_error_without_waiting_out_the_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let (socket, log) = (dir.path().join("ptyd.sock"), dir.path().join("ptyd.log"));
        std::fs::write(&log, "Error: an older run's failure\n").unwrap();
        // The cause arrives a beat after the headline, as a slow writer's would.
        let script = "printf 'Error: bind %s\\n' \"$0\" >&2; sleep 0.05; printf '\\nCaused by:\\n    path must be shorter than SUN_LEN\\n' >&2; exit 1";
        let argv: Vec<String> = ["/bin/sh", "-c", script, "/x/ptyd.sock"].map(String::from).to_vec();

        let started = std::time::Instant::now();
        let err = PtydClient::connect_or_spawn(&socket, "t", &argv, Some(log.clone()), Duration::from_secs(30))
            .await
            .err()
            .expect("the host never came up");
        assert!(started.elapsed() < Duration::from_secs(10), "waited {:?}", started.elapsed());
        assert_eq!(
            err.to_string(),
            format!("ninox-ptyd failed to start: bind /x/ptyd.sock: path must be shorter than SUN_LEN (see {})", log.display()),
        );
    }

    #[tokio::test]
    async fn an_overlong_socket_path_fails_before_spawning_anything() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("a".repeat(crate::max_socket_path_len())).join("ptyd.sock");
        let marker = dir.path().join("spawned");
        let argv: Vec<String> = ["/usr/bin/touch".to_string(), marker.display().to_string()].to_vec();
        let err = PtydClient::connect_or_spawn(&socket, "t", &argv, None, Duration::from_secs(1)).await.err().unwrap();
        assert!(err.to_string().starts_with("ptyd socket path is "), "{err}");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!marker.exists(), "no host may be spawned for a path that can never bind");
    }
}
