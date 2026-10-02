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
