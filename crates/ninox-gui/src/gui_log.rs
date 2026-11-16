//! Tracing output goes to stderr always, and to `gui.log` next to the store
//! once the GUI proper starts (`daemon.log` for `--headless`), so an incident
//! in a long-running app leaves evidence even when it was launched from a
//! terminal nobody kept.

use std::io::{self, Write};
use std::path::Path;
use std::sync::OnceLock;

/// Whether stderr is already the log file (a daemon spawned by
/// `spawn_headless`), in which case it isn't written a second time.
static STDERR_IS_LOG: OnceLock<bool> = OnceLock::new();

/// Start mirroring tracing output into `path`, size-capped and written off
/// the calling thread. A file that can't be opened leaves logging on stderr
/// only.
pub fn enable_file(path: &Path) {
    ninox_core::daemon_log::init_file(path);
    let _ = STDERR_IS_LOG.set(ninox_core::daemon_log::stderr_is(path));
}

pub fn init() {
    let filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(tracing::level_filters::LevelFilter::INFO.into())
        .from_env_lossy();
    let _ = tracing_subscriber::fmt().with_env_filter(filter).with_writer(Tee).try_init();
}

#[derive(Clone, Copy)]
struct Tee;

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Tee {
    type Writer = TeeWriter;
    fn make_writer(&'a self) -> TeeWriter {
        TeeWriter
    }
}

struct TeeWriter;

impl Write for TeeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if !STDERR_IS_LOG.get().copied().unwrap_or(false) {
            let _ = io::stderr().write_all(buf);
        }
        ninox_core::daemon_log::write(strip_ansi(buf).as_deref().unwrap_or(buf));
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stderr().flush()
    }
}

/// `fmt` colours its output when it thinks it is on a terminal; the file
/// should stay greppable.
fn strip_ansi(buf: &[u8]) -> Option<Vec<u8>> {
    if !buf.contains(&0x1b) {
        return None;
    }
    let mut out = Vec::with_capacity(buf.len());
    let mut i = 0;
    while i < buf.len() {
        if buf[i] == 0x1b && buf.get(i + 1) == Some(&b'[') {
            i += 2;
            while i < buf.len() && !(0x40..=0x7e).contains(&buf[i]) {
                i += 1;
            }
            i += 1;
        } else {
            out.push(buf[i]);
            i += 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_removes_colour_codes_only() {
        assert_eq!(strip_ansi(b"\x1b[2mhi\x1b[0m there").unwrap(), b"hi there");
        assert!(strip_ansi(b"plain").is_none());
    }
}
