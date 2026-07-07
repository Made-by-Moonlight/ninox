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

/// A key/value row whose value opens `url` in the browser when clicked.
fn link_field<'a>(
    label: &'static str,
    value: String,
    url: String,
    s: &'a ColorScheme,
) -> Element<'a, Message> {
    row![
        container(crate::style::micro_label(label, s.faint)).width(Length::Fixed(180.0)),
        iced::widget::rich_text![
            iced::widget::span(value)
                .color(s.accent)
                .underline(true)
                .link(Message::OpenUrl(url))
        ]
        .size(11.5)
        .font(crate::style::MONO),
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
        field("Burn",           format_burn(
            session.cost_usd,
            session.context_tokens,
            session.context_used_pct,
            session.context_total_tokens,
            session.context_window_size,
        ), s),
        match (session.pr_number, crate::app::pr_url_for_session(&app.prs, session)) {
            (Some(n), Some(url)) => link_field("PR", format!("#{n}"), url, s),
            (n, _) => field("PR", n.map(|n| format!("#{n}")).unwrap_or("—".into()), s),
        },
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
}
