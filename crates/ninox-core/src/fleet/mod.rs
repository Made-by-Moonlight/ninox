//! Durable fleets (spec `2026-10-01-terminal-native-runtime-design.md` §5):
//! the pure side of ordered fleet restore. Nothing here launches a process.
//!
//! - [`snapshot`]: [`FleetSnapshot`] — sessions + fleet facts + PR/CI +
//!   inbox/request state, read from the store.
//! - [`probe`]: workspace validation → [`Anomaly`]s (reported, never
//!   repaired by guessing).
//! - [`plan`]: restore order — workers first, orchestrators last.
//! - [`briefing`]: recovery briefing text.
//! - [`startup`]: `[fleet] restore_policy` at engine startup, and
//!   [`pending_restore`] for frontends.
//! - [`service`]: launchd/systemd autostart unit generation.
//!
//! Execution (relaunching sessions, delivering briefings) lives in
//! `ninox-app/src/fleet.rs` (`ninox fleet …`).

pub mod briefing;
pub mod cause;
pub mod plan;
pub mod probe;
pub mod service;
pub mod snapshot;
pub mod startup;

pub use plan::{plan_restore, project_outcomes, RestoreMode, RestorePlan, RestoreStep, SkippedSession};
pub use probe::{Anomaly, GitProbe, WorkspaceObservation, WorkspaceProbe};
pub use snapshot::{awaits_restore, FleetMember, FleetSnapshot, Role};
pub use startup::{dismiss_pending_restore, pending_restore, RestoreSummary, StartupAction};

use crate::store::Store;
use crate::types::SessionStatus;
use serde::Serialize;

/// Result of one session's restore attempt, persisted as
/// `fleet_sessions.restore_mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreOutcome {
    Resumed,
    Fresh,
    Failed,
    /// A live pane already existed (someone resumed it by hand).
    AlreadyLive,
}

impl RestoreOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Resumed     => "resumed",
            Self::Fresh       => "fresh",
            Self::Failed      => "failed",
            Self::AlreadyLive => "already_live",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "resumed"      => Self::Resumed,
            "fresh"        => Self::Fresh,
            "failed"       => Self::Failed,
            "already_live" => Self::AlreadyLive,
            _ => return None,
        })
    }
}

/// Called by `reconcile_dead_session` for every session it finds dead:
/// stamps the interruption time/cause and the pre-reconciliation status.
pub fn record_interruption(store: &Store, session_id: &str, started_at: i64, last_status: &SessionStatus, now: i64) {
    let boot = cause::boot_time_ms();
    let c = cause::classify(started_at, boot);
    let at = cause::interrupted_at(c, boot, now);
    if let Err(e) = store.record_interruption(session_id, at, last_status, Some(c.as_str())) {
        tracing::warn!("fleet: record interruption for {session_id}: {e}");
    }
}

/// A session was relaunched under its own id (the app's Resume/Re-file,
/// or `ninox fleet restore`). If it was awaiting restore, that interruption
/// is now handled — stamped exactly as a restore would, so a later
/// `ninox fleet restore` neither relaunches it again nor treats its next,
/// ordinary death as the old interruption.
pub fn note_relaunch(store: &Store, session_id: &str, resumed: bool, now: i64) {
    let Ok(Some(record)) = store.fleet_record(session_id) else { return };
    if !record.awaiting_restore() {
        return;
    }
    let outcome = if resumed { RestoreOutcome::Resumed } else { RestoreOutcome::Fresh };
    if let Err(e) = store.record_restore(session_id, now, outcome.as_str(), None) {
        tracing::warn!("fleet: record relaunch of {session_id}: {e}");
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum AckOutcome {
    NoRecovery,
    Acked { resolved_requests: usize },
}

/// `ninox fleet ack`: the orchestrator has taken stock of its briefing.
/// Delivered request-work items listed in that briefing count as handed
/// over, so they are resolved and won't reappear in a later briefing.
pub fn ack(store: &Store, orchestrator_id: &str, now: i64) -> anyhow::Result<AckOutcome> {
    let Some(rec) = store.recovery(orchestrator_id)? else { return Ok(AckOutcome::NoRecovery) };
    store.ack_recovery(orchestrator_id, now)?;
    let up_to = rec.briefing_sent_at.unwrap_or(now);
    let resolved_requests = store.resolve_delivered_work_requests(orchestrator_id, up_to, now)?;
    Ok(AckOutcome::Acked { resolved_requests })
}

/// Local wall-clock rendering for briefings: `2026-10-01 14:02`.
pub fn format_local(ms: i64) -> String {
    format_with_offset(ms, local_offset_secs(ms))
}

pub fn format_with_offset(ms: i64, offset_secs: i32) -> String {
    let Ok(utc) = time::OffsetDateTime::from_unix_timestamp(ms.div_euclid(1000)) else {
        return ms.to_string();
    };
    let off = time::UtcOffset::from_whole_seconds(offset_secs).unwrap_or(time::UtcOffset::UTC);
    let t = utc.to_offset(off);
    format!("{:04}-{:02}-{:02} {:02}:{:02}", t.year(), u8::from(t.month()), t.day(), t.hour(), t.minute())
}

fn local_offset_secs(ms: i64) -> i32 {
    let secs = ms.div_euclid(1000) as libc::time_t;
    // SAFETY: localtime_r writes only into the provided, zeroed `tm`.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&secs, &mut tm).is_null() {
            return 0;
        }
        tm.tm_gmtoff as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_round_trip() {
        for o in [RestoreOutcome::Resumed, RestoreOutcome::Fresh, RestoreOutcome::Failed, RestoreOutcome::AlreadyLive] {
            assert_eq!(RestoreOutcome::parse(o.as_str()), Some(o));
        }
        assert_eq!(RestoreOutcome::parse("nope"), None);
    }

    #[test]
    fn formats_with_offset() {
        // 2026-10-01T13:02:00Z
        let ms = 1_790_859_720_000;
        assert_eq!(format_with_offset(ms, 0), "2026-10-01 13:02");
        assert_eq!(format_with_offset(ms, 3600), "2026-10-01 14:02");
        assert_eq!(format_with_offset(ms, -14 * 3600), "2026-09-30 23:02");
    }

    #[test]
    fn relaunch_by_hand_closes_the_interruption() {
        let store = Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap();
        note_relaunch(&store, "never-interrupted", true, 5);
        assert!(store.fleet_record("never-interrupted").unwrap().is_none());

        store.record_interruption("w", 10, &SessionStatus::Working, Some("reboot")).unwrap();
        note_relaunch(&store, "w", true, 20);
        let r = store.fleet_record("w").unwrap().unwrap();
        assert!(!r.awaiting_restore());
        assert_eq!((r.restored_at, r.restore_mode.as_deref()), (Some(20), Some("resumed")));

        // Already handled: a second relaunch doesn't rewrite the outcome.
        note_relaunch(&store, "w", false, 30);
        assert_eq!(store.fleet_record("w").unwrap().unwrap().restored_at, Some(20));
    }

    #[test]
    fn ack_resolves_delivered_requests_up_to_the_briefing() {
        let store = Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap();
        assert_eq!(ack(&store, "o", 1).unwrap(), AckOutcome::NoRecovery);
        let req = |id: &str, at, delivered| crate::store::WorkRequestRow {
            id: id.into(), from_session: "w".into(), orchestrator_id: Some("o".into()),
            body: "b".into(), created_at: at, delivered_at: delivered, resolved_at: None,
        };
        store.insert_work_request(&req("before", 10, Some(11))).unwrap();
        store.insert_work_request(&req("undelivered", 12, None)).unwrap();
        store.insert_work_request(&req("after", 50, Some(51))).unwrap();
        store.begin_recovery("o", Some(5), 20).unwrap();
        store.mark_briefing_sent("o", 30).unwrap();
        assert_eq!(ack(&store, "o", 60).unwrap(), AckOutcome::Acked { resolved_requests: 1 });
        let open: Vec<_> = store.open_work_requests(Some("o")).unwrap().into_iter().map(|r| r.id).collect();
        assert_eq!(open, ["undelivered", "after"]);
        assert_eq!(store.recovery("o").unwrap().unwrap().acked_at, Some(60));
    }
}
