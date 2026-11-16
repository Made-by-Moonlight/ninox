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

/// One running process, as [`ProcessListing`] reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Proc {
    pub pid:  u32,
    pub ppid: u32,
    pub argv: Vec<String>,
}

/// One read of the process table, for judging many sessions at once with
/// [`ProcessListing::harness_alive`]. Read in-process rather than through
/// `ps`, which is setuid on macOS and so can't run under a sandbox.
pub struct ProcessListing(Option<Vec<Proc>>);

impl ProcessListing {
    pub fn read() -> Self {
        Self(read_procs())
    }

    /// Whether a session's harness process is still running even though no
    /// runtime host knows its pane — e.g. it lives in another ninox's ptyd
    /// host or tmux server. Resuming such a session would run a second copy
    /// of the same conversation, so this errs towards "alive": if the
    /// process table couldn't be read, a live `pid` is trusted as is.
    pub fn harness_alive(&self, pid: Option<u32>, claude_session_id: Option<&str>, harness_binary: &str) -> bool {
        match &self.0 {
            Some(procs) => harness_alive_in(procs, pid, claude_session_id, harness_binary),
            None => pid.is_some_and(is_pid_alive),
        }
    }
}

/// A session with a conversation id is alive only while some process names
/// that id — its pid may since have been reused by another pane, whose
/// harness children would otherwise match. Without one, `pid` (or one of
/// its children, for a pid recorded as the pane's shell) must be running
/// the harness binary; a bare live `pid` isn't enough.
pub(crate) fn harness_alive_in(procs: &[Proc], pid: Option<u32>, claude_session_id: Option<&str>, harness_binary: &str) -> bool {
    if let Some(id) = claude_session_id.filter(|id| id.len() >= 8) {
        return procs.iter().any(|p| p.argv.iter().any(|a| a == id));
    }
    let Some(pid) = pid.filter(|p| *p != 0) else { return false };
    let binary = basename(harness_binary);
    procs.iter()
        .filter(|p| p.pid == pid || p.ppid == pid)
        .any(|p| p.argv.first().is_some_and(|argv0| basename(argv0) == binary))
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

#[cfg(target_os = "macos")]
fn read_procs() -> Option<Vec<Proc>> {
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count <= 0 {
        return None;
    }
    let mut pids = vec![0 as libc::pid_t; count as usize + 64];
    let bytes = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
    let got = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    if got <= 0 {
        return None;
    }
    pids.truncate(got as usize);
    let argmax = macos_argmax()?;
    Some(
        pids.into_iter()
            .filter(|&pid| pid > 0)
            .filter_map(|pid| {
                let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
                let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
                let n = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&mut info as *mut libc::proc_bsdinfo).cast(), size) };
                (n == size).then(|| Proc { pid: pid as u32, ppid: info.pbi_ppid, argv: macos_argv(pid, argmax).unwrap_or_default() })
            })
            .collect(),
    )
}

/// `KERN_PROCARGS2` lays out `argc`, the exec path, NUL padding, then the
/// argv strings. Only readable for the caller's own processes, which is
/// every harness ninox launches.
#[cfg(target_os = "macos")]
fn macos_argmax() -> Option<usize> {
    let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
    let mut argmax: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    let ok = unsafe { libc::sysctl(mib.as_mut_ptr(), 2, (&mut argmax as *mut libc::c_int).cast(), &mut size, std::ptr::null_mut(), 0) } == 0;
    (ok && argmax > 0).then_some(argmax as usize)
}

#[cfg(target_os = "macos")]
fn macos_argv(pid: libc::pid_t, argmax: usize) -> Option<Vec<String>> {
    let mut buf = vec![0u8; argmax];
    let mut size = buf.len();
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    if unsafe { libc::sysctl(mib.as_mut_ptr(), 3, buf.as_mut_ptr().cast(), &mut size, std::ptr::null_mut(), 0) } != 0 {
        return None;
    }
    buf.truncate(size);
    let argc = i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?) as usize;
    let rest = &buf[4..];
    let after_exec = rest.iter().position(|&b| b == 0)?;
    let args = &rest[after_exec..];
    let args = &args[args.iter().position(|&b| b != 0)?..];
    Some(args.split(|&b| b == 0).take(argc).map(|a| String::from_utf8_lossy(a).into_owned()).collect())
}

#[cfg(target_os = "linux")]
fn read_procs() -> Option<Vec<Proc>> {
    let entries = std::fs::read_dir("/proc").ok()?;
    Some(
        entries
            .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u32>().ok())
            .filter_map(|pid| {
                let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
                let ppid = stat.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()?;
                let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
                let argv = cmdline.split(|&b| b == 0).filter(|a| !a.is_empty()).map(|a| String::from_utf8_lossy(a).into_owned()).collect();
                Some(Proc { pid, ppid, argv })
            })
            .collect(),
    )
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn read_procs() -> Option<Vec<Proc>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn procs() -> Vec<Proc> {
        [
            (1, 0, "/sbin/launchd"),
            (100, 1, "/Users/u/.cargo/bin/ninox ptyd"),
            (200, 100, "claude --dangerously-skip-permissions --session-id 6d4ccf43-4997-4148-bd9e-5232f1a3a4e4 --model m"),
            (300, 100, "/bin/zsh -l"),
            (301, 300, "/opt/homebrew/bin/claude --resume f67322c4-c3f1-4dcd-aab6-73cadee9895e"),
            (400, 1, "/usr/bin/vim notes.txt"),
        ]
        .into_iter()
        .map(|(pid, ppid, cmd)| Proc { pid, ppid, argv: cmd.split(' ').map(String::from).collect() })
        .collect()
    }

    #[test]
    fn a_process_naming_the_conversation_is_alive_whatever_its_pid() {
        assert!(harness_alive_in(&procs(), None, Some("6d4ccf43-4997-4148-bd9e-5232f1a3a4e4"), "claude"));
        assert!(harness_alive_in(&procs(), Some(999), Some("f67322c4-c3f1-4dcd-aab6-73cadee9895e"), "claude"));
    }

    #[test]
    fn a_recorded_pid_counts_only_while_it_or_its_child_runs_the_harness() {
        assert!(harness_alive_in(&procs(), Some(200), None, "claude"));
        assert!(harness_alive_in(&procs(), Some(300), None, "/usr/local/bin/claude"), "the pane shell's claude child");
        assert!(!harness_alive_in(&procs(), Some(400), None, "claude"), "a reused pid running something else");
        assert!(!harness_alive_in(&procs(), Some(999), Some("00000000-gone"), "claude"));
        assert!(
            !harness_alive_in(&procs(), Some(200), Some("00000000-reused-pid"), "claude"),
            "a pid reused by another conversation's harness",
        );
        assert!(!harness_alive_in(&procs(), Some(100), Some("00000000-reused-pid"), "claude"), "nor by a host with harness children");
        assert!(!harness_alive_in(&procs(), None, None, "claude"));
    }

    #[test]
    fn the_process_table_shows_this_process_with_its_parent_and_argv() {
        let procs = read_procs().expect("process table readable");
        let me = procs.iter().find(|p| p.pid == std::process::id()).expect("own pid listed");
        assert_eq!(me.ppid, std::os::unix::process::parent_id());
        assert_eq!(me.argv, std::env::args().collect::<Vec<_>>());
    }

    #[test]
    fn a_real_process_carrying_the_conversation_id_is_seen() {
        let id = format!("probe-{}-conversation", std::process::id());
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 30; :", &id])
            .spawn()
            .unwrap();
        let alive = ProcessListing::read().harness_alive(None, Some(&id), "claude");
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(alive);
        assert!(!ProcessListing::read().harness_alive(None, Some(&id), "claude"));
    }

    #[test]
    fn dead_pid_returns_false() {
        assert!(!is_pid_alive(0));
    }

    #[test]
    fn own_pid_is_alive() {
        assert!(is_pid_alive(std::process::id()));
    }
}
