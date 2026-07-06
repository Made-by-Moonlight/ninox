use crate::types::*;
use anyhow::Result;
use rusqlite::{params, Connection};
use std::{path::Path, sync::Mutex};

pub struct Store {
    conn: Mutex<Connection>,
}


impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch("
            PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY, orchestrator_id TEXT,
                name TEXT NOT NULL, repo TEXT NOT NULL,
                status TEXT NOT NULL, agent_type TEXT NOT NULL,
                cost_usd REAL NOT NULL DEFAULT 0, started_at INTEGER NOT NULL,
                pr_number INTEGER, pr_id INTEGER,
                workspace_path TEXT, pid INTEGER,
                model TEXT, context_tokens INTEGER
            );
            CREATE TABLE IF NOT EXISTS orchestrators (
                id TEXT PRIMARY KEY, name TEXT NOT NULL, created_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS prs (
                id INTEGER PRIMARY KEY, number INTEGER NOT NULL,
                title TEXT NOT NULL, url TEXT NOT NULL,
                body TEXT NOT NULL, session_id TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS ci_status (
                pr_id INTEGER PRIMARY KEY, total INTEGER NOT NULL,
                passing INTEGER NOT NULL, failing INTEGER NOT NULL,
                pending INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS review_comments (
                id INTEGER PRIMARY KEY, pr_id INTEGER NOT NULL,
                author TEXT NOT NULL, body TEXT NOT NULL,
                path TEXT, line INTEGER, created_at INTEGER NOT NULL
            );
        ")?;
        // Migrations for columns added after initial release — idempotent so
        // both fresh and pre-existing databases end up with the same schema.
        for (col, ddl) in [
            ("model",          "ALTER TABLE sessions ADD COLUMN model TEXT"),
            ("context_tokens", "ALTER TABLE sessions ADD COLUMN context_tokens INTEGER"),
            ("catalogue_path", "ALTER TABLE sessions ADD COLUMN catalogue_path TEXT"),
            ("context_used_pct",     "ALTER TABLE sessions ADD COLUMN context_used_pct REAL"),
            ("context_total_tokens", "ALTER TABLE sessions ADD COLUMN context_total_tokens INTEGER"),
            ("context_window_size",  "ALTER TABLE sessions ADD COLUMN context_window_size INTEGER"),
        ] {
            if !Self::column_exists(&conn, "sessions", col)? {
                conn.execute(ddl, [])?;
            }
        }
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let exists = stmt
            .query_map([], |r| r.get::<_, String>(1))?
            .filter_map(|r| r.ok())
            .any(|c| c == column);
        Ok(exists)
    }

    pub fn upsert_session(&self, s: &Session) -> Result<()> {
        let status = serde_json::to_string(&s.status)?.replace('"', "");
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO sessions (id,orchestrator_id,name,repo,status,agent_type,
             cost_usd,started_at,pr_number,pr_id,workspace_path,pid,model,context_tokens,
             catalogue_path,context_used_pct,context_total_tokens,context_window_size)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)
             ON CONFLICT(id) DO UPDATE SET
             status=excluded.status,cost_usd=excluded.cost_usd,
             started_at=excluded.started_at,
             pr_number=excluded.pr_number,pr_id=excluded.pr_id,
             workspace_path=excluded.workspace_path,pid=excluded.pid,
             model=excluded.model,context_tokens=excluded.context_tokens,
             catalogue_path=excluded.catalogue_path,
             context_used_pct=excluded.context_used_pct,
             context_total_tokens=excluded.context_total_tokens,
             context_window_size=excluded.context_window_size",
            params![
                s.id, s.orchestrator_id, s.name, s.repo, status, s.agent_type,
                s.cost_usd, s.started_at, s.pr_number, s.pr_id,
                s.workspace_path, s.pid, s.model, s.context_tokens,
                s.catalogue_path, s.context_used_pct, s.context_total_tokens,
                s.context_window_size
            ],
        )?;
        Ok(())
    }

    pub fn list_sessions(&self) -> Result<Vec<Session>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id,orchestrator_id,name,repo,status,agent_type,cost_usd,
             started_at,pr_number,pr_id,workspace_path,pid,model,context_tokens,
             catalogue_path,context_used_pct,context_total_tokens,context_window_size
             FROM sessions ORDER BY started_at DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, f64>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, Option<u64>>(8)?,
                r.get::<_, Option<i64>>(9)?,
                r.get::<_, Option<String>>(10)?,
                r.get::<_, Option<u32>>(11)?,
                r.get::<_, Option<String>>(12)?,
                r.get::<_, Option<i64>>(13)?,
                r.get::<_, Option<String>>(14)?,
                r.get::<_, Option<f64>>(15)?,
                r.get::<_, Option<i64>>(16)?,
                r.get::<_, Option<i64>>(17)?,
            ))
        })?;
        rows.map(|r| {
            let (id, orchestrator_id, name, repo, status_str, agent_type,
                 cost_usd, started_at, pr_number, pr_id, workspace_path, pid,
                 model, context_tokens, catalogue_path, context_used_pct,
                 context_total_tokens, context_window_size) = r?;
            let status = serde_json::from_str(&format!("\"{status_str}\""))
                .unwrap_or(SessionStatus::Working);
            Ok(Session {
                id, orchestrator_id, name, repo, status, agent_type,
                cost_usd, started_at, pr_number, pr_id, workspace_path, pid,
                model, context_tokens: context_tokens.map(|v| v.max(0) as u64),
                catalogue_path,
                context_used_pct,
                context_total_tokens: context_total_tokens.map(|v| v.max(0) as u64),
                context_window_size: context_window_size.map(|v| v.max(0) as u64),
            })
        })
        .collect()
    }

    pub fn get_session(&self, id: &str) -> Result<Option<Session>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id,orchestrator_id,name,repo,status,agent_type,cost_usd,
             started_at,pr_number,pr_id,workspace_path,pid,model,context_tokens,
             catalogue_path,context_used_pct,context_total_tokens,context_window_size
             FROM sessions WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map([id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, f64>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, Option<u64>>(8)?,
                r.get::<_, Option<i64>>(9)?,
                r.get::<_, Option<String>>(10)?,
                r.get::<_, Option<u32>>(11)?,
                r.get::<_, Option<String>>(12)?,
                r.get::<_, Option<i64>>(13)?,
                r.get::<_, Option<String>>(14)?,
                r.get::<_, Option<f64>>(15)?,
                r.get::<_, Option<i64>>(16)?,
                r.get::<_, Option<i64>>(17)?,
            ))
        })?;
        match rows.next() {
            None => Ok(None),
            Some(r) => {
                let (id, orchestrator_id, name, repo, status_str, agent_type,
                     cost_usd, started_at, pr_number, pr_id, workspace_path, pid,
                     model, context_tokens, catalogue_path, context_used_pct,
                     context_total_tokens, context_window_size) = r?;
                let status = serde_json::from_str(&format!("\"{status_str}\""))
                    .unwrap_or(SessionStatus::Working);
                Ok(Some(Session {
                    id, orchestrator_id, name, repo, status, agent_type,
                    cost_usd, started_at, pr_number, pr_id, workspace_path, pid,
                    model, context_tokens: context_tokens.map(|v| v.max(0) as u64),
                    catalogue_path,
                    context_used_pct,
                    context_total_tokens: context_total_tokens.map(|v| v.max(0) as u64),
                    context_window_size: context_window_size.map(|v| v.max(0) as u64),
                }))
            }
        }
    }

    /// Non-zero `cost_usd` samples recorded for sessions matching the given
    /// agent harness (`agent_type`) and model — used to compute a
    /// data-driven spawn-modal cost estimate once enough history exists for
    /// a given preset. Read-only; built on `list_sessions` like
    /// `sessions_by_orchestrator`.
    pub fn cost_samples(&self, agent_type: &str, model: Option<&str>) -> Result<Vec<f64>> {
        let sessions = self.list_sessions()?;
        Ok(sessions
            .into_iter()
            .filter(|s| {
                s.agent_type == agent_type
                    && s.model.as_deref() == model
                    && s.cost_usd > 0.0
            })
            .map(|s| s.cost_usd)
            .collect())
    }

    pub fn upsert_orchestrator(&self, o: &Orchestrator) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO orchestrators(id,name,created_at) VALUES(?1,?2,?3)
             ON CONFLICT(id) DO UPDATE SET name=excluded.name",
            params![o.id, o.name, o.created_at],
        )?;
        Ok(())
    }

    pub fn sessions_by_orchestrator(&self, orchestrator_id: &str) -> Result<Vec<Session>> {
        let sessions = self.list_sessions()?;
        Ok(sessions.into_iter().filter(|s| s.orchestrator_id.as_deref() == Some(orchestrator_id)).collect())
    }

    pub fn delete_session(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM sessions WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn delete_orchestrator(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM sessions WHERE orchestrator_id = ?1", [id])?;
        conn.execute("DELETE FROM sessions WHERE id = ?1", [id])?;
        conn.execute("DELETE FROM orchestrators WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn upsert_pr(&self, pr: &PR) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO prs
             (id, number, title, url, body, session_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![pr.id, pr.number, pr.title, pr.url, pr.body, pr.session_id],
        )?;
        Ok(())
    }

    pub fn get_pr(&self, id: PrId) -> Result<Option<PR>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, number, title, url, body, session_id FROM prs WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map([id], |r| {
            Ok(PR {
                id:         r.get(0)?,
                number:     r.get(1)?,
                title:      r.get(2)?,
                url:        r.get(3)?,
                body:       r.get(4)?,
                session_id: r.get(5)?,
            })
        })?;
        rows.next().transpose().map_err(Into::into)
    }

    pub fn upsert_ci_status(&self, ci: &CIStatus) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO ci_status
             (pr_id, total, passing, failing, pending)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![ci.pr_id, ci.total, ci.passing, ci.failing, ci.pending],
        )?;
        Ok(())
    }

    pub fn upsert_comment(&self, c: &Comment) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO review_comments
             (id, pr_id, author, body, path, line, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![c.id, c.pr_id, c.author, c.body, c.path, c.line, c.created_at],
        )?;
        Ok(())
    }

    pub fn list_orchestrators(&self) -> Result<Vec<Orchestrator>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id,name,created_at FROM orchestrators ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Orchestrator {
                id: r.get(0)?,
                name: r.get(1)?,
                created_at: r.get(2)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn test_store() -> Store {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.db");
        // keep dir alive for the lifetime of the test by leaking it
        std::mem::forget(dir);
        Store::open(path).unwrap()
    }

    #[test]
    fn upsert_and_list_session() {
        let store = test_store();
        let session = Session {
            id: "s1".into(), orchestrator_id: None, name: "worker-1".into(),
            repo: "slievr/Athene".into(), status: SessionStatus::Working,
            agent_type: "claude-code".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
        };
        store.upsert_session(&session).unwrap();
        let list = store.list_sessions().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "s1");
    }

    #[test]
    fn upsert_updates_status() {
        let store = test_store();
        let mut s = Session {
            id: "s1".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: SessionStatus::Working,
            agent_type: "c".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
        };
        store.upsert_session(&s).unwrap();
        s.status = SessionStatus::Done;
        store.upsert_session(&s).unwrap();
        let list = store.list_sessions().unwrap();
        assert_eq!(list.len(), 1);
        assert!(matches!(list[0].status, SessionStatus::Done));
    }

    #[test]
    fn get_session_by_id() {
        let store = test_store();
        let s = Session {
            id: "s1".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: SessionStatus::Working,
            agent_type: "c".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
        };
        store.upsert_session(&s).unwrap();
        let found = store.get_session("s1").unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "w");
        assert!(store.get_session("missing").unwrap().is_none());
    }

    #[test]
    fn model_and_context_tokens_round_trip() {
        let store = test_store();
        let s = Session {
            id: "s1".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: SessionStatus::Working,
            agent_type: "claude-code".into(), cost_usd: 1.5, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: Some("claude-fable-5".into()), context_tokens: Some(214_000), catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
        };
        store.upsert_session(&s).unwrap();
        let found = store.get_session("s1").unwrap().unwrap();
        assert_eq!(found.model.as_deref(), Some("claude-fable-5"));
        assert_eq!(found.context_tokens, Some(214_000));
    }

    #[test]
    fn catalogue_path_round_trips() {
        let store = test_store();
        let s = Session {
            id: "s2".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: SessionStatus::Working,
            agent_type: "claude-code".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None,
            catalogue_path: Some("/brains/x".into()),
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
        };
        store.upsert_session(&s).unwrap();
        let found = store.get_session("s2").unwrap().unwrap();
        assert_eq!(found.catalogue_path.as_deref(), Some("/brains/x"));
        // list path decodes it too
        assert_eq!(store.list_sessions().unwrap()[0].catalogue_path.as_deref(), Some("/brains/x"));
    }

    /// A Re-file respawns the same session id with a fresh `started_at` —
    /// the conflict-update path must persist it, or the in-memory time
    /// silently reverts to the original spawn time on app restart.
    #[test]
    fn upsert_conflict_updates_started_at() {
        let store = test_store();
        let mut s = Session {
            id: "s3".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: SessionStatus::Working,
            agent_type: "claude-code".into(), cost_usd: 0.0, started_at: 100,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
        };
        store.upsert_session(&s).unwrap();
        s.started_at = 200;
        store.upsert_session(&s).unwrap();
        assert_eq!(store.get_session("s3").unwrap().unwrap().started_at, 200);
    }

    #[test]
    fn get_pr_round_trips_and_misses_cleanly() {
        let store = test_store();
        assert!(store.get_pr(9).unwrap().is_none());
        let pr = PR {
            id: 9, number: 9, title: "t".into(),
            url: "https://github.com/org/repo/pull/9".into(),
            body: String::new(), session_id: "s1".into(),
        };
        store.upsert_pr(&pr).unwrap();
        let found = store.get_pr(9).unwrap().unwrap();
        assert_eq!(found.number, 9);
        assert_eq!(found.session_id, "s1");
        assert_eq!(found.url, pr.url);
    }

    #[test]
    fn context_fields_round_trip() {
        let store = test_store();
        let s = Session {
            id: "s1".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: SessionStatus::Working,
            agent_type: "claude-code".into(), cost_usd: 2.6, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: Some(62.0),
            context_total_tokens: Some(124_000),
            context_window_size: Some(200_000),
        };
        store.upsert_session(&s).unwrap();
        let found = store.get_session("s1").unwrap().unwrap();
        assert_eq!(found.context_used_pct, Some(62.0));
        assert_eq!(found.context_total_tokens, Some(124_000));
        assert_eq!(found.context_window_size, Some(200_000));
        // list path decodes it too
        assert_eq!(store.list_sessions().unwrap()[0].context_used_pct, Some(62.0));
    }

    #[test]
    fn context_fields_default_to_none() {
        let store = test_store();
        let s = Session {
            id: "s2".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: SessionStatus::Working,
            agent_type: "claude-code".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
        };
        store.upsert_session(&s).unwrap();
        let found = store.get_session("s2").unwrap().unwrap();
        assert_eq!(found.context_used_pct, None);
        assert_eq!(found.context_total_tokens, None);
        assert_eq!(found.context_window_size, None);
    }

    #[test]
    fn cost_samples_filters_by_agent_and_model_and_excludes_zero() {
        let store = test_store();
        for (id, agent_type, model, cost) in [
            ("a", "claude-code", Some("claude-fable-5"), 3.0),
            ("b", "claude-code", Some("claude-fable-5"), 5.0),
            ("c", "claude-code", Some("claude-fable-5"), 0.0), // excluded: zero cost
            ("d", "claude-code", Some("claude-opus-4-8"), 2.0), // excluded: different model
            ("e", "codex",       Some("claude-fable-5"), 4.0),  // excluded: different harness
        ] {
            store.upsert_session(&Session {
                id: id.into(), orchestrator_id: None, name: id.into(),
                repo: "r".into(), status: SessionStatus::Working,
                agent_type: agent_type.into(), cost_usd: cost, started_at: 0,
                pr_number: None, pr_id: None, workspace_path: None, pid: None,
                model: model.map(String::from), context_tokens: None, catalogue_path: None,
                context_used_pct: None, context_total_tokens: None, context_window_size: None,
            }).unwrap();
        }
        let samples = store.cost_samples("claude-code", Some("claude-fable-5")).unwrap();
        assert_eq!(samples.len(), 2);
        assert!((samples.iter().sum::<f64>() - 8.0).abs() < f64::EPSILON);
    }
}
