//! "Restore fleet": the interrupted sessions a restore would bring back,
//! chosen individually or all at once, behind a confirm. The restore itself
//! runs as a detached `ninox fleet restore` (`fleet::spawn_detached_restore`).

use std::collections::BTreeSet;

use iced::widget::{button, checkbox, column, container, row, scrollable, text, Space};
use iced::{Alignment, Background, Border, Color, Element, Length};
use ninox_core::fleet::{RestoreMode, RestorePlan, RestoreStep, Role};

use super::shell::tokens;
use crate::app::Message;
use crate::style::{MONO, SANS};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreScope {
    All,
    Selected,
}

#[derive(Debug, Clone, Default)]
pub struct RestoreModal {
    /// `None` while the plan loads (it probes workspaces off the UI thread).
    pub plan: Option<Result<RestorePlan, String>>,
    pub selected: BTreeSet<String>,
    /// The scope awaiting the inline confirm.
    pub confirm: Option<RestoreScope>,
    /// Set once the detached restore was started (or failed to start).
    pub outcome: Option<Result<String, String>>,
    /// A restore already holds the lease (e.g. "since 10:02"): a second
    /// would be refused by the CLI, out of sight in its log.
    pub running: Option<String>,
}

impl RestoreModal {
    /// Every restorable session starts selected.
    pub fn loaded(plan: Result<RestorePlan, String>) -> Self {
        let selected = plan.as_ref().map(|p| p.steps().map(|s| s.session_id.clone()).collect()).unwrap_or_default();
        Self { plan: Some(plan), selected, ..Default::default() }
    }

    pub fn toggle(&mut self, id: &str) {
        if !self.selected.remove(id) {
            self.selected.insert(id.to_string());
        }
    }

    /// Whether "Restore all" has anything to do (steps or undelivered
    /// briefings) and nothing else is restoring.
    pub fn can_restore_all(&self) -> bool {
        self.running.is_none() && matches!(&self.plan, Some(Ok(plan)) if !plan.is_empty())
    }

    /// Selected steps, in plan order.
    pub fn selected_steps(&self) -> Vec<String> {
        match &self.plan {
            Some(Ok(plan)) => plan.steps().map(|s| s.session_id.clone()).filter(|id| self.selected.contains(id)).collect(),
            _ => Vec::new(),
        }
    }

    /// Ask to confirm `scope`; a selective restore needs something selected.
    pub fn request(&mut self, scope: RestoreScope) {
        let ok = match scope {
            RestoreScope::All => self.can_restore_all(),
            RestoreScope::Selected => self.running.is_none() && !self.selected_steps().is_empty(),
        };
        if ok {
            self.confirm = Some(scope);
        }
    }

    /// The `--only` ids for `scope`, in plan order; empty restores
    /// everything. A selection of every step runs as a full restore too:
    /// `--only` would leave undelivered briefings behind.
    pub fn only(&self, scope: RestoreScope) -> Vec<String> {
        let steps = match (scope, &self.plan) {
            (RestoreScope::Selected, Some(Ok(plan))) => plan.steps().count(),
            _ => return Vec::new(),
        };
        let selected = self.selected_steps();
        if selected.len() == steps { Vec::new() } else { selected }
    }

    pub fn confirm_text(&self, scope: RestoreScope) -> String {
        let n = match (scope, &self.plan) {
            (RestoreScope::All, Some(Ok(plan))) => plan.steps().count(),
            (RestoreScope::Selected, _) => self.selected_steps().len(),
            _ => 0,
        };
        let s = if n == 1 { "" } else { "s" };
        format!("Restore {n} session{s}? Workers first, then orchestrators.")
    }
}

fn role_word(role: Role) -> &'static str {
    match role {
        Role::Orchestrator => "orchestrator",
        Role::Worker => "worker",
        Role::Standalone => "standalone",
    }
}

fn mode_word(mode: RestoreMode) -> &'static str {
    match mode {
        RestoreMode::Resume => "resume",
        RestoreMode::Fresh => "fresh restart + briefing",
    }
}

pub(super) fn modal_button<'a>(label: String, color: Color, filled: bool, on_press: Option<Message>) -> Element<'a, Message> {
    let enabled = on_press.is_some();
    button(text(label).font(SANS).size(11.5).color(if filled { tokens::BG } else { color }))
        .on_press_maybe(on_press)
        .padding([6, 12])
        .style(move |_t, status| {
            let hovered = enabled && matches!(status, button::Status::Hovered);
            let fill = match (filled, hovered) {
                (true, false) => color,
                (true, true) => tokens::TEXT,
                (false, true) => tokens::PANEL2,
                (false, false) => Color::TRANSPARENT,
            };
            button::Style {
                background: Some(Background::Color(if enabled { fill } else { Color { a: 0.4, ..fill } })),
                text_color: color,
                border: Border { color: if filled { color } else { tokens::BORDER }, width: 1.0, radius: 6.0.into() },
                ..Default::default()
            }
        })
        .into()
}

fn step_row<'a>(step: &'a RestoreStep, checked: bool) -> Element<'a, Message> {
    let id = step.session_id.clone();
    row![
        checkbox("", checked).on_toggle(move |_| Message::RestoreFleetToggle(id.clone())).size(14),
        text(&step.name).size(13).font(SANS).color(tokens::TEXT),
        text(role_word(step.role)).size(11).font(SANS).color(tokens::DIM2),
        Space::with_width(Length::Fill),
        text(mode_word(step.mode)).size(11).font(MONO).color(tokens::DIM),
    ]
    .spacing(10)
    .align_y(Alignment::Center)
    .into()
}

fn body<'a>(modal: &'a RestoreModal) -> Element<'a, Message> {
    let plan = match &modal.plan {
        None => return text("Reading the fleet…").size(12.5).font(SANS).color(tokens::DIM).into(),
        Some(Err(e)) => return text(format!("Couldn't read the fleet: {e}")).size(12.5).font(SANS).color(tokens::RED).into(),
        Some(Ok(plan)) => plan,
    };
    if plan.is_empty() && plan.skipped.is_empty() {
        return text("Nothing to restore.").size(12.5).font(SANS).color(tokens::DIM).into();
    }
    let mut rows = column![].spacing(8);
    for step in plan.steps() {
        rows = rows.push(step_row(step, modal.selected.contains(&step.session_id)));
    }
    if !plan.pending_briefings.is_empty() {
        let n = plan.pending_briefings.len();
        rows = rows.push(
            text(format!("{n} undelivered recovery briefing{} go out with a full restore.", if n == 1 { "" } else { "s" }))
                .size(11.5)
                .font(SANS)
                .color(tokens::DIM),
        );
    }
    if !plan.skipped.is_empty() {
        rows = rows.push(Space::with_height(6)).push(text("Can't restore").size(10.5).font(SANS).color(tokens::DIM2));
        for skipped in &plan.skipped {
            let why: Vec<String> = skipped.anomalies.iter().map(|a| a.describe()).collect();
            rows = rows.push(
                column![
                    text(&skipped.name).size(12.5).font(SANS).color(tokens::DIM),
                    text(why.join("; ")).size(11).font(SANS).color(tokens::DIM2),
                ]
                .spacing(2),
            );
        }
    }
    scrollable(rows).height(Length::Shrink).into()
}

fn footer<'a>(modal: &'a RestoreModal) -> Element<'a, Message> {
    if let Some(outcome) = &modal.outcome {
        let (msg, color) = match outcome {
            Ok(msg) => (msg.clone(), tokens::GREEN),
            Err(e) => (format!("Restore failed to start: {e}"), tokens::RED),
        };
        return row![
            text(msg).size(12).font(SANS).color(color).width(Length::Fill),
            modal_button("Close".into(), tokens::TEXT, false, Some(Message::RestoreFleetClose)),
        ]
        .spacing(12)
        .align_y(Alignment::Center)
        .into();
    }
    if let Some(scope) = modal.confirm {
        return row![
            text(modal.confirm_text(scope)).size(12.5).font(SANS).color(tokens::TEXT).width(Length::Fill),
            modal_button("Cancel".into(), tokens::TEXT, false, Some(Message::RestoreFleetCancelConfirm)),
            modal_button("Restore".into(), tokens::TEAL, true, Some(Message::RestoreFleetConfirm)),
        ]
        .spacing(10)
        .align_y(Alignment::Center)
        .into();
    }
    if let Some(since) = &modal.running {
        return row![
            text(format!("A restore is already running ({since}); its progress is in daemon.log."))
                .size(12)
                .font(SANS)
                .color(tokens::AMBER)
                .width(Length::Fill),
            modal_button("Close".into(), tokens::TEXT, false, Some(Message::RestoreFleetClose)),
        ]
        .spacing(12)
        .align_y(Alignment::Center)
        .into();
    }
    let restorable = modal.can_restore_all();
    let selected = modal.selected_steps().len();
    row![
        Space::with_width(Length::Fill),
        modal_button("Close".into(), tokens::TEXT, false, Some(Message::RestoreFleetClose)),
        modal_button(
            format!("Restore selected ({selected})"),
            tokens::TEAL,
            false,
            (restorable && selected > 0).then_some(Message::RestoreFleetRequest(RestoreScope::Selected)),
        ),
        modal_button("Restore all".into(), tokens::TEAL, true, restorable.then_some(Message::RestoreFleetRequest(RestoreScope::All))),
    ]
    .spacing(10)
    .align_y(Alignment::Center)
    .into()
}

pub fn restore_modal(modal: &RestoreModal) -> Element<'_, Message> {
    let header = container(text("Restore fleet").size(15).font(SANS).color(tokens::TEXT))
        .padding([16, 20])
        .width(Length::Fill)
        .style(|_| container::Style { background: Some(Background::Color(tokens::PANEL2)), ..Default::default() });
    let card = container(column![
        header,
        container(body(modal)).padding([16, 20]).max_height(420.0),
        container(footer(modal)).padding([12, 20]),
    ])
    .width(Length::Fixed(560.0))
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
mod tests {
    use super::*;

    fn step(id: &str, role: Role) -> RestoreStep {
        RestoreStep { session_id: id.into(), name: format!("{id}-name"), role, mode: RestoreMode::Resume }
    }

    fn modal() -> RestoreModal {
        RestoreModal::loaded(Ok(RestorePlan {
            workers: vec![step("w1", Role::Worker), step("w2", Role::Worker)],
            orchestrators: vec![step("o1", Role::Orchestrator)],
            ..Default::default()
        }))
    }

    #[test]
    fn every_restorable_session_starts_selected() {
        let m = modal();
        assert_eq!(m.selected.iter().map(String::as_str).collect::<Vec<_>>(), ["o1", "w1", "w2"]);
        assert_eq!(m.confirm, None);
    }

    #[test]
    fn a_selective_restore_passes_the_chosen_ids_in_plan_order() {
        let mut m = modal();
        m.toggle("w1");
        assert_eq!(m.only(RestoreScope::Selected), ["w2", "o1"]);
        m.toggle("w1");
        assert!(m.only(RestoreScope::All).is_empty(), "all restores everything, briefings included");
    }

    /// `--only` drops pending briefings, so a selection of everything must
    /// run as a full restore.
    #[test]
    fn selecting_everything_is_a_full_restore() {
        let m = modal();
        assert!(m.only(RestoreScope::Selected).is_empty());
        assert_eq!(m.confirm_text(RestoreScope::Selected), "Restore 3 sessions? Workers first, then orchestrators.");
    }

    #[test]
    fn a_plan_of_only_undelivered_briefings_can_still_be_restored() {
        let mut m = RestoreModal::loaded(Ok(RestorePlan { pending_briefings: vec!["o1".into()], ..Default::default() }));
        assert!(m.can_restore_all());
        m.request(RestoreScope::All);
        assert_eq!(m.confirm, Some(RestoreScope::All));
    }

    #[test]
    fn nothing_can_start_while_a_restore_is_running() {
        let mut m = modal();
        m.running = Some("pid-1-0, since 10:00".into());
        assert!(!m.can_restore_all());
        m.request(RestoreScope::All);
        m.request(RestoreScope::Selected);
        assert_eq!(m.confirm, None);
    }

    #[test]
    fn restoring_a_selection_needs_one() {
        let mut m = modal();
        for id in ["w1", "w2", "o1"] {
            m.toggle(id);
        }
        m.request(RestoreScope::Selected);
        assert_eq!(m.confirm, None);
        m.request(RestoreScope::All);
        assert_eq!(m.confirm, Some(RestoreScope::All));
    }

    #[test]
    fn the_confirm_says_how_many_and_in_what_order() {
        let mut m = modal();
        assert_eq!(m.confirm_text(RestoreScope::All), "Restore 3 sessions? Workers first, then orchestrators.");
        m.toggle("w1");
        m.toggle("w2");
        assert_eq!(m.confirm_text(RestoreScope::Selected), "Restore 1 session? Workers first, then orchestrators.");
    }

    #[test]
    fn a_failed_plan_selects_nothing() {
        let m = RestoreModal::loaded(Err("store locked".into()));
        assert!(m.selected.is_empty());
        assert!(m.only(RestoreScope::Selected).is_empty());
    }
}
