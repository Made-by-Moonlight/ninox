//! Orchestrators started in a folder the user chose (`--workspace`).
//!
//! A row maps a folder to the session running in it, so every relaunch
//! re-seeds into the right place. The folder stays on record after the
//! orchestrator is removed (its session id detached, so a later orchestrator
//! reusing the id is not mistaken for a custom one): the orphan sweep must keep
//! treating the folder as someone else's.

use super::Store;
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

pub(super) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS custom_workspaces (
            path       TEXT PRIMARY KEY,
            session_id TEXT
        );
        CREATE INDEX IF NOT EXISTS custom_workspaces_session ON custom_workspaces(session_id);",
    )?;
    Ok(())
}

impl Store {
    pub fn set_custom_workspace(&self, session_id: &str, path: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM custom_workspaces WHERE session_id = ?1 AND path <> ?2", params![session_id, path])?;
        conn.execute(
            "INSERT INTO custom_workspaces(path, session_id) VALUES(?1, ?2)
             ON CONFLICT(path) DO UPDATE SET session_id = excluded.session_id",
            params![path, session_id],
        )?;
        Ok(())
    }

    /// The orchestrator is gone: keep the folder on record, drop the link.
    pub fn detach_custom_workspace(&self, session_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("UPDATE custom_workspaces SET session_id = NULL WHERE session_id = ?1", [session_id])?;
        Ok(())
    }

    /// A launch that failed before anything ran: the folder was never ours.
    pub fn forget_custom_workspace(&self, session_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM custom_workspaces WHERE session_id = ?1", [session_id])?;
        Ok(())
    }

    pub fn custom_workspace(&self, session_id: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row("SELECT path FROM custom_workspaces WHERE session_id = ?1", [session_id], |r| r.get(0))
            .optional()?)
    }

    /// Every folder ever used as a custom workspace, including those of
    /// orchestrators since removed.
    pub fn custom_workspace_paths(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT path FROM custom_workspaces")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// `(session id, path)` for every custom-workspace orchestrator that
    /// still has a session row.
    pub fn live_custom_workspaces(&self) -> Result<Vec<(String, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT c.session_id, c.path FROM custom_workspaces c
             JOIN orchestrators o ON o.id = c.session_id",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_survive_orchestrator_deletion() {
        let store = Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap();
        store.upsert_orchestrator(&crate::types::Orchestrator { id: "o".into(), name: "o".into(), created_at: 0 }).unwrap();
        store.set_custom_workspace("o", "/repo").unwrap();
        assert_eq!(store.custom_workspace("o").unwrap().as_deref(), Some("/repo"));
        assert_eq!(store.live_custom_workspaces().unwrap(), vec![("o".to_string(), "/repo".to_string())]);
        store.detach_custom_workspace("o").unwrap();
        store.delete_orchestrator("o").unwrap();
        assert!(store.live_custom_workspaces().unwrap().is_empty());
        assert_eq!(store.custom_workspace("o").unwrap(), None, "a later orchestrator reusing the id is not custom");
        assert_eq!(store.custom_workspace_paths().unwrap(), vec!["/repo".to_string()]);
        assert_eq!(store.custom_workspace("plain").unwrap(), None);
    }
}
