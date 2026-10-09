//! `ninox machine add|list|remove` — SSH-connected remote machines (spec
//! §5). Transport and probing live in `ninox_core::remote`; this file is
//! just the interactive CLI flow and config persistence on top of it.

use anyhow::{bail, ensure, Context, Result};
use clap::Subcommand;
use ninox_core::config::{AppConfig, MachineProfile};
use ninox_core::remote;
use std::io::Write;

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
}

pub fn run(action: MachineAction) -> Result<()> {
    match action {
        MachineAction::Add { host, label, remote_session } => run_add(host, label, remote_session),
        MachineAction::List { json } => run_list(json),
        MachineAction::Remove { id } => run_remove(&id),
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

fn run_remove(id: &str) -> Result<()> {
    let (_, removed) = AppConfig::update(|cfg| {
        let before = cfg.remote_machines.machines.len();
        cfg.remote_machines.machines.retain(|m| m.id != id);
        before != cfg.remote_machines.machines.len()
    })?;
    ensure!(removed, "no machine profile with id {id}");
    println!("removed machine {id}");
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
            let local_bin = std::env::current_exe()?;
            println!("Copying local ninox binary to {host}:~/{}/ninox...", remote::REMOTE_INSTALL_DIR);
            remote::install_local_binary(&host, &control_path, &local_bin)?;
            let installed = remote::probe_remote_version(&host, &control_path)?;
            ensure!(
                installed.as_deref() == Some(local_version),
                "remote binary did not report the expected version after install (got {installed:?})"
            );
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
            println!("Restarting live sessions on {host} to pick up the new build...");
            let _ = remote::run_remote_ninox(&host, &control_path, "restart --all");
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
    fn default_label_uses_the_host_portion_of_user_at_host() {
        assert_eq!(default_label("ethan@10.0.0.5"), "10.0.0.5");
        assert_eq!(default_label("build-box"), "build-box");
    }
}
