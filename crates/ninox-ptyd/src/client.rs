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
        match Self::connect(socket, client_name).await {
            Ok(c) => return Ok(c),
            // A live host that rejected us (e.g. version mismatch) must not
            // be "fixed" by spawning a second one.
            Err(e) if e.downcast_ref::<HostError>().is_some() => return Err(e),
            Err(_) => {}
        }
        spawn_detached(host_argv, log.as_deref())?;
        let deadline = tokio::time::Instant::now() + timeout;
        let mut last_err;
        loop {
            match Self::connect(socket, client_name).await {
                Ok(c) => return Ok(c),
                Err(e) if e.downcast_ref::<HostError>().is_some() => return Err(e),
                Err(e) => last_err = e,
            }
            if tokio::time::Instant::now() >= deadline {
                let hint = log.as_ref().map(|l| format!(" (see {})", l.display())).unwrap_or_default();
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
        let mut c = Self::connect(socket, "subscription").await?;
        c.expect_ok(Request::Subscribe { pane: pane.into(), mode }, &[]).await?;
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
