//! "Clean up finished (N)": the Fleet toolbar's equivalent of a default
//! `ninox reap` for every orchestrator, behind a confirm. Runs
//! `Engine::reap_workers` with `ReapSelection::Finished` and no force, so
//! running and interrupted workers are never touched.

use std::collections::{BTreeMap, HashMap};

use iced::widget::{column, container, row, scrollable, text, Space};
use iced::{Alignment, Background, Border, Color, Element, Length};
use ninox_core::types::{Orchestrator, Session, SessionId};

use super::restore_modal::modal_button;
use super::shell::tokens;
use crate::app::Message;
use crate::style::SANS;

/// Local workers a default reap would clean up, by name. Remote workers are
/// left out: a reap runs against this machine's store and worktrees.
pub fn reapable<'a>(sessions: &'a HashMap<SessionId, Session>, orchestrators: &[Orchestrator]) -> Vec<&'a Session> {
    let mut out: Vec<&Session> = sessions
        .values()
        .filter(|s| s.orchestrator_id.is_some() && s.machine_id.is_none())
        .filter(|s| !orchestrators.iter().any(|o| o.id == s.id))
        .filter(|s| ninox_core::events::awaits_reap(s))
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
    out
}

#[derive(Debug, Clone, Default)]
pub struct ReapModal {
    /// Snapshot at open: (orchestrator id, worker id, worker name).
    pub workers: Vec<(String, String, String)>,
    pub running: bool,
    pub outcome: Option<Result<String, String>>,
}

impl ReapModal {
    pub fn open(sessions: &HashMap<SessionId, Session>, orchestrators: &[Orchestrator]) -> Self {
        let workers = reapable(sessions, orchestrators)
            .into_iter()
            .filter_map(|s| Some((s.orchestrator_id.clone()?, s.id.clone(), s.name.clone())))
            .collect();
        Self { workers, ..Default::default() }
    }

    /// The confirmed worker ids, grouped by orchestrator (reaps are scoped
    /// to one orchestrator's workers).
    pub fn by_orchestrator(&self) -> BTreeMap<String, Vec<String>> {
        let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (orch, id, _) in &self.workers {
            out.entry(orch.clone()).or_default().push(id.clone());
        }
        out
    }

    pub fn confirm_text(&self) -> String {
        let n = self.workers.len();
        format!("Clean up {n} finished worker{}?", if n == 1 { "" } else { "s" })
    }
}

pub fn outcome_text(reaped: usize) -> String {
    match reaped {
        0 => "Nothing needed cleaning up.".to_string(),
        1 => "Cleaned up 1 finished worker.".to_string(),
        n => format!("Cleaned up {n} finished workers."),
    }
}

fn body(modal: &ReapModal) -> Element<'_, Message> {
    let mut rows = column![
        text(modal.confirm_text()).size(13).font(SANS).color(tokens::TEXT),
        text("Their worktrees are removed (one with uncommitted changes is kept); the cards stay until the retention window ends. Running and interrupted workers are never touched.")
            .size(11.5)
            .font(SANS)
            .color(tokens::DIM),
        Space::with_height(4),
    ]
    .spacing(8);
    for (_, _, name) in &modal.workers {
        rows = rows.push(text(name).size(12.5).font(SANS).color(tokens::DIM));
    }
    scrollable(rows).height(Length::Shrink).into()
}

fn footer(modal: &ReapModal) -> Element<'_, Message> {
    if let Some(outcome) = &modal.outcome {
        let (msg, color) = match outcome {
            Ok(msg) => (msg.clone(), tokens::GREEN),
            Err(e) => (format!("Clean-up failed: {e}"), tokens::RED),
        };
        return row![
            text(msg).size(12).font(SANS).color(color).width(Length::Fill),
            modal_button("Close".into(), tokens::TEXT, false, Some(Message::ReapFinishedClose)),
        ]
        .spacing(12)
        .align_y(Alignment::Center)
        .into();
    }
    let idle = !modal.running;
    row![
        Space::with_width(Length::Fill),
        modal_button("Cancel".into(), tokens::TEXT, false, idle.then_some(Message::ReapFinishedClose)),
        modal_button(
            if modal.running { "Cleaning up…".into() } else { format!("Clean up ({})", modal.workers.len()) },
            tokens::TEAL,
            true,
            (idle && !modal.workers.is_empty()).then_some(Message::ReapFinishedConfirm),
        ),
    ]
    .spacing(10)
    .align_y(Alignment::Center)
    .into()
}

pub fn reap_modal(modal: &ReapModal) -> Element<'_, Message> {
    let header = container(text("Clean up finished workers").size(15).font(SANS).color(tokens::TEXT))
        .padding([16, 20])
        .width(Length::Fill)
        .style(|_| container::Style { background: Some(Background::Color(tokens::PANEL2)), ..Default::default() });
    let card = container(column![
        header,
        container(body(modal)).padding([16, 20]).max_height(420.0),
        container(footer(modal)).padding([12, 20]),
    ])
    .width(Length::Fixed(520.0))
    .style(|_| container::Style {
        background: Some(Background::Color(tokens::PANEL)),
        border: Border { color: tokens::BORDER, width: 1.0, radius: 8.0.into() },
        ..Default::default()
    });
    container(card)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .style(|_| container::Style { background: Some(Background::Color(Color { a: 0.6, ..Color::BLACK })), ..Default::default() })
        .into()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ninox_core::types::{ActivityState, SessionStatus};

    pub(crate) fn worker(id: &str, orch: &str, status: SessionStatus) -> Session {
        Session {
            id: id.into(), orchestrator_id: Some(orch.into()), name: format!("{id}-name"),
            repo: "o/r".into(), status, agent_type: "claude-code".into(),
            cost_usd: 0.0, started_at: 0, pr_number: None, pr_id: None,
            workspace_path: None, pid: None, model: None, context_tokens: None,
            brain_reads: Vec::new(), brain_write: None, context_used_pct: None, context_total_tokens: None,
            context_window_size: None, claude_session_id: None,
            summary: None, terminal_at: None, gate_status: None, merged_at: None,
            activity: ActivityState::default(), activity_note: None, activity_since: None,
            machine_id: None, worker_type: None,
            success_criteria: Vec::new(), completion_outcome: None, self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        }
    }

    fn fleet() -> (HashMap<SessionId, Session>, Vec<Orchestrator>) {
        use SessionStatus::*;
        let mut cleaned = worker("cleaned", "o1", Done);
        cleaned.cleanup_at = Some(1);
        let mut remote = worker("remote", "o1", Done);
        remote.machine_id = Some("mac-studio".into());
        let mut orch_row = worker("o2", "o2", Terminated);
        orch_row.orchestrator_id = Some("o2".into());
        let mut standalone = worker("solo", "o1", Done);
        standalone.orchestrator_id = None;
        let sessions = [
            worker("done", "o1", Done),
            worker("term", "o2", Terminated),
            worker("intr", "o1", Interrupted),
            worker("live", "o1", Working),
            cleaned, remote, orch_row, standalone,
        ];
        let orchestrators = ["o1", "o2"].map(|id| Orchestrator { id: id.into(), name: id.into(), created_at: 0 }).to_vec();
        (sessions.into_iter().map(|s| (s.id.clone(), s)).collect(), orchestrators)
    }

    #[test]
    fn only_uncleaned_local_finished_workers_are_reapable() {
        let (sessions, orchestrators) = fleet();
        let ids: Vec<&str> = reapable(&sessions, &orchestrators).iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["done", "term"]);
    }

    #[test]
    fn the_modal_reaps_each_orchestrator_once() {
        let (sessions, orchestrators) = fleet();
        let m = ReapModal::open(&sessions, &orchestrators);
        let batches: Vec<(String, Vec<String>)> = m.by_orchestrator().into_iter().collect();
        assert_eq!(batches, [("o1".to_string(), vec!["done".to_string()]), ("o2".to_string(), vec!["term".to_string()])]);
        assert_eq!(m.confirm_text(), "Clean up 2 finished workers?");
    }

    #[test]
    fn outcome_text_counts() {
        assert_eq!(outcome_text(0), "Nothing needed cleaning up.");
        assert_eq!(outcome_text(1), "Cleaned up 1 finished worker.");
        assert_eq!(outcome_text(3), "Cleaned up 3 finished workers.");
    }
}
