//! `ninox service install|uninstall|status` — the login autostart for the
//! headless engine (spec §5.4). Unit contents and command plans come from
//! `ninox_core::fleet::service`; this file only touches the filesystem and
//! runs launchctl/systemctl.

use clap::Subcommand;
use ninox_core::fleet::service::{
    install_commands, status_command, uninstall_commands, unit_contents, unit_path, ServicePlatform,
    ServiceSpec,
};
use std::path::{Path, PathBuf};

#[derive(Subcommand)]
pub enum ServiceAction {
    /// Start the headless engine at login (launchd agent / systemd user unit)
    Install,
    /// Remove the login autostart
    Uninstall,
    /// Show whether the autostart is installed and running
    Status,
}

pub fn run(action: ServiceAction) -> anyhow::Result<()> {
    let platform = ServicePlatform::current()
        .ok_or_else(|| anyhow::anyhow!("ninox service supports macOS (launchd) and Linux (systemd) only"))?;
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("cannot resolve the home directory"))?;
    let unit = unit_path(platform, &home);
    // The home directory's owner is the invoking user's uid (launchd's
    // `gui/<uid>` domain) — avoids an unsafe getuid binding.
    let uid = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(&home)?.uid()
    };
    match action {
        ServiceAction::Install => {
            let spec = ServiceSpec {
                ninox_bin: std::env::current_exe()?,
                log_path:  log_path(),
                path_env:  std::env::var("PATH").ok(),
            };
            write_unit(&unit, &unit_contents(platform, &spec))?;
            println!("wrote {}", unit.display());
            run_all(&install_commands(platform, &unit, uid), true)?;
            println!("installed — the ninox engine now starts at login");
        }
        ServiceAction::Uninstall => {
            run_all(&uninstall_commands(platform, uid), false)?;
            match std::fs::remove_file(&unit) {
                Ok(()) => println!("removed {}", unit.display()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => println!("not installed"),
                Err(e) => return Err(e.into()),
            }
            if platform == ServicePlatform::Systemd {
                run_all(&[vec!["systemctl".into(), "--user".into(), "daemon-reload".into()]], false)?;
            }
        }
        ServiceAction::Status => {
            let installed = unit.is_file();
            let running = installed && command_succeeds(&status_command(platform, uid));
            println!(
                "{}: {}",
                unit.display(),
                match (installed, running) {
                    (false, _)    => "not installed",
                    (true, true)  => "installed, running",
                    (true, false) => "installed, not running",
                },
            );
        }
    }
    Ok(())
}

fn log_path() -> PathBuf {
    dirs::data_local_dir().unwrap_or_else(|| PathBuf::from(".")).join("ninox").join("daemon.log")
}

fn write_unit(path: &Path, contents: &str) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if let Some(dir) = log_path().parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, contents)?;
    Ok(())
}

/// Runs each command. With `strict`, only the last command's failure is
/// fatal: the leading ones (launchd `bootout` of a not-yet-loaded agent,
/// `daemon-reload`) may fail harmlessly.
fn run_all(cmds: &[Vec<String>], strict: bool) -> anyhow::Result<()> {
    for (i, argv) in cmds.iter().enumerate() {
        let ok = command_succeeds(argv);
        if !ok && strict && i + 1 == cmds.len() {
            anyhow::bail!("`{}` failed", argv.join(" "));
        }
    }
    Ok(())
}

fn command_succeeds(argv: &[String]) -> bool {
    let Some((bin, args)) = argv.split_first() else { return false };
    std::process::Command::new(bin)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}
