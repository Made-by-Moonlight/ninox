//! Logging for this binary: stdout for CLI subcommands, as before; a file
//! once the TUI owns the terminal, where stdout would draw over its screen.

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use tracing::Level;
use tracing_subscriber::{filter, fmt::MakeWriter, prelude::*, EnvFilter};

static FILE: OnceLock<Mutex<File>> = OnceLock::new();
/// Set when this process is the headless daemon: info-level lines go to
/// `daemon.log` (and stderr, unless that already is the file).
static DAEMON_STDERR_TEE: OnceLock<bool> = OnceLock::new();

/// `RUST_LOG` when set. Otherwise errors only on stdout (what
/// `fmt::init()` gave), widened to warnings once logging goes to the file,
/// where they no longer cost the user anything, and to info for the
/// headless daemon, whose log is the only record of why it stopped.
pub fn init() {
    let layer = tracing_subscriber::fmt::layer().with_writer(Sink).with_ansi(false);
    if std::env::var_os("RUST_LOG").is_some() {
        tracing_subscriber::registry().with(layer.with_filter(EnvFilter::from_default_env())).init();
    } else {
        // Dynamic, not `filter_fn`: that caches each callsite's first answer,
        // so a warning logged before the TUI started would stay off for good.
        let level = filter::dynamic_filter_fn(|meta, _| {
            *meta.level()
                <= if DAEMON_STDERR_TEE.get().is_some() {
                    Level::INFO
                } else if FILE.get().is_some() {
                    Level::WARN
                } else {
                    Level::ERROR
                }
        })
        .with_max_level_hint(filter::LevelFilter::INFO);
        tracing_subscriber::registry().with(layer.with_filter(level)).init();
    }
}

/// Sends every later log line to `path` (appended) instead of stdout.
pub fn to_file(path: &Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    let _ = FILE.set(Mutex::new(file));
    Ok(())
}

/// Sends every later log line to `path` (size-capped, written off-thread)
/// and to stderr, for the headless daemon. A `daemon.log` that is already
/// this process's stderr (see `spawn_headless`) isn't written twice.
pub fn to_daemon_file(path: &Path) {
    ninox_core::daemon_log::init_file(path);
    let _ = DAEMON_STDERR_TEE.set(!ninox_core::daemon_log::stderr_is(path));
}

struct Sink;

enum Out {
    Stdout(std::io::Stdout),
    File(&'static Mutex<File>),
    Daemon { tee_stderr: bool },
}

impl Write for Out {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Out::Stdout(out) => out.write(buf),
            Out::File(file) => file.lock().unwrap_or_else(|e| e.into_inner()).write(buf),
            Out::Daemon { tee_stderr } => {
                ninox_core::daemon_log::write(buf);
                if *tee_stderr {
                    let _ = std::io::stderr().write_all(buf);
                }
                Ok(buf.len())
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Out::Stdout(out) => out.flush(),
            Out::File(file) => file.lock().unwrap_or_else(|e| e.into_inner()).flush(),
            Out::Daemon { .. } => Ok(()),
        }
    }
}

impl<'a> MakeWriter<'a> for Sink {
    type Writer = Out;

    fn make_writer(&'a self) -> Out {
        match (DAEMON_STDERR_TEE.get(), FILE.get()) {
            (Some(tee_stderr), _) => Out::Daemon { tee_stderr: *tee_stderr },
            (None, Some(file)) => Out::File(file),
            (None, None) => Out::Stdout(std::io::stdout()),
        }
    }
}
