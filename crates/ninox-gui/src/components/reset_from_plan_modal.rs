//! "Reset from plan" dialog for an orchestrator, opened from the session
//! page's action bar (`Message::ResetFromPlanAsk`). One dialog covers the
//! whole flow: asking, waiting for the plan update, confirming with a plan
//! preview, resetting. The flow itself is `ninox_ui_shared::reset_from_plan`.

use iced::{
    widget::{column, container, row, scrollable, text, Space},
    Alignment, Background, Border, Color, Element, Length,
};
use ninox_ui_shared::reset_from_plan::{PlanPreview, ResetFlow};

use super::markdown_blocks::{parse_blocks, Block, BlockKind};
use super::restore_modal::modal_button;
use super::shell::tokens;
use crate::{app::Message, style::SANS};

pub use ninox_ui_shared::reset_from_plan::UiPhase;

pub struct ResetUi {
    pub id: String,
    /// Tags this flow's background results; see `next_generation`.
    pub generation: u64,
    pub name: String,
    /// Taken by the confirm; `None` once the reset is running.
    pub flow: Option<ResetFlow>,
    pub phase: UiPhase,
    pub preview: Option<PlanPreview>,
    blocks: Vec<Block>,
    pub error: Option<String>,
    /// Opened from the automatic trigger's notification: cancelling it also
    /// keeps the trigger from starting another flow for this conversation.
    pub auto_started: bool,
}

impl ResetUi {
    pub fn new(id: String, name: String, flow: ResetFlow) -> Self {
        Self { id, generation: ninox_ui_shared::reset_from_plan::next_generation(), name, flow: Some(flow), phase: UiPhase::Sending, preview: None, blocks: Vec::new(), error: None, auto_started: false }
    }

    /// Close the dialog, backing out of the flow it drives.
    pub fn cancel(mut self) {
        if let (true, Some(flow)) = (self.auto_started, self.flow.take()) {
            flow.cancel();
        }
    }

    pub fn set_preview(&mut self, preview: Option<PlanPreview>) {
        let changed = self.preview.as_ref().map(|p| &p.content) != preview.as_ref().map(|p| &p.content);
        if changed {
            self.blocks = preview.as_ref().map(|p| parse_blocks(&p.content)).unwrap_or_default();
        }
        self.preview = preview;
    }

    pub fn can_reset(&self) -> bool {
        self.phase.can_reset() && self.flow.is_some() && self.preview.is_some()
    }
}

fn block_view(block: &Block) -> Element<'_, Message> {
    let (size, font) = super::plan_panel::style_for(block.kind);
    let prefix = match block.kind {
        BlockKind::ListItem { checked: Some(true), .. } => "[x] ".to_string(),
        BlockKind::ListItem { checked: Some(false), .. } => "[ ] ".to_string(),
        BlockKind::ListItem { ordinal: Some(n), checked: None } => format!("{n}. "),
        BlockKind::ListItem { ordinal: None, checked: None } => "• ".to_string(),
        _ => String::new(),
    };
    let color = if matches!(block.kind, BlockKind::Heading(_)) { tokens::TEXT } else { tokens::hex(0xd6d9e0) };
    text(format!("{prefix}{}", block.text)).size(size.min(15.0)).font(font).color(color).into()
}

pub fn reset_from_plan_modal(ui: &ResetUi) -> Element<'_, Message> {
    let header = container(
        text(format!("Reset {} from plan", ui.name)).size(15).font(SANS).color(tokens::TEXT),
    )
    .padding([16, 20])
    .width(Length::Fill)
    .style(|_| container::Style { background: Some(Background::Color(tokens::PANEL2)), ..Default::default() });

    let (status, hint) = match ui.phase {
        UiPhase::Sending => ("Asking the orchestrator to update its plan…".to_string(), ""),
        UiPhase::Waiting => (
            "Waiting for plan update…".to_string(),
            "The orchestrator was asked to write its plan so a fresh session can continue from it. \
             Nothing resets until you confirm.",
        ),
        UiPhase::TimedOut => (
            "Timed out waiting for the plan update.".to_string(),
            "You can still reset with the plan as it is, or cancel.",
        ),
        UiPhase::Ready => (
            "The plan was updated. Review it, then reset.".to_string(),
            "Resets onto a fresh conversation with this plan as its first prompt. Workers stay attached.",
        ),
        UiPhase::SendFailed => (
            "Could not ask the orchestrator to update its plan.".to_string(),
            "You can still reset with the plan as it is, or cancel.",
        ),
        UiPhase::Resetting => ("Resetting…".to_string(), ""),
        UiPhase::ResetFailed => ("The reset failed. The previous conversation was stopped; Resume on the session page brings it back.".to_string(), ""),
    };
    let mut body = column![text(status).size(13).font(SANS).color(tokens::TEXT)].spacing(8);
    if !hint.is_empty() {
        body = body.push(text(hint).size(11.5).font(SANS).color(tokens::DIM));
    }
    if let Some(err) = &ui.error {
        body = body.push(text(err.clone()).size(11.5).font(SANS).color(tokens::RED));
    }
    if ui.phase.pollable() && ui.preview.is_none() && ui.phase != UiPhase::Ready {
        body = body.push(text("No readable plan doc is registered yet; there is nothing to reset from.").size(11.5).font(SANS).color(tokens::RED));
    }
    if matches!(ui.phase, UiPhase::Ready | UiPhase::TimedOut | UiPhase::SendFailed) {
        if let Some(p) = &ui.preview {
            let blocks: Vec<Element<Message>> = ui.blocks.iter().map(block_view).collect();
            body = body
                .push(text(p.path.clone()).size(11).font(crate::style::MONO).color(tokens::DIM2))
                .push(
                    container(scrollable(column(blocks).spacing(6).padding(12)).height(Length::Fixed(280.0)))
                        .width(Length::Fill)
                        .style(|_| container::Style {
                            background: Some(Background::Color(tokens::PANEL2)),
                            border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
                            ..Default::default()
                        }),
                );
        } else if ui.phase == UiPhase::TimedOut {
            body = body.push(text("No readable plan doc is registered.").size(11.5).font(SANS).color(tokens::RED));
        }
    }

    let go = |label: &str| {
        modal_button(label.into(), tokens::TEAL, true, ui.can_reset().then_some(Message::ResetFromPlanConfirm))
    };
    let mut footer = row![Space::new(Length::Fill, 0)].spacing(10).align_y(Alignment::Center);
    if ui.phase != UiPhase::Resetting {
        footer = footer.push(modal_button("Cancel".into(), tokens::TEXT, false, Some(Message::ResetFromPlanCancel)));
    }
    footer = match ui.phase {
        UiPhase::Ready => footer.push(go("Reset now")),
        UiPhase::Waiting | UiPhase::TimedOut | UiPhase::SendFailed => footer.push(go("Reset with the plan as it is")),
        UiPhase::Sending | UiPhase::Resetting | UiPhase::ResetFailed => footer,
    };

    let card = container(column![header, container(body).padding([16, 20]), container(footer).padding([12, 20])])
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
