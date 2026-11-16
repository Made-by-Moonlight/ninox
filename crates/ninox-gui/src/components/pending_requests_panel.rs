//! Pending Requests panel — surfaces every open `ninox user-request`
//! escalation (spec: docs/superpowers/specs/2026-10-26-user-request-design.md).
//! Populated purely by polling `Store::open_user_requests(None)` on the
//! app's `PollSessions` tick (no event carries this; see
//! `app::Message::PollSessions` and `app::App::pending_user_requests`).
//! Reached via the nav bar's pending-requests badge
//! (`components::shell::nav`), which is itself the part that's always
//! rendered — the spec's "badge/count always visible in the chrome"
//! requirement — unlike a stale/answered request, which can only ever
//! leave the badge count by being answered or (once the asking session is
//! terminal) dismissed, never by a timeout or a dismiss-all.
//!
//! `sync` rows get an amber outline (vs the neutral `BORDER` of async rows)
//! plus always-visible Approve/Deny buttons; both modes get a freeform
//! answer input, revealed only once the row is clicked
//! (`Message::TogglePendingRequestExpanded`) — a click-driven reveal, not an
//! automatic focus-stealing overlay. This file's `answer_input` and
//! `quick_action` are the only places in this crate that fire
//! `Message::AnswerUserRequest`, which is in turn the only call site of
//! `Store::answer_user_request`.

use ninox_core::{store::UserRequestRow, types::UserRequestMode};
use iced::{
    widget::{button, column, container, mouse_area, row, scrollable, text, text_input, Space},
    Alignment, Background, Border, Color, Element, Length, Padding,
};

use super::{fleet_focus::badge, shell::tokens};
use crate::app::{App, Message};

const RED_BADGE_BG: Color = tokens::hex(0x3a1620);
const RED_BADGE_FG: Color = tokens::hex(0xf58fa3);

fn mode_label(mode: UserRequestMode) -> &'static str {
    match mode {
        UserRequestMode::Sync  => "Sync",
        UserRequestMode::Async => "Async",
    }
}

/// (background, foreground) of the mode badge.
fn mode_badge_colors(mode: UserRequestMode) -> (Color, Color) {
    match mode {
        UserRequestMode::Sync  => (tokens::AMBER_BADGE_BG, tokens::AMBER_BADGE_FG),
        UserRequestMode::Async => (tokens::PANEL, tokens::DIM),
    }
}

/// A small filled pill button — shared shape for Approve/Deny and the
/// freeform input's Send action.
fn action_button<'a>(label: &'a str, color: Color, message: Message) -> Element<'a, Message> {
    button(text(label).size(11).font(crate::style::SANS_BOLD).color(tokens::BG))
        .on_press(message)
        .padding([4, 10])
        .style(move |_theme, _status| button::Style {
            background: Some(Background::Color(color)),
            border: Border { radius: 6.0.into(), ..Default::default() },
            ..Default::default()
        })
        .into()
}

/// The sync row's always-visible quick-action pair — each fires
/// `Message::AnswerUserRequest` directly, no freeform text required.
fn quick_actions<'a>(id: &str) -> Element<'a, Message> {
    row![
        action_button(
            "Approve", tokens::GREEN,
            Message::AnswerUserRequest { id: id.to_string(), answer: "approved".to_string() },
        ),
        Space::new(6, 0),
        action_button(
            "Deny", tokens::RED,
            Message::AnswerUserRequest { id: id.to_string(), answer: "denied".to_string() },
        ),
    ]
    .into()
}

/// Whether `r`'s requester is independently confirmed unable to receive an
/// answer, looked up fresh against `app.sessions` on every render rather
/// than cached at request-creation time: its status is terminal
/// (`SessionStatus::is_terminal()`), or it's gone from the store entirely
/// (`PollSessions` loads requests before sessions, so a live asker is
/// always present). Gates the stale badge and dismiss button below.
pub(crate) fn requester_is_stale(app: &App, r: &UserRequestRow) -> bool {
    app.sessions.get(&r.requester_session_id).is_none_or(|s| s.status.is_terminal())
}

/// The stale row's dismiss control — cancels via
/// `Message::CancelUserRequest`, never answers.
fn dismiss_button<'a>(id: &str) -> Element<'a, Message> {
    action_button("Dismiss", tokens::RED, Message::CancelUserRequest(id.to_string()))
}

/// The freeform answer input, shown only once this row is expanded — fires
/// `Message::AnswerUserRequest` with the typed text on submit. Shared by
/// sync and async rows alike.
fn answer_input<'a>(app: &'a App, id: &str) -> Element<'a, Message> {
    let submit = Message::AnswerUserRequest {
        id: id.to_string(),
        answer: app.sidebar.pending_request_draft.clone(),
    };
    row![
        text_input("type an answer…", &app.sidebar.pending_request_draft)
            .on_input(Message::PendingRequestDraftChanged)
            .on_submit(submit.clone())
            .size(12)
            .padding([4, 6])
            .style(crate::style::v1_underlined_input_style()),
        Space::new(6, 0),
        action_button("Send", tokens::TEAL, submit),
    ]
    .align_y(Alignment::Center)
    .into()
}

/// One pending-request slip: mode stamp + requester, the prompt, (sync
/// only) Approve/Deny, and — once clicked open — the freeform input.
/// Pressing the slip outside those interactive controls toggles the reveal.
fn pending_request_slip<'a>(app: &'a App, r: &'a UserRequestRow) -> Element<'a, Message> {
    let is_sync = r.mode == UserRequestMode::Sync;
    let expanded = app.sidebar.expanded_pending_request.as_deref() == Some(r.id.as_str());
    let stale = requester_is_stale(app, r);

    let mut header = row![
        badge(mode_label(r.mode).to_uppercase(), mode_badge_colors(r.mode).0, mode_badge_colors(r.mode).1),
        Space::new(8, 0),
        text(format!("from {}", r.requester_session_id))
            .size(10).font(crate::style::MONO).color(tokens::DIM2),
    ]
    .align_y(Alignment::Center);
    if stale {
        header = header.push(Space::new(8, 0)).push(badge("STALE".into(), RED_BADGE_BG, RED_BADGE_FG));
    }

    let mut content = column![
        header,
        Space::new(0, 4),
        text(&r.prompt).size(12).font(crate::style::SANS).color(tokens::TEXT),
    ]
    .spacing(2);

    if is_sync {
        content = content.push(Space::new(0, 6)).push(quick_actions(&r.id));
    }
    // A row stays visible (badged, never hidden) the moment its requester
    // goes terminal; dismissal is a separate, explicit human action.
    if stale {
        content = content.push(Space::new(0, 6)).push(dismiss_button(&r.id));
    }
    if expanded {
        content = content.push(Space::new(0, 6)).push(answer_input(app, &r.id));
    }

    let slip = container(content)
        .width(Length::Fill)
        .padding([8, 12])
        .style(move |_| container::Style {
            background: Some(Background::Color(tokens::PANEL2)),
            border: Border {
                color: if is_sync { tokens::AMBER } else { tokens::BORDER },
                width: 1.0,
                radius: 6.0.into(),
            },
            ..Default::default()
        });

    container(mouse_area(slip).on_press(Message::TogglePendingRequestExpanded(r.id.clone())))
        .width(Length::Fill)
        .padding(Padding { top: 0.0, right: 8.0, bottom: 4.0, left: 8.0 })
        .into()
}

pub fn pending_requests_panel(app: &App) -> Element<'_, Message> {
    let header = row![crate::style::micro_label("Pending Requests", tokens::DIM)]
        .align_y(Alignment::Center)
        .padding([8, 12]);

    let errors: Vec<Element<Message>> = [
        app.human_console_error.as_ref().map(|e| format!("Can't answer from this window: {e}")),
        app.user_request_error.clone(),
    ]
    .into_iter()
    .flatten()
    .map(|msg| {
        container(text(msg).size(11).font(crate::style::SANS).color(tokens::RED))
            .padding(Padding { top: 0.0, right: 12.0, bottom: 6.0, left: 12.0 })
            .into()
    })
    .collect();

    let items: Vec<Element<Message>> = if app.pending_user_requests.is_empty() {
        vec![
            container(
                text("No pending requests").size(12).font(crate::style::SANS_ITALIC).color(tokens::DIM2),
            )
            .padding([12, 16])
            .into(),
        ]
    } else {
        app.pending_user_requests.iter().map(|r| pending_request_slip(app, r)).collect()
    };

    container(
        column![
            header,
            column(errors),
            scrollable(column(items).spacing(0)).height(Length::Fixed(220.0)),
        ]
    )
    .width(Length::Fill)
    .style(|_| container::Style {
        background: Some(Background::Color(tokens::PANEL)),
        border: Border { color: tokens::BORDER, width: 1.0, radius: 8.0.into() },
        ..Default::default()
    })
    .into()
}
