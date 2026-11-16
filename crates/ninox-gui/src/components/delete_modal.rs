//! "Delete…" for a worker, or an orchestrator and everything under it.
//! Shows exactly what goes (`Engine::plan_delete`) before
//! `Engine::execute_delete` runs; worktrees with work on them are kept
//! unless the user ticks the override.

use iced::widget::{button, checkbox, column, container, row, scrollable, text, Space};
use iced::{Alignment, Background, Border, Color, Element, Length};
use ninox_core::events::{DeleteOutcome, DeletePlan, DeleteTarget};
use ninox_core::types::SessionId;
use ninox_ui_shared::delete_view::{
    kill_warning, outcome_summary, override_label, plural, root_target, target_detail, target_name, title, under_root,
};

use super::restore_modal::modal_button;
use super::shell::tokens;
use crate::app::Message;
use crate::style::{SANS, SANS_SEMIBOLD};

#[derive(Debug, Clone)]
pub struct DeleteModal {
    pub root:            SessionId,
    /// `None` while the plan is read (it runs git in every worktree).
    pub plan:            Option<Result<DeletePlan, String>>,
    pub force_worktrees: bool,
    pub expanded:        bool,
    /// What was confirmed, once Delete is pressed: the plan's ids, in the
    /// order they're deleted, and the override.
    pub submitted:       Option<(Vec<SessionId>, bool)>,
    pub results:         Option<Vec<(SessionId, DeleteOutcome)>>,
}

impl DeleteModal {
    pub fn new(root: SessionId) -> Self {
        Self { root, plan: None, force_worktrees: false, expanded: false, submitted: None, results: None }
    }

    pub fn running(&self) -> bool {
        self.submitted.is_some() && self.results.is_none()
    }

    pub fn ready_plan(&self) -> Option<&DeletePlan> {
        self.plan.as_ref().and_then(|p| p.as_ref().ok())
    }
}

fn target_line<'a>(t: &DeleteTarget, force: bool) -> Element<'a, Message> {
    let (detail, at_risk) = target_detail(t, force);
    let tone = match (at_risk, force) {
        (false, _) => tokens::DIM,
        (true, false) => tokens::AMBER_BADGE_FG,
        (true, true) => tokens::RED,
    };
    column![
        text(target_name(t)).size(12.5).font(SANS_SEMIBOLD).color(tokens::TEXT),
        text(detail).size(11.5).font(SANS).color(tone),
    ]
    .spacing(2)
    .into()
}

fn body(modal: &DeleteModal) -> Element<'_, Message> {
    let plan = match &modal.plan {
        None => return text("Checking worktrees…").size(12.5).font(SANS).color(tokens::DIM).into(),
        Some(Err(e)) => return text(format!("Can't delete this session: {e}")).size(12.5).font(SANS).color(tokens::RED).into(),
        Some(Ok(plan)) => plan,
    };
    if let Some(results) = &modal.results {
        let (summary, failed) = outcome_summary(results);
        return text(summary).size(12.5).font(SANS).color(if failed { tokens::RED } else { tokens::GREEN }).into();
    }

    let mut rows = column![].spacing(10);
    if let Some(warning) = kill_warning(plan) {
        rows = rows.push(text(warning).size(12.5).font(SANS_SEMIBOLD).color(tokens::RED));
    }
    if let Some(root) = root_target(plan) {
        rows = rows.push(target_line(root, modal.force_worktrees));
    }
    let under = under_root(plan);
    if !under.is_empty() {
        // The tree's own chevrons: SANS has no glyph for them.
        let arrow = text(if modal.expanded { "▼" } else { "▶" }).size(10).color(tokens::DIM);
        rows = rows.push(
            button(row![arrow, text(format!("{} under it", plural(under.len(), "session", "sessions"))).size(12).font(SANS).color(tokens::TEXT)].spacing(6).align_y(Alignment::Center))
                .on_press(Message::DeleteToggleExpanded)
                .padding(0)
                .style(|_t, _s| button::Style { background: None, ..Default::default() }),
        );
        if modal.expanded {
            let mut list = column![].spacing(8);
            for t in under {
                let indent = 14.0 * t.depth as f32;
                list = list.push(container(target_line(t, modal.force_worktrees)).padding(iced::Padding { top: 0.0, right: 0.0, bottom: 0.0, left: indent }));
            }
            rows = rows.push(list);
        }
    }
    if let Some(label) = override_label(plan) {
        rows = rows.push(Space::with_height(2));
        rows = rows.push(
            checkbox(label, modal.force_worktrees)
                .on_toggle(Message::DeleteToggleForce)
                .size(14)
                .text_size(12)
                .font(SANS),
        );
    }
    scrollable(rows).height(Length::Shrink).into()
}

fn footer(modal: &DeleteModal) -> Element<'_, Message> {
    if modal.results.is_some() || matches!(modal.plan, Some(Err(_))) {
        return row![
            Space::with_width(Length::Fill),
            modal_button("Close".into(), tokens::TEXT, false, Some(Message::DeleteClose)),
        ]
        .into();
    }
    let idle = !modal.running();
    let ready = idle && modal.ready_plan().is_some();
    row![
        Space::with_width(Length::Fill),
        modal_button("Cancel".into(), tokens::TEXT, false, idle.then_some(Message::DeleteClose)),
        modal_button(
            if modal.running() { "Deleting…".into() } else { "Delete".into() },
            tokens::RED,
            true,
            ready.then_some(Message::DeleteConfirm),
        ),
    ]
    .spacing(10)
    .align_y(Alignment::Center)
    .into()
}

pub fn delete_modal(modal: &DeleteModal) -> Element<'_, Message> {
    let heading = modal.ready_plan().map(title).unwrap_or_else(|| "Delete session".to_string());
    let header = container(text(heading).size(15).font(SANS).color(tokens::TEXT))
        .padding([16, 20])
        .width(Length::Fill)
        .style(|_| container::Style { background: Some(Background::Color(tokens::PANEL2)), ..Default::default() });
    let card = container(column![
        header,
        container(body(modal)).padding([16, 20]).max_height(460.0),
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
