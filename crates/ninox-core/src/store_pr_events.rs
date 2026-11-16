//! Persistence for `crate::pr_events`: per-(subscriber, PR) cursors, so a
//! daemon restart never replays an event, and the latest observed PR state
//! that `ninox pr status` reads without calling GitHub.

use super::Store;
use crate::pr_events::{PrCursor, PrStatusView};
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::BTreeSet;

pub(super) const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS pr_event_cursors (
        subscriber TEXT NOT NULL,
        repo       TEXT NOT NULL,
        number     INTEGER NOT NULL,
        cursor     TEXT NOT NULL,
        updated_at INTEGER NOT NULL,
        PRIMARY KEY (subscriber, repo, number)
    );
    CREATE TABLE IF NOT EXISTS pr_status_cache (
        repo        TEXT NOT NULL,
        number      INTEGER NOT NULL,
        view        TEXT NOT NULL,
        observed_at INTEGER NOT NULL,
        PRIMARY KEY (repo, number)
    );
    CREATE TABLE IF NOT EXISTS github_budget (
        id       INTEGER PRIMARY KEY CHECK (id = 1),
        reading  TEXT NOT NULL
    );
";

pub(super) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)?;
    Ok(())
}

/// Identifies one subscription: who is told, about which PR.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CursorKey {
    pub subscriber: String,
    pub repo:       String,
    pub number:     u64,
}

impl Store {
    /// `None` for a subscription never observed (or one whose stored cursor
    /// no longer parses, which then re-baselines rather than failing).
    pub fn load_pr_event_cursor(&self, key: &CursorKey) -> Result<Option<PrCursor>> {
        let conn = self.conn.lock().unwrap();
        let raw: Option<String> = conn
            .query_row(
                "SELECT cursor FROM pr_event_cursors WHERE subscriber = ?1 AND repo = ?2 AND number = ?3",
                params![key.subscriber, key.repo, key.number as i64],
                |r| r.get(0),
            )
            .optional()?;
        Ok(raw.and_then(|s| serde_json::from_str(&s).ok()))
    }

    pub fn save_pr_event_cursor(&self, key: &CursorKey, cursor: &PrCursor, now: i64) -> Result<()> {
        let json = serde_json::to_string(cursor)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO pr_event_cursors (subscriber, repo, number, cursor, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (subscriber, repo, number) DO UPDATE SET cursor = excluded.cursor, updated_at = excluded.updated_at",
            params![key.subscriber, key.repo, key.number as i64, json, now],
        )?;
        Ok(())
    }

    /// Drops every cursor not in `keep` — subscriptions whose session ended
    /// or whose watch was closed. Driven by the store's own sessions and
    /// watches, never by what one GitHub sweep happened to return, so a
    /// transient miss doesn't reset a cursor.
    pub fn retain_pr_event_cursors(&self, keep: &BTreeSet<CursorKey>) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let existing: Vec<(String, String, i64)> = conn
            .prepare("SELECT subscriber, repo, number FROM pr_event_cursors")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut removed = 0;
        for (subscriber, repo, number) in existing {
            let key = CursorKey { subscriber, repo, number: number as u64 };
            if !keep.contains(&key) {
                removed += conn.execute(
                    "DELETE FROM pr_event_cursors WHERE subscriber = ?1 AND repo = ?2 AND number = ?3",
                    params![key.subscriber, key.repo, number],
                )?;
            }
        }
        Ok(removed)
    }

    pub fn save_pr_status(&self, view: &PrStatusView) -> Result<()> {
        let json = serde_json::to_string(view)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO pr_status_cache (repo, number, view, observed_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (repo, number) DO UPDATE SET view = excluded.view, observed_at = excluded.observed_at",
            params![view.repo, view.number as i64, json, view.observed_at],
        )?;
        Ok(())
    }

    pub fn get_pr_status(&self, repo: &str, number: u64) -> Result<Option<PrStatusView>> {
        let conn = self.conn.lock().unwrap();
        let raw: Option<String> = conn
            .query_row(
                "SELECT view FROM pr_status_cache WHERE repo = ?1 AND number = ?2",
                params![repo, number as i64],
                |r| r.get(0),
            )
            .optional()?;
        Ok(raw.and_then(|s| serde_json::from_str(&s).ok()))
    }

    /// Every cached PR with this number, for resolving a bare `#N`.
    pub fn find_pr_status_by_number(&self, number: u64) -> Result<Vec<PrStatusView>> {
        let conn = self.conn.lock().unwrap();
        let rows: Vec<String> = conn
            .prepare("SELECT view FROM pr_status_cache WHERE number = ?1 ORDER BY repo")?
            .query_map(params![number as i64], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows.into_iter().filter_map(|s| serde_json::from_str(&s).ok()).collect())
    }

    pub fn save_github_budget(&self, budget: &crate::github_budget::GithubBudget) -> Result<()> {
        let json = serde_json::to_string(budget)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO github_budget (id, reading) VALUES (1, ?1)
             ON CONFLICT (id) DO UPDATE SET reading = excluded.reading",
            params![json],
        )?;
        Ok(())
    }

    pub fn get_github_budget(&self) -> Result<Option<crate::github_budget::GithubBudget>> {
        let conn = self.conn.lock().unwrap();
        let raw: Option<String> = conn
            .query_row("SELECT reading FROM github_budget WHERE id = 1", [], |r| r.get(0))
            .optional()?;
        Ok(raw.and_then(|s| serde_json::from_str(&s).ok()))
    }

    pub fn list_prs(&self) -> Result<Vec<crate::types::PR>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT id, number, title, url, body, session_id FROM prs")?;
        let rows = stmt.query_map([], |r| {
            Ok(crate::types::PR {
                id:         r.get(0)?,
                number:     r.get(1)?,
                title:      r.get(2)?,
                url:        r.get(3)?,
                body:       r.get(4)?,
                session_id: r.get(5)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    pub fn list_ci_statuses(&self) -> Result<Vec<crate::types::CIStatus>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT pr_id, total, passing, failing, pending FROM ci_status")?;
        let rows = stmt.query_map([], |r| {
            Ok(crate::types::CIStatus {
                pr_id:   r.get(0)?,
                total:   r.get(1)?,
                passing: r.get(2)?,
                failing: r.get(3)?,
                pending: r.get(4)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    /// Bounds the cache: drops PRs nobody has observed for `max_age_ms`.
    pub fn prune_pr_status_cache(&self, older_than: i64) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute("DELETE FROM pr_status_cache WHERE observed_at < ?1", params![older_than])?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pr_events::CiVerdict;

    fn store() -> Store {
        Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap()
    }

    fn key(sub: &str, n: u64) -> CursorKey {
        CursorKey { subscriber: sub.into(), repo: "o/r".into(), number: n }
    }

    #[test]
    fn cursor_round_trips_and_upserts() {
        let _iso = crate::test_isolation::isolate();
        let s = store();
        assert_eq!(s.load_pr_event_cursor(&key("w1", 7)).unwrap(), None);
        let mut c = PrCursor { initialized: true, head_sha: "a".into(), ci: CiVerdict::Passed, ..PrCursor::default() };
        c.seen_inline.insert(5);
        s.save_pr_event_cursor(&key("w1", 7), &c, 1).unwrap();
        assert_eq!(s.load_pr_event_cursor(&key("w1", 7)).unwrap(), Some(c.clone()));
        c.head_sha = "b".into();
        s.save_pr_event_cursor(&key("w1", 7), &c, 2).unwrap();
        assert_eq!(s.load_pr_event_cursor(&key("w1", 7)).unwrap().unwrap().head_sha, "b");
        assert_eq!(s.load_pr_event_cursor(&key("w2", 7)).unwrap(), None, "cursors are per subscriber");
    }

    #[test]
    fn retain_drops_only_unlisted_subscriptions() {
        let _iso = crate::test_isolation::isolate();
        let s = store();
        for k in [key("w1", 7), key("w2", 7), key("w1", 8)] {
            s.save_pr_event_cursor(&k, &PrCursor::default(), 1).unwrap();
        }
        let keep = BTreeSet::from([key("w1", 7)]);
        assert_eq!(s.retain_pr_event_cursors(&keep).unwrap(), 2);
        assert!(s.load_pr_event_cursor(&key("w1", 7)).unwrap().is_some());
        assert!(s.load_pr_event_cursor(&key("w2", 7)).unwrap().is_none());
    }

    #[test]
    fn status_cache_upserts_finds_by_number_and_prunes() {
        let _iso = crate::test_isolation::isolate();
        let s = store();
        let view = |repo: &str, at: i64| PrStatusView { repo: repo.into(), number: 7, observed_at: at, ..PrStatusView::default() };
        s.save_pr_status(&view("o/r", 1)).unwrap();
        s.save_pr_status(&view("o/r", 5)).unwrap();
        s.save_pr_status(&view("x/y", 1)).unwrap();
        assert_eq!(s.get_pr_status("o/r", 7).unwrap().unwrap().observed_at, 5);
        assert_eq!(s.find_pr_status_by_number(7).unwrap().len(), 2);
        assert_eq!(s.prune_pr_status_cache(3).unwrap(), 1);
        assert!(s.get_pr_status("x/y", 7).unwrap().is_none());
    }
}
