use iced::{
    widget::{column, container, row, scrollable, text},
    Alignment, Element, Length,
};

use crate::{app::{App, Message}, theme::ColorScheme};
use ninox_core::types::Session;

/// A key/value row: fixed-width micro-label key, mono value.
fn field<'a>(label: &'static str, value: String, s: &'a ColorScheme) -> Element<'a, Message> {
    row![
        container(crate::style::micro_label(label, s.faint)).width(Length::Fixed(180.0)),
        text(value).size(11.5).font(crate::style::MONO).color(s.ink_2),
    ]
    .align_y(Alignment::Center)
    .into()
}

fn status_str(status: &ninox_core::types::SessionStatus) -> &'static str {
    match status {
        ninox_core::types::SessionStatus::Spawning      => "spawning",
        ninox_core::types::SessionStatus::Working       => "working",
        ninox_core::types::SessionStatus::PrOpen        => "pr_open",
        ninox_core::types::SessionStatus::CiFailed      => "ci_failed",
        ninox_core::types::SessionStatus::ReviewPending => "review_pending",
        ninox_core::types::SessionStatus::Mergeable     => "mergeable",
        ninox_core::types::SessionStatus::Done          => "done",
        ninox_core::types::SessionStatus::Terminated    => "terminated",
    }
}

pub fn inspector_panel<'a>(app: &'a App, session: &'a Session) -> Element<'a, Message> {
    let s = &app.scheme;

    let orchestrator_name = session.orchestrator_id.as_deref()
        .and_then(|oid| app.orchestrators.iter().find(|o| o.id == oid))
        .map(|o| o.name.as_str())
        .unwrap_or("—");

    let fields: Vec<Element<Message>> = vec![
        field("Session ID",     session.id.clone(), s),
        field("Name",           session.name.clone(), s),
        field("Repository",     session.repo.clone(), s),
        field("Status",         status_str(&session.status).to_string(), s),
        field("Agent",          session.agent_type.clone(), s),
        field("Orchestrator",   orchestrator_name.to_string(), s),
        field("Cost",           format!("${:.4}", session.cost_usd), s),
        field("PR",             session.pr_number.map(|n| format!("#{n}")).unwrap_or("—".into()), s),
        field("PID",            session.pid.map(|p| p.to_string()).unwrap_or("—".into()), s),
        field("Workspace",      session.workspace_path.clone().unwrap_or("—".into()), s),
        field("Started (unix)", session.started_at.to_string(), s),
    ];

    let content: Vec<Element<Message>> = fields.into_iter().flat_map(|f| {
        vec![
            container(f).padding([8, 0]).width(Length::Fill).into(),
            crate::style::dotted_rule(s.rule_dark),
        ]
    }).collect();

    container(
        scrollable(
            column(content),
        )
        .height(Length::Fill),
    )
    .padding([18, 22])
    .width(Length::Fill)
    .height(Length::Fill)
    .style(move |_theme| crate::style::card_style(s))
    .into()
}
