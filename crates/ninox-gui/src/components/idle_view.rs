//! The Idle tab — live workers parked on a review, CI run, merge or an
//! answer, grouped by orchestrator, with the worktree disk they hold. The
//! list comes from `ninox_core::idle_workers` (stored state and cached disk
//! numbers only; no GitHub calls, no disk walks here).

use iced::widget::{button, column, container, row, scrollable, text, text_input, Space};
use iced::{Alignment, Background, Border, Color, Element, Length};

use ninox_core::idle_workers::{self, IdleReport, IdleWorker, OrchestratorGroup, WaitingOn};

use super::shell::tokens;
use crate::app::{App, Message};
use crate::style::{MONO, SANS};

#[derive(Debug, Clone, Default)]
pub struct IdleState {
    pub report: IdleReport,
    /// Wall-clock ms the report was built, for "idle for" and "measured Xm ago".
    pub as_of: i64,
    pub notice: Option<Result<String, String>>,
    /// A prompt or clean is in flight; its buttons are disabled meanwhile.
    pub busy: bool,
}

pub fn idle_view(app: &App) -> Element<'_, Message> {
    let st = &app.idle;
    let r = &st.report;
    let n_orch = r.promptable().count();
    let n_prompt_workers: usize = r.promptable().map(|g| g.workers.len()).sum();

    let summary = if r.total == 0 {
        "No idle workers.".to_string()
    } else {
        format!(
            "{} idle worker{} · {} on disk, of which {} is build output",
            r.total,
            if r.total == 1 { "" } else { "s" },
            idle_workers::format_bytes(r.total_bytes),
            idle_workers::format_bytes(r.build_bytes),
        )
    };
    let prompt_label = if n_orch == 0 {
        "No orchestrator to prompt".to_string()
    } else {
        format!(
            "Prompt {n_orch} orchestrator{} about {n_prompt_workers} idle worker{}",
            if n_orch == 1 { "" } else { "s" },
            if n_prompt_workers == 1 { "" } else { "s" },
        )
    };
    let prompt_msg = (n_orch > 0 && !st.busy).then_some(Message::IdlePrompt(None));
    let header = row![
        action_button("← Fleet", tokens::TEXT, Some(Message::NavigateFleet)),
        column![
            text("Idle workers").size(13).font(SANS).color(tokens::TEXT),
            text(summary).font(SANS).size(11).color(tokens::DIM),
        ]
        .spacing(3),
        Space::new(Length::Fill, 0),
        action_button(&prompt_label, tokens::TEAL, prompt_msg),
    ]
    .spacing(10)
    .align_y(Alignment::Center)
    .padding([12, 20]);

    let notice: Option<Element<'_, Message>> = st.notice.as_ref().map(|n| {
        let (msg, color) = match n {
            Ok(m) => (m.clone(), tokens::GREEN),
            Err(m) => (m.clone(), tokens::RED),
        };
        container(text(msg).size(12).font(SANS).color(color)).padding([8, 20]).width(Length::Fill).into()
    });

    let body: Element<'_, Message> = if r.total == 0 {
        container(
            column![
                text("Nothing is waiting").size(13).font(SANS).color(tokens::DIM),
                text("Workers appear here after being idle or blocked for the configured time, or once their PR merged without finishing.")
                    .font(SANS).size(11).color(tokens::DIM2),
            ]
            .spacing(6)
            .align_x(Alignment::Center),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .into()
    } else {
        scrollable(
            column(r.groups.iter().map(|g| group(app, g)).collect::<Vec<_>>()).spacing(14).padding(14),
        )
        .width(Length::Fill)
        .into()
    };

    let mut layout = column![header, crate::style::hline(tokens::BORDER, 1.0)];
    if let Some(n) = notice {
        layout = layout.push(n);
    }
    layout.push(body).into()
}

fn group<'a>(app: &'a App, g: &'a OrchestratorGroup) -> Element<'a, Message> {
    let name = g.orchestrator_id.as_deref().unwrap_or("No orchestrator");
    let prompted = g.prompted_at.map(|t| {
        format!("prompted {} ago", idle_workers::format_duration(app.idle.as_of.saturating_sub(t)))
    });
    let can_prompt = g.orchestrator_id.is_some() && g.prompted_at.is_none() && !app.idle.busy;
    let mut head = row![
        text(name).size(12).font(SANS).color(tokens::TEXT),
        text(format!("{}", g.workers.len())).size(11).font(MONO).color(tokens::DIM),
    ]
    .spacing(8)
    .align_y(Alignment::Center);
    head = head.push(Space::new(Length::Fill, 0));
    if let Some(p) = prompted {
        head = head.push(text(p).size(11).font(SANS).color(tokens::DIM));
    }
    if let Some(o) = &g.orchestrator_id {
        head = head.push(action_button("Prompt", tokens::TEAL, can_prompt.then(|| Message::IdlePrompt(Some(o.clone())))));
    }
    let mut col = column![head].spacing(4);
    for w in &g.workers {
        col = col.push(worker_row(app, w));
    }
    container(col)
        .padding(10)
        .width(Length::Fill)
        .style(|_| container::Style {
            background: Some(Background::Color(tokens::PANEL)),
            border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
            ..Default::default()
        })
        .into()
}

fn waiting_color(w: &WaitingOn) -> Color {
    match w {
        WaitingOn::CiFailed | WaitingOn::Blocked { .. } => tokens::RED,
        WaitingOn::MergedNotCompleted | WaitingOn::Mergeable => tokens::GREEN,
        WaitingOn::Unknown => tokens::DIM2,
        _ => tokens::AMBER,
    }
}

fn worker_row<'a>(app: &'a App, w: &'a IdleWorker) -> Element<'a, Message> {
    let now = app.idle.as_of;
    let idle = w.idle_ms.map(idle_workers::format_duration).unwrap_or_else(|| "?".into());
    let disk = match w.disk {
        Some(d) => format!(
            "{} · {} build · {} ago",
            idle_workers::format_bytes(d.total_bytes),
            idle_workers::format_bytes(d.build_bytes),
            idle_workers::format_duration(now.saturating_sub(d.measured_at)),
        ),
        None => "not measured".to_string(),
    };
    let can_clean = w.can_clean();
    let pr_url = w.pr_number.filter(|_| !w.repo.is_empty()).map(|n| format!("https://github.com/{}/pull/{n}", w.repo));
    let mut actions = row![
        action_button("Open", tokens::TEXT, Some(Message::NavigateSession(w.id.clone()))),
        action_button("Message", tokens::TEXT, Some(Message::FleetSendMessageOpen(w.id.clone()))),
        action_button("Open PR", tokens::TEXT, pr_url.map(Message::OpenUrl)),
        action_button("Clean build output", tokens::AMBER, (can_clean && !app.idle.busy).then(|| Message::IdleClean(w.id.clone()))),
        action_button("Delete…", tokens::RED, Some(Message::DeleteAsk(w.id.clone()))),
    ]
    .spacing(4)
    .align_y(Alignment::Center);
    actions = actions.padding(0);

    let main = row![
        column![
            text(w.name.clone()).size(12).font(SANS).color(tokens::TEXT),
            text(w.id.clone()).size(10).font(MONO).color(tokens::DIM2),
        ]
        .spacing(1)
        .width(Length::FillPortion(3)),
        text(format!("idle {idle}")).size(11).font(MONO).color(tokens::DIM).width(Length::FillPortion(2)),
        text(w.waiting_on.label()).size(11).font(SANS).color(waiting_color(&w.waiting_on)).width(Length::FillPortion(3)),
        text(disk).size(11).font(MONO).color(tokens::DIM).width(Length::FillPortion(6)),
        actions,
    ]
    .spacing(10)
    .align_y(Alignment::Center);

    let mut col = column![main].spacing(4);
    if let Some((target, draft)) = app.fleet_message_draft.as_ref().filter(|(t, _)| t == &w.id) {
        let _ = target;
        col = col.push(
            row![
                text_input("Message this worker…", draft)
                    .on_input(Message::FleetSendMessageChanged)
                    .on_submit(Message::FleetSendMessageSubmit)
                    .size(12)
                    .padding(6),
                action_button("Send", tokens::TEAL, Some(Message::FleetSendMessageSubmit)),
                action_button("Cancel", tokens::DIM, Some(Message::FleetSendMessageCancel)),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        );
    }
    container(col).padding([6, 4]).into()
}

fn action_button<'a>(label: &str, color: Color, on_press: Option<Message>) -> Element<'a, Message> {
    let enabled = on_press.is_some();
    let color = if enabled { color } else { tokens::DIM2 };
    let mut b = button(text(label.to_string()).font(SANS).size(11.5).color(color))
        .padding([4, 9])
        .style(move |_theme, status| {
            let hovered = enabled && matches!(status, button::Status::Hovered);
            button::Style {
                background: Some(Background::Color(if hovered { tokens::PANEL2 } else { Color::TRANSPARENT })),
                text_color: color,
                border: Border { color: tokens::BORDER, width: 1.0, radius: 4.0.into() },
                ..Default::default()
            }
        });
    if let Some(m) = on_press {
        b = b.on_press(m);
    }
    b.into()
}
