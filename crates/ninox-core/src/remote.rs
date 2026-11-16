//! SSH transport for reaching a remote machine's `ninox-server`/`ninox-ptyd`
//! — see `docs/superpowers/specs/2026-10-05-remote-sessions-design.md` §1-2.
//!
//! Ninox implements no authentication of its own here: every function below
//! shells out to the user's own `ssh`/`scp`, so whatever `~/.ssh/config`,
//! agent, keys, and `known_hosts` the user already has is what authenticates
//! the connection. One SSH `ControlMaster` connection is held open per
//! machine profile (`ControlPersist`), so an interactive prompt (password,
//! passphrase, host-key confirmation) only ever happens once per profile,
//! and every subsequent command reuses that already-authenticated channel.

use anyhow::{anyhow, bail, ensure, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Directory holding per-profile SSH `ControlMaster` sockets. Deliberately
/// `/tmp` rather than `std::env::temp_dir()`: a `sockaddr_un` path is capped
/// around 104 bytes (macOS) and `TMPDIR` is a long per-session path there
/// (`/var/folders/.../T/`) that, combined with a profile id and OpenSSH's
/// own atomic-replace suffix on the socket file, overflows that limit.
fn control_dir() -> PathBuf {
    PathBuf::from("/tmp/ninox-ssh-control")
}

/// Deterministic hash of a `MachineProfile::id`, shared by every per-profile
/// derivation below ([`control_socket_path`], [`local_forward_port`]) so
/// there is exactly one place that turns an id into a number, not one copy
/// per derivation quietly drifting from the others.
fn hash_profile_id(profile_id: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    profile_id.hash(&mut hasher);
    hasher.finish()
}

/// The `ControlPath` for a given machine profile id. Deterministic per id,
/// so repeated commands against the same profile reuse the same socket.
/// Hashed down to a short hex token (rather than using the id — typically a
/// full UUID — verbatim) to leave headroom under the socket path length
/// limit alongside `control_dir()` and OpenSSH's own suffix.
pub fn control_socket_path(profile_id: &str) -> PathBuf {
    control_dir().join(format!("{:016x}.sock", hash_profile_id(profile_id)))
}

/// Single-quote `s` for safe inclusion in a remote shell command line,
/// escaping any embedded single quotes (`'` -> `'\''`).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// `ssh -S <control_path> <op_args...> -- <target>` — the skeleton every
/// `-O <verb>` control-connection command in this module shares.
fn ssh_control_command(control_path: &Path, op_args: &[&str], target: &str) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.arg("-S").arg(control_path);
    cmd.args(op_args);
    cmd.arg("--").arg(target);
    cmd
}

/// Whether a `ControlMaster` connection for `target` is already alive at
/// `control_path` (`ssh -O check`).
pub fn control_master_alive(target: &str, control_path: &Path) -> bool {
    ssh_control_command(control_path, &["-O", "check"], target)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Ensure a persistent `ControlMaster` connection exists for `target`,
/// establishing one if not already up. Inherits the caller's stdio so a
/// first-time password/passphrase/host-key prompt surfaces normally — this
/// is meant to be called from an interactive command (`machine add`), never
/// from a background probe. Idempotent: safe to call before every remote
/// operation for this profile.
pub fn ensure_control_master(target: &str, control_path: &Path) -> Result<()> {
    if control_master_alive(target, control_path) {
        return Ok(());
    }
    if let Some(parent) = control_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let status = Command::new("ssh")
        .args(["-f", "-N", "-M"])
        .arg("-o")
        .arg("ControlPersist=600")
        .arg("-o")
        .arg("ConnectTimeout=15")
        .arg("-S")
        .arg(control_path)
        .arg("--")
        .arg(target)
        .status()
        .context("failed to spawn ssh")?;
    ensure!(status.success(), "ssh connection to {target} failed");
    Ok(())
}

/// Tear down a profile's `ControlMaster` connection, if any. Best-effort —
/// errors are swallowed since this is cleanup, not a load-bearing step.
/// Also takes down any port forward added to it (`-O exit` closes the whole
/// multiplexed connection), so callers don't need to pair this with
/// [`cancel_port_forward`] unless they want the forward gone while keeping
/// the control connection itself alive.
pub fn close_control_master(target: &str, control_path: &Path) {
    let _ = ssh_control_command(control_path, &["-O", "exit"], target)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = std::fs::remove_file(forward_marker_path(control_path));
}

/// Like [`ensure_control_master`], but safe to call unattended from a
/// background health probe (design spec §6): `BatchMode=yes` makes any
/// would-be interactive prompt (password, passphrase, host-key confirmation)
/// fail immediately instead of hanging with no terminal to answer it, and
/// stdio is swallowed rather than inherited. This is reconnection only — it
/// never installs or replaces anything on the remote host (that's `machine
/// add`/`machine update`'s job, gated on explicit interactive approval);
/// this function doesn't even look at the remote binary.
pub fn reconnect_control_master(target: &str, control_path: &Path) -> Result<()> {
    if control_master_alive(target, control_path) {
        return Ok(());
    }
    if let Some(parent) = control_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let status = Command::new("ssh")
        .args(["-f", "-N", "-M"])
        .arg("-o")
        .arg("ControlPersist=600")
        .arg("-o")
        .arg("ConnectTimeout=15")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-S")
        .arg(control_path)
        .arg("--")
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("failed to spawn ssh")?;
    ensure!(
        status.success(),
        "background reconnect to {target} failed (batch mode — no interactive auth available)"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Port forwarding (tunnel)
// ---------------------------------------------------------------------------
//
// Spec §2's HTTP/WS forward, layered on top of the `ControlMaster` above
// rather than a connection of its own — `ssh -O forward`/`-O cancel` add and
// remove a local TCP forward on an already-open control connection.
//
// Lifecycle: lazy and per-`MachineProfile` — a forward is established the
// first time a caller actually needs to reach the remote host (today:
// `RemoteBackend::list_sessions`), not eagerly when the profile is added or
// a session is created. It is reused for as long as the control master
// itself lives (the same `ControlPersist=600` window `ensure_control_master`
// already sets — no separate timeout of its own). This function never
// establishes the control master itself — see its doc comment for why
// creating one here (outside the interactive `machine add`/reconnect flow)
// would risk a silent hang on a host-key or passphrase prompt with no
// terminal to answer it; a dead control master surfaces here as a clear
// error instead. Reconnection/backoff for that case is explicit follow-on
// work, not attempted here.
//
// "Already up" can't be decided from a bare local-port probe alone: OpenSSH
// has no per-forward equivalent of `-O check` (only the control connection
// itself), and a port probe on its own can't tell "our forward" apart from
// an unrelated process that happens to be listening on the same local port
// right now (ports are a shared, guessable namespace — [`local_forward_port`]
// is a deterministic hash, not a reservation) or, in the small window between
// two concurrent callers for the same profile, from a forward another caller
// is establishing concurrently. A marker file next to the control socket
// (`forward_marker_path`), written only right after *this module* actually
// establishes a forward and naming the remote port it forwards to, closes
// both gaps: a live probe plus a matching marker means something this module
// set up is actually listening; a probe with no matching marker is treated
// as "not ours" and a real `-O forward` attempt is made, which then fails
// loudly (port already bound, by that unrelated process) rather than
// silently reusing someone else's listener.

/// Whether a local TCP listener already answers on `local_port`.
fn forward_active(local_port: u16) -> bool {
    std::net::TcpStream::connect(("127.0.0.1", local_port)).is_ok()
}

/// Where this module records which remote port it last forwarded `target`'s
/// control connection to — see the module docs above.
fn forward_marker_path(control_path: &Path) -> PathBuf {
    control_path.with_extension("forward")
}

/// Whether `marker` confirms a forward *this module* established is the one
/// now answering, i.e. it names exactly `remote_port`.
fn forward_marker_matches(marker: &Path, remote_port: u16) -> bool {
    std::fs::read_to_string(marker).ok().and_then(|s| s.trim().parse::<u16>().ok()) == Some(remote_port)
}

/// Whether `local_port` is already forwarding to `remote_port`, per both the
/// live probe and the ownership marker — see the module docs.
fn forward_already_up(control_path: &Path, local_port: u16, remote_port: u16) -> bool {
    forward_active(local_port) && forward_marker_matches(&forward_marker_path(control_path), remote_port)
}

/// The `-L` argument for forwarding `local_port` to `remote_port` on the
/// remote host's own loopback interface (where `ninox-server` binds).
fn forward_spec(local_port: u16, remote_port: u16) -> String {
    format!("{local_port}:127.0.0.1:{remote_port}")
}

/// A deterministic local port for `profile_id`'s forward, in the IANA
/// dynamic/private range (49152-65535) — so repeat calls for the same
/// profile reuse the same local port without persisting anything, the same
/// hash-per-id approach [`control_socket_path`] already uses for the
/// control socket path. A hash collision between two profiles is handled,
/// not just accepted — see the module docs on the ownership marker.
pub fn local_forward_port(profile_id: &str) -> u16 {
    const RANGE_START: u32 = 49152;
    const RANGE_LEN: u64 = 65535 - 49152 + 1;
    (RANGE_START + (hash_profile_id(profile_id) % RANGE_LEN) as u32) as u16
}

/// Ensure a local TCP forward (`<local_port> -> 127.0.0.1:<remote_port>` on
/// `target`) exists over `target`'s already-open `ControlMaster`, so repeat
/// callers reach the remote machine's HTTP/WS API as an ordinary local
/// connection instead of spawning a fresh SSH client per call. Idempotent —
/// see the module docs for how "already up" is decided, including against a
/// concurrent caller racing to establish the same forward.
pub fn ensure_port_forward(target: &str, control_path: &Path, local_port: u16, remote_port: u16) -> Result<()> {
    if forward_already_up(control_path, local_port, remote_port) {
        return Ok(());
    }
    ensure!(
        control_master_alive(target, control_path),
        "no active SSH connection to {target} — run `ninox machine add` (or reconnect) before \
         a remote session can be reached"
    );
    let spec = forward_spec(local_port, remote_port);
    let status = ssh_control_command(control_path, &["-O", "forward", "-L", &spec], target)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("failed to spawn ssh -O forward")?;
    if !status.success() {
        // A concurrent caller for this same profile may have won the race to
        // add this exact forward first — if it's now up and the marker
        // confirms it's this module's own doing (not an unrelated listener
        // squatting the port), that's success, not a failure to report.
        ensure!(
            forward_already_up(control_path, local_port, remote_port),
            "failed to establish port forward {spec} to {target}"
        );
        return Ok(());
    }
    let _ = std::fs::write(forward_marker_path(control_path), remote_port.to_string());
    Ok(())
}

/// Tear down a forward previously established by [`ensure_port_forward`]
/// (`-O cancel`). Best-effort — errors are swallowed, same as
/// [`close_control_master`], which this is normally paired with.
pub fn cancel_port_forward(target: &str, control_path: &Path, local_port: u16, remote_port: u16) {
    let spec = forward_spec(local_port, remote_port);
    let _ = ssh_control_command(control_path, &["-O", "cancel", "-L", &spec], target)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = std::fs::remove_file(forward_marker_path(control_path));
}

/// Run `shell_cmd` on `target` as a *login* shell (`sh -lc`), so remote PATH
/// customizations (e.g. `~/.local/bin`) apply — sshd's own non-interactive
/// shell invocation does not source `.profile`/`.bash_profile` otherwise.
/// Reuses `control_path`'s connection (`BatchMode=yes` — no further auth
/// should be needed once the control master is up).
pub fn run_remote_shell(target: &str, control_path: &Path, shell_cmd: &str) -> Result<Output> {
    let remote_cmd = format!("sh -lc {}", shell_quote(shell_cmd));
    Command::new("ssh")
        .arg("-S")
        .arg(control_path)
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=15")
        .arg("--")
        .arg(target)
        .arg(remote_cmd)
        .stdin(Stdio::null())
        .output()
        .context("failed to run ssh")
}

/// `run_remote_shell` for a `ninox <args>` invocation specifically, with
/// `~/.local/bin` (the `machine add` install target — see
/// [`REMOTE_INSTALL_DIR`]) prepended to PATH so a freshly installed binary
/// is found even if the user hasn't added it themselves yet.
pub fn run_remote_ninox(target: &str, control_path: &Path, ninox_args: &str) -> Result<Output> {
    run_remote_shell(
        target,
        control_path,
        &format!("export PATH=\"$HOME/{REMOTE_INSTALL_DIR}:$PATH\"; ninox {ninox_args}"),
    )
}

/// Where `machine add` installs a missing/outdated remote binary, relative
/// to the remote `$HOME`. Same directory convention as a user's own
/// `~/.local/bin` PATH entry on most Linux/macOS setups.
pub const REMOTE_INSTALL_DIR: &str = ".local/bin";

/// A short, unique temp-file name under `REMOTE_INSTALL_DIR` to stage a
/// freshly transferred binary before atomically installing it — see
/// [`atomic_install_script`] for why a direct overwrite of the real path is
/// unsafe.
fn install_tmp_name() -> String {
    format!(".ninox.update-{}", std::process::id())
}

/// The remote shell script that moves an already-staged file at `tmp_name`
/// (under `REMOTE_INSTALL_DIR`) atomically into place as the real `ninox`
/// binary, marking it executable first. Never writes directly to
/// `REMOTE_INSTALL_DIR/ninox` — that path is exactly what `ninox service
/// install` points the remote engine and ptyd at, so for `machine update`
/// it is normally the currently-executing binary: overwriting it in place
/// can fail with `ETXTBSY` on Linux or crash a running process on macOS.
/// `mv` within one filesystem is atomic and a running process keeps its old
/// inode, the same reasoning as the local self-update's `replace_binary`
/// (`lifecycle::binary_update`).
fn atomic_install_script(tmp_name: &str) -> String {
    format!(
        "chmod +x \"$HOME/{REMOTE_INSTALL_DIR}/{tmp_name}\" && \
         mv -f \"$HOME/{REMOTE_INSTALL_DIR}/{tmp_name}\" \"$HOME/{REMOTE_INSTALL_DIR}/ninox\""
    )
}

/// [`atomic_install_script`], preceded by copying `cp_source` (an already
/// shell-quoted remote path) into `tmp_name` — the `cargo install`
/// fallback's equivalent of [`install_local_binary`]'s scp-then-rename,
/// sharing the same never-overwrite-the-running-binary-directly approach.
fn stage_and_install_script(cp_source: &str, tmp_name: &str) -> String {
    format!(
        "mkdir -p \"$HOME/{REMOTE_INSTALL_DIR}\" && cp {cp_source} \"$HOME/{REMOTE_INSTALL_DIR}/{tmp_name}\" && {}",
        atomic_install_script(tmp_name)
    )
}

/// Copy the local `ninox` binary to `~/.local/bin/ninox` on `target`,
/// creating the directory and marking it executable. Only correct when the
/// remote machine's OS/architecture matches the local one — see
/// [`install_remote_binary`] for the cross-architecture cases (a matching
/// prebuilt, or a `cargo install` fallback), which this function is the
/// final step of either way (a fetched prebuilt is copied over with this
/// same function, just from a downloaded path instead of `current_exe`).
pub fn install_local_binary(target: &str, control_path: &Path, local_bin: &Path) -> Result<()> {
    run_remote_shell(target, control_path, &format!("mkdir -p \"$HOME/{REMOTE_INSTALL_DIR}\""))
        .ok()
        .filter(|o| o.status.success())
        .context("failed to create remote install directory")?;
    let tmp_name = install_tmp_name();
    let mut cmd = Command::new("scp");
    cmd.arg("-o").arg(format!("ControlPath={}", control_path.display()));
    cmd.arg("--");
    cmd.arg(local_bin);
    cmd.arg(format!("{target}:{REMOTE_INSTALL_DIR}/{tmp_name}"));
    let status = cmd.status().context("failed to run scp")?;
    ensure!(status.success(), "scp to {target} failed");
    let out = run_remote_shell(target, control_path, &atomic_install_script(&tmp_name))?;
    if !out.status.success() {
        let _ = run_remote_shell(target, control_path, &format!("rm -f \"$HOME/{REMOTE_INSTALL_DIR}/{tmp_name}\""));
        bail!(
            "failed to install the remote binary on {target}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Parse `ninox --version`'s stdout (clap's default `<bin-name> <version>`
/// format) into just the version token.
fn parse_version_output(stdout: &str) -> Option<String> {
    stdout.split_whitespace().last().map(|s| s.trim_start_matches('v').to_string())
}

/// Probe `target` for a `ninox` binary on its login-shell PATH and report
/// its version. `Ok(None)` means no compatible binary was found (missing,
/// or `ninox --version` exited non-zero) — not an error, since this is an
/// expected, handled outcome of probing.
pub fn probe_remote_version(target: &str, control_path: &Path) -> Result<Option<String>> {
    let out = run_remote_ninox(target, control_path, "--version")?;
    if !out.status.success() {
        return Ok(None);
    }
    Ok(parse_version_output(&String::from_utf8_lossy(&out.stdout)))
}

/// `ninox-ptyd`'s identity as `ninox ptyd status` reports it (see
/// `ninox-ui-shared/src/runtime_cli.rs`'s `PtydAction::Status` handler) —
/// what a live-upgrade handoff keeps (`epoch_ms`) and what it changes
/// (`pid`), used to tell a completed takeover apart from the old host still
/// answering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtydStatus {
    pub pid: u32,
    pub epoch_ms: u64,
}

/// Parses `"ptyd running: pid {pid}, epoch {epoch_ms}, ..."` — `None` for
/// the `"ptyd not running..."` line, or anything else unparseable.
fn parse_ptyd_status_output(stdout: &str) -> Option<PtydStatus> {
    let pid = stdout.split("pid ").nth(1)?.split(',').next()?.trim().parse().ok()?;
    let epoch_ms = stdout.split("epoch ").nth(1)?.split(',').next()?.trim().parse().ok()?;
    Some(PtydStatus { pid, epoch_ms })
}

/// Probe `target`'s `ninox-ptyd` host for its current identity. `Ok(None)`
/// means no host is running (or its output didn't parse) — not an error,
/// mirroring [`probe_remote_version`]'s convention.
pub fn probe_remote_ptyd_status(target: &str, control_path: &Path) -> Result<Option<PtydStatus>> {
    let out = run_remote_ninox(target, control_path, "ptyd status")?;
    if !out.status.success() {
        return Ok(None);
    }
    Ok(parse_ptyd_status_output(&String::from_utf8_lossy(&out.stdout)))
}

/// The shell wrapper that lets `inner_cmd` outlive the ssh session that
/// launched it: `nohup` so it ignores the SIGHUP sshd sends the session's
/// process group on disconnect, stdin from `/dev/null` and stdout/stderr
/// appended to `log_path` (created first, same as `ninox-ptyd`'s own local
/// `spawn_detached` does — see `ninox-ptyd/src/client.rs`) so the ssh pipe
/// can close, and trailing `&` so the `sh -lc` invocation `run_remote_shell`
/// wraps this in returns immediately instead of waiting for `inner_cmd` to
/// exit. `inner_cmd` is wrapped in its own `sh -c` so `nohup` applies to the
/// whole (possibly compound, `;`-joined) command, not just its first word.
/// `log_path` is never attacker-controlled (always one of this module's own
/// fixed path constants, or `/dev/null`), so it's interpolated directly in
/// double quotes rather than through `shell_quote`'s single-quoting, which
/// would stop the remote shell from expanding a `$HOME` inside it.
fn detached_script(inner_cmd: &str, log_path: &str) -> String {
    format!(
        "mkdir -p \"$(dirname \"{log_path}\")\" 2>/dev/null; nohup sh -c {} </dev/null >>\"{log_path}\" 2>&1 &",
        shell_quote(inner_cmd),
    )
}

/// Run `inner_cmd` on `target`, detached, logging to `log_path` — see
/// [`detached_script`]. Needed for `ninox ptyd --takeover`, which (like
/// `ninox ptyd` itself) runs in the foreground indefinitely by design
/// (`ninox-ptyd/src/handoff.rs`); `run_remote_shell`'s blocking `.output()`
/// would otherwise wait for it to exit, which for a live upgrade is never,
/// until it's replaced again.
fn run_remote_detached(target: &str, control_path: &Path, inner_cmd: &str, log_path: &str) -> Result<()> {
    let out = run_remote_shell(target, control_path, &detached_script(inner_cmd, log_path))?;
    ensure!(
        out.status.success(),
        "failed to start a detached remote command on {target}: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}

/// Where a detached `ninox ptyd --takeover` logs on `target` — mirrors
/// `ptyd_backend.rs`'s local `<data_local_dir>/ninox/ptyd.log` convention
/// for the remote's own OS, so a failure before the handoff's commit point
/// (which would otherwise vanish into `/dev/null`) is still diagnosable.
fn remote_ptyd_log_path(remote_os: &str) -> &'static str {
    if remote_os == "macos" {
        "$HOME/Library/Application Support/ninox/ptyd.log"
    } else {
        "$HOME/.local/share/ninox/ptyd.log"
    }
}

/// Trigger the remote `ninox-ptyd` host's live-upgrade handoff — the exact
/// same `ninox ptyd --takeover` a local upgrade runs
/// (`ninox-ui-shared/src/runtime_cli.rs:47-57`), reused here over SSH rather
/// than reimplemented. The local ninox never participates in the SCM_RIGHTS
/// fd handoff itself (spec resolved decision #5) — this only starts it;
/// [`wait_for_ptyd_takeover`] confirms it finished.
pub fn trigger_remote_ptyd_takeover(target: &str, control_path: &Path) -> Result<()> {
    let (remote_os, _arch) = detect_remote_platform(target, control_path)?;
    run_remote_detached(
        target,
        control_path,
        &format!("export PATH=\"$HOME/{REMOTE_INSTALL_DIR}:$PATH\"; ninox ptyd --takeover"),
        remote_ptyd_log_path(&remote_os),
    )
}

/// How a freshly observed [`PtydStatus`] compares to the one observed right
/// before [`trigger_remote_ptyd_takeover`] ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PtydReadiness {
    /// Still the pre-takeover host answering — keep polling.
    NotYet,
    /// A new process (`pid` changed) kept the same `epoch_ms`: the handoff
    /// succeeded and every pane survived.
    LiveUpgraded,
    /// No `previous` status existed at all (nothing was running to
    /// preserve) — any running host counts as success.
    FreshStart,
    /// A new process answered with a *different* `epoch_ms` than before:
    /// `run_host_takeover` fell back to starting fresh instead of handing
    /// off — the old host is gone and its panes were not preserved.
    PanesLost,
}

fn classify_ptyd_status(status: &PtydStatus, previous: &Option<PtydStatus>) -> PtydReadiness {
    match previous {
        None => PtydReadiness::FreshStart,
        Some(prev) if status.pid == prev.pid => PtydReadiness::NotYet,
        Some(prev) if status.epoch_ms == prev.epoch_ms => PtydReadiness::LiveUpgraded,
        Some(_) => PtydReadiness::PanesLost,
    }
}

/// The polling loop behind [`wait_for_ptyd_takeover`], with an injectable
/// probe so the retry/timeout/error-classification logic is unit-testable
/// without a real remote.
fn wait_for_ptyd_ready_with<F: FnMut() -> Result<Option<PtydStatus>>>(
    mut probe: F,
    previous: Option<PtydStatus>,
    timeout: Duration,
    poll_interval: Duration,
) -> Result<PtydStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(Some(status)) = probe() {
            match classify_ptyd_status(&status, &previous) {
                PtydReadiness::LiveUpgraded | PtydReadiness::FreshStart => return Ok(status),
                PtydReadiness::PanesLost => bail!(
                    "ptyd restarted with a new epoch ({} -> {}) — existing panes were not preserved; \
                     expected a live-upgrade handoff, got a fresh start",
                    previous.map(|p| p.epoch_ms).unwrap_or_default(),
                    status.epoch_ms
                ),
                PtydReadiness::NotYet => {}
            }
        }
        if Instant::now() >= deadline {
            bail!(
                "ptyd did not come back up within {timeout:?} after the live-upgrade takeover — if it never \
                 committed, the previous host should still be serving its panes unharmed"
            );
        }
        std::thread::sleep(poll_interval);
    }
}

/// Arbitrary, picked to exceed the handoff's own worst-case internal
/// timeouts summed (`ACK_TIMEOUT` 30s + `RELEASE_TIMEOUT` 30s +
/// `RENAME_TIMEOUT` 5s = 65s — `ninox-ptyd/src/handoff.rs`), with slack for
/// network latency and a busy fleet's replay-building time; the common case
/// is near-instant.
pub const PTYD_TAKEOVER_TIMEOUT: Duration = Duration::from_secs(90);
const PTYD_TAKEOVER_POLL_INTERVAL: Duration = Duration::from_millis(300);

/// Poll `target`'s `ninox-ptyd` status until it reports having taken over
/// from `previous` (or, with `previous: None`, until anything answers at
/// all) — see [`classify_ptyd_status`] for exactly what that means.
pub fn wait_for_ptyd_takeover(
    target: &str,
    control_path: &Path,
    previous: Option<PtydStatus>,
    timeout: Duration,
) -> Result<PtydStatus> {
    wait_for_ptyd_ready_with(|| probe_remote_ptyd_status(target, control_path), previous, timeout, PTYD_TAKEOVER_POLL_INTERVAL)
        .with_context(|| {
            format!(
                "waiting for ptyd on {target} to come back up after the live-upgrade takeover — the new binary \
                 is already installed, so this can be retried by hand with `ssh {target} ninox ptyd --takeover` \
                 (detached) once the underlying issue is fixed; check {target}'s own ptyd log for why the \
                 takeover didn't complete"
            )
        })
}

/// The remote user's numeric uid (`id -u`) — launchd's `gui/<uid>` domain
/// needs it; the local `ninox service install` reads it from `$HOME`'s
/// metadata instead (no local `id -u` shell-out needed there), but a remote
/// uid can only be read by asking the remote shell.
pub fn remote_uid(target: &str, control_path: &Path) -> Result<u32> {
    let out = run_remote_shell(target, control_path, "id -u")?;
    ensure!(out.status.success(), "failed to read the remote uid on {target}");
    String::from_utf8_lossy(&out.stdout).trim().parse().context("unexpected `id -u` output")
}

/// [`crate::fleet::service::ServicePlatform::for_os`], pulled out as its own
/// function so the "unsupported OS" error wording is unit-testable without a
/// real remote.
fn require_service_platform(os: &str) -> Result<crate::fleet::service::ServicePlatform> {
    crate::fleet::service::ServicePlatform::for_os(os)
        .ok_or_else(|| anyhow!("no service-restart support for remote OS {os}"))
}

/// Whether a systemd unit's `KillMode` is safe to restart without risking
/// anything beyond the unit's own main process — only `process` is. The
/// default, `control-group`, sends every signal `systemctl restart` issues
/// to the whole cgroup, which on a remote where this engine spawned
/// `ninox-ptyd` on demand (`ensure_host`/`connect_or_spawn`) would include
/// that ptyd host and every pane it holds: a detached, double-forked,
/// `setsid`'d child still inherits its parent's cgroup, so leaving its
/// process group is not enough to escape this. Killing all of those is
/// exactly the failure this whole mechanism exists to prevent, so a restart
/// is only attempted once this is confirmed, never assumed.
fn kill_mode_is_safe(mode: &str) -> bool {
    mode.trim() == "process"
}

/// `ensure!`s [`kill_mode_is_safe`] for `target`'s already-installed unit —
/// see its doc for why. A unit installed before `KillMode=process` was
/// added to [`crate::fleet::service::systemd_unit`] still defaults to the
/// unsafe `control-group`, so this checks the unit actually running on
/// `target`, not just what the current template would produce.
fn require_safe_kill_mode(target: &str, control_path: &Path) -> Result<()> {
    let out = run_remote_shell(
        target,
        control_path,
        &format!("systemctl --user show -p KillMode --value {}", crate::fleet::service::SYSTEMD_UNIT_NAME),
    )?;
    let mode = String::from_utf8_lossy(&out.stdout).trim().to_string();
    ensure!(
        out.status.success() && kill_mode_is_safe(&mode),
        "the engine service's KillMode on {target} is {mode:?}, not \"process\" — restarting it could kill every \
         live ptyd pane in its cgroup; reinstall the service there (`ninox service install`) to pick up the safer \
         unit, then retry"
    );
    Ok(())
}

/// Whether `launchctl print`'s output for an already-installed job reports
/// `AbandonProcessGroup` as set — the launchd analog of
/// [`kill_mode_is_safe`]: without it, stopping the job (as `launchctl
/// kickstart -k` does to restart one) signals its whole *process group*,
/// not just the job's own tracked process, which could reach a
/// `ninox-ptyd` host (or any pane) sharing it. `launchctl print` lists this
/// as literal text in its `properties` line when set, and omits it
/// entirely when unset — verified against a real launchd job.
fn launchd_abandons_process_group(print_output: &str) -> bool {
    print_output.lines().any(|l| {
        let l = l.trim_start();
        l.starts_with("properties") && l.contains("abandon process group")
    })
}

/// `ensure!`s [`launchd_abandons_process_group`] for `target`'s
/// already-installed job — see its doc for why. A plist installed before
/// `AbandonProcessGroup` was added to [`crate::fleet::service::launchd_plist`]
/// still defaults to the unsafe whole-process-group kill, so this checks
/// the job actually loaded on `target`, not just what the current template
/// would produce.
fn require_abandoned_process_group(target: &str, control_path: &Path) -> Result<()> {
    let out = run_remote_shell(
        target,
        control_path,
        &format!("launchctl print gui/$(id -u)/{}", crate::fleet::service::LAUNCHD_LABEL),
    )?;
    ensure!(
        out.status.success() && launchd_abandons_process_group(&String::from_utf8_lossy(&out.stdout)),
        "the engine service's launchd job on {target} does not set AbandonProcessGroup — restarting it could \
         kill every live ptyd pane in its process group; reinstall the service there (`ninox service install`) \
         to pick up the safer plist, then retry"
    );
    Ok(())
}

/// Restart the headless engine (`ninox-server`) service on `target` so it
/// picks up a newly installed binary. Unlike [`trigger_remote_ptyd_takeover`],
/// this is a plain restart with no handoff of any kind — the engine holds no
/// pane state of its own (`ninox-ptyd/src/lib.rs`'s module doc: "Keeping
/// this crate free of engine logic is what lets the engine be upgraded or
/// crash without touching agent processes") — but see [`require_safe_kill_mode`]
/// / [`require_abandoned_process_group`] for why that's only true of the
/// engine process itself, not blindly of whatever else shares its cgroup
/// (systemd) or process group (launchd).
pub fn restart_remote_engine_service(target: &str, control_path: &Path) -> Result<()> {
    let (remote_os, _arch) = detect_remote_platform(target, control_path)?;
    let platform = require_service_platform(&remote_os)?;
    match platform {
        crate::fleet::service::ServicePlatform::Systemd => require_safe_kill_mode(target, control_path)?,
        crate::fleet::service::ServicePlatform::Launchd => require_abandoned_process_group(target, control_path)?,
    }
    let uid = remote_uid(target, control_path)?;
    for argv in crate::fleet::service::restart_commands(platform, uid) {
        let shell_cmd = argv.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ");
        let out = run_remote_shell(target, control_path, &shell_cmd)?;
        ensure!(
            out.status.success(),
            "`{}` failed on {target}: {}",
            argv.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Whether `machine add` should offer to install/update the remote binary:
/// true when none is present, or its version doesn't match the local
/// (running) binary's own version. ninox's only distribution mechanism here
/// is copying the local binary over (see [`install_local_binary`]), so
/// "compatible" means "identical" — there is no cross-version wire
/// compatibility story to lean on instead.
pub fn needs_binary_install(remote_version: Option<&str>, local_version: &str) -> bool {
    remote_version != Some(local_version)
}

/// This machine's (os, arch), in `std::env::consts` vocabulary — the same
/// one `lifecycle::binary_update::platform_supported` expects.
pub fn local_platform() -> (String, String) {
    (std::env::consts::OS.to_string(), std::env::consts::ARCH.to_string())
}

/// `uname`'s OS token normalized to `std::env::consts::OS` vocabulary.
fn normalize_os(raw: &str) -> String {
    match raw.trim() {
        "Darwin" => "macos".to_string(),
        "Linux" => "linux".to_string(),
        other => other.to_ascii_lowercase(),
    }
}

/// `uname`'s machine-hardware token normalized to `std::env::consts::ARCH`
/// vocabulary (macOS's `uname -m` reports `arm64`, not `aarch64`).
fn normalize_arch(raw: &str) -> String {
    match raw.trim() {
        "arm64" => "aarch64".to_string(),
        "amd64" => "x86_64".to_string(),
        other => other.to_ascii_lowercase(),
    }
}

/// Parses `uname -s -m`'s stdout (e.g. `"Darwin arm64\n"`) into a normalized
/// (os, arch) pair comparable against [`local_platform`].
fn parse_uname_output(stdout: &str) -> Option<(String, String)> {
    let mut parts = stdout.split_whitespace();
    let os = parts.next()?;
    let arch = parts.next()?;
    Some((normalize_os(os), normalize_arch(arch)))
}

/// Detects `target`'s (os, arch) via `uname -s -m` over the already-open
/// control connection.
pub fn detect_remote_platform(target: &str, control_path: &Path) -> Result<(String, String)> {
    let out = run_remote_shell(target, control_path, "uname -s -m")?;
    ensure!(out.status.success(), "uname -s -m failed on {target}: {}", String::from_utf8_lossy(&out.stderr).trim());
    parse_uname_output(&String::from_utf8_lossy(&out.stdout))
        .with_context(|| format!("couldn't parse `uname -s -m` output from {target}"))
}

/// Which mechanism installed a binary onto the remote machine — surfaced to
/// `machine add` purely for its own progress messaging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallMethod {
    /// Remote OS/arch matched local exactly — the local binary was copied
    /// over verbatim (today's only pre-existing mechanism).
    CopyLocalBinary,
    /// A prebuilt binary matching the remote's OS/arch was found in
    /// CodeArtifact and installed.
    FetchPrebuilt,
    /// No prebuilt existed for the remote's OS/arch; installed via
    /// `cargo install` against the internal CodeArtifact registry, run on
    /// the remote host itself (requires a remote Rust toolchain).
    CargoInstallFallback,
}

/// The three ways a binary can land on a remote host, abstracted so
/// [`install_remote_binary`]'s decision logic (fast path vs. prebuilt vs.
/// CodeArtifact fallback) is unit-testable without a real SSH connection or
/// `aws` CLI — mirrors the `UpdateSource`/`UpdateInstaller` split in
/// `lifecycle::update_check`.
pub trait RemoteInstallBackend {
    /// Detect the remote host's (os, arch) — normally [`detect_remote_platform`].
    fn detect_remote_platform(&self) -> Result<(String, String)>;
    /// Copy the local binary over verbatim — normally [`install_local_binary`].
    fn copy_local_binary(&self) -> Result<()>;
    /// Fetch and install whatever prebuilt CodeArtifact has published for
    /// `(remote_os, remote_arch)` at this machine's own version. `Ok(false)`
    /// (not an error) means no matching prebuilt exists — the caller falls
    /// through to the CodeArtifact `cargo install` fallback.
    fn install_prebuilt(&self, remote_os: &str, remote_arch: &str) -> Result<bool>;
    /// `cargo install` against the internal CodeArtifact registry, run over
    /// SSH on the remote host.
    fn cargo_install_fallback(&self, remote_os: &str, remote_arch: &str) -> Result<()>;
}

/// Picks and runs the right install mechanism for `target`, per the design
/// spec's resolution of open question #7: prefer a prebuilt matching the
/// remote's OS/arch; fall back to `cargo install` from CodeArtifact only
/// when no prebuilt exists; keep today's local-binary-copy fast path when
/// the remote's OS/arch matches `local` exactly.
pub fn install_remote_binary(backend: &dyn RemoteInstallBackend, local: (&str, &str)) -> Result<InstallMethod> {
    let (remote_os, remote_arch) = backend.detect_remote_platform()?;
    if (remote_os.as_str(), remote_arch.as_str()) == local {
        backend.copy_local_binary()?;
        return Ok(InstallMethod::CopyLocalBinary);
    }
    if crate::lifecycle::binary_update::platform_supported(&remote_os, &remote_arch)
        && backend.install_prebuilt(&remote_os, &remote_arch)?
    {
        return Ok(InstallMethod::FetchPrebuilt);
    }
    backend.cargo_install_fallback(&remote_os, &remote_arch)?;
    Ok(InstallMethod::CargoInstallFallback)
}

/// The error `cargo_install_on_remote` reports when the CodeArtifact
/// fallback itself can't be used — pulled out as its own function so the
/// exact wording is covered by a test without needing a real SSH session.
fn cargo_fallback_unavailable(remote_os: &str, remote_arch: &str, reason: &str) -> anyhow::Error {
    anyhow::anyhow!("no prebuilt binary for {remote_os}/{remote_arch} and CodeArtifact fallback is not available: {reason}")
}

/// `nx` is a `cargo install`-produced `[[bin]]` in its own right, not a
/// symlink, so its file name doesn't match the crate that actually ships it
/// (`ninox-tui`) — this maps it back, the way `update_cli.rs`'s own
/// `tarball_bin_name` does for `ninox update`. Shared by both the prebuilt
/// fetch and the cargo-install fallback below so the mapping lives in one
/// place in this file.
///
/// Rejects anything other than the two real crate names (`ninox-gui`,
/// `ninox-tui`) or their `nx` alias, rather than passing an unrecognized
/// name straight through: `local_bin` is expected to already be resolved
/// past the `ninox`/`nx` alias symlinks (`hooks::canonical_exe`, not
/// `std::env::current_exe`, which macOS reports as the alias path itself,
/// not its target) — a bare `ninox` reaching here means that resolution was
/// skipped, and letting it through would fail confusingly deep inside
/// `extract_binary` (no such tarball entry) or `cargo install` (no such
/// crate) instead of naming the real cause here.
fn package_name_for(local_bin: &Path) -> Result<&str> {
    let bin_name = local_bin.file_name().and_then(|n| n.to_str()).context("local binary has no file name")?;
    match bin_name {
        "nx" => Ok("ninox-tui"),
        "ninox-gui" | "ninox-tui" => Ok(bin_name),
        other => bail!(
            "{other:?} isn't a recognized ninox binary name (expected ninox-gui, ninox-tui, or nx) — \
             {} looks unresolved; it should point at the real binary, not the `ninox`/`nx` alias symlink",
            local_bin.display()
        ),
    }
}

/// The binary to extract from a prebuilt tarball for `remote_os`. Linux
/// remotes are ptyd/TUI-only (spec resolution #6) and the Linux tarball
/// never ships `ninox-gui` — so a Linux remote always extracts
/// `ninox-tui`, regardless of which binary the *local* host happens to be
/// running as. Any other (today: macOS) remote keeps using the local
/// host's own binary name via [`package_name_for`], matching its desktop
/// vs. terminal choice.
fn remote_bin_name<'a>(remote_os: &str, local_bin: &'a Path) -> Result<&'a str> {
    if remote_os == "linux" {
        Ok("ninox-tui")
    } else {
        package_name_for(local_bin)
    }
}

/// Fetches whatever prebuilt CodeArtifact has published for
/// `(remote_os, remote_arch)` at `local_version` and installs it on `target`
/// via [`install_local_binary`]. `Ok(false)` — not an error — whenever
/// CodeArtifact simply has nothing for this exact version (an absent
/// package version, or an asset missing from it, e.g. a generic-package
/// publish that never finished): the caller treats that as "no prebuilt",
/// not a failure.
///
/// Requires `(remote_os, remote_arch)` to be a platform CodeArtifact
/// actually publishes a prebuilt for (checked, not just asserted — every
/// coordinate this function builds, starting with `tarball_asset`, is
/// looked up from `(remote_os, remote_arch)` via
/// [`bu::target_triple`]/[`bu::generic_package`], so calling this for an
/// unsupported platform would otherwise panic on the `.expect()`s below
/// instead of erroring cleanly).
fn install_prebuilt_binary(
    target: &str,
    control_path: &Path,
    local_bin: &Path,
    local_version: &str,
    remote_os: &str,
    remote_arch: &str,
) -> Result<bool> {
    use crate::lifecycle::binary_update as bu;
    ensure!(
        bu::platform_supported(remote_os, remote_arch),
        "install_prebuilt_binary called for {remote_os}/{remote_arch}, which CodeArtifact never publishes a prebuilt for"
    );

    let triple = bu::target_triple(remote_os, remote_arch).expect("checked by platform_supported above");
    // The local build's own version decides the stream: an alpha build's
    // exact version only exists in the alpha package.
    let channel = if semver::Version::parse(local_version).is_ok_and(|v| !v.pre.is_empty()) {
        crate::config::UpdateChannel::Alpha
    } else {
        crate::config::UpdateChannel::Stable
    };
    let package = bu::generic_package(remote_os, channel).expect("checked by platform_supported above");
    let coords = bu::CodeArtifactCoords::from_env();
    let owner = bu::resolve_owner(&coords)?;
    let asset = bu::tarball_asset(local_version, triple);
    let assets_json = match bu::run_aws(&coords.list_assets_args(&owner, package, local_version)) {
        Ok(json) => json,
        Err(e) if e.to_string().contains("ResourceNotFoundException") => return Ok(false),
        Err(e) => return Err(e),
    };
    let Ok(expected_sha) = bu::asset_sha256(&assets_json, &asset) else {
        return Ok(false);
    };

    let work_dir = std::env::temp_dir().join(format!("ninox-machine-add-prebuilt-{}", std::process::id()));
    std::fs::create_dir_all(&work_dir).context("creating a temp dir for the downloaded prebuilt")?;
    let result = (|| -> Result<()> {
        let tarball = work_dir.join(&asset);
        bu::run_aws(&coords.get_asset_args(&owner, package, local_version, &asset, &tarball))?;
        bu::verify_sha256(&tarball, &expected_sha)?;
        let bin_name = remote_bin_name(remote_os, local_bin)?;
        let extracted = bu::extract_binary(&tarball, local_version, triple, bin_name, &work_dir)?;
        install_local_binary(target, control_path, &extracted)
    })();
    let _ = std::fs::remove_dir_all(&work_dir);
    result.map(|()| true)
}

/// `cargo install` against the internal CodeArtifact registry, run over SSH
/// on `target`, pinned to `local_version` so the remote ends up on exactly
/// the same build as local (same reasoning as [`needs_binary_install`]'s
/// equality check). Requires a Rust toolchain on the remote host — checked
/// explicitly so a missing one is a clear, specific error rather than a
/// `cargo: command not found` buried in `cargo install`'s own failure.
fn cargo_install_on_remote(
    target: &str,
    control_path: &Path,
    local_bin: &Path,
    local_version: &str,
    remote_os: &str,
    remote_arch: &str,
) -> Result<()> {
    let has_cargo = run_remote_shell(target, control_path, "cargo --version").ok().is_some_and(|o| o.status.success());
    if !has_cargo {
        return Err(cargo_fallback_unavailable(
            remote_os,
            remote_arch,
            &format!("no Rust toolchain found on {target} (`cargo --version` failed) — install rustup there and retry"),
        ));
    }

    let package = package_name_for(local_bin)?;
    let registry = crate::lifecycle::binary_update::CodeArtifactCoords::from_env().repository;

    let out = run_remote_shell(
        target,
        control_path,
        &format!("cargo install --registry {registry} --locked --force --version {local_version} {package}"),
    )?;
    if !out.status.success() {
        return Err(cargo_fallback_unavailable(
            remote_os,
            remote_arch,
            &format!(
                "`cargo install --registry {registry} --version {local_version} {package}` failed on {target}: {} \
                 (see README.md's CodeArtifact Cargo registry section for the one-time ~/.cargo/config.toml setup \
                 this needs on a new machine)",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        ));
    }

    let tmp_name = install_tmp_name();
    let link = run_remote_shell(
        target,
        control_path,
        &stage_and_install_script(&format!("\"$HOME/.cargo/bin/{package}\""), &tmp_name),
    )?;
    if !link.status.success() {
        return Err(cargo_fallback_unavailable(
            remote_os,
            remote_arch,
            &format!(
                "cargo install succeeded on {target} but linking it to ~/{REMOTE_INSTALL_DIR}/ninox failed: {}",
                String::from_utf8_lossy(&link.stderr).trim()
            ),
        ));
    }
    Ok(())
}

/// The real [`RemoteInstallBackend`]: shells out to `ssh`/`scp`/`aws` for
/// each step. Production code constructs one of these and hands it to
/// [`install_remote_binary`]; tests use a fake instead.
pub struct SshInstallBackend<'a> {
    pub target: &'a str,
    pub control_path: &'a Path,
    pub local_bin: &'a Path,
    pub local_version: &'a str,
}

impl RemoteInstallBackend for SshInstallBackend<'_> {
    fn detect_remote_platform(&self) -> Result<(String, String)> {
        detect_remote_platform(self.target, self.control_path)
    }

    fn copy_local_binary(&self) -> Result<()> {
        install_local_binary(self.target, self.control_path, self.local_bin)
    }

    fn install_prebuilt(&self, remote_os: &str, remote_arch: &str) -> Result<bool> {
        install_prebuilt_binary(self.target, self.control_path, self.local_bin, self.local_version, remote_os, remote_arch)
    }

    fn cargo_install_fallback(&self, remote_os: &str, remote_arch: &str) -> Result<()> {
        cargo_install_on_remote(self.target, self.control_path, self.local_bin, self.local_version, remote_os, remote_arch)
    }
}

/// The `ssh` argv that attaches to `id`'s pane by recursing into
/// `profile`'s own machine's `ninox pane attach <id>` — resolved decision #1
/// of `docs/superpowers/specs/2026-10-05-remote-sessions-design.md`. `-t`
/// allocates a remote pty, so SIGWINCH/resize and Ctrl-C pass through
/// exactly as any interactive SSH session; detach semantics are whatever
/// the remote machine's own `ninox pane attach` provides, since this is
/// just a transport hop, not a new state machine. `-S <control_path>`
/// opportunistically reuses the profile's `ControlMaster` when one is still
/// alive within its `ControlPersist` window (resolved decision #9); when
/// it's expired or was never established, `ssh` just falls back to an
/// ordinary, directly-authenticated connection instead of failing — fine
/// here since this `exec()`s straight into the user's own interactive
/// terminal, unlike a background caller with no terminal to answer a
/// prompt. The remote command runs through a login shell with
/// `REMOTE_INSTALL_DIR` on `PATH`, matching [`run_remote_ninox`]'s
/// convention, so a `machine add`-installed binary is found — built as a
/// single `sh -lc '<cmd>'` argument (like [`run_remote_shell`]), not several
/// separate ones: `ssh` joins multiple trailing arguments with spaces
/// before sending them to the remote shell, which would otherwise hand
/// `-lc` only its first word (`export`) as the command and silently drop
/// everything after it, including the `PATH` assignment. The remote
/// machine's own store has `machine_id: None` for this same session id — it
/// is local there — so the recursion terminates after one hop.
pub fn attach_exec_argv(profile: &crate::config::MachineProfile, id: &str) -> Vec<String> {
    attach_exec_argv_with(profile, id, &[])
}

/// [`attach_exec_argv`] with extra `ninox pane attach` flags, placed inside
/// the quoted remote command (appending them to the finished argv element
/// would land after the closing quote, where `sh` takes them as `$0`).
pub fn attach_exec_argv_with(profile: &crate::config::MachineProfile, id: &str, flags: &[&str]) -> Vec<String> {
    let control_path = control_socket_path(&profile.id);
    let flags: String = flags.iter().map(|f| format!(" {f}")).collect();
    let remote_cmd = format!(
        "export PATH=\"$HOME/{REMOTE_INSTALL_DIR}:$PATH\"; ninox pane attach{flags} {}",
        shell_quote(id)
    );
    vec![
        "ssh".to_string(),
        "-t".to_string(),
        "-S".to_string(),
        control_path.display().to_string(),
        "--".to_string(),
        profile.ssh_target.clone(),
        format!("sh -lc {}", shell_quote(&remote_cmd)),
    ]
}

/// What a capability probe learned about a remote `ninox`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    Supported,
    Unsupported,
    /// ssh itself failed (no connection, auth); says nothing about the
    /// remote binary, so it is never cached.
    Inconclusive,
}

const FOLLOW_SIZE_YES_TTL: Duration = Duration::from_secs(3600);
const FOLLOW_SIZE_NO_TTL: Duration = Duration::from_secs(300);

fn follow_size_cache_path(control_path: &Path) -> PathBuf {
    control_path.with_extension("follow-size")
}

/// Whether the remote `ninox pane attach` accepts `--follow-size`, by
/// running `pane attach --follow-size --help` there: clap exits 0 when it
/// knows the flag and 2 on an unknown one, independent of version numbers.
/// The answer is cached beside the profile's control socket so an attach
/// costs no extra ssh round trip inside the TTL; anything short of a
/// definite yes is `false`, so an old remote attaches exactly as before.
pub fn remote_supports_follow_size(target: &str, control_path: &Path) -> bool {
    follow_size_support(&follow_size_cache_path(control_path), std::time::SystemTime::now(), || {
        match run_remote_ninox(target, control_path, "pane attach --follow-size --help") {
            Ok(out) if out.status.success() => ProbeOutcome::Supported,
            Ok(out) if out.status.code() == Some(255) => ProbeOutcome::Inconclusive,
            Ok(_) => ProbeOutcome::Unsupported,
            Err(_) => ProbeOutcome::Inconclusive,
        }
    })
}

fn follow_size_support(cache: &Path, now: std::time::SystemTime, probe: impl FnOnce() -> ProbeOutcome) -> bool {
    if let Ok(meta) = std::fs::metadata(cache) {
        let age = meta.modified().ok().and_then(|m| now.duration_since(m).ok()).unwrap_or(Duration::ZERO);
        match std::fs::read_to_string(cache).as_deref().map(str::trim) {
            Ok("1") if age < FOLLOW_SIZE_YES_TTL => return true,
            Ok("0") if age < FOLLOW_SIZE_NO_TTL => return false,
            _ => {}
        }
    }
    let outcome = probe();
    if outcome != ProbeOutcome::Inconclusive {
        let _ = std::fs::write(cache, if outcome == ProbeOutcome::Supported { "1" } else { "0" });
    }
    outcome == ProbeOutcome::Supported
}

/// A session discovered on a remote machine via `ninox list --json`. Only
/// the fields `machine add` needs to offer a choice — not a full `Session`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteOrchestrator {
    pub id: String,
    pub name: String,
}

/// Parse `ninox list --json`'s `{"orchestrators": [...], "sessions": [...]}`
/// shape, extracting just `id`/`name` per orchestrator. Entries missing
/// either field are skipped rather than failing the whole parse — a
/// partially-upgraded remote `ninox` is still worth discovering what it can
/// report.
fn parse_orchestrators_json(json: &[u8]) -> Result<Vec<RemoteOrchestrator>> {
    let value: serde_json::Value = serde_json::from_slice(json)?;
    let orchestrators = value.get("orchestrators").and_then(|o| o.as_array()).cloned().unwrap_or_default();
    Ok(orchestrators
        .iter()
        .filter_map(|o| {
            Some(RemoteOrchestrator {
                id: o.get("id")?.as_str()?.to_string(),
                name: o.get("name")?.as_str()?.to_string(),
            })
        })
        .collect())
}

/// Run `ninox list --json` on `target` and parse its orchestrators. An
/// `Err` here (non-zero exit, unparseable output) means the remote
/// `ninox-server` isn't reachable yet — callers treat that as "needs
/// `ninox service install`", not a hard failure.
pub fn list_remote_orchestrators(target: &str, control_path: &Path) -> Result<Vec<RemoteOrchestrator>> {
    let out = run_remote_ninox(target, control_path, "list --json")?;
    if !out.status.success() {
        bail!(
            "ninox list --json failed on {target}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    parse_orchestrators_json(&out.stdout)
}

/// `workspace`'s unified diff against its default branch, computed on
/// `target` over its SSH connection instead of a local `git diff` — see
/// `routes::sessions::session_diff` in `ninox-server` and resolved decision
/// #4 of `docs/superpowers/specs/2026-10-05-remote-sessions-design.md`.
/// Detects the default branch (mirroring
/// `lifecycle::brain_harvest::detect_default_branch`'s origin/HEAD → local
/// `main` → local `master` fallback) and runs the diff in a single remote
/// shell invocation, so this costs one SSH round trip, not several.
///
/// `Ok(None)` means no diff. `Err` is an explicit, specific failure —
/// `workspace` missing on the remote, git not installed there, or the SSH
/// connection itself failing — never silently collapsed into "no changes":
/// that silent collapse is exactly what this decision was resolved to avoid.
pub fn remote_git_diff(target: &str, control_path: &Path, workspace: &str) -> Result<Option<String>> {
    let script = format!(
        "cd {ws} && branch=$(git symbolic-ref --short -q refs/remotes/origin/HEAD \
         || git rev-parse --verify -q refs/heads/main 2>/dev/null \
         || git rev-parse --verify -q refs/heads/master 2>/dev/null) \
         && git --no-ext-diff --no-color diff \"$branch...HEAD\"",
        ws = shell_quote(workspace),
    );
    let out = run_remote_shell(target, control_path, &script)
        .with_context(|| format!("ssh to {target} failed"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        bail!("remote git diff failed on {target}: {stderr}");
    }
    let diff = String::from_utf8_lossy(&out.stdout).to_string();
    if diff.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(diff))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MachineProfile;
    use std::cell::RefCell;

    #[test]
    fn shell_quote_escapes_embedded_single_quotes() {
        assert_eq!(shell_quote("it's fine"), "'it'\\''s fine'");
        assert_eq!(shell_quote("plain"), "'plain'");
    }

    /// Regression for a real finding: a target string starting with `-`
    /// must never be parsed as an ssh option. `control_master_alive` builds
    /// its `ssh -S <path> -O check -- <target>` command with a `--`
    /// separator, so even a hostile-looking target is just a (nonexistent,
    /// safely-rejected) hostname — this must return `false`, not hang or
    /// otherwise behave as though an option were accepted.
    #[test]
    fn control_master_alive_treats_a_dash_prefixed_target_as_a_hostname_not_an_option() {
        let dir = tempfile::tempdir().unwrap();
        let control_path = dir.path().join("nonexistent.sock");
        assert!(!control_master_alive("-oProxyCommand=true", &control_path));
    }

    /// Same hostile-target safety property as `control_master_alive`, for the
    /// background reconnect path specifically — a dash-prefixed target must
    /// never be parsed as an ssh option here either.
    #[test]
    fn reconnect_control_master_treats_a_dash_prefixed_target_as_a_hostname_not_an_option() {
        let dir = tempfile::tempdir().unwrap();
        let control_path = dir.path().join("nonexistent.sock");
        assert!(reconnect_control_master("-oProxyCommand=true", &control_path).is_err());
    }

    /// An unreachable host must fail cleanly (DNS resolution failure here,
    /// so this returns near-instantly) rather than hang. Note this doesn't
    /// by itself prove `BatchMode=yes` is doing anything — a DNS failure
    /// errors out before ssh ever reaches an auth phase, so this would pass
    /// identically without it. `BatchMode=yes`'s actual job (never blocking
    /// on a real password/passphrase/host-key prompt) is verified by code
    /// inspection, not exercised end-to-end by any test here — doing so
    /// would need a host that genuinely offers an interactive prompt, which
    /// isn't available in this environment or CI.
    #[test]
    fn reconnect_control_master_fails_cleanly_for_an_unreachable_host() {
        let dir = tempfile::tempdir().unwrap();
        let control_path = dir.path().join("nonexistent.sock");
        let err = reconnect_control_master("nonexistent.invalid", &control_path)
            .expect_err("an unreachable host must error, never hang or fabricate success");
        assert!(err.to_string().contains("background reconnect"), "unexpected error message: {err}");
    }

    #[test]
    fn control_socket_path_is_deterministic_per_profile() {
        let a = control_socket_path("abc");
        let b = control_socket_path("abc");
        let c = control_socket_path("xyz");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn parse_version_output_takes_the_last_whitespace_token() {
        assert_eq!(parse_version_output("ninox 0.32.0\n"), Some("0.32.0".to_string()));
        assert_eq!(parse_version_output("ninox v0.32.0"), Some("0.32.0".to_string()));
        assert_eq!(parse_version_output(""), None);
    }

    #[test]
    fn parse_ptyd_status_output_extracts_pid_and_epoch() {
        let out = "ptyd running: pid 4821, epoch 1732000000123, 2 live / 3 panes, socket /tmp/ptyd.sock\n";
        assert_eq!(parse_ptyd_status_output(out), Some(PtydStatus { pid: 4821, epoch_ms: 1732000000123 }));
    }

    #[test]
    fn parse_ptyd_status_output_none_when_not_running() {
        assert_eq!(parse_ptyd_status_output("ptyd not running (socket /tmp/ptyd.sock)\n"), None);
        assert_eq!(parse_ptyd_status_output(""), None);
    }

    #[test]
    fn detached_script_backgrounds_a_nohup_subshell_and_logs_to_the_given_path() {
        let script = detached_script("echo hi", "/tmp/ptyd.log");
        assert!(script.contains("mkdir -p \"$(dirname \"/tmp/ptyd.log\")\""), "{script}");
        assert!(script.contains("nohup sh -c "), "{script}");
        assert!(script.contains(&shell_quote("echo hi")), "{script}");
        assert!(script.contains(">>\"/tmp/ptyd.log\" 2>&1 &"), "{script}");
        assert!(script.ends_with(" &"), "{script}");
    }

    #[test]
    fn remote_ptyd_log_path_matches_each_os_data_dir_convention() {
        assert_eq!(remote_ptyd_log_path("macos"), "$HOME/Library/Application Support/ninox/ptyd.log");
        assert_eq!(remote_ptyd_log_path("linux"), "$HOME/.local/share/ninox/ptyd.log");
    }

    /// The one genuinely risky property of this mechanism: the ssh call must
    /// return long before a slow remote command finishes.
    #[test]
    fn run_remote_detached_returns_before_the_command_finishes() {
        if !local_sshd_reachable() {
            eprintln!("skipping: no reachable localhost sshd");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let control_path = dir.path().join("c.sock");
        ensure_control_master("localhost", &control_path).unwrap();
        let marker = dir.path().join("done");
        let start = Instant::now();
        run_remote_detached(
            "localhost",
            &control_path,
            &format!("sleep 1 && touch {}", shell_quote(marker.to_str().unwrap())),
            "/dev/null",
        )
        .unwrap();
        assert!(start.elapsed() < Duration::from_millis(500), "must return well before the 1s sleep finishes");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !marker.exists() {
            assert!(Instant::now() < deadline, "detached command never ran");
            std::thread::sleep(Duration::from_millis(50));
        }
        close_control_master("localhost", &control_path);
    }

    fn status(pid: u32, epoch_ms: u64) -> PtydStatus {
        PtydStatus { pid, epoch_ms }
    }

    #[test]
    fn classify_ptyd_status_covers_every_case() {
        assert_eq!(classify_ptyd_status(&status(2, 100), &None), PtydReadiness::FreshStart);
        assert_eq!(classify_ptyd_status(&status(1, 100), &Some(status(1, 100))), PtydReadiness::NotYet);
        assert_eq!(classify_ptyd_status(&status(2, 100), &Some(status(1, 100))), PtydReadiness::LiveUpgraded);
        assert_eq!(classify_ptyd_status(&status(2, 999), &Some(status(1, 100))), PtydReadiness::PanesLost);
    }

    #[test]
    fn wait_for_ptyd_ready_with_retries_until_the_pid_changes() {
        let calls =
            RefCell::new(vec![Ok(Some(status(1, 100))), Ok(Some(status(1, 100))), Ok(Some(status(2, 100)))].into_iter());
        let got = wait_for_ptyd_ready_with(
            || calls.borrow_mut().next().unwrap(),
            Some(status(1, 100)),
            Duration::from_secs(1),
            Duration::from_millis(1),
        )
        .unwrap();
        assert_eq!(got, status(2, 100));
    }

    #[test]
    fn wait_for_ptyd_ready_with_errors_clearly_on_panes_lost() {
        let err = wait_for_ptyd_ready_with(
            || Ok(Some(status(2, 999))),
            Some(status(1, 100)),
            Duration::from_secs(1),
            Duration::from_millis(1),
        )
        .unwrap_err();
        assert!(err.to_string().contains("100") && err.to_string().contains("999"), "{err}");
    }

    #[test]
    fn wait_for_ptyd_ready_with_times_out_when_never_ready() {
        let err = wait_for_ptyd_ready_with(
            || Ok(Some(status(1, 100))),
            Some(status(1, 100)),
            Duration::from_millis(20),
            Duration::from_millis(5),
        )
        .unwrap_err();
        assert!(err.to_string().contains("did not come back up"), "{err}");
    }

    #[test]
    fn require_service_platform_errors_clearly_for_an_unsupported_os() {
        assert_eq!(require_service_platform("macos").unwrap(), crate::fleet::service::ServicePlatform::Launchd);
        let err = require_service_platform("windows").unwrap_err();
        assert!(err.to_string().contains("windows"), "{err}");
    }

    /// Regression for a real finding: a systemd unit installed before
    /// `KillMode=process` was added would still default to `control-group`,
    /// and restarting it would kill every pane in its cgroup (see
    /// `fleet::service`'s `systemd_unit_restarts_only_the_engine_process_not_its_whole_cgroup`).
    /// Only a *confirmed* `process` KillMode is safe to restart.
    #[test]
    fn kill_mode_is_safe_only_for_process() {
        assert!(kill_mode_is_safe("process"));
        assert!(!kill_mode_is_safe("control-group"));
        assert!(!kill_mode_is_safe("mixed"));
        assert!(!kill_mode_is_safe(""));
    }

    /// Regression for a real finding (code review on this PR): launchd's
    /// default when it stops a job is to signal its whole *process group*,
    /// not just the one tracked process — the exact same class of bug as
    /// the systemd cgroup case above, just keyed by process group instead
    /// of cgroup. `launchctl print`'s `properties` line lists
    /// `AbandonProcessGroup` as literal text when set
    /// (`"properties = abandon process group | inferred program"`) and
    /// omits it entirely when unset — verified against a real launchd job.
    #[test]
    fn launchd_abandons_process_group_detects_the_properties_line() {
        assert!(launchd_abandons_process_group("\tproperties = abandon process group | inferred program\n"));
        assert!(!launchd_abandons_process_group("\tproperties = inferred program\n"));
        assert!(!launchd_abandons_process_group(""));
    }

    /// Regression for a real finding: scp'ing straight onto
    /// `REMOTE_INSTALL_DIR/ninox` can hit `ETXTBSY` on Linux, or crash a
    /// running process on macOS, when that path is the binary `ninox
    /// service install` already pointed the engine/ptyd at — which is the
    /// normal case for `machine update`, not an edge case. The install must
    /// stage to a temp name and atomically rename it into place instead.
    #[test]
    fn install_tmp_name_is_never_the_real_binary_name() {
        assert_ne!(install_tmp_name(), "ninox");
        assert!(install_tmp_name().starts_with('.'), "{}", install_tmp_name());
    }

    #[test]
    fn atomic_install_script_renames_into_place_rather_than_overwriting_directly() {
        let script = atomic_install_script(".ninox.update-123");
        assert!(script.contains("mv -f"), "{script}");
        assert!(script.contains(".ninox.update-123"), "{script}");
        assert!(script.ends_with("/ninox\""), "{script}");
    }

    #[test]
    fn stage_and_install_script_copies_into_the_temp_name_before_the_atomic_rename() {
        let script = stage_and_install_script("\"$HOME/.cargo/bin/ninox-tui\"", ".ninox.update-99");
        assert!(
            script.contains("cp \"$HOME/.cargo/bin/ninox-tui\" \"$HOME/.local/bin/.ninox.update-99\""),
            "{script}"
        );
        assert!(script.contains("mv -f"), "{script}");
    }

    #[test]
    fn needs_binary_install_true_when_missing_or_mismatched() {
        assert!(needs_binary_install(None, "0.32.0"));
        assert!(needs_binary_install(Some("0.31.0"), "0.32.0"));
        assert!(needs_binary_install(Some("0.33.0"), "0.32.0"));
        assert!(!needs_binary_install(Some("0.32.0"), "0.32.0"));
    }

    fn fake_profile(id: &str, ssh_target: &str) -> MachineProfile {
        MachineProfile {
            id: id.to_string(),
            label: "box".to_string(),
            ssh_target: ssh_target.to_string(),
            remote_session: "default".to_string(),
            enabled: true,
        }
    }

    #[test]
    fn attach_exec_argv_builds_the_ssh_recursion_command() {
        let profile = fake_profile("prof1", "user@box.example");
        let argv = attach_exec_argv(&profile, "sess-1");
        assert_eq!(
            argv,
            vec![
                "ssh".to_string(),
                "-t".to_string(),
                "-S".to_string(),
                control_socket_path("prof1").display().to_string(),
                "--".to_string(),
                "user@box.example".to_string(),
                "sh -lc 'export PATH=\"$HOME/.local/bin:$PATH\"; ninox pane attach '\\''sess-1'\\'''".to_string(),
            ]
        );
    }

    #[test]
    fn attach_exec_argv_with_places_flags_inside_the_quoted_remote_command() {
        let profile = fake_profile("prof1", "box");
        let argv = attach_exec_argv_with(&profile, "sess-1", &["--no-detach", "--follow-size"]);
        assert_eq!(argv.len(), 7, "{argv:?}");
        assert!(
            argv.last().unwrap().contains("ninox pane attach --no-detach --follow-size "),
            "{}",
            argv.last().unwrap()
        );
        assert_eq!(attach_exec_argv_with(&profile, "sess-1", &[]), attach_exec_argv(&profile, "sess-1"));
    }

    #[test]
    fn follow_size_support_is_cached_and_never_caches_an_inconclusive_probe() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("c.follow-size");
        let now = std::time::SystemTime::now();
        assert!(!follow_size_support(&cache, now, || ProbeOutcome::Inconclusive));
        assert!(!cache.exists());
        assert!(follow_size_support(&cache, now, || ProbeOutcome::Supported));
        assert!(follow_size_support(&cache, now, || panic!("cache hit must not re-probe")));
        // Past the yes TTL it probes again.
        let later = now + FOLLOW_SIZE_YES_TTL + Duration::from_secs(1);
        assert!(!follow_size_support(&cache, later, || ProbeOutcome::Unsupported));
        assert!(!follow_size_support(&cache, now, || panic!("fresh no is cached")));
        let much_later = now + FOLLOW_SIZE_NO_TTL + Duration::from_secs(1);
        assert!(follow_size_support(&cache, much_later, || ProbeOutcome::Supported));
    }

    /// Regression for a real finding: `ssh` joins every trailing argument
    /// with a single space before sending them to the remote shell, so a
    /// `"sh"`, `"-lc"`, `<cmd>` split across three separate argv elements
    /// would hand `-lc` only its first word as the command (`export`,
    /// which no-ops) and silently drop the `PATH` assignment and the actual
    /// `ninox pane attach` invocation after it. The whole `sh -lc '<cmd>'`
    /// must be exactly one argv element.
    #[test]
    fn attach_exec_argv_is_a_single_trailing_argument_not_several() {
        let profile = fake_profile("prof1", "box");
        let argv = attach_exec_argv(&profile, "sess-1");
        assert_eq!(argv.len(), 7, "{argv:?}");
        assert!(argv.last().unwrap().starts_with("sh -lc "), "{}", argv.last().unwrap());
    }

    #[test]
    fn attach_exec_argv_shell_quotes_the_session_id() {
        let profile = fake_profile("prof1", "box");
        let argv = attach_exec_argv(&profile, "it's-weird");
        let last = argv.last().unwrap();
        // The id goes through shell_quote twice — once for its own `ninox
        // pane attach` argument, once more as part of the outer `sh -lc`
        // wrapping — so this only checks the outer quoting is well-formed
        // and the id's content survives, not an exact escaped literal.
        assert!(last.starts_with("sh -lc '") && last.ends_with('\''), "{last}");
        assert!(last.contains("ninox pane attach"), "{last}");
        assert!(last.contains("weird"), "{last}");
    }

    #[test]
    fn parse_orchestrators_json_extracts_id_and_name() {
        let json = br#"{"orchestrators":[{"id":"o1","name":"default","created_at":0}],"sessions":[]}"#;
        let got = parse_orchestrators_json(json).unwrap();
        assert_eq!(got, vec![RemoteOrchestrator { id: "o1".into(), name: "default".into() }]);
    }

    #[test]
    fn parse_orchestrators_json_skips_entries_missing_fields() {
        let json = br#"{"orchestrators":[{"id":"o1"},{"id":"o2","name":"ok"}]}"#;
        let got = parse_orchestrators_json(json).unwrap();
        assert_eq!(got, vec![RemoteOrchestrator { id: "o2".into(), name: "ok".into() }]);
    }

    #[test]
    fn parse_orchestrators_json_empty_without_the_key() {
        let json = br#"{"sessions":[]}"#;
        assert!(parse_orchestrators_json(json).unwrap().is_empty());
    }

    /// Whether a local sshd answers `localhost` right now — gates every
    /// real-SSH-round-trip test below, which otherwise skip gracefully (most
    /// dev machines don't run sshd by default; CI/this workstation's
    /// acceptance test uses a container instead — see the PR description for
    /// what was actually exercised). A real reachable-remote-host
    /// integration environment would let these run unconditionally instead.
    fn local_sshd_reachable() -> bool {
        matches!(
            Command::new("ssh")
                .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=2", "localhost", "true"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status(),
            Ok(s) if s.success()
        )
    }

    /// End-to-end against a real local sshd, when one is reachable — see
    /// [`local_sshd_reachable`].
    #[test]
    fn control_master_against_localhost_if_sshd_is_up() {
        if !local_sshd_reachable() {
            eprintln!("skipping: no reachable localhost sshd");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let control_path = dir.path().join("test.sock");
        ensure_control_master("localhost", &control_path).unwrap();
        assert!(control_master_alive("localhost", &control_path));
        let out = run_remote_shell("localhost", &control_path, "echo hi").unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi");
        close_control_master("localhost", &control_path);
    }

    /// `reconnect_control_master` against a real local sshd: a no-op when
    /// already alive, and able to re-establish the control connection from
    /// scratch after it's torn down — the two cases the background health
    /// probe actually needs. See [`local_sshd_reachable`].
    #[test]
    fn reconnect_control_master_against_localhost_if_sshd_is_up() {
        if !local_sshd_reachable() {
            eprintln!("skipping: no reachable localhost sshd");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let control_path = dir.path().join("test.sock");

        // From scratch: no control master exists yet.
        reconnect_control_master("localhost", &control_path).unwrap();
        assert!(control_master_alive("localhost", &control_path));

        // Already alive: must be a no-op, not an error.
        reconnect_control_master("localhost", &control_path).unwrap();
        assert!(control_master_alive("localhost", &control_path));

        close_control_master("localhost", &control_path);
        assert!(!control_master_alive("localhost", &control_path));

        // Torn down, then reconnected.
        reconnect_control_master("localhost", &control_path).unwrap();
        assert!(control_master_alive("localhost", &control_path));
        close_control_master("localhost", &control_path);
    }

    #[test]
    fn forward_spec_formats_local_to_remote_loopback() {
        assert_eq!(forward_spec(51000, 7777), "51000:127.0.0.1:7777");
    }

    #[test]
    fn local_forward_port_is_deterministic_and_in_the_dynamic_range() {
        let a = local_forward_port("abc");
        let b = local_forward_port("abc");
        let c = local_forward_port("xyz");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a >= 49152);
    }

    #[test]
    fn forward_active_reflects_a_real_local_listener() {
        // Only the positive case is asserted: re-probing right after
        // dropping the listener would race an unrelated process for the
        // freed port on a busy shared machine.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(forward_active(port));
    }

    /// Real SSH round-trip for the tunnel itself: forwards a local port to a
    /// plain TCP listener standing in for a remote `ninox-server`, over a
    /// real `ControlMaster` to `localhost` — see [`local_sshd_reachable`].
    #[test]
    fn port_forward_against_localhost_if_sshd_is_up() {
        if !local_sshd_reachable() {
            eprintln!("skipping: no reachable localhost sshd");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let control_path = dir.path().join("test.sock");
        ensure_control_master("localhost", &control_path).unwrap();

        use std::io::{Read, Write};
        let remote_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let remote_port = remote_listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = remote_listener.accept().unwrap();
            let mut buf = [0u8; 5];
            stream.read_exact(&mut buf).unwrap();
            stream.write_all(b"world").unwrap();
        });

        let local_port = local_forward_port("port-forward-test-profile");
        ensure_port_forward("localhost", &control_path, local_port, remote_port).unwrap();
        assert!(forward_active(local_port));

        let mut client = std::net::TcpStream::connect(("127.0.0.1", local_port)).unwrap();
        client.write_all(b"hello").unwrap();
        let mut resp = [0u8; 5];
        client.read_exact(&mut resp).unwrap();
        assert_eq!(&resp, b"world");

        server.join().unwrap();
        cancel_port_forward("localhost", &control_path, local_port, remote_port);
        close_control_master("localhost", &control_path);
    }

    #[test]
    fn parse_uname_output_normalizes_os_and_arch() {
        assert_eq!(parse_uname_output("Darwin arm64\n"), Some(("macos".into(), "aarch64".into())));
        assert_eq!(parse_uname_output("Linux x86_64\n"), Some(("linux".into(), "x86_64".into())));
        assert_eq!(parse_uname_output("Linux aarch64"), Some(("linux".into(), "aarch64".into())));
        assert_eq!(parse_uname_output(""), None);
        assert_eq!(parse_uname_output("Linux"), None);
    }

    #[test]
    fn local_platform_matches_env_consts() {
        assert_eq!(local_platform(), (std::env::consts::OS.to_string(), std::env::consts::ARCH.to_string()));
    }

    #[test]
    fn package_name_for_maps_the_real_crate_names_and_the_nx_alias() {
        assert_eq!(package_name_for(Path::new("/usr/local/bin/ninox-gui")).unwrap(), "ninox-gui");
        assert_eq!(package_name_for(Path::new("/usr/local/bin/ninox-tui")).unwrap(), "ninox-tui");
        assert_eq!(package_name_for(Path::new("/home/u/.cargo/bin/nx")).unwrap(), "ninox-tui");
    }

    /// Regression for a real finding: `std::env::current_exe()` reports the
    /// path ninox was invoked *through* — on macOS that's the `ninox`/`nx`
    /// alias symlink itself, not the real binary it points to — so a caller
    /// that forgot to resolve it first (instead of `hooks::canonical_exe()`)
    /// would hand this function a `local_bin` literally named `ninox`. That
    /// must fail clearly here, not silently fall through to a confusing
    /// `extract_binary`/`cargo install` failure for a binary name that
    /// matches no real tarball entry or crate.
    #[test]
    fn package_name_for_rejects_the_unresolved_ninox_alias_name() {
        let err = package_name_for(Path::new("/home/u/.local/bin/ninox")).unwrap_err().to_string();
        assert!(err.contains("ninox-gui"), "{err}");
        assert!(err.contains("ninox-tui"), "{err}");
        assert!(err.contains("unresolved"), "{err}");
    }

    /// Regression for a real finding: a macOS GUI host (`local_bin` =
    /// `ninox-gui`) installing a prebuilt to a Linux remote must still
    /// extract `ninox-tui` from the tarball — Linux remotes are
    /// ptyd/TUI-only (spec resolution #6) and the Linux tarball never
    /// ships `ninox-gui`, so using the *local* host's own binary name
    /// here would look for an entry that doesn't exist.
    #[test]
    fn remote_bin_name_always_extracts_ninox_tui_for_a_linux_remote() {
        assert_eq!(
            remote_bin_name("linux", Path::new("/Applications/Ninox.app/Contents/MacOS/ninox-gui")).unwrap(),
            "ninox-tui"
        );
        assert_eq!(remote_bin_name("linux", Path::new("/usr/local/bin/ninox-tui")).unwrap(), "ninox-tui");
        assert_eq!(remote_bin_name("linux", Path::new("/home/u/.cargo/bin/nx")).unwrap(), "ninox-tui");
    }

    #[test]
    fn remote_bin_name_uses_the_local_binarys_own_name_for_a_macos_remote() {
        assert_eq!(
            remote_bin_name("macos", Path::new("/Applications/Ninox.app/Contents/MacOS/ninox-gui")).unwrap(),
            "ninox-gui"
        );
        assert_eq!(remote_bin_name("macos", Path::new("/usr/local/bin/ninox-tui")).unwrap(), "ninox-tui");
    }

    #[test]
    fn cargo_fallback_unavailable_message_names_the_arch_and_reason() {
        let err = cargo_fallback_unavailable("linux", "x86_64", "no Rust toolchain found on host");
        assert_eq!(
            err.to_string(),
            "no prebuilt binary for linux/x86_64 and CodeArtifact fallback is not available: \
             no Rust toolchain found on host"
        );
    }

    /// A [`RemoteInstallBackend`] that never touches the network, recording
    /// which steps `install_remote_binary` actually invoked.
    #[derive(Default)]
    struct FakeBackend {
        remote_platform: (String, String),
        prebuilt_result: Option<Result<bool, String>>,
        cargo_fallback_result: Option<Result<(), String>>,
        copy_local_called: RefCell<bool>,
        install_prebuilt_called: RefCell<bool>,
        cargo_fallback_called: RefCell<bool>,
    }

    impl RemoteInstallBackend for FakeBackend {
        fn detect_remote_platform(&self) -> Result<(String, String)> {
            Ok(self.remote_platform.clone())
        }

        fn copy_local_binary(&self) -> Result<()> {
            *self.copy_local_called.borrow_mut() = true;
            Ok(())
        }

        fn install_prebuilt(&self, _remote_os: &str, _remote_arch: &str) -> Result<bool> {
            *self.install_prebuilt_called.borrow_mut() = true;
            match self.prebuilt_result.as_ref().expect("prebuilt_result must be set when install_prebuilt may be called") {
                Ok(found) => Ok(*found),
                Err(e) => Err(anyhow::anyhow!(e.clone())),
            }
        }

        fn cargo_install_fallback(&self, remote_os: &str, remote_arch: &str) -> Result<()> {
            *self.cargo_fallback_called.borrow_mut() = true;
            match self.cargo_fallback_result.clone().expect("cargo_fallback_result must be set when cargo_install_fallback may be called") {
                Ok(()) => Ok(()),
                Err(reason) => Err(cargo_fallback_unavailable(remote_os, remote_arch, &reason)),
            }
        }
    }

    #[test]
    fn matching_arch_uses_the_fast_local_binary_copy_path_unchanged() {
        let backend = FakeBackend { remote_platform: ("macos".into(), "aarch64".into()), ..Default::default() };
        let method = install_remote_binary(&backend, ("macos", "aarch64")).unwrap();
        assert_eq!(method, InstallMethod::CopyLocalBinary);
        assert!(*backend.copy_local_called.borrow());
        assert!(!*backend.install_prebuilt_called.borrow());
        assert!(!*backend.cargo_fallback_called.borrow());
    }

    #[test]
    fn mismatched_supported_arch_tries_the_prebuilt_fetch_path_first() {
        let backend = FakeBackend {
            remote_platform: ("macos".into(), "aarch64".into()),
            prebuilt_result: Some(Ok(true)),
            cargo_fallback_result: Some(Ok(())),
            ..Default::default()
        };
        let method = install_remote_binary(&backend, ("linux", "x86_64")).unwrap();
        assert_eq!(method, InstallMethod::FetchPrebuilt);
        assert!(*backend.install_prebuilt_called.borrow());
        assert!(!*backend.copy_local_called.borrow());
        assert!(!*backend.cargo_fallback_called.borrow(), "the prebuilt was found — no need to fall back");
    }

    #[test]
    fn mismatched_unsupported_arch_skips_straight_to_cargo_install_fallback() {
        let backend = FakeBackend {
            remote_platform: ("linux".into(), "arm".into()),
            cargo_fallback_result: Some(Ok(())),
            ..Default::default()
        };
        let method = install_remote_binary(&backend, ("macos", "aarch64")).unwrap();
        assert_eq!(method, InstallMethod::CargoInstallFallback);
        assert!(!*backend.install_prebuilt_called.borrow(), "linux/arm has no published prebuilt");
        assert!(*backend.cargo_fallback_called.borrow());
    }

    #[test]
    fn linux_x86_64_remote_now_tries_the_prebuilt_fetch_path() {
        let backend = FakeBackend {
            remote_platform: ("linux".into(), "x86_64".into()),
            prebuilt_result: Some(Ok(true)),
            cargo_fallback_result: Some(Ok(())),
            ..Default::default()
        };
        let method = install_remote_binary(&backend, ("macos", "aarch64")).unwrap();
        assert_eq!(method, InstallMethod::FetchPrebuilt);
        assert!(*backend.install_prebuilt_called.borrow());
        assert!(!*backend.cargo_fallback_called.borrow());
    }

    #[test]
    fn no_matching_prebuilt_falls_back_to_cargo_install() {
        let backend = FakeBackend {
            remote_platform: ("macos".into(), "aarch64".into()),
            prebuilt_result: Some(Ok(false)),
            cargo_fallback_result: Some(Ok(())),
            ..Default::default()
        };
        let method = install_remote_binary(&backend, ("linux", "x86_64")).unwrap();
        assert_eq!(method, InstallMethod::CargoInstallFallback);
        assert!(*backend.install_prebuilt_called.borrow());
        assert!(*backend.cargo_fallback_called.borrow());
    }

    #[test]
    fn an_unavailable_cargo_fallback_surfaces_a_clear_specific_error_not_a_passthrough() {
        let backend = FakeBackend {
            remote_platform: ("linux".into(), "arm".into()),
            cargo_fallback_result: Some(Err("no Rust toolchain found on host (`cargo --version` failed)".into())),
            ..Default::default()
        };
        let err = install_remote_binary(&backend, ("macos", "aarch64")).unwrap_err();
        assert_eq!(
            err.to_string(),
            "no prebuilt binary for linux/arm and CodeArtifact fallback is not available: \
             no Rust toolchain found on host (`cargo --version` failed)"
        );
    }
}
