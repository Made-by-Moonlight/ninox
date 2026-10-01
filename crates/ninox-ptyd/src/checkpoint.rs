//! On-disk screen checkpoints, so a pane's last screen can be shown after a
//! reboot until its resumed process repaints.
//!
//! Layout: `<dir>/<pane>.json` holding a `Checkpoint`. The host rewrites a
//! pane's file at most every few seconds while its screen changes, and once
//! more when the process exits. Writes are atomic (tmp + rename).
//!
//! The host never deletes checkpoints (not even on `Kill`): whether a dead
//! pane's last screen is still useful is the engine's call, via [`remove`].
//! Pane ids are mapped to file names by replacing anything outside
//! `[A-Za-z0-9._-]` with `_`.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::protocol::ScreenSnapshot;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Checkpoint {
    pub pane: String,
    pub saved_ms: u64,
    pub screen: ScreenSnapshot,
}

/// `<data_local_dir>/ninox/checkpoints`, or `<dir of $NINOX_PTYD_SOCKET>/checkpoints`
/// when the socket is overridden (keeps test fleets isolated).
pub fn default_dir() -> PathBuf {
    if let Some(sock) = std::env::var_os(crate::SOCKET_ENV) {
        let sock = PathBuf::from(sock);
        let parent = sock.parent().map(Path::to_path_buf).unwrap_or_else(std::env::temp_dir);
        return parent.join("checkpoints");
    }
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("ninox")
        .join("checkpoints")
}

/// File holding `pane`'s checkpoint inside `dir`.
pub fn path_for(dir: &Path, pane: &str) -> PathBuf {
    let mut name: String = pane
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' })
        .collect();
    if name.is_empty() || name.chars().all(|c| c == '.') {
        name = format!("_{name}");
    }
    dir.join(format!("{name}.json"))
}

pub fn load(dir: &Path, pane: &str) -> Option<Checkpoint> {
    let bytes = std::fs::read(path_for(dir, pane)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn remove(dir: &Path, pane: &str) {
    let _ = std::fs::remove_file(path_for(dir, pane));
}

/// Atomically (tmp file + fsync + rename) write `ck` to its pane's file.
pub fn write(dir: &Path, ck: &Checkpoint) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = path_for(dir, &ck.pane);
    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    let bytes = serde_json::to_vec(ck).map_err(std::io::Error::other)?;
    let result = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, &path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{AlacrittyEngine, TerminalEngine};

    #[test]
    fn write_load_remove_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = AlacrittyEngine::new(20, 3, 100);
        e.feed(b"hello\r\nworld");
        let ck = Checkpoint { pane: "a/b".into(), saved_ms: 7, screen: e.snapshot(0) };
        write(dir.path(), &ck).unwrap();
        assert!(dir.path().join("a_b.json").exists());
        assert_eq!(load(dir.path(), "a/b"), Some(ck));
        remove(dir.path(), "a/b");
        assert_eq!(load(dir.path(), "a/b"), None);
    }

    #[test]
    fn hostile_ids_stay_inside_dir() {
        let d = Path::new("/x");
        assert_eq!(path_for(d, ".."), Path::new("/x/_...json"));
        assert_eq!(path_for(d, "../../etc/passwd"), Path::new("/x/.._.._etc_passwd.json"));
    }
}
