use iced::{
    widget::{column, container, scrollable, text, Space},
    Background, Element, Length,
};

use crate::{app::{App, Message}, theme::ColorScheme};
use athene_core::types::Session;

fn field<'a>(label: &'static str, value: String, s: &'a ColorScheme) -> Element<'a, Message> {
    column![
        text(label).size(10).color(s.text_muted),
        text(value).size(12).color(s.text_primary),
    ]
    .spacing(2)
    .into()
}

fn status_str(status: &athene_core::types::SessionStatus) -> &'static str {
    match status {
        athene_core::types::SessionStatus::Spawning      => "spawning",
        athene_core::types::SessionStatus::Working       => "working",
        athene_core::types::SessionStatus::PrOpen        => "pr_open",
        athene_core::types::SessionStatus::CiFailed      => "ci_failed",
        athene_core::types::SessionStatus::ReviewPending => "review_pending",
        athene_core::types::SessionStatus::Mergeable     => "mergeable",
        athene_core::types::SessionStatus::Done          => "done",
        athene_core::types::SessionStatus::Terminated    => "terminated",
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

    let content = fields.into_iter().flat_map(|f| {
        let divider = container(Space::new(Length::Fill, 1.0))
            .width(Length::Fill)
            .style(move |_| container::Style {
                background: Some(Background::Color(s.border)),
                ..Default::default()
            });
        vec![
            container(f).padding([8, 16]).width(Length::Fill).into(),
            divider.into(),
        ]
    });

    scrollable(
        column(content.collect::<Vec<_>>())
    )
    .height(Length::Fill)
    .into()
}
