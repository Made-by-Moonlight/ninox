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

use anyhow::{bail, ensure, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// Directory holding per-profile SSH `ControlMaster` sockets. Deliberately
/// `/tmp` rather than `std::env::temp_dir()`: a `sockaddr_un` path is capped
/// around 104 bytes (macOS) and `TMPDIR` is a long per-session path there
/// (`/var/folders/.../T/`) that, combined with a profile id and OpenSSH's
/// own atomic-replace suffix on the socket file, overflows that limit.
fn control_dir() -> PathBuf {
    PathBuf::from("/tmp/ninox-ssh-control")
}

/// The `ControlPath` for a given machine profile id. Deterministic per id,
/// so repeated commands against the same profile reuse the same socket.
/// Hashed down to a short hex token (rather than using the id — typically a
/// full UUID — verbatim) to leave headroom under the socket path length
/// limit alongside `control_dir()` and OpenSSH's own suffix.
pub fn control_socket_path(profile_id: &str) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    profile_id.hash(&mut hasher);
    control_dir().join(format!("{:016x}.sock", hasher.finish()))
}

/// Single-quote `s` for safe inclusion in a remote shell command line,
/// escaping any embedded single quotes (`'` -> `'\''`).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Whether a `ControlMaster` connection for `target` is already alive at
/// `control_path` (`ssh -O check`).
pub fn control_master_alive(target: &str, control_path: &Path) -> bool {
    Command::new("ssh")
        .arg("-S")
        .arg(control_path)
        .arg("-O")
        .arg("check")
        .arg("--")
        .arg(target)
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
pub fn close_control_master(target: &str, control_path: &Path) {
    let _ = Command::new("ssh")
        .arg("-S")
        .arg(control_path)
        .arg("-O")
        .arg("exit")
        .arg("--")
        .arg(target)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
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

/// Copy the local `ninox` binary to `~/.local/bin/ninox` on `target`,
/// creating the directory and marking it executable. This is the only
/// "install" mechanism this module implements: it mirrors whatever
/// architecture/OS the *local* binary was built for, which only works when
/// the remote machine matches — cross-architecture distribution is
/// explicitly unresolved (see the design spec's open question #7) and left
/// for a follow-up.
pub fn install_local_binary(target: &str, control_path: &Path, local_bin: &Path) -> Result<()> {
    run_remote_shell(target, control_path, &format!("mkdir -p \"$HOME/{REMOTE_INSTALL_DIR}\""))
        .ok()
        .filter(|o| o.status.success())
        .context("failed to create remote install directory")?;
    let mut cmd = Command::new("scp");
    cmd.arg("-o").arg(format!("ControlPath={}", control_path.display()));
    cmd.arg("--");
    cmd.arg(local_bin);
    cmd.arg(format!("{target}:{REMOTE_INSTALL_DIR}/ninox"));
    let status = cmd.status().context("failed to run scp")?;
    ensure!(status.success(), "scp to {target} failed");
    run_remote_shell(target, control_path, &format!("chmod +x \"$HOME/{REMOTE_INSTALL_DIR}/ninox\""))
        .ok()
        .filter(|o| o.status.success())
        .context("failed to chmod remote binary")?;
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

/// Whether `machine add` should offer to install/update the remote binary:
/// true when none is present, or its version doesn't match the local
/// (running) binary's own version. ninox's only distribution mechanism here
/// is copying the local binary over (see [`install_local_binary`]), so
/// "compatible" means "identical" — there is no cross-version wire
/// compatibility story to lean on instead.
pub fn needs_binary_install(remote_version: Option<&str>, local_version: &str) -> bool {
    remote_version != Some(local_version)
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn needs_binary_install_true_when_missing_or_mismatched() {
        assert!(needs_binary_install(None, "0.32.0"));
        assert!(needs_binary_install(Some("0.31.0"), "0.32.0"));
        assert!(needs_binary_install(Some("0.33.0"), "0.32.0"));
        assert!(!needs_binary_install(Some("0.32.0"), "0.32.0"));
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

    /// End-to-end against a real local sshd, when one is reachable — skips
    /// gracefully otherwise (most dev machines don't run sshd by default;
    /// CI/this workstation's acceptance test uses a container instead — see
    /// the PR description for what was actually exercised).
    #[test]
    fn control_master_against_localhost_if_sshd_is_up() {
        let probe = Command::new("ssh")
            .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=2", "localhost", "true"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if !matches!(probe, Ok(s) if s.success()) {
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
}
