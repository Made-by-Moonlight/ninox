use iced::{
    widget::{button, column, container, row, scrollable, text, Space},
    Alignment, Background, Border, Element, Length, Shadow, Vector,
};

use crate::app::{App, Message};
use crate::components::filter_bar::filter_bar;
use crate::components::shell::tokens;
use ninox_core::types::{OrchestratorId, Session, SessionStatus};

struct Column {
    label: &'static str,
    status: SessionStatus,
}

const COLUMNS: &[Column] = &[
    Column { label: "Working",   status: SessionStatus::Working },
    Column { label: "PR Open",   status: SessionStatus::PrOpen },
    Column { label: "CI Failed", status: SessionStatus::CiFailed },
    Column { label: "Review",    status: SessionStatus::ReviewPending },
    Column { label: "Mergeable", status: SessionStatus::Mergeable },
    Column { label: "Done",      status: SessionStatus::Done },
];

#[cfg(test)]
pub fn filtered_sessions(app: &App) -> Vec<&Session> {
    let q = app.fleet_filter.query.to_lowercase();
    app.sessions.values().filter(|s| {
        q.is_empty()
            || s.name.to_lowercase().contains(&q)
            || s.repo.to_lowercase().contains(&q)
    }).collect()
}

#[cfg(test)]
pub fn board_sessions<'a>(
    app: &'a App,
    status: &SessionStatus,
    scope: Option<&str>,
) -> Vec<&'a Session> {
    let q = app.fleet_filter.query.to_lowercase();
    let orch_ids: std::collections::HashSet<&str> =
        app.orchestrators.iter().map(|o| o.id.as_str()).collect();
    let mut sessions: Vec<&Session> = app.sessions.values().filter(|s| {
        &s.status == status && ninox_overview::board::on_board(s, orch_ids.contains(s.id.as_str()), scope, &q)
    }).collect();
    sessions.sort_by(|a, b| a.name.cmp(&b.name));
    sessions
}

pub fn attention_count(app: &App) -> usize {
    app.sessions.values().filter(|s| {
        !ninox_core::worker_type::is_prompt_agent_session(s)
            && matches!(s.status, SessionStatus::CiFailed | SessionStatus::ReviewPending)
    }).count()
}

pub fn interrupted_count(app: &App) -> usize {
    ninox_overview::board::resume_all_targets(app.sessions.values()).len()
}

/// Like `board_sessions`, but matches via `column_absorbs` instead of exact
/// status equality — this is what a column actually renders. Duplicates
/// `board_sessions`' query/orchestrator/scope filtering by design rather
/// than calling it twice and merging (Terminated-under-Done was previously
/// done that way, via a base call plus a conditional `.extend()` — adding
/// a second folded status that way would mean a third additive call, and
/// still wouldn't help `folio`'s `shown` count reuse the same rule).
pub fn column_sessions<'a>(
    app: &'a App,
    col_status: &SessionStatus,
    scope: Option<&str>,
) -> Vec<&'a Session> {
    let q = app.fleet_filter.query.to_lowercase();
    let orch_ids: std::collections::HashSet<&str> =
        app.orchestrators.iter().map(|o| o.id.as_str()).collect();
    let mut sessions: Vec<&Session> = app.sessions.values().filter(|s| {
        ninox_overview::board::column_absorbs(col_status, &s.status)
            && ninox_overview::board::on_board(s, orch_ids.contains(s.id.as_str()), scope, &q)
    }).collect();
    sessions.sort_by(|a, b| a.name.cmp(&b.name));
    sessions
}

fn tab_chip<'a>(label: String, msg: Message, is_active: bool) -> Element<'a, Message> {
    button(text(label).size(10).font(crate::style::SANS).color(
        if is_active { tokens::BG } else { tokens::DIM },
    ))
    .on_press(msg)
    .padding([4, 10])
    .style(move |_t, status| button::Style {
        background: Some(Background::Color(if is_active {
            tokens::TEAL
        } else if matches!(status, button::Status::Hovered) {
            tokens::PANEL2
        } else {
            tokens::PANEL
        })),
        border: Border { color: tokens::BORDER, width: 1.0, radius: 4.0.into() },
        ..Default::default()
    })
    .into()
}

/// Orchestrator scope chips ("All" plus one per orchestrator). Wraps onto
/// further rows instead of shrinking any chip below its label's natural
/// width (MLOPS-5337) — `iced`'s non-wrapping `Row` layout hands `Shrink`
/// children width from a shared budget that only decreases and is never
/// reset, so once ~18 orchestrators exceed the container width, later
/// chips get squeezed to a near-zero width and their text wraps
/// letter-by-letter.
fn orchestrator_chip_row<'a>(
    orchestrators: &'a [ninox_core::types::Orchestrator],
    scope: Option<&'a OrchestratorId>,
) -> Element<'a, Message> {
    let mut chips: Vec<Element<Message>> = Vec::new();
    chips.push(tab_chip("ALL".to_string(), Message::NavigateBoard { scope: None }, scope.is_none()));
    for orch in orchestrators {
        let is_active = scope.map(|id| id == &orch.id).unwrap_or(false);
        chips.push(tab_chip(
            orch.name.to_uppercase(),
            Message::NavigateBoard { scope: Some(orch.id.clone()) },
            is_active,
        ));
    }
    row(chips).spacing(8).wrap().into()
}

/// Sessions counted toward the "total" figure — every session that isn't
/// itself an orchestrator's own bookkeeping session, and never the
/// prompt-agent singleton (MLOPS-5415), which is answered by `ninox
/// list`/`connect` directly but has no Board row of its own.
pub(crate) fn total_session_count(app: &App) -> usize {
    let orch_ids: std::collections::HashSet<&str> =
        app.orchestrators.iter().map(|o| o.id.as_str()).collect();
    app.sessions.values()
        .filter(|w| !orch_ids.contains(w.id.as_str()) && !ninox_core::worker_type::is_prompt_agent_session(w))
        .count()
}

/// Slim header: session count + filter field. Replaces the old Field
/// Notes "folio" (serif greeting, roman-numeral volume/date stamp) —
/// dropped per round-1 review feedback (item A2), which also flagged the
/// cream/serif look this avoids entirely by not touching `app.scheme`.
fn header<'a>(app: &'a App, total: usize, shown: usize) -> Element<'a, Message> {
    let n = interrupted_count(app);
    let mut items: Vec<Element<Message>> = vec![filter_bar(app), Space::new(Length::Fill, 0).into()];
    if n > 0 {
        items.push(
            button(text(format!("Resume all ({n})")).size(10).color(tokens::AMBER))
                .on_press(Message::ResumeAllSessions)
                .padding([4, 10])
                .style(|_t, status| button::Style {
                    background: matches!(status, button::Status::Hovered)
                        .then_some(Background::Color(tokens::AMBER_BADGE_BG)),
                    text_color: tokens::AMBER,
                    border: Border { color: tokens::AMBER, width: 1.0, radius: 4.0.into() },
                    ..Default::default()
                })
                .into(),
        );
        items.push(Space::new(14, 0).into());
    }
    items.push(text(format!("{shown}/{total} sessions")).size(10.5).font(crate::style::MONO).color(tokens::DIM).into());

    row(items).align_y(Alignment::Center).padding([16, 20]).into()
}

/// 1.5px border banner shown while any session needs attention.
fn attention_banner(app: &App) -> Option<Element<'_, Message>> {
    if attention_count(app) == 0 { return None; }
    let ci = app.sessions.values()
        .filter(|w| !ninox_core::worker_type::is_prompt_agent_session(w) && matches!(w.status, SessionStatus::CiFailed))
        .count();
    let review = app.sessions.values()
        .filter(|w| !ninox_core::worker_type::is_prompt_agent_session(w) && matches!(w.status, SessionStatus::ReviewPending))
        .count();
    let mut parts = Vec::new();
    if ci > 0 { parts.push(format!("{ci} CI failure{}", if ci == 1 { "" } else { "s" })); }
    if review > 0 { parts.push(format!("{review} review{}", if review == 1 { "" } else { "s" })); }
    Some(
        container(
            row![
                text(format!("{} require attention.", parts.join(" and ")))
                    .size(12).font(crate::style::SANS).color(tokens::RED),
            ]
            .align_y(Alignment::Center),
        )
        .padding([8, 14])
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(Background::Color(iced::Color { a: 0.08, ..tokens::RED })),
            border: Border { color: tokens::RED, width: 1.5, radius: 4.0.into() },
            ..Default::default()
        })
        .into(),
    )
}

/// Clips a session summary to a sane single-card length so an unusually
/// long spawn-prompt first line can't blow out the card height.
fn truncate_summary(summary: &str) -> String {
    truncate_chars(summary, 90)
}

/// Clips `s` to at most `max_chars`, appending an ellipsis if it had to.
/// Used (with `Wrapping::None`) instead of letting a long hyphenated name
/// or repo slug wrap — iced's default text wrapping breaks at hyphens
/// (and, once a word segment is still too wide, mid-word), which read as
/// a design defect on a name like `bottlerocket-selinux-mount-propagation-test`
/// (round-2 review feedback, item 3).
fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let clipped: String = s.chars().take(max_chars).collect();
        format!("{clipped}…")
    }
}

/// v1 card style: a 4px status-colored top bar, harness/status badge,
/// name, brief, cost — matching `Main.dc.html`'s `.card`/`.card-bar`
/// (dark panel, not cream paper — round-1 review feedback, item A2).
fn session_card<'a>(app: &'a App, session: &'a Session) -> Element<'a, Message> {
    let st_color = tokens::status_color(&session.status);
    let word = crate::style::stamp_word(&session.status);
    // The last path segment (the repo itself), not the full `org/repo` —
    // shorter, and more useful than an org name repeated on every card.
    let repo_full = if session.repo.is_empty() { &session.id } else { &session.repo };
    let repo_line = repo_full.rsplit('/').next().unwrap_or(repo_full);

    // A card's column is a fraction of the window (six `FillPortion`
    // columns), not a flexible budget shared with unrelated siblings —
    // unlike the nav bar's wrap bug, `Wrapping::None` plus a box this
    // narrow still reflows (iced always fits text within the width it's
    // given), so names/repos are truncated short enough to fit even a
    // single 1024px-wide column rather than relying on wrap mode alone
    // (round-2 review feedback, item 3).
    let mut body: Vec<Element<Message>> = vec![
        Space::new(0, 4).into(),
        text(truncate_chars(&session.name, 20))
            .size(13)
            .font(crate::style::SANS)
            .color(tokens::TEXT)
            .wrapping(iced::widget::text::Wrapping::None)
            .into(),
        Space::new(0, 2).into(),
        text(truncate_chars(repo_line, 24))
            .size(9.5)
            .font(crate::style::SANS)
            .color(tokens::DIM2)
            .wrapping(iced::widget::text::Wrapping::None)
            .into(),
    ];
    if let Some(summary) = session.summary.as_deref().filter(|sum| !sum.is_empty()) {
        body.push(Space::new(0, 4).into());
        body.push(
            text(truncate_summary(summary))
                .size(10.5)
                .font(crate::style::SANS)
                .color(tokens::DIM)
                .into(),
        );
    }
    body.push(Space::new(0, 9).into());

    let retention_badge: Option<Element<Message>> = if matches!(
        session.status, SessionStatus::Done | SessionStatus::Terminated
    ) {
        session.terminal_at.and_then(|terminal_at| {
            crate::components::lifecycle_status::retention_label(
                terminal_at,
                app.config.session_retention.retention_millis(),
                crate::components::lifecycle_status::now_millis(),
            )
        }).map(|label| text(label).size(9.5).font(crate::style::MONO).color(tokens::DIM2).into())
    } else {
        None
    };
    // A kept-alive merged worker ([auto_reap] off) otherwise sits in the
    // Mergeable/PR-open column reading "ready to merge", when its PR has in
    // fact already merged and it's only waiting to be reaped. Mutually
    // exclusive with `retention_badge` (that one is terminal-only).
    let merged_badge: Option<Element<Message>> = if session.merged_at.is_some()
        && !session.status.is_terminal()
    {
        Some(text("merged · reap").size(9.5).font(crate::style::MONO).color(tokens::DIM).into())
    } else if session.status == SessionStatus::Queued {
        let label = ninox_ui_shared::session_queue::queued_badge(app.queue_positions.get(&session.id).copied());
        Some(text(label).size(9.5).font(crate::style::MONO).color(tokens::DIM).into())
    } else {
        None
    };
    // `.wrap()`, not a `Space::Fill` spacer: in a narrow `FillPortion`
    // column a long name/summary can leave the badge+cost row too tight
    // for a `Fill` spacer to resolve sanely, and iced's non-wrapping `Row`
    // then squeezes the trailing cost text below its natural width,
    // letter-wrapping it vertically (the same failure mode the
    // orchestrator-chip-row regression test above guards against).
    // Wrapping lets the cost drop to its own line instead.
    let mut bottom_row: Vec<Element<Message>> = vec![crate::style::stamp(word, st_color)];
    if let Some(badge) = retention_badge.or(merged_badge) {
        bottom_row.push(badge);
    }
    bottom_row.push(
        text(format!("${:.2}", session.cost_usd))
            .size(11)
            .font(crate::style::MONO_MEDIUM)
            .color(tokens::DIM)
            .wrapping(iced::widget::text::Wrapping::None)
            .into(),
    );
    body.push(row(bottom_row).align_y(Alignment::Center).spacing(8).wrap().into());

    button(
        column(body).padding(iced::Padding { top: 0.0, right: 13.0, bottom: 10.0, left: 13.0 }),
    )
    .on_press(Message::NavigateSession(session.id.clone()))
    .width(Length::Fill)
    .style(move |_t, status| {
        let hovered = matches!(status, button::Status::Hovered);
        button::Style {
            background: Some(Background::Color(tokens::PANEL)),
            text_color: tokens::TEXT,
            border: Border {
                color: if hovered { tokens::DIM2 } else { tokens::BORDER },
                width: 1.0,
                radius: 8.0.into(),
            },
            shadow: Shadow {
                color: iced::Color { a: 0.4, ..iced::Color::BLACK },
                offset: Vector::new(0.0, 3.0),
                blur_radius: 14.0,
            },
        }
    })
    .into()
}

/// A 4px status-colored bar along a card's top edge, matching
/// `Main.dc.html`'s `.card-bar`.
fn card_bar<'a>(session_card: Element<'a, Message>, color: iced::Color) -> Element<'a, Message> {
    column![
        container(Space::new(0, 0)).width(Length::Fill).height(Length::Fixed(4.0)).style(move |_| {
            container::Style { background: Some(Background::Color(color)), ..Default::default() }
        }),
        session_card,
    ]
    .into()
}

fn ledger_column<'a>(
    app: &'a App,
    col: &'static Column,
    scope: Option<&str>,
) -> Element<'a, Message> {
    let sessions = column_sessions(app, &col.status, scope);
    let count = sessions.len();
    let cards: Vec<Element<Message>> = sessions
        .iter()
        .map(|s| card_bar(session_card(app, s), tokens::status_color(&s.status)))
        .collect();

    let head = row![
        text(col.label).size(12).font(crate::style::SANS).color(tokens::DIM),
        Space::new(6, 0),
        text(format!("{count}")).size(10).font(crate::style::MONO).color(tokens::DIM2),
    ]
    .align_y(Alignment::Center);

    // Thin, `--border`-coloured scrollbar instead of the default theme's
    // thick grey one (round-2 review feedback, item 4).
    let body = scrollable(column(cards).spacing(10).padding(iced::Padding {
        top: 10.0, right: 2.0, bottom: 4.0, left: 0.0,
    }))
    .height(Length::Fill)
    .direction(scrollable::Direction::Vertical(
        scrollable::Scrollbar::new().width(4).scroller_width(4),
    ))
    .style(|_t, _status| scrollable::Style {
        container: container::Style::default(),
        vertical_rail: scrollable::Rail {
            background: None,
            border: Border::default(),
            scroller: scrollable::Scroller { color: tokens::BORDER, border: Border::default() },
        },
        horizontal_rail: scrollable::Rail {
            background: None,
            border: Border::default(),
            scroller: scrollable::Scroller { color: tokens::BORDER, border: Border::default() },
        },
        gap: None,
    });

    container(column![head, Space::new(0, 8), body])
        .width(Length::FillPortion(1))
        .padding([0, 8])
        .into()
}

pub fn fleet_board<'a>(app: &'a App, scope: Option<&'a OrchestratorId>) -> Element<'a, Message> {
    let total = total_session_count(app);
    let shown = COLUMNS.iter()
        .map(|c| column_sessions(app, &c.status, scope.map(|x| x.as_str())).len())
        .sum::<usize>();

    let mut sections: Vec<Element<Message>> = Vec::new();
    sections.push(header(app, total, shown));

    if let Some(banner) = attention_banner(app) {
        sections.push(
            container(banner)
                .padding(iced::Padding { top: 0.0, right: 20.0, bottom: 0.0, left: 20.0 })
                .width(Length::Fill)
                .into(),
        );
        sections.push(Space::new(0, 10).into());
    }

    if !app.orchestrators.is_empty() {
        sections.push(
            container(orchestrator_chip_row(&app.orchestrators, scope))
                .padding(iced::Padding { top: 0.0, right: 20.0, bottom: 10.0, left: 20.0 })
                .width(Length::Fill)
                .into(),
        );
    }

    if total == 0 {
        sections.push(
            container(text("No sessions in the field.").size(13).font(crate::style::SANS).color(tokens::DIM))
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .width(Length::Fill)
                .height(Length::Fill)
                .into(),
        );
    } else {
        // Flexible, equal-width columns that fill the available width — no
        // horizontal scrollbar at any window width (round-1 review
        // feedback, item B10: the old fixed-220px columns forced one once
        // six of them exceeded the window).
        let ledger_cols: Vec<Element<Message>> =
            COLUMNS.iter().map(|col| ledger_column(app, col, scope.map(|s| s.as_str()))).collect();

        sections.push(
            container(row(ledger_cols).spacing(0))
                .padding(iced::Padding { top: 6.0, right: 20.0, bottom: 16.0, left: 20.0 })
                .width(Length::Fill)
                .height(Length::Fill)
                .into(),
        );
    }

    container(column(sections).width(Length::Fill).height(Length::Fill))
        .width(Length::Fill)
        .height(Length::Fill)
        .style(|_| container::Style {
            background: Some(Background::Color(tokens::BG)),
            ..Default::default()
        })
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_summary_leaves_short_text_untouched() {
        assert_eq!(truncate_summary("Fix flaky CI"), "Fix flaky CI");
    }

    #[test]
    fn truncate_summary_clips_long_text_with_ellipsis() {
        let long = "a".repeat(120);
        let clipped = truncate_summary(&long);
        assert_eq!(clipped.chars().count(), 91); // 90 chars + "…"
        assert!(clipped.ends_with('…'));
    }

    #[test]
    fn truncate_chars_clips_a_long_hyphenated_name_whole_instead_of_wrapping() {
        let name = "bottlerocket-selinux-mount-propagation-test";
        let clipped = truncate_chars(name, 34);
        assert_eq!(clipped.chars().count(), 35); // 34 chars + "…"
        assert!(clipped.ends_with('…'));
        assert!(name.starts_with(clipped.trim_end_matches('…')));
    }

    #[test]
    fn truncate_chars_leaves_short_text_untouched() {
        assert_eq!(truncate_chars("short-name", 34), "short-name");
    }

    /// MLOPS-5337: with ~18 orchestrators the chip row's combined natural
    /// width exceeds the container width. `iced`'s non-wrapping `Row`
    /// layout (`iced_core::layout::flex::resolve`) hands each `Shrink`
    /// child a width from a shared budget that only ever decreases and is
    /// never reset or wrapped — once it runs out, later chips get a
    /// near-zero width limit, forcing their text to wrap letter-by-letter.
    /// Chips must keep their natural label width and wrap onto further
    /// rows instead.
    #[test]
    fn orchestrator_chip_row_never_shrinks_chips_below_their_natural_width() {
        use iced::advanced::layout::Limits;
        use iced::advanced::widget::Tree;
        use iced::{Font, Pixels, Size};
        use ninox_core::types::Orchestrator;

        // 18 orchestrators mirrors the real fleet-board data dir that
        // triggered MLOPS-5337.
        let orchestrators: Vec<Orchestrator> = (0..18)
            .map(|i| Orchestrator {
                id: format!("orch-{i:02}"),
                name: format!("worker-orchestrator-{i:02}"),
                created_at: 0,
            })
            .collect();
        let labels: Vec<String> = std::iter::once("ALL".to_string())
            .chain(orchestrators.iter().map(|o| o.name.to_uppercase()))
            .collect();

        crate::style::load_bundled_fonts();

        let renderer = iced::Renderer::Secondary(iced_tiny_skia::Renderer::new(
            Font::default(),
            Pixels(16.0),
        ));

        let natural_widths: Vec<f32> = labels
            .iter()
            .map(|label| {
                let element = tab_chip(label.clone(), Message::NavigateBoard { scope: None }, false);
                let mut tree = Tree::new(element.as_widget());
                element
                    .as_widget()
                    .layout(&mut tree, &renderer, &Limits::new(Size::ZERO, Size::INFINITY))
                    .size()
                    .width
            })
            .collect();

        let chip_row = orchestrator_chip_row(&orchestrators, None);
        let mut tree = Tree::new(chip_row.as_widget());
        // Comfortably fits only a handful of chips per line — the real
        // fleet board hits this with ~18 orchestrators at a normal window
        // width.
        let limits = Limits::new(Size::ZERO, Size::new(600.0, f32::INFINITY));
        let node = chip_row.as_widget().layout(&mut tree, &renderer, &limits);

        let children: Vec<_> = node.children().to_vec();
        assert_eq!(children.len(), labels.len());

        for (label, (natural_width, child)) in
            labels.iter().zip(natural_widths.iter().zip(&children))
        {
            assert!(
                (child.size().width - natural_width).abs() < 0.5,
                "chip {label:?} shrunk below its natural label width: natural={natural_width}, actual={}",
                child.size().width,
            );
        }

        let distinct_rows = children
            .iter()
            .map(|c| (c.bounds().y * 10.0).round() as i32)
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        assert!(distinct_rows > 1, "expected chips to wrap onto multiple rows, got {distinct_rows}");
    }
}
