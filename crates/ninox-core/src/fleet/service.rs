//! Autostart unit generation (spec §5.4): a launchd agent on macOS, a
//! systemd user unit on Linux, both running the headless engine at login.
//! Pure — `ninox service install` (ninox-app) writes these files and runs
//! the [`install_commands`] / [`uninstall_commands`].

use std::path::{Path, PathBuf};

pub const LAUNCHD_LABEL: &str = "io.ninox.engine";
pub const SYSTEMD_UNIT_NAME: &str = "ninox-engine.service";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServicePlatform {
    Launchd,
    Systemd,
}

impl ServicePlatform {
    pub fn current() -> Option<Self> {
        if cfg!(target_os = "macos") {
            Some(Self::Launchd)
        } else if cfg!(target_os = "linux") {
            Some(Self::Systemd)
        } else {
            None
        }
    }

    /// Same mapping as [`Self::current`], but for a remote host's own
    /// `std::env::consts::OS`-vocabulary string (as `remote::local_platform`/
    /// `remote::detect_remote_platform` report it) rather than this process's
    /// own `cfg!(target_os)`.
    pub fn for_os(os: &str) -> Option<Self> {
        match os {
            "macos" => Some(Self::Launchd),
            "linux" => Some(Self::Systemd),
            _ => None,
        }
    }
}

/// Inputs shared by both unit formats.
#[derive(Debug, Clone)]
pub struct ServiceSpec {
    pub ninox_bin: PathBuf,
    /// Where stdout/stderr go (launchd only; systemd uses the journal).
    pub log_path:  PathBuf,
    /// The installer's `PATH`. Login services start with a minimal PATH
    /// that lacks tmux/git/gh/claude, so it is captured at install time.
    pub path_env:  Option<String>,
}

pub fn unit_path(platform: ServicePlatform, home: &Path) -> PathBuf {
    match platform {
        ServicePlatform::Launchd => home.join("Library/LaunchAgents").join(format!("{LAUNCHD_LABEL}.plist")),
        ServicePlatform::Systemd => home.join(".config/systemd/user").join(SYSTEMD_UNIT_NAME),
    }
}

pub fn unit_contents(platform: ServicePlatform, spec: &ServiceSpec) -> String {
    match platform {
        ServicePlatform::Launchd => launchd_plist(spec),
        ServicePlatform::Systemd => systemd_unit(spec),
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

pub fn launchd_plist(spec: &ServiceSpec) -> String {
    let bin = xml_escape(&spec.ninox_bin.to_string_lossy());
    let log = xml_escape(&spec.log_path.to_string_lossy());
    let env = spec.path_env.as_deref().map(|p| format!(
        "    <key>EnvironmentVariables</key>\n    <dict>\n        <key>PATH</key>\n        <string>{}</string>\n    </dict>\n",
        xml_escape(p),
    )).unwrap_or_default();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LAUNCHD_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{bin}</string>
        <string>--headless</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>AbandonProcessGroup</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ThrottleInterval</key>
    <integer>10</integer>
{env}    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#
    )
}

/// systemd quoting for one `ExecStart=` word.
fn systemd_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

pub fn systemd_unit(spec: &ServiceSpec) -> String {
    let bin = systemd_quote(&spec.ninox_bin.to_string_lossy());
    let env = spec.path_env.as_deref()
        .map(|p| format!("Environment={}\n", systemd_quote(&format!("PATH={p}"))))
        .unwrap_or_default();
    format!(
        "[Unit]\n\
         Description=Ninox engine (headless)\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={bin} --headless\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         KillMode=process\n\
         {env}\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

/// Commands that load an already-written unit. `uid` is the user's
/// numeric id (launchd's `gui/<uid>` domain).
pub fn install_commands(platform: ServicePlatform, unit: &Path, uid: u32) -> Vec<Vec<String>> {
    let unit = unit.to_string_lossy().to_string();
    match platform {
        ServicePlatform::Launchd => vec![
            // Re-installs replace a loaded agent; a not-loaded bootout fails
            // harmlessly and callers ignore its status.
            vec!["launchctl".into(), "bootout".into(), format!("gui/{uid}/{LAUNCHD_LABEL}")],
            vec!["launchctl".into(), "bootstrap".into(), format!("gui/{uid}"), unit],
        ],
        ServicePlatform::Systemd => vec![
            vec!["systemctl".into(), "--user".into(), "daemon-reload".into()],
            vec!["systemctl".into(), "--user".into(), "enable".into(), "--now".into(), SYSTEMD_UNIT_NAME.into()],
        ],
    }
}

/// Commands run before the unit file is removed.
pub fn uninstall_commands(platform: ServicePlatform, uid: u32) -> Vec<Vec<String>> {
    match platform {
        ServicePlatform::Launchd => vec![
            vec!["launchctl".into(), "bootout".into(), format!("gui/{uid}/{LAUNCHD_LABEL}")],
        ],
        ServicePlatform::Systemd => vec![
            vec!["systemctl".into(), "--user".into(), "disable".into(), "--now".into(), SYSTEMD_UNIT_NAME.into()],
        ],
    }
}

/// Commands that restart an already-running headless engine in place, so it
/// picks up a newly installed binary — unlike [`install_commands`]'s
/// systemd path (`enable --now` is a no-op on an already-active unit; see
/// its own call site for why this needs a dedicated command instead of
/// reusing that one).
pub fn restart_commands(platform: ServicePlatform, uid: u32) -> Vec<Vec<String>> {
    match platform {
        ServicePlatform::Launchd => {
            vec![vec!["launchctl".into(), "kickstart".into(), "-k".into(), format!("gui/{uid}/{LAUNCHD_LABEL}")]]
        }
        ServicePlatform::Systemd => {
            vec![vec!["systemctl".into(), "--user".into(), "restart".into(), SYSTEMD_UNIT_NAME.into()]]
        }
    }
}

/// Command whose success means the service is loaded/active.
pub fn status_command(platform: ServicePlatform, uid: u32) -> Vec<String> {
    match platform {
        ServicePlatform::Launchd => vec!["launchctl".into(), "print".into(), format!("gui/{uid}/{LAUNCHD_LABEL}")],
        ServicePlatform::Systemd => vec!["systemctl".into(), "--user".into(), "is-active".into(), SYSTEMD_UNIT_NAME.into()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ServiceSpec {
        ServiceSpec {
            ninox_bin: PathBuf::from("/Users/me/My Apps/ninox"),
            log_path:  PathBuf::from("/Users/me/Library/Application Support/ninox/daemon.log"),
            path_env:  Some("/opt/homebrew/bin:/usr/bin:/bin".into()),
        }
    }

    #[test]
    fn plist_runs_headless_at_load_and_restarts_on_crash() {
        let p = launchd_plist(&spec());
        assert!(p.contains("<string>io.ninox.engine</string>"));
        assert!(p.contains("<string>/Users/me/My Apps/ninox</string>\n        <string>--headless</string>"));
        assert!(p.contains("<key>RunAtLoad</key>\n    <true/>"));
        assert!(p.contains("<key>SuccessfulExit</key>\n        <false/>"), "KeepAlive only on crash");
        assert!(p.contains("<string>/opt/homebrew/bin:/usr/bin:/bin</string>"));
        assert!(p.contains("ninox/daemon.log</string>"));
        assert!(p.starts_with("<?xml"));
    }

    /// Regression for a real finding (code review on this PR, after the
    /// analogous `systemd_unit_restarts_only_the_engine_process_not_its_whole_cgroup`
    /// fix): launchd's default when it stops a job is to signal its whole
    /// *process group*, not just the one tracked process — the exact same
    /// class of bug as the systemd cgroup case, just keyed by process group
    /// instead of cgroup. `AbandonProcessGroup` is launchd's equivalent of
    /// `KillMode=process`.
    #[test]
    fn plist_abandons_the_process_group_on_stop() {
        let p = launchd_plist(&spec());
        assert!(p.contains("<key>AbandonProcessGroup</key>\n    <true/>"), "{p}");
    }

    #[test]
    fn plist_escapes_xml_and_omits_env_without_path() {
        let mut s = spec();
        s.ninox_bin = PathBuf::from("/a&b/<ninox>");
        s.path_env = None;
        let p = launchd_plist(&s);
        assert!(p.contains("<string>/a&amp;b/&lt;ninox&gt;</string>"));
        assert!(!p.contains("EnvironmentVariables"));
    }

    #[test]
    fn systemd_unit_quotes_exec_and_restarts_on_failure() {
        let u = systemd_unit(&spec());
        assert!(u.contains("ExecStart=\"/Users/me/My Apps/ninox\" --headless\n"));
        assert!(u.contains("Restart=on-failure\n"));
        assert!(u.contains("Environment=\"PATH=/opt/homebrew/bin:/usr/bin:/bin\"\n"));
        assert!(u.contains("WantedBy=default.target\n"));
    }

    /// Regression for a real finding: without `KillMode=process` (the
    /// systemd default is `control-group`), restarting this unit sends
    /// SIGTERM to every process in its cgroup — including any `ninox-ptyd`
    /// host (and every pane it holds) that this engine spawned on demand,
    /// since a detached/double-forked child still inherits its parent's
    /// cgroup. That would kill every live pane a ptyd live-upgrade just
    /// preserved, the exact failure this whole mechanism exists to prevent.
    #[test]
    fn systemd_unit_restarts_only_the_engine_process_not_its_whole_cgroup() {
        let u = systemd_unit(&spec());
        assert!(u.contains("KillMode=process\n"), "{u}");
    }

    #[test]
    fn for_os_maps_macos_and_linux_only() {
        assert_eq!(ServicePlatform::for_os("macos"), Some(ServicePlatform::Launchd));
        assert_eq!(ServicePlatform::for_os("linux"), Some(ServicePlatform::Systemd));
        assert_eq!(ServicePlatform::for_os("windows"), None);
    }

    #[test]
    fn restart_commands_kickstart_on_launchd() {
        let cmds = restart_commands(ServicePlatform::Launchd, 501);
        assert_eq!(cmds, vec![vec![
            "launchctl".to_string(), "kickstart".to_string(), "-k".to_string(), "gui/501/io.ninox.engine".to_string(),
        ]]);
    }

    #[test]
    fn restart_commands_restarts_the_unit_on_systemd() {
        let cmds = restart_commands(ServicePlatform::Systemd, 501);
        assert_eq!(cmds, vec![vec![
            "systemctl".to_string(), "--user".to_string(), "restart".to_string(), "ninox-engine.service".to_string(),
        ]]);
    }

    #[test]
    fn unit_paths() {
        let home = Path::new("/home/me");
        assert_eq!(
            unit_path(ServicePlatform::Launchd, home),
            PathBuf::from("/home/me/Library/LaunchAgents/io.ninox.engine.plist"),
        );
        assert_eq!(
            unit_path(ServicePlatform::Systemd, home),
            PathBuf::from("/home/me/.config/systemd/user/ninox-engine.service"),
        );
    }

    #[test]
    fn command_plans() {
        let unit = Path::new("/u.plist");
        let l = install_commands(ServicePlatform::Launchd, unit, 501);
        assert_eq!(l[1], ["launchctl", "bootstrap", "gui/501", "/u.plist"]);
        let s = install_commands(ServicePlatform::Systemd, unit, 0);
        assert_eq!(s[1], ["systemctl", "--user", "enable", "--now", "ninox-engine.service"]);
        assert_eq!(uninstall_commands(ServicePlatform::Launchd, 501)[0][2], "gui/501/io.ninox.engine");
        assert_eq!(status_command(ServicePlatform::Systemd, 0)[2], "is-active");
    }
}
