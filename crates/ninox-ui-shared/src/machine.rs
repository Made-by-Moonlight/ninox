//! `ninox machine add|list|remove` — SSH-connected remote machines (spec
//! §5). Transport and probing live in `ninox_core::remote`; this file is
//! just the interactive CLI flow and config persistence on top of it.

use anyhow::{bail, ensure, Context, Result};
use clap::Subcommand;
use ninox_core::config::{AppConfig, MachineProfile};
use ninox_core::remote;
use ninox_core::types::MachineHealth;
use std::collections::HashMap;
use std::io::Write;

/// A configured machine joined with its last-observed connection health —
/// shared by the TUI's Remotes tab and the GUI's (`ninox-tui`'s `remotes`
/// module re-exports this rather than defining its own copy).
#[derive(Debug, Clone)]
pub struct RemoteRow {
    /// Carried through from `MachineProfile.id`: callers key remove/confirm
    /// lookups off this rather than the row's position.
    pub id: String,
    pub label: String,
    pub ssh_target: String,
    pub remote_session: String,
    pub enabled: bool,
    pub health: Option<MachineHealth>,
}

pub fn collect_rows(cfg: &AppConfig, health: &HashMap<String, MachineHealth>) -> Vec<RemoteRow> {
    cfg.remote_machines
        .machines
        .iter()
        .map(|m| RemoteRow {
            id: m.id.clone(),
            label: m.label.clone(),
            ssh_target: m.ssh_target.clone(),
            remote_session: m.remote_session.clone(),
            enabled: m.enabled,
            health: health.get(&m.id).copied(),
        })
        .collect()
}

#[derive(Subcommand)]
pub enum MachineAction {
    /// Connect to a remote machine over SSH and track one of its sessions
    Add {
        /// SSH target: `user@host`, a bare host, or a `~/.ssh/config` alias
        host: String,
        /// Label shown in `machine list` / the sidebar (default: the host
        /// portion of the SSH target)
        #[arg(long)]
        label: Option<String>,
        /// Track this remote orchestrator/session name without prompting
        #[arg(long)]
        remote_session: Option<String>,
    },
    /// List saved machine profiles
    List {
        /// Emit JSON instead of one line per machine
        #[arg(long)]
        json: bool,
    },
    /// Remove a saved machine profile (does not touch the remote machine)
    Remove {
        /// Profile id (see `ninox machine list`)
        id: String,
    },
    /// Check an already-added machine for a version-skewed remote binary
    /// and, with approval, upgrade it to match this machine's version
    Update {
        /// Profile id (see `ninox machine list`)
        id: String,
    },
}

pub fn run(action: MachineAction) -> Result<()> {
    match action {
        MachineAction::Add { host, label, remote_session } => run_add(host, label, remote_session),
        MachineAction::List { json } => run_list(json),
        MachineAction::Remove { id } => run_remove(&id),
        MachineAction::Update { id } => run_update(&id),
    }
}

fn run_list(json: bool) -> Result<()> {
    let cfg = AppConfig::load()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&cfg.remote_machines.machines)?);
        return Ok(());
    }
    if cfg.remote_machines.machines.is_empty() {
        println!("no machines configured");
        return Ok(());
    }
    for m in &cfg.remote_machines.machines {
        println!(
            "{}  {:<16}  {:<28} -> {:<16} [{}]",
            m.id,
            m.label,
            m.ssh_target,
            m.remote_session,
            if m.enabled { "enabled" } else { "disabled" },
        );
    }
    Ok(())
}

/// Forget a saved machine profile; the remote machine itself is untouched.
/// Shared by the CLI (`ninox machine remove`) and the TUI's Remotes-tab `d`
/// key — silent, since the TUI owns the terminal and can't have this print
/// straight to the alternate screen outside ratatui's own diffing.
pub fn remove(id: &str) -> Result<()> {
    let (_, removed_ssh_target) = AppConfig::update(|cfg| {
        let i = cfg.remote_machines.machines.iter().position(|m| m.id == id)?;
        Some(cfg.remote_machines.machines.remove(i).ssh_target)
    })?;
    let ssh_target = removed_ssh_target.ok_or_else(|| anyhow::anyhow!("no machine profile with id {id}"))?;
    // Closes any port forward this profile's RemoteBackend established too
    // (`-O exit` tears down the whole multiplexed connection) — best-effort,
    // same as this already was before the profile had a forward to leak.
    remote::close_control_master(&ssh_target, &remote::control_socket_path(id));
    Ok(())
}

fn run_remove(id: &str) -> Result<()> {
    remove(id)?;
    println!("removed machine {id}");
    Ok(())
}

/// The saved profile with this exact `id`, or a clear error — same wording
/// `run_remove` already uses for a missing id.
fn find_profile<'a>(cfg: &'a AppConfig, id: &str) -> Result<&'a MachineProfile> {
    cfg.remote_machines.machines.iter().find(|m| m.id == id).ok_or_else(|| anyhow::anyhow!("no machine profile with id {id}"))
}

fn run_update(id: &str) -> Result<()> {
    let cfg = AppConfig::load()?;
    let profile = find_profile(&cfg, id)?;
    let host = profile.ssh_target.clone();
    let control_path = remote::control_socket_path(&profile.id);

    remote::ensure_control_master(&host, &control_path).context("could not establish an SSH connection")?;

    let local_version = env!("CARGO_PKG_VERSION");
    let remote_version = remote::probe_remote_version(&host, &control_path)?;

    if !remote::needs_binary_install(remote_version.as_deref(), local_version) {
        println!("{host} is already on ninox {local_version}");
        return Ok(());
    }

    match &remote_version {
        Some(v) => println!("{host}: ninox {v} -> {local_version}"),
        None => println!("{host} has no ninox binary — install ninox {local_version}?"),
    }
    if !prompt_yes_no("Proceed?")? {
        bail!("aborted: update declined");
    }

    println!("Updating ninox on {host}...");
    let method = install_and_verify_binary(&host, &control_path, local_version)?;
    println!("{}", describe_install_method(method, &host));

    if remote_version.is_some() {
        upgrade_remote_daemons(&host, &control_path)?;
    }

    Ok(())
}

fn prompt_yes_no(question: &str) -> Result<bool> {
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

fn default_label(target: &str) -> String {
    target.rsplit('@').next().unwrap_or(target).to_string()
}

/// The error `install_and_verify_binary` raises when the remote doesn't
/// report `local_version` right after an install — pulled out as its own
/// function so the exact wording is covered by a test.
fn version_mismatch_after_install(installed: Option<&str>) -> anyhow::Error {
    anyhow::anyhow!("remote binary did not report the expected version after install (got {installed:?})")
}

/// Human-readable description of how [`remote::install_remote_binary`] put
/// the binary on `host` — shared by `machine add`'s and `machine update`'s
/// post-install messaging.
fn describe_install_method(method: remote::InstallMethod, host: &str) -> String {
    match method {
        remote::InstallMethod::CopyLocalBinary => {
            format!("Copied the local ninox binary to {host}:~/{}/ninox.", remote::REMOTE_INSTALL_DIR)
        }
        remote::InstallMethod::FetchPrebuilt => {
            format!("{host}'s architecture differs from this machine's — installed a matching prebuilt binary instead.")
        }
        remote::InstallMethod::CargoInstallFallback => format!(
            "{host}'s architecture differs from this machine's and no matching prebuilt was available — \
             installed via `cargo install` on {host} instead (requires a Rust toolchain there)."
        ),
    }
}

/// The warning printed when a best-effort post-install `restart --all`
/// fails on `host` — pulled out as its own function so the wording is
/// covered by a test.
fn restart_warning(host: &str, reason: &str) -> String {
    format!("warning: restarting live sessions on {host} failed: {reason}")
}

/// Best-effort `ninox restart --all` on `target` after an install — shared
/// by `machine add` and `machine update`. Never fails the caller (a session
/// restart hiccup shouldn't undo a successful binary upgrade), but a
/// failure is printed rather than silently swallowed.
fn restart_remote_sessions(target: &str, control_path: &std::path::Path) {
    match remote::run_remote_ninox(target, control_path, "restart --all") {
        Ok(out) if !out.status.success() => {
            println!("{}", restart_warning(target, String::from_utf8_lossy(&out.stderr).trim()));
        }
        Err(e) => println!("{}", restart_warning(target, &e.to_string())),
        Ok(_) => {}
    }
}

/// The warning printed when the best-effort headless-engine service restart
/// fails on `host` — pulled out as its own function so the wording is
/// covered by a test, distinct from [`restart_warning`]'s session-restart
/// wording.
fn engine_restart_warning(host: &str, reason: &str) -> String {
    format!("warning: restarting the ninox engine service on {host} failed: {reason}")
}

/// Whether the ptyd live-upgrade step has anything to do: with nothing
/// running on `target` to begin with, there are no panes to preserve, so
/// triggering a takeover would just start a brand-new `ninox-ptyd` daemon
/// the remote never had (e.g. a tmux-backend remote, or one that hasn't
/// been used yet) — skipped entirely rather than treated as a from-scratch
/// "success".
fn ptyd_upgrade_needed(previous: &Option<remote::PtydStatus>) -> bool {
    previous.is_some()
}

/// After a binary install where a previous version was running, bring
/// `target`'s long-running `ninox-ptyd`/engine daemons onto the newly
/// installed build without losing any live pane (spec resolved decision
/// #5) — shared by `machine add` and `machine update`. The ptyd live
/// upgrade is the correctness-critical half (its failure aborts the
/// caller — including a failure to probe `target`'s *current* ptyd status,
/// which must not be mistaken for "nothing was running"); the
/// engine-service restart and agent-session refresh are orthogonal
/// best-effort steps, same as `restart_remote_sessions` already was.
fn upgrade_remote_daemons(target: &str, control_path: &std::path::Path) -> Result<()> {
    let previous = remote::probe_remote_ptyd_status(target, control_path)?;
    if ptyd_upgrade_needed(&previous) {
        println!("Live-upgrading ptyd on {target}...");
        remote::trigger_remote_ptyd_takeover(target, control_path)?;
        remote::wait_for_ptyd_takeover(target, control_path, previous, remote::PTYD_TAKEOVER_TIMEOUT)?;
        println!("ptyd on {target} is live-upgraded.");
    }

    match remote::restart_remote_engine_service(target, control_path) {
        Ok(()) => println!("Restarted the ninox engine service on {target}."),
        Err(e) => println!("{}", engine_restart_warning(target, &e.to_string())),
    }

    println!("Restarting live sessions on {target} to pick up the new build...");
    restart_remote_sessions(target, control_path);
    Ok(())
}

/// Installs/updates the remote binary on `target` so it matches
/// `local_version` exactly, then verifies the result — the common tail of
/// `machine add`'s approved install and `machine update`'s approved
/// upgrade.
fn install_and_verify_binary(target: &str, control_path: &std::path::Path, local_version: &str) -> Result<remote::InstallMethod> {
    // `current_exe()` reports the path ninox was *invoked* through — on
    // macOS that's the `ninox`/`nx` alias symlink itself, not what it
    // resolves to. `install_remote_binary`'s prebuilt-fetch and
    // cargo-install-fallback paths key off the binary's file name (to find
    // it in the release tarball / know which crate to install), so an
    // unresolved `ninox`/`nx` name would break both; `canonical_exe()` is
    // the same fix `update_cli.rs` already applies for this exact reason.
    let local_bin = ninox_core::hooks::canonical_exe()?;
    let local_platform = remote::local_platform();
    let backend = remote::SshInstallBackend { target, control_path, local_bin: &local_bin, local_version };
    let method = remote::install_remote_binary(&backend, (local_platform.0.as_str(), local_platform.1.as_str()))?;
    let installed = remote::probe_remote_version(target, control_path)?;
    if installed.as_deref() != Some(local_version) {
        return Err(version_mismatch_after_install(installed.as_deref()));
    }
    Ok(method)
}

fn run_add(host: String, label: Option<String>, remote_session_override: Option<String>) -> Result<()> {
    let existing = AppConfig::load()?;
    ensure!(
        !existing.remote_machines.machines.iter().any(|m| m.ssh_target == host),
        "a machine profile already tracks {host} — remove it first (`ninox machine remove <id>`) \
         to re-add it differently"
    );

    let profile_id = MachineProfile::new_id();
    let control_path = remote::control_socket_path(&profile_id);

    println!("Connecting to {host} over SSH (you may be prompted by ssh itself)...");
    remote::ensure_control_master(&host, &control_path).context("could not establish an SSH connection")?;

    let local_version = env!("CARGO_PKG_VERSION");
    let remote_version = remote::probe_remote_version(&host, &control_path)?;
    let install_needed = remote::needs_binary_install(remote_version.as_deref(), local_version);

    let mut discovered =
        if install_needed { None } else { remote::list_remote_orchestrators(&host, &control_path).ok() };
    let service_install_needed = discovered.is_none();

    if install_needed || service_install_needed {
        println!("The following changes on {host} need your approval:");
        match (&remote_version, install_needed) {
            (Some(v), true) => println!("  - update the ninox binary ({v} -> {local_version})"),
            (None, true) => println!("  - install the ninox binary (v{local_version})"),
            _ => {}
        }
        if service_install_needed {
            println!("  - run `ninox service install` (starts the headless engine)");
        }
        if !prompt_yes_no("Proceed?")? {
            remote::close_control_master(&host, &control_path);
            bail!("aborted: remote changes declined");
        }

        if install_needed {
            println!("Installing ninox on {host}...");
            let method = install_and_verify_binary(&host, &control_path, local_version)?;
            println!("{}", describe_install_method(method, &host));
        }

        println!("Running `ninox service install` on {host}...");
        let out = remote::run_remote_ninox(&host, &control_path, "service install")?;
        ensure!(
            out.status.success(),
            "ninox service install failed on {host}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );

        // A pre-existing binary (just the wrong version) implies there may
        // already be live sessions running under it — refresh them so they
        // pick up the build we just installed. A from-scratch install has
        // nothing running yet, so there's nothing to restart.
        if install_needed && remote_version.is_some() {
            upgrade_remote_daemons(&host, &control_path)?;
        }

        discovered = Some(remote::list_remote_orchestrators(&host, &control_path).context(
            "ninox service install ran but the remote ninox-server still isn't reachable",
        )?);
    }

    let discovered = discovered.unwrap_or_default();

    let remote_session = match remote_session_override {
        Some(name) => {
            if !discovered.iter().any(|o| o.name == name) {
                println!(
                    "warning: no session named '{name}' was found on {host} — tracking it anyway, \
                     since it was explicitly requested (check `ninox list` on {host} if this is unexpected)"
                );
            }
            name
        }
        None => pick_remote_session(&host, &control_path, &discovered)?,
    };

    let label = label.unwrap_or_else(|| default_label(&host));
    AppConfig::update(|cfg| {
        cfg.remote_machines.enabled = true;
        cfg.remote_machines.machines.push(MachineProfile {
            id: profile_id.clone(),
            label: label.clone(),
            ssh_target: host.clone(),
            remote_session: remote_session.clone(),
            enabled: true,
        });
    })?;

    println!("Added machine '{label}' ({profile_id}) -> {host}, tracking session '{remote_session}'.");
    Ok(())
}

/// Resolve which remote orchestrator/session name to track: prompt among
/// multiple, pick the only one automatically, or offer to spawn a `default`
/// orchestrator when none exist yet (spec §5.4).
fn pick_remote_session(
    host: &str,
    control_path: &std::path::Path,
    discovered: &[remote::RemoteOrchestrator],
) -> Result<String> {
    match discovered {
        [] => {
            ensure!(
                prompt_yes_no(&format!(
                    "No existing sessions found on {host}. Spawn a default orchestrator there now?"
                ))?,
                "aborted: no remote session to track and no orchestrator was spawned"
            );
            let out =
                remote::run_remote_ninox(host, control_path, "spawn-orchestrator --name default --user-requested")?;
            ensure!(
                out.status.success(),
                "failed to spawn a remote orchestrator on {host}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
            Ok("default".to_string())
        }
        [only] => Ok(only.name.clone()),
        many => {
            println!("Multiple sessions found on {host}:");
            for (i, o) in many.iter().enumerate() {
                println!("  [{}] {} ({})", i + 1, o.name, o.id);
            }
            loop {
                print!("Pick one [1-{}]: ", many.len());
                std::io::stdout().flush()?;
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                if let Ok(idx) = line.trim().parse::<usize>() {
                    if idx >= 1 && idx <= many.len() {
                        return Ok(many[idx - 1].name.clone());
                    }
                }
                println!("invalid selection");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_rows_joins_health_by_id_and_preserves_config_order() {
        let mut cfg = AppConfig::default();
        cfg.remote_machines.machines = vec![
            MachineProfile { id: "a".into(), label: "A".into(), ssh_target: "a@host".into(), remote_session: "default".into(), enabled: true },
            MachineProfile { id: "b".into(), label: "B".into(), ssh_target: "b@host".into(), remote_session: "default".into(), enabled: false },
        ];
        let mut health = std::collections::HashMap::new();
        health.insert("a".to_string(), ninox_core::types::MachineHealth::Connected);

        let rows = collect_rows(&cfg, &health);

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "a");
        assert_eq!(rows[0].health, Some(ninox_core::types::MachineHealth::Connected));
        assert_eq!(rows[1].id, "b");
        assert_eq!(rows[1].health, None);
        assert!(!rows[1].enabled);
    }

    #[test]
    fn default_label_uses_the_host_portion_of_user_at_host() {
        assert_eq!(default_label("ethan@10.0.0.5"), "10.0.0.5");
        assert_eq!(default_label("build-box"), "build-box");
    }

    #[test]
    fn describe_install_method_names_the_host_for_each_method() {
        let host = "box.example";
        assert_eq!(
            describe_install_method(remote::InstallMethod::CopyLocalBinary, host),
            "Copied the local ninox binary to box.example:~/.local/bin/ninox."
        );
        assert!(describe_install_method(remote::InstallMethod::FetchPrebuilt, host)
            .contains("installed a matching prebuilt binary"));
        assert!(
            describe_install_method(remote::InstallMethod::CargoInstallFallback, host).contains("cargo install")
        );
    }

    #[test]
    fn restart_warning_names_the_host_and_reason() {
        let msg = restart_warning("box.example", "connection refused");
        assert!(msg.contains("box.example"), "{msg}");
        assert!(msg.contains("connection refused"), "{msg}");
    }

    #[test]
    fn engine_restart_warning_names_the_host_and_reason() {
        let msg = engine_restart_warning("box.example", "unit not found");
        assert!(msg.contains("box.example"), "{msg}");
        assert!(msg.contains("unit not found"), "{msg}");
        assert_ne!(
            msg,
            restart_warning("box.example", "unit not found"),
            "distinct wording from the session-restart warning"
        );
    }

    /// Regression for a real finding: with nothing running to preserve, the
    /// live-upgrade step must be skipped entirely rather than starting a new
    /// `ninox-ptyd` daemon on a remote that never had one (e.g. a
    /// tmux-backend remote, or one that hasn't been used yet).
    #[test]
    fn ptyd_upgrade_needed_only_when_something_was_already_running() {
        assert!(!ptyd_upgrade_needed(&None));
        assert!(ptyd_upgrade_needed(&Some(remote::PtydStatus { pid: 1, epoch_ms: 1 })));
    }

    #[test]
    fn version_mismatch_after_install_names_the_actual_version() {
        let err = version_mismatch_after_install(Some("0.31.0"));
        assert!(err.to_string().contains("0.31.0"), "{err}");
        let err = version_mismatch_after_install(None);
        assert!(err.to_string().contains("None"), "{err}");
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

    fn fake_config_with(machines: Vec<MachineProfile>) -> AppConfig {
        let mut cfg = AppConfig::default();
        cfg.remote_machines.machines = machines;
        cfg
    }

    #[test]
    fn find_profile_returns_the_matching_profile() {
        let cfg = fake_config_with(vec![fake_profile("prof1", "box")]);
        let found = find_profile(&cfg, "prof1").unwrap();
        assert_eq!(found.ssh_target, "box");
    }

    #[test]
    fn find_profile_errors_clearly_for_an_unknown_id() {
        let cfg = fake_config_with(vec![fake_profile("prof1", "box")]);
        let err = find_profile(&cfg, "nonexistent").unwrap_err();
        assert_eq!(err.to_string(), "no machine profile with id nonexistent");
    }

    #[test]
    fn update_subcommand_parses_the_profile_id() {
        use clap::Parser;
        #[derive(clap::Parser)]
        struct Cli {
            #[command(subcommand)]
            action: MachineAction,
        }
        let cli = Cli::parse_from(["ninox-machine", "update", "prof1"]);
        assert!(matches!(cli.action, MachineAction::Update { id } if id == "prof1"));
    }
}
