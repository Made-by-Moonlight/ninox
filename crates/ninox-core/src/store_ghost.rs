//! Persistence for `crate::ghost_plan`. Node transitions are conditional
//! UPDATEs (`... WHERE state IN (...)`), so two processes deciding or
//! spawning the same node can never both win.

use super::Store;
use crate::brain_set::BrainSet;
use crate::ghost_plan::{NodeRow, NodeState, PlanRow, PlanStatus, PlanWithNodes, ValidatedProposal};
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension, Row, TransactionBehavior};

pub(super) const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS ghost_plans (
        id            TEXT PRIMARY KEY,
        proposer      TEXT NOT NULL,
        created_at    INTEGER NOT NULL,
        expires_at    INTEGER NOT NULL,
        status        TEXT NOT NULL,
        target_parent TEXT,
        title         TEXT,
        decided_by    TEXT,
        decided_at    INTEGER
    );
    CREATE TABLE IF NOT EXISTS ghost_nodes (
        plan_id        TEXT NOT NULL,
        node_id        TEXT NOT NULL,
        seq            INTEGER NOT NULL,
        parent_node_id TEXT,
        worker_type    TEXT NOT NULL,
        name           TEXT NOT NULL,
        brief          TEXT NOT NULL,
        workspace      TEXT,
        model          TEXT,
        brain          TEXT,
        dependencies   TEXT NOT NULL,
        await_done     INTEGER NOT NULL DEFAULT 0,
        linear_ref     TEXT,
        state          TEXT NOT NULL,
        session_id     TEXT,
        error          TEXT,
        updated_at     INTEGER NOT NULL,
        PRIMARY KEY (plan_id, node_id)
    );
";

pub(super) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)?;
    Ok(())
}

/// A `spawning` claim older than this with no session behind it belongs to
/// a process that died mid-spawn; it goes back to `waiting`.
/// Arbitrary; a spawn normally finishes in seconds.
const STALE_CLAIM_MS: i64 = 10 * 60 * 1000;

fn plan_from_row(r: &Row) -> rusqlite::Result<PlanRow> {
    Ok(PlanRow {
        id: r.get(0)?,
        proposer: r.get(1)?,
        created_at: r.get(2)?,
        expires_at: r.get(3)?,
        status: PlanStatus::parse(&r.get::<_, String>(4)?),
        target_parent: r.get(5)?,
        title: r.get(6)?,
        decided_by: r.get(7)?,
        decided_at: r.get(8)?,
    })
}

const PLAN_COLS: &str = "id, proposer, created_at, expires_at, status, target_parent, title, decided_by, decided_at";
const NODE_COLS: &str = "plan_id, node_id, parent_node_id, worker_type, name, brief, workspace, model, brain, \
                         dependencies, await_done, linear_ref, state, session_id, error, updated_at, seq";

fn node_from_row(r: &Row) -> rusqlite::Result<NodeRow> {
    let brain: Option<String> = r.get(8)?;
    let deps: String = r.get(9)?;
    Ok(NodeRow {
        plan_id: r.get(0)?,
        node_id: r.get(1)?,
        parent_node_id: r.get(2)?,
        worker_type: r.get(3)?,
        name: r.get(4)?,
        brief: r.get(5)?,
        workspace: r.get(6)?,
        model: r.get(7)?,
        brain: brain.and_then(|b| serde_json::from_str::<BrainSet>(&b).ok()),
        dependencies: serde_json::from_str(&deps).unwrap_or_default(),
        await_done: r.get::<_, i64>(10)? != 0,
        linear_ref: r.get(11)?,
        state: NodeState::parse(&r.get::<_, String>(12)?),
        session_id: r.get(13)?,
        error: r.get(14)?,
        updated_at: r.get(15)?,
        seq: r.get(16)?,
    })
}

fn nodes_of(conn: &Connection, plan_id: &str) -> Result<Vec<NodeRow>> {
    let mut stmt = conn.prepare(&format!("SELECT {NODE_COLS} FROM ghost_nodes WHERE plan_id = ?1 ORDER BY seq"))?;
    let rows = stmt.query_map([plan_id], node_from_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Derives a plan's status from its nodes once none is left undecided.
fn settle_status(conn: &Connection, plan_id: &str) -> Result<()> {
    let nodes = nodes_of(conn, plan_id)?;
    if nodes.iter().any(|n| n.state == NodeState::Ghost) {
        return Ok(());
    }
    let status = if nodes.iter().any(|n| n.state.is_accepted()) {
        PlanStatus::Accepted
    } else if nodes.iter().any(|n| n.state == NodeState::Expired) {
        PlanStatus::Expired
    } else {
        PlanStatus::Discarded
    };
    conn.execute(
        "UPDATE ghost_plans SET status = ?2 WHERE id = ?1 AND status = 'pending'",
        params![plan_id, status.as_str()],
    )?;
    Ok(())
}

impl Store {
    pub fn insert_ghost_plan(&self, id: &str, proposer: &str, proposal: &ValidatedProposal, now: i64, expires_at: i64) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            &format!("INSERT INTO ghost_plans ({PLAN_COLS}) VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?6, NULL, NULL)"),
            params![id, proposer, now, expires_at, proposal.target_parent, proposal.title],
        )?;
        for n in &proposal.nodes {
            let brain = n.brain.as_ref().map(serde_json::to_string).transpose()?;
            tx.execute(
                &format!("INSERT INTO ghost_nodes ({NODE_COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,'ghost',NULL,NULL,?13,?14)"),
                params![
                    id, n.node_id, n.parent_node_id, n.worker_type, n.name, n.brief, n.workspace, n.model, brain,
                    serde_json::to_string(&n.dependencies)?, i64::from(n.await_done), n.linear_ref, now, n.seq,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_ghost_plan(&self, id: &str) -> Result<Option<PlanWithNodes>> {
        let conn = self.conn.lock().unwrap();
        let plan = conn
            .query_row(&format!("SELECT {PLAN_COLS} FROM ghost_plans WHERE id = ?1"), [id], plan_from_row)
            .optional()?;
        match plan {
            Some(plan) => Ok(Some(PlanWithNodes { nodes: nodes_of(&conn, &plan.id)?, plan })),
            None => Ok(None),
        }
    }

    /// Newest first.
    pub fn list_ghost_plans(&self, pending_only: bool) -> Result<Vec<PlanWithNodes>> {
        let conn = self.conn.lock().unwrap();
        let filter = if pending_only { "WHERE status = 'pending'" } else { "" };
        let mut stmt = conn.prepare(&format!("SELECT {PLAN_COLS} FROM ghost_plans {filter} ORDER BY created_at DESC, id"))?;
        let plans = stmt.query_map([], plan_from_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        plans
            .into_iter()
            .map(|plan| Ok(PlanWithNodes { nodes: nodes_of(&conn, &plan.id)?, plan }))
            .collect()
    }

    /// Plans with accepted nodes still to spawn (for the poller).
    pub fn ghost_plans_with_waiting(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT DISTINCT plan_id FROM ghost_nodes WHERE state IN ('waiting','spawning') ORDER BY plan_id")?;
        let ids = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(ids)
    }

    /// Moves each listed node from one of `from` to `to`; returns the ids
    /// that actually moved. Settles the plan's status afterwards.
    pub fn transition_ghost_nodes(&self, plan_id: &str, node_ids: &[String], from: &[NodeState], to: NodeState, now: i64) -> Result<Vec<String>> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let from_sql = from.iter().map(|s| format!("'{}'", s.as_str())).collect::<Vec<_>>().join(",");
        let mut moved = Vec::new();
        for id in node_ids {
            let n = tx.execute(
                &format!("UPDATE ghost_nodes SET state = ?3, updated_at = ?4, error = NULL WHERE plan_id = ?1 AND node_id = ?2 AND state IN ({from_sql})"),
                params![plan_id, id, to.as_str(), now],
            )?;
            if n == 1 {
                moved.push(id.clone());
            }
        }
        settle_status(&tx, plan_id)?;
        tx.commit()?;
        Ok(moved)
    }

    pub fn record_ghost_decision(&self, plan_id: &str, decided_by: &str, now: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE ghost_plans SET decided_by = ?2, decided_at = ?3 WHERE id = ?1",
            params![plan_id, decided_by, now],
        )?;
        Ok(())
    }

    /// `waiting` → `spawning`, for exactly one caller.
    pub fn claim_ghost_node(&self, plan_id: &str, node_id: &str, now: i64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE ghost_nodes SET state = 'spawning', updated_at = ?3 WHERE plan_id = ?1 AND node_id = ?2 AND state = 'waiting'",
            params![plan_id, node_id, now],
        )?;
        Ok(n == 1)
    }

    /// Ends this caller's claim: `spawned` (with its session) or `failed`
    /// (with why).
    pub fn finish_ghost_node(&self, plan_id: &str, node_id: &str, state: NodeState, session_id: Option<&str>, error: Option<&str>, now: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE ghost_nodes SET state = ?3, session_id = COALESCE(?4, session_id), error = ?5, updated_at = ?6
             WHERE plan_id = ?1 AND node_id = ?2 AND state = 'spawning'",
            params![plan_id, node_id, state.as_str(), session_id, error, now],
        )?;
        Ok(())
    }

    /// A waiting node can never spawn (its parent or a dependency is gone).
    /// Only from `waiting`, so a stale verdict can't override another
    /// process's live claim.
    pub fn fail_waiting_ghost_node(&self, plan_id: &str, node_id: &str, error: &str, now: i64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE ghost_nodes SET state = 'failed', error = ?3, updated_at = ?4 WHERE plan_id = ?1 AND node_id = ?2 AND state = 'waiting'",
            params![plan_id, node_id, error, now],
        )?;
        Ok(n == 1)
    }

    /// A waiting node can't spawn yet; remember why for display.
    pub fn note_ghost_node_wait(&self, plan_id: &str, node_id: &str, reason: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE ghost_nodes SET error = ?3 WHERE plan_id = ?1 AND node_id = ?2 AND state = 'waiting'",
            params![plan_id, node_id, reason],
        )?;
        Ok(())
    }

    /// Returns stale `spawning` claims with no session row or queue entry to
    /// `waiting`. A claim whose name is taken by a session is marked
    /// `failed` for a human to look at: that session may be this node's or
    /// an unrelated one with the same name, and guessing either way risks a
    /// double spawn or a wrong attachment.
    pub fn recover_stale_ghost_claims(&self, now: i64) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT plan_id, node_id, name FROM ghost_nodes WHERE state = 'spawning' AND updated_at < ?1")?;
        let stale = stmt
            .query_map([now - STALE_CLAIM_MS], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        let mut n = 0;
        for (plan, node, name) in stale {
            let slug = crate::slugify(&name);
            let exists: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1) OR EXISTS(SELECT 1 FROM session_queue WHERE id = ?1)",
                    [&slug],
                    |r| r.get(0),
                )?;
            let (state, error) = if exists {
                ("failed", Some(format!("its spawn was interrupted and a session named '{slug}' exists; check it")))
            } else {
                ("waiting", None)
            };
            n += conn.execute(
                "UPDATE ghost_nodes SET state = ?3, error = ?4, updated_at = ?5
                 WHERE plan_id = ?1 AND node_id = ?2 AND state = 'spawning'",
                params![plan, node, state, error, now],
            )?;
        }
        Ok(n)
    }

    /// Expires every pending plan past its deadline: its undecided nodes
    /// become `expired` (accepted ones keep going). Returns the plan ids.
    pub fn expire_ghost_plans(&self, now: i64) -> Result<Vec<String>> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ids = {
            let mut stmt = tx.prepare("SELECT id FROM ghost_plans WHERE status = 'pending' AND expires_at <= ?1")?;
            let ids = stmt.query_map([now], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            ids
        };
        for id in &ids {
            tx.execute(
                "UPDATE ghost_nodes SET state = 'expired', updated_at = ?2 WHERE plan_id = ?1 AND state = 'ghost'",
                params![id, now],
            )?;
            settle_status(&tx, id)?;
        }
        tx.commit()?;
        Ok(ids)
    }
}
