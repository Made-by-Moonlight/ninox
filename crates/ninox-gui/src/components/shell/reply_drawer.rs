//! The Ninox prompt reply drawer (`PromptReply.dc.html`): rises from the
//! prompt bar over the bottom of the current view, never navigating away —
//! the prompt agent has no terminal in the GUI. Collapsed, it folds to a
//! one-line indicator above the prompt bar. State lives in
//! `prompt_agent::ReplyDrawer`; answers are parsed to markdown in
//! `app.rs`'s update, never here.

use iced::{
    widget::{button, column, container, mouse_area, rich_text, row, scrollable, span, text, Space},
    Alignment, Background, Border, Color, Element, Length, Padding, Shadow, Vector,
};

use super::tokens;
use crate::app::{App, Message};
use crate::prompt_agent::{AnswerState, DrawerStatus, Notice, Relay, Turn};
use ninox_ui_shared::prompt_agent::reply::{relay_tail, status_line};

/// The board's drawer height (470px in a 944px window).
const DRAWER_HEIGHT: f32 = 470.0;
/// Room always left above the drawer, so a short window never loses the
/// whole view behind it.
const MIN_VISIBLE_ABOVE: f32 = 160.0;

/// (session id, display term) — a session contributes its id and its name.
pub type SessionTerm = (String, String);
/// (PR number, PR url).
pub type PrLink = (u64, String);

/// Maps a clicked drawer link back to a `Message`: session links open the
/// session's island, brain links go through the Brain's own handler, and
/// an undecodable internal link is dropped rather than opening a dead tab.
pub fn link_message(url: &iced::widget::markdown::Url) -> Message {
    use crate::components::internal_link::{decode, is_internal, InternalLink};
    match decode(url) {
        Some(InternalLink::Session(id)) => Message::FleetOpenNode(id),
        Some(InternalLink::Brain(_)) => Message::BrainLinkClicked(url.clone()),
        Some(InternalLink::Unresolved(_)) => Message::Noop,
        None if is_internal(url) => Message::Noop,
        None => Message::OpenUrl(url.to_string()),
    }
}

/// Turns known session ids/names into session links (`internal_link`) and
/// `#N` into its PR URL, outside code spans/fences and on lines with no
/// existing markdown link. `sessions`: (id, display term) pairs — pass
/// both a session's id and its name as separate terms. `prs`: (number,
/// url), only for numbers that resolve to exactly one known PR.
pub fn linkify(markdown: &str, sessions: &[SessionTerm], prs: &[PrLink]) -> String {
    let mut terms: Vec<&SessionTerm> = sessions
        .iter()
        .filter(|(_, t)| t.chars().count() >= 3)
        .collect();
    terms.sort_by_key(|(_, t)| std::cmp::Reverse(t.len()));

    let mut out = String::with_capacity(markdown.len());
    let mut in_fence = false;
    for (i, line) in markdown.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            out.push_str(line);
            continue;
        }
        if in_fence || line.contains("](") {
            out.push_str(line);
            continue;
        }
        for (j, segment) in line.split('`').enumerate() {
            if j > 0 {
                out.push('`');
            }
            if j % 2 == 1 {
                out.push_str(segment);
            } else {
                linkify_segment(segment, &terms, prs, &mut out);
            }
        }
    }
    out
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '-' || c == '_'
}

fn linkify_segment(segment: &str, terms: &[&SessionTerm], prs: &[PrLink], out: &mut String) {
    let mut i = 0;
    let mut prev: Option<char> = None;
    while i < segment.len() {
        let rest = &segment[i..];
        let at_boundary = !prev.is_some_and(is_word_char);
        if at_boundary {
            let term = terms.iter().find(|(_, t)| {
                rest.starts_with(t.as_str())
                    && !rest[t.len()..].chars().next().is_some_and(is_word_char)
            });
            if let Some((id, t)) = term {
                out.push_str(&format!(
                    "[{t}]({})",
                    crate::components::internal_link::encode(
                        &crate::components::internal_link::InternalLink::Session(id.clone())
                    )
                ));
                i += t.len();
                prev = t.chars().last();
                continue;
            }
            if let Some(digits) = rest.strip_prefix('#') {
                let len = digits.chars().take_while(char::is_ascii_digit).count();
                let number = digits[..len].parse::<u64>().ok();
                let url = number
                    .and_then(|n| prs.iter().find(|(p, _)| *p == n))
                    .map(|(_, u)| u);
                if let (Some(url), false) =
                    (url, digits[len..].chars().next().is_some_and(is_word_char))
                {
                    out.push_str(&format!("[#{}]({url})", &digits[..len]));
                    i += 1 + len;
                    prev = digits[..len].chars().last();
                    continue;
                }
            }
        }
        let c = rest.chars().next().expect("i < len");
        out.push(c);
        i += c.len_utf8();
        prev = Some(c);
    }
}

/// Link terms for `linkify` from the app's sessions and orchestrators,
/// excluding the prompt agent itself.
pub fn link_terms(app: &App) -> (Vec<SessionTerm>, Vec<PrLink>) {
    let mut sessions: Vec<SessionTerm> = Vec::new();
    for s in app
        .sessions
        .values()
        .filter(|s| !ninox_core::worker_type::is_prompt_agent_session(s))
    {
        sessions.push((s.id.clone(), s.id.clone()));
        sessions.push((s.id.clone(), s.name.clone()));
    }
    for o in &app.orchestrators {
        sessions.push((o.id.clone(), o.name.clone()));
    }
    let mut prs: Vec<PrLink> = Vec::new();
    for pr in app.prs.values() {
        if app.prs.values().filter(|p| p.number == pr.number).count() == 1 {
            prs.push((pr.number, pr.url.clone()));
        }
    }
    (sessions, prs)
}

fn header_button<'a>(label: &'a str, key: &'a str, msg: Message) -> Element<'a, Message> {
    let key_font = if key.is_ascii() {
        crate::style::MONO
    } else {
        crate::style::glyph_font_for(key)
    };
    let keycap = container(text(key).size(10).font(key_font).color(tokens::PLACEHOLDER))
        .padding(Padding {
            top: 1.0,
            right: 5.0,
            bottom: 1.0,
            left: 5.0,
        })
        .style(|_| container::Style {
            background: Some(Background::Color(tokens::BG)),
            border: Border {
                color: tokens::BORDER,
                width: 1.0,
                radius: 4.0.into(),
            },
            ..Default::default()
        });
    button(
        row![
            text(label)
                .size(11)
                .font(crate::style::SANS)
                .color(tokens::TEXT),
            Space::new(6, 0),
            keycap
        ]
        .align_y(Alignment::Center),
    )
    .on_press(msg)
    .padding(Padding {
        top: 4.0,
        right: 10.0,
        bottom: 4.0,
        left: 10.0,
    })
    .style(|_t, status| button::Style {
        background: Some(Background::Color(
            if matches!(status, button::Status::Hovered) {
                tokens::BORDER
            } else {
                tokens::PANEL2
            },
        )),
        border: Border {
            color: tokens::BORDER,
            width: 1.0,
            radius: 6.0.into(),
        },
        ..Default::default()
    })
    .into()
}

fn header<'a>(app: &'a App, now_ms: i64) -> Element<'a, Message> {
    let drawer = &app.ninox_reply_drawer;
    let status = drawer.status(now_ms);
    let dot_color = match status {
        DrawerStatus::Answering if App::wants_thinking_tick(app) => {
            Color { a: crate::motion::dot_pulse(app.thinking_tick), ..tokens::TEAL }
        }
        DrawerStatus::Answering => tokens::TEAL,
        DrawerStatus::Relaying => tokens::AMBER,
        DrawerStatus::Idle => tokens::DIM2,
    };
    let dot = container(Space::new(7, 7)).style(move |_| container::Style {
        background: Some(Background::Color(dot_color)),
        border: Border {
            radius: 3.5.into(),
            ..Default::default()
        },
        ..Default::default()
    });
    container(
        row![
            text("❯")
                .size(13)
                .font(crate::style::NERD)
                .color(tokens::VIOLET),
            text("Ninox prompt")
                .size(13)
                .font(crate::style::SANS)
                .color(tokens::TEXT),
            dot,
            text(status_line(status, drawer.context_pct))
                .size(11.5)
                .font(crate::style::SANS)
                .color(tokens::DIM),
            Space::with_width(Length::Fill),
            header_button("Clear context", "⌘⇧K", Message::NinoxPromptClear),
            header_button("Collapse", "esc", Message::NinoxDrawerCollapse),
        ]
        .spacing(10)
        .align_y(Alignment::Center),
    )
    .height(42)
    .width(Length::Fill)
    .align_y(Alignment::Center)
    .padding(Padding {
        top: 0.0,
        right: 16.0,
        bottom: 0.0,
        left: 16.0,
    })
    .into()
}

fn rule<'a>() -> Element<'a, Message> {
    container(Space::new(Length::Fill, 1))
        .style(|_| container::Style {
            background: Some(Background::Color(tokens::BORDER)),
            ..Default::default()
        })
        .into()
}

fn user_bubble<'a>(question: &'a str, max_width: f32) -> Element<'a, Message> {
    let bubble = container(
        text(question)
            .size(12.5)
            .font(crate::style::SANS)
            .color(tokens::TEXT),
    )
    .max_width(max_width)
    .padding(Padding {
        top: 8.0,
        right: 12.0,
        bottom: 8.0,
        left: 12.0,
    })
    .style(|_| container::Style {
        background: Some(Background::Color(Color::from_rgb8(0x1b, 0x22, 0x36))),
        border: Border {
            color: Color::from_rgb8(0x2a, 0x3a, 0x63),
            width: 1.0,
            radius: 10.0.into(),
        },
        ..Default::default()
    });
    let copy = column![bubble, turn_actions(vec![("Copy", Message::CopyToClipboard(question.to_string()), false)])]
        .spacing(3)
        .align_x(Alignment::End);
    row![Space::with_width(Length::Fill), copy].into()
}

/// Small dim text buttons under a message; a `(label, message)` per action.
fn turn_actions<'a>(actions: Vec<(&'static str, Message, bool)>) -> Element<'a, Message> {
    let buttons = actions.into_iter().map(|(label, msg, active)| {
        button(text(label).size(11).font(crate::style::SANS))
            .on_press(msg)
            .padding(Padding { top: 1.0, right: 6.0, bottom: 1.0, left: 6.0 })
            .style(move |_t, status| button::Style {
                text_color: if active {
                    tokens::TEAL
                } else if matches!(status, button::Status::Hovered) {
                    tokens::TEXT
                } else {
                    tokens::DIM2
                },
                ..Default::default()
            })
            .into()
    });
    row(buttons).spacing(2).into()
}

fn target_role(app: &App, id: &str) -> &'static str {
    if app.orchestrators.iter().any(|o| o.id == id) {
        "its orchestrator"
    } else {
        "worker"
    }
}

fn target_name<'a>(app: &'a App, id: &'a str) -> &'a str {
    app.orchestrators
        .iter()
        .find(|o| o.id == id)
        .map(|o| o.name.as_str())
        .or_else(|| app.sessions.get(id).map(|s| s.name.as_str()))
        .unwrap_or(id)
}

fn relay_row<'a>(
    app: &'a App,
    turn: &'a Turn,
    relay: &'a Relay,
    now_ms: i64,
) -> Element<'a, Message> {
    let body = |s: String| {
        text(s)
            .size(13)
            .font(crate::style::SANS)
            .color(tokens::TEXT)
    };
    row![
        body("Asked ".into()),
        text(target_name(app, &relay.to_id))
            .size(13)
            .font(crate::style::SANS)
            .color(tokens::TEXT),
        body(format!(" ({}) — ", target_role(app, &relay.to_id))),
        text(relay_tail(
            turn.relay_state(relay, now_ms),
            relay.relayed_at_ms
        ))
        .size(13)
        .font(crate::style::SANS)
        .color(tokens::DIM),
    ]
    .wrap()
    .into()
}

/// The "is thinking" placeholder as coloured segments: three trailing
/// bullets a teal crest walks across, or a still ellipsis under Reduce Motion.
fn thinking_line(label: &str, tick: u64, reduce_motion: bool) -> Vec<(String, Color)> {
    if reduce_motion {
        return vec![(format!("{label} is thinking…"), tokens::DIM)];
    }
    let mix = |level: f32| Color {
        r: tokens::DIM.r + (tokens::TEAL.r - tokens::DIM.r) * level,
        g: tokens::DIM.g + (tokens::TEAL.g - tokens::DIM.g) * level,
        b: tokens::DIM.b + (tokens::TEAL.b - tokens::DIM.b) * level,
        a: 1.0,
    };
    let mut segments = vec![(format!("{label} is thinking"), tokens::DIM)];
    segments.extend(crate::motion::dot_levels(tick).map(|level| (" •".to_string(), mix(level))));
    segments
}

/// Ghost-plan cards for the plans the prompt agent proposed during `turn`
/// (matched by proposer and the turn's time window).
fn turn_extensions<'a>(app: &'a App, turn: &'a Turn, now_ms: i64) -> Option<Element<'a, Message>> {
    let turns = &app.ninox_reply_drawer.turns;
    let index = turns.iter().position(|t| std::ptr::eq(t, turn))?;
    let asked_at: Vec<i64> = turns.iter().map(|t| t.asked_at_ms).collect();
    let ids = ninox_ui_shared::ghost_view::plans_per_turn(&asked_at, &app.ghost.plans).swap_remove(index);
    if turn.claims_unstored_plan(&ids, &app.ghost.plans, now_ms) {
        return Some(
            text(format!("⚠ {}", ninox_ui_shared::ghost_view::NO_PLAN_STORED))
                .size(12.5)
                .font(crate::style::SANS)
                .color(tokens::RED)
                .into(),
        );
    }
    crate::components::ghost_plans::drawer_cards(app, &ids)
}

fn agent_message<'a>(
    app: &'a App,
    turn: &'a Turn,
    max_width: f32,
    now_ms: i64,
) -> Element<'a, Message> {
    let mut parts: Vec<Element<Message>> = Vec::new();
    let asked_at = turn.asked_at_ms;
    let selecting = app.ninox_turn_select.get(&asked_at);
    if let Some(content) = selecting {
        parts.push(
            crate::components::selectable_markdown::selectable_markdown(content, move |action| {
                Message::NinoxTurnSelectAction(asked_at, action)
            }),
        );
    } else if !turn.rendered.is_empty() {
        parts.push(
            iced::widget::markdown::view(
                &turn.rendered,
                iced::widget::markdown::Settings {
                    code_size: 12.0.into(),
                    ..iced::widget::markdown::Settings::with_text_size(13.0)
                },
                crate::components::markdown_style::inline_code_style(tokens::BLUE, 3.0),
            )
            .map(|url| link_message(&url)),
        );
    } else if turn.state == AnswerState::Pending || turn.state == AnswerState::Streaming {
        let spans: Vec<_> = thinking_line(&turn.target_label, app.thinking_tick, app.reduce_motion)
            .into_iter()
            .map(|(segment, color)| span(segment).color(color))
            .collect();
        parts.push(rich_text(spans).size(13).font(crate::style::SANS).into());
    }
    if !turn.answer.is_empty() {
        let mut actions = vec![("Copy", Message::CopyToClipboard(turn.answer.clone()), false)];
        if selecting.is_some() {
            actions.push(("✓ Selecting — back to formatted view", Message::NinoxTurnSelectToggle(asked_at), true));
        } else {
            actions.push(("Select", Message::NinoxTurnSelectToggle(asked_at), false));
        }
        parts.push(turn_actions(actions));
    }
    if let Some(error) = &turn.error {
        parts.push(
            text(format!("{}: {error}", turn.target_label))
                .size(12.5)
                .font(crate::style::SANS)
                .color(tokens::RED)
                .into(),
        );
    }
    if let Some(extra) = turn_extensions(app, turn, now_ms) {
        parts.push(extra);
    }
    for relay in &turn.relays {
        parts.push(relay_row(app, turn, relay, now_ms));
    }
    container(column(parts).spacing(10))
        .max_width(max_width)
        .into()
}

fn notice<'a>(n: &'a Notice) -> Element<'a, Message> {
    let (label, color) = match n {
        Notice::ContextCleared => (
            "Context cleared — the next question starts a fresh conversation.".to_string(),
            tokens::DIM,
        ),
        Notice::ClearPending => (ninox_ui_shared::prompt_agent::reply::CLEAR_PENDING_TEXT.to_string(), tokens::DIM),
        Notice::Error(e) => (e.clone(), tokens::RED),
    };
    text(label)
        .size(12.5)
        .font(crate::style::SANS)
        .color(color)
        .into()
}

fn drawer_height(window_height: f32) -> f32 {
    DRAWER_HEIGHT.min((window_height - MIN_VISIBLE_ABOVE).max(120.0))
}

/// The expanded drawer, bottom-aligned for `shell` to stack over the
/// centre view. `None` unless open.
pub fn reply_drawer(app: &App) -> Option<Element<'_, Message>> {
    let drawer = &app.ninox_reply_drawer;
    if !drawer.is_open() {
        return None;
    }
    let now_ms = chrono::Utc::now().timestamp_millis();
    let width = (app.window_width - app.sidebar_width).max(400.0);
    let mut items: Vec<Element<Message>> = Vec::new();
    for turn in &drawer.turns {
        items.push(user_bubble(&turn.question, width * 0.7));
        items.push(agent_message(app, turn, width * 0.86, now_ms));
    }
    if let Some(n) = &drawer.notice {
        items.push(notice(n));
    }
    let conversation = scrollable(
        column(items)
            .spacing(14)
            .padding(Padding {
                top: 16.0,
                right: 22.0,
                bottom: 16.0,
                left: 22.0,
            })
            .width(Length::Fill),
    )
    .anchor_bottom()
    .direction(scrollable::Direction::Vertical(
        scrollable::Scrollbar::new().width(6).scroller_width(6),
    ))
    .height(Length::Fill);

    let panel = container(column![header(app, now_ms), rule(), conversation])
        .width(Length::Fill)
        .height(drawer_height(app.window_height))
        .style(|_| container::Style {
            background: Some(Background::Color(tokens::PANEL)),
            border: Border {
                color: Color::from_rgb8(0x2f, 0x37, 0x50),
                width: 1.0,
                radius: iced::border::Radius {
                    top_left: 12.0,
                    top_right: 12.0,
                    bottom_right: 0.0,
                    bottom_left: 0.0,
                },
            },
            shadow: Shadow {
                color: Color {
                    a: 0.5,
                    ..Color::BLACK
                },
                offset: Vector::new(0.0, -12.0),
                blur_radius: 40.0,
            },
            ..Default::default()
        });
    // Swallows presses on the panel's background so they never reach the
    // view stacked underneath.
    let panel = mouse_area(panel).on_press(Message::Noop);

    Some(
        container(panel)
            .width(Length::Fill)
            .height(Length::Fill)
            .align_y(Alignment::End)
            .padding(Padding {
                top: 0.0,
                right: 14.0,
                bottom: 0.0,
                left: 14.0,
            })
            .into(),
    )
}

/// The collapsed drawer: one line above the prompt bar that reopens it.
/// Laid-out height of the collapsed indicator: 8px padding above and below
/// a 12px line (iced lines are 1.3× the size).
pub(crate) const INDICATOR_HEIGHT: f32 = 2.0 * 8.0 + 12.0 * 1.3;

/// Whether the collapsed indicator row sits above the prompt bar. The
/// expanded drawer overlays the view instead, so it takes no height.
pub(crate) fn indicator_shown(app: &App) -> bool {
    let drawer = &app.ninox_reply_drawer;
    !drawer.expanded && drawer.has_content()
}

pub fn collapsed_indicator(app: &App) -> Option<Element<'_, Message>> {
    let drawer = &app.ninox_reply_drawer;
    if !indicator_shown(app) {
        return None;
    }
    let now_ms = chrono::Utc::now().timestamp_millis();
    let line = row![
        text("❯")
            .size(12)
            .font(crate::style::NERD)
            .color(tokens::VIOLET),
        text(drawer.summary_line())
            .size(12)
            .font(crate::style::SANS)
            .color(tokens::TEXT)
            .width(Length::Fill),
        text(drawer.status(now_ms).label())
            .size(11)
            .font(crate::style::SANS)
            .color(tokens::DIM),
        row![
            text("↑")
                .size(11)
                .font(crate::style::glyph_font_for("↑"))
                .color(tokens::DIM),
            text("expand")
                .size(11)
                .font(crate::style::SANS)
                .color(tokens::DIM),
        ]
        .spacing(4),
    ]
    .spacing(10)
    .align_y(Alignment::Center);
    Some(
        button(line)
            .on_press(Message::NinoxDrawerReopen)
            .width(Length::Fill)
            .padding(Padding {
                top: 8.0,
                right: 16.0,
                bottom: 8.0,
                left: 16.0,
            })
            .style(|_t, status| button::Style {
                background: Some(Background::Color(
                    if matches!(status, button::Status::Hovered) {
                        tokens::BORDER
                    } else {
                        tokens::PANEL2
                    },
                )),
                ..Default::default()
            })
            .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_thinking_dots_cycle_while_the_label_holds_still() {
        let frames: Vec<_> = (0..crate::motion::WAVE_FRAMES).map(|t| thinking_line("ninox", t, false)).collect();
        assert!(frames.iter().all(|f| f.len() == 4 && f[0] == ("ninox is thinking".to_string(), tokens::DIM)));
        assert!(frames.iter().all(|f| f[1..].iter().all(|(dot, _)| dot == " •")));
        assert_ne!(frames[1], frames[3], "the crest moves");
        assert_eq!(thinking_line("ninox", 2, false), thinking_line("ninox", 2 + crate::motion::WAVE_FRAMES, false));
    }

    #[test]
    fn reduce_motion_shows_a_still_ellipsis() {
        for tick in 0..20 {
            assert_eq!(thinking_line("ninox", tick, true), [("ninox is thinking…".to_string(), tokens::DIM)]);
        }
    }

    fn sessions() -> Vec<(String, String)> {
        vec![
            ("o1".into(), "Bellhop api crashing".into()),
            ("w1".into(), "mlops-5372-bellhop-dashboard-resources".into()),
            ("w1".into(), "w1".into()),
        ]
    }

    #[test]
    fn linkify_links_session_names_and_known_prs() {
        let prs = vec![(1099, "https://github.com/o/r/pull/1099".to_string())];
        let out = linkify(
            "Work is on **mlops-5372-bellhop-dashboard-resources** (PR #1099, open) and #7.",
            &sessions(),
            &prs,
        );
        assert_eq!(
            out,
            "Work is on **[mlops-5372-bellhop-dashboard-resources](https://ninox.invalid/session/w1)** \
             (PR [#1099](https://github.com/o/r/pull/1099), open) and #7.",
        );
    }

    #[test]
    fn linkify_respects_word_boundaries_and_short_terms() {
        let out = linkify("w1 and w12 and xBellhop api crashing", &sessions(), &[]);
        assert_eq!(
            out, "w1 and w12 and xBellhop api crashing",
            "w1 is under 3 chars; a mid-word match never links"
        );
        let out = linkify("Ask Bellhop api crashing.", &sessions(), &[]);
        assert_eq!(
            out,
            "Ask [Bellhop api crashing](https://ninox.invalid/session/o1)."
        );
    }

    #[test]
    fn linkify_skips_code_and_existing_links() {
        let md = "```\nBellhop api crashing\n```\nrun `Bellhop api crashing` now\n[x](y) Bellhop api crashing";
        assert_eq!(linkify(md, &sessions(), &[]), md);
    }

    #[test]
    fn links_route_sessions_to_the_fleet_and_urls_to_the_browser() {
        let url = |s: &str| iced::widget::markdown::Url::parse(s).unwrap();
        assert!(
            matches!(link_message(&url("https://ninox.invalid/session/w1")), Message::FleetOpenNode(id) if id == "w1")
        );
        assert!(matches!(
            link_message(&url("https://ninox.invalid/brain/b1")),
            Message::BrainLinkClicked(_)
        ));
        assert!(
            matches!(link_message(&url("https://x/y")), Message::OpenUrl(u) if u == "https://x/y")
        );
        for internal in [
            "https://ninox.invalid/session/",
            "https://ninox.invalid/session/a/b",
            "https://ninox.invalid/other/x",
        ] {
            assert!(
                matches!(link_message(&url(internal)), Message::Noop),
                "{internal} must never reach the browser"
            );
        }
    }

    #[test]
    fn drawer_height_matches_the_board_and_clamps_on_short_windows() {
        assert_eq!(drawer_height(944.0), 470.0);
        assert_eq!(drawer_height(500.0), 340.0);
    }
}
