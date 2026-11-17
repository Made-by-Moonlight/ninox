//! The v1 session page (`Session.dc.html`): breadcrumb header with badges,
//! live activity and actions, a tab row, and the terminal in a dark bordered
//! frame. Split adds a side panel with PR, definition of done, brief,
//! workspace state and typed-message activity.

use iced::{
    widget::{
        button, column, container, mouse_area, rich_text, row, scrollable, span, text, text_input,
        Space,
    },
    Alignment, Background, Border, Color, Element, Length, Padding,
};

use crate::{
    app::{App, DragTarget, Message},
    components::{
        fleet_detail::{ci_summary_color, diff_line_color, field_box, field_label},
        fleet_focus::{badge, key_hint},
        info_panel::info_panel,
        inspector_panel::inspector_panel,
        shell::tokens,
        terminal::{TerminalWidget, FONT_SIZE},
    },
    style::{MONO, SANS, SANS_BOLD, SANS_SEMIBOLD},
};
use ninox_core::{
    config::AppConfig,
    types::{MessageEvent, Session},
    workspace::WorkspaceFacts,
};

fn resume_label(status: &ninox_core::types::SessionStatus) -> &'static str {
    use ninox_core::types::SessionStatus::*;
    match status {
        Done | Terminated | Interrupted => "Resume",
        _ => "Restart",
    }
}

fn can_resume(session: &Session, config: &AppConfig) -> bool {
    ninox_ui_shared::restart::can_resume(session, config)
}

// ── Layout budget ────────────────────────────────────────────────────────────
//
// `App::resize_terminals` sizes the PTY from the window before iced lays
// anything out, so every row above and below the canvas has a fixed height
// and `TERM_CHROME_H` is their exact sum. Over-estimating only leaves a
// sliver of empty frame; under-estimating clips the live prompt row.
const HEADER_PAD_TOP: f32 = 12.0;
const CRUMB_H: f32 = 18.0;
const CRUMB_GAP: f32 = 10.0;
const TITLE_H: f32 = 30.0;
const TABS_GAP: f32 = 8.0;
const TABS_H: f32 = 38.0;
const RULE: f32 = 1.0;
const STAGE_PAD_V: f32 = 14.0;
const STAGE_PAD_L: f32 = 20.0;
const STAGE_PAD_R: f32 = 16.0;
const FRAME_BAR_H: f32 = 30.0;
const FRAME_FOOT_H: f32 = 36.0;
const TERM_PAD_V: f32 = 8.0;
const TERM_PAD_H: f32 = 10.0;

pub(crate) const HEADER_H: f32 =
    HEADER_PAD_TOP + CRUMB_H + CRUMB_GAP + TITLE_H + TABS_GAP + TABS_H + RULE;

pub(crate) const TERM_CHROME_H: f32 = crate::components::shell::nav::NAV_HEIGHT
    + crate::components::shell::prompt_bar::BAR_HEIGHT
    + HEADER_H
    + 2.0 * STAGE_PAD_V
    + FRAME_BAR_H + RULE
    + 2.0 * TERM_PAD_V
    + RULE + FRAME_FOOT_H
    // Font hinting can round a line a hair taller than this arithmetic.
    + 4.0;

pub(crate) const TERM_CHROME_W: f32 = STAGE_PAD_L + STAGE_PAD_R + 2.0 * TERM_PAD_H;

pub fn composer_input_id() -> text_input::Id {
    text_input::Id::new("session-composer")
}

/// Split's side panel default width (`Session.dc.html`: 360px).
pub const SIDE_PANEL_WIDTH: f32 = 360.0;

const BTN_PRIMARY_BG: Color = tokens::hex(0x123f3b);
const BTN_PRIMARY_BORDER: Color = tokens::hex(0x2a7d75);
const BTN_PRIMARY_FG: Color = tokens::hex(0xbff3ec);
const BTN_WARN_FG: Color = tokens::hex(0xf3c27a);
const BTN_WARN_BORDER: Color = tokens::hex(0x5a4520);
const BTN_DANGER_FG: Color = tokens::hex(0xf7a1b2);
const BTN_DANGER_BORDER: Color = tokens::hex(0x5a2c38);
const HARNESS_BADGE_BG: Color = tokens::hex(0x15231d);
const HARNESS_BADGE_FG: Color = tokens::hex(0x7fd8a8);
const BODY_FG: Color = tokens::hex(0xc8ccd6);
const TAB_IDLE_FG: Color = tokens::hex(0xa3aabb);

/// Panel selection — which view is active in session detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DetailPanel {
    Terminal,
    #[default]
    Split,
    Info,
    Inspector,
    Diff,
    /// Orchestrator-only: terminal + the registered plan doc, side by side —
    /// the same layout/resize mechanics as `Split`, with `plan_pane`
    /// instead of the side panel.
    Plan,
}

impl DetailPanel {
    /// Panels that render the live terminal (and so can hold key focus).
    pub fn shows_terminal(self) -> bool {
        matches!(
            self,
            DetailPanel::Terminal | DetailPanel::Split | DetailPanel::Plan
        )
    }
}

/// The tab row for a session. Orchestrators have no PR/workspace of their
/// own, so they keep Terminal · Plan.
pub fn tabs_for(is_orchestrator: bool) -> &'static [(DetailPanel, &'static str)] {
    if is_orchestrator {
        &[
            (DetailPanel::Terminal, "Terminal"),
            (DetailPanel::Plan, "Plan"),
        ]
    } else {
        &[
            (DetailPanel::Terminal, "Terminal"),
            (DetailPanel::Split, "Split"),
            (DetailPanel::Info, "Info"),
            (DetailPanel::Inspector, "Inspector"),
            (DetailPanel::Diff, "Diff"),
        ]
    }
}

/// What actually renders for a requested panel: the sticky global
/// `worker_panel` can carry a panel the current session doesn't offer.
pub fn effective_panel(panel: DetailPanel, is_orchestrator: bool) -> DetailPanel {
    match (is_orchestrator, panel) {
        (true, DetailPanel::Plan) => DetailPanel::Plan,
        (true, _) => DetailPanel::Terminal,
        (false, DetailPanel::Plan) => DetailPanel::Split,
        (false, other) => other,
    }
}

/// Header and More-menu actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAction {
    Connect,
    SendMessage,
    Refile,
    OpenInEditor,
    PopOut,
    Restart,
    Kill,
    Delete,
}

/// The two actions that wait for an inline Confirm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirmable {
    Restart,
    Kill,
}

/// What pressing `action` sends. Restart and Kill only ask for
/// confirmation; `confirmed_message` is what fires on Confirm. `None` when
/// the action can't apply (no workspace to open).
pub fn action_message(
    action: SessionAction,
    session_id: &str,
    workspace: Option<&str>,
) -> Option<Message> {
    Some(match action {
        SessionAction::Connect => Message::TerminalFocus,
        SessionAction::SendMessage => Message::FleetSendMessageOpen(session_id.to_string()),
        SessionAction::Refile => Message::RefileSession(session_id.to_string()),
        SessionAction::OpenInEditor => Message::OpenInEditor(workspace?.to_string()),
        SessionAction::PopOut => Message::PopOutSession(session_id.to_string()),
        SessionAction::Restart => Message::SessionConfirmAsk(Confirmable::Restart),
        SessionAction::Kill => Message::SessionConfirmAsk(Confirmable::Kill),
        SessionAction::Delete => Message::DeleteAsk(session_id.to_string()),
    })
}

pub fn confirmed_message(what: Confirmable, session_id: &str) -> Message {
    match what {
        Confirmable::Restart => Message::ResumeSession(session_id.to_string()),
        Confirmable::Kill => Message::KillSession(session_id.to_string()),
    }
}

/// The More ▾ menu: label, action, and whether it's enabled.
pub fn more_menu_items(
    session: &Session,
    editor_is_terminal: bool,
) -> [(&'static str, SessionAction, bool); 3] {
    let has_workspace = session.workspace_path.is_some();
    [
        ("Re-file", SessionAction::Refile, has_workspace),
        (
            "Open in editor",
            SessionAction::OpenInEditor,
            has_workspace && !editor_is_terminal,
        ),
        (
            "Delete…",
            SessionAction::Delete,
            ninox_core::events::undeletable_reason(&session.id).is_none(),
        ),
    ]
}

/// `+N −M` across every file in a unified diff.
pub fn diff_totals(diff: &str) -> (usize, usize) {
    ninox_overview::diff::files(diff)
        .iter()
        .fold((0, 0), |(a, r), f| (a + f.added, r + f.removed))
}

/// Worktrees live at `<repo>/.claude/worktrees/<branch>`; `Session` has no
/// branch field of its own.
pub fn branch_from_workspace(session: &Session) -> Option<&str> {
    session
        .workspace_path
        .as_deref()
        .and_then(|p| p.rsplit('/').next())
        .filter(|b| !b.is_empty())
}

/// `Workspace` field: path (repo-relative for ninox worktrees), then
/// clean/dirty and commits ahead once the git facts have loaded.
pub fn workspace_line(path: &str, facts: Option<&WorkspaceFacts>) -> String {
    let shown = path
        .find("/.claude/worktrees/")
        .map_or(path, |i| &path[i + 1..]);
    match facts {
        None => format!("{shown} · checking…"),
        Some(f) => {
            let state = if f.dirty { "dirty" } else { "clean" };
            let n = f.commits_beyond_base;
            let ahead = if n == 1 {
                "1 commit ahead".to_string()
            } else {
                format!("{n} commits ahead")
            };
            format!("{shown} · {state} · {ahead}")
        }
    }
}

/// Before this, a timestamp is seconds or a placeholder, not epoch ms.
const PLAUSIBLE_MS: i64 = 1_000_000_000_000;
const ACTIVITY_LIMIT: usize = 8;
const ACTIVITY_TEXT_MAX: usize = 80;

fn first_line_clipped(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    if line.chars().count() > ACTIVITY_TEXT_MAX {
        format!(
            "{}…",
            line.chars().take(ACTIVITY_TEXT_MAX).collect::<String>()
        )
    } else {
        line.to_string()
    }
}

/// Activity rows `(epoch ms, text)`, oldest first: the spawn, then the most
/// recent typed messages to or from `session`. A message the session sent
/// reads `[kind] text`; one it received names the sender.
pub fn activity_lines(
    session: &Session,
    orchestrator_name: Option<&str>,
    events: &[MessageEvent],
    name_of: impl Fn(&str) -> String,
) -> Vec<(i64, String)> {
    let mut out = Vec::new();
    if session.started_at >= PLAUSIBLE_MS {
        out.push((
            session.started_at,
            match orchestrator_name {
                Some(o) => format!("spawned by {o}"),
                None => "spawned".to_string(),
            },
        ));
    }
    let mine: Vec<&MessageEvent> = events
        .iter()
        .filter(|e| e.to_id == session.id || e.from_id.as_deref() == Some(session.id.as_str()))
        .collect();
    let skip = mine.len().saturating_sub(ACTIVITY_LIMIT);
    for e in mine.into_iter().skip(skip) {
        let body = first_line_clipped(&e.text);
        let line = match e.from_id.as_deref() {
            Some(from) if from == session.id => format!("{} {body}", e.kind.tag()),
            Some(from) => format!("{} {}: {body}", e.kind.tag(), name_of(from)),
            None => format!("{} {body}", e.kind.tag()),
        };
        out.push((e.delivered_at, line));
    }
    out
}

/// The type badge: the worker type in capitals, ORCHESTRATOR for an
/// orchestrator, WORKER when the session predates worker types.
pub fn type_badge_label(session: &Session, is_orchestrator: bool) -> String {
    if is_orchestrator {
        return "ORCHESTRATOR".to_string();
    }
    session
        .worker_type
        .as_deref()
        .filter(|t| !t.is_empty())
        .unwrap_or("worker")
        .to_uppercase()
}

/// `harness · model`, or just the harness when the model is unknown.
pub fn harness_label(session: &Session) -> String {
    match session.model.as_deref().filter(|m| !m.is_empty()) {
        Some(m) => format!("{} · {m}", session.agent_type),
        None => session.agent_type.clone(),
    }
}

/// The header's held-message note: what is waiting for this session, and
/// what it sent that is still waiting on an unreachable target.
pub fn held_label(held: Option<(usize, usize)>, queued_sent: usize) -> Option<String> {
    let (queued, unreachable) = held.unwrap_or((0, 0));
    let mut parts = Vec::new();
    if unreachable > 0 {
        parts.push(format!("✉ {unreachable} undelivered (retrying)"));
    }
    if queued > 0 {
        parts.push(format!("✉ {queued} held until it starts"));
    }
    if queued_sent > 0 {
        parts.push("message queued (target unreachable)".to_string());
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// The live-activity note, shown after a ⚒ while the agent is working.
pub fn live_activity(session: &Session) -> Option<&str> {
    use ninox_core::types::SessionStatus::*;
    if !matches!(session.status, Spawning | Working) {
        return None;
    }
    session
        .activity_note
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
}

/// The terminal frame header's runtime label.
fn backend_label(session: &Session, config: &AppConfig) -> String {
    if session.machine_id.is_some() {
        return "remote".to_string();
    }
    ninox_core::runtime::effective_backend(config.runtime.backend).to_string()
}

fn hm(ms: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|t| t.format("%H:%M").to_string())
        .unwrap_or_default()
}

// ── Widgets ──────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tone {
    Plain,
    Primary,
    Warn,
    Danger,
}

fn btn<'a>(label: Element<'a, Message>, msg: Option<Message>, tone: Tone) -> Element<'a, Message> {
    let enabled = msg.is_some();
    button(label)
        .on_press_maybe(msg)
        .padding([6, 12])
        .style(move |_t, status| {
            let hovered = enabled && matches!(status, button::Status::Hovered);
            let (bg, border, fg) = match tone {
                Tone::Primary => (BTN_PRIMARY_BG, BTN_PRIMARY_BORDER, BTN_PRIMARY_FG),
                Tone::Warn => (tokens::PANEL2, BTN_WARN_BORDER, BTN_WARN_FG),
                Tone::Danger => (tokens::PANEL2, BTN_DANGER_BORDER, BTN_DANGER_FG),
                Tone::Plain => (tokens::PANEL2, tokens::BORDER, tokens::TEXT),
            };
            button::Style {
                background: Some(Background::Color(if hovered {
                    tokens::BLUE_BADGE_BG
                } else {
                    bg
                })),
                text_color: if enabled { fg } else { tokens::DIM2 },
                border: Border {
                    color: border,
                    width: 1.0,
                    radius: 6.0.into(),
                },
                ..Default::default()
            }
        })
        .into()
}

fn btn_text<'a>(label: impl Into<String>, tone: Tone, enabled: bool) -> Element<'a, Message> {
    let fg = match (enabled, tone) {
        (false, _) => tokens::DIM2,
        (true, Tone::Primary) => BTN_PRIMARY_FG,
        (true, Tone::Warn) => BTN_WARN_FG,
        (true, Tone::Danger) => BTN_DANGER_FG,
        (true, Tone::Plain) => tokens::TEXT,
    };
    text(label.into())
        .size(12)
        .font(SANS_SEMIBOLD)
        .color(fg)
        .into()
}

fn dot<'a>(color: Color, size: f32) -> Element<'a, Message> {
    container(Space::new(0, 0))
        .width(Length::Fixed(size))
        .height(Length::Fixed(size))
        .style(move |_| container::Style {
            background: Some(Background::Color(color)),
            border: Border {
                radius: (size / 2.0).into(),
                ..Default::default()
            },
            ..Default::default()
        })
        .into()
}

fn hrule<'a>() -> Element<'a, Message> {
    container(Space::new(Length::Fill, Length::Fixed(RULE)))
        .style(|_| container::Style {
            background: Some(Background::Color(tokens::BORDER)),
            ..Default::default()
        })
        .into()
}

fn crumb_link<'a>(label: String, msg: Message) -> Element<'a, Message> {
    mouse_area(text(label).size(12).font(SANS_SEMIBOLD).color(tokens::BLUE))
        .interaction(iced::mouse::Interaction::Pointer)
        .on_press(msg)
        .into()
}

fn breadcrumb<'a>(app: &'a App, session: &'a Session) -> Element<'a, Message> {
    let sep = || text("›").size(12).font(SANS).color(tokens::DIM2);
    let orch = session
        .orchestrator_id
        .as_deref()
        .and_then(|oid| app.orchestrators.iter().find(|o| o.id == oid));
    let mut items: Vec<Element<Message>> = vec![
        crumb_link("Fleet".into(), Message::NavigateFleet),
        sep().into(),
    ];
    if let Some(o) = orch {
        items.push(crumb_link(
            o.name.clone(),
            Message::FleetOpenNode(o.id.clone()),
        ));
        items.push(sep().into());
    }
    items.push(
        text(session.name.clone())
            .size(12)
            .font(SANS)
            .color(tokens::DIM)
            .into(),
    );
    items.push(Space::new(6, 0).into());
    items.push(key_hint("esc"));
    items.push(
        text("back")
            .size(10.5)
            .font(SANS)
            .color(tokens::DIM2)
            .into(),
    );
    items.push(Space::with_width(Length::Fill).into());
    items.push(
        text(ninox_core::brain_set::session_brains_line(session, &app.config))
            .size(11)
            .font(SANS)
            .color(tokens::DIM)
            .wrapping(text::Wrapping::None)
            .into(),
    );
    row(items)
        .spacing(8)
        .align_y(Alignment::Center)
        .height(Length::Fixed(CRUMB_H))
        .into()
}

/// Rough rendered widths for the identity row's optional parts. iced
/// squeezes an over-full row's trailing children into wrapped fragments
/// rather than clipping them, so parts that don't fit are dropped instead.
const IDENTITY_SPACING: f32 = 12.0;

fn est_text_width(text: &str, size: f32) -> f32 {
    text.chars().count() as f32 * size * 0.58
}

/// How many of the optional `parts` (estimated widths, in display order)
/// fit after the mandatory `base` width.
pub fn parts_that_fit(available: f32, base: f32, parts: &[f32]) -> usize {
    let mut used = base;
    parts
        .iter()
        .take_while(|w| {
            used += IDENTITY_SPACING + **w;
            used <= available
        })
        .count()
}

fn identity<'a>(session: &'a Session, is_orchestrator: bool) -> Element<'a, Message> {
    let status = tokens::status_color(&session.status);
    let name = session.name.clone();
    let type_label = type_badge_label(session, is_orchestrator);
    let harness = harness_label(session);
    let note = live_activity(session).map(str::to_string);
    iced::widget::responsive(move |size| {
        let base = 10.0 + IDENTITY_SPACING + est_text_width(&name, 19.0);
        let mut widths = vec![
            est_text_width(&type_label, 9.5) + 14.0,
            est_text_width(&harness, 9.5) + 14.0,
        ];
        if let Some(n) = &note {
            widths.push(est_text_width(n, 12.0) + 16.0);
        }
        let shown = parts_that_fit(size.width, base, &widths);

        let mut items: Vec<Element<Message>> = vec![
            dot(status, 10.0),
            text(name.clone())
                .size(19)
                .font(SANS_BOLD)
                .color(tokens::TEXT)
                .wrapping(text::Wrapping::None)
                .into(),
        ];
        if shown > 0 {
            items.push(badge(
                type_label.clone(),
                tokens::BLUE_BADGE_BG,
                tokens::BLUE,
            ));
        }
        if shown > 1 {
            items.push(badge(harness.clone(), HARNESS_BADGE_BG, HARNESS_BADGE_FG));
        }
        if let (true, Some(n)) = (shown > 2, &note) {
            items.push(
                rich_text![
                    span("⚒ ").font(crate::style::glyph_font_for("⚒")),
                    span(n.clone())
                ]
                .size(12)
                .font(SANS)
                .color(tokens::TEAL)
                .wrapping(text::Wrapping::None)
                .into(),
            );
        }
        container(
            row(items)
                .spacing(IDENTITY_SPACING)
                .align_y(Alignment::Center),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .align_y(Alignment::Center)
        .clip(true)
        .into()
    })
    .into()
}

fn actions<'a>(app: &'a App, session: &'a Session, is_orchestrator: bool) -> Element<'a, Message> {
    let sid = session.id.as_str();

    if let Some(draft) = app
        .fleet_message_draft
        .as_ref()
        .filter(|(id, _)| id == sid)
        .map(|(_, d)| d.as_str())
    {
        return row![
            text_input("message…", draft)
                .id(composer_input_id())
                .on_input(Message::FleetSendMessageChanged)
                .on_submit(Message::FleetSendMessageSubmit)
                .size(12.5)
                .font(SANS)
                .padding([5, 10])
                .width(Length::Fixed(320.0))
                .style(|_t, _s| text_input::Style {
                    background: Background::Color(tokens::PANEL2),
                    border: Border {
                        color: tokens::TEAL,
                        width: 1.0,
                        radius: 6.0.into()
                    },
                    icon: tokens::DIM,
                    placeholder: tokens::DIM2,
                    value: tokens::TEXT,
                    selection: tokens::VIOLET,
                }),
            btn(
                btn_text("Send", Tone::Primary, true),
                Some(Message::FleetSendMessageSubmit),
                Tone::Primary
            ),
            btn(
                btn_text("Cancel", Tone::Plain, true),
                Some(Message::FleetSendMessageCancel),
                Tone::Plain
            ),
        ]
        .spacing(8)
        .align_y(Alignment::Center)
        .into();
    }

    // An ACP session has no pty to connect to: Send message is its primary action.
    let acp = app.transcripts.get(sid).is_some_and(|e| !e.is_empty());
    let connect = (!acp).then(|| {
        btn(
            row![
                btn_text("Connect", Tone::Primary, true),
                Space::new(6, 0),
                container(text("c").size(10).font(MONO).color(BTN_PRIMARY_FG))
                    .padding([0, 5])
                    .style(|_| container::Style {
                        border: Border {
                            color: BTN_PRIMARY_BORDER,
                            width: 1.0,
                            radius: 4.0.into()
                        },
                        ..Default::default()
                    }),
            ]
            .align_y(Alignment::Center)
            .into(),
            action_message(SessionAction::Connect, sid, None),
            Tone::Primary,
        )
    });
    let send_tone = if acp { Tone::Primary } else { Tone::Plain };
    let mut items: Vec<Element<Message>> = vec![text(format!("${:.2}", session.cost_usd))
        .size(12)
        .font(MONO)
        .color(tokens::DIM)
        .into()];
    items.extend(connect);
    items.extend([
        btn(
            btn_text("Send message", send_tone, true),
            action_message(SessionAction::SendMessage, sid, None),
            send_tone,
        ),
        mouse_area(btn(
            rich_text![
                span("More "),
                span("▾").font(crate::style::glyph_font_for("▾"))
            ]
            .size(12)
            .font(SANS_SEMIBOLD)
            .color(tokens::TEXT)
            .into(),
            Some(Message::SessionMoreToggle),
            Tone::Plain,
        ))
        .on_enter(Message::SessionMoreHover(true))
        .on_exit(Message::SessionMoreHover(false))
        .into(),
    ]);

    match app.session_confirm {
        Some(what) => {
            let q = match what {
                Confirmable::Restart => format!("{}?", resume_label(&session.status)),
                Confirmable::Kill => "Kill?".to_string(),
            };
            let tone = if what == Confirmable::Kill {
                Tone::Danger
            } else {
                Tone::Warn
            };
            items.push(
                text(q)
                    .size(12)
                    .font(SANS_SEMIBOLD)
                    .color(tokens::TEXT)
                    .into(),
            );
            items.push(btn(
                btn_text("Confirm", tone, true),
                Some(Message::SessionConfirmGo),
                tone,
            ));
            items.push(btn(
                btn_text("Cancel", Tone::Plain, true),
                Some(Message::SessionConfirmCancel),
                Tone::Plain,
            ));
        }
        None => {
            let resumable = can_resume(session, &app.config);
            items.push(btn(
                btn_text(resume_label(&session.status), Tone::Warn, resumable),
                resumable
                    .then(|| action_message(SessionAction::Restart, sid, None))
                    .flatten(),
                Tone::Warn,
            ));
            if !is_orchestrator {
                items.push(btn(
                    btn_text("Kill", Tone::Danger, true),
                    action_message(SessionAction::Kill, sid, None),
                    Tone::Danger,
                ));
            }
        }
    }

    row(items).spacing(10).align_y(Alignment::Center).into()
}

fn tab_row<'a>(
    app: &'a App,
    session: &'a Session,
    is_orchestrator: bool,
    active: DetailPanel,
) -> Element<'a, Message> {
    let diff_counts = match app.diffs.get(&session.id) {
        Some(Some(diff)) => Some(diff_totals(diff)),
        _ => None,
    };
    let tabs: Vec<Element<Message>> = tabs_for(is_orchestrator)
        .iter()
        .map(|&(panel, label)| {
            let on = panel == active;
            let mut label_row: Vec<Element<Message>> = vec![text(label)
                .size(12.5)
                .font(SANS_SEMIBOLD)
                .color(if on { tokens::TEXT } else { TAB_IDLE_FG })
                .into()];
            if let (DetailPanel::Diff, Some((a, r))) = (panel, diff_counts) {
                label_row.push(
                    container(
                        text(format!("+{a} −{r}"))
                            .size(10.5)
                            .font(MONO)
                            .color(BODY_FG),
                    )
                    .padding([0, 6])
                    .style(|_| container::Style {
                        background: Some(Background::Color(tokens::PANEL2)),
                        border: Border {
                            color: tokens::BORDER,
                            width: 1.0,
                            radius: 10.0.into(),
                        },
                        ..Default::default()
                    })
                    .into(),
                );
            }
            let underline =
                container(Space::new(Length::Fill, Length::Fixed(2.0))).style(move |_| {
                    container::Style {
                        background: on.then_some(Background::Color(tokens::TEAL)),
                        ..Default::default()
                    }
                });
            button(
                column![
                    container(row(label_row).spacing(6).align_y(Alignment::Center))
                        .height(Length::Fixed(TABS_H - 2.0))
                        .padding([0, 14])
                        .align_y(Alignment::Center),
                    underline,
                ]
                .width(Length::Shrink),
            )
            .on_press(Message::SwitchDetailPanel(panel))
            .padding(0)
            .style(|_t, _s| button::Style {
                background: None,
                ..Default::default()
            })
            .into()
        })
        .collect();
    row(tabs).height(Length::Fixed(TABS_H)).into()
}

fn header<'a>(
    app: &'a App,
    session: &'a Session,
    is_orchestrator: bool,
    active: DetailPanel,
) -> Element<'a, Message> {
    let held = held_label(
        app.held_messages.get(&session.id).copied(),
        app.queued_by_sender.get(&session.id).copied().unwrap_or(0),
    );
    let title = row![identity(session, is_orchestrator)]
        .push_maybe(held.map(|label| text(label).size(12).color(tokens::AMBER_BADGE_FG)))
        .push(actions(app, session, is_orchestrator))
    .spacing(12)
    .align_y(Alignment::Center)
    .height(Length::Fixed(TITLE_H));
    column![
        container(column![
            breadcrumb(app, session),
            Space::new(0, CRUMB_GAP),
            title,
            Space::new(0, TABS_GAP),
            tab_row(app, session, is_orchestrator, active),
        ])
        .padding(Padding {
            top: HEADER_PAD_TOP,
            right: 20.0,
            bottom: 0.0,
            left: 20.0
        }),
        hrule(),
    ]
    .into()
}

/// The More ▾ dropdown, positioned under the More button. Restart and Kill
/// sit to its right, so it's offset from the header's right edge by their
/// width.
fn more_menu<'a>(
    app: &'a App,
    session: &'a Session,
    is_orchestrator: bool,
) -> Element<'a, Message> {
    let editor_is_terminal = ninox_ui_shared::editor::is_terminal(app.config.editor);
    let items: Vec<Element<Message>> = more_menu_items(session, editor_is_terminal)
        .into_iter()
        .map(|(label, action, enabled)| {
            let msg = enabled
                .then(|| action_message(action, &session.id, session.workspace_path.as_deref()))
                .flatten()
                .map(|_| Message::SessionMenuPick(action));
            button(text(label).size(12).font(SANS).color(if enabled {
                tokens::TEXT
            } else {
                tokens::DIM2
            }))
            .on_press_maybe(msg)
            .width(Length::Fill)
            .padding([7, 12])
            .style(move |_t, status| button::Style {
                background: (enabled && matches!(status, button::Status::Hovered))
                    .then_some(Background::Color(tokens::PANEL2)),
                ..Default::default()
            })
            .into()
        })
        .collect();
    let mut body: Vec<Element<Message>> = items;
    if editor_is_terminal && session.workspace_path.is_some() {
        body.push(
            container(
                text("Terminal editors open from the nx TUI's e key")
                    .size(10.5)
                    .font(SANS)
                    .color(tokens::DIM2),
            )
            .padding([4, 12])
            .into(),
        );
    }
    let menu = mouse_area(
        container(column(body))
            .width(Length::Fixed(200.0))
            .padding([4, 0])
            .style(|_| container::Style {
                background: Some(Background::Color(tokens::PANEL)),
                border: Border {
                    color: tokens::BORDER,
                    width: 1.0,
                    radius: 6.0.into(),
                },
                shadow: iced::Shadow {
                    color: Color::from_rgba(0.0, 0.0, 0.0, 0.5),
                    offset: iced::Vector::new(0.0, 6.0),
                    blur_radius: 16.0,
                },
                ..Default::default()
            }),
    )
    .on_enter(Message::SessionMoreHover(true))
    .on_exit(Message::SessionMoreHover(false));
    let right = if is_orchestrator { 106.0 } else { 160.0 };
    container(menu)
        .width(Length::Fill)
        .align_x(Alignment::End)
        .padding(Padding {
            top: HEADER_PAD_TOP + CRUMB_H + CRUMB_GAP + TITLE_H + 2.0,
            right,
            bottom: 0.0,
            left: 0.0,
        })
        .into()
}

fn terminal_pane<'a>(app: &'a App, session: &'a Session) -> Element<'a, Message> {
    let s = &app.scheme;
    let session_id = session.id.as_str();
    if let Some(entries) = app.transcripts.get(session_id).filter(|e| !e.is_empty()) {
        return transcript_pane(session_id, entries);
    }
    let progress = app.attach.get(session_id);
    let waiting_for_first_frame = progress.is_some_and(|p| p.spawned_at.is_some() && !p.got_output);
    let term_state = app.terminals.get(session_id).filter(|_| !waiting_for_first_frame);
    let Some(term_state) = term_state else {
        use ninox_core::types::SessionStatus;
        let centred = |content: Element<'a, Message>| -> Element<'a, Message> {
            container(content)
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .into()
        };
        let exited = matches!(session.status, SessionStatus::Terminated | SessionStatus::Done);
        if let Some(reason) = progress.and_then(|p| p.failure.as_deref()).filter(|_| !exited) {
            return centred(
                column![
                    text("Terminal disconnected").size(13).font(SANS).color(tokens::TEXT),
                    text(reason).size(12).font(SANS).color(tokens::DIM),
                    button(text("Reconnect").size(12).font(SANS))
                        .on_press(Message::ReconnectTerminal(session_id.to_string())),
                ]
                .spacing(8)
                .align_x(Alignment::Center)
                .into(),
            );
        }
        let placeholder = match session.status {
            SessionStatus::Terminated | SessionStatus::Done => "Session exited".to_string(),
            SessionStatus::Interrupted => "Session interrupted — Resume to continue".to_string(),
            _ => crate::attach::placeholder(progress),
        };
        return centred(text(placeholder).size(13).font(SANS).color(tokens::DIM).into());
    };
    let canvas = iced::widget::Canvas::new(TerminalWidget {
        state: term_state,
        session_id: session_id.to_string(),
        font_size: FONT_SIZE,
        terminal_bg: tokens::BG,
        terminal_fg: s.term_fg,
        cursor_color: tokens::TEAL,
        ansi: s.ansi,
        session_ids: app.sessions.keys().cloned().collect(),
        focused: app.terminal_focused,
    })
    .width(Length::Fill)
    .height(Length::Fill);

    // Scrolled into history: new output won't auto-scroll into view, so
    // offer a jump back to the live screen.
    let jump_to_latest: Option<Element<Message>> = term_state.is_scrolled_back().then(|| {
        container(
            button(
                text("↓ Jump to latest")
                    .size(12)
                    .font(SANS)
                    .color(BTN_PRIMARY_FG),
            )
            .on_press(Message::JumpToLatest {
                session_id: session_id.to_string(),
            })
            .padding([6, 14])
            .style(|_t, _s| button::Style {
                background: Some(Background::Color(BTN_PRIMARY_BG)),
                border: Border {
                    color: BTN_PRIMARY_BORDER,
                    width: 1.0,
                    radius: 999.0.into(),
                },
                ..Default::default()
            }),
        )
        .center_x(Length::Fill)
        .align_bottom(Length::Fill)
        .padding(Padding::default().bottom(16))
        .into()
    });

    iced::widget::Stack::new()
        .width(Length::Fill)
        .height(Length::Fill)
        .push(container(canvas).padding([TERM_PAD_V, TERM_PAD_H]))
        .push_maybe(jump_to_latest)
        .into()
}

/// An ACP-native session's structured transcript in place of the terminal —
/// there is no raw pty to show. Pinned to the newest entry, like a tailing log.
fn transcript_pane<'a>(session_id: &str, entries: &'a [ninox_core::runtime::TranscriptEntry]) -> Element<'a, Message> {
    use iced::widget::{rich_text, span};
    use ninox_core::runtime::{ToolCallStatus, TranscriptEntry};
    use ninox_ui_shared::transcript as shared;

    let line = |entry: &'a TranscriptEntry| -> Element<'a, Message> {
        match entry {
            TranscriptEntry::UserMessage(t) => rich_text![
                span("▶ user  ").font(crate::style::SANS_BOLD).color(tokens::TEAL),
                span(t.as_str()).color(tokens::TEXT),
            ]
            .size(12.5)
            .font(SANS)
            .into(),
            TranscriptEntry::AgentText(t) => rich_text![
                span("· agent ").font(crate::style::SANS_BOLD).color(tokens::DIM),
                span(t.as_str()).color(tokens::TEXT),
            ]
            .size(12.5)
            .font(SANS)
            .into(),
            TranscriptEntry::ToolCall { name, status, detail, .. } => {
                let color = match status {
                    ToolCallStatus::Pending => tokens::DIM2,
                    ToolCallStatus::InProgress => tokens::AMBER,
                    ToolCallStatus::Completed => tokens::GREEN,
                    ToolCallStatus::Failed => tokens::RED,
                };
                let icon = shared::status_icon(*status);
                rich_text![
                    span(format!("{icon} ")).font(crate::style::glyph_font_for(icon)).color(color),
                    span(format!("{name} ")).font(crate::style::SANS_BOLD).color(color),
                    span(format!("{}: ", shared::status_word(*status))).color(color),
                    span(detail.as_str()).font(MONO).color(tokens::DIM),
                ]
                .size(12.5)
                .font(SANS)
                .into()
            }
            TranscriptEntry::TurnStarted | TranscriptEntry::TurnEnded => {
                let label = if matches!(entry, TranscriptEntry::TurnStarted) { "turn started" } else { "turn ended" };
                row![
                    container(Space::new(Length::Fill, 1)).style(|_| container::Style {
                        background: Some(Background::Color(tokens::BORDER)),
                        ..Default::default()
                    }),
                    text(label).size(10.5).font(SANS).color(tokens::DIM2),
                    container(Space::new(Length::Fill, 1)).style(|_| container::Style {
                        background: Some(Background::Color(tokens::BORDER)),
                        ..Default::default()
                    }),
                ]
                .spacing(10)
                .align_y(Alignment::Center)
                .into()
            }
        }
    };

    let body = column(entries.iter().map(line).collect::<Vec<_>>()).spacing(8).padding([TERM_PAD_V, TERM_PAD_H]);
    column![
        container(text(shared::BANNER).size(11).font(SANS).color(tokens::DIM2)).padding([8.0, TERM_PAD_H]),
        // Per-session id: each transcript keeps its own scroll position.
        iced::widget::scrollable(body)
            .id(iced::widget::scrollable::Id::new(format!("transcript-{session_id}")))
            .anchor_bottom()
            .width(Length::Fill)
            .height(Length::Fill),
    ]
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

fn term_frame<'a>(app: &'a App, session: &'a Session) -> Element<'a, Message> {
    let (cols, rows) = app
        .terminals
        .get(&session.id)
        .map(|t| t.grid_size())
        .unwrap_or((app.terminal_cols, app.terminal_rows));
    let focused = app.terminal_focused;
    let acp = app.transcripts.get(&session.id).is_some_and(|e| !e.is_empty());

    let mut bar_items: Vec<Element<Message>> = vec![
        dot(tokens::status_color(&session.status), 7.0),
        // The one item that gives way when the window is narrow: the full
        // label, else just the grid size.
        {
            let (full, short) = if acp {
                (format!("acp · {}", session.id), "acp".to_string())
            } else {
                (format!("{} · {} · {cols}×{rows}", backend_label(session, &app.config), session.id), format!("{cols}×{rows}"))
            };
            iced::widget::responsive(move |size| {
                let label = if est_text_width(&full, 11.0) <= size.width {
                    full.clone()
                } else {
                    short.clone()
                };
                container(
                    text(label)
                        .size(11)
                        .font(MONO)
                        .color(tokens::DIM)
                        .wrapping(text::Wrapping::None),
                )
                .height(Length::Fill)
                .align_y(Alignment::Center)
                .clip(true)
                .into()
            })
            .into()
        },
        text(crate::style::stamp_word(&session.status).to_lowercase())
            .size(11)
            .font(SANS)
            .color(tokens::DIM)
            .wrapping(text::Wrapping::None)
            .into(),
    ];
    if focused {
        bar_items.push(key_hint("Ctrl+]"));
        bar_items.push(
            text("release")
                .size(11)
                .font(SANS)
                .color(tokens::DIM2)
                .wrapping(text::Wrapping::None)
                .into(),
        );
    }
    bar_items.push(key_hint("⌘↑"));
    bar_items.push(
        text("scroll")
            .size(11)
            .font(SANS)
            .color(tokens::DIM2)
            .wrapping(text::Wrapping::None)
            .into(),
    );
    bar_items.push(
        button(
            text("Pop out")
                .size(10.5)
                .font(SANS)
                .color(tokens::TEXT)
                .wrapping(text::Wrapping::None),
        )
        .on_press_maybe(action_message(SessionAction::PopOut, &session.id, None))
        .padding([2, 8])
        .style(|_t, status| button::Style {
            background: Some(Background::Color(
                if matches!(status, button::Status::Hovered) {
                    tokens::BLUE_BADGE_BG
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
        .into(),
    );
    let bar = container(row(bar_items).spacing(8).align_y(Alignment::Center))
        .height(Length::Fixed(FRAME_BAR_H))
        .width(Length::Fill)
        .padding([0, 12])
        .align_y(Alignment::Center)
        .clip(true)
        .style(|_| container::Style {
            background: Some(Background::Color(tokens::PANEL)),
            ..Default::default()
        });

    let hint: Element<Message> = if acp {
        text("No terminal to type into · use Send message to interject")
            .size(12)
            .font(SANS)
            .color(tokens::DIM)
            .into()
    } else if focused {
        row![
            text("Typing into this session ·")
                .size(12)
                .font(SANS)
                .color(tokens::DIM),
            key_hint("Ctrl+]"),
            text("to release").size(12).font(SANS).color(tokens::DIM),
        ]
        .spacing(6)
        .align_y(Alignment::Center)
        .into()
    } else {
        row![
            text("Click the terminal or press")
                .size(12)
                .font(SANS)
                .color(tokens::DIM),
            key_hint("c"),
            text("to type into this session")
                .size(12)
                .font(SANS)
                .color(tokens::DIM),
        ]
        .spacing(6)
        .align_y(Alignment::Center)
        .into()
    };
    let foot = container(
        row![
            text("❯")
                .size(13)
                .font(crate::style::NERD)
                .color(tokens::VIOLET),
            hint
        ]
        .spacing(10)
        .align_y(Alignment::Center),
    )
    .height(Length::Fixed(FRAME_FOOT_H))
    .width(Length::Fill)
    .padding([0, 12])
    .align_y(Alignment::Center)
    .clip(true)
    .style(|_| container::Style {
        background: Some(Background::Color(tokens::PANEL)),
        ..Default::default()
    });

    let border = if focused {
        tokens::TEAL
    } else {
        tokens::BORDER
    };
    let frame = container(column![
        bar,
        hrule(),
        terminal_pane(app, session),
        hrule(),
        foot
    ])
    .width(Length::Fill)
    .height(Length::Fill)
    .clip(true)
    .style(move |_| container::Style {
        background: Some(Background::Color(tokens::BG)),
        border: Border {
            color: border,
            width: 1.0,
            radius: 8.0.into(),
        },
        ..Default::default()
    });

    container(frame)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(Padding {
            top: STAGE_PAD_V,
            right: STAGE_PAD_R,
            bottom: STAGE_PAD_V,
            left: STAGE_PAD_L,
        })
        .into()
}

fn side_section<'a>(label: &'static str, body: Element<'a, Message>) -> Vec<Element<'a, Message>> {
    vec![
        Space::new(0, 12).into(),
        field_label(label),
        Space::new(0, 2).into(),
        body,
    ]
}

fn side_panel<'a>(app: &'a App, session: &'a Session) -> Element<'a, Message> {
    let list = app.session_prs.get(&session.id).map(Vec::as_slice).filter(|l| !l.is_empty());
    let mut items: Vec<Element<Message>> = vec![
        text(crate::components::pr_list::heading(list.map_or(1, <[_]>::len)))
            .size(9.5).font(SANS).color(tokens::DIM2).into(),
        Space::new(0, 6).into(),
    ];

    match (list, session.pr_id.and_then(|id| app.prs.get(&id))) {
        (Some(list), _) => items.extend(crate::components::pr_list::rows(list, tokens::BLUE, 12.5, 11.5)),
        (None, Some(pr)) => {
            items.push(
                rich_text![span(format!("#{} {}", pr.number, pr.title))
                    .color(tokens::BLUE)
                    .underline(true)
                    .link(Message::OpenUrl(pr.url.clone()))]
                .size(12.5)
                .font(SANS)
                .into(),
            );
            if let Some(ci) = app.ci_status.get(&pr.id) {
                items.push(
                    text(format!("{}/{} checks passing", ci.passing, ci.total))
                        .size(11.5)
                        .font(SANS)
                        .color(ci_summary_color(ci))
                        .into(),
                );
            }
        }
        (None, None) => {
            let mut parts = vec![span("No PR yet").color(tokens::DIM)];
            if let Some(branch) = branch_from_workspace(session) {
                parts.push(span(" · branch ").color(tokens::DIM));
                parts.push(
                    span(branch.to_string())
                        .color(BODY_FG)
                        .font(MONO)
                        .size(11.5),
                );
            }
            items.push(rich_text(parts).size(12.5).font(SANS).into());
        }
    }

    let dod = ninox_overview::completion::definition_of_done(session);
    let dod_body: Element<Message> = if dod.is_empty() {
        text("No success criteria recorded")
            .size(12)
            .font(SANS)
            .color(tokens::DIM2)
            .into()
    } else {
        column(dod.into_iter().map(|(done, label)| {
            row![
                text(if done { "✓" } else { "○" })
                    .size(12)
                    .font(crate::style::glyph_font_for(if done {
                        "✓"
                    } else {
                        "○"
                    }))
                    .color(if done { tokens::GREEN } else { tokens::DIM2 }),
                text(label).size(12).font(SANS).color(BODY_FG),
            ]
            .spacing(8)
            .into()
        }))
        .spacing(6)
        .into()
    };
    items.extend(side_section("Definition of done", dod_body));

    let brief = session
        .summary
        .as_deref()
        .map(ninox_overview::display::display_brief)
        .filter(|b| !b.is_empty());
    items.extend(side_section(
        "Brief",
        field_box(
            text(brief.unwrap_or_else(|| "—".into()))
                .size(12)
                .font(SANS)
                .color(BODY_FG)
                .into(),
        ),
    ));

    items.extend(side_section(
        "Brains",
        field_box(
            text(ninox_core::brain_set::session_brains_line(session, &app.config))
                .size(12)
                .font(SANS)
                .color(BODY_FG)
                .into(),
        ),
    ));

    let ws = match session.workspace_path.as_deref() {
        Some(path) => workspace_line(path, app.workspace_facts.get(&session.id)),
        None => "No workspace recorded".to_string(),
    };
    items.extend(side_section(
        "Workspace",
        field_box(text(ws).size(11).font(MONO).color(BODY_FG).into()),
    ));

    let orch_name = session
        .orchestrator_id
        .as_deref()
        .and_then(|oid| app.orchestrators.iter().find(|o| o.id == oid))
        .map(|o| o.name.as_str());
    let events = app
        .session_activity
        .get(&session.id)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let name_of = |id: &str| {
        app.sessions
            .get(id)
            .map_or_else(|| id.to_string(), |s| s.name.clone())
    };
    let lines = activity_lines(session, orch_name, events, name_of);
    let activity: Element<Message> = if lines.is_empty() {
        text("No messages yet")
            .size(12)
            .font(SANS)
            .color(tokens::DIM2)
            .into()
    } else {
        column(lines.into_iter().map(|(at, line)| {
            row![
                text(hm(at)).size(11.5).font(MONO).color(tokens::DIM2),
                text(line).size(12).font(SANS).color(BODY_FG),
            ]
            .spacing(10)
            .into()
        }))
        .spacing(6)
        .into()
    };
    items.extend(side_section("Activity", activity));

    container(
        scrollable(
            container(column(items))
                .width(Length::Fill)
                .padding([16, 18]),
        )
        .width(Length::Fill)
        .height(Length::Fill),
    )
    .width(Length::Fixed(app.info_width))
    .height(Length::Fill)
    .style(|_| container::Style {
        background: Some(Background::Color(tokens::PANEL)),
        ..Default::default()
    })
    .into()
}

/// Diff tab: the workspace's unified `git diff`, colored per line.
fn diff_panel<'a>(app: &'a App, session_id: &str) -> Element<'a, Message> {
    // Iced lays out one widget per line with no virtualization, so an
    // enormous diff would otherwise stall layout/paint on every frame.
    const MAX_DIFF_LINES: usize = 4000;

    let body: Element<Message> = match app.diffs.get(session_id) {
        None => text("Loading diff…")
            .size(13)
            .font(MONO)
            .color(tokens::DIM)
            .into(),
        Some(None) => text("No changes yet")
            .size(13)
            .font(MONO)
            .color(tokens::DIM)
            .into(),
        Some(Some(diff)) => {
            let all_lines: Vec<&str> = diff.lines().collect();
            let shown = all_lines.len().min(MAX_DIFF_LINES);
            let mut rows: Vec<Element<Message>> = all_lines[..shown]
                .iter()
                .map(|line| {
                    text(line.to_string())
                        .size(12)
                        .font(MONO)
                        .color(diff_line_color(line))
                        .into()
                })
                .collect();
            if all_lines.len() > MAX_DIFF_LINES {
                rows.push(
                    text(format!(
                        "… {} more lines truncated",
                        all_lines.len() - MAX_DIFF_LINES
                    ))
                    .size(11)
                    .font(MONO)
                    .color(tokens::DIM2)
                    .into(),
                );
            }
            scrollable(column(rows))
                .width(Length::Fill)
                .height(Length::Fill)
                .into()
        }
    };

    container(
        container(body)
            .width(Length::Fill)
            .height(Length::Fill)
            .padding([12, 14])
            .style(|_| container::Style {
                background: Some(Background::Color(tokens::BG)),
                border: Border {
                    color: tokens::BORDER,
                    width: 1.0,
                    radius: 8.0.into(),
                },
                ..Default::default()
            }),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .padding(Padding {
        top: STAGE_PAD_V,
        right: STAGE_PAD_R,
        bottom: STAGE_PAD_V,
        left: STAGE_PAD_L,
    })
    .into()
}

/// Session detail view — header, tabs and the active panel.
pub fn session_detail<'a>(
    app: &'a App,
    session_id: &str,
    panel: &DetailPanel,
) -> Element<'a, Message> {
    let Some(session) = app.sessions.get(session_id) else {
        return container(
            text("Session not found")
                .size(14)
                .font(SANS)
                .color(tokens::DIM),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(20)
        .into();
    };
    let is_orchestrator = app.orchestrators.iter().any(|o| o.id == session_id);
    let active = effective_panel(*panel, is_orchestrator);

    let content: Element<Message> = match active {
        DetailPanel::Terminal => term_frame(app, session),
        DetailPanel::Split => row![
            term_frame(app, session),
            App::drag_handle(DragTarget::InfoPanel, tokens::BORDER),
            side_panel(app, session),
        ]
        .height(Length::Fill)
        .into(),
        DetailPanel::Info => {
            let pr = session.pr_id.and_then(|id| app.prs.get(&id));
            let ci = pr.and_then(|p| app.ci_status.get(&p.id));
            let comments = pr
                .and_then(|p| app.review_threads.get(&p.id))
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            info_panel(pr, ci, comments, &app.comment_editors, ninox_core::brain_set::session_brains_line(session, &app.config))
        }
        DetailPanel::Inspector => inspector_panel(app, session),
        DetailPanel::Diff => diff_panel(app, session_id),
        DetailPanel::Plan => row![
            term_frame(app, session),
            App::drag_handle(DragTarget::InfoPanel, tokens::BORDER),
            container(crate::components::plan_panel::plan_pane(app, session_id, true))
            .width(Length::Fixed(app.info_width))
            .height(Length::Fill),
        ]
        .height(Length::Fill)
        .into(),
    };

    let page: Element<Message> = column![header(app, session, is_orchestrator, active), content]
        .width(Length::Fill)
        .height(Length::Fill)
        .into();

    // Always a stack, so opening the menu doesn't rebuild the page's widget
    // tree (scroll offsets, terminal selection) underneath it.
    let menu: Element<Message> = if app.session_more_open {
        more_menu(app, session, is_orchestrator)
    } else {
        Space::new(0, 0).into()
    };
    iced::widget::stack![page, menu].into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninox_core::types::{MessageKind, SessionStatus};

    fn session_with(
        status: SessionStatus,
        claude_session_id: Option<&str>,
        workspace_path: Option<&str>,
    ) -> Session {
        Session {
            id: "s1".into(),
            orchestrator_id: None,
            name: "s1".into(),
            repo: String::new(),
            status,
            agent_type: "claude-code".into(),
            cost_usd: 0.0,
            started_at: 0,
            pr_number: None,
            pr_id: None,
            workspace_path: workspace_path.map(String::from),
            pid: None,
            model: None,
            context_tokens: None,
            brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None,
            context_total_tokens: None,
            context_window_size: None,
            claude_session_id: claude_session_id.map(String::from),
            summary: None,
            terminal_at: None,
            gate_status: None,
            merged_at: None,
            activity: Default::default(),
            activity_note: None,
            activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None,
            cleanup_at: None,
        }
    }

    fn event(
        id: i64,
        from: Option<&str>,
        to: &str,
        text: &str,
        kind: MessageKind,
        at: i64,
    ) -> MessageEvent {
        MessageEvent {
            id,
            from_id: from.map(String::from),
            to_id: to.into(),
            text: text.into(),
            delivered_at: at,
            kind,
            outcome: Default::default(),
        }
    }

    fn facts(dirty: bool, ahead: u32) -> WorkspaceFacts {
        WorkspaceFacts {
            dirty,
            has_branch: true,
            has_upstream: true,
            unpushed_commit_count: 0,
            commits_beyond_base: ahead,
            merged: false,
            commits_since_merge: 0,
            interrupted: false,
            live_session_row: false,
        }
    }

    #[test]
    fn resume_available_whenever_id_is_stored_regardless_of_status() {
        let _iso = ninox_core::test_isolation::isolate();
        let cfg = AppConfig::default();
        for status in [
            SessionStatus::Terminated,
            SessionStatus::Interrupted,
            SessionStatus::Working,
            SessionStatus::Done,
        ] {
            let s = session_with(status, Some("uuid-1"), Some("/tmp/ws"));
            assert!(
                can_resume(&s, &cfg),
                "expected resumable regardless of status when an id is stored",
            );
        }
    }

    #[test]
    fn resume_labeled_restart_when_alive_resume_when_dead() {
        use SessionStatus::*;
        for status in [
            Spawning,
            Working,
            PrOpen,
            CiFailed,
            ReviewPending,
            Mergeable,
        ] {
            assert_eq!(
                resume_label(&status),
                "Restart",
                "{status:?} should read Restart"
            );
        }
        for status in [Done, Terminated, Interrupted] {
            assert_eq!(
                resume_label(&status),
                "Resume",
                "{status:?} should read Resume"
            );
        }
    }

    #[test]
    fn resume_unavailable_without_a_stored_claude_session_id() {
        let cfg = AppConfig::default();
        let s = session_with(SessionStatus::Terminated, None, Some("/tmp/ws"));
        assert!(!can_resume(&s, &cfg));
    }

    #[test]
    fn resume_unavailable_without_a_workspace() {
        let cfg = AppConfig::default();
        let s = session_with(SessionStatus::Terminated, Some("uuid-1"), None);
        assert!(!can_resume(&s, &cfg));
    }

    #[test]
    fn resume_unavailable_for_harness_without_resume_args() {
        let cfg = AppConfig::default();
        let mut s = session_with(SessionStatus::Terminated, Some("uuid-1"), Some("/tmp/ws"));
        s.agent_type = "codex".into();
        assert!(!can_resume(&s, &cfg));
    }

    #[test]
    fn workers_get_the_five_board_tabs_in_order() {
        let labels: Vec<&str> = tabs_for(false).iter().map(|(_, l)| *l).collect();
        assert_eq!(labels, ["Terminal", "Split", "Info", "Inspector", "Diff"]);
    }

    #[test]
    fn orchestrators_keep_terminal_and_plan() {
        let panels: Vec<DetailPanel> = tabs_for(true).iter().map(|(p, _)| *p).collect();
        assert_eq!(panels, [DetailPanel::Terminal, DetailPanel::Plan]);
    }

    #[test]
    fn effective_panel_never_renders_a_tab_the_session_does_not_offer() {
        for panel in [
            DetailPanel::Terminal,
            DetailPanel::Split,
            DetailPanel::Info,
            DetailPanel::Inspector,
            DetailPanel::Diff,
            DetailPanel::Plan,
        ] {
            for orch in [false, true] {
                let shown = effective_panel(panel, orch);
                assert!(
                    tabs_for(orch).iter().any(|(p, _)| *p == shown),
                    "{panel:?} orch={orch} → {shown:?}"
                );
            }
        }
        assert_eq!(
            effective_panel(DetailPanel::Plan, false),
            DetailPanel::Split
        );
        assert_eq!(
            effective_panel(DetailPanel::Diff, true),
            DetailPanel::Terminal
        );
        assert_eq!(
            effective_panel(DetailPanel::Inspector, false),
            DetailPanel::Inspector
        );
    }

    #[test]
    fn only_terminal_panels_can_hold_key_focus() {
        assert!(DetailPanel::Terminal.shows_terminal());
        assert!(DetailPanel::Split.shows_terminal());
        assert!(DetailPanel::Plan.shows_terminal());
        assert!(!DetailPanel::Info.shows_terminal());
        assert!(!DetailPanel::Inspector.shows_terminal());
        assert!(!DetailPanel::Diff.shows_terminal());
    }

    #[test]
    fn actions_map_to_the_existing_messages() {
        let m = |a| action_message(a, "w1", Some("/ws/w1"));
        assert!(matches!(
            m(SessionAction::Connect),
            Some(Message::TerminalFocus)
        ));
        assert!(
            matches!(m(SessionAction::SendMessage), Some(Message::FleetSendMessageOpen(id)) if id == "w1")
        );
        assert!(matches!(m(SessionAction::Refile), Some(Message::RefileSession(id)) if id == "w1"));
        assert!(
            matches!(m(SessionAction::OpenInEditor), Some(Message::OpenInEditor(p)) if p == "/ws/w1")
        );
        assert!(matches!(m(SessionAction::PopOut), Some(Message::PopOutSession(id)) if id == "w1"));
    }

    #[test]
    fn restart_and_kill_only_ask_for_confirmation() {
        assert!(matches!(
            action_message(SessionAction::Restart, "w1", None),
            Some(Message::SessionConfirmAsk(Confirmable::Restart))
        ));
        assert!(matches!(
            action_message(SessionAction::Kill, "w1", None),
            Some(Message::SessionConfirmAsk(Confirmable::Kill))
        ));
        assert!(
            matches!(confirmed_message(Confirmable::Restart, "w1"), Message::ResumeSession(id) if id == "w1")
        );
        assert!(
            matches!(confirmed_message(Confirmable::Kill, "w1"), Message::KillSession(id) if id == "w1")
        );
    }

    #[test]
    fn open_in_editor_needs_a_workspace() {
        assert!(action_message(SessionAction::OpenInEditor, "w1", None).is_none());
    }

    #[test]
    fn more_menu_holds_refile_open_in_editor_and_delete() {
        let s = session_with(SessionStatus::Working, None, Some("/ws/w1"));
        let items = more_menu_items(&s, false);
        assert_eq!(
            items.map(|(l, a, _)| (l, a)),
            [
                ("Re-file", SessionAction::Refile),
                ("Open in editor", SessionAction::OpenInEditor),
                ("Delete…", SessionAction::Delete),
            ]
        );
        assert!(items.iter().all(|(_, _, enabled)| *enabled));
    }

    #[test]
    fn more_menu_disables_what_cannot_run() {
        let no_ws = session_with(SessionStatus::Working, None, None);
        assert!(more_menu_items(&no_ws, false)[..2]
            .iter()
            .all(|(_, _, enabled)| !enabled));
        let ws = session_with(SessionStatus::Working, None, Some("/ws/w1"));
        let items = more_menu_items(&ws, true);
        assert!(items[0].2, "re-file still works with a terminal editor");
        assert!(
            !items[1].2,
            "a terminal editor has nowhere to open from the desktop app"
        );
    }

    #[test]
    fn diff_totals_sum_every_file() {
        let diff = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1,2 @@\n-a\n+b\n+c\n\
                    diff --git a/y b/y\n--- a/y\n+++ b/y\n@@ -1 +1 @@\n-d\n+e\n";
        assert_eq!(diff_totals(diff), (3, 2));
        assert_eq!(diff_totals(""), (0, 0));
    }

    #[test]
    fn workspace_line_shows_repo_relative_path_state_and_ahead_count() {
        let p = "/Users/x/proj/ninox/.claude/worktrees/v1-foo";
        assert_eq!(
            workspace_line(p, Some(&facts(false, 2))),
            ".claude/worktrees/v1-foo · clean · 2 commits ahead"
        );
        assert_eq!(
            workspace_line(p, Some(&facts(true, 1))),
            ".claude/worktrees/v1-foo · dirty · 1 commit ahead"
        );
        assert_eq!(workspace_line("/tmp/ws", None), "/tmp/ws · checking…");
    }

    #[test]
    fn activity_lists_spawn_then_typed_messages_for_this_session_only() {
        let mut s = session_with(SessionStatus::Working, None, None);
        s.id = "w1".into();
        s.started_at = 1_793_300_000_000;
        let events = vec![
            event(
                1,
                Some("w1"),
                "orch",
                "read-back sent\nmore",
                MessageKind::Status,
                1_793_300_100_000,
            ),
            event(
                2,
                Some("other"),
                "orch",
                "unrelated",
                MessageKind::Freeform,
                1_793_300_150_000,
            ),
            event(
                3,
                Some("orch"),
                "w1",
                "read-back approved",
                MessageKind::Freeform,
                1_793_300_200_000,
            ),
        ];
        let lines = activity_lines(&s, Some("Ninox"), &events, |id| {
            if id == "orch" {
                "Ninox".into()
            } else {
                id.into()
            }
        });
        let text: Vec<&str> = lines.iter().map(|(_, l)| l.as_str()).collect();
        assert_eq!(
            text,
            [
                "spawned by Ninox",
                "[status] read-back sent",
                "[freeform] Ninox: read-back approved"
            ]
        );
        assert_eq!(lines[0].0, s.started_at);
    }

    #[test]
    fn activity_keeps_only_the_latest_messages_and_skips_an_implausible_spawn_time() {
        let mut s = session_with(SessionStatus::Working, None, None);
        s.id = "w1".into();
        let events: Vec<MessageEvent> = (0..20)
            .map(|i| {
                event(
                    i,
                    None,
                    "w1",
                    &format!("m{i}"),
                    MessageKind::System,
                    1_793_300_000_000 + i,
                )
            })
            .collect();
        let lines = activity_lines(&s, None, &events, |id| id.into());
        assert_eq!(lines.len(), ACTIVITY_LIMIT);
        assert_eq!(lines.last().unwrap().1, "[system] m19");
    }

    #[test]
    fn badges_read_type_and_harness_model() {
        let mut s = session_with(SessionStatus::Working, None, None);
        assert_eq!(type_badge_label(&s, false), "WORKER");
        assert_eq!(type_badge_label(&s, true), "ORCHESTRATOR");
        s.worker_type = Some("implementor".into());
        assert_eq!(type_badge_label(&s, false), "IMPLEMENTOR");
        assert_eq!(harness_label(&s), "claude-code");
        s.model = Some("sonnet-5".into());
        assert_eq!(harness_label(&s), "claude-code · sonnet-5");
    }

    #[test]
    fn held_label_names_what_is_waiting_and_what_was_queued() {
        assert_eq!(held_label(None, 0), None);
        assert_eq!(held_label(Some((0, 0)), 0), None);
        assert_eq!(held_label(Some((0, 2)), 0).as_deref(), Some("✉ 2 undelivered (retrying)"));
        assert_eq!(
            held_label(Some((1, 0)), 1).as_deref(),
            Some("✉ 1 held until it starts · message queued (target unreachable)"),
        );
    }

    #[test]
    fn live_activity_only_while_working() {
        let mut s = session_with(SessionStatus::Working, None, None);
        assert_eq!(live_activity(&s), None);
        s.activity_note = Some("running cargo test".into());
        assert_eq!(live_activity(&s), Some("running cargo test"));
        s.status = SessionStatus::PrOpen;
        assert_eq!(live_activity(&s), None);
    }

    #[test]
    fn identity_drops_trailing_parts_that_do_not_fit() {
        assert_eq!(parts_that_fit(1000.0, 300.0, &[80.0, 120.0, 260.0]), 3);
        assert_eq!(parts_that_fit(560.0, 300.0, &[80.0, 120.0, 260.0]), 2);
        assert_eq!(parts_that_fit(400.0, 300.0, &[80.0, 120.0]), 1);
        assert_eq!(parts_that_fit(300.0, 300.0, &[80.0]), 0);
    }

    #[test]
    fn branch_comes_from_the_workspace_leaf() {
        let s = session_with(
            SessionStatus::Working,
            None,
            Some("/r/.claude/worktrees/v1-foo"),
        );
        assert_eq!(branch_from_workspace(&s), Some("v1-foo"));
        assert_eq!(
            branch_from_workspace(&session_with(SessionStatus::Working, None, None)),
            None
        );
    }
}
