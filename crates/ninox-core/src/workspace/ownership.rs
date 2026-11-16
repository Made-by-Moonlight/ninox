use std::path::Path;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

/// Written into every ninox-created worktree at spawn time ([`write_marker`]
/// called from `create_worktree_at`), git-excluded so it never lands on the
/// branch. The orphan sweeper's authoritative "ninox created this" signal —
/// it survives `sweep_retired_sessions` purging the session row, since it
/// lives in the worktree itself, not the store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceMarker {
    pub session_id: String,
    pub repo: String,
    pub branch: String,
    pub created_at: i64,
}

pub const MARKER_FILENAME: &str = ".ninox-workspace.json";

pub fn write_marker(worktree_path: &Path, marker: &WorkspaceMarker) -> anyhow::Result<()> {
    let json = serde_json::to_string_pretty(marker).context("serialize workspace marker")?;
    std::fs::write(worktree_path.join(MARKER_FILENAME), json).context("write workspace marker")
}

/// `None` for a missing or unparseable marker — never panics, since a
/// corrupt marker must fall back to the conservative legacy-heuristic path,
/// not crash the sweeper.
pub fn read_marker(worktree_path: &Path) -> Option<WorkspaceMarker> {
    let data = std::fs::read_to_string(worktree_path.join(MARKER_FILENAME)).ok()?;
    serde_json::from_str(&data).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn marker_round_trips() {
        let dir = tempdir().unwrap();
        let marker = WorkspaceMarker {
            session_id: "s1".into(),
            repo: "/r".into(),
            branch: "s1".into(),
            created_at: 123,
        };
        write_marker(dir.path(), &marker).unwrap();

        let read = read_marker(dir.path()).unwrap();
        assert_eq!(read.session_id, marker.session_id);
        assert_eq!(read.repo, marker.repo);
        assert_eq!(read.branch, marker.branch);
        assert_eq!(read.created_at, marker.created_at);
    }

    #[test]
    fn missing_marker_reads_as_none() {
        let dir = tempdir().unwrap();
        assert!(read_marker(dir.path()).is_none());
    }

    #[test]
    fn corrupt_marker_reads_as_none() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join(MARKER_FILENAME), "not json").unwrap();
        assert!(read_marker(dir.path()).is_none());
    }
}
