//! Bookkeeping for the automatic reset-from-plan trigger
//! (`[orchestrator_reset]`): one row per orchestrator for its current
//! conversation, plus markers for flows a person started.

use anyhow::Result;
use rusqlite::{params, OptionalExtension};

use super::Store;

/// Where the automatic trigger stands for an orchestrator's current conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetState {
    /// Armed: nothing sent yet for this conversation.
    Fresh,
    /// The plan request went out; waiting for the plan update.
    Requested,
    /// A failed or timed-out flow; armed again at `cooldown_until`.
    Cooldown,
    /// The user cancelled the flow; stays off for this conversation.
    Suppressed,
}

impl ResetState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Fresh => "fresh",
            Self::Requested => "requested",
            Self::Cooldown => "cooldown",
            Self::Suppressed => "suppressed",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "requested" => Self::Requested,
            "cooldown" => Self::Cooldown,
            "suppressed" => Self::Suppressed,
            _ => Self::Fresh,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResetRow {
    pub orchestrator_id: String,
    /// The Claude conversation this row describes; a different one is a new row.
    pub conversation_id: String,
    pub state: ResetState,
    pub started_at: i64,
    pub baseline_path: Option<String>,
    pub baseline_hash: Option<String>,
    pub cooldown_until: i64,
    /// This conversation was started by a reset.
    pub after_reset: bool,
    /// The first context reading of a conversation started by a reset.
    pub post_reset_baseline_pct: Option<f64>,
    pub loop_notified: bool,
    pub ready_notified: bool,
}

impl ResetRow {
    pub fn fresh(orchestrator_id: &str, conversation_id: &str, after_reset: bool) -> Self {
        Self {
            orchestrator_id: orchestrator_id.into(),
            conversation_id: conversation_id.into(),
            state: ResetState::Fresh,
            started_at: 0,
            baseline_path: None,
            baseline_hash: None,
            cooldown_until: 0,
            after_reset,
            post_reset_baseline_pct: None,
            loop_notified: false,
            ready_notified: false,
        }
    }
}

impl Store {
    pub fn reset_row(&self, orchestrator_id: &str) -> Result<Option<ResetRow>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT orchestrator_id,conversation_id,state,started_at,baseline_path,baseline_hash,cooldown_until,
                    after_reset,post_reset_baseline_pct,loop_notified,ready_notified
             FROM orchestrator_resets WHERE orchestrator_id=?1",
            [orchestrator_id],
            |r| {
                Ok(ResetRow {
                    orchestrator_id: r.get(0)?,
                    conversation_id: r.get(1)?,
                    state: ResetState::parse(&r.get::<_, String>(2)?),
                    started_at: r.get(3)?,
                    baseline_path: r.get(4)?,
                    baseline_hash: r.get(5)?,
                    cooldown_until: r.get(6)?,
                    after_reset: r.get::<_, i64>(7)? != 0,
                    post_reset_baseline_pct: r.get(8)?,
                    loop_notified: r.get::<_, i64>(9)? != 0,
                    ready_notified: r.get::<_, i64>(10)? != 0,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn put_reset_row(&self, row: &ResetRow) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO orchestrator_resets
               (orchestrator_id,conversation_id,state,started_at,baseline_path,baseline_hash,cooldown_until,
                after_reset,post_reset_baseline_pct,loop_notified,ready_notified)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
             ON CONFLICT(orchestrator_id) DO UPDATE SET
               conversation_id=excluded.conversation_id, state=excluded.state, started_at=excluded.started_at,
               baseline_path=excluded.baseline_path, baseline_hash=excluded.baseline_hash,
               cooldown_until=excluded.cooldown_until, after_reset=excluded.after_reset,
               post_reset_baseline_pct=excluded.post_reset_baseline_pct, loop_notified=excluded.loop_notified,
               ready_notified=excluded.ready_notified",
            params![
                row.orchestrator_id, row.conversation_id, row.state.as_str(), row.started_at, row.baseline_path,
                row.baseline_hash, row.cooldown_until, row.after_reset as i64, row.post_reset_baseline_pct,
                row.loop_notified as i64, row.ready_notified as i64
            ],
        )?;
        Ok(())
    }

    /// A reset launched `new_conversation` for `orchestrator_id`: re-arm the
    /// trigger for it, and drop the old conversation's context readings so
    /// nothing reads them as the new conversation's.
    pub fn note_conversation_reset(&self, orchestrator_id: &str, new_conversation: &str) -> Result<()> {
        self.put_reset_row(&ResetRow::fresh(orchestrator_id, new_conversation, true))?;
        self.clear_context_readings(orchestrator_id)
    }

    /// Forget the statusline's context readings and the activity state, which
    /// belong to the conversation that just ended: a fresh one must report
    /// its own before anything acts on them. Column-scoped, so it cannot
    /// revert a field the poller or statusline hook wrote meanwhile.
    pub fn clear_context_readings(&self, session_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET context_used_pct=NULL, context_tokens=NULL, context_total_tokens=NULL,
                context_window_size=NULL, activity='unknown', activity_note=NULL, activity_since=NULL
             WHERE id=?1",
            [session_id],
        )?;
        Ok(())
    }

    /// The user cancelled the automatic flow for `orchestrator_id`'s current
    /// conversation; a no-op when none is running.
    pub fn suppress_auto_reset(&self, orchestrator_id: &str) -> Result<()> {
        if let Some(mut row) = self.reset_row(orchestrator_id)? {
            if row.state == ResetState::Requested {
                row.state = ResetState::Suppressed;
                self.put_reset_row(&row)?;
            }
        }
        Ok(())
    }

    pub fn set_reset_marker(&self, orchestrator_id: &str, started_at: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO reset_flow_markers(orchestrator_id,started_at) VALUES(?1,?2)
             ON CONFLICT(orchestrator_id) DO UPDATE SET started_at=excluded.started_at",
            params![orchestrator_id, started_at],
        )?;
        Ok(())
    }

    /// Refresh an existing marker's timestamp; does nothing, and returns
    /// false, when it has been cleared, so a heartbeat can never bring back
    /// a marker its flow already released.
    pub fn refresh_reset_marker(&self, orchestrator_id: &str, at: i64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute("UPDATE reset_flow_markers SET started_at=?2 WHERE orchestrator_id=?1", params![orchestrator_id, at])?;
        Ok(n > 0)
    }

    /// Clear the marker only if it is still the one set at `started_at`, so a
    /// flow that ends cannot drop another flow's marker.
    pub fn clear_reset_marker_if(&self, orchestrator_id: &str, started_at: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM reset_flow_markers WHERE orchestrator_id=?1 AND started_at=?2",
            params![orchestrator_id, started_at],
        )?;
        Ok(())
    }

    pub fn clear_reset_marker(&self, orchestrator_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM reset_flow_markers WHERE orchestrator_id=?1", [orchestrator_id])?;
        Ok(())
    }

    pub fn reset_marker(&self, orchestrator_id: &str) -> Result<Option<i64>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT started_at FROM reset_flow_markers WHERE orchestrator_id=?1", [orchestrator_id], |r| r.get(0))
            .optional()
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap()
    }

    #[test]
    fn rows_round_trip_and_markers_come_and_go() {
        let s = store();
        assert!(s.reset_row("o").unwrap().is_none());
        let mut row = ResetRow::fresh("o", "c1", true);
        row.state = ResetState::Requested;
        row.baseline_hash = Some("h".into());
        row.post_reset_baseline_pct = Some(12.5);
        s.put_reset_row(&row).unwrap();
        assert_eq!(s.reset_row("o").unwrap().unwrap(), row);
        s.suppress_auto_reset("o").unwrap();
        assert_eq!(s.reset_row("o").unwrap().unwrap().state, ResetState::Suppressed);

        assert_eq!(s.reset_marker("o").unwrap(), None);
        s.set_reset_marker("o", 5).unwrap();
        assert_eq!(s.reset_marker("o").unwrap(), Some(5));
        s.clear_reset_marker("o").unwrap();
        assert_eq!(s.reset_marker("o").unwrap(), None);
    }

    #[test]
    fn clearing_readings_only_touches_the_reading_and_activity_columns() {
        let s = store();
        let mut row = crate::types::Session {
            id: "o".into(), orchestrator_id: None, name: "o".into(), repo: String::new(),
            status: crate::types::SessionStatus::Working, agent_type: "claude-code".into(),
            cost_usd: 0.0, started_at: 0, pr_number: None, pr_id: None,
            workspace_path: None, pid: None,
            model: None, observed_model: None, context_tokens: None,
            brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None, worker_type: None, success_criteria: Vec::new(),
            completion_outcome: None, self_completed_at: None, cleanup_result: None, cleanup_at: None,
        };
        row.context_used_pct = Some(55.0);
        row.context_tokens = Some(1);
        row.context_window_size = Some(3);
        row.cost_usd = 9.5;
        row.activity = crate::types::ActivityState::Idle;
        s.upsert_session(&row).unwrap();
        s.clear_context_readings("o").unwrap();
        let got = s.get_session("o").unwrap().unwrap();
        assert_eq!((got.context_used_pct, got.context_tokens, got.context_window_size), (None, None, None));
        assert_eq!(got.activity, crate::types::ActivityState::Unknown);
        assert_eq!(got.cost_usd, 9.5, "everything else stays");
    }

    #[test]
    fn refreshing_a_cleared_marker_does_not_bring_it_back() {
        let s = store();
        assert!(!s.refresh_reset_marker("o", 5).unwrap());
        assert_eq!(s.reset_marker("o").unwrap(), None);
        s.set_reset_marker("o", 1).unwrap();
        assert!(s.refresh_reset_marker("o", 9).unwrap());
        assert_eq!(s.reset_marker("o").unwrap(), Some(9));
        s.clear_reset_marker("o").unwrap();
        assert!(!s.refresh_reset_marker("o", 12).unwrap());
        assert_eq!(s.reset_marker("o").unwrap(), None);
    }

    #[test]
    fn a_marker_is_only_cleared_by_the_flow_that_set_it() {
        let s = store();
        s.set_reset_marker("o", 5).unwrap();
        s.set_reset_marker("o", 9).unwrap();
        s.clear_reset_marker_if("o", 5).unwrap();
        assert_eq!(s.reset_marker("o").unwrap(), Some(9), "the newer flow's marker survives");
        s.clear_reset_marker_if("o", 9).unwrap();
        assert_eq!(s.reset_marker("o").unwrap(), None);
    }

    #[test]
    fn suppress_only_touches_a_running_flow() {
        let s = store();
        s.put_reset_row(&ResetRow::fresh("o", "c1", false)).unwrap();
        s.suppress_auto_reset("o").unwrap();
        assert_eq!(s.reset_row("o").unwrap().unwrap().state, ResetState::Fresh);
        s.suppress_auto_reset("none").unwrap();
    }
}
