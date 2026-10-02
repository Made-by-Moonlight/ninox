//! Live upgrade: a successor host takes over every pane of a running host
//! without the agent processes noticing.
//!
//! Sequence (old = running host, new = `run_host_takeover`):
//!
//! 1. new connects to the normal socket, `Hello`s, binds its own listener at
//!    `<socket>.next` (not yet accepting), and sends `Request::Handoff`.
//! 2. old replies `Ok`; from here the connection leaves the request
//!    protocol. old refuses new spawns (`Unavailable`), *freezes* every
//!    reader thread (they stop consuming PTY output, so anything not yet
//!    read stays in the kernel buffer for new), flushes pending input
//!    writes, and builds a [`Manifest`].
//! 3. old sends the manifest frame, then one frame per pane whose payload is
//!    its *replay* (bytes that rebuild screen, scrollback and input modes in
//!    a fresh emulator, capped to fit a frame), then the fds over
//!    `SCM_RIGHTS`: the host lock fd first (sharing the `flock`, so no third
//!    host can start in between), then one PTY master per manifest pane, in
//!    batches.
//! 4. new rebuilds the panes *without starting them* and acks with
//!    `{"owned": true}`.
//! 5. old replies `{"released": true}`. Writing that frame is the commit
//!    point: from here old never touches the panes again.
//! 6. new starts the pane threads, atomically renames `<socket>.next` over
//!    `<socket>` (clients never see a missing socket) and sends
//!    `{"renamed": true}`.
//! 7. old, on that frame (or EOF, or a short timeout), stops without killing
//!    anything and exits; its client connections drop, and clients
//!    reconnect to new, which keeps old's epoch.
//!
//! Before the commit point, any failure leaves old owning everything: it
//! thaws its readers and carries on, and new exits with an error having
//! started nothing and renamed nothing. Adopted panes are not children of
//! new, so their exit is detected by polling and reported with `code: None`.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::codec::{read_frame_blocking, write_frame_blocking};
use crate::engine::{AlacrittyEngine, TerminalEngine, DEFAULT_SCROLLBACK};
use crate::host::{self, Host, Signals, Stop};
use crate::pane::{self, lock, PaneMeta, PaneSetup, PendingPane, ReaderState};
use crate::protocol::*;

const HANDOFF_VERSION: u32 = 2;
/// Well below the per-message SCM_RIGHTS limits (253 Linux, 254 macOS).
const FDS_PER_MESSAGE: usize = 64;
/// How long old waits for new to rebuild every pane and ack.
const ACK_TIMEOUT: Duration = Duration::from_secs(30);
/// How long new waits for `released` after acking (old sends it at once).
const RELEASE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a committed old host keeps its listener for clients while new
/// renames its socket into place.
const RENAME_TIMEOUT: Duration = Duration::from_secs(5);
/// Per-pane replay cap, leaving room for the frame header.
const MAX_REPLAY_BYTES: usize = MAX_FRAME_BYTES - 64 * 1024;

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    version: u32,
    epoch_ms: u64,
    panes: Vec<HandoffPane>,
}

#[derive(Debug, Serialize, Deserialize)]
struct HandoffPane {
    meta: PaneMeta,
    adopted: bool,
    alive: bool,
    exit_code: Option<i32>,
    seq: u64,
    last_output_ms: u64,
    cols: u16,
    rows: u16,
}

/// Header of the frame carrying one pane's replay.
#[derive(Serialize, Deserialize)]
struct ReplayHeader {
    pane: PaneId,
}

#[derive(Serialize, Deserialize)]
struct Ack {
    owned: bool,
}

#[derive(Serialize, Deserialize)]
struct Released {
    released: bool,
}

#[derive(Serialize, Deserialize)]
struct Renamed {
    renamed: bool,
}

// ---------------------------------------------------------------------------
// Old host side
// ---------------------------------------------------------------------------

pub async fn serve_handoff(host: Arc<Host>, stream: tokio::net::UnixStream, id: u64) {
    let frame = HostFrame::Reply { id, result: ReplyResult::Ok(Reply::Ok) };
    let mut stream = stream;
    if crate::codec::write_frame(&mut stream, &frame, &[]).await.is_err() {
        return;
    }
    let std_stream = match stream.into_std() {
        Ok(s) => s,
        Err(_) => return,
    };
    let h = Arc::clone(&host);
    let outcome = tokio::task::spawn_blocking(move || {
        h.begin_handoff();
        let mut stream = std_stream;
        send_panes(&h, &mut stream).map(|()| stream)
    })
    .await;
    match outcome {
        Ok(Ok(stream)) => {
            tracing::info!("live upgrade: successor owns all panes");
            let _ = tokio::task::spawn_blocking(move || wait_renamed(stream)).await;
            host.stop(Stop::HandedOff);
        }
        Ok(Err(e)) => {
            tracing::warn!("live upgrade aborted: {e:#}");
            thaw(&host);
        }
        Err(e) => {
            tracing::error!("live upgrade panicked: {e}");
            thaw(&host);
        }
    }
}

fn thaw(host: &Host) {
    host.unfreeze_readers();
    let panes: Vec<_> = lock(&host.panes).values().cloned().collect();
    for p in panes {
        if let Err(e) = p.restart_reader(host.freeze.clone()) {
            tracing::error!(pane = %p.meta.id, "could not restart reader after aborted upgrade: {e}");
        }
    }
    host.abort_handoff();
}

/// Steps 2-5. `Ok` means committed: the successor owns the panes.
fn send_panes(host: &Host, stream: &mut StdUnixStream) -> Result<()> {
    stream.set_nonblocking(false)?;
    host.freeze_readers();
    let panes: Vec<_> = {
        let map = lock(&host.panes);
        let mut v: Vec<_> = map.values().cloned().collect();
        v.sort_by_key(|p| p.meta.created_ms);
        v
    };
    for p in &panes {
        // Readers wake from poll immediately; a reader mid-`ingest` finishes
        // its chunk first.
        if p.wait_reader_stopped(Duration::from_secs(2)) == ReaderState::Running {
            bail!("reader of pane {} did not stop", p.meta.id);
        }
        p.flush_writes(Duration::from_secs(1));
    }
    let mut manifest = Manifest { version: HANDOFF_VERSION, epoch_ms: host.epoch_ms, panes: Vec::new() };
    let mut fds: Vec<RawFd> = vec![host.lock_fd().ok_or_else(|| anyhow!("host lock fd unavailable"))?];
    for p in &panes {
        let info = p.info();
        manifest.panes.push(HandoffPane {
            meta: p.meta.clone(),
            adopted: p.adopted,
            alive: info.alive,
            exit_code: p.exit_code(),
            seq: info.seq,
            last_output_ms: p.last_output_ms(),
            cols: info.cols,
            rows: info.rows,
        });
        fds.push(p.master.as_raw_fd());
    }
    write_frame_blocking(stream, &manifest, &[]).context("send manifest")?;
    // One frame per pane, built as it is sent: a busy fleet's replays
    // together are far larger than one frame may be.
    for p in &panes {
        let replay = p.replay_bytes(MAX_REPLAY_BYTES);
        write_frame_blocking(stream, &ReplayHeader { pane: p.meta.id.clone() }, &replay)
            .with_context(|| format!("send replay of pane {}", p.meta.id))?;
    }
    for batch in fds.chunks(FDS_PER_MESSAGE) {
        send_fds(stream, batch).context("send fds")?;
    }
    stream.set_read_timeout(Some(ACK_TIMEOUT))?;
    let frame = read_frame_blocking(stream)?.ok_or_else(|| anyhow!("successor closed before ack"))?;
    let ack: Ack = frame.parse_header()?;
    if !ack.owned {
        bail!("successor declined the panes");
    }
    host.set_released();
    write_frame_blocking(stream, &Released { released: true }, &[]).context("release panes")?;
    Ok(())
}

/// Step 7: keep serving until new's socket is in place, so clients that
/// reconnect meanwhile still find a listener.
fn wait_renamed(mut stream: StdUnixStream) {
    let _ = stream.set_read_timeout(Some(RENAME_TIMEOUT));
    match read_frame_blocking(&mut stream) {
        Ok(Some(f)) if f.parse_header::<Renamed>().is_ok_and(|r| r.renamed) => {}
        other => tracing::warn!("successor did not confirm its socket ({:?}); stopping anyway", other.err()),
    }
}

// ---------------------------------------------------------------------------
// New host side
// ---------------------------------------------------------------------------

pub async fn run_takeover(socket: PathBuf, checkpoint_dir: Option<PathBuf>) -> Result<()> {
    host::ignore_sighup();
    host::prepare_socket_dir(&socket)?;
    let client = match crate::client::PtydClient::connect(&socket, "ninox-ptyd-takeover").await {
        Ok(c) => c,
        Err(e) => {
            tracing::info!("no host to take over ({e:#}); starting fresh");
            return host::run(socket, checkpoint_dir, Signals::Takeover).await;
        }
    };
    let next = next_path(&socket);
    let _ = std::fs::remove_file(&next);
    let listener = host::bind_at(&next)?;
    match take_panes(client, checkpoint_dir, &socket, &next).await {
        Ok(host) => host::serve(host, listener, socket, Signals::Takeover).await,
        Err(e) => {
            drop(listener);
            let _ = std::fs::remove_file(&next);
            Err(e)
        }
    }
}

fn next_path(socket: &Path) -> PathBuf {
    let mut s = socket.as_os_str().to_owned();
    s.push(".next");
    PathBuf::from(s)
}

/// Steps 1-6. On `Err` nothing was started or renamed, and every received
/// fd has been closed again.
async fn take_panes(
    client: crate::client::PtydClient,
    checkpoint_dir: Option<PathBuf>,
    socket: &Path,
    next: &Path,
) -> Result<Arc<Host>> {
    let stream = client.into_handoff_stream().await.context("old host refused the handoff")?;
    let std_stream = stream.into_std()?;
    let (manifest, replays, mut fds, std_stream) = tokio::task::spawn_blocking(move || receive(std_stream))
        .await
        .map_err(|e| anyhow!("handoff receive panicked: {e}"))??;
    let lock_fd = fds.remove(0);

    let host = Host::new(checkpoint_dir, manifest.epoch_ms)?;
    host.set_lock_fd(lock_fd);
    let mut pending: Vec<PendingPane> = Vec::with_capacity(manifest.panes.len());
    for ((hp, replay), master) in manifest.panes.into_iter().zip(replays).zip(fds) {
        let mut engine = AlacrittyEngine::new(hp.cols, hp.rows, DEFAULT_SCROLLBACK);
        engine.feed(&replay);
        let _ = engine.take_replies();
        let id = hp.meta.id.clone();
        let setup = PaneSetup {
            meta: hp.meta,
            master,
            engine: Box::new(engine),
            // Everything inherited is no longer our child.
            adopted: true,
            seq: hp.seq,
            last_output_ms: hp.last_output_ms,
            alive: hp.alive,
            exit_code: hp.exit_code,
        };
        match pane::prepare_pane(setup) {
            Ok(p) => pending.push(p),
            Err(e) => tracing::error!(pane = %id, "could not adopt pane: {e}"),
        }
    }

    let mut std_stream = tokio::task::spawn_blocking(move || -> Result<StdUnixStream> {
        let mut s = std_stream;
        write_frame_blocking(&mut s, &Ack { owned: true }, &[]).context("ack handoff")?;
        s.set_read_timeout(Some(RELEASE_TIMEOUT))?;
        let frame = read_frame_blocking(&mut s)
            .context("wait for release")?
            .ok_or_else(|| anyhow!("old host closed before releasing the panes"))?;
        if !frame.parse_header::<Released>()?.released {
            bail!("old host did not release the panes");
        }
        Ok(s)
    })
    .await
    .map_err(|e| anyhow!("{e}"))??;

    // Committed: the panes are ours.
    for p in pending {
        let id = p.id().to_string();
        match p.start(host.freeze.clone(), &host.rt, host.checkpoint_dir.clone()) {
            Ok(p) => {
                lock(&host.panes).insert(id, p);
            }
            Err(e) => tracing::error!(pane = %id, "could not start adopted pane: {e}"),
        }
    }
    std::fs::rename(next, socket).with_context(|| format!("rename {} over {}", next.display(), socket.display()))?;
    let _ = tokio::task::spawn_blocking(move || write_frame_blocking(&mut std_stream, &Renamed { renamed: true }, &[])).await;
    tracing::info!(panes = lock(&host.panes).len(), "live upgrade complete");
    Ok(host)
}

type Received = (Manifest, Vec<Vec<u8>>, Vec<OwnedFd>, StdUnixStream);

fn receive(mut stream: StdUnixStream) -> Result<Received> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let frame = read_frame_blocking(&mut stream)?.ok_or_else(|| anyhow!("old host closed during handoff"))?;
    let manifest: Manifest = frame.parse_header()?;
    if manifest.version != HANDOFF_VERSION {
        bail!("handoff version {} not supported (want {HANDOFF_VERSION})", manifest.version);
    }
    let mut replays = Vec::with_capacity(manifest.panes.len());
    for hp in &manifest.panes {
        let frame = read_frame_blocking(&mut stream)?.ok_or_else(|| anyhow!("old host closed during replays"))?;
        let header: ReplayHeader = frame.parse_header()?;
        if header.pane != hp.meta.id {
            bail!("replay for pane {} arrived where {} was expected", header.pane, hp.meta.id);
        }
        replays.push(frame.payload);
    }
    let want = manifest.panes.len() + 1;
    let mut fds = Vec::with_capacity(want);
    while fds.len() < want {
        let batch = recv_fds(&stream, (want - fds.len()).min(FDS_PER_MESSAGE))?;
        if batch.is_empty() {
            bail!("old host sent no fds");
        }
        fds.extend(batch);
    }
    if fds.len() != want {
        bail!("expected {want} fds, got {}", fds.len());
    }
    Ok((manifest, replays, fds, stream))
}

// ---------------------------------------------------------------------------
// SCM_RIGHTS
// ---------------------------------------------------------------------------

fn send_fds(stream: &StdUnixStream, fds: &[RawFd]) -> io::Result<()> {
    let data = [0u8; 1];
    let mut iov = libc::iovec { iov_base: data.as_ptr() as *mut _, iov_len: 1 };
    let fd_bytes = std::mem::size_of_val(fds) as u32;
    let space = unsafe { libc::CMSG_SPACE(fd_bytes) } as usize;
    let mut control = vec![0u8; space];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return Err(io::Error::other("no room for cmsg"));
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(fd_bytes) as _;
        std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(cmsg) as *mut RawFd, fds.len());
        loop {
            if libc::sendmsg(stream.as_raw_fd(), &msg, 0) >= 0 {
                return Ok(());
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }
}

fn recv_fds(stream: &StdUnixStream, max: usize) -> io::Result<Vec<OwnedFd>> {
    let mut data = [0u8; 1];
    let mut iov = libc::iovec { iov_base: data.as_mut_ptr().cast(), iov_len: 1 };
    let space = unsafe { libc::CMSG_SPACE((max * std::mem::size_of::<RawFd>()) as u32) } as usize;
    let mut control = vec![0u8; space];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    let n = loop {
        let n = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, 0) };
        if n >= 0 {
            break n;
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    };
    if n == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "handoff stream closed"));
    }
    let mut out = Vec::new();
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let header = libc::CMSG_LEN(0) as usize;
                let len = ((*cmsg).cmsg_len as usize).saturating_sub(header) / std::mem::size_of::<RawFd>();
                let ptr = libc::CMSG_DATA(cmsg) as *const RawFd;
                for i in 0..len {
                    let fd = std::ptr::read_unaligned(ptr.add(i));
                    libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                    out.push(OwnedFd::from_raw_fd(fd));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }
    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(io::Error::other("fd control message truncated"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::net::UnixStream as StdStream;

    use super::*;
    use crate::codec::{read_frame, write_frame};

    fn inode(p: &Path) -> Option<u64> {
        std::fs::symlink_metadata(p).ok().map(|m| m.ino())
    }

    fn pending_bytes(fd: RawFd) -> libc::c_int {
        let mut n: libc::c_int = 0;
        unsafe { libc::ioctl(fd, libc::FIONREAD, &mut n) };
        n
    }

    /// Plays the old host up to (and including) reading the successor's
    /// ack. The one pane's "PTY master" is `master`.
    async fn old_host_until_ack(listener: &tokio::net::UnixListener, master: &StdStream) -> StdUnixStream {
        let (mut conn, _) = listener.accept().await.unwrap();
        let hello = read_frame(&mut conn).await.unwrap().unwrap().parse_header::<ClientFrame>().unwrap();
        let reply = Reply::Hello { version: PROTOCOL_VERSION, host_pid: 1, epoch_ms: 42 };
        write_frame(&mut conn, &HostFrame::Reply { id: hello.id, result: ReplyResult::Ok(reply) }, &[]).await.unwrap();
        let req = read_frame(&mut conn).await.unwrap().unwrap().parse_header::<ClientFrame>().unwrap();
        assert_eq!(req.request, Request::Handoff);
        write_frame(&mut conn, &HostFrame::Reply { id: req.id, result: ReplyResult::Ok(Reply::Ok) }, &[]).await.unwrap();

        let mut s = conn.into_std().unwrap();
        s.set_nonblocking(false).unwrap();
        let lock = tempfile::tempfile().unwrap();
        let fd = master.as_raw_fd();
        tokio::task::spawn_blocking(move || {
            let manifest = serde_json::json!({"version": HANDOFF_VERSION, "epoch_ms": 42, "panes": [{
                "meta": {"id": "p", "pid": 0, "created_ms": 0, "cwd": "/"},
                "adopted": true, "alive": false, "exit_code": null, "seq": 0,
                "last_output_ms": 0, "cols": 80, "rows": 24,
            }]});
            write_frame_blocking(&mut s, &manifest, &[]).unwrap();
            write_frame_blocking(&mut s, &ReplayHeader { pane: "p".into() }, b"replayed").unwrap();
            send_fds(&s, &[lock.as_raw_fd(), fd]).unwrap();
            let ack: Ack = read_frame_blocking(&mut s).unwrap().unwrap().parse_header().unwrap();
            assert!(ack.owned);
            s
        })
        .await
        .unwrap()
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        socket: PathBuf,
        listener: tokio::net::UnixListener,
        master: StdStream,
        agent: StdStream,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::Builder::new().prefix("ptyd").tempdir_in("/tmp").unwrap();
        let socket = dir.path().join("s.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let (master, mut agent) = StdStream::pair().unwrap();
        // Output the agent produced that nobody has read yet.
        std::io::Write::write_all(&mut agent, b"early").unwrap();
        Fixture { _dir: dir, socket, listener, master, agent }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn successor_touches_nothing_until_released() {
        let f = fixture();
        let ours = inode(&f.socket);
        let takeover = tokio::spawn(run_takeover(f.socket.clone(), None));
        let stream = old_host_until_ack(&f.listener, &f.master).await;

        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(inode(&f.socket), ours, "socket renamed before the release");
        assert_eq!(pending_bytes(f.master.as_raw_fd()), 5, "pane output read before the release");

        // The old host dies (or declines) instead of releasing.
        drop(stream);
        let result = tokio::time::timeout(Duration::from_secs(10), takeover).await.unwrap().unwrap();
        assert!(result.is_err());
        assert_eq!(inode(&f.socket), ours);
        assert!(!next_path(&f.socket).exists());
        assert_eq!(pending_bytes(f.master.as_raw_fd()), 5);
        drop(f.agent);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn successor_starts_and_renames_after_release() {
        let f = fixture();
        let ours = inode(&f.socket);
        let takeover = tokio::spawn(run_takeover(f.socket.clone(), None));
        let mut stream = old_host_until_ack(&f.listener, &f.master).await;
        let stream = tokio::task::spawn_blocking(move || {
            write_frame_blocking(&mut stream, &Released { released: true }, &[]).unwrap();
            let renamed: Renamed = read_frame_blocking(&mut stream).unwrap().unwrap().parse_header().unwrap();
            assert!(renamed.renamed);
            stream
        })
        .await
        .unwrap();
        assert_ne!(inode(&f.socket), ours, "successor's socket is in place");
        let mut c = crate::client::PtydClient::connect(&f.socket, "t").await.unwrap();
        assert_eq!(c.host_identity().1, 42, "epoch carried over");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while pending_bytes(f.master.as_raw_fd()) != 0 {
            assert!(std::time::Instant::now() < deadline, "reader never started");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(c.screen("p", 0).await.unwrap().to_plain_text().contains("replayed"));
        c.shutdown().await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), takeover).await.unwrap().unwrap().unwrap();
        drop((stream, f.agent));
    }
}
