use iced::{
    widget::{column, container, row, scrollable, text},
    Alignment, Background, Border, Element, Length,
};

use crate::{
    app::{App, Message},
    components::shell::tokens,
    style::{MONO, SANS_BOLD},
};
use ninox_core::types::Session;

/// A key/value row: fixed-width micro-label key, mono value.
fn key<'a>(label: &'static str) -> Element<'a, Message> {
    container(text(label.to_uppercase()).size(10).font(SANS_BOLD).color(tokens::DIM2))
        .width(Length::Fixed(180.0))
        .into()
}

fn field<'a>(label: &'static str, value: String) -> Element<'a, Message> {
    row![
        key(label),
        text(value).size(11.5).font(MONO).color(tokens::TEXT),
    ]
    .align_y(Alignment::Center)
    .into()
}

/// A key/value row whose value opens `url` in the browser when clicked.
fn link_field<'a>(
    label: &'static str,
    value: String,
    url: String,
) -> Element<'a, Message> {
    row![
        key(label),
        iced::widget::rich_text![
            iced::widget::span(value)
                .color(tokens::BLUE)
                .underline(true)
                .link(Message::OpenUrl(url))
        ]
        .size(11.5)
        .font(MONO),
    ]
    .align_y(Alignment::Center)
    .into()
}

/// Renders the `Burn` field per the Field Notes kv-sheet spec
/// (`docs/design-concepts/03-field-notes.html`), preferring the
/// statusline-sourced context percentage (`ninox_core::lifecycle::
/// statusline`, more accurate — accounts for window size and the
/// auto-compact buffer) over the transcript-derived raw token count
/// (`ninox_core::lifecycle::usage`) when all three statusline fields are
/// present.
fn format_burn(
    cost_usd:             f64,
    context_tokens:       Option<u64>,
    context_used_pct:     Option<f64>,
    context_total_tokens: Option<u64>,
    context_window_size:  Option<u64>,
) -> String {
    if let (Some(pct), Some(total), Some(size)) =
        (context_used_pct, context_total_tokens, context_window_size)
    {
        return format!(
            "${cost_usd:.2} · {}% context ({}/{})",
            pct.round() as i64,
            format_tokens_k(total),
            format_tokens_k(size),
        );
    }
    match context_tokens {
        Some(t) => format!("${cost_usd:.2} · {} tokens", format_tokens_k(t)),
        None    => format!("${cost_usd:.2}"),
    }
}

/// `214389` → `"214k"`. Rounds to the nearest thousand.
fn format_tokens_k(tokens: u64) -> String {
    format!("{}k", (tokens as f64 / 1000.0).round() as u64)
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
        ninox_core::types::SessionStatus::Interrupted   => "interrupted",
        ninox_core::types::SessionStatus::AwaitingReview => "awaiting_review",
        ninox_core::types::SessionStatus::Queued        => "queued",
    }
}

pub fn inspector_panel<'a>(app: &'a App, session: &'a Session) -> Element<'a, Message> {
    let orchestrator_name = session.orchestrator_id.as_deref()
        .and_then(|oid| app.orchestrators.iter().find(|o| o.id == oid))
        .map(|o| o.name.as_str())
        .unwrap_or("—");

    let fields: Vec<Element<Message>> = vec![
        field("Session ID",     session.id.clone()),
        field("Name",           session.name.clone()),
        field("Repository",     session.repo.clone()),
        field("Status",         status_str(&session.status).to_string()),
        field("Agent",          session.agent_type.clone()),
        field("Orchestrator",   orchestrator_name.to_string()),
        field("Burn",           format_burn(
            session.cost_usd,
            session.context_tokens,
            session.context_used_pct,
            session.context_total_tokens,
            session.context_window_size,
        )),
        match (session.pr_number, crate::app::pr_url_for_session(&app.prs, session)) {
            (Some(n), Some(url)) => link_field("PR", format!("#{n}"), url),
            (n, _) => field("PR", n.map(|n| format!("#{n}")).unwrap_or("—".into())),
        },
        field("PID",            session.pid.map(|p| p.to_string()).unwrap_or("—".into())),
        field("Workspace",      session.workspace_path.clone().unwrap_or("—".into())),
        field("Started (unix)", session.started_at.to_string()),
    ];

    let content: Vec<Element<Message>> = fields
        .into_iter()
        .map(|f| {
            container(f).padding([9, 0]).width(Length::Fill).into()
        })
        .collect();

    container(
        scrollable(
            container(column(content).spacing(1))
                .width(Length::Fill)
                .padding([12, 16])
                .style(|_| container::Style {
                    background: Some(Background::Color(tokens::PANEL)),
                    border: Border { color: tokens::BORDER, width: 1.0, radius: 8.0.into() },
                    ..Default::default()
                }),
        )
        .height(Length::Fill),
    )
    .padding([14, 20])
    .width(Length::Fill)
    .height(Length::Fill)
    .style(|_| container::Style { background: Some(Background::Color(tokens::BG)), ..Default::default() })
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_burn_matches_design_spec_example() {
        assert_eq!(
            format_burn(3.60, Some(214_000), None, None, None),
            "$3.60 · 214k tokens",
        );
    }

    #[test]
    fn format_burn_omits_tokens_when_unknown() {
        assert_eq!(format_burn(0.0, None, None, None, None), "$0.00");
    }

    #[test]
    fn format_burn_uses_statusline_context_when_present() {
        assert_eq!(
            format_burn(2.60, Some(999_999), Some(62.0), Some(124_000), Some(200_000)),
            "$2.60 · 62% context (124k/200k)",
        );
    }

    #[test]
    fn format_burn_falls_back_when_statusline_context_partially_absent() {
        // context_window_size missing — not enough to render the new format,
        // falls back to the transcript-based token count.
        assert_eq!(
            format_burn(1.00, Some(50_000), Some(25.0), Some(50_000), None),
            "$1.00 · 50k tokens",
        );
    }

    #[test]
    fn format_tokens_k_rounds_to_nearest_thousand() {
        assert_eq!(format_tokens_k(214_389), "214k");
        assert_eq!(format_tokens_k(500), "1k");
        assert_eq!(format_tokens_k(0), "0k");
    }

    #[test]
    fn status_str_covers_all_variants() {
        assert_eq!(status_str(&ninox_core::types::SessionStatus::Spawning), "spawning");
        assert_eq!(status_str(&ninox_core::types::SessionStatus::Working), "working");
        assert_eq!(status_str(&ninox_core::types::SessionStatus::PrOpen), "pr_open");
        assert_eq!(status_str(&ninox_core::types::SessionStatus::CiFailed), "ci_failed");
        assert_eq!(status_str(&ninox_core::types::SessionStatus::ReviewPending), "review_pending");
        assert_eq!(status_str(&ninox_core::types::SessionStatus::Mergeable), "mergeable");
        assert_eq!(status_str(&ninox_core::types::SessionStatus::Done), "done");
        assert_eq!(status_str(&ninox_core::types::SessionStatus::Terminated), "terminated");
        assert_eq!(status_str(&ninox_core::types::SessionStatus::Interrupted), "interrupted");
    }
}
