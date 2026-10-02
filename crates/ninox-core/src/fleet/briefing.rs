//! Recovery briefings (spec §5.3): the first input a restored orchestrator
//! (or worker) receives, generated purely from a [`FleetSnapshot`] so the
//! same text can be previewed (`ninox fleet brief`, `--dry-run`) and sent.

use super::cause::InterruptCause;
use super::snapshot::{FleetMember, FleetSnapshot, Role};
use super::RestoreOutcome;
use crate::types::{CIStatus, SessionStatus};
use std::fmt::Write as _;

/// Formats an epoch-millisecond timestamp for display.
pub type TimeFmt<'a> = &'a dyn Fn(i64) -> String;

pub const ORCHESTRATOR_HEADER: &str = "[Ninox fleet recovery briefing]";
pub const WORKER_HEADER: &str = "[Ninox recovery note]";

fn interruption_phrase(at: Option<i64>, cause: Option<&str>, fmt: TimeFmt) -> String {
    let cause = cause.map(InterruptCause::parse).unwrap_or(InterruptCause::Unknown);
    match at {
        Some(t) => format!("You were interrupted at {} ({})", fmt(t), cause.describe()),
        None => "You were interrupted".to_string(),
    }
}

fn ci_phrase(ci: &CIStatus) -> Option<String> {
    if ci.total == 0 {
        None
    } else if ci.failing > 0 {
        Some(format!("CI is failing ({}/{} checks failing)", ci.failing, ci.total))
    } else if ci.pending > 0 {
        Some(format!("CI is pending ({}/{} passing so far)", ci.passing, ci.total))
    } else if ci.passing == ci.total {
        Some("CI is green".to_string())
    } else {
        None
    }
}

/// "its PR #41 is open and CI is green" — `None` without a PR.
fn pr_phrase(m: &FleetMember) -> Option<String> {
    let number = m.session.pr_number.or(m.pr.as_ref().map(|p| p.number))?;
    let state = if m.session.merged_at.is_some() {
        "merged".to_string()
    } else {
        match m.effective_status() {
            SessionStatus::Mergeable     => "open and mergeable".into(),
            SessionStatus::ReviewPending => "open and awaiting review".into(),
            SessionStatus::Done          => "closed out".into(),
            _                            => "open".into(),
        }
    };
    let mut s = format!("PR #{number} is {state}");
    if m.session.merged_at.is_none() {
        if let Some(ci) = m.ci.as_ref().and_then(ci_phrase) {
            let _ = write!(s, " and {ci}");
        } else if *m.effective_status() == SessionStatus::CiFailed {
            s.push_str(" and CI is failing");
        }
    }
    Some(s)
}

fn task_of(m: &FleetMember) -> Option<String> {
    m.session.summary.clone()
        .or_else(|| m.record.task_brief.as_deref().and_then(|b| b.lines().find(|l| !l.trim().is_empty())).map(str::to_string))
}

fn is_dirty(m: &FleetMember) -> bool {
    m.workspace.dirty == Some(true)
}

/// One bullet describing a worker's post-restore state.
pub fn worker_line(m: &FleetMember) -> String {
    let mut line = format!("- `{}`", m.id());
    let restored_live = matches!(m.last_restore_outcome(), Some(RestoreOutcome::Resumed | RestoreOutcome::Fresh | RestoreOutcome::AlreadyLive));
    match m.last_restore_outcome() {
        Some(RestoreOutcome::Resumed) => line.push_str(" was resumed and is continuing"),
        Some(RestoreOutcome::Fresh) => line.push_str(
            " was restarted fresh (its harness cannot resume a conversation) and re-briefed with its task",
        ),
        Some(RestoreOutcome::AlreadyLive) => line.push_str(" was already running"),
        Some(RestoreOutcome::Failed) => {
            let note = m.record.restore_note.as_deref().unwrap_or("unknown error");
            let _ = write!(line, " could NOT be restored ({note})");
        }
        None if m.awaiting_restore() => {
            let blocking: Vec<String> = m.blocking_anomalies().map(|a| a.describe()).collect();
            if blocking.is_empty() {
                line.push_str(" is still interrupted (not part of this restore)");
            } else {
                let _ = write!(line, " was NOT restored: {}", blocking.join("; "));
            }
        }
        None => match m.session.status {
            SessionStatus::Done => line.push_str(" had finished"),
            SessionStatus::Terminated => line.push_str(" is gone (terminated)"),
            _ => line.push_str(" was not interrupted and is still running"),
        },
    }
    if let Some(task) = task_of(m) {
        let _ = write!(line, " — task: {task}");
    }
    if let Some(pr) = pr_phrase(m) {
        let _ = write!(line, "; {pr}");
    }
    if is_dirty(m) {
        let path = m.session.workspace_path.as_deref().unwrap_or("?");
        if restored_live {
            let _ = write!(line, "; uncommitted changes in `{path}`");
        } else {
            let _ = write!(line, "; Retained with uncommitted changes in `{path}`");
        }
    }
    if m.pending_inbox > 0 {
        let _ = write!(line, "; {} undelivered inbox message(s) will drain on its next turn", m.pending_inbox);
    }
    line.push('.');
    line
}

/// The orchestrator briefing; `None` if `orchestrator_id` isn't an
/// orchestrator in the snapshot.
pub fn orchestrator_briefing(snap: &FleetSnapshot, orchestrator_id: &str, fmt: TimeFmt) -> Option<String> {
    let orch = snap.member(orchestrator_id).filter(|m| m.role == Role::Orchestrator)?;
    let mut out = String::new();
    out.push_str(ORCHESTRATOR_HEADER);
    out.push_str("\n\n");

    let (at, cause) = snap.interruption_of(orchestrator_id).unzip();
    out.push_str(&interruption_phrase(at, cause.flatten().as_deref(), fmt));
    out.push_str(". ");
    match orch.last_restore_outcome() {
        Some(RestoreOutcome::Fresh) => out.push_str(
            "Ninox restarted you fresh — your previous conversation could not be resumed, \
             so this briefing is the context you have.",
        ),
        Some(RestoreOutcome::Resumed) => out.push_str("Ninox resumed you with your conversation intact."),
        _ => out.push_str("Ninox has restored your fleet."),
    }
    out.push_str("\n\n");

    let workers: Vec<&FleetMember> = snap.workers_of(orchestrator_id).collect();
    if workers.is_empty() {
        out.push_str("You have no workers.\n");
    } else {
        out.push_str("Workers:\n");
        for w in &workers {
            out.push_str(&worker_line(w));
            out.push('\n');
        }
    }

    let requests: Vec<_> = snap.requests_for(orchestrator_id).collect();
    if !requests.is_empty() {
        let _ = writeln!(out, "\n{} request-work item(s) are pending:", requests.len());
        for r in requests {
            let state = if r.delivered_at.is_some() {
                "delivered before the interruption"
            } else {
                "not delivered to you before this briefing"
            };
            let _ = writeln!(out, "- `{}` from `{}`: {} ({state})", r.id, r.from_session, r.body.trim());
        }
    }

    if orch.pending_inbox > 0 {
        let _ = writeln!(
            out,
            "\nYou have {} undelivered inbox message(s); they will be delivered after this briefing.",
            orch.pending_inbox,
        );
    }

    out.push_str(
        "\nResume coordination from here; do not re-inspect workers unless something above \
         looks wrong. Once you have taken stock, run `ninox fleet ack` to mark recovery complete.",
    );
    Some(out)
}

fn workspace_state(m: &FleetMember) -> String {
    let path = m.session.workspace_path.as_deref().unwrap_or("?");
    let mut s = format!("Your worktree `{path}`");
    if let Some(b) = &m.workspace.branch {
        let _ = write!(s, " is on branch `{b}`");
    }
    match m.workspace.dirty {
        Some(true)  => s.push_str(" with uncommitted changes"),
        Some(false) => s.push_str(" with no uncommitted changes"),
        None        => {}
    }
    s.push('.');
    if let Some(pr) = pr_phrase(m) {
        let _ = write!(s, " Your {pr}.");
    }
    s
}

/// The note a restored worker (or standalone session) receives. Resumed
/// sessions get a short nudge; fresh restarts get their task brief and
/// state, since their conversation is gone.
pub fn worker_briefing(snap: &FleetSnapshot, session_id: &str, fmt: TimeFmt) -> Option<String> {
    let m = snap.member(session_id).filter(|m| m.role != Role::Orchestrator)?;
    let orch = m.session.orchestrator_id.as_deref();
    let when = interruption_phrase(m.record.interrupted_at, m.record.interrupt_cause.as_deref(), fmt);
    let report = match orch {
        Some(o) => format!(" If something looks wrong, tell your orchestrator: `ninox send {o} \"<message>\"`."),
        None => String::new(),
    };
    let mut out = String::new();
    match m.last_restore_outcome() {
        Some(RestoreOutcome::Fresh) => {
            out.push_str(WORKER_HEADER);
            let _ = write!(
                out,
                "\n\n{when}, and this session could not be resumed: the conversation is lost, \
                 but the work in your worktree is not.\n\n",
            );
            match orch {
                Some(o) => { let _ = writeln!(out, "Your task, as issued by orchestrator `{o}`:\n"); }
                None => out.push_str("Your task:\n\n"),
            }
            match m.record.task_brief.as_deref().or(m.session.summary.as_deref()) {
                Some(brief) => { out.push_str(brief.trim()); out.push('\n'); }
                None if orch.is_some() => out.push_str("(no task brief was recorded — ask your orchestrator for it)\n"),
                None => out.push_str("(no task brief was recorded)\n"),
            }
            let _ = write!(
                out,
                "\n{}\n\nInspect the worktree (`git status`, `git log`) and continue the task from its \
                 current state; do not start over.{report}",
                workspace_state(m),
            );
            if let Some(o) = orch {
                let _ = write!(
                    out,
                    "\n\nNinox session `{}` · orchestrator `{o}`. Hand off out-of-scope work with \
                     `ninox request-work \"<description>\"`; run `ninox capabilities --worker` to list \
                     what ninox can do.",
                    m.id(),
                );
            }
        }
        Some(RestoreOutcome::Resumed) => {
            out.push_str(WORKER_HEADER);
            let _ = write!(
                out,
                " {when}, and this session has been resumed with its conversation intact. {} \
                 Continue your task from where you left off.{report}",
                workspace_state(m),
            );
        }
        _ => return None,
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::super::snapshot::test_support::*;
    use super::*;
    use crate::store::WorkRequestRow;

    fn fmt(t: i64) -> String {
        format!("T{t}")
    }

    fn restored(mut m: FleetMember, outcome: RestoreOutcome) -> FleetMember {
        m.record.restored_at = Some(m.record.interrupted_at.unwrap_or(0) + 1);
        m.record.restore_mode = Some(outcome.as_str().into());
        if outcome != RestoreOutcome::Failed {
            m.session.status = SessionStatus::Working;
        }
        m
    }

    fn worker(id: &str, start: i64) -> FleetMember {
        interrupted(member(session(id, Some("o"), SessionStatus::Working, start), Role::Worker), 100, SessionStatus::Working)
    }

    fn spec_fleet() -> FleetSnapshot {
        let o = restored(
            interrupted(member(session("o", None, SessionStatus::Working, 0), Role::Orchestrator), 100, SessionStatus::Working),
            RestoreOutcome::Resumed,
        );
        let a = restored(worker("a", 1), RestoreOutcome::Resumed);
        let b = restored(worker("b", 2), RestoreOutcome::Resumed);
        let mut c = restored(worker("c", 3), RestoreOutcome::Resumed);
        c.record.last_status = Some(SessionStatus::PrOpen);
        c.session.pr_number = Some(41);
        c.ci = Some(CIStatus { pr_id: 1, total: 4, passing: 4, failing: 0, pending: 0 });
        let mut d = worker("d", 4);
        d.workspace.exists = false; // blocked → skipped
        let mut e = member(session("e", Some("o"), SessionStatus::Done, 5), Role::Worker);
        e.workspace.dirty = Some(true);
        let requests = vec![
            WorkRequestRow {
                id: "wr-1".into(), from_session: "a".into(), orchestrator_id: Some("o".into()),
                body: "split the parser".into(), created_at: 50, delivered_at: Some(51), resolved_at: None,
            },
            WorkRequestRow {
                id: "wr-2".into(), from_session: "b".into(), orchestrator_id: Some("o".into()),
                body: "add docs".into(), created_at: 60, delivered_at: None, resolved_at: None,
            },
            WorkRequestRow {
                id: "wr-x".into(), from_session: "z".into(), orchestrator_id: Some("other".into()),
                body: "not ours".into(), created_at: 60, delivered_at: None, resolved_at: None,
            },
        ];
        let mut snap = FleetSnapshot::assemble(vec![o, a, b, c, d, e], requests, vec![], 0);
        snap.members.iter_mut().find(|m| m.id() == "o").unwrap().pending_inbox = 2;
        snap
    }

    #[test]
    fn orchestrator_briefing_covers_spec_example() {
        let text = orchestrator_briefing(&spec_fleet(), "o", &fmt).unwrap();
        assert!(text.starts_with(ORCHESTRATOR_HEADER));
        assert!(text.contains("You were interrupted at T100 (machine reboot)"), "{text}");
        assert!(text.contains("resumed you with your conversation intact"));
        assert!(text.contains("- `a` was resumed and is continuing — task: task of a."), "{text}");
        assert!(text.contains("`c` was resumed and is continuing — task: task of c; PR #41 is open and CI is green."), "{text}");
        assert!(text.contains("`d` was NOT restored: workspace `/ws/d` is missing"), "{text}");
        assert!(text.contains("`e` had finished — task: task of e; Retained with uncommitted changes in `/ws/e`."), "{text}");
        assert!(text.contains("2 request-work item(s) are pending"));
        assert!(text.contains("`wr-1` from `a`: split the parser (delivered before the interruption)"));
        assert!(text.contains("`wr-2` from `b`: add docs (not delivered to you before this briefing)"));
        assert!(!text.contains("not ours"));
        assert!(text.contains("2 undelivered inbox message(s)"));
        assert!(text.contains("do not re-inspect workers unless something above looks wrong"));
        assert!(text.trim_end().ends_with("run `ninox fleet ack` to mark recovery complete."));
    }

    #[test]
    fn fresh_orchestrator_is_told_its_conversation_is_gone() {
        let o = restored(
            interrupted(member(session("o", None, SessionStatus::Working, 0), Role::Orchestrator), 100, SessionStatus::Working),
            RestoreOutcome::Fresh,
        );
        let snap = FleetSnapshot::assemble(vec![o], vec![], vec![], 0);
        let text = orchestrator_briefing(&snap, "o", &fmt).unwrap();
        assert!(text.contains("restarted you fresh"));
        assert!(text.contains("You have no workers."));
    }

    #[test]
    fn failed_restore_is_reported_with_its_note() {
        let mut snap = spec_fleet();
        let a = snap.members.iter_mut().find(|m| m.id() == "a").unwrap();
        a.record.restore_mode = Some("failed".into());
        a.record.restore_note = Some("tmux create failed".into());
        let text = orchestrator_briefing(&snap, "o", &fmt).unwrap();
        assert!(text.contains("`a` could NOT be restored (tmux create failed)"), "{text}");
    }

    #[test]
    fn briefing_is_none_for_non_orchestrators() {
        assert!(orchestrator_briefing(&spec_fleet(), "a", &fmt).is_none());
        assert!(orchestrator_briefing(&spec_fleet(), "missing", &fmt).is_none());
    }

    #[test]
    fn ci_phrases() {
        let ci = |t, p, f, pe| CIStatus { pr_id: 0, total: t, passing: p, failing: f, pending: pe };
        assert_eq!(ci_phrase(&ci(0, 0, 0, 0)), None);
        assert_eq!(ci_phrase(&ci(3, 3, 0, 0)).as_deref(), Some("CI is green"));
        assert!(ci_phrase(&ci(3, 1, 1, 1)).unwrap().contains("failing (1/3"));
        assert!(ci_phrase(&ci(3, 1, 0, 2)).unwrap().contains("pending"));
    }

    #[test]
    fn merged_pr_drops_ci() {
        let mut m = restored(worker("m", 1), RestoreOutcome::Resumed);
        m.session.pr_number = Some(7);
        m.session.merged_at = Some(1);
        m.ci = Some(CIStatus { pr_id: 1, total: 1, passing: 0, failing: 1, pending: 0 });
        assert_eq!(pr_phrase(&m).as_deref(), Some("PR #7 is merged"));
    }

    #[test]
    fn resumed_worker_gets_a_short_nudge() {
        let mut snap = spec_fleet();
        snap.members.iter_mut().find(|m| m.id() == "a").unwrap().workspace.branch = Some("a".into());
        let text = worker_briefing(&snap, "a", &fmt).unwrap();
        assert!(text.starts_with(WORKER_HEADER));
        assert!(text.contains("resumed with its conversation intact"));
        assert!(text.contains("is on branch `a` with no uncommitted changes"), "{text}");
        assert!(text.contains("Continue your task from where you left off"));
        assert!(text.contains("ninox send o"));
    }

    #[test]
    fn fresh_worker_gets_its_task_brief_and_state() {
        let mut w = restored(worker("w", 1), RestoreOutcome::Fresh);
        w.record.task_brief = Some("Implement the frobnicator.\n\nDetails here.".into());
        w.workspace.dirty = Some(true);
        let o = member(session("o", None, SessionStatus::Working, 0), Role::Orchestrator);
        let snap = FleetSnapshot::assemble(vec![o, w], vec![], vec![], 0);
        let text = worker_briefing(&snap, "w", &fmt).unwrap();
        assert!(text.contains("could not be resumed: the conversation is lost"));
        assert!(text.contains("as issued by orchestrator `o`"));
        assert!(text.contains("Implement the frobnicator.\n\nDetails here."));
        assert!(text.contains("with uncommitted changes"));
        assert!(text.contains("do not start over"));
        assert!(text.contains("ninox request-work"));
    }

    #[test]
    fn unrestored_worker_has_no_briefing() {
        assert!(worker_briefing(&spec_fleet(), "d", &fmt).is_none());
        assert!(worker_briefing(&spec_fleet(), "o", &fmt).is_none());
    }

    #[test]
    fn standalone_fresh_restart_omits_orchestrator() {
        let mut s = restored(
            interrupted(member(session("s", None, SessionStatus::Working, 0), Role::Standalone), 100, SessionStatus::Working),
            RestoreOutcome::Fresh,
        );
        s.record.task_brief = None;
        s.session.summary = None;
        let snap = FleetSnapshot::assemble(vec![s], vec![], vec![], 0);
        let text = worker_briefing(&snap, "s", &fmt).unwrap();
        assert!(text.contains("Your task:\n\n(no task brief was recorded"));
        assert!(!text.contains("orchestrator"));
    }
}
