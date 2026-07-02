/// Returns the current errno value in a portable way.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn errno() -> i32 {
    unsafe {
        #[cfg(target_os = "macos")]
        { *libc::__error() }
        #[cfg(target_os = "linux")]
        { *libc::__errno_location() }
    }
}

/// Returns `true` if the process with the given PID is alive.
/// PID 0 always returns `false` (guard against sending signals to process groups).
pub fn is_pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    unsafe {
        let r = libc::kill(pid as libc::pid_t, 0);
        // r == 0  → process exists and we have permission to signal it
        // r == -1 && EPERM → process exists but we lack permission (still alive)
        // r == -1 && ESRCH → no such process
        r == 0 || (r == -1 && errno() == libc::EPERM)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dead_pid_returns_false() {
        assert!(!is_pid_alive(0));
    }

    #[test]
    fn own_pid_is_alive() {
        assert!(is_pid_alive(std::process::id()));
    }
}
