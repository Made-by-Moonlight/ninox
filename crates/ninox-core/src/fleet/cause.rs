//! Why a session died, as far as the engine can tell at reconciliation.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InterruptCause {
    /// The machine booted after the session started, so it cannot have
    /// survived.
    Reboot,
    /// Same boot, but the process hosting the pane (tmux server / ptyd) or
    /// the pane itself went away.
    HostExited,
    Unknown,
}

impl InterruptCause {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reboot     => "reboot",
            Self::HostExited => "host_exited",
            Self::Unknown    => "unknown",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "reboot"      => Self::Reboot,
            "host_exited" => Self::HostExited,
            _             => Self::Unknown,
        }
    }

    /// Phrase used in briefings ("… (machine reboot)").
    pub fn describe(self) -> &'static str {
        match self {
            Self::Reboot     => "machine reboot",
            Self::HostExited => "the terminal host exited",
            Self::Unknown    => "cause unknown",
        }
    }
}

/// Pure classification. `boot_ms` is `None` when the boot time can't be
/// read, in which case the cause is honestly `Unknown`.
pub fn classify(session_started_at_ms: i64, boot_ms: Option<i64>) -> InterruptCause {
    match boot_ms {
        Some(boot) if boot > session_started_at_ms => InterruptCause::Reboot,
        Some(_) => InterruptCause::HostExited,
        None => InterruptCause::Unknown,
    }
}

/// Best estimate of when the session died: for a reboot the boot time (the
/// latest moment it can have been alive), otherwise the detection time.
pub fn interrupted_at(cause: InterruptCause, boot_ms: Option<i64>, detected_at_ms: i64) -> i64 {
    match (cause, boot_ms) {
        (InterruptCause::Reboot, Some(boot)) => boot.min(detected_at_ms),
        _ => detected_at_ms,
    }
}

/// System boot time in Unix epoch milliseconds.
pub fn boot_time_ms() -> Option<i64> {
    #[cfg(target_os = "macos")]
    {
        // `kern.boottime` prints `{ sec = 1727770000, usec = 123 } ...`.
        let out = std::process::Command::new("sysctl").args(["-n", "kern.boottime"]).output().ok()?;
        parse_macos_boottime(&String::from_utf8_lossy(&out.stdout))
    }
    #[cfg(target_os = "linux")]
    {
        parse_linux_btime(&std::fs::read_to_string("/proc/stat").ok()?)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_macos_boottime(s: &str) -> Option<i64> {
    let after = s.split("sec =").nth(1)?;
    let secs: i64 = after.trim_start().split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()?;
    Some(secs * 1000)
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_linux_btime(stat: &str) -> Option<i64> {
    let line = stat.lines().find(|l| l.starts_with("btime "))?;
    let secs: i64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(secs * 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reboot_when_boot_is_after_start() {
        assert_eq!(classify(1_000, Some(2_000)), InterruptCause::Reboot);
        assert_eq!(classify(3_000, Some(2_000)), InterruptCause::HostExited);
        assert_eq!(classify(3_000, None), InterruptCause::Unknown);
    }

    #[test]
    fn interrupted_at_uses_boot_time_for_reboots_only() {
        assert_eq!(interrupted_at(InterruptCause::Reboot, Some(2_000), 9_000), 2_000);
        assert_eq!(interrupted_at(InterruptCause::HostExited, Some(2_000), 9_000), 9_000);
        assert_eq!(interrupted_at(InterruptCause::Unknown, None, 9_000), 9_000);
    }

    #[test]
    fn round_trips_through_str() {
        for c in [InterruptCause::Reboot, InterruptCause::HostExited, InterruptCause::Unknown] {
            assert_eq!(InterruptCause::parse(c.as_str()), c);
        }
    }

    #[test]
    fn parses_platform_boot_time_formats() {
        assert_eq!(
            parse_macos_boottime("{ sec = 1727770000, usec = 123456 } Tue Oct  1 08:06:40 2024\n"),
            Some(1_727_770_000_000),
        );
        assert_eq!(parse_linux_btime("cpu 1 2 3\nbtime 1727770000\nprocesses 5\n"), Some(1_727_770_000_000));
        assert_eq!(parse_linux_btime("cpu 1 2 3\n"), None);
    }
}
