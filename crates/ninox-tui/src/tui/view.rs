//! Drawing. Pure over `TuiState` + the frame's `Layout`.
//!
//! Field Notes in a terminal: paper chrome, ink text, one vermilion accent
//! for "this needs you" and focus, status colour only on status glyphs.
//! Structure comes from whitespace and single rules, not boxes.

use ninox_core::types::{ActivityState, GateCheck, SessionStatus, UserRequestMode};
use ninox_overview::islands::{Island, IslandKey};
use ninox_overview::status::{classify_member, MemberStatus};
use ratatui::{
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use super::fleet;
use super::markdown;
use ninox_ui_shared::prompt_agent::reply::{relay_tail, status_line, DrawerStatus, Notice, RelayState};
use super::keys::prefix_label;
use super::layout::{Layout, CHEVRON_W};
use super::palette::Palette;
use super::state::{
    prompt_target_label, Attention, Backend, DeleteConfirm, Focus, Level, Modal, Mode, Pending, PtydState, Row, Section, SideItem, TuiState, View, HELP,
};

pub fn attention_glyph(a: Attention, p: &Palette) -> (&'static str, Color) {
    match a {
        Attention::Blocked => ("◉", p.accent),
        Attention::Done => ("✓", p.pr_open),
        Attention::Working => ("●", p.working),
        Attention::Idle => ("○", p.faint),
        Attention::Unknown => ("·", p.faint),
    }
}

/// A session's glyph: its attention while live, a dimmed end marker once
/// its process is gone (kept visible until reaped).
pub(super) fn status_glyph(st: &TuiState, r: &Row) -> (&'static str, Color) {
    let p = &st.palette;
    if st.backend_of(r.id()) == (Backend::Ptyd { alive: false }) && !r.session.status.is_terminal() {
        return ("⊘", p.faint);
    }
    match r.session.status {
        SessionStatus::Interrupted => ("↻", p.review),
        SessionStatus::Done => ("✓", p.faint),
        SessionStatus::Terminated => ("⊘", p.faint),
        // No self-reported activity: the PR state is the best signal.
        ref status => match (st.attention(r), status) {
            (Attention::Unknown, SessionStatus::CiFailed) => ("✗", p.ci_failed),
            (Attention::Unknown, SessionStatus::PrOpen) => ("○", p.pr_open),
            (Attention::Unknown, SessionStatus::ReviewPending) => ("○", p.review),
            (Attention::Unknown, SessionStatus::Mergeable) => ("◆", p.mergeable),
            (a, _) => attention_glyph(a, p),
        },
    }
}

pub(super) fn ci_glyph(c: &GateCheck, p: &Palette) -> Option<(&'static str, Color)> {
    match c {
        GateCheck::Passing => Some(("✓", p.working)),
        GateCheck::Failing => Some(("✗", p.ci_failed)),
        GateCheck::Pending => Some(("◌", p.faint)),
        GateCheck::Unknown => None,
    }
}

/// The word a status is stamped with (not the enum name).
pub(super) fn status_word(s: &SessionStatus) -> &'static str {
    match s {
        SessionStatus::Spawning => "starting",
        SessionStatus::Working => "working",
        SessionStatus::PrOpen => "PR open",
        SessionStatus::CiFailed => "CI failed",
        SessionStatus::ReviewPending => "in review",
        SessionStatus::Mergeable => "mergeable",
        SessionStatus::Done => "done",
        SessionStatus::Terminated => "ended",
        SessionStatus::Interrupted => "interrupted",
        SessionStatus::AwaitingReview => "for review",
        SessionStatus::Queued => "queued",
    }
}

pub(super) fn activity_label(a: ActivityState) -> &'static str {
    match a {
        ActivityState::Working => "working",
        ActivityState::Idle => "idle",
        ActivityState::Blocked => "blocked",
        ActivityState::Unknown => "—",
    }
}

/// "PR open · idle", without repeating a word or naming an unknown state.
fn state_phrase(r: &Row) -> String {
    let s = &r.session;
    let status = status_word(&s.status);
    match s.activity {
        ActivityState::Unknown => status.to_string(),
        _ if s.status.is_terminal() => status.to_string(),
        a if activity_label(a) == status => status.to_string(),
        a => format!("{status} · {}", activity_label(a)),
    }
}

/// Before September 2001 in milliseconds: never a real session time, but
/// what an unset (0) or seconds-valued timestamp looks like.
const PLAUSIBLE_MS: i64 = 1_000_000_000_000;

/// `ago_opt`, with an unknown time as "—".
pub fn ago(now_ms: i64, t_ms: i64) -> String {
    ago_opt(now_ms, t_ms).unwrap_or_else(|| "—".into())
}

/// `None` for a time too early to be real, for a caller that words it.
pub fn ago_opt(now_ms: i64, t_ms: i64) -> Option<String> {
    if t_ms < PLAUSIBLE_MS {
        return None;
    }
    let s = ((now_ms - t_ms) / 1000).max(0);
    Some(match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    })
}

/// When the session last did something: pane output, else its state
/// change, else its start.
fn last_activity_ms(st: &TuiState, r: &Row) -> Option<i64> {
    let s = &r.session;
    st.panes
        .get(r.id())
        .map(|p| p.last_output_ms as i64)
        .filter(|&t| t > 0)
        .or(s.activity_since)
        .or((s.started_at > 0).then_some(s.started_at))
}

pub(super) fn truncate(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut w = 0;
    for ch in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw + 1 > max {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

pub(super) fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.content.width()).sum()
}

/// `left` then `right` pushed to the far edge of `width` cells; `right` is
/// dropped if both do not fit.
fn spread(mut left: Vec<Span<'static>>, right: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let (lw, rw) = (spans_width(&left), spans_width(&right));
    if lw + rw < width {
        left.push(Span::raw(" ".repeat(width - lw - rw)));
        left.extend(right);
    }
    Line::from(left)
}

fn fill(f: &mut Frame, area: Rect, bg: Color) {
    f.render_widget(Block::default().style(Style::default().bg(bg)), area);
}

/// A friendly centred message for an empty view: a title, then a muted
/// line saying how to fix it, and optionally a second, dimmer line (e.g. a
/// fallback CLI command alongside the primary key hint).
fn empty_state(f: &mut Frame, area: Rect, p: &Palette, title: &str, hint: Vec<Span<'static>>, hint2: Option<Vec<Span<'static>>>) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let mut lines = vec![
        Line::from(Span::styled("⬡", Style::default().fg(p.accent))),
        Line::from(""),
        Line::from(Span::styled(title.to_string(), Style::default().fg(p.ink).add_modifier(Modifier::BOLD))),
        Line::from(hint),
    ];
    if let Some(hint2) = hint2 {
        lines.push(Line::from(hint2));
    }
    let h = (lines.len() as u16).min(area.height);
    let y = area.y + (area.height - h) / 2;
    f.render_widget(
        Paragraph::new(lines).alignment(Alignment::Center).wrap(Wrap { trim: true }),
        Rect { y, height: area.bottom() - y, ..area },
    );
}

/// Greedily keeps `candidates` (priority order, most important first) while
/// their rendered width — `key.len() + 1 + what.len()`, plus a 3-col
/// separator between hints — still fits `total_w` after `prefix_w` (the
/// footer's mode pill). Mirrors `layout::tabs()`'s width-budget pattern: a
/// hint whose key binding still works but silently vanishes from the only
/// on-screen reminder of it is worse than reserving it priority and
/// dropping the least important ones first.
fn fit_hints(prefix_w: u16, total_w: u16, candidates: &[(&str, &'static str)]) -> Vec<(String, &'static str)> {
    let mut out = Vec::new();
    let mut x = prefix_w;
    for (i, (k, what)) in candidates.iter().enumerate() {
        let sep = if i > 0 { 3 } else { 0 };
        let w = k.chars().count() as u16 + 1 + what.chars().count() as u16 + sep;
        if x + w > total_w {
            break;
        }
        x += w;
        out.push((k.to_string(), *what));
    }
    out
}

fn key_hint(p: &Palette, k: &str, what: &str) -> Vec<Span<'static>> {
    vec![
        Span::styled(k.to_string(), Style::default().fg(p.ink_2).add_modifier(Modifier::BOLD)),
        Span::styled(format!(" {what}"), Style::default().fg(p.faint)),
    ]
}

/// Draws the frame; returns the terminal cursor position for the focused pane.
pub fn draw(f: &mut Frame, st: &TuiState, lay: &Layout) {
    let p = &st.palette;
    st.chip_hits.borrow_mut().clear();
    st.chip_scratch.borrow_mut().clear();
    f.render_widget(Block::default().style(Style::default().bg(p.paper).fg(p.ink)), f.area());
    draw_header(f, lay, st);
    let mut cursor = None;
    match st.view {
        View::Fleet if matches!(st.focus, Focus::Pane | Focus::Inspector) => cursor = draw_fleet_pane(f, st, lay),
        View::Fleet => draw_fleet(f, st, lay),
        View::Board => draw_board(f, st, lay),
        // Intentionally minimal, not a placeholder: the real cross-fleet
        // plan backend is a stated non-goal of the design spec (Global
        // Constraints).
        View::Plans if st.ghost_plans.is_empty() => empty_state(f, lay.body, p, "No plans pending", key_hint(p, "ninox prompt", "propose a plan via the prompt on the Fleet tab"), None),
        View::Plans => draw_plans(f, st, lay.body),
        View::Remotes => draw_remotes(f, st, lay),
        View::Brain => draw_brain(f, lay, st),
        View::WorkerTypes => draw_worker_types(f, lay, st),
        View::UserRequests => draw_user_requests(f, lay, st),
        View::Idle => super::idle::draw_idle(f, st, lay.body),
    }
    draw_footer(f, lay.footer, st);
    if let Some(modal) = &st.modal {
        draw_modal(f, f.area(), st, lay, modal);
    } else if let Some(pos) = cursor {
        f.set_cursor_position(pos);
    }
}

fn draw_header(f: &mut Frame, lay: &Layout, st: &TuiState) {
    let p = &st.palette;
    let area = lay.header;
    if area.height == 0 {
        return;
    }
    let mark = Line::from(vec![
        Span::raw(" "),
        Span::styled("⬡ ", Style::default().fg(p.accent)),
        Span::styled("ninox", Style::default().fg(p.ink).add_modifier(Modifier::BOLD)),
    ]);
    f.render_widget(Paragraph::new(mark), area);
    for t in &lay.tabs {
        let on = t.view == st.view;
        let (key, label, bg) = if on {
            (Style::default().fg(p.accent), Style::default().fg(p.ink).add_modifier(Modifier::BOLD), p.highlight)
        } else {
            (Style::default().fg(p.faint), Style::default().fg(p.ink_2), p.paper)
        };
        let tab = Line::from(vec![
            Span::raw(" "),
            Span::styled(t.key.to_string(), key),
            Span::raw(" "),
            Span::styled(t.label, label),
            Span::raw(" "),
        ]);
        f.render_widget(Paragraph::new(tab).style(Style::default().bg(bg)), t.rect);
    }
    if let Some(r) = lay.settings_button {
        let on = matches!(st.modal, Some(Modal::Settings));
        let (glyph, bg) = if on { (Style::default().fg(p.accent), p.highlight) } else { (Style::default().fg(p.ink_2), p.paper) };
        let line = Line::from(vec![Span::raw(" "), Span::styled("⚙ ", glyph), Span::styled("s", Style::default().fg(p.faint)), Span::raw(" ")]);
        f.render_widget(Paragraph::new(line).style(Style::default().bg(bg)), r);
    }

    let dot = |up: bool| Style::default().fg(if up { p.working } else { p.ci_failed });
    let health = vec![
        Span::styled("ptyd ", Style::default().fg(p.faint)),
        Span::styled("●", dot(st.ptyd == PtydState::Up)),
        Span::styled(" engine ", Style::default().fg(p.faint)),
        Span::styled("●", dot(st.daemon_up)),
        Span::raw(" "),
    ];
    let live = st.rows.iter().filter(|r| !r.session.status.is_terminal() && r.session.status != SessionStatus::Queued).count();
    let working = st.rows.iter().filter(|r| !r.session.status.is_terminal() && st.attention(r) == Attention::Working).count();
    let needs = st.needs_rows().len();
    let mut counts = vec![Span::styled(format!("{live} live"), Style::default().fg(p.ink_2))];
    if working > 0 {
        counts.push(Span::styled(" · ", Style::default().fg(p.faint)));
        counts.push(Span::styled("●", Style::default().fg(p.working)));
        counts.push(Span::styled(format!(" {working} working"), Style::default().fg(p.ink_2)));
    }
    if needs > 0 {
        counts.push(Span::styled(" · ", Style::default().fg(p.faint)));
        counts.push(Span::styled(format!("⚑ {needs} need you"), Style::default().fg(p.accent).add_modifier(Modifier::BOLD)));
    }
    let text = Rect { x: lay.header_text_x, width: area.right().saturating_sub(lay.header_text_x), ..area };
    // Widest first; the notification count goes before the needs-you alert.
    let notes = (!st.notifications.is_empty()).then(|| {
        vec![
            Span::styled(" · ", Style::default().fg(p.faint)),
            Span::styled(format!("✦ {} (N)", st.notifications.len()), Style::default().fg(p.ink_2)),
        ]
    });
    // "6/6 workers running locally · 3 orchestrators · 2 queued" stands in for "N live" (it already
    // says how many run); widest first, falling back to the plain counts.
    let sep = || Span::styled(" · ", Style::default().fg(p.faint));
    let limited = (!st.limits_headline.is_empty()).then(|| {
        let mut c = vec![Span::styled(st.limits_headline.clone(), Style::default().fg(p.ink_2))];
        if needs > 0 {
            c.push(sep());
            c.push(Span::styled(format!("⚑ {needs} need you"), Style::default().fg(p.accent).add_modifier(Modifier::BOLD)));
        }
        c
    });
    let with_notes = notes.clone().map(|n| [counts.clone(), n].concat());
    let candidates = [
        limited.clone().zip(notes).map(|(l, n)| [l, n].concat()),
        limited,
        with_notes,
        Some(counts),
    ];
    let right = candidates
        .into_iter()
        .flatten()
        .map(|mut c| {
            c.push(Span::raw("   "));
            c.extend(health.clone());
            c
        })
        .find(|c| spans_width(c) < text.width as usize)
        // Too narrow even for the labels: one dot, red if either is down.
        .unwrap_or_else(|| {
            if spans_width(&health) <= text.width as usize {
                health
            } else {
                vec![Span::styled("●", dot(st.ptyd == PtydState::Up && st.daemon_up))]
            }
        });
    f.render_widget(Paragraph::new(Line::from(right).alignment(Alignment::Right)), text);
}

fn draw_footer(f: &mut Frame, area: Rect, st: &TuiState) {
    let p = &st.palette;
    let pre = prefix_label(st.prefix);
    let (pill, fg, bg) = match st.mode {
        Mode::Prefix => (" PREFIX ", p.paper, p.accent),
        Mode::Scroll => (" SCROLL ", p.paper, p.review),
        Mode::Normal => match (st.view, st.focus) {
            (View::Fleet, Focus::Pane) if st.report_shown() => (" REPORT ", p.ink, p.rule),
            (View::Fleet, Focus::Pane) => (" AGENT ", p.paper, p.working),
            (View::Fleet, Focus::Inspector) => (" INSPECT ", p.paper, p.accent),
            (View::Fleet, _) => (" FLEET ", p.ink, p.rule),
            (View::Board, _) => (" BOARD ", p.ink, p.rule),
            (View::Plans, _) => (" PLANS ", p.ink, p.rule),
            (View::Brain, _) if st.brain.sources.open => (" SOURCES ", p.ink, p.rule),
            (View::Brain, _) => (" BRAIN ", p.ink, p.rule),
            (View::WorkerTypes, _) => (" WORKER TYPES ", p.ink, p.rule),
            (View::Remotes, _) => (" REMOTES ", p.ink, p.rule),
            (View::UserRequests, _) => (" REQUESTS ", p.ink, p.rule),
            (View::Idle, _) => (" IDLE ", p.ink, p.rule),
        },
    };
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(pill, Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
    ];
    // Shown on every other screen when nonempty, and ahead of the
    // notice/hints so a long hint row can never push it off a narrow footer
    // — a pending request needing a human is more durable than either. The
    // requests screen carries its own count, so the footer room goes to its
    // notices there.
    let n = st.pending_user_requests.len();
    if n > 0 && st.view != View::UserRequests {
        spans.push(Span::styled("● ", Style::default().fg(p.accent)));
        spans.push(Span::styled(
            format!("{n} pending user request{} — press u to view", if n == 1 { "" } else { "s" }),
            Style::default().fg(p.ink_2).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw("   "));
    }
    let idle_n = st.idle.report.total;
    if idle_n > 0 && st.view != View::Idle && st.pending_user_requests.is_empty() {
        spans.push(Span::styled("● ", Style::default().fg(p.review)));
        spans.push(Span::styled(format!("{idle_n} idle — 7"), Style::default().fg(p.ink_2)));
        spans.push(Span::raw("   "));
    }
    if let Some(n) = &st.notice {
        let color = match n.level {
            Level::Info => p.ink_2,
            Level::Warn => p.review,
            Level::Error => p.ci_failed,
        };
        spans.push(Span::styled("● ", Style::default().fg(color)));
        spans.push(Span::styled(n.text.clone(), Style::default().fg(p.ink)));
    } else if let Some(text) = &st.config_warning {
        spans.push(Span::styled("● ", Style::default().fg(p.ci_failed)));
        let budget = (area.width as usize).saturating_sub(spans_width(&spans));
        spans.push(Span::styled(truncate(text, budget), Style::default().fg(p.ink)));
    } else if st.server_restarting {
        spans.push(Span::styled("● ", Style::default().fg(p.working)));
        spans.push(Span::styled("Restarting server… sessions keep running", Style::default().fg(p.ink)));
    } else if let Some(alert) = st.server_alerts.first() {
        spans.push(Span::styled("● ", Style::default().fg(p.review)));
        let where_ = match alert.sessions.len() {
            0 => "Settings › Restart server".to_string(),
            1 => "1 affected session · Settings".to_string(),
            n => format!("{n} affected sessions · Settings"),
        };
        let text = format!("{} ({where_})", alert.line());
        let budget = (area.width as usize).saturating_sub(spans_width(&spans));
        spans.push(Span::styled(truncate(&text, budget), Style::default().fg(p.ink)));
    } else {
        let hints: Vec<(String, &str)> = match (st.mode, st.view, st.focus) {
            (Mode::Prefix, ..) => vec![
                ("esc".into(), "cancel"),
                ("d".into(), "detach"),
                ("g".into(), "go to"),
                ("o".into(), "overview"),
                (pre.clone(), "send prefix"),
                ("?".into(), "all keys"),
            ],
            (Mode::Scroll, ..) => {
                vec![("j/k".into(), "line"), ("PgUp/PgDn".into(), "page"), ("g/G".into(), "top/bottom"), ("y".into(), "copy"), ("q".into(), "back to live")]
            }
            (_, View::Idle, _) => super::idle::footer_hints(),
            (_, View::UserRequests, _) if st.user_requests_draft.is_some() => {
                vec![("↵".into(), "submit"), ("esc".into(), "cancel"), ("Ctrl+u".into(), "clear")]
            }
            (_, View::UserRequests, _) => {
                let mut hints = vec![("j/k".into(), "move"), ("↵".into(), "answer (async only)")];
                let selected_is_stale = st.pending_user_requests.get(st.user_requests_selected)
                    .is_some_and(|r| super::user_requests::requester_is_stale(st, &r.requester_session_id));
                if selected_is_stale {
                    hints.push(("x".into(), "dismiss (stale)"));
                }
                hints.push(("u/esc".into(), "back"));
                hints.push(("?".into(), "keys"));
                hints
            }
            (_, View::Fleet, Focus::Inspector) if st.diff.open.is_some() && st.inspector_tab == super::inspector::Tab::Diff => {
                vec![("j/k".into(), "scroll"), ("PgUp/PgDn".into(), "page"), ("esc".into(), "back to files"), ("tab".into(), "sidebar")]
            }
            (_, View::Fleet, Focus::Inspector) => vec![
                ("←/→".into(), "info/diff"),
                ("j/k".into(), "move"),
                ("↵".into(), "open file"),
                ("r".into(), "refresh diff"),
                ("esc".into(), "back to pane"),
                ("tab".into(), "sidebar"),
            ],
            (_, View::Brain, _) if st.brain.sources.open => fit_hints(12, area.width, &[
                ("j/k", "move"),
                ("↵", "details"),
                ("a", "add"),
                ("e", "edit"),
                ("d", "remove"),
                ("t", "on/off"),
                ("s", "sync"),
                ("S", "sync all"),
                ("esc", "back"),
                ("?", "keys"),
            ]),
            (_, View::Brain, _) if st.brain.editing_query => vec![("↵".into(), "search"), ("esc".into(), "stop typing")],
            (_, View::Brain, _) if st.brain.open => {
                vec![("j/k".into(), "scroll"), ("e".into(), "edit"), ("h".into(), "back to list"), ("/".into(), "search"), ("esc".into(), "close")]
            }
            (_, View::Brain, _) => fit_hints(10, area.width, &[
                ("j/k", "move"),
                ("↵", "read"),
                ("S", "sources"),
                ("L", "links"),
                ("[ / ]", "kind"),
                ("s", "sort"),
                ("x", "clear"),
                ("/", "search"),
                ("space", "fold"),
                ("t", "group"),
                ("e", "edit"),
                ("a", "new"),
                ("D", "delete"),
                ("c", "catalogue"),
            ]),
            (_, View::Fleet, focus) if st.report_shown() => {
                let buttons = st.selected_row().map(|r| super::report::buttons(st, r)).unwrap_or_default();
                let mut v: Vec<(String, &str)> = Vec::new();
                for b in buttons {
                    v.push((b.key().into(), match b {
                        super::report::ReportButton::Remove => "remove",
                        super::report::ReportButton::Resume => "resume",
                        super::report::ReportButton::OpenPr => "open PR",
                    }));
                }
                if focus == Focus::Pane {
                    v.extend([("j/k".into(), "scroll"), ("esc".into(), "back")]);
                } else {
                    v.extend([("j/k".into(), "move"), ("↵".into(), "report")]);
                }
                v.push(("?".into(), "keys"));
                v
            }
            (_, View::Fleet, Focus::Pane) => vec![
                ("typing".into(), "goes to the agent"),
                ("Ctrl+]".into(), "back to the fleet"),
                ("Ctrl+] then x".into(), "kill"),
                ("Ctrl+] then ?".into(), "all keys"),
            ],
            (_, View::Fleet, _) if st.fleet.prompt_editing && st.fleet.reply.is_open() => vec![
                ("↵".into(), "send"),
                ("PgUp/PgDn".into(), "scroll reply"),
                ("ctrl+l".into(), "clear"),
                ("esc".into(), "to reply"),
            ],
            (_, View::Fleet, _) if st.fleet.prompt_editing => vec![("↵".into(), "send"), ("esc".into(), "cancel")],
            (_, View::Fleet, _) if st.fleet.reply.focused && st.fleet.reply.is_open() => vec![
                ("j/k".into(), "scroll"),
                ("PgUp/PgDn".into(), "page"),
                ("g/G".into(), "top/latest"),
                ("/".into(), "ask"),
                ("ctrl+l".into(), "clear"),
                ("esc".into(), "collapse"),
            ],
            (_, View::Fleet, _) if st.rows.is_empty() => vec![("n".into(), "spawn"), ("?".into(), "keys"), ("q".into(), "detach")],
            (_, View::Fleet, _) => fleet_footer_hints(st),
            (_, View::Board, _) => vec![
                ("h/l".into(), "column"),
                ("j/k".into(), "row"),
                ("↵/c".into(), "open"),
                ("esc".into(), "back"),
                ("?".into(), "keys"),
            ],
            (_, View::Plans, _) => vec![("/".into(), "ask ninox (on the Fleet tab)"), ("esc".into(), "back")],
            (_, View::Remotes, _) => {
                let mut v = vec![("a".into(), "add remote")];
                if !st.remote_rows.is_empty() {
                    v.push(("d".into(), "remove"));
                }
                v.push(("esc".into(), "back"));
                v
            }
            (_, View::WorkerTypes, _) if st.worker_types.editing.is_some() => {
                vec![("↵".into(), "save"), ("esc".into(), "cancel"), ("Ctrl+u".into(), "clear")]
            }
            (_, View::WorkerTypes, _) => vec![
                ("tab".into(), "list/form"),
                ("j/k".into(), "move"),
                ("↵/space".into(), "change"),
                ("n".into(), "new"),
                ("D".into(), "delete"),
                ("x".into(), "export"),
                ("i".into(), "import"),
                ("esc".into(), "back"),
            ],
        };
        for (i, (k, what)) in hints.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw("   "));
            }
            spans.extend(key_hint(p, k, what));
        }
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Where a session line is drawn: decides indent and what goes right.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LineKind {
    /// In the fleet tree (orchestrators are group headers).
    Tree,
    /// Under "Needs you": the reason on the right.
    Pinned,
    /// A flat list (the goto picker).
    Flat,
}

/// "2 working · 1 blocked", or `●2 ◉1` when that would not fit.
fn group_summary(st: &TuiState, group: &str, room: usize) -> Vec<Span<'static>> {
    let p = &st.palette;
    let workers: Vec<&Row> = st.rows.iter().filter(|w| w.group.as_deref() == Some(group) && !w.is_orchestrator).collect();
    let live: Vec<&&Row> = workers.iter().filter(|w| !w.session.status.is_terminal()).collect();
    let ended = workers.len() - live.len();
    let count = |a: Attention| live.iter().filter(|w| st.attention(w) == a).count();
    let mut parts: Vec<(usize, &str, &str, Color)> = vec![
        (count(Attention::Blocked), "blocked", "◉", p.accent),
        (count(Attention::Working), "working", "●", p.working),
        (count(Attention::Done), "done", "✓", p.pr_open),
        (count(Attention::Idle) + count(Attention::Unknown), "idle", "○", p.faint),
        (ended, "ended", "⊘", p.faint),
    ];
    parts.retain(|(n, ..)| *n > 0);
    if parts.is_empty() {
        return vec![Span::styled("no workers", Style::default().fg(p.faint))];
    }
    let mut long = Vec::new();
    for (i, (n, word, _, color)) in parts.iter().enumerate() {
        if i > 0 {
            long.push(Span::styled(" · ", Style::default().fg(p.faint)));
        }
        let style = if *word == "blocked" { Style::default().fg(*color) } else { Style::default().fg(p.faint) };
        long.push(Span::styled(format!("{n} {word}"), style));
    }
    if spans_width(&long) <= room {
        return long;
    }
    let mut short = Vec::new();
    for (i, (n, _, glyph, color)) in parts.iter().enumerate() {
        if i > 0 {
            short.push(Span::raw(" "));
        }
        short.push(Span::styled(format!("{glyph}{n}"), Style::default().fg(*color)));
    }
    short
}

/// Right-hand metadata of a session line, most important first; pieces are
/// dropped from the back until the name keeps a readable width.
fn row_meta(st: &TuiState, r: &Row, kind: LineKind) -> Vec<Vec<Span<'static>>> {
    let p = &st.palette;
    let s = &r.session;
    let faint = Style::default().fg(p.faint);
    let mut pieces: Vec<Vec<Span<'static>>> = Vec::new();
    if kind == LineKind::Pinned {
        if let Some(need) = st.needs_you(r) {
            pieces.push(vec![Span::styled(need.label(), Style::default().fg(p.accent))]);
        }
    } else if s.status.is_terminal() {
        let color = if s.status == SessionStatus::Interrupted { p.review } else { p.faint };
        pieces.push(vec![Span::styled(status_word(&s.status).to_string(), Style::default().fg(color))]);
    } else {
        let unread = st.unread(r.id());
        if unread > 0 {
            pieces.push(vec![Span::styled(format!("✉{unread}"), Style::default().fg(p.accent))]);
        }
    }
    if let Some(pr) = s.pr_number {
        let mut piece = vec![Span::styled(format!("#{pr}"), Style::default().fg(p.ink_2))];
        if let Some((g, c)) = s.gate_status.as_ref().and_then(|g| ci_glyph(&g.ci, p)) {
            piece.push(Span::styled(format!(" {g}"), Style::default().fg(c)));
        }
        pieces.push(piece);
    }
    if let Some(t) = last_activity_ms(st, r) {
        pieces.push(vec![Span::styled(ago(st.now_ms, t), faint)]);
    }
    if st.ptyd == PtydState::Up && st.backend_of(r.id()) == Backend::Legacy && !s.status.is_terminal() {
        pieces.push(vec![Span::styled("tmux", faint)]);
    }
    if s.cost_usd >= 0.005 {
        pieces.push(vec![Span::styled(format!("${:.2}", s.cost_usd), faint)]);
    }
    pieces
}

const NAME_MIN: usize = 10;

fn session_line(st: &TuiState, idx: usize, width: usize, kind: LineKind, selected: bool) -> Line<'static> {
    let p = &st.palette;
    let Some(r) = st.rows.get(idx) else { return Line::from("") };
    let s = &r.session;
    let terminal = s.status.is_terminal() && s.status != SessionStatus::Interrupted;
    let header = kind == LineKind::Tree && r.is_orchestrator;
    // `ink_2` rather than `faint` while selected — the sidebar fills a
    // selected row's background with `p.highlight`, and `faint` is
    // deliberately the same colour as `highlight` (see
    // `Palette::terminal`'s doc comment).
    let dim_fg = if selected { p.ink_2 } else { p.faint };
    // A folded group shows its most urgent member on the header.
    let (glyph, gcolor) = if header && st.collapsed.contains(r.id()) && !s.status.is_terminal() {
        attention_glyph(st.rollup(r.id()), p)
    } else {
        status_glyph(st, r)
    };
    let mut left: Vec<Span<'static>> = Vec::new();
    match kind {
        LineKind::Tree if r.is_orchestrator => {
            let folded = st.collapsed.contains(r.id());
            left.push(Span::styled(if folded { "▸ " } else { "▾ " }, Style::default().fg(dim_fg)));
        }
        LineKind::Tree if r.group.is_some() => left.push(Span::raw("    ")),
        LineKind::Flat if r.group.is_some() && !r.is_orchestrator => left.push(Span::raw("  ")),
        _ => left.push(Span::raw("  ")),
    }
    left.push(Span::styled(format!("{glyph} "), Style::default().fg(gcolor)));

    let mut right: Vec<Span<'static>> = Vec::new();
    let prefix_w = spans_width(&left);
    let room = width.saturating_sub(prefix_w + NAME_MIN + 1);
    if r.is_orchestrator && kind != LineKind::Pinned {
        right = group_summary(st, r.id(), room);
        if spans_width(&right) > room {
            right.clear();
        }
    } else {
        for piece in row_meta(st, r, kind) {
            let sep = usize::from(!right.is_empty()) * 2;
            if spans_width(&right) + sep + spans_width(&piece) > room {
                break;
            }
            if sep > 0 {
                right.push(Span::raw("  "));
            }
            right.extend(piece);
        }
    }

    let rw = spans_width(&right);
    let name_w = width.saturating_sub(prefix_w + rw + usize::from(rw > 0));
    let name = truncate(&s.name, name_w);
    let mut name_style = Style::default().fg(p.ink);
    if header {
        name_style = name_style.add_modifier(Modifier::BOLD);
    }
    if terminal {
        name_style = Style::default().fg(dim_fg);
    }
    left.push(Span::styled(name, name_style));
    let mut line = spread(left, right, width);
    // The metadata pieces (`row_meta`, `group_summary`) and a done status
    // glyph are styled for an unselected row; on the selected row's
    // `highlight` fill any of them coloured exactly `highlight` vanishes.
    if selected {
        for span in &mut line.spans {
            if span.style.fg == Some(p.highlight) {
                span.style.fg = Some(p.ink_2);
            }
        }
    }
    line
}

/// One sidebar line: full-width background when selected, with an accent
/// bar in the padding column while the sidebar has focus.
fn draw_side_item(f: &mut Frame, st: &TuiState, lay: &Layout, side: Rect, area: Rect, y: u16, item: &SideItem) {
    let p = &st.palette;
    let w = area.width as usize;
    let line_rect = Rect { y, height: 1, ..area };
    let line = match item {
        SideItem::Gap => return,
        SideItem::Heading(Section::NeedsYou) => {
            let n = st.needs_rows().len();
            Line::from(vec![
                Span::styled("⚑ NEEDS YOU", Style::default().fg(p.accent).add_modifier(Modifier::BOLD)),
                Span::styled(format!("  {n}"), Style::default().fg(p.faint)),
            ])
        }
        SideItem::Heading(Section::Standalone) => {
            Line::from(Span::styled("STANDALONE", Style::default().fg(p.faint).add_modifier(Modifier::BOLD)))
        }
        SideItem::More(n) => Line::from(Span::styled(format!("  + {n} more · g to find"), Style::default().fg(p.faint))),
        SideItem::Group(g) => {
            let folded = st.collapsed.contains(g);
            let left = vec![
                Span::styled(if folded { "▸ " } else { "▾ " }, Style::default().fg(p.faint)),
                Span::styled(truncate(g, w.saturating_sub(CHEVRON_W as usize + NAME_MIN)), Style::default().fg(p.ink_2).add_modifier(Modifier::BOLD)),
            ];
            let room = w.saturating_sub(spans_width(&left) + 1);
            spread(left, group_summary(st, g, room), w)
        }
        SideItem::Session { row, pinned } => {
            let kind = if *pinned { LineKind::Pinned } else { LineKind::Tree };
            let selected = *row == st.selected && *pinned == st.cursor_pinned();
            // The selected row gives its right edge to the `✕` (kill/remove).
            let close = lay.sidebar_close.filter(|c| selected && c.y == y);
            let line = session_line(st, *row, w - close.map_or(0, |c| c.width as usize), kind, selected);
            if selected {
                let row_rect = Rect { x: side.x, y, width: area.right() + 1 - side.x, height: 1 };
                fill(f, row_rect, p.highlight);
                if st.focus == Focus::Sidebar && st.view == View::Board {
                    f.render_widget(Paragraph::new(Span::styled("▌", Style::default().fg(p.accent))), Rect { width: 1, ..row_rect });
                }
            }
            if let Some(c) = close {
                let glyph = Span::styled("✕", Style::default().fg(p.ci_failed).add_modifier(Modifier::BOLD));
                f.render_widget(Paragraph::new(Line::from(vec![Span::raw(" "), glyph])), c);
            }
            line
        }
    };
    f.render_widget(Paragraph::new(line), line_rect);
}

fn draw_sidebar(f: &mut Frame, st: &TuiState, lay: &Layout) {
    let p = &st.palette;
    let (Some(side), Some(list)) = (lay.sidebar, lay.sidebar_list) else { return };
    fill(f, side, p.paper_2);
    let split = lay.pane.is_some_and(|pane| pane.x >= side.right());
    if split {
        let rule = Rect { x: side.right() - 1, width: 1, ..side };
        let bar = vec![Line::from(Span::styled("│", Style::default().fg(p.rule))); rule.height as usize];
        f.render_widget(Paragraph::new(bar).style(Style::default().bg(p.paper)), rule);
    }
    if st.rows.is_empty() {
        empty_state(f, list, p, "No sessions yet", key_hint(p, "n", "spawn"), None);
        return;
    }
    if let Some(needs) = lay.sidebar_needs {
        for (k, item) in lay.needs_items.iter().enumerate().take(needs.height as usize) {
            draw_side_item(f, st, lay, side, needs, needs.y + k as u16, item);
        }
    }
    for (k, item) in lay.sidebar_items.iter().skip(lay.sidebar_offset).take(list.height as usize).enumerate() {
        draw_side_item(f, st, lay, side, list, list.y + k as u16, item);
    }
}

/// The pane's header row: `◀ fleet`, then who this is and what state it is
/// in; dimmed while the pane is unfocused. `tag` sits at the right edge.
fn pane_header(st: &TuiState, r: &Row, width: usize, lay: &Layout, focused: bool, tag: Option<(String, Color)>) -> Line<'static> {
    let p = &st.palette;
    let s = &r.session;
    let (glyph, color) = status_glyph(st, r);
    let text = if focused { p.ink } else { p.ink_2 };
    let mut left = Vec::new();
    if lay.pane_back.is_some() {
        let style = if focused {
            Style::default().fg(p.accent).bg(p.card).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(p.faint).bg(p.paper_2)
        };
        left.push(Span::styled(super::layout::BACK_LABEL, style));
        left.push(Span::raw(" "));
        if lay.pane_kill.is_some() {
            let button = Style::default().fg(p.ink_2).bg(p.paper_2);
            left.push(Span::styled(super::layout::KILL_LABEL, button.fg(p.ci_failed)));
            left.push(Span::raw(" "));
            let zoom = if st.zoom { Style::default().fg(p.paper).bg(p.accent) } else { button };
            left.push(Span::styled(super::layout::ZOOM_LABEL, zoom));
        }
        left.push(Span::raw(" "));
    } else {
        left.push(Span::raw(" "));
    }
    left.push(Span::styled(format!("{glyph} "), Style::default().fg(color)));
    let mut name_style = Style::default().fg(text);
    if focused {
        name_style = name_style.add_modifier(Modifier::BOLD);
    }
    let mut info: Vec<Span<'static>> = vec![Span::styled(format!("  {}", state_phrase(r)), Style::default().fg(p.faint))];
    if let Some(note) = &s.activity_note {
        info.push(Span::styled(format!(": {}", truncate(note, 40)), Style::default().fg(p.faint)));
    }
    if let Some(pr) = s.pr_number {
        info.push(Span::styled(format!("  #{pr}"), Style::default().fg(p.ink_2)));
        if let Some((g, c)) = s.gate_status.as_ref().and_then(|g| ci_glyph(&g.ci, p)) {
            info.push(Span::styled(format!(" {g}"), Style::default().fg(c)));
        }
    }
    if let Some(dir) = s.workspace_path.as_deref().and_then(|w| std::path::Path::new(w).file_name()).and_then(|n| n.to_str()) {
        if !r.is_orchestrator {
            info.push(Span::styled(format!("  ⎇ {dir}"), Style::default().fg(p.faint)));
        }
    }
    let right: Vec<Span<'static>> = tag.map(|(t, c)| vec![Span::styled(t, Style::default().fg(c)), Span::raw(" ")]).unwrap_or_default();
    let fixed = spans_width(&left) + spans_width(&right);
    let name = truncate(&s.name, width.saturating_sub(fixed + 1).max(1));
    left.push(Span::styled(name, name_style));
    // Info is the first thing to give way on a narrow pane.
    let mut used = spans_width(&left) + spans_width(&right);
    for span in info {
        let w = span.content.width();
        if used + w + 1 > width {
            break;
        }
        used += w;
        left.push(span);
    }
    if right.is_empty() {
        return Line::from(left);
    }
    spread(left, right, width)
}

fn draw_fleet_pane(f: &mut Frame, st: &TuiState, lay: &Layout) -> Option<ratatui::layout::Position> {
    let p = &st.palette;
    draw_sidebar(f, st, lay);
    if let Some(insp) = lay.inspector {
        draw_inspector(f, insp, st, lay);
    }
    let (Some(outer), Some(inner)) = (lay.pane, lay.pane_inner) else { return None };
    let focused = st.focus == Focus::Pane;
    let Some(row) = st.selected_row() else {
        empty_state(f, outer, p, "No agents running yet", {
            let mut h = key_hint(p, "n", "spawn");
            h.push(Span::styled("  ·  from an agent pane: Ctrl+] then n", Style::default().fg(p.faint)));
            h
        }, None);
        return None;
    };
    let id = row.id();
    let view = st.views.get(id);
    let target = st.pane_target(id);
    let target_view = target.as_deref().and_then(|t| st.views.get(t));
    let mut tag = None;
    match st.backend_of(id) {
        Backend::Ptyd { alive } => {
            if !alive {
                let code = st.panes.get(id).and_then(|p| p.exit_code).map(|c| format!(" {c}")).unwrap_or_default();
                tag = Some((format!("exited{code}"), p.ci_failed));
            } else if let Some(v) = view {
                if v.is_restored() {
                    tag = Some(("◷ restored — waiting for agent".to_string(), p.review));
                } else if v.scroll > 0 {
                    tag = Some((format!("scroll +{}", v.scroll), p.review));
                }
            }
        }
        Backend::Legacy if !row.session.status.is_terminal() => tag = Some(("tmux".to_string(), p.faint)),
        Backend::Legacy => {}
    }
    let head = Rect { height: 1, ..outer };
    f.render_widget(Paragraph::new(pane_header(st, row, head.width as usize, lay, focused, tag)), head);
    let rule = Rect { y: outer.y + 1, height: 1, ..outer };
    let rule_style = Style::default().fg(if focused { p.accent } else { p.rule });
    f.render_widget(Paragraph::new(Span::styled("─".repeat(rule.width as usize), rule_style)), rule);

    if super::report::shows_report(st, row) {
        draw_report(f, st, lay, row, inner);
        return None;
    }
    if let Some(entries) = st.transcripts.get(id).filter(|v| !v.is_empty()) {
        draw_transcript(f, st, inner, entries);
        return None;
    }
    let live_view = target_view.filter(|v| st.backend_of(id) != Backend::Legacy || v.live.as_ref().is_some_and(super::pane::has_content));
    let cursor = match (st.backend_of(id), live_view) {
        (_, Some(v)) => v.render(inner, f.buffer_mut(), focused && st.mode != Mode::Scroll),
        (Backend::Ptyd { .. }, None) => {
            f.render_widget(Paragraph::new(Span::styled("connecting…", Style::default().fg(p.faint))), inner);
            None
        }
        (Backend::Legacy, None) => {
            let restored = view.filter(|v| v.checkpoint.is_some());
            if let Some(v) = restored {
                v.render(inner, f.buffer_mut(), false);
            }
            let (msg, hint) = if let Some(e) = &st.viewers_unavailable {
                (format!("ptyd host unavailable ({e})"), "↵ attaches full-screen; Ctrl+b d returns here".to_string())
            } else if let Some(e) = st.viewer_errors.get(id) {
                (format!("tmux view: {e}"), "↵ reopens it · Ctrl+] then a attaches full-screen".to_string())
            } else {
                ("starting the tmux view…".to_string(), "Ctrl+] then a attaches full-screen instead".to_string())
            };
            let hint = vec![Span::styled(hint, Style::default().fg(p.faint))];
            if restored.is_some() {
                let h = 3.min(inner.height);
                let card = Rect { y: inner.bottom().saturating_sub(h), height: h, ..inner };
                f.render_widget(Clear, card);
                fill(f, card, p.paper);
                let lines = vec![Line::from(""), Line::from(Span::styled(msg, Style::default().fg(p.ink))), Line::from(hint)];
                f.render_widget(Paragraph::new(lines).alignment(Alignment::Center), card);
            } else {
                empty_state(f, inner, p, &msg, hint, None);
            }
            None
        }
    };
    if let (Some(sel), Some(t)) = (&st.selection, &target) {
        if sel.pane == *t {
            super::pane::highlight(inner, f.buffer_mut(), sel.anchor, sel.cursor);
        }
    }
    cursor
}

/// An ACP-native session's live structured transcript in place of a pty
/// screen — there is no raw terminal to attach to (see the cross-harness-
/// messaging design's "Rendering implications" section). Interjecting still
/// goes through ninox's own prompt controls, unchanged.
fn draw_transcript(f: &mut Frame, st: &TuiState, area: Rect, entries: &[ninox_core::runtime::TranscriptEntry]) {
    use ninox_core::runtime::{ToolCallStatus, TranscriptEntry};
    let p = &st.palette;
    if area.height == 0 {
        return;
    }
    let banner = Rect { height: 1, ..area };
    f.render_widget(
        Paragraph::new(Span::styled(ninox_ui_shared::transcript::BANNER, Style::default().fg(p.faint))),
        banner,
    );
    let body = Rect { y: area.y + 1, height: area.height.saturating_sub(1), ..area };
    let lines: Vec<Line> = entries
        .iter()
        .map(|entry| match entry {
            TranscriptEntry::UserMessage(text) => Line::from(vec![
                Span::styled("▶ user  ", Style::default().fg(p.accent).add_modifier(Modifier::BOLD)),
                Span::styled(text.clone(), Style::default().fg(p.ink)),
            ]),
            TranscriptEntry::AgentText(text) => Line::from(vec![
                Span::styled("· agent ", Style::default().fg(p.ink_2).add_modifier(Modifier::BOLD)),
                Span::styled(text.clone(), Style::default().fg(p.ink)),
            ]),
            TranscriptEntry::ToolCall { name, status, detail, .. } => {
                let color = match status {
                    ToolCallStatus::Pending => p.faint,
                    ToolCallStatus::InProgress => p.working,
                    ToolCallStatus::Completed => p.done,
                    ToolCallStatus::Failed => p.ci_failed,
                };
                let icon = ninox_ui_shared::transcript::status_icon(*status);
                let status_word = ninox_ui_shared::transcript::status_word(*status);
                Line::from(vec![
                    Span::styled(format!("{icon} {name} "), Style::default().fg(color).add_modifier(Modifier::BOLD)),
                    Span::styled(format!("{status_word}: "), Style::default().fg(color)),
                    Span::styled(detail.clone(), Style::default().fg(p.ink_2)),
                ])
            }
            TranscriptEntry::TurnStarted => Line::from(Span::styled("── turn started ──", Style::default().fg(p.rule))),
            TranscriptEntry::TurnEnded => Line::from(Span::styled("── turn ended ──", Style::default().fg(p.rule))),
        })
        .collect();
    // Always show the newest activity, same as a tailing log: once the
    // transcript outgrows the pane, scroll to its tail rather than
    // freezing on the oldest turns while the agent keeps working. Wrapped
    // lines can make this an underestimate (one logical entry spans more
    // rendered rows than counted here), but it always lands at or past the
    // true tail, never short of it.
    let scroll = (lines.len() as u16).saturating_sub(body.height);
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }).scroll((scroll, 0)), body);
}

/// An ended session's report: a pinned head (who, how it ended, buttons)
/// over a body that scrolls.
fn draw_report(f: &mut Frame, st: &TuiState, lay: &Layout, row: &Row, inner: Rect) {
    let plan = super::report::plan(st, row, inner.width);
    let head_h = super::report::HEAD_H.min(inner.height);
    f.render_widget(Paragraph::new(plan.head), Rect { height: head_h, ..inner });
    let body = lay.report_body.unwrap_or(Rect { y: inner.y + head_h, height: inner.height - head_h, ..inner });
    let scroll = st.report_scroll.min(lay.report_max_scroll);
    f.render_widget(Paragraph::new(plan.body).scroll((scroll, 0)), body);
    if lay.report_max_scroll > 0 && body.height > 0 && body.width > 12 {
        let more = if scroll < lay.report_max_scroll { " ↓ more " } else { " ↑ top " };
        let w = more.width() as u16;
        let tag = Rect { x: body.right() - w, y: body.bottom() - 1, width: w, height: 1 };
        f.render_widget(Clear, tag);
        f.render_widget(Paragraph::new(Span::styled(more, Style::default().fg(st.palette.faint).bg(st.palette.card))), tag);
    }
}

/// The inspector (`i`): a tab strip over the Info or Diff content, with a
/// teal rule while it has focus (prefix-Tab from the pane).
fn draw_inspector(f: &mut Frame, area: Rect, st: &TuiState, lay: &Layout) {
    use super::inspector::{self, Tab};
    let p = &st.palette;
    let focused = st.focus == Focus::Inspector;
    let rule = Block::default().borders(Borders::LEFT).border_style(Style::default().fg(if focused { p.working } else { p.rule }));
    f.render_widget(rule, area);
    let head = Rect { x: area.x + 2, width: area.width.saturating_sub(3), height: inspector::HEAD_H.min(area.height), ..area };
    let tab = |label: &'static str, t: Tab| {
        let style = if st.inspector_tab == t {
            Style::default().fg(p.accent).add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            Style::default().fg(p.faint)
        };
        Span::styled(label, style)
    };
    let tabs = vec![tab("INFO", Tab::Info), Span::raw("  "), tab("DIFF", Tab::Diff)];
    let hint = if !focused {
        "prefix tab focus"
    } else if st.inspector_tab == Tab::Info {
        "←→ tab · j/k scroll"
    } else if st.diff.open.is_some() {
        "j/k scroll · esc files"
    } else {
        "←→ tab · ↵ open · r refresh"
    };
    f.render_widget(Paragraph::new(spread(tabs, vec![Span::styled(hint, Style::default().fg(p.faint))], head.width as usize)), head);

    let body = inspector::body(area);
    if body.height == 0 || st.selected_row().is_none() {
        return;
    }
    let lines = inspector::lines(st, body.width as usize);
    let list = st.inspector_tab == Tab::Diff && st.diff.open.is_none();
    let offset = if list {
        // The cursor's row is `selected + 1`, under the totals line.
        (st.diff.selected + 2).saturating_sub(body.height as usize) as u16
    } else if st.inspector_tab == Tab::Info {
        st.inspector_scroll.min(lay.inspector_max_scroll)
    } else {
        st.diff.scroll.min(lay.inspector_max_scroll)
    };
    f.render_widget(Paragraph::new(lines).scroll((offset, 0)), body);
}

/// The pending-requests screen: every open `ninox user-request`
/// fleet-wide, a `sync`/`async` stamp per row, the inline draft input once
/// an `async` row is opened (`st.user_requests_draft`), and — inside any
/// tmux server — the persistent keystroke-injection caveat.
fn draw_user_requests(f: &mut Frame, lay: &Layout, st: &TuiState) {
    let p = &st.palette;
    let body = lay.body;
    if body.width == 0 || body.height == 0 {
        return;
    }
    let faint = Style::default().fg(p.faint);
    let caveat_h: u16 = if st.in_tmux { 1 } else { 0 };
    let draft_h: u16 = if st.user_requests_draft.is_some() { 1 } else { 0 };

    let n = st.pending_user_requests.len();
    let summary = Rect { height: 1, ..body };
    let mut summary_spans =
        vec![Span::styled(format!("{n} pending request{}", if n == 1 { "" } else { "s" }), Style::default().fg(p.ink_2))];
    if let Some(e) = &st.human_console_error {
        summary_spans.push(Span::styled(format!("   can't answer from this TUI: {e}"), Style::default().fg(p.review)));
    }
    f.render_widget(Paragraph::new(Line::from(summary_spans)), summary);
    let rule = Rect { y: body.y + 1, height: 1, ..body };
    f.render_widget(Paragraph::new(Span::styled("─".repeat(rule.width as usize), Style::default().fg(p.rule))), rule);

    let reserved = 2 + caveat_h + draft_h;
    let list = Rect { y: body.y + 2, height: body.height.saturating_sub(reserved), ..body };
    if st.pending_user_requests.is_empty() {
        empty_state(f, list, p, "No pending requests", vec![Span::styled("a worker's ninox user-request shows up here", faint)], None);
    } else {
        let sel = st.user_requests_selected.min(st.pending_user_requests.len() - 1);
        for (i, r) in st.pending_user_requests.iter().enumerate().take(list.height as usize) {
            let row = Rect { y: list.y + i as u16, height: 1, ..list };
            let selected = i == sel;
            if selected {
                fill(f, row, p.card);
            }
            let (mode_label, mode_color) = match r.mode {
                UserRequestMode::Sync => ("SYNC ", p.review),
                UserRequestMode::Async => ("ASYNC", p.faint),
            };
            let mut spans = vec![
                Span::raw(if selected { "▌ " } else { "  " }),
                Span::styled(mode_label, Style::default().fg(mode_color).add_modifier(Modifier::BOLD)),
                Span::raw("  "),
                Span::styled(format!("from {}  ", r.requester_session_id), Style::default().fg(p.ink_2)),
            ];
            // Badged, never hidden: a row whose requester is independently
            // confirmed terminal stays listed until the human explicitly
            // dismisses it with `x`.
            if super::user_requests::requester_is_stale(st, &r.requester_session_id) {
                spans.push(Span::styled("STALE", Style::default().fg(p.review).add_modifier(Modifier::BOLD)));
                spans.push(Span::raw("  "));
            }
            let used = spans_width(&spans);
            spans.push(Span::styled(truncate(&r.prompt, (row.width as usize).saturating_sub(used)), Style::default().fg(p.ink)));
            f.render_widget(Paragraph::new(Line::from(spans)), row);
        }
    }

    if let Some(draft) = &st.user_requests_draft {
        let row = Rect { y: body.y + body.height.saturating_sub(caveat_h + 1), height: 1, ..body };
        let line = Line::from(vec![
            Span::styled("answer: ", Style::default().fg(p.ink_2)),
            Span::styled(draft.buf.clone(), Style::default().fg(p.ink)),
            Span::styled("▏", Style::default().fg(p.accent)),
        ]);
        f.render_widget(Paragraph::new(line), row);
    }

    if st.in_tmux {
        let row = Rect { y: body.y + body.height.saturating_sub(1), height: 1, ..body };
        let text = "answers here are exposed to keystroke-injection by other agent processes on this machine — \
                     for anything that gates an agent's progress, use ninox-gui instead.";
        f.render_widget(Paragraph::new(Span::styled(truncate(text, row.width as usize), Style::default().fg(p.review))), row);
    }
}

fn fleet_footer_hints(st: &TuiState) -> Vec<(String, &'static str)> {
    vec![
        ("j/k".into(), "move"),
        ("tab".into(), "focus"),
        ("[".into(), if st.tree_collapsed { "show tree" } else { "hide tree" }),
        ("↵".into(), if st.fleet.nav.open.is_some() { "reopen" } else { "open island" }),
        ("esc".into(), "back"),
        ("s".into(), "sort"),
        ("m".into(), if st.fleet.compact { "full" } else { "compact" }),
        ("f".into(), "fold island"),
        ("/".into(), "prompt"),
        ("c".into(), "connect"),
        ("n".into(), "new"),
        ("x".into(), "kill/remove"),
        ("D".into(), "delete"),
        ("g".into(), "go to"),
    ]
}

fn draw_fleet(f: &mut Frame, st: &TuiState, lay: &Layout) {
    let Some(fl) = &lay.fleet else { return };
    draw_fleet_tree(f, st, fl.tree);
    draw_fleet_toolbar(f, st, fl.toolbar);
    if st.rows.is_empty() && !st.ghost_plans.is_empty() {
        draw_ghost_boxes(f, st, fl.main);
    } else if st.rows.is_empty() {
        // The shell (tree/toolbar/prompt) still renders with zero rows — a
        // brand-new install must be able to see and use the prompt, not
        // just `n`, to get its first orchestrator going.
        let p = &st.palette;
        // Bare `n` reaches `command()` here too — `fleet_key` doesn't
        // claim it, so it falls through the same as on the old Board.
        let mut hint = key_hint(p, "n", "to spawn, or");
        hint.push(Span::raw(" "));
        hint.extend(key_hint(p, "/", "to ask ninox"));
        empty_state(f, fl.main, p, "No sessions yet.", hint, None);
    } else if fl.main.width > 0 && fl.main.height > 0 {
        match &st.fleet.nav.open {
            Some(key) => draw_fleet_opened(f, st, fl.main, key),
            None => {
                draw_fleet_grid(f, st, &fl.grid);
                let used = fl.grid.iter().map(|(_, r)| r.bottom()).max().unwrap_or(fl.main.y);
                draw_ghost_boxes(f, st, Rect { y: used, height: fl.main.bottom().saturating_sub(used), ..fl.main });
            }
        }
    }
    if let Some(strip) = fl.reply_strip {
        draw_reply_strip(f, st, strip);
    }
    draw_fleet_prompt(f, st, fl.prompt);
    if let Some(detail) = fl.detail {
        draw_fleet_detail(f, st, detail);
    }
    if let Some(panel) = fl.reply_panel {
        draw_reply_panel(f, st, panel);
    }
}

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const SPINNER_STEP_MS: i64 = 80;

/// The reply panel's "is thinking" spinner glyph at `now_ms`; redraws are
/// paced while it's on screen (`reply_is_thinking` in `mod.rs`).
pub(super) fn spinner_frame(now_ms: i64) -> char {
    SPINNER[(now_ms.div_euclid(SPINNER_STEP_MS)).rem_euclid(SPINNER.len() as i64) as usize]
}

fn reply_status_color(status: DrawerStatus, p: &Palette) -> Color {
    match status {
        DrawerStatus::Answering => p.accent,
        DrawerStatus::Relaying => p.review,
        DrawerStatus::Idle => p.faint,
    }
}

/// The collapsed reply panel: one line above the prompt box with the last
/// answer's first line — never silent, per T3 ("I asked it to spawn some
/// orchestrators and it did not reply").
fn draw_reply_strip(f: &mut Frame, st: &TuiState, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let p = &st.palette;
    let reply = &st.fleet.reply;
    let status = reply.drawer.status(st.now_ms);
    let summary_style = match &reply.drawer.notice {
        Some(Notice::Error(_)) if reply.drawer.turns.is_empty() => Style::default().fg(p.ci_failed),
        _ => Style::default().fg(p.ink_2),
    };
    let mut hint = Vec::new();
    if reply.orchestrator_id().is_some() {
        hint.extend(key_hint(p, "o", "open pane"));
        hint.push(Span::raw("  "));
    }
    hint.extend(key_hint(p, "/", "reopen"));
    let budget = (area.width as usize).saturating_sub(spans_width(&hint) + 2);
    let mut left = vec![
        Span::styled("❯ ", Style::default().fg(p.accent).add_modifier(Modifier::BOLD)),
        Span::styled("●", Style::default().fg(reply_status_color(status, p))),
        Span::raw(" "),
    ];
    let used = spans_width(&left);
    left.push(Span::styled(truncate(&reply.drawer.summary_line(), budget.saturating_sub(used)), summary_style));
    f.render_widget(Paragraph::new(spread(left, hint, area.width as usize)), area);
}

/// Who a relay went to, as the board words it: "Bellhop (its orchestrator)".
fn relay_target(st: &TuiState, id: &str) -> (String, &'static str) {
    match st.rows.iter().find(|r| r.id() == id) {
        Some(r) => (r.session.name.clone(), if r.is_orchestrator { "its orchestrator" } else { "worker" }),
        None => (id.to_string(), "worker"),
    }
}

/// The reply panel's conversation, wrapped to `width`: each turn's question
/// right-aligned in a bubble, then the rendered answer and any relays.
/// Also returns the row the latest turn starts on.
pub(super) fn reply_conversation(st: &TuiState, width: u16) -> (Vec<Line<'static>>, usize) {
    let p = &st.palette;
    let drawer = &st.fleet.reply.drawer;
    let w = width as usize;
    let mut out: Vec<Line<'static>> = Vec::new();
    if drawer.turns.is_empty() {
        let (text, style) = match &drawer.notice {
            Some(Notice::Error(e)) => (e.clone(), Style::default().fg(p.ci_failed)),
            _ => ("Context cleared — the next question starts a fresh conversation.".to_string(), Style::default().fg(p.ink_2)),
        };
        return (markdown::wrap_plain(&text, style, width), 0);
    }
    let asked_at: Vec<i64> = drawer.turns.iter().map(|t| t.asked_at_ms).collect();
    let turn_plans = ninox_ui_shared::ghost_view::plans_per_turn(&asked_at, &st.ghost_plans);
    let mut latest_start = 0;
    for (i, turn) in drawer.turns.iter().enumerate() {
        if i > 0 {
            out.push(Line::default());
        }
        latest_start = out.len();
        let bubble_style = Style::default().fg(p.ink).bg(p.highlight);
        let inner_w = (w * 7 / 10).saturating_sub(2).max(1);
        let rows = markdown::wrap_plain(&turn.question, bubble_style, inner_w as u16);
        let bubble_w = rows.iter().map(Line::width).max().unwrap_or(0) + 2;
        for row in rows {
            let pad = bubble_w - 2 - row.width();
            let mut spans = vec![Span::raw(" ".repeat(w.saturating_sub(bubble_w))), Span::styled(" ", bubble_style)];
            spans.extend(row.spans);
            spans.push(Span::styled(" ".repeat(pad + 1), bubble_style));
            out.push(Line::from(spans));
        }
        out.push(Line::default());
        if !turn.answer.trim().is_empty() {
            out.extend(markdown::render(&turn.answer, width, p));
        } else if turn.is_active() {
            let line = format!("{} {} is thinking…", spinner_frame(st.now_ms), turn.target_label);
            out.extend(markdown::wrap_plain(&line, Style::default().fg(p.faint), width));
        }
        if let Some(error) = &turn.error {
            out.extend(markdown::wrap_plain(&format!("{}: {error}", turn.target_label), Style::default().fg(p.ci_failed), width));
        }
        if let Some(ids) = turn_plans.get(i) {
            for plan in ids.iter().filter_map(|id| st.ghost_plans.iter().find(|p| &p.plan.id == id)) {
                out.extend(ghost_card_lines(st, plan, width));
            }
            if turn.claims_unstored_plan(ids, &st.ghost_plans, st.now_ms) {
                out.extend(markdown::wrap_plain(
                    &format!("⚠ {}", ninox_ui_shared::ghost_view::NO_PLAN_STORED),
                    Style::default().fg(p.ci_failed),
                    width,
                ));
            }
        }
        for relay in &turn.relays {
            let (name, role) = relay_target(st, &relay.to_id);
            let state = turn.relay_state(relay, st.now_ms);
            let color = if state == RelayState::Waiting { p.review } else { p.faint };
            let text = format!("↳ Asked {name} ({role}) — {}", relay_tail(state, relay.relayed_at_ms));
            out.extend(markdown::wrap_plain(&text, Style::default().fg(color), width));
        }
    }
    if drawer.notice == Some(Notice::ClearPending) {
        out.push(Line::default());
        out.extend(markdown::wrap_plain(ninox_ui_shared::prompt_agent::reply::CLEAR_PENDING_TEXT, Style::default().fg(p.ink_2), width));
    }
    (out, latest_start)
}

/// The open reply panel (`PromptReply.dc.html`), over the bottom of the
/// Fleet view down to the prompt box.
fn draw_reply_panel(f: &mut Frame, st: &TuiState, area: Rect) {
    if area.width < 4 || area.height < 3 {
        return;
    }
    let p = &st.palette;
    let reply = &st.fleet.reply;
    f.render_widget(Clear, area);
    let border = if reply.focused || st.fleet.prompt_editing { p.accent } else { p.rule };
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border))
        .style(Style::default().bg(p.paper));
    let inner = block.inner(area);
    let content = Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner };
    let body = Rect { y: content.y + 2, height: content.height.saturating_sub(2), ..content };
    let (lines, latest_start) = reply_conversation(st, body.width);
    let rows = |n: usize| u16::try_from(n).unwrap_or(u16::MAX);
    let max_scroll = rows(lines.len()).saturating_sub(body.height);
    let top = reply.top_row(rows(latest_start), max_scroll);
    reply.max_scroll.set(max_scroll);
    reply.drawn_top.set(top);
    if top < max_scroll {
        // j/k belong to the grid unless the panel has the keyboard.
        let how = if reply.focused { "j/k scroll" } else { "PgDn scroll" };
        block = block.title_bottom(Line::from(Span::styled(format!(" ↓ more · {how} "), Style::default().fg(p.faint))).right_aligned());
    }
    f.render_widget(block, area);

    let status = reply.drawer.status(st.now_ms);
    let left = vec![
        Span::styled("❯ ", Style::default().fg(p.accent).add_modifier(Modifier::BOLD)),
        Span::styled("Ninox prompt", Style::default().fg(p.ink).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled("● ", Style::default().fg(reply_status_color(status, p))),
        Span::styled(status_line(status, reply.drawer.context_pct), Style::default().fg(p.ink_2)),
    ];
    let mut hint = Vec::new();
    if reply.focused {
        hint.extend(key_hint(p, "j/k", "scroll"));
        hint.push(Span::raw("  "));
    }
    hint.extend(key_hint(p, "ctrl+l", "clear"));
    hint.push(Span::raw("  "));
    hint.extend(key_hint(p, "esc", "collapse"));
    f.render_widget(Paragraph::new(spread(left, hint, content.width as usize)), Rect { height: 1, ..content });

    if body.height > 0 {
        f.render_widget(Paragraph::new(lines).scroll((top, 0)), body);
    }
}

/// The toolbar line above the islands grid / opened tree: a focus-indicator
/// dot (teal while the grid/opened island holds `main_focus`, dim
/// otherwise — the centre pane has no border of its own to recolor, unlike
/// the tree/detail panes), then `sort: … · view: … · finished: …` plus key
/// hints (`TUIAtScale.dc.html`).
fn draw_fleet_toolbar(f: &mut Frame, st: &TuiState, area: Rect) {
    if area.height == 0 {
        return;
    }
    f.render_widget(Paragraph::new(fleet_toolbar_line(st, area.width)), area);
}

/// The toolbar fitted to `width`: the state fields drop from the right,
/// then key hints are taken in priority order (`/ prompt` first) while
/// they fit, so nothing is ever cut mid-word.
fn fleet_toolbar_line(st: &TuiState, width: u16) -> Line<'static> {
    let p = &st.palette;
    let w = width as usize;
    let dot_color = if st.fleet.main_focus == fleet::FleetFocus::Main { p.working } else { p.faint };
    let field = |label: &'static str, value: &'static str| {
        vec![Span::styled(label, Style::default().fg(p.faint)), Span::styled(value, Style::default().fg(p.working))]
    };
    let machine = match &st.fleet.machine_filter {
        ninox_overview::islands::MachineFilter::All => "all".to_string(),
        ninox_overview::islands::MachineFilter::Local => "local".to_string(),
        ninox_overview::islands::MachineFilter::Machine(id) => id.clone(),
    };
    let remote = !st.fleet.machines.is_empty();
    let mut fields = vec![
        field("sort: ", fleet::sort_label(st.fleet.sort)),
        field(" · view: ", if st.fleet.compact { "compact" } else { "full" }),
        // The island `f` would flip, else the default every island starts from.
        field(" · finished: ", if st.focused_island().map_or(st.fleet.fold_expanded, |k| st.fleet.island_fold_expanded(&k)) { "expanded" } else { "folded" }),
    ];
    if remote {
        fields.insert(1, vec![Span::styled(" · machine: ", Style::default().fg(p.faint)), Span::styled(machine, Style::default().fg(p.working))]);
    }
    let mut spans = vec![Span::styled("● ", Style::default().fg(dot_color))];
    for fd in fields {
        if spans_width(&spans) + spans_width(&fd) > w {
            break;
        }
        spans.extend(fd);
    }
    let mut hints = vec![("s", "sort"), ("m", "compact"), ("f", "fold island"), ("enter", "expand"), ("/", "prompt")];
    let mut priority = vec!["/", "enter", "s", "f", "m"];
    if remote {
        hints.insert(2, ("M", "machine"));
        priority.insert(3, "M");
    }
    let hint_w = |(k, what): (&str, &str)| 2 + k.width() + 1 + what.width();
    let mut room = w.saturating_sub(spans_width(&spans) + 1);
    let mut keep = Vec::new();
    for k in priority {
        let h = hints.iter().copied().find(|(key, _)| *key == k).expect("every priority is a hint");
        if hint_w(h) > room {
            continue;
        }
        room -= hint_w(h);
        keep.push(k);
    }
    for (k, what) in hints.into_iter().filter(|(k, _)| keep.contains(k)) {
        spans.push(Span::raw("  "));
        spans.extend(key_hint(p, k, what));
    }
    Line::from(spans)
}

/// The Fleet tab's own tree: a pinned Needs-you block, sessions grouped by
/// machine, finished workers folded — hidden entirely rather than shown
/// (`[`, `layout::fleet`'s `area.width == 0`) when collapsed. Its border
/// turns teal while it holds `main_focus`, dim otherwise — the Fleet tab's
/// visible focus indicator (final review: "the other panes dimmed").
fn draw_fleet_tree(f: &mut Frame, st: &TuiState, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let p = &st.palette;
    let needs = st.needs_rows();
    // A group is its orchestrator; a sub-orchestrator's group folds with
    // the island it sits in (keyed by the top orchestrator), like the grid.
    let fold = |group: Option<&str>| match group {
        Some(g) => {
            let key = ninox_overview::nav::owning_island(&st.fleet.islands, g)
                .unwrap_or_else(|| ninox_overview::islands::IslandKey::Root(g.to_string()));
            st.fleet.island_fold_expanded(&key)
        }
        None => st.fleet.fold_expanded,
    };
    let rows = fleet::tree_rows(&st.rows, &needs, &fold);
    let selected = st.selected_id();
    st.chip_scratch.borrow_mut().clear();
    let lines: Vec<Line> = rows.iter().map(|row| tree_line(st, row, selected.as_deref())).collect();
    super::pr_chips::register(st, &lines, Rect { width: area.width.saturating_sub(1), ..area }, false);
    let border_color = if st.fleet.main_focus == fleet::FleetFocus::Tree { p.working } else { p.rule };
    let block = Block::default().borders(Borders::RIGHT).border_style(Style::default().fg(border_color));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn tree_line(st: &TuiState, row: &fleet::TreeRow, selected: Option<&str>) -> Line<'static> {
    let p = &st.palette;
    match row {
        fleet::TreeRow::NeedsYouHeading => {
            Line::from(Span::styled("NEEDS YOU", Style::default().fg(p.accent).add_modifier(Modifier::BOLD)))
        }
        fleet::TreeRow::MachineHeading(None) => Line::from(Span::styled("this machine", Style::default().fg(p.faint))),
        fleet::TreeRow::MachineHeading(Some(id)) => {
            let remote = st.remote_rows.iter().find(|r| &r.id == id);
            let label = remote.map(|r| r.label.clone()).unwrap_or_else(|| id.clone());
            let (word, color) = match remote.and_then(|r| r.health) {
                Some(ninox_core::types::MachineHealth::Connected) | None => ("connected", p.working),
                Some(ninox_core::types::MachineHealth::Reconnecting) => ("reconnecting", p.review),
                Some(ninox_core::types::MachineHealth::Unreachable) => ("unreachable", p.ci_failed),
            };
            Line::from(vec![
                Span::styled(format!("⌁ {label} "), Style::default().fg(p.ink_2)),
                Span::styled("● ", Style::default().fg(color)),
                Span::styled(word, Style::default().fg(p.faint)),
            ])
        }
        fleet::TreeRow::Needs(i) | fleet::TreeRow::Orchestrator(i) | fleet::TreeRow::Worker(i) | fleet::TreeRow::Standalone(i) => {
            let Some(r) = st.rows.get(*i) else { return Line::default() };
            let sel_mod = if selected == Some(r.id()) { Modifier::REVERSED } else { Modifier::empty() };
            let indent = if matches!(row, fleet::TreeRow::Worker(_)) { "  " } else { "" };
            let is_orch = matches!(row, fleet::TreeRow::Orchestrator(_));
            let is_open = is_orch && st.fleet.nav.open.as_ref() == Some(&IslandKey::Root(r.id().to_string()));
            let caret = if is_orch { if is_open { "▾ " } else { "▸ " } } else { "" };
            let status = classify_member(&r.session, false);
            let dot_color = fleet::member_status_color(status, p);
            let tag = fleet::member_tag(status, r, p).filter(|_| !super::pr_chips::replaces_pr_tag(r));
            let mut spans = vec![
                Span::styled(indent, Style::default().add_modifier(sel_mod)),
                Span::styled(caret, Style::default().fg(p.faint).add_modifier(sel_mod)),
                Span::styled("● ", Style::default().fg(dot_color).add_modifier(sel_mod)),
                Span::styled(r.session.name.clone(), Style::default().fg(p.ink).add_modifier(sel_mod)),
            ];
            if let Some((text, color)) = tag {
                spans.push(Span::styled(text, Style::default().fg(color).add_modifier(sel_mod)));
            }
            let used: usize = spans.iter().map(|s| s.content.width()).sum();
            let tree_w = st.layout.fleet.as_ref().map_or(30, |f| f.tree.width as usize).saturating_sub(1);
            spans.extend(super::pr_chips::spans(st, r, 1, sel_mod, tree_w.saturating_sub(used)));
            if is_orch {
                let count = st.rows.iter().filter(|w| w.group.as_deref() == Some(r.id()) && !w.is_orchestrator).count();
                if count > 0 {
                    spans.push(Span::styled(format!(" {count}"), Style::default().fg(p.faint).add_modifier(sel_mod)));
                }
            }
            Line::from(spans)
        }
        fleet::TreeRow::Folded { done, stopped } => {
            let mut parts = Vec::new();
            if *done > 0 {
                parts.push(format!("{done} done"));
            }
            if *stopped > 0 {
                parts.push(format!("{stopped} stopped"));
            }
            Line::from(Span::styled(format!("  ▸ {}", parts.join(" · ")), Style::default().fg(p.faint)))
        }
    }
}

/// One island's body lines (everything inside its border): the title line,
/// then worker rows, then the finished-fold row or an empty-state line.
/// Called both to measure the box's height and to render it — islands are
/// small and this redraws at most once a second, so recomputing rather than
/// caching is simplest.
fn island_rows(st: &TuiState, island: &Island) -> Vec<Line<'static>> {
    let p = &st.palette;
    let mut lines = Vec::new();
    let mut title = vec![Span::styled("● ", Style::default().fg(p.working))];
    title.push(Span::styled(truncate(&island.name, 26), Style::default().fg(p.ink).add_modifier(Modifier::BOLD)));
    if island.needs_attention() {
        title.push(Span::styled(format!("  !{}", island.summary.needs_you), Style::default().fg(p.accent)));
    }
    let total = island.summary.total();
    if total > 0 {
        title.push(Span::styled(format!("  {total}"), Style::default().fg(p.faint)));
    }
    lines.push(Line::from(title));

    let others: Vec<&ninox_overview::islands::IslandMember> =
        island.members.iter().filter(|m| Some(&m.id) != island.root.as_ref()).collect();
    let (folded, visible): (Vec<_>, Vec<_>) =
        others.into_iter().partition(|m| matches!(m.status, MemberStatus::Done | MemberStatus::Stopped));
    let expanded = st.fleet.island_fold_expanded(&island.key);
    for m in &visible {
        lines.push(member_row_line(st, m, false));
    }
    if !folded.is_empty() {
        if expanded {
            for m in &folded {
                lines.push(member_row_line(st, m, false));
            }
        } else {
            let done = folded.iter().filter(|m| m.status == MemberStatus::Done).count();
            let stopped = folded.len() - done;
            let mut parts = Vec::new();
            if done > 0 {
                parts.push(format!("{done} done"));
            }
            if stopped > 0 {
                parts.push(format!("{stopped} stopped"));
            }
            lines.push(Line::from(Span::styled(format!("└ ▸ {}", parts.join(" · ")), Style::default().fg(p.faint))));
        }
    } else if visible.is_empty() {
        lines.push(Line::from(Span::styled("└ no workers", Style::default().fg(p.faint))));
    }
    lines
}

fn member_row_line(st: &TuiState, m: &ninox_overview::islands::IslandMember, is_last: bool) -> Line<'static> {
    let p = &st.palette;
    let rail = if is_last { "└ " } else { "├ " };
    let row = st.rows.iter().find(|r| r.id() == m.id);
    let name = row.map(|r| r.session.name.as_str()).unwrap_or(m.id.as_str());
    let tag = row.and_then(|r| fleet::member_tag(m.status, r, p).filter(|_| !super::pr_chips::replaces_pr_tag(r)));
    const MEMBER_CHIP_BUDGET: usize = 22;
    let chips = row.map(|r| super::pr_chips::fitting(r.id(), &r.prs, 1, MEMBER_CHIP_BUDGET)).unwrap_or_default();
    let chips_w: usize = chips.iter().map(|c| c.0.chars().count() + 1).sum();
    let dot_color = fleet::member_status_color(m.status, p);
    let budget = 34usize.saturating_sub(tag.as_ref().map(|(t, _)| t.chars().count()).unwrap_or(0) + chips_w);
    let mut spans = vec![
        Span::styled(rail, Style::default().fg(p.faint)),
        Span::styled("● ", Style::default().fg(dot_color)),
        Span::styled(truncate(name, budget), Style::default().fg(p.ink)),
    ];
    if let Some((text, color)) = tag {
        spans.push(Span::styled(text, Style::default().fg(color)));
    }
    if let Some(r) = row {
        spans.extend(super::pr_chips::spans(st, r, 1, Modifier::empty(), MEMBER_CHIP_BUDGET));
    }
    Line::from(spans)
}

fn remote_band_line(st: &TuiState, machine: &ninox_overview::islands::IslandMachine) -> Line<'static> {
    let p = &st.palette;
    let label = st.remote_rows.iter().find(|r| r.id == machine.machine_id).map(|r| r.label.clone()).unwrap_or_else(|| machine.machine_id.clone());
    let (word, color) = match machine.health {
        Some(ninox_core::types::MachineHealth::Connected) | None => ("connected", p.working),
        Some(ninox_core::types::MachineHealth::Reconnecting) => ("reconnecting", p.review),
        Some(ninox_core::types::MachineHealth::Unreachable) => ("unreachable", p.ci_failed),
    };
    Line::from(vec![
        Span::styled(format!("⌁ REMOTE {label} "), Style::default().fg(p.ink_2)),
        Span::styled("● ", Style::default().fg(color)),
        Span::styled(word, Style::default().fg(p.faint)),
    ])
}

fn draw_island_box(f: &mut Frame, st: &TuiState, area: Rect, island: &Island) {
    let p = &st.palette;
    let selected = st.fleet.nav.selected_island.as_ref() == Some(&island.key);
    let is_remote = island.machine.is_some();
    let border_color = if selected { p.working } else if is_remote { p.ink_2 } else { p.rule };
    let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(border_color));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let mut lines = Vec::new();
    if let Some(machine) = &island.machine {
        lines.push(remote_band_line(st, machine));
    }
    st.chip_scratch.borrow_mut().clear();
    lines.extend(island_rows(st, island));
    super::pr_chips::register(st, &lines, inner, true);
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

/// A dashed outline for ghost (proposed, not spawned) boxes.
const GHOST_BORDER: ratatui::symbols::border::Set = ratatui::symbols::border::Set {
    top_left: "╭",
    top_right: "╮",
    bottom_left: "╰",
    bottom_right: "╯",
    vertical_left: "┆",
    vertical_right: "┆",
    horizontal_top: "┄",
    horizontal_bottom: "┄",
};

/// One pending plan as text: header, its ghost nodes, the decision hint.
fn ghost_plan_lines(st: &TuiState, plan: &ninox_core::ghost_plan::PlanWithNodes) -> Vec<Line<'static>> {
    use ninox_ui_shared::ghost_view;
    let p = &st.palette;
    let mut lines = vec![Line::from(vec![
        Span::styled("GHOST ", Style::default().fg(p.review).add_modifier(Modifier::BOLD)),
        Span::styled(ghost_view::plan_title(plan), Style::default().fg(p.ink_2)),
        Span::styled(format!("  {}", plan.plan.id), Style::default().fg(p.faint)),
    ])];
    for n in ghost_view::ghost_nodes(plan) {
        lines.push(Line::from(vec![
            Span::raw("  ".repeat(ghost_view::depth(plan, n) + 1)),
            Span::styled(format!("{} ", n.worker_type), Style::default().fg(p.faint)),
            Span::styled(n.name.clone(), Style::default().fg(p.ink_2)),
        ]));
    }
    lines.push(Line::from(Span::styled(
        format!("{} · Plans tab: a accept · x discard", ghost_view::NOTHING_SPAWNS),
        Style::default().fg(p.faint),
    )));
    lines
}

/// Pending ghost plans below the islands grid: dimmed, dashed boxes, one
/// per plan, as many as fit whole.
fn draw_ghost_boxes(f: &mut Frame, st: &TuiState, area: Rect) {
    let p = &st.palette;
    let mut y = area.y;
    for plan in st.ghost_plans.iter().filter(|p| p.plan.status == ninox_core::ghost_plan::PlanStatus::Pending) {
        let lines = ghost_plan_lines(st, plan);
        let h = lines.len() as u16 + 2;
        if y + h > area.bottom() {
            break;
        }
        let rect = Rect { y, height: h, ..area };
        let title = match plan.plan.target_parent.as_deref() {
            Some(t) => format!(" proposed · under {t} "),
            None => " proposed · new orchestrators ".to_string(),
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_set(GHOST_BORDER)
            .border_style(Style::default().fg(p.faint))
            .title(Span::styled(title, Style::default().fg(p.faint)));
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        f.render_widget(Paragraph::new(lines).style(Style::default().add_modifier(Modifier::DIM)), inner);
        y += h;
    }
}

/// The reply panel's ghost card for a plan proposed in that turn.
fn ghost_card_lines(st: &TuiState, plan: &ninox_core::ghost_plan::PlanWithNodes, width: u16) -> Vec<Line<'static>> {
    use ninox_ui_shared::ghost_view;
    let p = &st.palette;
    let w = width as usize;
    let rule = |l: &str, r: &str| Line::from(Span::styled(format!("{l}{}{r}", "┄".repeat(w.saturating_sub(2))), Style::default().fg(p.faint)));
    let mut out = vec![rule("╭", "╮")];
    let nodes = if plan.plan.status == ninox_core::ghost_plan::PlanStatus::Pending { ghost_view::ghost_nodes(plan) } else { plan.top_down() };
    for n in nodes {
        out.push(Line::from(vec![
            Span::styled(format!("┆ {} ", ghost_view::badge(n)), Style::default().fg(p.review).add_modifier(Modifier::BOLD)),
            Span::styled(n.name.clone(), Style::default().fg(p.ink)),
        ]));
        for row in markdown::wrap_plain(&ninox_overview::display::display_brief(&n.brief), Style::default().fg(p.ink_2), width.saturating_sub(4)) {
            let mut spans = vec![Span::styled("┆   ", Style::default().fg(p.faint))];
            spans.extend(row.spans);
            out.push(Line::from(spans));
        }
        if let Some(deps) = ghost_view::dependency_line(plan, n) {
            out.push(Line::from(Span::styled(format!("┆   {deps}"), Style::default().fg(p.faint))));
        }
    }
    let footer = if plan.plan.status == ninox_core::ghost_plan::PlanStatus::Pending {
        format!("┆ {} · {} — Plans tab (3): a accept · x discard", ghost_view::accept_label(plan), ghost_view::NOTHING_SPAWNS)
    } else {
        format!("┆ {}", ghost_view::plan_status_line(plan))
    };
    out.extend(markdown::wrap_plain(&footer, Style::default().fg(p.faint), width));
    out.push(rule("╰", "╯"));
    out
}

/// The Plans tab: one block per plan, the selected one highlighted, with
/// its nodes and their states.
fn draw_plans(f: &mut Frame, st: &TuiState, area: Rect) {
    use ninox_ui_shared::ghost_view;
    let p = &st.palette;
    let sel = st.plans_sel_index();
    let mut lines: Vec<Line<'static>> = vec![
        Line::from(Span::styled("Plans", Style::default().fg(p.ink).add_modifier(Modifier::BOLD))),
        Line::from(Span::styled("j/k select · a accept (spawns, after a confirm) · x discard · esc back", Style::default().fg(p.faint))),
        Line::default(),
    ];
    let mut selected_row = 0;
    for (i, plan) in st.ghost_plans.iter().enumerate() {
        let is_sel = i == sel;
        if is_sel {
            selected_row = lines.len();
        }
        let marker = if is_sel { "▸ " } else { "  " };
        let head_style = if is_sel { Style::default().fg(p.ink).bg(p.highlight).add_modifier(Modifier::BOLD) } else { Style::default().fg(p.ink) };
        lines.push(Line::from(vec![
            Span::styled(format!("{marker}{} ", plan.plan.status.as_str().to_uppercase()), head_style.fg(p.review)),
            Span::styled(ghost_view::plan_title(plan), head_style),
            Span::styled(format!("  {}", plan.plan.id), Style::default().fg(p.faint)),
        ]));
        lines.push(Line::from(Span::styled(format!("    {}", ghost_view::plan_status_line(plan)), Style::default().fg(p.ink_2))));
        for n in plan.top_down() {
            lines.push(Line::from(vec![
                Span::raw("    ".to_string() + &"  ".repeat(ghost_view::depth(plan, n))),
                Span::styled(format!("{} ", n.worker_type), Style::default().fg(p.faint)),
                Span::styled(n.name.clone(), Style::default().fg(p.ink)),
                Span::styled(format!("  {}", ghost_view::node_state_line(n)), Style::default().fg(p.faint)),
            ]));
        }
        lines.push(Line::default());
    }
    let scroll = selected_row.saturating_sub(area.height.saturating_sub(4) as usize) as u16;
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).scroll((scroll, 0)), Rect { x: area.x + 2, width: area.width.saturating_sub(4), ..area });
}

/// The Fleet tab's islands grid (`TUIAtScale.dc.html`): one box per
/// orchestrator tree (plus Unparented), at the `Rect`s `layout::fleet_grid`
/// already computed — vertical scroll only, never horizontal (a box that
/// doesn't fully fit is left out of `grid` entirely rather than drawn
/// clipped).
fn draw_fleet_grid(f: &mut Frame, st: &TuiState, grid: &[(IslandKey, Rect)]) {
    for (key, rect) in grid {
        if let Some(island) = st.fleet.islands.iter().find(|i| &i.key == key) {
            draw_island_box(f, st, *rect, island);
        }
    }
}

/// Hops from `root` down to `id` along the opened island's own parent→child
/// map — "depth n/m" in the detail panel's breadcrumb meta
/// (`TUI-Inspect.dc.html`).
fn depth_of(children: &std::collections::HashMap<String, Vec<String>>, root: &str, id: &str) -> usize {
    fn walk(children: &std::collections::HashMap<String, Vec<String>>, node: &str, target: &str, depth: usize) -> Option<usize> {
        if node == target {
            return Some(depth);
        }
        children.get(node)?.iter().find_map(|c| walk(children, c, target, depth + 1))
    }
    walk(children, root, id, 0).unwrap_or(0)
}

/// The opened island's full tree (`TUI.dc.html`'s focused view): a
/// breadcrumb, then the island's own rataflow card tree
/// (`island_flow::build_flow` — Sugiyama-laid-out, zoomed to fit, marching
/// ants on edges to a `Working` member), the TUI counterpart to the GUI's
/// card tree (#254).
fn draw_fleet_opened(f: &mut Frame, st: &TuiState, area: Rect, key: &IslandKey) {
    if area.height == 0 {
        return;
    }
    let p = &st.palette;
    let Some(island) = st.fleet.islands.iter().find(|i| &i.key == key) else {
        return;
    };
    let breadcrumb = Line::from(vec![
        Span::styled("Fleet", Style::default().fg(p.ink_2)),
        Span::styled(" › ", Style::default().fg(p.faint)),
        Span::styled(truncate(&island.name, 40), Style::default().fg(p.ink).add_modifier(Modifier::BOLD)),
        Span::styled(
            format!("  ·  {} active · {} done · {} stopped", island.summary.active(), island.summary.done, island.summary.stopped),
            Style::default().fg(p.faint),
        ),
    ]);
    f.render_widget(Paragraph::new(breadcrumb), Rect { height: 1.min(area.height), ..area });
    let body = Rect { y: area.y + 1, height: area.height.saturating_sub(1), ..area };
    if body.width == 0 || body.height == 0 {
        return;
    }

    st.fleet.ensure_opened_flow(&st.rows, p, body.width as f64);
    st.fleet.tick_opened_animation();
    let mut flow = st.fleet.opened_flow.borrow_mut();
    match flow.as_mut() {
        Some(flow) => f.render_widget(flow, body),
        None => f.render_widget(Paragraph::new(Span::styled("nothing in this island yet", Style::default().fg(p.faint))), body),
    }
}

/// The always-visible ninox prompt, docked under the main area. Submit
/// routes the text to the selected node's orchestrator (`fleet_prompt_key`/
/// `prompt_target`); the delivery confirmation or a local refusal (e.g. no
/// node selected to disambiguate) shows here afterward, in place of the
/// placeholder, until the prompt is reopened. The placeholder itself is
/// scoped to the opened island's name, once one is open.
///
/// A 3-row bordered box, not a bare line — the one deliberate exception to
/// this module's usual "whitespace and rules, not boxes" structure (see the
/// FEEDBACK-v1-tryout-round1.md T2 item: the prompt is a primary control and
/// must read as an input box at a glance, overriding the artboards' subtler
/// styling for the prompt only).
fn draw_fleet_prompt(f: &mut Frame, st: &TuiState, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let p = &st.palette;
    let editing = st.fleet.prompt_editing;
    let border_style = Style::default().fg(if editing { p.accent } else { p.faint });
    let block = Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).border_style(border_style);
    let inner = block.inner(area);
    f.render_widget(block, area);

    // The target chip (`❯ ninox ▾`, `ctrl+t` cycles) — always shown, not
    // just while editing, so the target is visible before you even start
    // typing.
    let chip = Span::styled(format!("❯ {} ▾ ", prompt_target_label(st)), Style::default().fg(p.accent).add_modifier(Modifier::BOLD));
    let (text, style) = if editing {
        (format!("{}▏", st.fleet.prompt_input), Style::default().fg(p.ink).add_modifier(Modifier::BOLD))
    } else {
        let placeholder = match st.fleet.nav.open.as_ref().and_then(|k| st.fleet.islands.iter().find(|i| &i.key == k)) {
            Some(island) => format!("ask ninox about {}", island.name),
            None => "ask ninox — \"what's blocking …?\"".to_string(),
        };
        // Mid grey, not the old DarkGray — the user's complaint was that the
        // prompt was "not very obvious".
        (placeholder, Style::default().fg(p.ink_2))
    };
    let hint = vec![Span::styled("/ focus · ⏎ send", Style::default().fg(p.faint))];
    let line = spread(vec![chip, Span::styled(text, style)], hint, inner.width as usize);
    f.render_widget(Paragraph::new(line), inner);
}

fn status_badge_parts(status: MemberStatus, p: &Palette) -> (&'static str, Color) {
    let label = match status {
        MemberStatus::NeedsYou => "NEEDS YOU",
        MemberStatus::Working => "WORKING",
        MemberStatus::WaitingOnPr => "WAITING ON PR",
        MemberStatus::Done => "DONE",
        MemberStatus::Stopped => "STOPPED",
        MemberStatus::Queued => "QUEUED",
    };
    (label, fleet::member_status_color(status, p))
}

/// The opened island's detail pane (`TUI-Inspect.dc.html`), right side —
/// only rendered while `lay.fleet.detail` is `Some` (an island is open and
/// a node in it is selected). Field order matches the slice brief: status,
/// breadcrumb meta, Waiting on, Brief, type · harness · model, artifacts,
/// then PR state/checks/comments as plain text, then key hints.
fn draw_fleet_detail(f: &mut Frame, st: &TuiState, area: Rect) {
    let p = &st.palette;
    let Some(key) = &st.fleet.nav.open else { return };
    let Some(island) = st.fleet.islands.iter().find(|i| &i.key == key) else { return };
    let Some(selected) = st.fleet.nav.selected.as_deref() else { return };
    let Some(member) = island.members.iter().find(|m| m.id == selected) else { return };
    let Some(row) = st.rows.iter().find(|r| r.id() == selected) else { return };

    let (status_label, status_color) = status_badge_parts(member.status, p);
    let mut lines = vec![
        Line::from(Span::styled(status_label, Style::default().fg(p.paper).bg(status_color).add_modifier(Modifier::BOLD))),
        Line::from(Span::styled(row.session.name.clone(), Style::default().fg(p.ink).add_modifier(Modifier::BOLD))),
    ];
    let max_depth = fleet::max_orchestrator_depth();
    let depth = match &island.root {
        Some(root) => depth_of(&fleet::island_children(&st.rows, island), root, selected),
        None => 0,
    };
    lines.push(Line::from(Span::styled(
        format!("{} › {}  ·  depth {depth}/{max_depth}", island.name, row.session.name),
        Style::default().fg(p.faint),
    )));
    lines.push(Line::from(""));

    if row.is_orchestrator {
        lines.push(Line::from(format!(
            "{} active · {} done · {} stopped",
            island.summary.active(),
            island.summary.done,
            island.summary.stopped,
        )));
        let needs: Vec<&str> = island
            .members
            .iter()
            .filter(|m| m.status == MemberStatus::NeedsYou)
            .filter_map(|m| st.rows.iter().find(|r| r.id() == m.id).map(|r| r.session.name.as_str()))
            .collect();
        if !needs.is_empty() {
            lines.push(Line::from(Span::styled(format!("Needs you: {}", needs.join(", ")), Style::default().fg(p.accent))));
        }
        if let Some(brains) = st.brain_lines.get(selected) {
            lines.push(Line::from(Span::styled(brains.clone(), Style::default().fg(p.ink_2))));
        }
    } else {
        if row.session.activity == ActivityState::Blocked {
            if let Some(waiting) = ninox_overview::waiting_on::waiting_on_text(row.session.activity_note.as_deref()) {
                lines.push(Line::from(Span::styled("Waiting on", Style::default().fg(p.faint))));
                lines.push(Line::from(Span::styled(waiting, Style::default().fg(p.review))));
                lines.push(Line::from(""));
            }
        }
        if let Some(summary) = row.session.summary.as_deref() {
            let brief = ninox_overview::display::display_brief(summary);
            if !brief.is_empty() {
                lines.push(Line::from(Span::styled("Brief", Style::default().fg(p.faint))));
                lines.push(Line::from(brief));
                lines.push(Line::from(""));
            }
        }
        lines.push(Line::from(format!(
            "{} · {} · {}",
            row.session.worker_type.as_deref().unwrap_or("Worker"),
            row.session.agent_type,
            row.session.model.as_deref().unwrap_or("—"),
        )));
        let branch = row.session.workspace_path.as_deref().and_then(|w| w.rsplit('/').next()).filter(|b| !b.is_empty());
        let mut artifacts = Vec::new();
        if let Some(b) = branch {
            artifacts.push(format!("branch {b}"));
        }
        match (ninox_core::session_prs::summary(&row.prs), row.session.pr_number) {
            (Some(summary), _) => artifacts.push(summary),
            (None, Some(n)) => artifacts.push(format!("PR #{n}")),
            (None, None) => artifacts.push("no PR yet".to_string()),
        }
        lines.push(Line::from(artifacts.join(" · ")));

        if !row.session.success_criteria.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("Definition of done", Style::default().fg(p.faint))));
            for (done, label) in ninox_overview::completion::definition_of_done(&row.session) {
                let mark = if done { "✓" } else { "○" };
                lines.push(Line::from(format!("{mark} {label}")));
            }
        }

        if let Some(outcome) = &row.session.completion_outcome {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("Outcome", Style::default().fg(p.faint))));
            lines.push(Line::from(outcome.summary.clone()));
            for evidence in &outcome.evidence {
                lines.push(Line::from(Span::styled(evidence.clone(), Style::default().fg(p.ink_2))));
            }
        }

        // Guards against a stale cache from the previous tick's selection —
        // `sync_detail_pr` runs once per refresh, a selection change can
        // land between ticks (final review discipline matching
        // `fleet_detail::context`'s own staleness guard on the GUI side).
        let detail = st.fleet.detail_pr.as_ref().filter(|d| row.session.pr_id == Some(d.pr_id));
        if let Some(detail) = detail {
            lines.push(Line::from(""));
            if !row.prs.is_empty() {
                lines.push(Line::from(Span::styled(super::inspector::pr_heading(row.prs.len()), Style::default().fg(p.faint))));
                for (title, detail) in super::inspector::pr_entries(&row.prs) {
                    lines.push(Line::from(Span::styled(title, Style::default().fg(p.ink_2))));
                    lines.push(Line::from(Span::styled(format!("  {detail}"), Style::default().fg(p.faint))));
                }
            } else if let Some(pr) = &detail.pr {
                lines.push(Line::from(Span::styled(format!("#{} {}", pr.number, pr.title), Style::default().fg(p.ink_2))));
            } else if let Some(n) = row.session.pr_number {
                lines.push(Line::from(Span::styled(format!("PR #{n}"), Style::default().fg(p.ink_2))));
            }
            if row.prs.is_empty() {
                if let Some(ci) = &detail.ci {
                    let color = if ci.failing > 0 { p.ci_failed } else if ci.pending > 0 { p.review } else { p.working };
                    lines.push(Line::from(Span::styled(format!("{}/{} checks passing", ci.passing, ci.total), Style::default().fg(color))));
                }
            }
            if !detail.comments.is_empty() {
                lines.push(Line::from(Span::styled(format!("Comments · {}", detail.comments.len()), Style::default().fg(p.faint))));
                for c in &detail.comments {
                    let body = ninox_overview::comment_markdown::plain_text(&c.body);
                    lines.push(Line::from(Span::styled(format!("{}: {}", c.author, truncate(&body, 60)), Style::default().fg(p.ink_2))));
                }
            }
        }
    }

    lines.push(Line::from(""));
    // The GUI action row's set (Connect, Message, Open PR, Reap), on the
    // same global keys the sidebar uses.
    let mut spans = key_hint(p, "c", "connect");
    spans.push(Span::raw("  "));
    spans.extend(key_hint(p, "t", "message"));
    if row.session.pr_number.is_some() {
        spans.push(Span::raw("  "));
        spans.extend(key_hint(p, "O", "open PR"));
    }
    if !row.is_orchestrator {
        spans.push(Span::raw("  "));
        spans.extend(key_hint(p, "R", "reap"));
    }
    if super::state::row_can_restart(row) {
        spans.push(Span::raw("  "));
        spans.extend(key_hint(p, "^r", "restart"));
    }
    if row.session.workspace_path.is_some() {
        spans.push(Span::raw("  "));
        spans.extend(key_hint(p, "F", "re-file"));
    }
    lines.push(Line::from(spans));

    let border_color = if st.fleet.main_focus == fleet::FleetFocus::Detail { p.working } else { p.rule };
    let block = Block::default().borders(Borders::LEFT).border_style(Style::default().fg(border_color));
    f.render_widget(Paragraph::new(lines).block(block).wrap(Wrap { trim: true }).style(Style::default().fg(p.ink_2)), area);
}

/// The Board's header: the `/` filter, the scope chips (`o` cycles), Resume
/// all when anything is interrupted, and how many cards are shown. Chips
/// that don't fit collapse to a "+n" count rather than clipping.
fn board_header_line(st: &TuiState, width: u16) -> Line<'static> {
    let p = &st.palette;
    let v = &st.kanban;
    let mut left = vec![Span::styled("⌕ ", Style::default().fg(p.accent))];
    match (v.editing_query, v.query.is_empty()) {
        (true, _) => left.push(Span::styled(format!("{}▏", v.query), Style::default().fg(p.ink).add_modifier(Modifier::BOLD))),
        (false, true) => left.extend(key_hint(p, "/", "filter")),
        (false, false) => left.push(Span::styled(v.query.clone(), Style::default().fg(p.ink))),
    }
    left.push(Span::raw("   "));
    let total = super::kanban::shown(&st.rows, &super::kanban::KanbanView::default());
    let mut right = Vec::new();
    let resumable = ninox_overview::board::resume_all_targets(st.rows.iter().map(|r| &r.session)).len();
    if resumable > 0 {
        right.extend(key_hint(p, "A", ""));
        right.push(Span::styled(format!("resume all ({resumable})"), Style::default().fg(p.review)));
        right.push(Span::raw("   "));
    }
    right.push(Span::styled(format!("{}/{total} sessions", super::kanban::shown(&st.rows, v)), Style::default().fg(p.faint)));
    let chip = |label: String, on: bool| {
        if on {
            Span::styled(format!(" {label} "), Style::default().fg(p.paper).bg(p.accent).add_modifier(Modifier::BOLD))
        } else {
            Span::styled(format!(" {label} "), Style::default().fg(p.ink_2))
        }
    };
    let scopes = super::kanban::scopes(&st.rows);
    let room = (width as usize).saturating_sub(spans_width(&left) + spans_width(&right) + 1);
    let mut chips = vec![Span::styled("o ", Style::default().fg(p.ink_2).add_modifier(Modifier::BOLD)), chip("ALL".into(), v.scope.is_none())];
    let mut hidden = 0;
    for (id, name) in &scopes {
        let c = chip(name.clone(), v.scope.as_deref() == Some(id.as_str()));
        if hidden == 0 && spans_width(&chips) + c.width() + 4 <= room {
            chips.push(c);
        } else {
            hidden += 1;
        }
    }
    if hidden > 0 {
        chips.push(Span::styled(format!(" +{hidden}"), Style::default().fg(p.faint)));
    }
    if spans_width(&chips) <= room {
        left.extend(chips);
    }
    spread(left, right, width as usize)
}

/// A column's status-bar/chip color — the same meanings `pr_state_color`
/// gives a PR-list row, now stamped directly on each kanban card (spec:
/// "v1 replaces the shell" — PR state shows on each node, not a list).
fn kanban_color(status: &SessionStatus, p: &Palette) -> Color {
    match status {
        SessionStatus::Working | SessionStatus::Spawning => p.working,
        SessionStatus::PrOpen => p.pr_open,
        SessionStatus::CiFailed => p.ci_failed,
        SessionStatus::ReviewPending => p.review,
        SessionStatus::Mergeable => p.mergeable,
        SessionStatus::Done | SessionStatus::Terminated => p.done,
        SessionStatus::Interrupted => p.review,
        SessionStatus::AwaitingReview => p.review,
        SessionStatus::Queued => p.faint,
    }
}

/// The Board tab: one column per `kanban::COLUMNS` entry (spec: "v1
/// replaces the shell" — the existing Kanban, restyled, "unchanged in
/// function"). Each card shows the session's name, a status-color bar, and
/// its PR chip when it has one.
fn draw_board(f: &mut Frame, st: &TuiState, lay: &Layout) {
    let Some(kl) = &lay.kanban else { return };
    let p = &st.palette;
    f.render_widget(Paragraph::new(board_header_line(st, kl.header.width)), kl.header);
    for (i, col) in super::kanban::COLUMNS.iter().enumerate() {
        let area = kl.columns[i];
        if area.width == 0 || area.height == 0 {
            continue;
        }
        let rows = super::kanban::column_rows(&st.rows, col.status.clone(), &st.kanban);
        let title = format!("{} ({})", col.label, rows.len());
        let selected_here = st.kanban.col == i;
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(if selected_here { p.accent } else { p.rule }))
            .title(Span::styled(title, Style::default().fg(p.ink_2).add_modifier(Modifier::BOLD)));
        let inner = block.inner(area);
        f.render_widget(block, area);
        let visible = inner.height as usize;
        let offset = if selected_here && visible > 0 { st.kanban.row.saturating_sub(visible - 1) } else { 0 };
        for (k, row) in rows.iter().enumerate().skip(offset).take(visible) {
            let y = inner.y + (k - offset) as u16;
            let line_area = Rect { y, height: 1, ..inner };
            let color = kanban_color(&row.session.status, p);
            let selected = selected_here && st.kanban.row == k;
            let bg = if selected { p.highlight } else { p.paper };
            let mut spans = vec![Span::styled("▌", Style::default().fg(color)), Span::raw(" ")];
            // The full "queued · #N" when the name still fits beside it, else
            // the short "q#N" (a Board column is ~20 cells at 140 columns).
            let queued = (row.session.status == SessionStatus::Queued).then(|| {
                let full = format!(" {}", ninox_ui_shared::session_queue::queued_badge(row.queue_position));
                let fits = row.session.name.chars().count() + full.chars().count() + 2 <= inner.width as usize;
                match row.queue_position {
                    Some(n) if !fits => format!(" q#{n}"),
                    _ => full,
                }
            });
            let chip_budget = (inner.width as usize).saturating_sub(2 + row.session.name.chars().count().min(10));
            let pr_chips = if queued.is_none() { super::pr_chips::fitting(row.id(), &row.prs, 1, chip_budget) } else { Vec::new() };
            let chips_w: usize = pr_chips.iter().map(|c| c.0.chars().count() + 1).sum();
            let tail_w = queued.as_ref().map_or(
                if chips_w > 0 { chips_w + 2 } else if row.session.pr_number.is_some() { 9 } else { 2 },
                |q| q.chars().count() + 2,
            );
            let name_w = (inner.width as usize).saturating_sub(tail_w);
            spans.push(Span::styled(truncate(&row.session.name, name_w), Style::default().fg(p.ink)));
            if let Some(q) = queued {
                spans.push(Span::styled(q, Style::default().fg(p.faint)));
            } else if !pr_chips.is_empty() {
                st.chip_scratch.borrow_mut().clear();
                spans.extend(super::pr_chips::spans(st, row, 1, Modifier::empty(), chip_budget));
            } else if let Some(pr) = row.session.pr_number {
                spans.push(Span::styled(format!(" #{pr}"), Style::default().fg(p.ink_2)));
            }
            let line = Line::from(spans);
            super::pr_chips::register(st, std::slice::from_ref(&line), line_area, false);
            f.render_widget(Paragraph::new(line).style(Style::default().bg(bg)), line_area);
        }
        if rows.is_empty() && inner.height > 0 {
            f.render_widget(Paragraph::new(Span::styled(" empty", Style::default().fg(p.faint))), Rect { height: 1, ..inner });
        }
    }
}

fn machine_health_dot(h: ninox_core::types::MachineHealth, p: &Palette) -> (&'static str, Color) {
    use ninox_core::types::MachineHealth;
    match h {
        MachineHealth::Connected => ("●", p.working),
        MachineHealth::Reconnecting => ("●", p.review),
        MachineHealth::Unreachable => ("●", p.ci_failed),
    }
}

/// The Remotes tab: a read-only list of configured machines
/// (`TuiState::remote_rows`, refreshed each tick) with their last-observed
/// connection health. No mockup exists for this screen (see the PR's
/// deviation list) — this is a minimal, functional list consistent with
/// the existing palette, not a port of a design.
fn draw_remotes(f: &mut Frame, st: &TuiState, lay: &Layout) {
    let Some(area) = lay.remotes else { return };
    let p = &st.palette;
    let area = if st.workspaces.open {
        let left_width = area.width / 2;
        let left = Rect { width: left_width, ..area };
        let panel = Rect { x: area.x + left_width, width: area.width.saturating_sub(left_width), ..area };
        draw_workspaces_panel(f, st, panel);
        left
    } else {
        area
    };
    if st.remote_rows.is_empty() {
        empty_state(
            f,
            area,
            p,
            "No remote machines configured",
            key_hint(p, "a", "add a remote machine"),
            Some(vec![Span::styled("ninox machine add", Style::default().fg(p.faint))]),
        );
        return;
    }
    let pad = Rect { x: area.x + 1, width: area.width.saturating_sub(2), ..area };
    for (i, row) in st.remote_rows.iter().enumerate().take(pad.height as usize) {
        let y = pad.y + i as u16;
        let line_area = Rect { y, height: 1, ..pad };
        let selected = st.remotes.selected == i;
        let bg = if selected { p.highlight } else { p.paper };
        // `ink_2` rather than `faint` while selected — `highlight` is
        // deliberately the same colour as `faint` (see `Palette::terminal`'s
        // doc comment), so `faint` text would vanish on a selected row.
        let dim_fg = if selected { p.ink_2 } else { p.faint };
        let mut spans = Vec::new();
        match row.health {
            Some(h) => {
                let (glyph, color) = machine_health_dot(h, p);
                spans.push(Span::styled(format!("{glyph} "), Style::default().fg(color)));
            }
            None => spans.push(Span::styled("○ ", Style::default().fg(dim_fg))),
        }
        let enabled = if row.enabled { "" } else { " (disabled)" };
        spans.push(Span::styled(format!("{}{enabled}", row.label), Style::default().fg(p.ink).add_modifier(Modifier::BOLD)));
        spans.push(Span::styled(format!("  {}", row.ssh_target), Style::default().fg(dim_fg)));
        spans.push(Span::styled(format!("  · {}", row.remote_session), Style::default().fg(dim_fg)));
        f.render_widget(Paragraph::new(Line::from(spans)).style(Style::default().bg(bg)), line_area);
    }
}

/// The Remotes tab's `W` panel (MLOPS-5418): a read-only list of leftover
/// agent worktrees, rendered beside the remote-machines list rather than
/// replacing it. See `workspaces_panel::collect` for how rows are built.
fn draw_workspaces_panel(f: &mut Frame, st: &TuiState, area: Rect) {
    let p = &st.palette;
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(p.rule))
        .title(Span::styled(" WORKSPACES (W) ", Style::default().fg(p.ink).add_modifier(Modifier::BOLD)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height == 0 {
        return;
    }
    // Last line is always the hint/preview footer, never a data row — a
    // narrow or short panel still never loses it to row overflow.
    let footer_y = inner.y + inner.height - 1;
    let rows_area = Rect { height: inner.height - 1, ..inner };

    if st.workspace_rows.is_empty() {
        empty_state(f, rows_area, p, "No leftover workspaces", key_hint(p, "W", "close"), None);
    } else {
        let pad = Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..rows_area };
        for (i, row) in st.workspace_rows.iter().enumerate().take(pad.height as usize) {
            let y = pad.y + i as u16;
            let line_area = Rect { y, height: 1, ..pad };
            let selected = st.workspaces.selected == i;
            let bg = if selected { p.highlight } else { p.paper };
            let line = crate::tui::workspaces_panel::format_row_line(row, pad.width as usize);
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(line, Style::default().fg(p.ink))))
                    .style(Style::default().bg(bg)),
                line_area,
            );
        }
    }

    let footer_area = Rect { y: footer_y, height: 1, ..inner };
    let mut footer_spans = key_hint(p, "p", "prune (preview)");
    footer_spans.push(Span::styled("  ·  ", Style::default().fg(p.rule)));
    footer_spans.extend(key_hint(p, "esc", "close"));
    if st.workspaces.preview {
        const SEP: &str = "  —  ";
        // The hint prefix is fixed-width; the preview summary gets whatever
        // the panel's actual width leaves over, truncated to fit — so it's
        // cut cleanly (with an ellipsis) instead of silently clipped
        // mid-word by the renderer at the area's edge.
        let used: usize = footer_spans.iter().map(|s| s.content.chars().count()).sum::<usize>() + SEP.chars().count();
        let remaining = (inner.width as usize).saturating_sub(used);
        let summary = crate::tui::workspaces_panel::preview_summary_line(&st.workspace_rows);
        if remaining > 0 {
            footer_spans.push(Span::styled(SEP, Style::default().fg(p.rule)));
            footer_spans.push(Span::styled(
                crate::tui::workspaces_panel::truncate(&summary, remaining),
                Style::default().fg(p.ink_2),
            ));
        }
    }
    f.render_widget(Paragraph::new(Line::from(footer_spans)), footer_area);
}

/// The backlinks ("Cited by") / related / outlinks ("Links to") panel
/// under the reading pane: one header per non-empty section, its entries
/// below, the `link_focus`ed row (`L` to enter) marked with `▸` and
/// scrolled into view — `Enter` jumps to it (`BrainView::jump_to_focused_link`).
fn draw_brain_links(f: &mut Frame, area: Rect, b: &super::brain::BrainView, p: &Palette) {
    use ninox_core::brain::display_title;
    let rows = b.link_rows();
    let mut lines: Vec<Line> = Vec::new();
    let mut last_label: Option<&str> = None;
    let mut focused_line = None;
    for (i, (label, e)) in rows.iter().enumerate() {
        if last_label != Some(*label) {
            let count = rows.iter().filter(|(l, _)| l == label).count();
            lines.push(Line::from(Span::styled(format!("{label} · {count}"), Style::default().fg(p.faint))));
            last_label = Some(*label);
        }
        let focused = b.link_focus == Some(i);
        if focused {
            focused_line = Some(lines.len());
        }
        let color = if focused { p.accent } else { p.ink_2 };
        lines.push(Line::from(vec![
            Span::styled(if focused { "▸ " } else { "  " }, Style::default().fg(color)),
            Span::styled(truncate(&display_title(e), area.width.saturating_sub(2) as usize), Style::default().fg(color)),
        ]));
    }
    let offset = focused_line.map(|i| i.saturating_sub((area.height as usize) / 2)).unwrap_or(0) as u16;
    f.render_widget(Paragraph::new(lines).scroll((offset, 0)), area);
}

/// The Neighbourhood graph (`Tab` from the List view): a one-line
/// breadcrumb in `bl.search`'s rect, then the rataflow canvas
/// (`brain_flow::build_neighbourhood_flow`, rebuilt every render like the
/// opened island's own card tree) filling `bl.graph`.
/// The Brain tab's graph mode (`Tab`): a breadcrumb naming the current
/// scope, then either the Neighbourhood canvas or the Clusters table.
/// `GraphScope::Everything` has no TUI branch at all — the breadcrumb's own
/// hint is where the brief's "Everything is GUI-only; use Clusters" lives,
/// since there's no scope-bar widget to disable a third option on.
fn draw_brain_graph(f: &mut Frame, bl: &super::layout::BrainLayout, st: &TuiState) {
    use super::brain::GraphScope;
    use ninox_core::brain::display_title;
    let p = &st.palette;
    let b = &st.brain;
    let mut header = vec![Span::styled("Brain graph", Style::default().fg(p.ink_2)), Span::styled(" › ", Style::default().fg(p.faint))];
    match b.graph_scope {
        GraphScope::Neighbourhood => {
            let focus_title = b.graph_focus.as_deref().and_then(|id| b.entries.iter().find(|e| e.id == id)).map(display_title);
            header.push(match &focus_title {
                Some(t) => Span::styled(t.clone(), Style::default().fg(p.ink).add_modifier(Modifier::BOLD)),
                None => Span::styled("nothing selected", Style::default().fg(p.faint)),
            });
        }
        GraphScope::Clusters => header.push(Span::styled("by repo", Style::default().fg(p.ink).add_modifier(Modifier::BOLD))),
    }
    header.push(Span::styled(format!("  ·  {}", b.graph_scope.label()), Style::default().fg(p.faint)));
    let hint = match b.graph_scope {
        GraphScope::Neighbourhood => "   s cycles · Everything is GUI-only, use Clusters",
        GraphScope::Clusters => "   s cycles grouping · ↵ filters list",
    };
    header.push(Span::styled(hint, Style::default().fg(p.faint)));
    f.render_widget(Paragraph::new(Line::from(header)), bl.search);
    if bl.graph.height == 0 {
        return;
    }
    match b.graph_scope {
        GraphScope::Neighbourhood => draw_brain_neighbourhood(f, bl, st),
        GraphScope::Clusters => draw_brain_clusters(f, bl, st),
    }
}

fn draw_brain_neighbourhood(f: &mut Frame, bl: &super::layout::BrainLayout, st: &TuiState) {
    let p = &st.palette;
    let b = &st.brain;
    if b.graph_focus.is_none() {
        empty_state(f, bl.graph, p, "Nothing to show", key_hint(p, "esc", "back to the list"), None);
        return;
    }
    b.ensure_graph_flow();
    let mut flow = b.graph_flow_mut();
    match flow.as_mut() {
        Some(flow) => f.render_widget(flow, bl.graph),
        None => empty_state(f, bl.graph, p, "No links from this entry", key_hint(p, "o", "open it in the list"), None),
    }
}

/// A ranked table (`ninox_core::brain::cluster_by_repo`'s own order — count
/// descending, alphabetical ties), the TUI's translation of the GUI's
/// Clusters bubbles. `Enter` jumps to the List view filtered to the
/// selected row's repo (`repo_filter`, the same field `r` sets there)
/// rather than trying to express "a graph scoped to one repo" as a second
/// kind of graph.
fn draw_brain_clusters(f: &mut Frame, bl: &super::layout::BrainLayout, st: &TuiState) {
    use ninox_core::brain::kind_label;
    let p = &st.palette;
    let b = &st.brain;
    let clusters = b.clusters();
    if clusters.is_empty() {
        empty_state(f, bl.graph, p, "Nothing to show", key_hint(p, "esc", "back to the list"), None);
        return;
    }
    let header_line = Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(format!("{:<24}", "REPO"), Style::default().fg(p.faint)),
        Span::styled(format!("{:>4}  ", "CNT"), Style::default().fg(p.faint)),
        Span::styled("KIND", Style::default().fg(p.faint)),
    ]);
    let mut lines: Vec<Line> = vec![header_line];
    lines.extend(clusters.iter().enumerate().map(|(i, c)| {
        let selected = i == b.clusters_cursor;
        let style = if selected { Style::default().fg(p.accent).add_modifier(Modifier::BOLD) } else { Style::default().fg(p.ink) };
        Line::from(vec![
            Span::styled(if selected { "▸ " } else { "  " }, style),
            Span::styled(format!("{:<24}", truncate(&c.label, 24)), style),
            Span::styled(format!("{:>4}  ", c.count), Style::default().fg(p.faint)),
            Span::styled(kind_label(&c.dominant_kind), Style::default().fg(p.ink_2)),
        ])
    }));
    f.render_widget(Paragraph::new(lines), bl.graph);
}

/// `s` padded or cut to exactly `w` columns.
fn cell(s: &str, w: usize) -> String {
    let t = truncate(s, w.saturating_sub(1));
    let pad = w.saturating_sub(t.width());
    format!("{t}{}", " ".repeat(pad))
}

/// Brain › Sources (`S`): `BrainSources.dc.html`'s source cards as one
/// table row each, its edit panel as a read-only detail pane on the right
/// (`↵`), and its "Recent sync activity" underneath. Every source value is
/// shown through `SourceField::shown` / `redact_credentials`, so a
/// credential in a URL or a sync error never reaches the screen.
fn draw_brain_sources(f: &mut Frame, area: Rect, st: &TuiState) {
    use ninox_ui_shared::brain_sources::{self as shared, Health, SourceType};
    let p = &st.palette;
    let v = &st.brain.sources;
    if area.height < 3 || area.width < 20 {
        return;
    }
    let n = v.catalogues.len();
    let attention = v.needs_attention();
    let mut summary = format!("  {n} source{}", if n == 1 { "" } else { "s" });
    if attention > 0 {
        summary.push_str(&format!(" · {attention} need{} attention", if attention == 1 { "s" } else { "" }));
    }
    let title = spread(
        vec![
            Span::styled("Brain", Style::default().fg(p.ink_2)),
            Span::styled(" › ", Style::default().fg(p.faint)),
            Span::styled("Sources", Style::default().fg(p.ink).add_modifier(Modifier::BOLD)),
            Span::styled(summary, Style::default().fg(if attention > 0 { p.ci_failed } else { p.faint })),
        ],
        vec![Span::styled("searched by Brain, workers and the prompt ", Style::default().fg(p.faint))],
        area.width as usize,
    );
    f.render_widget(Paragraph::new(title), Rect { height: 1, ..area });
    f.render_widget(
        Paragraph::new(Span::styled("─".repeat(area.width as usize), Style::default().fg(p.rule))),
        Rect { y: area.y + 1, height: 1, ..area },
    );
    let body = Rect { y: area.y + 2, height: area.height - 2, ..area };

    let detail_w = if v.detail && v.selected().is_some() { (body.width * 2 / 5).clamp(34, 60).min(body.width / 2) } else { 0 };
    let list = Rect { width: body.width - detail_w, ..body };
    let detail = Rect { x: list.right(), width: detail_w, ..body };

    // Sync activity sits under the table when there's room for both.
    let log_h = if v.log.is_empty() { 0 } else { (v.log.len().min(5) as u16 + 2).min(list.height.saturating_sub(n as u16 + 3)) };
    let table = Rect { height: list.height - log_h, ..list };

    if n == 0 {
        empty_state(f, table, p, "No brain sources", key_hint(p, "a", "adds a local folder, GitHub wiki or S3 prefix"), None);
    } else {
        let w = table.width.saturating_sub(1) as usize;
        let lead = 3;
        let kind_w = 13;
        let min_sync = 12;
        let pending_w = if w >= lead + 12 + kind_w + min_sync + 11 { 11 } else { 0 };
        let name_w = 24.min(w.saturating_sub(lead + kind_w + min_sync + pending_w));
        let sync_w = 32.min(w.saturating_sub(lead + name_w + kind_w + pending_w));
        let error_w = w.saturating_sub(lead + name_w + kind_w + sync_w + pending_w);
        let head = |s: &str, w: usize| Span::styled(cell(s, w), Style::default().fg(p.faint));
        let mut header = vec![Span::raw(" ".repeat(lead)), head("NAME", name_w), head("KIND", kind_w), head("SYNC", sync_w), head("PENDING", pending_w)];
        if error_w > 0 {
            header.push(head("LAST ERROR", error_w));
        }
        f.render_widget(Paragraph::new(Line::from(header)), Rect { height: 1, ..table });

        let rows_area = Rect { y: table.y + 1, height: table.height.saturating_sub(1), ..table };
        let visible = rows_area.height as usize;
        let offset = (v.cursor + 1).saturating_sub(visible);
        let now = shared::now_unix();
        for (k, (i, c)) in v.catalogues.iter().enumerate().skip(offset).take(visible).enumerate() {
            let y = rows_area.y + k as u16;
            let row = Rect { y, height: 1, ..rows_area };
            let selected = i == v.cursor;
            if selected {
                fill(f, row, p.highlight);
            }
            let status = v.status_of(c);
            let dim = if selected { p.ink_2 } else { p.faint };
            let text_fg = |live: Color| if c.enabled { live } else { dim };
            let kind = SourceType::from_remote(c.remote.as_deref());
            let kind_fg = match kind {
                SourceType::Local => p.pr_open,
                SourceType::GithubWiki => p.mergeable,
                SourceType::S3 => p.review,
            };
            let (dot, dot_fg) = match (c.enabled, shared::health(status)) {
                (false, _) => ("○", dim),
                (true, Health::Failing) => ("●", p.ci_failed),
                (true, Health::Healthy) => ("●", p.working),
            };
            let sync = if status.is_some_and(|s| s.syncing) {
                Span::styled(cell("syncing…", sync_w), Style::default().fg(p.accent).add_modifier(Modifier::BOLD))
            } else {
                Span::styled(cell(&shared::sync_label(c, status, now), sync_w), Style::default().fg(text_fg(p.ink_2)))
            };
            let pending = shared::pending_pushes(status);
            let pending_text = if pending > 0 { format!("{pending} to push") } else { "—".to_string() };
            let name = if c.enabled { c.name.clone() } else { format!("{} (off)", c.name) };
            let name_style = Style::default().fg(text_fg(p.ink)).add_modifier(if selected { Modifier::BOLD } else { Modifier::empty() });
            let mut spans = vec![
                Span::styled(if selected { "▌" } else { " " }, Style::default().fg(p.accent)),
                Span::styled(format!("{dot} "), Style::default().fg(dot_fg)),
                Span::styled(cell(&name, name_w), name_style),
                Span::styled(cell(kind.label(), kind_w), Style::default().fg(text_fg(kind_fg))),
                sync,
                Span::styled(cell(&pending_text, pending_w), Style::default().fg(if pending > 0 { text_fg(p.review) } else { dim })),
            ];
            if error_w > 0 {
                let err = status.and_then(|s| s.last_error.as_deref()).map(shared::redact_credentials).unwrap_or_default();
                spans.push(Span::styled(cell(&err, error_w), Style::default().fg(text_fg(p.ci_failed))));
            }
            f.render_widget(Paragraph::new(Line::from(spans)), row);
        }
    }

    if log_h > 0 {
        let log = Rect { y: table.bottom(), height: log_h, ..list };
        let mut lines = vec![
            Line::from(""),
            Line::from(Span::styled("RECENT SYNC ACTIVITY", Style::default().fg(p.faint).add_modifier(Modifier::BOLD))),
        ];
        for entry in v.log.iter().take(log_h.saturating_sub(2) as usize) {
            let failed = entry.contains(" — failed");
            lines.push(Line::from(Span::styled(
                truncate(&shared::redact_credentials(entry), log.width as usize),
                Style::default().fg(if failed { p.ci_failed } else { p.ink_2 }),
            )));
        }
        f.render_widget(Paragraph::new(lines), log);
    }

    if detail_w > 0 {
        if let Some(c) = v.selected() {
            draw_source_detail(f, detail, st, c);
        }
    }
}

fn draw_source_detail(f: &mut Frame, area: Rect, st: &TuiState, c: &ninox_core::config::CatalogueRef) {
    use ninox_ui_shared::brain_sources::{self as shared, SourceType};
    let p = &st.palette;
    let status = st.brain.sources.status_of(c);
    let block = Block::default().borders(Borders::LEFT).border_style(Style::default().fg(p.rule)).padding(ratatui::widgets::Padding::horizontal(1));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let w = inner.width as usize;
    let label_w = 12;
    let value_w = w.saturating_sub(label_w);
    let field = |label: &str, value: String, fg: Color| {
        Line::from(vec![
            Span::styled(cell(label, label_w), Style::default().fg(p.faint)),
            Span::styled(truncate(&value, value_w), Style::default().fg(fg)),
        ])
    };
    let mut lines = vec![
        Line::from(Span::styled(truncate(&c.name, w), Style::default().fg(p.ink).add_modifier(Modifier::BOLD))),
        Line::from(""),
    ];
    for fd in shared::source_fields(c) {
        lines.push(field(fd.label, fd.shown(), p.ink));
    }
    if let Some(hint) = SourceType::from_remote(c.remote.as_deref()).credentials_hint() {
        lines.push(Line::from(Span::styled(truncate(hint, w), Style::default().fg(p.faint))));
    }
    lines.push(Line::from(""));
    lines.push(field("Entries", status.map_or(0, |s| s.entry_count).to_string(), p.ink_2));
    lines.push(field("State", shared::sync_label(c, status, shared::now_unix()), p.ink_2));
    let pending = shared::pending_pushes(status);
    lines.push(field("Pending", if pending > 0 { format!("{pending} local change(s) to push") } else { "none".into() }, if pending > 0 { p.review } else { p.ink_2 }));
    let conflicts = status.and_then(|s| s.remote_status.as_ref()).map_or(0, |rs| rs.conflict_files.len());
    if conflicts > 0 {
        lines.push(field("Conflicts", format!("{conflicts} kept beside the original"), p.review));
    }
    if let Some(err) = status.and_then(|s| s.last_error.as_deref()) {
        lines.push(field("Last error", shared::redact_credentials(err), p.ci_failed));
    }
    lines.push(Line::from(""));
    let actions = if super::sources::is_builtin(c) { "s sync · set in config.toml [brain]" } else { "e edit · s sync · t on/off · d remove" };
    lines.push(Line::from(Span::styled(truncate(actions, w), Style::default().fg(p.faint))));
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_brain(f: &mut Frame, lay: &Layout, st: &TuiState) {
    use super::brain::{Grouping, Item, UNTAGGED};
    use ninox_core::brain::{display_title, kind_label, normalize_kind, strip_leading_h1, visible_entries, BrainSort};
    let p = &st.palette;
    if st.brain.sources.open {
        draw_brain_sources(f, lay.body, st);
        return;
    }
    let Some(bl) = &lay.brain else { return };
    let b = &st.brain;
    if b.graph_mode {
        draw_brain_graph(f, bl, st);
        return;
    }
    let (query, qstyle) = if b.query.is_empty() && !b.editing_query {
        ("search the brain — click or /".to_string(), Style::default().fg(p.faint))
    } else {
        (format!("{}{}", b.query, if b.editing_query { "▏" } else { "" }), Style::default().fg(p.ink).add_modifier(Modifier::BOLD))
    };
    let searching = !b.query.trim().is_empty();
    let hits: std::collections::HashMap<usize, super::brain::Hit> =
        if searching { b.search().into_iter().map(|h| (h.idx, h)).collect() } else { Default::default() };
    let mut right = match &b.indexing {
        Some(what) => vec![Span::styled(format!("{what}…"), Style::default().fg(p.accent).add_modifier(Modifier::BOLD))],
        None if searching => {
            let related = hits.values().filter(|h| h.related).count();
            let mut v = vec![Span::styled(format!("{} match{}", hits.len() - related, if hits.len() - related == 1 { "" } else { "es" }), Style::default().fg(p.faint))];
            if b.searching.as_deref() == Some(b.query.as_str()) {
                v.push(Span::styled(" · finding related…", Style::default().fg(p.accent)));
            } else if related > 0 {
                v.push(Span::styled(format!(" · {related} related"), Style::default().fg(p.faint)));
            } else if b.semantic.as_ref().is_none_or(|(q, _)| *q != b.query) {
                v.push(Span::styled(" · ↵ adds related", Style::default().fg(p.faint)));
            }
            v
        }
        None => {
            let n = b.entries.len();
            let any_filter = b.kind_filter.is_some() || b.repo_filter.is_some() || b.tag_filter.is_some();
            let count = if any_filter {
                let visible = visible_entries(&b.entries, &b.edges, b.kind_filter.as_deref(), b.repo_filter.as_deref(), b.tag_filter.as_deref(), "", b.sort).len();
                format!("{visible}/{n} entries")
            } else {
                format!("{n} entr{}", if n == 1 { "y" } else { "ies" })
            };
            let mut v = vec![Span::styled(format!("{count} · {}", b.grouping.label()), Style::default().fg(p.faint))];
            if let Some(k) = &b.kind_filter {
                v.push(Span::styled(format!(" · kind:{}", kind_label(k)), Style::default().fg(p.accent)));
            }
            if let Some(r) = &b.repo_filter {
                v.push(Span::styled(format!(" · repo:{r}"), Style::default().fg(p.accent)));
            }
            if let Some(t) = &b.tag_filter {
                v.push(Span::styled(format!(" · tag:{t}"), Style::default().fg(p.accent)));
            }
            if b.sort != BrainSort::Recent {
                let label = match b.sort {
                    BrainSort::Az => "A–Z",
                    BrainSort::MostLinked => "most linked",
                    BrainSort::Recent => "",
                };
                v.push(Span::styled(format!(" · sort:{label}"), Style::default().fg(p.faint)));
            }
            if b.catalogue_options.len() > 1 {
                v.push(Span::styled(format!(" · {}", b.active_catalogue_label()), Style::default().fg(p.accent)));
            }
            v
        }
    };
    let new_button = bl.buttons.iter().find(|(k, _)| *k == super::layout::BrainButton::New).map(|(_, r)| *r);
    if new_button.is_some() {
        right.push(Span::raw("  "));
        right.push(Span::raw(" ".repeat(super::layout::BrainButton::New.text().len())));
    }
    let search = spread(
        vec![Span::styled("⌕ ", Style::default().fg(if b.editing_query { p.accent } else { p.faint })), Span::styled(query, qstyle)],
        right,
        bl.search.width as usize,
    );
    f.render_widget(Paragraph::new(search), bl.search);
    let button_style = Style::default().fg(p.ink).bg(p.paper_2).add_modifier(Modifier::BOLD);
    for (k, r) in &bl.buttons {
        let style = if *k == super::layout::BrainButton::Delete { button_style.fg(p.ci_failed) } else { button_style };
        f.render_widget(Paragraph::new(Span::styled(k.text(), style)), *r);
    }
    if bl.search.width > 0 && lay.body.height > 2 {
        let under = Rect { y: bl.search.y + 1, height: 1, ..bl.search };
        let color = if b.editing_query { p.accent } else { p.rule };
        f.render_widget(Paragraph::new(Span::styled("─".repeat(under.width as usize), Style::default().fg(color))), under);
    }
    let whole = Rect { width: bl.doc.right().saturating_sub(bl.list.x).max(bl.list.width), ..bl.list };
    if let Some(e) = &b.error {
        empty_state(f, whole, p, "The brain is unavailable", vec![Span::styled(e.clone(), Style::default().fg(p.ci_failed))], None);
        return;
    }
    if bl.items.is_empty() {
        if b.entries.is_empty() {
            empty_state(f, whole, p, "The brain is empty", {
                let mut h = key_hint(p, "a", "adds an entry");
                h.push(Span::styled(" · agents file notes with ", Style::default().fg(p.faint)));
                h.extend(key_hint(p, "ninox brain add", ""));
                h
            }, None);
        } else {
            empty_state(f, whole, p, "No entries match", key_hint(p, "/", "edits the search"), None);
        }
        return;
    }
    let list_w = bl.list.width as usize;
    let list_focused = !b.open && !b.editing_query;
    for (k, (i, item)) in bl.items.iter().enumerate().skip(bl.list_offset).take(bl.list.height as usize).enumerate() {
        let y = bl.list.y + k as u16;
        let selected = i == bl.cursor;
        if selected {
            let row = Rect { x: bl.list.x.saturating_sub(1), y, width: bl.list.width + 1, height: 1 };
            fill(f, row, p.highlight);
            if list_focused {
                f.render_widget(Paragraph::new(Span::styled("▌", Style::default().fg(p.accent))), Rect { width: 1, ..row });
            }
        }
        // `ink_2` rather than `faint` while selected — this row's
        // background is `p.highlight` then, and `faint` is deliberately
        // the same colour as `highlight` (see `Palette::terminal`'s doc
        // comment).
        let dim_fg = if selected { p.ink_2 } else { p.faint };
        let line = match item {
            Item::Group { key, count, folded } => {
                let style = if key == UNTAGGED { Style::default().fg(dim_fg) } else { Style::default().fg(p.ink_2) };
                let count = format!("  ({count})");
                let label = if b.grouping == Grouping::Type { kind_label(key) } else { key.clone() };
                Line::from(vec![
                    Span::styled(if *folded { "▸ " } else { "▾ " }, Style::default().fg(dim_fg)),
                    Span::styled(truncate(&label, list_w.saturating_sub(CHEVRON_W as usize + count.len())), style.add_modifier(Modifier::BOLD)),
                    Span::styled(count, Style::default().fg(dim_fg)),
                ])
            }
            Item::Entry { idx, .. } if searching => {
                let Some(e) = b.entries.get(*idx) else { continue };
                let hit = hits.get(idx);
                let mut spans = vec![
                    Span::styled(if hit.is_some_and(|h| h.related) { " ≈ " } else { " " }, Style::default().fg(dim_fg)),
                    Span::styled(truncate(&display_title(e), list_w.saturating_sub(3).max(1)), Style::default().fg(p.ink).add_modifier(if selected { Modifier::BOLD } else { Modifier::empty() })),
                ];
                if let Some((n, text)) = hit.and_then(|h| h.line.as_ref()) {
                    let room = list_w.saturating_sub(spans_width(&spans) + 2);
                    if room > 6 {
                        spans.push(Span::styled(format!("  {n}:"), Style::default().fg(dim_fg)));
                        let room = room.saturating_sub(format!("{n}:").len());
                        spans.extend(highlight_terms(&truncate(text, room), &b.query, p, dim_fg));
                    }
                }
                Line::from(spans)
            }
            Item::Entry { group, idx } => {
                let Some(e) = b.entries.get(*idx) else { continue };
                let indent = if group.is_some() { "    " } else { " " };
                let kind = if b.grouping == Grouping::Type {
                    Vec::new()
                } else {
                    vec![Span::styled(kind_label(&normalize_kind(&e.entry_type)), Style::default().fg(dim_fg))]
                };
                let name_w = list_w.saturating_sub(spans_width(&kind) + indent.len() + 1).max(list_w.min(NAME_MIN));
                let mut name_style = Style::default().fg(p.ink);
                if selected {
                    name_style = name_style.add_modifier(Modifier::BOLD);
                }
                spread(vec![Span::raw(indent), Span::styled(truncate(&display_title(e), name_w), name_style)], kind, list_w)
            }
        };
        f.render_widget(Paragraph::new(line), Rect { y, height: 1, ..bl.list });
    }
    if bl.doc.width > 0 {
        let rule = Rect { x: bl.list.right() + 1, width: 1, ..bl.list };
        let color = if b.open { p.accent } else { p.rule };
        let bar = vec![Line::from(Span::styled("│", Style::default().fg(color))); rule.height as usize];
        f.render_widget(Paragraph::new(bar), rule);
    }
    match bl.items.get(bl.cursor) {
        Some(Item::Entry { idx, .. }) => {
            let Some(e) = b.entries.get(*idx) else { return };
            let tags = if e.tags.is_empty() { "untagged".to_string() } else { e.tags.iter().map(|t| format!("#{t}")).collect::<Vec<_>>().join(" ") };
            let mut lines = vec![
                Line::from(Span::styled(display_title(e), Style::default().fg(p.ink).add_modifier(Modifier::BOLD))),
                Line::from(Span::styled(
                    format!("{} · {} · {tags}{}", e.id, kind_label(&normalize_kind(&e.entry_type)), e.updated.as_ref().map(|u| format!(" · {u}")).unwrap_or_default()),
                    Style::default().fg(p.faint),
                )),
                Line::from(""),
            ];
            lines.extend(super::markdown::render(super::markdown::strip_frontmatter(&strip_leading_h1(&e.body)), bl.doc_body.width, p));
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).scroll((b.scroll, 0)), bl.doc_body);
            if bl.links.height > 0 {
                draw_brain_links(f, bl.links, b, p);
            }
        }
        Some(Item::Group { key, count, .. }) => {
            let what = match b.grouping {
                Grouping::Tag if key == UNTAGGED => "untagged".to_string(),
                Grouping::Tag => format!("tagged #{key}"),
                _ => format!("of type {}", kind_label(key)),
            };
            let mut lines = vec![
                Line::from(Span::styled(format!("{count} entr{} {what}", if *count == 1 { "y" } else { "ies" }), Style::default().fg(p.ink).add_modifier(Modifier::BOLD))),
                Line::from(""),
            ];
            let members = b.entries.iter().filter(|e| match b.grouping {
                Grouping::Tag if key == UNTAGGED => e.tags.is_empty(),
                Grouping::Tag => e.tags.contains(key),
                _ => normalize_kind(&e.entry_type) == *key,
            });
            for e in members {
                lines.push(Line::from(vec![Span::styled("• ", Style::default().fg(p.faint)), Span::styled(display_title(e), Style::default().fg(p.ink_2))]));
            }
            lines.push(Line::from(""));
            let mut hint = key_hint(p, "space", "folds");
            hint.push(Span::raw("   "));
            hint.extend(key_hint(p, "a", "adds an entry here"));
            lines.push(Line::from(hint));
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), bl.doc_body);
        }
        None => {}
    }
}

/// `text` with each occurrence of a query term emphasised, matching case
/// the way the search does (smart case).
fn highlight_terms(text: &str, query: &str, p: &super::palette::Palette, plain_fg: Color) -> Vec<Span<'static>> {
    let sensitive = query.chars().any(char::is_uppercase);
    let hay = if sensitive { text.to_string() } else { text.to_lowercase() };
    let mut marks = vec![false; text.len()];
    for t in query.split_whitespace() {
        let t = if sensitive { t.to_string() } else { t.to_lowercase() };
        if t.is_empty() || hay.len() != text.len() {
            continue;
        }
        for (i, _) in hay.match_indices(t.as_str()) {
            marks[i..i + t.len()].iter_mut().for_each(|m| *m = true);
        }
    }
    let (plain, hit) = (Style::default().fg(plain_fg), Style::default().fg(p.accent).add_modifier(Modifier::BOLD));
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut start = 0;
    for (i, _) in text.char_indices().skip(1).chain(std::iter::once((text.len(), ' '))) {
        if i == text.len() || marks.get(i) != marks.get(start) {
            out.push(Span::styled(text[start..i].to_string(), if marks.get(start) == Some(&true) { hit } else { plain }));
            start = i;
        }
    }
    out
}

fn draw_settings_bar(f: &mut Frame, sl: &super::layout::SettingsLayout, st: &TuiState) {
    use super::layout::SettingsBarLine;
    let p = &st.palette;
    let v = &st.settings;
    for (k, line) in sl.bar_lines.iter().skip(sl.bar_offset).take(sl.bar.height as usize).enumerate() {
        let r = Rect { y: sl.bar.y + k as u16, height: 1, ..sl.bar };
        let text = match *line {
            SettingsBarLine::Gap => continue,
            SettingsBarLine::Heading(h) => Line::from(Span::styled(h.to_uppercase(), Style::default().fg(p.faint).add_modifier(Modifier::BOLD))),
            SettingsBarLine::Section(s) => {
                let sel = s == v.section;
                let focused = sel && !v.in_fields;
                if focused {
                    fill(f, r, p.highlight);
                }
                let mark = if sel { Span::styled(if focused { "▌ " } else { "▏ " }, Style::default().fg(p.accent)) } else { Span::raw("  ") };
                let style = if sel { Style::default().fg(p.ink).add_modifier(Modifier::BOLD) } else { Style::default().fg(p.ink_2) };
                Line::from(vec![mark, Span::styled(truncate(s.title(), r.width.saturating_sub(2) as usize), style)])
            }
        };
        f.render_widget(Paragraph::new(text), r);
    }
    let rule = Rect { x: sl.bar.right() + 1, width: 1, ..sl.bar };
    let col: Vec<Line> = (0..rule.height).map(|_| Line::from(Span::styled("│", Style::default().fg(p.rule)))).collect();
    f.render_widget(Paragraph::new(col), rule);
}

fn draw_settings(f: &mut Frame, lay: &Layout, st: &TuiState) {
    use super::layout::SettingsLine;
    use super::settings::Kind;
    let p = &st.palette;
    let Some(sl) = &lay.settings else { return };
    let v = &st.settings;
    let faint = Style::default().fg(p.faint);
    let path = ninox_core::config::AppConfig::config_path().display().to_string();
    let room = sl.path.width.saturating_sub(sl.open.width + 2) as usize;
    let version = format!("ninox v{}  ·  ", env!("CARGO_PKG_VERSION"));
    let line = Line::from(vec![
        Span::styled(version.clone(), Style::default().fg(p.ink).add_modifier(Modifier::BOLD)),
        Span::styled(truncate(&path, room.saturating_sub(version.chars().count())), Style::default().fg(p.ink_2)),
    ]);
    f.render_widget(Paragraph::new(line), sl.path);
    f.render_widget(
        Paragraph::new(Span::styled(super::layout::OPEN_CONFIG_LABEL, Style::default().fg(p.ink).bg(p.paper_2).add_modifier(Modifier::BOLD))),
        sl.open,
    );
    if let Some(e) = &v.load_error {
        let r = Rect { y: sl.path.y + 1, ..sl.path };
        if r.y < sl.list.y {
            f.render_widget(Paragraph::new(Span::styled(format!("✗ {e} — changes are not saved until it does"), Style::default().fg(p.ci_failed))), r);
        }
    }
    draw_settings_bar(f, sl, st);
    let section = v.section;
    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(section.title(), Style::default().fg(p.ink).add_modifier(Modifier::BOLD))),
            Line::from(Span::styled(truncate(section.blurb(), sl.title.width as usize), Style::default().fg(p.ink_2))),
        ]),
        sl.title,
    );
    f.render_widget(Paragraph::new(Span::styled(super::layout::SETTINGS_CLOSE_LABEL, Style::default().fg(p.ink).bg(p.paper_2).add_modifier(Modifier::BOLD))), sl.close);
    let hint = if v.editing.is_some() {
        "↵ save · esc cancel"
    } else if v.in_fields {
        "↑↓ field · ↵ change · ←/→ cycle · tab sections · e config · esc back"
    } else {
        "↑↓ section · →/↵/tab fields · e config · esc close"
    };
    f.render_widget(Paragraph::new(Span::styled(truncate(hint, sl.hint.width as usize), faint)), sl.hint);
    let label_w = sl.label_w as usize;
    let w = sl.list.width as usize;
    for (k, line) in sl.lines.iter().skip(sl.offset).take(sl.list.height as usize).enumerate() {
        let r = Rect { y: sl.list.y + k as u16, height: 1, ..sl.list };
        let text = match *line {
            SettingsLine::Gap => continue,
            SettingsLine::Heading(h) => Line::from(Span::styled(h.to_uppercase(), faint.add_modifier(Modifier::BOLD))),
            SettingsLine::Error => {
                let e = v.error.clone().unwrap_or_default();
                // Under the value when the message fits there, else from the label's edge.
                let indent = if label_w + 2 + 2 + e.chars().count() <= w { label_w + 2 } else { 2 };
                Line::from(Span::styled(truncate(&format!("{}✗ {e}", " ".repeat(indent)), w), Style::default().fg(p.ci_failed)))
            }
            SettingsLine::Field(i) => {
                let Some(fd) = v.fields.get(i) else { continue };
                let sel = i == v.selected && v.in_fields;
                if sel {
                    fill(f, r, p.highlight);
                }
                let label_style = if sel { Style::default().fg(p.ink).add_modifier(Modifier::BOLD) } else { Style::default().fg(p.ink_2) };
                let bar = if sel { Span::styled("▌ ", Style::default().fg(p.accent)) } else { Span::raw("  ") };
                let mut spans = vec![bar, Span::styled(format!("{:<label_w$}", truncate(&fd.label, label_w)), label_style)];
                let vw = w.saturating_sub(label_w + 2);
                // `ink_2` rather than `faint` while selected — this row's
                // background is `p.highlight` then, and `faint` is
                // deliberately the same colour as `highlight` (see
                // `Palette::terminal`'s doc comment).
                let dim = if sel { Style::default().fg(p.ink_2) } else { faint };
                match (fd.kind, sel.then_some(v.editing.as_ref()).flatten()) {
                    (_, Some(buf)) => {
                        let shown = truncate(&format!("{buf}▏"), vw);
                        spans.push(Span::styled(shown, Style::default().fg(p.ink).bg(p.paper_2).add_modifier(Modifier::BOLD)));
                    }
                    (Kind::Toggle, None) => {
                        let on = fd.value == "on";
                        let (g, c) = if on { ("● on", p.working) } else { ("○ off", dim.fg.unwrap_or(p.faint)) };
                        spans.push(Span::styled(g, Style::default().fg(c).add_modifier(if on { Modifier::BOLD } else { Modifier::empty() })));
                        if fd.locked {
                            spans.push(Span::styled("  always", dim));
                        }
                    }
                    (Kind::Choice, None) => {
                        let shown = truncate(&fd.value, vw.saturating_sub(4));
                        if sel {
                            spans.push(Span::styled("‹ ", dim));
                            spans.push(Span::styled(shown, Style::default().fg(p.accent).add_modifier(Modifier::BOLD)));
                            spans.push(Span::styled(" ›", dim));
                        } else {
                            spans.push(Span::styled(shown, Style::default().fg(p.ink)));
                        }
                    }
                    (Kind::Text, None) if fd.value.is_empty() => spans.push(Span::styled(truncate(&fd.placeholder, vw), dim)),
                    (Kind::Text, None) => spans.push(Span::styled(truncate(&fd.value, vw), Style::default().fg(p.ink))),
                    (Kind::Action, None) => {
                        let verb = if fd.id == super::settings::FieldId::RestartServer && st.server_restarting {
                            "restarting…"
                        } else if !sel {
                            ""
                        } else if matches!(fd.id, super::settings::FieldId::RestartServer | super::settings::FieldId::RestartAffected) {
                            "↵ restart"
                        } else {
                            "↵ open"
                        };
                        spans.push(Span::styled(verb, Style::default().fg(p.accent)));
                    }
                }
                Line::from(spans)
            }
        };
        f.render_widget(Paragraph::new(text), r);
    }
    if let Some(fd) = v.selected_field().filter(|_| sl.help.height > 0 && sl.help.width > 4) {
        let mut lines = vec![Line::from(Span::styled(fd.label.clone(), Style::default().fg(p.ink).add_modifier(Modifier::BOLD))), Line::from(Span::styled(fd.help.clone(), Style::default().fg(p.ink_2)))];
        let how = match fd.kind {
            Kind::Toggle if fd.locked => "",
            Kind::Toggle => "↵, space or a click toggles it",
            Kind::Choice => "↵, space or a click picks the next · ←/→ cycle",
            Kind::Text if fd.options.is_empty() => "↵ or a click edits it · ↵ saves · esc cancels",
            Kind::Text => "↵ or a click edits it · ←/→ cycle suggestions",
            Kind::Action => "↵, e or a click",
        };
        if !how.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(how, faint)));
        }
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), sl.help);
    }
}

fn draw_worker_types(f: &mut Frame, lay: &Layout, st: &TuiState) {
    use super::settings::Kind;
    let p = &st.palette;
    let Some(wl) = &lay.worker_types else { return };
    let v = &st.worker_types;
    let faint = Style::default().fg(p.faint);
    if let Some(e) = &v.load_error {
        f.render_widget(Paragraph::new(Span::styled(format!("✗ {e} — changes are not saved until it does"), Style::default().fg(p.ci_failed))), wl.names);
        return;
    }

    let total_rows = v.names.len() + 1;
    for row_i in wl.names_offset..total_rows.min(wl.names_offset + wl.names.height as usize) {
        let r = Rect { y: wl.names.y + (row_i - wl.names_offset) as u16, height: 1, ..wl.names };
        let sel = row_i == v.list_selected;
        if sel {
            fill(f, r, p.highlight);
        }
        let is_new_row = row_i == v.names.len();
        let (label, badge): (&str, &str) = match (is_new_row, v.names.get(row_i)) {
            (true, _) => ("+ New type", ""),
            (false, Some(name)) if ninox_core::worker_type::is_system_type(name) => (ninox_core::worker_type::display_name(name), " system · single"),
            (false, Some(name)) if v.custom.contains(name) => (name, " custom"),
            (false, Some(name)) => (name, " built-in"),
            (false, None) => continue,
        };
        let style = if sel { Style::default().fg(p.ink).add_modifier(Modifier::BOLD) } else { Style::default().fg(p.ink_2) };
        let bar = if sel { Span::styled("▌ ", Style::default().fg(p.accent)) } else { Span::raw("  ") };
        // `ink_2` rather than `faint` while selected: this row's background
        // becomes `p.highlight` then, and `faint` is deliberately the same
        // colour as `highlight` (see `Palette::terminal`'s doc comment).
        let badge_style = if sel { Style::default().fg(p.ink_2) } else { faint };
        let line = Line::from(vec![bar, Span::styled(truncate(label, wl.names.width.saturating_sub(2) as usize), style), Span::styled(badge, badge_style)]);
        f.render_widget(Paragraph::new(line), r);
    }

    let label_w = super::layout::SETTINGS_LABEL_W as usize;
    let w = wl.fields.width as usize;
    for (k, fd) in v.fields.iter().enumerate().skip(wl.fields_offset).take(wl.fields.height as usize) {
        let r = Rect { y: wl.fields.y + (k - wl.fields_offset) as u16, height: 1, ..wl.fields };
        let sel = k == v.field_selected;
        if sel && !v.on_list {
            fill(f, r, p.highlight);
        }
        let label_style = if sel { Style::default().fg(p.ink).add_modifier(Modifier::BOLD) } else { Style::default().fg(p.ink_2) };
        let bar = if sel && !v.on_list { Span::styled("▌ ", Style::default().fg(p.accent)) } else { Span::raw("  ") };
        let label = if fd.continued { "" } else { fd.label.as_str() };
        let mut spans = vec![bar, Span::styled(format!("{:<label_w$}", truncate(label, label_w)), label_style)];
        let vw = w.saturating_sub(label_w + 2);
        // `ink_2` rather than `faint` while selected (and not on the list
        // side) — this row's background is `p.highlight` then, and `faint`
        // is deliberately the same colour as `highlight` (see
        // `Palette::terminal`'s doc comment).
        let dim = if sel && !v.on_list { Style::default().fg(p.ink_2) } else { faint };
        match (fd.kind, sel.then_some(v.editing.as_ref()).flatten()) {
            (_, Some(buf)) => {
                let new = if v.adding_rule.is_some() { "+ " } else { "" };
                let shown = truncate(&format!("{new}{buf}▏"), vw);
                spans.push(Span::styled(shown, Style::default().fg(p.ink).bg(p.paper_2).add_modifier(Modifier::BOLD)));
            }
            (Kind::Toggle, None) => {
                let on = fd.value == "on";
                let (g, c) = if on { ("● on", p.working) } else { ("○ off", dim.fg.unwrap_or(p.faint)) };
                spans.push(Span::styled(g, Style::default().fg(c).add_modifier(if on { Modifier::BOLD } else { Modifier::empty() })));
            }
            (Kind::Choice, None) => {
                let shown = truncate(&fd.value, vw.saturating_sub(4));
                if sel && !v.on_list {
                    spans.push(Span::styled("‹ ", dim));
                    spans.push(Span::styled(shown, Style::default().fg(p.accent).add_modifier(Modifier::BOLD)));
                    spans.push(Span::styled(" ›", dim));
                } else {
                    spans.push(Span::styled(shown, Style::default().fg(p.ink)));
                }
            }
            (Kind::Text, None) if fd.value.is_empty() && fd.id == super::worker_types::FieldId::Model && !v.model_hint.is_empty() => {
                spans.push(Span::styled(truncate(&v.model_hint, vw), dim));
            }
            (Kind::Text, None) if fd.value.is_empty() => spans.push(Span::styled("(empty)", dim)),
            (Kind::Text, None) => spans.push(Span::styled(truncate(&fd.value, vw), Style::default().fg(p.ink))),
            (Kind::Action, None) => spans.push(Span::styled(truncate(&fd.value, vw), Style::default().fg(p.ink_2))),
        }
        if fd.locked {
            spans.push(Span::styled("  locked", dim));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), r);
    }

    if wl.help.height > 0 && wl.help.width > 4 {
        let mut lines = Vec::new();
        if let Some(err) = &v.error {
            lines.push(Line::from(Span::styled(format!("✗ {err}"), Style::default().fg(p.ci_failed))));
        }
        if let Some(notice) = &v.notice {
            lines.push(Line::from(Span::styled(notice.clone(), Style::default().fg(p.working))));
        }
        if let Some(fd) = v.selected_field().filter(|_| !v.on_list) {
            lines.push(Line::from(Span::styled(fd.label.clone(), Style::default().fg(p.ink).add_modifier(Modifier::BOLD))));
            lines.push(Line::from(Span::styled(fd.help.clone(), Style::default().fg(p.ink_2))));
        }
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), wl.help);
    }
}

/// Greedy word wrap to `width` columns; a word longer than a line is split.
fn wrap_words(s: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut line = String::new();
    for word in s.split(' ') {
        let mut word = word.to_string();
        loop {
            let sep = usize::from(!line.is_empty());
            if line.width() + sep + word.width() <= width {
                if sep == 1 {
                    line.push(' ');
                }
                line.push_str(&word);
                break;
            }
            if !line.is_empty() {
                out.push(std::mem::take(&mut line));
                continue;
            }
            let mut head = String::new();
            let mut rest = word.chars();
            for c in rest.by_ref() {
                if head.width() + c.to_string().width() > width {
                    word = std::iter::once(c).chain(rest).collect();
                    break;
                }
                head.push(c);
            }
            out.push(head);
            if word.is_empty() || out.last().is_some_and(|h| h.width() == 0) {
                break;
            }
        }
    }
    if !line.is_empty() || out.is_empty() {
        out.push(line);
    }
    out
}

/// `D`'s confirm, wrapped to `width`: the scrolling body (title, warnings,
/// the tree) and the footer that must always show (the override and the
/// buttons). Same plan and wording as the GUI's delete modal.
/// `footer_rows` caps the footer, so the override names as many worktrees
/// as fit and the buttons stay on screen.
fn delete_lines(st: &TuiState, d: &DeleteConfirm, width: usize, footer_rows: usize) -> (Vec<Line<'static>>, Vec<Line<'static>>) {
    use ninox_ui_shared::delete_view as dv;
    let p = &st.palette;
    let dim = Style::default().fg(p.ink_2);
    let bold = Style::default().fg(p.ink).add_modifier(Modifier::BOLD);
    let key = |k: &'static str| Span::styled(k, Style::default().fg(p.accent).add_modifier(Modifier::BOLD));
    let wrapped = |text: &str, indent: usize, style: Style| -> Vec<Line<'static>> {
        let pad = " ".repeat(indent);
        wrap_words(text, width.saturating_sub(indent)).into_iter().map(|l| Line::from(Span::styled(format!("{pad}{l}"), style))).collect()
    };
    let close = |label: &'static str| vec![Line::from(""), Line::from(vec![key("n"), Span::styled(label, dim)])];
    let plan = match &d.plan {
        None => return (wrapped("Checking worktrees…", 0, dim), close(" cancel")),
        Some(Err(e)) => return (wrapped(&format!("Can't delete this session: {e}"), 0, Style::default().fg(p.ci_failed)), close(" close")),
        Some(Ok(plan)) => plan,
    };
    let target = |t: &ninox_core::events::DeleteTarget, indent: usize| -> Vec<Line<'static>> {
        let (detail, at_risk) = dv::target_detail(t, d.force);
        let tone = match (at_risk, d.force) {
            (false, _) => dim,
            (true, false) => Style::default().fg(p.review),
            (true, true) => Style::default().fg(p.ci_failed),
        };
        let mut lines = wrapped(&dv::target_name(t), indent, bold);
        lines.extend(wrapped(&detail, indent + 2, tone));
        lines
    };
    let mut body = wrapped(&dv::title(plan), 0, bold);
    if let Some(warning) = dv::kill_warning(plan) {
        body.extend(wrapped(&warning, 0, Style::default().fg(p.ci_failed).add_modifier(Modifier::BOLD)));
    }
    body.push(Line::from(""));
    if let Some(root) = dv::root_target(plan) {
        body.extend(target(root, 0));
    }
    let under = dv::under_root(plan);
    if !under.is_empty() {
        let n = dv::plural(under.len(), "session", "sessions");
        body.push(Line::from(vec![
            Span::styled(format!("{} {n} under it  ", if d.expanded { "▾" } else { "▸" }), Style::default().fg(p.ink)),
            key("e"),
            Span::styled(if d.expanded { " collapse" } else { " expand" }, dim),
        ]));
        if d.expanded {
            for t in under {
                body.extend(target(t, 2 * t.depth));
            }
        }
    }
    let mut footer = Vec::new();
    let lead = if d.force { "[x] f  " } else { "[ ] f  " };
    let override_rows = footer_rows.saturating_sub(3).max(1);
    let fitting = (0..=plan.at_risk().len()).rev().find_map(|shown| {
        let label = dv::override_label_naming(plan, shown)?;
        let rows = wrap_words(&label, width.saturating_sub(lead.width()));
        (rows.len() <= override_rows || shown == 0).then_some(rows)
    });
    if let Some(rows) = fitting {
        footer.push(Line::from(""));
        let mut rows = rows.into_iter();
        let first = rows.next().unwrap_or_default();
        footer.push(Line::from(vec![
            Span::styled(&lead[..4], Style::default().fg(if d.force { p.ci_failed } else { p.ink })),
            key("f"),
            Span::styled(format!("  {first}"), Style::default().fg(p.ink)),
        ]));
        footer.extend(rows.map(|l| Line::from(Span::styled(format!("{}{l}", " ".repeat(lead.width())), Style::default().fg(p.ink)))));
    }
    footer.push(Line::from(""));
    footer.push(Line::from(vec![
        Span::styled("[ ", dim),
        key("y"),
        Span::styled(" Delete ", Style::default().fg(p.ci_failed).add_modifier(Modifier::BOLD)),
        Span::styled("]   [ ", dim),
        key("n"),
        Span::styled(" Cancel ]", Style::default().fg(p.ink)),
    ]));
    (body, footer)
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

/// The end of `s` that fits in `width` columns — a one-line input keeps the
/// caret in view by scrolling its text left, never wrapping or clipping it.
fn tail_fit(s: &str, width: usize) -> &str {
    let mut used = 0;
    for (i, c) in s.char_indices().rev() {
        used += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used > width {
            return &s[i + c.len_utf8()..];
        }
    }
    s
}

/// The help modal's key column; a longer key runs into its description.
const HELP_KEY_W: usize = 34;

/// How many lines `help_lines` draws, from `HELP` alone: the prefix line,
/// then per group a blank line, its title and its rows.
pub fn help_line_count() -> usize {
    1 + HELP.iter().map(|(_, keys)| 2 + keys.len()).sum::<usize>()
}

pub fn help_lines(st: &TuiState) -> Vec<Line<'static>> {
    let p = &st.palette;
    let faint = Style::default().fg(p.faint);
    let mut lines = vec![Line::from(vec![
        Span::styled("prefix ", faint),
        Span::styled(prefix_label(st.prefix), Style::default().fg(p.accent).add_modifier(Modifier::BOLD)),
        Span::styled("  (change it in Settings, s)   ", faint),
        Span::styled("Ctrl+]", Style::default().fg(p.accent).add_modifier(Modifier::BOLD)),
        Span::styled(" always returns to the fleet", faint),
        Span::styled(format!("   ninox v{}", env!("CARGO_PKG_VERSION")), faint),
    ])];
    for (group, keys) in HELP {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(group.to_string(), Style::default().fg(p.accent).add_modifier(Modifier::BOLD))));
        for (k, v) in keys.iter() {
            lines.push(Line::from(vec![
                Span::styled(format!("  {k:<HELP_KEY_W$}"), Style::default().fg(p.ink).add_modifier(Modifier::BOLD)),
                Span::styled(*v, Style::default().fg(p.ink_2)),
            ]));
        }
    }
    lines
}

fn modal_block(p: &Palette, title: &str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(p.accent))
        .style(Style::default().bg(p.card).fg(p.ink))
        .padding(ratatui::widgets::Padding::horizontal(1))
        .title(Span::styled(format!(" {title} "), Style::default().fg(p.accent).add_modifier(Modifier::BOLD)))
}

fn draw_modal(f: &mut Frame, area: Rect, st: &TuiState, lay: &Layout, modal: &Modal) {
    let p = &st.palette;
    let faint = Style::default().fg(p.faint);
    match modal {
        Modal::Confirm(pending) => {
            let (q, detail) = st.confirm_text(pending);
            let all_label = st.confirm_all_label(pending);
            let (r, yes, no, all) = super::layout::confirm_rects(area, q.width().max(detail.width()) as u16, all_label);
            let title = match pending {
                Pending::Kill(_) => "kill",
                Pending::Remove(_) => "remove",
                Pending::Reap(_) => "reap",
                Pending::DeleteBrain(_) => "delete",
                Pending::DeleteWorkerType(_) => "delete",
                Pending::ResetWorkerType(_) => "reset",
                Pending::RestartAll => "restart all",
                Pending::RestartServer => "restart server",
                Pending::RestartAffected(_) => "restart sessions",
                Pending::ResumeAll => "resume all",
                Pending::Restart(_) => "restart",
                Pending::Refile(_) => "re-file",
                Pending::RemoveMachine(_) => "remove machine",
                Pending::RemoveBrainSource(_) => "remove source",
                Pending::GhostAccept(_) => "spawn",
                Pending::GhostDiscard(_) => "discard",
            };
            let button = |key: &'static str, label: &'static str, primary: bool| {
                let bg = Style::default().bg(if primary { p.paper_2 } else { p.paper });
                vec![
                    Span::styled("[ ", bg.fg(p.faint)),
                    Span::styled(key, bg.fg(p.accent).add_modifier(Modifier::BOLD)),
                    Span::styled(format!(" {label} "), bg.fg(if primary { p.ci_failed } else { p.ink }).add_modifier(Modifier::BOLD)),
                    Span::styled("]", bg.fg(p.faint)),
                ]
            };
            let mut buttons = button("y", "Yes", true);
            buttons.push(Span::raw(" ".repeat((no.x.saturating_sub(yes.right())) as usize)));
            buttons.extend(button("n", "No", false));
            if let (Some(all), Some(label)) = (all, all_label) {
                buttons.push(Span::raw(" ".repeat((all.x.saturating_sub(no.right())) as usize)));
                buttons.extend(button("a", label, false));
            }
            let lines = vec![
                Line::from(Span::styled(q, Style::default().fg(p.ink).add_modifier(Modifier::BOLD))),
                Line::from(Span::styled(detail, faint)),
                Line::from(""),
                Line::from(buttons),
            ];
            f.render_widget(Clear, r);
            f.render_widget(Paragraph::new(lines).block(modal_block(p, title)), r);
        }
        Modal::Delete(d) => {
            let w = (area.width * 2 / 3).clamp(50, 100).min(area.width);
            // At least the title and its warning keep their rows.
            let footer_rows = (area.height.saturating_sub(2) as usize).saturating_sub(3);
            let block = modal_block(p, "delete");
            let text_w = block.inner(Rect { width: w, height: area.height, ..area }).width;
            let (mut body, footer) = delete_lines(st, d, text_w as usize, footer_rows);
            let r = centered(area, w, (body.len() + footer.len() + 2) as u16);
            f.render_widget(Clear, r);
            let inner = block.inner(r);
            f.render_widget(block, r);
            // The override and the buttons always show; the tree gives way.
            let foot_h = (footer.len() as u16).min(inner.height);
            let body_h = (inner.height - foot_h) as usize;
            if body.len() > body_h && body_h > 0 {
                let hidden = body.len() - body_h + 1;
                body.truncate(body_h - 1);
                body.push(Line::from(Span::styled(format!("… {hidden} more lines (e collapses the tree)"), Style::default().fg(p.ink_2))));
            }
            f.render_widget(Paragraph::new(body), Rect { height: body_h as u16, ..inner });
            let skip = footer.len() - foot_h as usize;
            f.render_widget(Paragraph::new(footer[skip..].to_vec()), Rect { y: inner.y + inner.height - foot_h, height: foot_h, ..inner });
        }
        Modal::Notifications(cur) => {
            use ninox_overview::notifications::{kind_label, kind_tone, Tone};
            let r = centered(area, (area.width * 3 / 4).clamp(50, 110), (area.height * 3 / 4).clamp(8, 40));
            f.render_widget(Clear, r);
            let block = modal_block(p, &format!("notifications · {}", st.notifications.len()));
            let inner = block.inner(r);
            f.render_widget(block, r);
            let body_h = inner.height.saturating_sub(1) as usize;
            let w = inner.width as usize;
            let mut lines: Vec<Line> = Vec::new();
            if st.notifications.is_empty() {
                lines.push(Line::from(Span::styled(
                    if st.daemon_up { "Nothing new." } else { "Nothing new — and no ninox daemon is reachable to send any." },
                    Style::default().fg(p.ink_2),
                )));
            }
            // Two rows per entry; keep the cursor's pair on screen.
            let per_page = (body_h / 2).max(1);
            let first = (*cur + 1).saturating_sub(per_page);
            for (i, n) in st.notifications.iter().rev().enumerate().skip(first).take(per_page) {
                let color = match kind_tone(&n.kind) {
                    Tone::Failed => p.ci_failed,
                    Tone::Attention => p.review,
                    Tone::Open => p.pr_open,
                    Tone::Done => p.working,
                };
                let sel = i == *cur;
                let bg = if sel { Style::default().bg(p.highlight) } else { Style::default() };
                let stamp = format!(" {} ", kind_label(&n.kind));
                let clock = super::report::fmt_clock(n.created_at, st.now_ms);
                let room = w.saturating_sub(stamp.width() + clock.width() + 4);
                let head = vec![
                    Span::styled(if sel { "▸ " } else { "  " }, bg.fg(p.accent)),
                    Span::styled(stamp, Style::default().fg(p.paper).bg(color).add_modifier(Modifier::BOLD)),
                    Span::styled(" ", bg),
                    Span::styled(truncate(&n.title, room), bg.fg(p.ink).add_modifier(Modifier::BOLD)),
                ];
                let mut head = spread(head, vec![Span::styled(clock, bg.fg(if sel { p.ink_2 } else { p.faint }))], w);
                let mut body = Line::from(vec![
                    Span::styled("    ", bg),
                    Span::styled(truncate(&n.body.replace('\n', " "), w.saturating_sub(4)), bg.fg(if sel { p.ink } else { p.ink_2 })),
                ]);
                if sel {
                    head = head.style(bg);
                    body = body.style(bg);
                }
                lines.push(head);
                lines.push(body);
            }
            f.render_widget(Paragraph::new(lines), inner);
            let hint = Rect { y: inner.bottom().saturating_sub(1), height: 1, ..inner };
            f.render_widget(Paragraph::new(Span::styled("j/k move · ↵ go to session · d dismiss · D dismiss all · esc close", faint)), hint);
        }
        Modal::Message(m) => {
            let r = centered(area, (area.width * 2 / 3).clamp(44, 90), 4);
            let room = r.width.saturating_sub(7) as usize;
            let lines = vec![
                Line::from(vec![
                    Span::styled("› ", Style::default().fg(p.accent)),
                    Span::styled(format!("{}▏", tail_fit(&m.draft.replace('\n', "↵"), room)),Style::default().fg(p.ink).add_modifier(Modifier::BOLD)),
                ]),
                Line::from(Span::styled("↵ send · esc cancel", faint)),
            ];
            f.render_widget(Clear, r);
            f.render_widget(Paragraph::new(lines).block(modal_block(p, &format!("message {}", m.name))), r);
        }
        Modal::Restore(s) => {
            let text = format!("Restore fleet ({} workers, {} orchestrators)?  y / n", s.workers, s.orchestrators);
            let r = centered(area, text.width() as u16 + 4, 3);
            f.render_widget(Clear, r);
            f.render_widget(Paragraph::new(text).block(modal_block(p, "fleet restore")), r);
        }
        Modal::Spawn => {
            use super::spawn::{estimate_inputs, model_line, visible, FieldId, Kind as SpawnFieldKind};
            // Not `faint`: in `Palette::terminal()` (the default — no
            // `[tui] colors = "field-notes"` set), `faint` and `card` are
            // both `Color::DarkGray`, so `faint` text is invisible against
            // this modal's `modal_block`-filled card background. `ink_2`
            // stays `Color::Reset` there, same as `ink`.
            let dim = Style::default().fg(p.ink_2);
            let form = &st.spawn_form;
            let visible_fields: Vec<_> = form.fields.iter().enumerate().filter(|(_, fd)| visible(form, &fd.id)).collect();
            let label_w = visible_fields.iter().map(|(_, fd)| fd.label.width()).max().unwrap_or(0) + 1;
            // Capped at 90 cols — matching the GUI's fixed 500px dialog
            // width, this would otherwise stretch to 2/3 of a wide terminal.
            let r = centered(area, (area.width * 2 / 3).clamp(50, 90), visible_fields.len() as u16 + 6);
            let mut lines: Vec<Line> = Vec::new();
            for (i, fd) in &visible_fields {
                let sel = *i == form.focus;
                let label_style = if sel { Style::default().fg(p.ink).add_modifier(Modifier::BOLD) } else { Style::default().fg(p.ink_2) };
                let bar = if sel { Span::styled("▌ ", Style::default().fg(p.accent)) } else { Span::raw("  ") };
                let mut spans = vec![bar, Span::styled(format!("{:<label_w$}", fd.label), label_style)];
                match fd.kind {
                    _ if fd.id == FieldId::Brains => {
                        for (n, b) in form.brains.iter().enumerate() {
                            if n > 0 {
                                spans.push(Span::styled("  ", dim));
                            }
                            let on_cursor = sel && b.name == fd.value;
                            let mark = if b.selected { "[x] " } else { "[ ] " };
                            let style = if on_cursor {
                                Style::default().fg(p.accent).add_modifier(Modifier::BOLD)
                            } else if b.selected {
                                Style::default().fg(p.ink)
                            } else {
                                dim
                            };
                            spans.push(Span::styled(format!("{mark}{}", b.name), style));
                            if !b.writable {
                                spans.push(Span::styled(" ro", dim));
                            }
                        }
                    }
                    SpawnFieldKind::Choice if fd.id == FieldId::BrainWrite && fd.value.is_empty() => {
                        spans.push(Span::styled("— no writable brain selected", Style::default().fg(p.ci_failed)));
                    }
                    SpawnFieldKind::Choice if sel => {
                        spans.push(Span::styled("‹ ", dim));
                        spans.push(Span::styled(fd.value.clone(), Style::default().fg(p.accent).add_modifier(Modifier::BOLD)));
                        spans.push(Span::styled(" ›", dim));
                    }
                    SpawnFieldKind::Choice => spans.push(Span::styled(fd.value.clone(), Style::default().fg(p.ink))),
                    _ if fd.id == FieldId::Prompt => {
                        // Multi-line value — show only its first line inline;
                        // a dedicated scrollable editor is future work.
                        let first = fd.value.lines().next().unwrap_or("");
                        let caret = if sel { "▏" } else { "" };
                        spans.push(Span::styled(format!("{first}{caret}"), if sel { Style::default().fg(p.ink).add_modifier(Modifier::BOLD) } else { Style::default().fg(p.ink_2) }));
                    }
                    _ if fd.value.is_empty() && !sel => spans.push(Span::styled("—", dim)),
                    _ => {
                        let caret = if sel { "▏" } else { "" };
                        spans.push(Span::styled(format!("{}{caret}", fd.value), if sel { Style::default().fg(p.ink).add_modifier(Modifier::BOLD) } else { Style::default().fg(p.ink_2) }));
                    }
                }
                lines.push(Line::from(spans));
            }
            lines.push(Line::from(Span::styled(format!("  {}", model_line(form)), dim)));
            lines.push(Line::from(""));
            // The GUI's cost line falls back to a historical per-session
            // average once enough filed sessions exist, via a live Store
            // read (`Store::cost_samples`) — unavailable here since view
            // code only ever sees the already-polled `TuiState`, not the
            // store. The TUI's line always shows the static prior.
            let (harness, model) = estimate_inputs(form);
            let estimate = ninox_ui_shared::spawn_form::estimate_text(
                ninox_ui_shared::spawn_form::static_prior(&harness, model.as_deref()), &[],
            );
            lines.push(Line::from(Span::styled(estimate, dim)));
            if let Some(e) = &form.error {
                lines.push(Line::from(Span::styled(format!("✗ {e}"), Style::default().fg(p.ci_failed))));
            }
            let hint = if form.fields.get(form.focus).is_some_and(|fd| fd.id == FieldId::Brains) {
                "tab/shift-tab field · ←/→ move · space toggle brain · ctrl+enter spawn"
            } else {
                "tab/shift-tab field · ←/→ choose · ctrl+enter spawn · esc cancel"
            };
            lines.push(Line::from(Span::styled(hint, dim)));
            f.render_widget(Clear, r);
            f.render_widget(Paragraph::new(lines).block(modal_block(p, "spawn a session")), r);
        }
        Modal::SourceForm(form) => {
            // `ink_2`, not `faint`: `faint` is invisible on `card` in the
            // terminal palette (see the Spawn arm).
            let dim = Style::default().fg(p.ink_2);
            let fields = form.fields();
            let label_w = fields.iter().map(|fd| form.label(*fd).width()).max().unwrap_or(0) + 2;
            let hint = form.form.source_type.credentials_hint();
            let extra = 3 + u16::from(hint.is_some()) + u16::from(form.form.error.is_some());
            let r = centered(area, (area.width * 2 / 3).clamp(50, 84), fields.len() as u16 + extra + 2);
            let room = (r.width as usize).saturating_sub(label_w + 6);
            let mut lines: Vec<Line> = Vec::new();
            for fd in &fields {
                let sel = *fd == form.focus;
                let label_style = if sel { Style::default().fg(p.ink).add_modifier(Modifier::BOLD) } else { dim };
                let bar = if sel { Span::styled("▌ ", Style::default().fg(p.accent)) } else { Span::raw("  ") };
                let mut spans = vec![bar, Span::styled(format!("{:<label_w$}", form.label(*fd)), label_style)];
                let value = form.shown(*fd);
                if fd.is_choice() {
                    if sel {
                        spans.push(Span::styled("‹ ", dim));
                        spans.push(Span::styled(value, Style::default().fg(p.accent).add_modifier(Modifier::BOLD)));
                        spans.push(Span::styled(" ›", dim));
                    } else {
                        spans.push(Span::styled(value, Style::default().fg(p.ink)));
                    }
                } else if value.is_empty() && !sel {
                    spans.push(Span::styled("—", dim));
                } else if sel {
                    spans.push(Span::styled(format!("{}▏", tail_fit(&value, room)), Style::default().fg(p.ink).add_modifier(Modifier::BOLD)));
                } else {
                    spans.push(Span::styled(truncate(&value, room), dim));
                }
                lines.push(Line::from(spans));
            }
            lines.push(Line::from(""));
            if let Some(hint) = hint {
                lines.push(Line::from(Span::styled(hint, dim)));
            }
            if let Some(e) = &form.form.error {
                lines.push(Line::from(Span::styled(format!("✗ {e}"), Style::default().fg(p.ci_failed))));
            }
            lines.push(Line::from(Span::styled("tab/↑↓ field · ←/→ choose · ↵ save · esc cancel", dim)));
            let title = if form.form.editing.is_some() { "edit source" } else { "add source" };
            f.render_widget(Clear, r);
            f.render_widget(Paragraph::new(lines).block(modal_block(p, title)), r);
        }
        Modal::AddRemote(m) => {
            let r = centered(area, (area.width * 2 / 3).max(44), 7);
            let active = Style::default().fg(p.ink).add_modifier(Modifier::BOLD);
            let idle = Style::default().fg(p.ink_2);
            let (hs, ls) = if m.on_label_field { (idle, active) } else { (active, idle) };
            let caret = |on: bool| if on { "▏" } else { "" };
            let label = |on: bool| Style::default().fg(if on { p.accent } else { p.faint });
            let label_span = if m.label.is_empty() {
                let placeholder = m.host.rsplit('@').next().unwrap_or(&m.host);
                Span::styled(format!("{placeholder}{}", caret(m.on_label_field)), Style::default().fg(p.faint))
            } else {
                Span::styled(format!("{}{}", m.label, caret(m.on_label_field)), ls)
            };
            let lines = vec![
                Line::from(vec![Span::styled("host   ", label(!m.on_label_field)), Span::styled(format!("{}{}", m.host, caret(!m.on_label_field)), hs)]),
                Line::from(vec![Span::styled("label  ", label(m.on_label_field)), label_span]),
                match &m.error {
                    Some(e) => Line::from(Span::styled(e.clone(), Style::default().fg(p.ci_failed))),
                    None => Line::from(""),
                },
                Line::from(Span::styled("tab switch field · ↵ add · esc cancel", faint)),
            ];
            f.render_widget(Clear, r);
            f.render_widget(Paragraph::new(lines).block(modal_block(p, "add a remote machine")), r);
        }
        Modal::Help => {
            let lines = help_lines(st);
            let r = centered(area, 104, help_line_count() as u16 + 2);
            let max = lay.help_max_scroll;
            let hint = if max == 0 {
                "any key closes".to_string()
            } else {
                let more = if st.help_scroll < max { "↓ more · " } else { "" };
                format!("{more}j/k scroll · any other key closes")
            };
            let block = modal_block(p, "keys").title_bottom(Line::from(Span::styled(format!(" {hint} "), faint)).right_aligned());
            f.render_widget(Clear, r);
            f.render_widget(Paragraph::new(lines).scroll((st.help_scroll.min(max), 0)).block(block), r);
        }
        Modal::Goto(g) => {
            let r = centered(area, (area.width * 3 / 4).clamp(40, 110), (area.height * 3 / 4).clamp(8, 40));
            f.render_widget(Clear, r);
            let block = modal_block(p, "go to");
            let inner = block.inner(r);
            f.render_widget(block, r);
            let matches = st.goto_matches(g);
            let mut filters: Vec<Span> = Vec::new();
            for fl in super::state::GotoFilter::default_order() {
                let style = if *fl == g.filter {
                    Style::default().fg(p.accent).add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                } else {
                    faint
                };
                filters.push(Span::styled(fl.label(), style));
                filters.push(Span::raw("   "));
            }
            let mut lines = vec![
                Line::from(vec![Span::styled("⌕ ", Style::default().fg(p.accent)), Span::raw(format!("{}▏", g.query))]),
                Line::from(filters),
                Line::from(Span::styled("─".repeat(inner.width as usize), Style::default().fg(p.rule))),
            ];
            let h = inner.height.saturating_sub(4) as usize;
            let offset = g.selected.saturating_sub(h.saturating_sub(1));
            for (k, &i) in matches.iter().enumerate().skip(offset).take(h) {
                let mut line = session_line(st, i, inner.width as usize, LineKind::Flat, false);
                if k == g.selected {
                    line = line.style(Style::default().bg(p.paper_2).add_modifier(Modifier::BOLD));
                }
                lines.push(line);
            }
            if matches.is_empty() {
                lines.push(Line::from(Span::styled("no matching agents", faint)));
            }
            f.render_widget(Paragraph::new(lines), inner);
            let hint = Rect { y: inner.bottom().saturating_sub(1), height: 1, ..inner };
            f.render_widget(Paragraph::new(Span::styled("type to filter · tab state · ↑↓ move · ↵ open · esc close", faint)), hint);
        }
        Modal::Settings => {
            f.render_widget(Clear, area);
            f.render_widget(modal_block(p, "settings"), area);
            let sl = super::layout::settings(super::layout::settings_area(area), st);
            let modal_lay = Layout { settings: Some(sl), ..Default::default() };
            draw_settings(f, &modal_lay, st);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::layout;
    use crate::tui::pane::{tests::snap, PaneView};
    use crate::tui::state::{Row, TuiState};

    fn render_buffer(st: &TuiState, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let lay = layout::compute(ratatui::layout::Rect::new(0, 0, w, h), st);
        terminal.draw(|f| draw(f, st, &lay)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn render(st: &TuiState, w: u16, h: u16) -> String {
        let buf = render_buffer(st, w, h);
        let mut out = String::new();
        for y in 0..h {
            for x in 0..w {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn st_with(ids: &[&str]) -> TuiState {
        TuiState { rows: ids.iter().map(|i| Row::test_row(i)).collect(), ..Default::default() }
    }

    #[test]
    fn footer_shows_the_config_warning_over_hints_but_an_active_notice_still_wins() {
        let mut st = TuiState {
            config_warning: Some("config.toml line 3: bad · running with defaults, saves disabled".into()),
            ..Default::default()
        };
        // No active notice: the persistent warning beats the ordinary hints.
        let out = render(&st, 100, 20);
        assert!(out.contains("config.toml line 3"), "{out}");

        // An active notice (e.g. a spawn failure) must still be seen even
        // while config.toml is broken — it must not be permanently hidden.
        st.notice = Some(crate::tui::state::Notice { text: "spawn failed".into(), level: Level::Error, expires_ms: i64::MAX });
        let out = render(&st, 100, 20);
        assert!(out.contains("spawn failed"), "{out}");
    }

    #[test]
    fn footer_shows_a_restart_in_flight_then_server_alerts() {
        let mut st = TuiState {
            server_restarting: true,
            server_alerts: vec![ninox_ui_shared::server_restart::classify_issue("credential-provider exited").unwrap()],
            ..Default::default()
        };
        let out = render(&st, 140, 20);
        assert!(out.contains("Restarting server"), "{out}");
        st.server_restarting = false;
        let out = render(&st, 140, 20);
        assert!(out.contains("aws sso login"), "{out}");
        assert!(out.contains("Settings › Restart server"), "{out}");
    }

    #[test]
    fn footer_truncates_a_long_config_warning_with_an_ellipsis_at_the_window_edge() {
        let st = TuiState {
            config_warning: Some(
                "config.toml line 3: this toml error message is deliberately long enough that it cannot possibly fit in a narrow footer · running with defaults, saves disabled".into(),
            ),
            ..Default::default()
        };
        let out = render(&st, 40, 20);
        let footer_line = out.lines().last().unwrap_or("");
        assert!(footer_line.contains('…'), "{footer_line:?}");
        assert!(footer_line.width() <= 40, "{footer_line:?}");
    }

    #[test]
    fn fleet_pane_draws_sidebar_rows_and_the_focused_live_pane() {
        let mut st = st_with(&["alpha", "beta"]);
        // Fleet's canvas is the default content now; the sidebar+pane split
        // (today's assertions) only renders once `c` has attached a pane.
        st.focus = Focus::Pane;
        st.ptyd = PtydState::Up;
        st.panes.insert(
            "alpha".into(),
            ninox_ptyd::PaneInfo {
                pane: "alpha".into(),
                pid: 7,
                cols: 80,
                rows: 24,
                alive: true,
                exit_code: None,
                created_ms: 0,
                last_output_ms: 0,
                title: None,
                cwd: "/".into(),
                seq: 1, history_size: 0,
            },
        );
        st.views.insert("alpha".into(), PaneView { live: Some(snap(&["hello from agent"], 0, 40)), ..Default::default() });
        let out = render(&st, 100, 20);
        assert!(out.contains("AGENT"), "a live, non-report pane shows the AGENT pill: {out}");
        assert!(out.contains("alpha") && out.contains("beta"));
        assert!(out.contains("hello from agent"));
        assert!(out.contains("tmux"), "beta is not in ptyd → tagged tmux");
    }

    #[test]
    fn board_groups_sessions_into_six_status_columns() {
        use ninox_core::types::SessionStatus as S;
        let mut w1 = ninox_ui_shared::cli::test_fixtures::session("w1", None, S::CiFailed);
        w1.pr_number = Some(142);
        let w2 = ninox_ui_shared::cli::test_fixtures::session("w2", None, S::Mergeable);
        let st = TuiState {
            rows: vec![
                Row { session: w1, is_orchestrator: false, group: None, machine_health: None , queue_position: None, prs: Vec::new()},
                Row { session: w2, is_orchestrator: false, group: None, machine_health: None , queue_position: None, prs: Vec::new()},
            ],
            view: View::Board,
            ..Default::default()
        };
        let out = render(&st, 160, 20);
        for label in ["Working (0)", "PR Open (0)", "CI Failed (1)", "Review (0)", "Mergeable (1)", "Done (0)"] {
            assert!(out.contains(label), "{label} missing:\n{out}");
        }
        assert!(out.contains("w1") && out.contains("#142"), "the CI-failed card shows its PR chip:\n{out}");
        assert!(out.contains("w2"), "{out}");
    }

    fn chip_fixture(view: View) -> TuiState {
        use ninox_core::session_prs::{PrState, SessionPr};
        let pr = |n, state| SessionPr {
            repo: "o/bellhop".into(), number: n, url: format!("https://github.com/o/bellhop/pull/{n}"), title: String::new(),
            state, draft: false, ci: ninox_core::pr_events::CiVerdict::Passed, passing: 1, failing: 0, pending: 0,
            changes_requested: false, approved: false, tracked: false,
        };
        let mut w1 = ninox_ui_shared::cli::test_fixtures::session("w1", None, ninox_core::types::SessionStatus::PrOpen);
        w1.pr_number = Some(1466);
        let mut row = Row::test_row("w1");
        row.session = w1;
        row.prs = vec![pr(1466, PrState::Open), pr(1473, PrState::Open), pr(900, PrState::Merged)];
        let mut st = TuiState { rows: vec![row], view, ..Default::default() };
        st.fleet.sync(&st.rows.clone(), &std::collections::HashMap::new());
        st
    }

    /// Writes the rendered frame to `$NINOX_SHOT_DIR/<name>.txt` when set.
    fn dump(name: &str, frame: &str) {
        if let Ok(dir) = std::env::var("NINOX_SHOT_DIR") {
            std::fs::write(std::path::Path::new(&dir).join(format!("{name}.txt")), frame).unwrap();
        }
    }

    fn click_at(st: &mut TuiState, x: u16, y: u16) -> crate::tui::state::Action {
        use crate::tui::state::handle_mouse;
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let ev = MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: x, row: y, modifiers: KeyModifiers::NONE };
        handle_mouse(st, ev)
    }

    #[test]
    fn board_card_shows_pr_chips_and_clicking_one_opens_the_pr_without_selecting_the_card() {
        let mut st = chip_fixture(View::Board);
        let out = render(&st, 160, 20);
        dump("tui-board", &out);
        assert!(out.contains("[bellhop#1466]"), "a Board column is narrow, so the lead chip stays and +N yields:\n{out}");
        st.layout = layout::compute(ratatui::layout::Rect::new(0, 0, 160, 20), &st);
        let (rect, target) = st.chip_hits.borrow().iter().find(|(_, t)| matches!(t, crate::tui::pr_chips::ChipTarget::Open(_))).cloned().expect("an open chip");
        assert_eq!(target, crate::tui::pr_chips::ChipTarget::Open("https://github.com/o/bellhop/pull/1466".into()));
        let before = (st.kanban.col, st.kanban.row, st.selected);
        let action = click_at(&mut st, rect.x + 1, rect.y);
        assert!(matches!(&action, crate::tui::state::Action::OpenUrl(u) if u == "https://github.com/o/bellhop/pull/1466"), "{action:?}");
        assert_eq!((st.kanban.col, st.kanban.row, st.selected), before, "the card underneath was not selected");
    }

    #[test]
    fn fleet_tree_chips_are_clickable_and_plus_n_opens_the_session() {
        let mut st = chip_fixture(View::Fleet);
        dump("tui-fleet", &render(&st, 160, 24));
        st.layout = layout::compute(ratatui::layout::Rect::new(0, 0, 160, 40), &st);
        let hits: Vec<_> = st.chip_hits.borrow().clone();
        assert!(hits.iter().any(|(_, t)| matches!(t, crate::tui::pr_chips::ChipTarget::Open(_))), "{hits:?}");
        let (rect, _) = hits.iter().find(|(_, t)| matches!(t, crate::tui::pr_chips::ChipTarget::Open(_))).unwrap().clone();
        assert!(matches!(click_at(&mut st, rect.x, rect.y), crate::tui::state::Action::OpenUrl(_)));
        let (more, _) = hits.iter().find(|(_, t)| matches!(t, crate::tui::pr_chips::ChipTarget::More(_))).unwrap().clone();
        let action = click_at(&mut st, more.x, more.y);
        assert!(!matches!(action, crate::tui::state::Action::OpenUrl(_)), "{action:?}");
        assert!(st.inspector, "+N opens the session with its inspector");
    }

    #[test]
    fn board_scrolls_the_selected_column_so_the_cursor_stays_visible() {
        use ninox_core::types::SessionStatus as S;
        let rows: Vec<Row> = (0..12)
            .map(|i| Row {
                session: ninox_ui_shared::cli::test_fixtures::session(&format!("w{i}"), None, S::Working),
                is_orchestrator: false,
                group: None,
                machine_health: None, queue_position: None, prs: Vec::new(),
            })
            .collect();
        let mut st = TuiState { rows, view: View::Board, ..Default::default() };
        st.kanban.row = 11;
        let out = render(&st, 160, 12);
        assert!(out.contains("w11"), "the selected card must be scrolled into view:\n{out}");
        assert!(!out.contains("w0"), "the top card should have scrolled off:\n{out}");
    }

    #[test]
    fn remotes_lists_configured_machines_with_health() {
        use crate::tui::remotes::RemoteRow;
        let st = TuiState {
            view: View::Remotes,
            remote_rows: vec![
                RemoteRow { id: "m1".into(), label: "Laptop".into(), ssh_target: "me@laptop".into(), remote_session: "default".into(), enabled: true, health: Some(ninox_core::types::MachineHealth::Connected) },
                RemoteRow { id: "m2".into(), label: "Desktop".into(), ssh_target: "me@desktop".into(), remote_session: "default".into(), enabled: false, health: None },
            ],
            ..Default::default()
        };
        let out = render(&st, 120, 20);
        assert!(out.contains("Laptop") && out.contains("me@laptop"), "{out}");
        assert!(out.contains("Desktop") && out.contains("me@desktop"), "{out}");
        assert!(out.contains("add remote") && out.contains("remove"), "populated list must hint both a and d: {out}");
    }

    #[test]
    fn remotes_shows_an_empty_state_with_no_configured_machines() {
        let st = TuiState { view: View::Remotes, ..Default::default() };
        let out = render(&st, 120, 20);
        assert!(out.contains("No remote machines configured"), "{out}");
        assert!(out.contains("add a remote machine") && out.contains("ninox machine add"), "{out}");
        assert!(out.contains("add remote"), "{out}");
        assert!(!out.contains("remove"), "an empty list has nothing to remove, so the footer must not hint d: {out}");
    }

    #[test]
    fn workspaces_panel_renders_empty_state_without_panicking_when_open() {
        let mut st = TuiState { view: View::Remotes, ..Default::default() };
        st.workspaces.open = true;
        let out = render(&st, 120, 20);
        assert!(out.contains("WORKSPACES"), "{out}");
        assert!(out.contains("No leftover workspaces"), "{out}");
    }

    #[test]
    fn workspaces_panel_lists_rows_when_populated() {
        use crate::tui::workspaces_panel::WorkspaceRowView;
        let mut st = TuiState { view: View::Remotes, ..Default::default() };
        st.workspaces.open = true;
        st.set_workspace_rows(vec![WorkspaceRowView {
            path: "/repo/.claude/worktrees/leftover-1".into(),
            session_id: "leftover-1".into(),
            status: "orphan(tracked)".into(),
            size: "1.2GB".into(),
            action: "would clean (worktree + branch)".into(),
            size_bytes: 1_200_000_000,
            prunable: true,
        }]);
        let out = render(&st, 120, 20);
        assert!(out.contains("leftover-1"), "{out}");
        assert!(out.contains("1.2GB"), "{out}");
        assert!(out.contains("prune (preview)") && out.contains("close"), "{out}");
    }

    #[test]
    fn workspaces_panel_preview_toggle_shows_the_summary_line() {
        use crate::tui::workspaces_panel::WorkspaceRowView;
        let mut st = TuiState { view: View::Remotes, ..Default::default() };
        st.workspaces.open = true;
        st.set_workspace_rows(vec![WorkspaceRowView {
            path: "/repo/.claude/worktrees/leftover-1".into(),
            session_id: "leftover-1".into(),
            status: "orphan(tracked)".into(),
            size: "1.2GB".into(),
            action: "would clean (worktree + branch)".into(),
            size_bytes: 1_200_000_000,
            prunable: true,
        }]);

        let without_preview = render(&st, 120, 20);
        assert!(!without_preview.contains("would prune"), "{without_preview}");

        st.workspaces.preview = true;
        // The panel pane is roughly half the terminal's content width, so
        // the fixed hint prefix can leave little room for the summary at a
        // realistic terminal size — check a short, truncation-safe prefix
        // rather than the full phrase (the exact-fit behavior itself is
        // `format_row_line`'s/`truncate`'s own unit tests' job).
        let with_preview = render(&st, 120, 20);
        assert!(with_preview.contains("would prune"), "{with_preview}");
    }

    #[test]
    fn workspaces_panel_is_absent_when_closed() {
        let st = TuiState { view: View::Remotes, ..Default::default() };
        let out = render(&st, 120, 20);
        assert!(!out.contains("WORKSPACES"), "{out}");
    }

    #[test]
    fn plans_tab_lists_plans_with_their_nodes() {
        use ninox_core::ghost_plan::PlanStatus;
        let mut st = TuiState { view: View::Plans, ..Default::default() };
        st.ghost_plans = vec![crate::tui::ghost_fixture("gp-1", &["alpha", "beta"], PlanStatus::Pending, 1)];
        let out = render(&st, 120, 20);
        assert!(out.contains("PENDING Q4 objectives") && out.contains("gp-1"), "{out}");
        assert!(out.contains("Orchestrator alpha") && out.contains("a accept"), "{out}");
    }

    #[test]
    fn fleet_draws_pending_ghost_plans_as_dashed_boxes() {
        use ninox_core::ghost_plan::PlanStatus;
        let mut st = st_with(&["a"]);
        st.view = View::Fleet;
        st.fleet.sync(&st.rows, &std::collections::HashMap::new());
        st.ghost_plans = vec![crate::tui::ghost_fixture("gp-1", &["alpha", "beta", "gamma"], PlanStatus::Pending, 1)];
        let out = render(&st, 120, 40);
        assert!(out.contains("proposed · new orchestrators"), "{out}");
        assert!(out.contains("┆") && out.contains("Orchestrator gamma"), "{out}");
    }

    #[test]
    fn an_empty_fleet_still_shows_pending_ghost_plans() {
        use ninox_core::ghost_plan::PlanStatus;
        let mut st = TuiState { view: View::Fleet, ..Default::default() };
        st.ghost_plans = vec![crate::tui::ghost_fixture("gp-1", &["alpha"], PlanStatus::Pending, 1)];
        let out = render(&st, 120, 30);
        assert!(out.contains("proposed · new orchestrators") && !out.contains("No sessions yet"), "{out}");
    }

    #[test]
    fn reply_turn_shows_the_ghost_cards_it_proposed() {
        use ninox_core::ghost_plan::PlanStatus;
        let mut st = reply_state(&["a"]);
        add_turn(&mut st, "spawn three orchestrators", "Proposed gp-1.");
        st.ghost_plans = vec![
            crate::tui::ghost_fixture("gp-1", &["alpha", "beta", "gamma"], PlanStatus::Pending, st.now_ms + 5),
            crate::tui::ghost_fixture("gp-old", &["stale"], PlanStatus::Pending, st.now_ms - 5),
        ];
        let text: String = reply_conversation(&st, 100).0.iter().map(|l| l.to_string() + "\n").collect();
        assert!(text.contains("GHOST ORCHESTRATOR alpha") && text.contains("Accept plan · spawn 3"), "{text}");
        assert!(text.contains("Nothing is spawned until you accept."), "{text}");
        assert!(!text.contains("stale"), "a plan from before the turn isn't its card: {text}");
        for line in reply_conversation(&st, 40).0 {
            assert!(line.width() <= 40, "{line:?}");
        }
    }

    #[test]
    fn plans_tab_shows_empty_state_with_no_pending_plans() {
        let st = TuiState { view: View::Plans, ..Default::default() };
        let out = render(&st, 120, 20);
        assert!(out.contains("No plans pending"), "{out}");
    }

    fn settings_state(section: ninox_ui_shared::settings_form::SettingsSection) -> TuiState {
        let mut st = st_with(&["alpha"]);
        st.settings.reload(Ok(Default::default()));
        st.settings.select_section(section);
        st.modal = Some(Modal::Settings);
        st
    }

    #[test]
    fn settings_page_shows_the_section_bar_and_the_selected_sections_fields() {
        use ninox_ui_shared::settings_form::SettingsSection;
        let _iso = ninox_core::test_isolation::isolate();
        let out = render(&settings_state(SettingsSection::Runtime), 140, 50);
        for t in ["GENERAL", "SESSIONS", "SYSTEM", "Appearance", "Agents", "Harnesses", "Messaging", "Session runtime", "Session limits", "Terminal", "Fleet", "Engine", "Version"] {
            assert!(out.contains(t), "{t} missing from the bar:\n{out}");
        }
        assert!(out.contains("Runtime for new sessions") && out.contains("tmux"), "{out}");
        assert!(!out.contains("Colours"), "another section's fields are not listed:\n{out}");
        let mut st = settings_state(SettingsSection::Runtime);
        st.settings.in_fields = true;
        assert!(render(&st, 140, 50).contains("‹ tmux ›"), "the selected choice shows its arrows");
    }

    #[test]
    fn settings_page_at_80x24_fits_the_bar_fields_and_hint() {
        use ninox_ui_shared::settings_form::SettingsSection;
        let _iso = ninox_core::test_isolation::isolate();
        let out = render(&settings_state(SettingsSection::Agents), 80, 24);
        println!("{out}");
        for t in ["Appearance", "Version", "Orchestrator harness", "Worker model", "↑↓ section", "✕ close", "$EDITOR"] {
            assert!(out.contains(t), "{t} missing at 80x24:\n{out}");
        }
    }

    #[test]
    fn settings_page_at_a_wide_size_puts_the_help_beside_the_fields() {
        use ninox_ui_shared::settings_form::SettingsSection;
        let _iso = ninox_core::test_isolation::isolate();
        let mut st = settings_state(SettingsSection::Messaging);
        st.settings.in_fields = true;
        let out = render(&st, 200, 40);
        println!("{out}");
        assert!(out.contains("Send mechanism") && out.contains("Consolidated PR watching"), "{out}");
    }

    #[test]
    fn settings_page_shows_a_refused_change_under_its_field_and_the_load_error() {
        use ninox_ui_shared::settings_form::SettingsSection;
        let _iso = ninox_core::test_isolation::isolate();
        let mut st = settings_state(SettingsSection::Engine);
        st.settings.in_fields = true;
        st.settings.error = Some("port must be 1-65535".into());
        assert!(render(&st, 80, 24).contains("✗ port must be 1-65535"));
        st.settings.reload(Err("expected `=`".into()));
        assert!(render(&st, 80, 24).contains("✗ expected `=`"));
    }

    #[test]
    fn header_shows_the_settings_entry_at_80_columns() {
        let _iso = ninox_core::test_isolation::isolate();
        let st = st_with(&["alpha"]);
        let out = render(&st, 80, 24);
        let first = out.lines().next().unwrap_or_default();
        println!("{first}");
        assert!(first.contains("⚙ s"), "{first}");
        assert!(!first.contains("pt"), "no clipped status label: {first}");
        for tab in ["Fleet", "Board", "Plans", "Brain", "Types", "Remotes"] {
            assert!(first.contains(tab), "{tab} missing: {first}");
        }
    }

    #[test]
    fn board_renders_an_acp_sessions_transcript_instead_of_a_pty_screen() {
        use ninox_core::runtime::{ToolCallStatus, TranscriptEntry};
        let mut st = st_with(&["alpha"]);
        st.focus = Focus::Pane;
        // An ACP session has no pty screen at all — if the board fell back
        // to the normal pty branch it would show "tmux"/"connecting…", not
        // this content, so a live PaneView is deliberately absent here.
        st.transcripts.insert(
            "alpha".into(),
            vec![
                TranscriptEntry::UserMessage("please run the tests".to_string()),
                TranscriptEntry::TurnStarted,
                TranscriptEntry::ToolCall {
                    id:     "call_1".to_string(),
                    name:   "bash".to_string(),
                    status: ToolCallStatus::Completed,
                    detail: "3 passed".to_string(),
                },
                TranscriptEntry::ToolCall {
                    id:     "call_2".to_string(),
                    name:   "lint".to_string(),
                    status: ToolCallStatus::Failed,
                    detail: "2 errors".to_string(),
                },
                TranscriptEntry::AgentText("all tests pass".to_string()),
                TranscriptEntry::TurnEnded,
            ],
        );
        let out = render(&st, 120, 20);
        assert!(out.contains("ACP-native"), "no-raw-terminal indicator missing: {out}");
        assert!(out.contains("▶ user") && out.contains("please run the tests"), "{out}");
        assert!(out.contains("turn started") && out.contains("turn ended"), "{out}");
        assert!(out.contains("bash") && out.contains("completed") && out.contains("3 passed"), "{out}");
        assert!(out.contains("lint") && out.contains("failed") && out.contains("2 errors"), "{out}");
        assert!(out.contains("· agent") && out.contains("all tests pass"), "{out}");
        // Entries render as distinct, styled lines — not a single Debug dump.
        assert!(!out.contains("TranscriptEntry"), "entries must not render as raw Debug output: {out}");
    }

    #[test]
    fn a_transcript_longer_than_the_pane_scrolls_to_show_the_newest_entry() {
        use ninox_core::runtime::TranscriptEntry;
        let mut st = st_with(&["alpha"]);
        st.focus = Focus::Pane;
        let entries = (0..50).map(|i| TranscriptEntry::AgentText(format!("turn {i}"))).collect();
        st.transcripts.insert("alpha".into(), entries);
        // A short terminal: far fewer rows than entries, so the naive
        // top-of-buffer render would freeze on "turn 0" forever.
        let out = render(&st, 120, 15);
        assert!(out.contains("turn 49"), "the newest entry must be visible: {out}");
        assert!(!out.contains("turn 0 "), "the oldest entry must have scrolled out of view: {out}");
    }

    fn live_pane(st: &mut TuiState, id: &str) {
        st.ptyd = PtydState::Up;
        st.panes.insert(
            id.into(),
            ninox_ptyd::PaneInfo {
                pane: id.into(),
                pid: 7,
                cols: 80,
                rows: 24,
                alive: true,
                exit_code: None,
                created_ms: 0,
                last_output_ms: 0,
                title: None,
                cwd: "/".into(),
                seq: 1,
                history_size: 0,
            },
        );
        st.views.insert(id.into(), PaneView { live: Some(snap(&["hello from agent"], 0, 40)), ..Default::default() });
    }

    #[test]
    fn a_focused_pane_offers_kill_and_zoom_buttons_and_prefix_free_hints() {
        let mut st = st_with(&["alpha", "beta"]);
        live_pane(&mut st, "alpha");
        st.focus = Focus::Pane;
        let out = render(&st, 120, 20);
        let title = out.lines().find(|l| l.contains("◀ fleet")).unwrap();
        let (back, kill, zoom) = (title.find("◀ fleet").unwrap(), title.find("✕ kill").unwrap(), title.find("⤢ zoom").unwrap());
        assert!(back < kill && kill < zoom, "{title}");
        let footer = out.lines().last().unwrap();
        assert!(footer.contains("Ctrl+] back to the fleet") && footer.contains("Ctrl+] then x kill"), "{footer}");
        assert!(!footer.contains(&prefix_label(st.prefix)), "no prefix-only hint: {footer}");
        assert!(title.contains("$1.50 ✕ │"), "the selected sidebar row ends in its ✕: {title}");
    }

    #[test]
    fn help_puts_c_bracket_first_and_groups_by_where_you_are() {
        let mut st = st_with(&["alpha"]);
        st.modal = Some(Modal::Help);
        let out = render(&st, 120, 50);
        let at = |s: &str| out.find(s).unwrap_or_else(|| panic!("{s} missing: {out}"));
        assert!(at("Ctrl+] always returns") < at("Inside an agent pane"));
        assert!(at("Inside an agent pane") < at("From the fleet list (bare keys)"));
        assert!(at("back to the fleet list") < at("From the fleet list"));
    }

    #[test]
    fn help_matches_the_six_tab_set_not_the_old_seven() {
        // Final review Important #6: HELP still described the pre-v1
        // Overview/PRs/Settings-tab/Graph-tab world.
        let st = st_with(&["alpha"]);
        // The whole list, not one screen of it (help scrolls).
        let out: String = help_lines(&st).iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>() + "\n").collect();
        for gone in ["PRs tab", "Graph tab", "overview grid", "settings · graph", "1-7"] {
            assert!(!out.contains(gone), "stale reference to the old shell: {gone}\n{out}");
        }
        for present in ["Board tab", "Settings (press s", "1-6", "hide/show the tree"] {
            assert!(out.contains(present), "{present} missing:\n{out}");
        }
    }

    #[test]
    fn header_shows_clickable_tabs_and_a_tmux_session_renders_through_its_viewer() {
        let mut st = st_with(&["legacy"]);
        st.ptyd = PtydState::Up;
        let viewer = ninox_core::runtime::viewer_pane_id(std::process::id(), "legacy");
        st.viewers.insert(
            "legacy".into(),
            ninox_ptyd::PaneInfo {
                pane: viewer.clone(),
                pid: 9,
                cols: 60,
                rows: 10,
                alive: true,
                exit_code: None,
                created_ms: 0,
                last_output_ms: 0,
                title: None,
                cwd: "/".into(),
                seq: 1, history_size: 0,
            },
        );
        st.views.insert(viewer, PaneView { live: Some(snap(&["inside tmux"], 0, 40)), ..Default::default() });
        st.focus = crate::tui::state::Focus::Pane;
        let out = render(&st, 120, 20);
        let header = out.lines().next().unwrap();
        for tab in ["1 Fleet", "2 Board", "3 Plans", "4 Brain", "5 Types", "6 Remotes"] {
            assert!(header.contains(tab), "{header}");
        }
        assert!(out.contains("inside tmux"), "{out}");
        assert!(out.contains("◀ fleet"), "{out}");
        assert!(out.contains("Ctrl+]"), "the footer names the escape chord: {out}");
    }

    #[test]
    fn restored_checkpoint_is_marked() {
        let mut st = st_with(&["a"]);
        st.focus = Focus::Pane;
        st.panes.insert(
            "a".into(),
            ninox_ptyd::PaneInfo {
                pane: "a".into(),
                pid: 1,
                cols: 80,
                rows: 24,
                alive: true,
                exit_code: None,
                created_ms: 0,
                last_output_ms: 0,
                title: None,
                cwd: "/".into(),
                seq: 0, history_size: 0,
            },
        );
        st.views.insert(
            "a".into(),
            PaneView {
                checkpoint: Some(ninox_ptyd::checkpoint::Checkpoint { pane: "a".into(), saved_ms: 0, screen: snap(&["before reboot"], 0, 40) }),
                ..Default::default()
            },
        );
        let out = render(&st, 100, 12);
        assert!(out.contains("restored — waiting for agent"), "{out}");
        assert!(out.contains("before reboot"));
    }

    #[test]
    fn modals_render() {
        let mut st = st_with(&["a", "b", "c"]);
        st.modal = Some(Modal::Help);
        assert!(render(&st, 100, 30).contains("detach"));
        st.modal = Some(Modal::Goto(Default::default()));
        assert!(render(&st, 100, 30).contains("go to"));
        st.modal = Some(Modal::Restore(crate::tui::backend::RestoreSummary { workers: 3, orchestrators: 1, interrupted_at: None, flagged_at: None }));
        assert!(render(&st, 100, 30).contains("Restore fleet (3 workers, 1 orchestrators)?"));
    }

    #[test]
    fn add_remote_modal_renders_fields_placeholder_and_error() {
        use crate::tui::state::AddRemoteModal;
        let mut st = st_with(&["a"]);
        st.modal = Some(Modal::AddRemote(AddRemoteModal { host: "box.example.com".into(), ..Default::default() }));
        let out = render(&st, 100, 30);
        assert!(out.contains("add a remote machine") && out.contains("box.example.com") && out.contains("box"), "label placeholder (the host portion) must show when label is blank: {out}");

        st.modal = Some(Modal::AddRemote(AddRemoteModal { error: Some("host is required".into()), ..Default::default() }));
        assert!(render(&st, 100, 30).contains("host is required"));
    }

    #[test]
    fn spawn_modal_renders_fields_cost_line_hint_and_error() {
        let mut st = st_with(&["a"]);
        st.modal = Some(Modal::Spawn);
        let out = render(&st, 200, 55);
        for want in ["spawn a session", "Kind", "Orchestrator", "Name", "Agent", "claude-code", "Model", "Brains this tree reads", "[x] default", "Writes to", "Initial prompt", "Model: claude-opus-5-5 (claude-code default)", "est. $2", "tab/shift-tab field"] {
            assert!(out.contains(want), "missing {want:?} in:\n{out}");
        }
        assert!(!out.contains("Workspace") && !out.contains("Worker type"), "Workspace/Worker type must stay hidden for the Orchestrator kind:\n{out}");

        st.spawn_form.error = Some("name is required".to_string());
        let out = render(&st, 200, 55);
        assert!(out.contains("✗ name is required"), "{out}");
    }

    fn multi_brain_form() -> crate::tui::spawn::SpawnForm {
        let mut cfg = ninox_core::config::AppConfig::default();
        let cat = |name: &str, enabled: bool, access: &str| ninox_core::config::CatalogueRef {
            name: name.into(), path: format!("/scratch/{name}").into(), remote: None, endpoint: None,
            region: None, cache_ttl_secs: None, enabled, cadence: "manual".into(), access: access.into(),
        };
        cfg.brain.catalogues = vec![cat("alpha", true, "read_write"), cat("beta", true, "read_only"), cat("old", false, "read_write")];
        crate::tui::spawn::build(&cfg)
    }

    #[test]
    fn spawn_modal_shows_each_brain_with_its_selection_and_read_only_badge() {
        let mut st = st_with(&["a"]);
        st.modal = Some(Modal::Spawn);
        st.spawn_form = multi_brain_form();
        st.spawn_form.focus = st.spawn_form.fields.iter().position(|f| f.id == crate::tui::spawn::FieldId::Brains).unwrap();
        let out = render(&st, 120, 30);
        for want in ["Brains this tree reads", "[x] default", "[x] alpha", "[x] beta ro", "[ ] old", "Writes to", "space toggle brain"] {
            assert!(out.contains(want), "missing {want:?}:\n{out}");
        }
    }

    #[test]
    fn inspector_info_shows_the_session_brains() {
        let mut st = st_with(&["a"]);
        st.brain_lines.insert("a".into(), "Brains: reads alpha, beta · writes alpha".into());
        let lines = crate::tui::inspector::info_lines(&st, &st.rows[0], 80);
        let text: String = lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>() + "\n").collect();
        assert!(text.contains("reads alpha, beta · writes alpha"), "{text}");
    }

    /// Pins the exact terminal size a kitty capture settled at for this
    /// modal (200x55 requested; the real window reported 163x50 — see the
    /// PR body) with a non-empty Initial prompt, so a future regression in
    /// the field list's height math or the Prompt field's line handling at
    /// this specific size is caught here rather than only visually.
    #[test]
    fn spawn_modal_renders_cost_line_and_hint_at_the_captured_kitty_size_with_a_prompt() {
        let mut st = st_with(&["a"]);
        st.modal = Some(Modal::Spawn);
        use crate::tui::spawn::FieldId;
        st.spawn_form.fields.iter_mut().find(|f| f.id == FieldId::Name).unwrap().value = "theme-tokens".to_string();
        st.spawn_form.fields.iter_mut().find(|f| f.id == FieldId::Prompt).unwrap().value = "fix the thing".to_string();
        let out = render(&st, 163, 50);
        for want in ["spawn a session", "theme-tokens", "fix the thing", "Model: claude-opus-5-5 (claude-code default)", "est. $2", "tab/shift-tab field"] {
            assert!(out.contains(want), "missing {want:?} at 163x50 with a prompt:\n{out}");
        }
    }

    fn delete_plan() -> ninox_core::events::DeletePlan {
        use ninox_core::events::{DeletePlan, DeleteTarget, WorktreeState};
        let target = |id: &str, orch: bool, depth: usize, tree_index: usize, dirty: bool| DeleteTarget {
            id: id.into(), name: format!("{id}-name"), is_orchestrator: orch, running: true, resumable: false, depth, tree_index,
            worktree: (!orch).then(|| WorktreeState { path: format!("/r/.claude/worktrees/{id}"), uncommitted: dirty, stash: false, unpushed: false }),
        };
        DeletePlan {
            root: "o".into(),
            targets: vec![target("dirty", false, 2, 2, true), target("sub", true, 1, 1, false), target("clean", false, 1, 3, false), target("o", true, 0, 0, false)],
        }
    }

    #[test]
    fn the_delete_confirm_shows_the_tree_the_kills_and_the_override() {
        let mut st = st_with(&["a"]);
        st.modal = Some(Modal::Delete(DeleteConfirm { root: "o".into(), plan: Some(Ok(delete_plan())), force: false, expanded: true }));
        let screen = render(&st, 140, 40);
        for want in [
            "Delete orchestrator o-name and 3 sessions under it",
            "This kills 4 running sessions.",
            "▾ 3 sessions under it",
            "    dirty-name (worker)",
            "worktree kept: uncommitted changes",
            "  clean-name (worker)",
            "worktree: clean and pushed",
            "[ ] f  Also delete 1 worktree with uncommitted or unpushed work: dirty-name",
            "[ y Delete ]",
        ] {
            assert!(screen.contains(want), "missing {want:?} in:\n{screen}");
        }
        let sub = screen.find("  sub-name (orchestrator)").expect("sub listed");
        assert!(sub < screen.find("dirty-name (worker)").unwrap(), "a parent reads before its children");
    }

    #[test]
    fn wrap_words_breaks_at_spaces_and_splits_only_an_overlong_word() {
        assert_eq!(wrap_words("aa bb cc", 5), ["aa bb", "cc"]);
        assert_eq!(wrap_words("aaaaaaaaaa bb", 4), ["aaaa", "aaaa", "aa", "bb"]);
        assert_eq!(wrap_words("", 4), [""]);
    }

    #[test]
    fn a_big_tree_on_a_small_terminal_still_shows_the_override_and_the_buttons() {
        use ninox_core::events::{DeletePlan, DeleteTarget, WorktreeState};
        let mut targets: Vec<DeleteTarget> = (1..=30)
            .map(|i| DeleteTarget {
                id: format!("w{i}"), name: format!("worker-with-a-long-name-{i}"), is_orchestrator: false, running: true,
                resumable: false, depth: 1, tree_index: i,
                worktree: Some(WorktreeState { path: String::new(), uncommitted: true, stash: false, unpushed: false }),
            })
            .collect();
        targets.push(DeleteTarget { id: "o".into(), name: "o".into(), is_orchestrator: true, running: true, resumable: false, depth: 0, tree_index: 0, worktree: None });
        let mut st = st_with(&["a"]);
        st.modal = Some(Modal::Delete(DeleteConfirm { root: "o".into(), plan: Some(Ok(DeletePlan { root: "o".into(), targets })), force: false, expanded: true }));
        let screen = render(&st, 44, 24);
        assert!(screen.contains("[ y Delete ]"), "{screen}");
        assert!(screen.contains("[ ] f  Also delete 30 worktrees"), "{screen}");
        assert!(screen.contains("more"), "the override names what fits and counts the rest: {screen}");
        assert!(screen.contains("more lines (e collapses the tree)"), "{screen}");
    }

    #[test]
    fn a_worktree_name_wider_than_the_modal_shows_every_character() {
        use ninox_core::events::{DeletePlan, DeleteTarget, WorktreeState};
        let name: String = (0..12).map(|i| format!("seg{i}-abcd")).collect::<Vec<_>>().join("") + "-END";
        let target = DeleteTarget {
            id: "w".into(), name: name.clone(), is_orchestrator: false, running: false, resumable: false, depth: 0, tree_index: 0,
            worktree: Some(WorktreeState { path: String::new(), uncommitted: true, stash: false, unpushed: false }),
        };
        let mut st = st_with(&["a"]);
        st.modal = Some(Modal::Delete(DeleteConfirm { root: "w".into(), plan: Some(Ok(DeletePlan { root: "w".into(), targets: vec![target] })), force: false, expanded: true }));
        let screen = render(&st, 80, 30);
        let joined: String = screen.lines().map(|l| l.trim().trim_matches('│').trim()).collect();
        assert!(joined.contains(&format!("work:{name}")), "the override's name lost characters at the right edge: {screen}");
    }

    /// No glyph painted on a `card`-coloured background (every modal's own
    /// surface — see `modal_block`) may share that exact colour, in any
    /// palette, in any of the seven `Modal` variants.
    #[test]
    fn no_modal_paints_a_glyph_the_same_colour_as_its_own_card_background() {
        let _iso = ninox_core::test_isolation::isolate();
        use crate::tui::backend::RestoreSummary;
        use crate::tui::state::{AddRemoteModal, Goto, Pending};

        for palette in [Palette::terminal(), Palette::of(ninox_core::ThemeVariant::Dark, true)] {
            let modals: Vec<(&str, Modal)> = vec![
                ("Confirm", Modal::Confirm(Pending::Kill("w-ci".into()))),
                ("Spawn", Modal::Spawn),
                ("AddRemote", Modal::AddRemote(AddRemoteModal { host: "box.example.com".into(), ..Default::default() })),
                ("Goto", Modal::Goto(Goto::default())),
                ("Help", Modal::Help),
                ("Notifications", Modal::Notifications(0)),
                ("Message", Modal::Message(crate::tui::state::MessageComposer { id: "a".into(), name: "a".into(), draft: "hi".into() })),
                ("Restore", Modal::Restore(RestoreSummary { workers: 3, orchestrators: 1, interrupted_at: None, flagged_at: None })),
                ("Settings", Modal::Settings),
                ("Delete", Modal::Delete(DeleteConfirm { root: "o".into(), plan: Some(Ok(delete_plan())), force: false, expanded: true })),
            ];
            for (name, modal) in modals {
                let mut st = st_with(&["a"]);
                st.palette = palette;
                if matches!(modal, Modal::Settings) {
                    st.settings.reload(Ok(ninox_core::config::AppConfig::default()));
                }
                for (id, kind) in [("n1", ninox_core::types::NotificationKind::WorkerDone), ("n2", ninox_core::types::NotificationKind::CiFailure)] {
                    st.notifications.push_back(ninox_core::types::Notification {
                        id: id.into(), kind, title: "t".into(), body: "b".into(), session_id: None, created_at: 0,
                    });
                }
                st.modal = Some(modal);
                let buf = render_buffer(&st, 163, 50);
                let mut checked_any = false;
                for y in 0..50u16 {
                    for x in 0..163u16 {
                        let cell = &buf[(x, y)];
                        if cell.symbol().trim().is_empty() {
                            continue;
                        }
                        // Only cells inside a card-coloured block are at
                        // risk — everywhere else uses the page background.
                        if cell.bg != st.palette.card {
                            continue;
                        }
                        checked_any = true;
                        assert_ne!(cell.fg, cell.bg, "{name} modal: invisible glyph {:?} at ({x},{y}) with palette {:?}", cell.symbol(), st.palette);
                    }
                }
                assert!(checked_any, "{name} modal never painted its own card background — test fixture is stale");
            }
        }
    }

    /// No glyph painted on a `highlight`-coloured background (every
    /// selected row/card — the Fleet island, the sidebar, Remotes, Worker
    /// Types, Settings, Brain's list) may share that exact colour, in any
    /// palette. Companion to the `card`-background test above: `highlight`
    /// is `card`'s sibling token for selection state, not modal fills, and
    /// carries the same invisible-text risk whenever something still
    /// styled `faint` lands on it.
    #[test]
    fn no_selected_row_or_card_paints_a_glyph_the_same_colour_as_its_own_highlight_background() {
        let _iso = ninox_core::test_isolation::isolate();
        use ninox_core::types::SessionStatus as S;

        fn assert_no_invisible_glyph(buf: &ratatui::buffer::Buffer, w: u16, h: u16, bg: Color, label: &str) {
            let mut checked_any = false;
            for y in 0..h {
                for x in 0..w {
                    let cell = &buf[(x, y)];
                    if cell.symbol().trim().is_empty() {
                        continue;
                    }
                    if cell.bg != bg {
                        continue;
                    }
                    checked_any = true;
                    assert_ne!(cell.fg, cell.bg, "{label}: invisible glyph {:?} at ({x},{y})", cell.symbol());
                }
            }
            assert!(checked_any, "{label} never painted a highlight background — test fixture is stale");
        }

        for palette in [Palette::terminal(), Palette::of(ninox_core::ThemeVariant::Dark, true)] {
            // Fleet sidebar: a terminal-status (Done/Terminated) selected
            // session exercises the dimmed name; an orchestrator row
            // exercises the fold chevron.
            let mut st = fleet();
            st.palette = palette;
            st.selected = st.rows.iter().position(|r| r.id() == "w-ended").unwrap();
            let buf = render_buffer(&st, 160, 30);
            assert_no_invisible_glyph(&buf, 160, 30, palette.highlight, "sidebar selected terminal row");

            // Fleet island: a PR-open orchestrator's status dot is `p.faint`
            // via `member_status_color`'s `WaitingOnPr` mapping.
            let row = Row {
                session: ninox_core::types::Session { name: "orch2".into(), ..ninox_ui_shared::cli::test_fixtures::session("orch2", Some("orch2"), S::PrOpen) },
                is_orchestrator: true,
                group: Some("orch2".into()),
                machine_health: None, queue_position: None, prs: Vec::new(),
            };
            let mut st = TuiState { rows: vec![row], palette, ..Default::default() };
            st.fleet.sync(&st.rows.clone(), &std::collections::HashMap::new());
            st.fleet.nav.open_node("orch2", &st.fleet.islands);
            let buf = render_buffer(&st, 160, 30);
            assert_no_invisible_glyph(&buf, 160, 30, palette.highlight, "fleet island selected card");

            // Remotes: a row with unknown health dims its ssh/session text.
            use crate::tui::remotes::RemoteRow;
            let mut st = TuiState {
                view: View::Remotes,
                palette,
                remote_rows: vec![RemoteRow { id: "m1".into(), label: "Desktop".into(), ssh_target: "me@desktop".into(), remote_session: "default".into(), enabled: true, health: None }],
                ..Default::default()
            };
            st.remotes.selected = 0;
            let buf = render_buffer(&st, 120, 20);
            assert_no_invisible_glyph(&buf, 120, 20, palette.highlight, "remotes selected row with unknown health");

            // Worker Types: the selected field's Toggle-off text.
            let mut st = TuiState { view: View::WorkerTypes, palette, ..Default::default() };
            st.worker_types.reload(Ok(Default::default()));
            st.worker_types.on_list = false;
            let off_field = st
                .worker_types
                .fields
                .iter()
                .position(|f| matches!(f.kind, crate::tui::settings::Kind::Toggle) && f.value == "off")
                .expect("a built-in worker type has an off toggle");
            st.worker_types.field_selected = off_field;
            let buf = render_buffer(&st, 160, 30);
            assert_no_invisible_glyph(&buf, 160, 30, palette.highlight, "worker types selected toggle-off field");

            // Settings (itself a modal): the selected field's Toggle-off
            // text — `highlight` can appear nested inside a `card`-filled
            // modal, so this also guards that combination.
            let mut st = TuiState { palette, ..Default::default() };
            st.settings.reload(Ok(ninox_core::config::AppConfig::default()));
            let off_field = st
                .settings
                .fields
                .iter()
                .position(|f| matches!(f.kind, crate::tui::settings::Kind::Toggle) && f.value == "off")
                .expect("default config has an off toggle");
            st.settings.selected = off_field;
            st.modal = Some(Modal::Settings);
            let buf = render_buffer(&st, 163, 50);
            assert_no_invisible_glyph(&buf, 163, 50, palette.highlight, "settings selected toggle-off field");

            // Brain: the selected group header (untagged).
            let mut st = TuiState { view: View::Brain, palette, ..Default::default() };
            st.brain = crate::tui::brain::tests::sample();
            st.brain.move_cursor(7); // "▾ untagged (1)"
            let buf = render_buffer(&st, 100, 20);
            assert_no_invisible_glyph(&buf, 100, 20, palette.highlight, "brain selected untagged group header");

            // Brain: a search hit's matched-line prefix (`{n}:`), the one
            // span `sample()`'s bodyless entries can't otherwise exercise.
            let mut st = TuiState { view: View::Brain, palette, ..Default::default() };
            st.brain = crate::tui::brain::tests::sample();
            st.brain.entries[2].body = "first line\nthe Unix socket path is fixed\n".into();
            st.brain.query = "unix sock".into();
            assert_eq!(st.brain.search().first().and_then(|h| h.line.as_ref()).map(|(n, _)| *n), Some(2), "fixture must exercise the matched-line branch");
            let buf = render_buffer(&st, 100, 20);
            assert_no_invisible_glyph(&buf, 100, 20, palette.highlight, "brain selected search hit with matched line");
        }
    }

    #[test]
    fn spawn_modal_standalone_kind_shows_workspace_and_worker_type() {
        let mut st = st_with(&["a"]);
        st.modal = Some(Modal::Spawn);
        st.spawn_form.fields[0].value = "Standalone".to_string();
        let out = render(&st, 200, 55);
        assert!(out.contains("Workspace") && out.contains("Worker type") && out.contains("Implementor"), "{out}");
    }

    #[test]
    fn remove_machine_confirm_names_the_machine() {
        use crate::tui::remotes::RemoteRow;
        let mut st = TuiState {
            remote_rows: vec![RemoteRow { id: "m1".into(), label: "Laptop".into(), ssh_target: "me@laptop".into(), remote_session: "default".into(), enabled: true, health: None }],
            ..Default::default()
        };
        st.modal = Some(Modal::Confirm(Pending::RemoveMachine("m1".into())));
        let out = render(&st, 100, 30);
        assert!(out.contains("Remove machine Laptop?"), "{out}");
    }

    #[test]
    fn empty_fleet_and_small_terminal_do_not_panic() {
        let st = TuiState::default();
        let out = render(&st, 100, 20);
        assert!(out.contains("No sessions yet") && out.contains("to spawn") && out.contains("to ask ninox"), "{out}");
        for (w, h) in [(1, 1), (10, 3), (30, 5)] {
            render(&st_with(&["x"]), w, h);
        }
    }

    /// A brand-new install (zero sessions) must still be able to see and use
    /// the prompt — not just `n` — to get its first orchestrator going.
    #[test]
    fn fleet_prompt_box_is_present_with_zero_sessions() {
        let st = TuiState::default();
        assert!(st.rows.is_empty());
        let out = render(&st, 120, 30);
        assert!(out.contains("❯ ninox ▾"), "the prompt must render even with no sessions: {out}");
        assert!(out.contains("/ focus · ⏎ send"), "the prompt box's hint must render even with no sessions: {out}");
    }

    /// An orchestrator with three workers (one blocked, one with a failing
    /// PR, one ended), a standalone session and an interrupted one.
    fn fleet() -> TuiState {
        use ninox_ui_shared::cli::test_fixtures::session;
        use ninox_core::types::{GateStatus, SessionStatus as S};
        let row = |id: &str, orch: Option<&str>, status: S| Row {
            session: ninox_core::types::Session { name: format!("{id} with a rather long name"), ..session(id, orch, status) },
            is_orchestrator: orch == Some(id),
            group: orch.map(str::to_string),
            machine_health: None, queue_position: None, prs: Vec::new(),
        };
        let mut rows = vec![
            row("orch", Some("orch"), S::Working),
            row("w-blocked", Some("orch"), S::Working),
            row("w-ci", Some("orch"), S::CiFailed),
            row("w-ended", Some("orch"), S::Terminated),
            row("solo", None, S::Working),
            row("lost", None, S::Interrupted),
        ];
        rows[1].session.activity = ActivityState::Blocked;
        rows[2].session.pr_number = Some(142);
        rows[2].session.gate_status = Some(GateStatus { ci: GateCheck::Failing, review: GateCheck::Unknown, mergeable: GateCheck::Unknown, since: 0 });
        rows[4].session.cost_usd = 1.25;
        let mut st = TuiState { rows, now_ms: 600_000, ..Default::default() };
        st.msg_counts.insert("solo".into(), 2);
        st.msg_seen.insert("solo".into(), 0);
        st
    }

    #[test]
    fn needs_you_pins_what_wants_attention_above_the_tree() {
        let mut st = fleet();
        st.focus = Focus::Pane;
        let out = render(&st, 120, 30);
        let lines: Vec<&str> = out.lines().collect();
        let at = |needle: &str| lines.iter().position(|l| l.contains(needle)).unwrap_or_else(|| panic!("{needle} missing:\n{out}"));
        let needs = at("NEEDS YOU");
        assert!(needs < at("STANDALONE"));
        for reason in ["blocked", "CI failed"] {
            assert!(at(reason) > needs, "{reason}");
        }
        // "lost" (interrupted — terminal) and "solo" (an unread message,
        // no other signal) are not pinned: `MemberStatus::NeedsYou` is
        // blocked activity or CI failure only, matching `classify_member`.
        assert_eq!(st.needs_rows().len(), 2, "only w-blocked and w-ci are pinned");
        assert!(out.contains("#142 ✗"), "PR and CI glyph on the row: {out}");
        assert!(out.contains("ended"), "an ended worker stays visible until reaped: {out}");
        assert!(out.contains("◉1"), "the group header rolls its workers up: {out}");
        assert!(out.contains("✗ w-ci"), "a failing PR is the row's glyph when the agent reports nothing: {out}");
    }

    #[test]
    fn a_folded_group_hides_its_workers_but_keeps_its_header() {
        let mut st = fleet();
        st.focus = Focus::Pane;
        st.collapsed.insert("orch".into());
        let out = render(&st, 120, 30);
        assert!(out.contains("▸ ◉ orch"), "folded header shows its most urgent member: {out}");
        let tree = out.split("STANDALONE").next().unwrap();
        assert!(!tree.contains("w-ended"), "{out}");
    }

    #[test]
    fn chrome_is_painted_in_the_configured_field_notes_variant() {
        let mut st = fleet();
        st.palette = Palette::of(ninox_core::ThemeVariant::Light, true);
        let backend = ratatui::backend::TestBackend::new(100, 20);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let lay = layout::compute(Rect::new(0, 0, 100, 20), &st);
        terminal.draw(|f| draw(f, &st, &lay)).unwrap();
        let buf = terminal.backend().buffer();
        assert_eq!(buf[(0, 0)].bg, Color::Rgb(0xf5, 0xf0, 0xe4), "light paper");
        let mark = (0..10).find(|&x| buf[(x, 0)].symbol() == "⬡").unwrap();
        assert_eq!(buf[(mark, 0)].fg, Color::Rgb(0xc8, 0x45, 0x1f), "vermilion mark");
    }

    #[test]
    fn every_view_renders_at_tiny_and_standard_sizes() {
        let modals = || {
            vec![
                None,
                Some(Modal::Help),
                Some(Modal::Goto(Default::default())),
                Some(Modal::Spawn),
                Some(Modal::Confirm(Pending::Kill("w-ci".into()))),
            ]
        };
        for (w, h) in [(40, 10), (80, 24), (1, 1), (12, 4), (220, 60)] {
            for empty in [false, true] {
                for view in [View::Fleet, View::Board, View::Plans, View::Brain, View::WorkerTypes, View::Remotes, View::UserRequests, View::Idle] {
                    for focus in [Focus::Sidebar, Focus::Pane] {
                        for modal in modals() {
                            let mut st = if empty { TuiState::default() } else { fleet() };
                            st.view = view;
                            st.focus = focus;
                            st.inspector = true;
                            st.selected = st.rows.len().saturating_sub(1).min(2);
                            st.modal = modal;
                            if !empty {
                                st.worker_types.reload(Ok(Default::default()));
                                if focus == Focus::Pane {
                                    st.worker_types.editing = Some("Research the following: {prompt}".into());
                                    st.worker_types.error = Some("worker type \"X\" has no {prompt} placeholder".into());
                                }
                            }
                            let out = render(&st, w, h);
                            assert_eq!(out.lines().count(), h as usize);
                        }
                    }
                }
            }
        }
        let st = fleet();
        let header = render(&st, 80, 24).lines().next().unwrap().to_string();
        // All six tabs must render at 80 columns — the tab bar never
        // silently drops the last one(s) under a tight width budget.
        for tab in ["1 Fleet", "2 Board", "3 Plans", "4 Brain", "5 Types", "6 Remotes"] {
            assert!(header.contains(tab), "all tabs fit at 80 columns: {header}");
        }
        // The `⚙ s` entry takes five columns after the tabs, so the labelled
        // status strip needs 100 columns; narrower it degrades to one health
        // dot rather than vanishing or clipping a label.
        let header = render(&st, 100, 24).lines().next().unwrap().to_string();
        assert!(header.contains("ptyd"), "the status strip must still show something at 100 columns: {header}");
        let header = render(&st, 90, 24).lines().next().unwrap().to_string();
        assert!(header.contains('●') && header.contains("⚙ s"), "{header}");
    }

    #[test]
    fn fleet_view_renders_islands_without_panicking() {
        let mut st = fleet();
        st.view = View::Fleet;
        st.fleet.sync(&st.rows.clone(), &std::collections::HashMap::new());
        let out = render(&st, 220, 40);
        assert!(out.contains("orch"), "the island's own orchestrator name must appear:\n{out}");
        assert!(out.contains("with a"), "a worker name must appear in the island's member rows:\n{out}");
    }

    #[test]
    fn fleet_detail_panel_includes_the_selected_sessions_summary() {
        let _iso = ninox_core::test_isolation::isolate();
        let mut st = fleet();
        st.view = View::Fleet;
        st.rows[1].session.summary = Some("refactoring the widget tree".into());
        st.fleet.sync(&st.rows.clone(), &std::collections::HashMap::new());
        st.fleet.nav.open_node("w-blocked", &st.fleet.islands);

        let out = render(&st, 220, 40);

        assert!(out.contains("refactoring the widget tree"), "detail panel missing the brief line:\n{out}");
    }

    #[test]
    fn fleet_detail_panel_lists_the_message_key_with_the_other_actions() {
        let _iso = ninox_core::test_isolation::isolate();
        let mut st = fleet();
        st.view = View::Fleet;
        st.fleet.sync(&st.rows.clone(), &std::collections::HashMap::new());
        st.fleet.nav.open_node("w-blocked", &st.fleet.islands);

        let out = render(&st, 220, 40);

        let hints = out.lines().find(|l| l.contains("c connect")).unwrap_or_else(|| panic!("no action row:\n{out}"));
        assert!(hints.contains("t message") && hints.contains("R reap"), "{hints}");
        assert!(!out.contains("^r restart") && !out.contains("F re-file"), "no conversation or workspace to restart into:\n{out}");

        let w = st.rows.iter_mut().find(|r| r.id() == "w-blocked").unwrap();
        w.session.workspace_path = Some("/work/w-blocked".into());
        w.session.claude_session_id = Some("conv".into());
        let out = render(&st, 220, 40);
        assert!(out.contains("^r restart") && out.contains("F re-file"), "the row wraps, but both are listed:\n{out}");
    }

    fn inspected(tab: crate::tui::inspector::Tab) -> TuiState {
        let mut st = st_with(&["a"]);
        st.view = View::Fleet;
        st.focus = Focus::Pane;
        st.inspector = true;
        st.inspector_tab = tab;
        st
    }

    #[test]
    fn inspector_info_shows_pr_ci_full_comments_done_criteria_and_outcome() {
        use ninox_core::types::{CIStatus, Comment, CompletionOutcome, PR};
        use ninox_core::worker_type::SuccessCriterion;
        let _iso = ninox_core::test_isolation::isolate();
        let mut st = inspected(crate::tui::inspector::Tab::Info);
        let s = &mut st.rows[0].session;
        s.pr_id = Some(9);
        s.pr_number = Some(412);
        s.success_criteria = vec![SuccessCriterion::PrOpened, SuccessCriterion::PrMerged];
        s.completion_outcome = Some(CompletionOutcome { summary: "parser rebased".into(), evidence: vec!["https://ci/run/1".into()], completed_at: 0 });
        let long = "Please **split** this function: the error branch duplicates the retry logic, and the final clause about backoff reaches past sixty characters";
        st.fleet.detail_pr = Some(fleet::DetailPr {
            pr_id: 9,
            pr: Some(PR { id: 9, number: 412, title: "Rebase the parser".into(), url: String::new(), body: String::new(), session_id: "a".into() }),
            ci: Some(CIStatus { pr_id: 9, total: 4, passing: 3, failing: 1, pending: 0 }),
            comments: vec![Comment { id: 1, pr_id: 9, author: "reviewer".into(), body: long.into(), path: Some("src/p.rs".into()), line: Some(7), created_at: 0 }],
        });

        let out = render(&st, 200, 80);

        for want in ["INFO", "#412 Rebase the parser", "3/4 checks passing · 1 failing", "REVIEW COMMENTS · 1", "reviewer src/p.rs:7",
                     "DEFINITION OF DONE", "✓ Pull request opened", "○ Pull request merged", "OUTCOME", "parser rebased", "https://ci/run/1"] {
            assert!(out.contains(want), "{want} missing:\n{out}");
        }
        assert!(out.contains("Please split this function") && out.contains("sixty characters"), "the comment is plain text and never cut:\n{out}");
    }

    #[test]
    fn inspector_info_lists_every_pr_of_a_session_open_first() {
        use ninox_core::pr_events::CiVerdict;
        use ninox_core::session_prs::{PrState, SessionPr};
        let _iso = ninox_core::test_isolation::isolate();
        let mut st = inspected(crate::tui::inspector::Tab::Info);
        let pr = |repo: &str, n, title: &str, state, passing, pending| SessionPr {
            repo: repo.into(), number: n, url: String::new(), title: title.into(), state,
            draft: false, ci: CiVerdict::Pending, passing, failing: 0, pending,
            changes_requested: false, approved: false, tracked: false,
        };
        st.rows[0].session.pr_number = Some(1466);
        st.rows[0].prs = vec![
            pr("o/bellhop", 1466, "Spoke sizing", PrState::Open, 28, 25),
            pr("o/manifests", 988, "Enable eus2", PrState::Open, 3, 0),
            pr("o/manifests", 989, "Enable euc1", PrState::Merged, 3, 0),
        ];

        let out = render(&st, 200, 80);
        dump("tui-inspector", &out);

        for want in ["PULL REQUEST · 3", "● bellhop#1466 Spoke sizing", "open · 28/53 checks passing",
                     "● manifests#988 Enable eus2", "✓ manifests#989 Enable euc1", "  merged"] {
            assert!(out.contains(want), "{want} missing:\n{out}");
        }
        let (a, b) = (out.find("bellhop#1466").unwrap(), out.find("manifests#989").unwrap());
        assert!(a < b, "open PRs come before merged ones:\n{out}");
    }

    #[test]
    fn inspector_diff_lists_files_with_counts_then_shows_one_files_hunks() {
        let _iso = ninox_core::test_isolation::isolate();
        let mut st = inspected(crate::tui::inspector::Tab::Diff);
        st.rows[0].session.workspace_path = Some("/work/a".into());
        st.diff.begin("a");
        st.diff.finish("a", Ok(Some("diff --git a/src/p.rs b/src/p.rs\n@@ -1,2 +1,2 @@\n-old line\n+new line\n ctx\ndiff --git a/README.md b/README.md\n@@ -1 +1,2 @@\n+more\n")), 1);
        st.diff.selected = 1;

        let out = render(&st, 200, 40);
        assert!(out.contains("DIFF") && out.contains("2 files · +2 −1"), "{out}");
        assert!(out.contains("src/p.rs") && out.contains("▸ README.md"), "{out}");

        st.diff.open = Some(0);
        let out = render(&st, 200, 40);
        assert!(out.contains("-old line") && out.contains("+new line") && !out.contains("README.md"), "{out}");
    }

    #[test]
    fn a_focused_inspector_still_draws_the_pane_layout_with_its_content() {
        let _iso = ninox_core::test_isolation::isolate();
        let mut st = inspected(crate::tui::inspector::Tab::Info);
        st.focus = Focus::Inspector;
        for (w, h) in [(160, 40), (120, 40)] {
            let out = render(&st, w, h);
            assert!(out.contains("INFO") && out.contains("j/k scroll") && out.contains("NAME"), "{w}x{h}:\n{out}");
        }
    }

    #[test]
    fn the_inspector_wraps_long_paths_reports_a_missing_worktree_and_owns_the_footer() {
        let _iso = ninox_core::test_isolation::isolate();
        let mut st = inspected(crate::tui::inspector::Tab::Info);
        st.focus = Focus::Inspector;
        st.rows[0].session.workspace_path = Some("/Users/someone/proj/ninox/.claude/worktrees/v1-mlops-5350-tui-parity-work".into());
        let out = render(&st, 160, 40);
        assert!(out.contains("parity-work") && out.contains(" INSPECT ") && out.contains("info/diff"), "{out}");
        st.inspector_tab = crate::tui::inspector::Tab::Diff;
        st.diff.begin("a");
        st.diff.finish("a", Err("/w no longer exists"), 1);
        assert!(render(&st, 160, 40).contains("couldn't read the diff: /w no longer exists"));
    }

    #[test]
    fn inspector_diff_says_why_when_there_is_nothing_to_list() {
        let _iso = ninox_core::test_isolation::isolate();
        let mut st = inspected(crate::tui::inspector::Tab::Diff);
        assert!(render(&st, 200, 40).contains("no recorded workspace"));
        st.rows[0].session.workspace_path = Some("/work/a".into());
        assert!(render(&st, 200, 40).contains("loading diff"));
        st.diff.begin("a");
        st.diff.finish("a", Ok(None), 1);
        assert!(render(&st, 200, 40).contains("no committed changes"));
        st.rows[0].session.machine_id = Some("box".into());
        assert!(render(&st, 200, 40).contains("remote sessions"));
    }

    #[test]
    fn a_selected_sidebar_rows_metadata_stays_visible_on_its_highlight() {
        let _iso = ninox_core::test_isolation::isolate();
        let mut st = st_with(&["a"]);
        st.view = View::Fleet;
        st.focus = Focus::Pane;
        st.rows[0].session.cost_usd = 1.25;
        let line = session_line(&st, 0, 60, LineKind::Tree, true);
        let cost = line.spans.iter().find(|s| s.content.contains("$1.25")).expect("the cost piece fits at 60 columns");
        assert_ne!(cost.style.fg, Some(st.palette.highlight), "{cost:?}");
    }

    #[test]
    fn the_selected_diff_file_never_paints_a_glyph_in_its_own_highlight_colour() {
        let _iso = ninox_core::test_isolation::isolate();
        for palette in [Palette::terminal(), Palette::of(ninox_core::ThemeVariant::Dark, true)] {
            let mut st = inspected(crate::tui::inspector::Tab::Diff);
            st.palette = palette;
            st.rows[0].session.workspace_path = Some("/work/a".into());
            st.diff.begin("a");
            st.diff.finish("a", Ok(Some("diff --git a/x b/x\n@@ -1 +1 @@\n-a\n+b\n")), 1);
            let buf = render_buffer(&st, 200, 40);
            for y in 0..40 {
                for x in 0..200 {
                    let c = &buf[(x, y)];
                    if c.bg == palette.highlight && c.symbol() != " " {
                        assert_ne!(c.fg, c.bg, "invisible {:?} at ({x},{y})", c.symbol());
                    }
                }
            }
        }
    }

    #[test]
    fn notifications_list_newest_first_with_stamps_and_the_header_counts_them() {
        use ninox_core::types::{Notification, NotificationKind as K};
        let _iso = ninox_core::test_isolation::isolate();
        for palette in [Palette::terminal(), Palette::of(ninox_core::ThemeVariant::Dark, true)] {
            let mut st = st_with(&["a"]);
            st.palette = palette;
            assert!(!render(&st, 140, 40).contains("✦"), "no count with nothing to show");
            for (id, kind, title) in [("1", K::CiFailure, "CI failed on a #7"), ("2", K::PrReadyToMerge, "a #7 is ready to merge")] {
                st.notifications.push_back(Notification { id: id.into(), kind, title: title.into(), body: format!("body {id}"), session_id: Some("a".into()), created_at: 0 });
            }
            assert!(render(&st, 140, 40).contains("✦ 2 (N)"));
            st.modal = Some(Modal::Notifications(1));
            let out = render(&st, 140, 40);
            let (ready, ci) = (out.find("a #7 is ready to merge").expect(&out), out.find("CI failed on a #7").expect(&out));
            assert!(ready < ci, "newest first:\n{out}");
            assert!(out.contains(" Ready ") && out.contains(" CI ") && out.contains("body 1") && out.contains("D dismiss all"), "{out}");
            let buf = render_buffer(&st, 140, 40);
            for y in 0..40 {
                for x in 0..140 {
                    let c = &buf[(x, y)];
                    if c.bg == palette.highlight && c.symbol() != " " {
                        assert_ne!(c.fg, c.bg, "invisible {:?} at ({x},{y})", c.symbol());
                    }
                }
            }
        }
    }

    #[test]
    fn the_notification_count_is_the_first_header_count_to_go_when_space_runs_out() {
        use ninox_core::types::{Notification, NotificationKind};
        let _iso = ninox_core::test_isolation::isolate();
        let mut st = st_with(&["a", "b"]);
        st.rows[0].session.activity = ActivityState::Blocked;
        assert!(!st.needs_rows().is_empty(), "fixture: something needs you");
        st.notifications.push_back(Notification { id: "1".into(), kind: NotificationKind::WorkerDone, title: "t".into(), body: String::new(), session_id: None, created_at: 0 });
        let mut dropped_only_the_count = false;
        for w in 60..200 {
            let out = render(&st, w, 20);
            let header = out.lines().next().unwrap_or_default();
            if header.contains("✦") {
                assert!(header.contains("need you"), "at {w} cols the count stayed but the alert went: {header}");
            } else if header.contains("need you") {
                dropped_only_the_count = true;
            }
        }
        assert!(dropped_only_the_count, "some width keeps the alert without the count");
    }

    #[test]
    fn worker_types_form_shows_a_row_per_rule_and_the_can_spawn_options() {
        let _iso = ninox_core::test_isolation::isolate();
        let mut st = st_with(&["a"]);
        st.view = View::WorkerTypes;
        let mut cfg = ninox_core::config::AppConfig::default();
        let mut spec = cfg.worker_type_registry().get("Evaluator").cloned().unwrap();
        spec.permissions.allow = vec!["Read".into(), "Grep".into()];
        spec.can_spawn = vec!["Researcher".into()];
        cfg.upsert_worker_type(spec);
        st.worker_types.reload(Ok(cfg));
        let out = render(&st, 160, 60);
        let read = out.lines().find(|l| l.contains("Read") && l.contains("Allow rules")).unwrap_or_else(|| panic!("{out}"));
        let grep = out.lines().find(|l| l.contains("Grep")).unwrap_or_else(|| panic!("{out}"));
        assert!(!read.is_empty() && !grep.contains("Allow rules"), "the second rule row has no label of its own:\n{out}");
        assert!(out.contains("Can spawn") && out.contains("↳ *") && out.contains("↳ Researcher") && out.contains("Can manage types"), "{out}");

        st.worker_types.on_list = false;
        st.worker_types.field_selected = st.worker_types.fields.iter().position(|f| f.value == "Grep").unwrap();
        st.worker_types.adding_rule = Some(crate::tui::worker_types::RuleList::Allow);
        st.worker_types.editing = Some("Bash(ls:*)".into());
        let out = render(&st, 160, 60);
        assert!(out.contains("+ Bash(ls:*)"), "a rule being added reads as new, not as an edit of the row under it:\n{out}");
    }

    #[test]
    fn the_board_header_shows_the_filter_scope_chips_resume_all_and_the_count() {
        use ninox_core::types::SessionStatus as S;
        let _iso = ninox_core::test_isolation::isolate();
        let mut st = st_with(&["orch", "w-a", "w-b"]);
        st.view = View::Board;
        st.rows[0].is_orchestrator = true;
        st.rows[1].session.orchestrator_id = Some("orch".into());
        st.rows[2].session.status = S::Interrupted;
        st.rows.push(Row::test_row("w-spawning"));
        st.rows[3].session.status = S::Spawning;
        let out = render(&st, 160, 30);
        let header = out.lines().nth(2).unwrap_or_default();
        assert!(header.contains("/ filter") && header.contains("ALL  orch"), "{header}");
        assert!(header.contains("resume all (1)") && header.contains("2/2 sessions"), "{header}");
        st.kanban.query = "w-a".into();
        st.kanban.scope = Some("orch".into());
        let header = render(&st, 160, 30).lines().nth(2).unwrap_or_default().to_string();
        assert!(header.contains("w-a") && header.contains("1/2 sessions"), "{header}");
        assert!(render(&st, 60, 30).lines().all(|l| l.width() <= 60));
    }

    #[test]
    fn a_sub_orchestrators_tree_group_folds_with_its_island_and_the_toolbar_says_so() {
        use ninox_core::types::SessionStatus as S;
        let _iso = ninox_core::test_isolation::isolate();
        let mut st = st_with(&["o1", "o2", "w-done"]);
        st.view = View::Fleet;
        st.tree_collapsed = false;
        for (i, (orch, group, is_orch)) in [(None, "o1", true), (Some("o1"), "o2", true), (Some("o2"), "o2", false)].into_iter().enumerate() {
            st.rows[i].session.orchestrator_id = orch.map(str::to_string);
            st.rows[i].group = Some(group.into());
            st.rows[i].is_orchestrator = is_orch;
        }
        st.rows[2].session.status = S::Done;
        st.fleet.sync(&st.rows.clone(), &std::collections::HashMap::new());
        let island = ninox_overview::nav::owning_island(&st.fleet.islands, "o2").expect("o2 sits in o1's island");
        assert!(!render(&st, 160, 30).contains("w-done"), "folded by default");
        st.fleet.toggle_island_fold(&island);
        st.fleet.nav.selected_island = Some(island);
        let out = render(&st, 160, 30);
        assert!(out.contains("w-done"), "the tree follows the island's fold:\n{out}");
        assert!(out.contains("finished: expanded"), "the toolbar reads the focused island:\n{out}");
    }

    #[test]
    fn the_board_header_is_inset_like_the_columns_and_empty_columns_say_so_quietly() {
        let _iso = ninox_core::test_isolation::isolate();
        let mut st = st_with(&["w-a"]);
        st.view = View::Board;
        let out = render(&st, 120, 30);
        let header = out.lines().nth(2).unwrap_or_default();
        assert!(header.starts_with(' ') && header.trim_end().ends_with("1/1 sessions"), "{header:?}");
        assert!(header.chars().count() <= 120 && header.ends_with(' '), "the count stops one cell short of the edge: {header:?}");
        assert!(!out.contains('○'), "no stray empty-state glyph:\n{out}");
        assert!(out.contains("empty"), "{out}");
    }

    #[test]
    fn a_truncated_board_card_keeps_its_pr_number() {
        let _iso = ninox_core::test_isolation::isolate();
        let mut st = st_with(&["a-rather-long-worker-name"]);
        st.view = View::Board;
        st.rows[0].session.pr_number = Some(412);
        let out = render(&st, 120, 30);
        assert!(out.contains("… #412"), "{out}");
    }

    #[test]
    fn restart_confirm_labels_its_third_button_restart_all() {
        let _iso = ninox_core::test_isolation::isolate();
        let mut st = st_with(&["a", "b"]);
        st.modal = Some(Modal::Confirm(Pending::Restart("a".into())));
        let out = render(&st, 120, 40);
        assert!(out.contains("Restart a?") && out.contains("[ a Restart all ]"), "{out}");
        let lay = layout::compute(ratatui::layout::Rect::new(0, 0, 120, 40), &st);
        let all = lay.modal_all.expect("third button is clickable");
        assert_eq!(all.width as usize, "[ a Restart all ]".len(), "click target matches the drawn label");
    }

    #[test]
    fn message_composer_names_its_target_and_keeps_the_caret_end_of_a_long_draft_in_view() {
        let _iso = ninox_core::test_isolation::isolate();
        let mut st = st_with(&["a"]);
        let draft = format!("{}the end", "x".repeat(200));
        st.modal = Some(Modal::Message(crate::tui::state::MessageComposer { id: "a".into(), name: "acme-worker".into(), draft }));

        let out = render(&st, 120, 40);

        assert!(out.contains("message acme-worker"), "{out}");
        assert!(out.contains("the end▏"), "caret end of the draft must stay visible:\n{out}");
    }

    #[test]
    fn tail_fit_keeps_the_end_that_fits_by_display_width() {
        assert_eq!(tail_fit("hello", 10), "hello");
        assert_eq!(tail_fit("hello", 3), "llo");
        assert_eq!(tail_fit("ab日本", 4), "日本", "wide chars count two columns");
        assert_eq!(tail_fit("ab日本", 3), "本");
    }

    #[test]
    fn fleet_detail_panel_shows_the_definition_of_done_with_unmet_criteria() {
        let _iso = ninox_core::test_isolation::isolate();
        use ninox_core::worker_type::SuccessCriterion;

        let mut st = fleet();
        st.view = View::Fleet;
        st.rows[1].session.success_criteria = vec![SuccessCriterion::PrMerged, SuccessCriterion::Check("docs updated".into())];
        st.fleet.sync(&st.rows.clone(), &std::collections::HashMap::new());
        st.fleet.nav.open_node("w-blocked", &st.fleet.islands);

        let out = render(&st, 220, 40);

        assert!(out.contains("Definition of done"), "missing the field label:\n{out}");
        assert!(out.contains("docs updated"), "missing the Check criterion's own text:\n{out}");
        assert!(out.contains('○'), "neither criterion is met yet — must show a hollow circle:\n{out}");
    }

    #[test]
    fn fleet_detail_panel_shows_the_outcome_once_stored() {
        let _iso = ninox_core::test_isolation::isolate();
        let mut st = fleet();
        st.view = View::Fleet;
        st.rows[1].session.success_criteria = vec![ninox_core::worker_type::SuccessCriterion::ReportSent];
        st.rows[1].session.completion_outcome = Some(ninox_core::types::CompletionOutcome {
            summary: "shipped the refactor".into(), evidence: vec![], completed_at: 0,
        });
        st.fleet.sync(&st.rows.clone(), &std::collections::HashMap::new());
        st.fleet.nav.open_node("w-blocked", &st.fleet.islands);

        let out = render(&st, 220, 40);

        assert!(out.contains("Outcome"), "missing the field label:\n{out}");
        assert!(out.contains("shipped the refactor"), "missing the outcome summary:\n{out}");
    }

    const LONG_ANSWER: &str = "Bellhop's dashboard work is spread over **two** sessions. I'd split the migration into one orchestrator with three workers, so each slice lands on its own.\n\n## Slices\n\n1. Map every panel's query against the old API.\n2. Port the panels to `resources`.\n\n```rust\nlet client = ResourcesClient::new(cfg);\nclient.migrate_dashboard(\"bellhop\")?;\n```\n\nNothing is spawned until you accept.";

    fn reply_state(w_rows: &[&str]) -> TuiState {
        let mut st = st_with(w_rows);
        st.view = View::Fleet;
        st.now_ms = 10_000;
        st
    }

    fn add_turn(st: &mut TuiState, question: &str, answer: &str) {
        use ninox_ui_shared::prompt_agent::reply::{AnswerSource, AnswerState};
        st.fleet.reply.ask(question.into(), "ninox".into(), st.now_ms);
        let turn = st.fleet.reply.drawer.latest_mut().unwrap();
        turn.source = AnswerSource::Transcript(Default::default());
        turn.answer = answer.into();
        turn.state = AnswerState::Done;
        turn.ended_at_ms = Some(st.now_ms);
    }

    #[test]
    fn reply_panel_renders_stacked_turns_with_markdown_at_120_and_200_columns() {
        for (w, h) in [(120u16, 40u16), (200, 55)] {
            let mut st = reply_state(&["a"]);
            add_turn(&mut st, "Is #1099 blocked?", "No — it's waiting on review.");
            add_turn(&mut st, "Propose how to split the bellhop migration.", LONG_ANSWER);
            st.fleet.reply.drawer.context_pct = Some(18.0);

            let out = render(&st, w, h);

            assert!(out.contains("❯ Ninox prompt"), "{w}x{h}: {out}");
            assert!(out.contains("idle · context 18% used"), "{w}x{h}: {out}");
            assert!(out.contains("ctrl+l clear") && out.contains("esc collapse"), "{w}x{h}: {out}");
            assert!(out.contains("Slices") && !out.contains("## Slices"), "headings render without markup: {out}");
            assert!(out.contains("1. Map every panel"), "{out}");
            assert!(out.contains("│ let client = ResourcesClient::new(cfg);"), "code sits in a box: {out}");
            assert!(!out.contains("**"), "{out}");
            let question = out.lines().find(|l| l.contains("Propose how to split")).expect("the latest question opens the panel");
            let lead = question.find("Propose").unwrap();
            assert!(lead > w as usize / 4, "the question is right-aligned: {question:?}");
            st.fleet.reply.scroll_to_bottom();
            let out = render(&st, w, h);
            assert!(out.contains("Nothing is spawned until you accept."), "{w}x{h}: {out}");
        }
    }

    #[test]
    fn the_thinking_line_spins_with_the_clock() {
        let mut st = reply_state(&[]);
        st.fleet.reply.ask("q".into(), "ninox".into(), st.now_ms);
        let line_at = |st: &mut TuiState, now_ms: i64| {
            st.now_ms = now_ms;
            let (lines, _) = reply_conversation(st, 100);
            lines.iter().map(|l| l.to_string()).find(|l| l.contains("is thinking…")).expect("a thinking line")
        };
        assert_eq!(line_at(&mut st, 0), "⠋ ninox is thinking…");
        assert_eq!(line_at(&mut st, 80), "⠙ ninox is thinking…");
        assert_eq!(line_at(&mut st, 80 * 10), "⠋ ninox is thinking…", "ten frames, then around again");
        let frames: std::collections::HashSet<char> = (0..10).map(|i| spinner_frame(i * 80)).collect();
        assert_eq!(frames.len(), 10);
    }

    #[test]
    fn reply_conversation_never_exceeds_its_width() {
        for width in [116u16, 196, 30] {
            let mut st = reply_state(&[]);
            add_turn(&mut st, &"long question ".repeat(30), LONG_ANSWER);
            for line in reply_conversation(&st, width).0 {
                assert!(line.width() <= width as usize, "{width}: {line:?}");
            }
        }
    }

    #[test]
    fn collapsed_reply_shows_the_last_answers_first_line_and_reopen_hint() {
        let mut st = reply_state(&["a"]);
        add_turn(&mut st, "q", LONG_ANSWER);
        st.fleet.reply.collapse();

        let out = render(&st, 120, 40);

        assert!(!out.contains("❯ Ninox prompt"), "{out}");
        assert!(out.contains("Bellhop's dashboard work is spread over two sessions."), "{out}");
        assert!(out.contains("/ reopen"), "{out}");
        assert!(!out.contains("open pane"), "the prompt-agent singleton is never a navigable pane: {out}");
    }

    #[test]
    fn cleared_reply_shows_the_notice_with_no_history() {
        let mut st = reply_state(&["a"]);
        add_turn(&mut st, "q", "an answer that must vanish");
        st.fleet.reply.cleared();

        let out = render(&st, 120, 40);

        assert!(out.contains("Context cleared — the next question starts a fresh conversation."), "{out}");
        assert!(!out.contains("an answer that must vanish"), "{out}");
    }

    #[test]
    fn relay_row_waits_then_times_out_to_no_direct_reply() {
        use ninox_ui_shared::prompt_agent::reply::{Relay, RELAY_REPLY_GRACE_MS};
        let mut st = st_with(&[]);
        st.rows = vec![{
            let mut r = Row::test_row("orch-1");
            r.session.name = "Bellhop api crashing".into();
            r.is_orchestrator = true;
            r
        }];
        st.view = View::Fleet;
        st.now_ms = 10_000;
        add_turn(&mut st, "Is #1099 blocked?", "");
        st.fleet.reply.drawer.latest_mut().unwrap().relays.push(Relay { to_id: "orch-1".into(), relayed_at_ms: st.now_ms, replied_at_ms: None });

        let text = |st: &TuiState| reply_conversation(st, 200).0.iter().map(|l| l.spans.iter().map(|s| s.content.to_string()).collect::<String>()).collect::<Vec<_>>().join("\n");
        let hhmm = ninox_ui_shared::prompt_agent::reply::format_hhmm(st.now_ms);
        assert!(text(&st).contains(&format!("Asked Bellhop api crashing (its orchestrator) — relayed {hhmm}, waiting for its answer…")), "{}", text(&st));
        assert_eq!(st.fleet.reply.drawer.status(st.now_ms), ninox_ui_shared::prompt_agent::reply::DrawerStatus::Relaying);

        st.now_ms += RELAY_REPLY_GRACE_MS;
        assert!(text(&st).contains("no direct reply yet"), "{}", text(&st));
        assert_eq!(st.fleet.reply.drawer.status(st.now_ms), ninox_ui_shared::prompt_agent::reply::DrawerStatus::Idle);
    }

    #[test]
    fn overflowing_reply_shows_a_more_hint_on_its_bottom_border() {
        let mut st = reply_state(&["a"]);
        for i in 0..3 {
            add_turn(&mut st, &format!("question {i}"), LONG_ANSWER);
        }
        let out = render(&st, 120, 40);
        assert!(out.contains("↓ more · PgDn scroll"), "unfocused, PgDn is the key that scrolls: {out}");
        st.fleet.reply.focused = true;
        assert!(render(&st, 120, 40).contains("↓ more · j/k scroll"));
        st.fleet.reply.scroll_to_bottom();
        assert!(!render(&st, 120, 40).contains("↓ more"), "nothing below the fold at the end");

        let mut short = reply_state(&["a"]);
        add_turn(&mut short, "q", "short");
        assert!(!render(&short, 120, 40).contains("↓ more"));
    }

    #[test]
    fn reply_panel_scroll_is_bounded_by_what_was_drawn() {
        let mut st = reply_state(&["a"]);
        for i in 0..6 {
            add_turn(&mut st, &format!("question {i}"), LONG_ANSWER);
        }
        let out = render(&st, 120, 40);
        let max = st.fleet.reply.max_scroll.get();
        assert!(max > 0, "six long turns overflow a 40-row panel");
        assert!(out.contains("question 5") && !out.contains("question 4"), "it opens on the latest question: {out}");
        st.fleet.reply.scroll_to_top();
        let out = render(&st, 120, 40);
        assert!(out.contains("question 0"), "scrolled to the top shows the first turn: {out}");
        st.fleet.reply.scroll_by(1);
        assert_eq!(st.fleet.reply.scroll, Some(0), "never above the first row");
        st.fleet.reply.scroll_to_bottom();
        st.fleet.reply.scroll_by(-1);
        assert_eq!(st.fleet.reply.scroll, Some(max), "never past the last row");
    }

    #[test]
    fn fleet_prompt_shows_live_input_while_editing_with_the_reply_open() {
        let mut st = st_with(&["a"]);
        st.view = View::Fleet;
        add_turn(&mut st, "q", "a");
        st.fleet.prompt_editing = true;
        st.fleet.prompt_input = "what next?".to_string();

        let out = render(&st, 100, 20);

        assert!(out.contains("what next?"), "{out}");
    }

    /// T2 (FEEDBACK-v1-tryout-round1.md): the prompt is a primary control and
    /// must read as an input box at a glance.
    #[test]
    fn fleet_prompt_is_a_bordered_box_with_one_target_line_and_hint() {
        let st = fleet();
        let out = render(&st, 120, 30);
        assert!(out.contains("❯ ninox ▾"), "the prompt line leads with ❯ and its target: {out}");
        assert_eq!(out.matches("ninox ❯").count(), 0, "no separate title repeating the target: {out}");
        assert!(out.contains("/ focus · ⏎ send"), "the prompt box must show its focus/send hint: {out}");
        assert!(out.contains('╭') || out.contains('─'), "the prompt must be a bordered box, not a bare line: {out}");
    }

    #[test]
    fn fleet_prompt_border_turns_to_the_accent_colour_when_focused() {
        let (w, h) = (120, 30);
        let mut st = fleet();
        let prompt = layout::compute(Rect::new(0, 0, w, h), &st).fleet.expect("fleet layout").prompt;
        let border_fg = |st: &TuiState| {
            let backend = ratatui::backend::TestBackend::new(w, h);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            let lay = layout::compute(Rect::new(0, 0, w, h), st);
            terminal.draw(|f| draw(f, st, &lay)).unwrap();
            // The box's top-left corner glyph, not the title text.
            terminal.backend().buffer()[(prompt.x, prompt.y)].fg
        };
        assert_eq!(border_fg(&st), st.palette.faint, "unfocused prompt border should be dim");
        st.fleet.prompt_editing = true;
        assert_eq!(border_fg(&st), st.palette.accent, "focused prompt border should turn to the accent colour");
    }

    #[test]
    fn fleet_lays_out_tree_toolbar_main_and_prompt_with_non_zero_area() {
        let mut st = fleet();
        st.view = View::Fleet;
        st.tree_collapsed = false; // this test is about the tree's own region, not its collapse state
        st.fleet.sync(&st.rows.clone(), &std::collections::HashMap::new());
        st.fleet.nav.open_node("orch", &st.fleet.islands);

        let lay = layout::compute(Rect::new(0, 0, 220, 40), &st);
        let fl = lay.fleet.expect("fleet layout must be computed for View::Fleet");

        assert!(fl.tree.width > 0 && fl.tree.height > 0, "tree region must have area: {:?}", fl.tree);
        assert!(fl.toolbar.width > 0 && fl.toolbar.height > 0, "toolbar region must have area: {:?}", fl.toolbar);
        assert!(fl.main.width > 0 && fl.main.height > 0, "main region must have area: {:?}", fl.main);
        assert!(fl.prompt.width > 0 && fl.prompt.height > 0, "prompt region must have area: {:?}", fl.prompt);
        assert!(fl.detail.is_some(), "detail panel must appear once an island is open and a node selected");
    }

    #[test]
    fn fleet_footer_still_advertises_ordinary_session_management() {
        // Final review Important #6: the footer only showed Fleet-specific
        // hints, even though n/x/g already reach `command()` as bare keys
        // on Fleet (nothing in `fleet_key` claims them).
        let st = fleet();
        let out = render(&st, 220, 40);
        let footer = out.lines().last().unwrap();
        assert!(footer.contains("n new") && footer.contains("x kill/remove") && footer.contains("g go to"), "{footer}");
    }

    #[test]
    fn fleet_toolbar_shows_sort_view_and_fold_state() {
        let st = fleet();
        let out = render(&st, 220, 40);
        assert!(out.contains("sort: needs-you"), "{out}");
        assert!(out.contains("view: compact"), "{out}");
        assert!(out.contains("finished: folded"), "{out}");
    }

    #[test]
    fn graph_view_renders_with_an_empty_fleet() {
        let st = TuiState { view: View::Fleet, ..Default::default() };
        let out = render(&st, 80, 24);
        let _ = out; // must not panic
    }

    fn ended_report(pr: bool, workspace: bool) -> TuiState {
        use crate::tui::report::{Commit, FileStat, GitState, GitSummary, PrFacts, ReportData, ReportSlot};
        let mut st = st_with(&["w1", "w2"]);
        let s = &mut st.rows[0].session;
        s.name = "fix the auth token refresh race".into();
        s.status = SessionStatus::Done;
        s.started_at = 1_000_000;
        s.terminal_at = Some(1_000_000 + 39 * 60_000);
        s.cost_usd = 1.25;
        s.pr_number = pr.then_some(142);
        if workspace {
            s.workspace_path = Some("/repo/.claude/worktrees/w1".into());
        }
        st.now_ms = 1_000_000 + 40 * 60_000;
        let git = if workspace {
            GitState::Ready(GitSummary {
                branch: Some("fix-auth".into()),
                base: Some("origin/main".into()),
                commits: vec![Commit { sha: "a1b2c3d".into(), subject: "fix: refresh before expiry".into() }],
                commits_total: 1,
                files: vec![FileStat { path: "src/auth.rs".into(), added: 12, removed: 3 }],
                uncommitted: 2,
            })
        } else {
            GitState::NoWorkspace
        };
        let data = ReportData {
            task: Some("Fix the race where two requests refresh the token at once".into()),
            pr: pr.then(|| PrFacts { number: 142, title: Some("Fix token refresh race".into()), url: Some("https://github.com/o/r/pull/142".into()), ci: None }),
            git,
            resumable: true,
            ..Default::default()
        };
        st.reports.insert("w1".into(), ReportSlot { data: Some(data), loading: false });
        st
    }

    #[test]
    fn an_ended_session_shows_its_report_with_buttons() {
        let mut st = ended_report(true, true);
        st.focus = Focus::Pane;
        let out = render(&st, 140, 40);
        for want in [
            "fix the auth token refresh race",
            " done ",
            "PR merged",
            "(39m)",
            "$1.25",
            "[ x Remove ]",
            "[ r Resume ]",
            "[ O Open PR ]",
            "TASK",
            "two requests refresh the token",
            "#142 Fix token refresh race",
            "https://github.com/o/r/pull/142",
            "⎇ fix-auth",
            "1 commit ahead of origin/main",
            "a1b2c3d fix: refresh before expiry",
            "1 file changed  +12 −3",
            "2 uncommitted changes",
            "/repo/.claude/worktrees/w1",
        ] {
            assert!(out.contains(want), "{want} missing:\n{out}");
        }
        assert!(!out.contains("no live process"), "the old empty state is gone: {out}");
        let footer = out.lines().last().unwrap();
        assert!(footer.contains("x remove") && footer.contains("r resume") && footer.contains("O open PR"), "bare keys in the footer: {footer}");
    }

    #[test]
    fn a_report_without_pr_or_workspace_says_so_and_drops_the_pr_button() {
        let mut first = ended_report(false, false);
        first.focus = Focus::Pane;
        let out = render(&first, 120, 30);
        assert!(out.contains("no workspace recorded"), "{out}");
        assert!(!out.contains("PULL REQUEST") && !out.contains("Open PR"), "{out}");
        assert!(out.contains("[ x Remove ]"));
        let mut st = ended_report(false, false);
        st.focus = Focus::Pane;
        st.reports.clear();
        let out = render(&st, 120, 30);
        assert!(out.contains("loading…"), "before the report arrives: {out}");
    }

    #[test]
    fn report_shows_an_exited_ptyd_panes_last_screen() {
        let mut st = ended_report(false, true);
        st.focus = Focus::Pane;
        st.rows[0].session.status = SessionStatus::Working;
        st.panes.insert(
            "w1".into(),
            ninox_ptyd::PaneInfo {
                pane: "w1".into(), pid: 7, cols: 80, rows: 24, alive: false, exit_code: Some(1),
                created_ms: 0, last_output_ms: 0, title: None, cwd: "/".into(), seq: 1, history_size: 0,
            },
        );
        st.views.insert("w1".into(), PaneView { live: Some(snap(&["error: tests failed", "", "$ "], 0, 40)), ..Default::default() });
        let out = render(&st, 140, 50);
        assert!(out.contains("exited (code 1)") && out.contains(" exited ") && out.contains("LAST SCREEN") && out.contains("error: tests failed"), "{out}");
    }

    #[test]
    fn report_renders_at_narrow_sizes_without_panicking() {
        for (pr, ws) in [(true, true), (false, false)] {
            for focus in [Focus::Sidebar, Focus::Pane] {
                for (w, h) in [(40, 10), (80, 24), (12, 4), (1, 1)] {
                    let mut st = ended_report(pr, ws);
                    st.focus = focus;
                    st.report_scroll = 99;
                    assert_eq!(render(&st, w, h).lines().count(), h as usize);
                    st.modal = Some(Modal::Confirm(Pending::Remove("w1".into())));
                    assert_eq!(render(&st, w, h).lines().count(), h as usize);
                }
            }
        }
        let mut st = ended_report(true, true);
        st.modal = Some(Modal::Confirm(Pending::Remove("w1".into())));
        let out = render(&st, 100, 30);
        assert!(out.contains("Remove fix the auth token refresh race?") && out.contains("[ y Yes ]") && out.contains("[ n No ]"), "{out}");
    }

    #[test]
    fn brain_list_shows_the_active_catalogue_name_when_more_than_one_is_configured() {
        let mut st = TuiState { view: View::Brain, ..Default::default() };
        st.brain = crate::tui::brain::tests::sample();
        st.brain.set_catalogues(vec![
            ninox_core::config::CatalogueRef {
                name: "default".to_string(),
                path: std::path::PathBuf::from("/brains/default"),
                remote: None,
                endpoint: None,
                region: None,
                cache_ttl_secs: None,
                enabled: true,
                cadence: "manual".to_string(),
                access: "read_write".to_string(),
            },
            ninox_core::config::CatalogueRef {
                name: "team".to_string(),
                path: std::path::PathBuf::from("/brains/team"),
                remote: None,
                endpoint: None,
                region: None,
                cache_ttl_secs: None,
                enabled: true,
                cadence: "manual".to_string(),
                access: "read_write".to_string(),
            },
        ]);
        let out = render(&st, 100, 20);
        assert!(out.contains("All sources"), "{out}");
        st.brain.cycle_catalogue();
        let out = render(&st, 100, 20);
        assert!(out.contains("default"), "{out}");
    }

    #[test]
    fn brain_list_hides_the_catalogue_name_with_only_the_implicit_default() {
        let mut st = TuiState { view: View::Brain, ..Default::default() };
        st.brain = crate::tui::brain::tests::sample();
        let out = render(&st, 100, 20);
        assert!(!out.contains("All sources"), "nothing to pick from, so no clutter: {out}");
    }

    #[test]
    fn brain_renders_a_tag_tree_with_buttons_and_an_indexing_note() {
        let mut st = TuiState { view: View::Brain, ..Default::default() };
        st.brain = crate::tui::brain::tests::sample();
        st.brain.move_cursor(5);
        let out = render(&st, 100, 20);
        assert!(out.contains("4 entries · by tag") && out.contains("[ a New entry ]"), "{out}");
        assert!(out.contains("▾ tmux  (2)") && out.contains("▾ untagged  (1)"), "{out}");
        assert!(out.contains("[ e Edit ]  [ D Delete ]") && out.contains("#tmux #runtime"), "{out}");
        assert!(out.contains("L links") && out.contains("[ / ] kind") && out.contains("s sort") && out.contains("x clear"), "footer hints: {out}");
        assert!(!out.contains("a new") && !out.contains("D delete"), "at width 100 the lowest-priority hints make way for the new ones: {out}");
        assert!(render(&st, 200, 20).contains("D delete"), "at full width nothing is permanently dropped, only trimmed under pressure");
        st.brain.indexing = Some("indexing concepts/ptyd.md".into());
        st.brain.toggle_fold();
        let out = render(&st, 100, 20);
        assert!(out.contains("indexing concepts/ptyd.md…"), "{out}");
        assert!(out.contains("▸ tmux  (2)") && !out.contains("[ e Edit ]"), "a folded header is selected, no Edit: {out}");
        for (w, h) in [(1, 1), (12, 4), (40, 10), (220, 60)] {
            assert_eq!(render(&st, w, h).lines().count(), h as usize);
        }
    }

    #[test]
    fn user_requests_render_shows_rows_draft_footer_badge_and_tmux_caveat() {
        use ninox_core::store::UserRequestRow;
        use ninox_core::types::UserRequestStatus;
        let row = |id: &str, mode: UserRequestMode| UserRequestRow {
            id: id.into(),
            requester_session_id: "w1".into(),
            prompt: "merge now?".into(),
            mode,
            status: UserRequestStatus::Pending,
            answer: None,
            answered_at: None,
            answer_delivered_at: None,
            created_at: 0,
        };
        let mut st = TuiState { view: View::UserRequests, ..Default::default() };
        let out = render(&st, 120, 20);
        assert!(out.contains("No pending requests"), "{out}");
        assert!(!out.contains("pending user request"), "footer badge hidden when nothing is pending: {out}");

        st.pending_user_requests = vec![row("ur-1", UserRequestMode::Sync), row("ur-2", UserRequestMode::Async)];
        let out = render(&st, 120, 20);
        assert!(out.contains("2 pending requests"), "{out}");
        assert!(out.contains("SYNC") && out.contains("ASYNC") && out.contains("merge now?"), "{out}");
        assert!(!out.contains("pending user requests — press u"), "the requests screen has its own count: {out}");
        st.view = View::Board;
        let board = render(&st, 120, 20);
        assert!(board.contains("2 pending user requests — press u to view"), "the footer badge is visible elsewhere: {board}");
        st.view = View::UserRequests;

        st.user_requests_draft = Some(crate::tui::state::UserRequestDraft { id: "ur-2".into(), buf: "yes".into() });
        let out = render(&st, 120, 20);
        assert!(out.contains("answer: yes"), "{out}");

        st.in_tmux = true;
        let out = render(&st, 120, 20);
        assert!(out.contains("keystroke-injection"), "{out}");

        for (w, h) in [(1, 1), (12, 4), (40, 10), (220, 60)] {
            assert_eq!(render(&st, w, h).lines().count(), h as usize);
        }
    }

    #[test]
    fn pending_badge_survives_a_long_hint_row_on_a_narrow_footer() {
        use ninox_core::store::UserRequestRow;
        use ninox_core::types::UserRequestStatus;
        let mut st = fleet();
        st.pending_user_requests = vec![UserRequestRow {
            id: "ur-1".into(), requester_session_id: "w1".into(), prompt: "merge now?".into(),
            mode: UserRequestMode::Sync, status: UserRequestStatus::Pending, answer: None,
            answered_at: None, answer_delivered_at: None, created_at: 0,
        }];
        let out = render(&st, 80, 24);
        assert!(out.contains("1 pending user request — press u to view"), "{out}");
    }

    #[test]
    fn a_failed_console_registration_is_shown_on_the_requests_screen() {
        let st = TuiState {
            view: View::UserRequests,
            human_console_error: Some("caller descends from a known agent process".into()),
            ..Default::default()
        };
        let out = render(&st, 160, 20);
        assert!(out.contains("can't answer from this TUI: caller descends from a known agent process"), "{out}");
    }

    #[test]
    fn stale_row_is_badged_and_still_shown_a_live_one_is_not() {
        use ninox_core::store::UserRequestRow;
        use ninox_core::types::{SessionStatus, UserRequestStatus};
        let row = |id: &str, requester: &str| UserRequestRow {
            id: id.into(),
            requester_session_id: requester.into(),
            prompt: "merge now?".into(),
            mode: UserRequestMode::Async,
            status: UserRequestStatus::Pending,
            answer: None,
            answered_at: None,
            answer_delivered_at: None,
            created_at: 0,
        };
        let session_row = |id: &str, status: SessionStatus| Row {
            session: ninox_ui_shared::cli::test_fixtures::session(id, None, status),
            is_orchestrator: false,
            group: None,
            machine_health: None,
            queue_position: None, prs: Vec::new(),
        };
        let st = TuiState {
            view: View::UserRequests,
            pending_user_requests: vec![row("ur-dead", "dead"), row("ur-live", "live")],
            rows: vec![session_row("dead", SessionStatus::Terminated), session_row("live", SessionStatus::Working)],
            ..Default::default()
        };
        let out = render(&st, 120, 20);

        assert!(out.contains("merge now?"), "neither row is hidden: {out}");
        let dead_line = out.lines().find(|l| l.contains("from dead")).unwrap();
        let live_line = out.lines().find(|l| l.contains("from live")).unwrap();
        assert!(dead_line.contains("STALE"), "{dead_line}");
        assert!(!live_line.contains("STALE"), "{live_line}");
    }

    #[test]
    fn brain_renders_the_backlinks_related_and_outlinks_panel_with_the_focused_row_marked() {
        let mut st = TuiState { view: View::Brain, ..Default::default() };
        st.brain = crate::tui::brain::tests::sample();
        st.brain.move_cursor(5); // tmux's ptyd copy
        let alpha = st.brain.entries[3].clone(); // patterns/Alpha.md
        let loose = st.brain.entries[1].clone(); // concepts/loose.md
        st.brain.backlinks = vec![alpha];
        st.brain.outlinks = vec![loose];
        let out = render(&st, 100, 30);
        assert!(out.contains("Cited by · 1") && out.contains("Links to · 1"), "{out}");
        assert!(out.contains("Alpha") && out.contains("loose"), "{out}");
        assert!(!out.contains('▸') || out.contains("▸ tmux"), "no link row is focused yet: {out}");
        st.brain.link_focus = Some(0);
        let out = render(&st, 100, 30);
        let links_region = out.split("Cited by").nth(1).unwrap_or("");
        assert!(links_region.contains("▸ Alpha"), "the focused (first) link row is marked: {out}");
    }

    #[test]
    fn brain_graph_renders_the_focus_and_its_links() {
        let mut st = TuiState { view: View::Brain, ..Default::default() };
        st.brain = crate::tui::brain::tests::sample();
        st.brain.set_edges(&[("concepts/ptyd.md".to_string(), "errors/socket.md".to_string())]);
        st.brain.graph_mode = true;
        st.brain.graph_focus = Some("concepts/ptyd.md".into());
        let out = render(&st, 100, 30);
        assert!(out.contains("Brain graph") && out.contains("Neighbourhood"), "{out}");
        assert!(out.contains("ptyd") && out.contains("socket"), "focus and hop-1 node titles both render: {out}");
    }

    #[test]
    fn brain_graph_shows_an_empty_state_with_nothing_selected() {
        let mut st = TuiState { view: View::Brain, ..Default::default() };
        st.brain.graph_mode = true;
        let out = render(&st, 100, 30);
        assert!(out.contains("Nothing to show"), "{out}");
    }

    #[test]
    fn brain_clusters_table_renders_ranked_repos_with_the_selected_row_marked() {
        let mut st = TuiState { view: View::Brain, ..Default::default() };
        st.brain = crate::tui::brain::tests::sample();
        st.brain.entries[0].repos = vec!["ninox".to_string()]; // ptyd
        st.brain.entries[1].repos = vec!["ninox".to_string()]; // loose
        st.brain.entries[2].repos = vec!["bellhop".to_string()]; // socket
        st.brain.graph_mode = true;
        st.brain.graph_scope = crate::tui::brain::GraphScope::Clusters;
        let out = render(&st, 100, 30);
        assert!(out.contains("Clusters"), "{out}");
        assert!(out.contains("ninox") && out.contains("bellhop"), "{out}");
        assert!(out.contains("▸ ninox"), "the top-ranked (highest count) row is selected by default: {out}");
    }

    #[test]
    fn brain_clusters_hint_is_accurate_to_the_current_scope_not_the_everything_steer() {
        let mut st = TuiState { view: View::Brain, ..Default::default() };
        st.brain = crate::tui::brain::tests::sample();
        st.brain.graph_mode = true;
        st.brain.graph_scope = crate::tui::brain::GraphScope::Clusters;
        let out = render(&st, 100, 30);
        assert!(!out.contains("Everything is GUI-only"), "already in Clusters, so steering toward it is stale: {out}");
        assert!(out.contains("filters list"), "the hint should describe what Enter does here instead: {out}");
    }

    #[test]
    fn brain_neighbourhood_hint_still_steers_toward_clusters() {
        let mut st = TuiState { view: View::Brain, ..Default::default() };
        st.brain = crate::tui::brain::tests::sample();
        st.brain.graph_mode = true;
        let out = render(&st, 100, 30);
        assert!(out.contains("Everything is GUI-only"), "Neighbourhood has no Everything option, so the steer still belongs here: {out}");
    }

    #[test]
    fn brain_clusters_table_shows_column_headers() {
        let mut st = TuiState { view: View::Brain, ..Default::default() };
        st.brain = crate::tui::brain::tests::sample();
        for e in &mut st.brain.entries {
            e.repos = vec!["zephyr".to_string()];
        }
        st.brain.graph_mode = true;
        st.brain.graph_scope = crate::tui::brain::GraphScope::Clusters;
        let out = render(&st, 100, 30);
        let header_pos = out.find("REPO").expect("column headers render above the table: {out}");
        let kind_header_pos = out.find("KIND").expect("kind column header renders: {out}");
        let row_pos = out.find("zephyr").expect("a data row renders: {out}");
        assert!(header_pos < row_pos && kind_header_pos < row_pos, "headers come before the data rows: {out}");
    }

    #[test]
    fn brain_prefers_the_bodys_first_h1_as_title_and_shows_the_normalized_kind_label() {
        let mut st = TuiState { view: View::Brain, ..Default::default() };
        st.brain.grouping = crate::tui::brain::Grouping::Flat;
        st.brain.entries = vec![ninox_core::BrainEntry {
            id: "concepts/ptyd.md".into(),
            entry_type: "concept".into(),
            name: "ptyd".into(),
            tags: vec![],
            repos: vec![],
            updated: None,
            body: "# The PTY Daemon\n\nRuns workers.\n".into(),
            source: String::new(),
        }];
        st.brain.move_cursor(0);
        let out = render(&st, 100, 20);
        assert_eq!(out.matches("The PTY Daemon").count(), 2, "the H1 becomes the row and doc title (2 places) but is stripped from the rendered body, not a 3rd repeat: {out}");
        assert!(out.contains("Concepts"), "the kind badge uses the normalized kind_label, not the raw entry_type: {out}");
    }

    #[test]
    fn markdown_reader_strips_frontmatter_and_styles_blocks() {
        let body = super::super::markdown::strip_frontmatter("---\nname: x\n---\n# Title\n- item with `code`\n```\nlet a = 1;\n```\n");
        let lines = super::super::markdown::render(body, 16, &Palette::default());
        let text: Vec<String> = lines.iter().map(|l| l.spans.iter().map(|s| s.content.to_string()).collect()).collect();
        assert_eq!(text, vec!["Title", "• item with code", "╭──────────────╮", "│ let a = 1;   │", "╰──────────────╯"]);
    }

    #[test]
    fn no_help_key_cell_is_wider_than_its_column() {
        for (_, keys) in HELP {
            for (k, _) in keys.iter() {
                assert!(k.width() <= HELP_KEY_W, "{k:?} is {} wide and runs into its description", k.width());
            }
        }
    }

    #[test]
    fn help_line_count_matches_the_lines_drawn() {
        assert_eq!(help_line_count(), help_lines(&TuiState::default()).len());
    }

    #[test]
    fn ago_formats_and_shows_a_dash_for_an_unknown_time() {
        let now = 1_793_302_252_000;
        assert_eq!(ago(now, now - 61_000), "1m");
        assert_eq!(ago(now, now + 5_000), "0s");
        assert_eq!(ago(now, 0), "—", "never set");
        assert_eq!(ago(now, 1_793_302_252), "—", "seconds stored where ms belong");
    }

    #[test]
    fn the_fleet_toolbar_drops_hints_by_priority_instead_of_clipping_one() {
        let st = fleet();
        for w in 30..200 {
            let line = fleet_toolbar_line(&st, w);
            assert!(line.width() <= w as usize, "{w} cols: {} wide", line.width());
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            for (k, what) in [("s", "sort"), ("m", "compact"), ("f", "fold island"), ("enter", "expand"), ("/", "prompt")] {
                assert_eq!(text.contains(&format!("{k} ")), text.contains(&format!("{k} {what}")), "{w} cols: a cut-off {k} hint: {text}");
            }
            let left = (w as usize).saturating_sub(line.width());
            for (k, what) in [("s", "sort"), ("m", "compact"), ("f", "fold island"), ("enter", "expand"), ("/", "prompt")] {
                let hint = format!("{k} {what}");
                // `fleet_toolbar_line` keeps one column spare at the edge.
                assert!(text.contains(&hint) || hint.width() + 2 + 1 > left, "{w} cols: {hint} would still fit: {text}");
            }
        }
    }

    #[test]
    fn the_help_modal_scrolls_when_it_is_taller_than_the_screen() {
        let mut st = st_with(&["a"]);
        st.modal = Some(Modal::Help);
        let lay = layout::compute(Rect::new(0, 0, 120, 40), &st);
        assert!(lay.help_max_scroll > 0, "the key list doesn't fit 40 rows");
        let top = render(&st, 120, 40);
        assert!(top.contains("j/k scroll") && !top.contains("Mouse"), "{top}");
        st.help_scroll = lay.help_max_scroll;
        let bottom = render(&st, 120, 40);
        assert!(bottom.contains("Mouse") && bottom.contains("wheel"), "the last group is reachable:\n{bottom}");
    }

    fn sources_state() -> TuiState {
        let mut st = TuiState { view: View::Brain, ..Default::default() };
        st.brain.sources = crate::tui::sources::tests::sample();
        st
    }

    #[test]
    fn sources_view_lists_every_source_and_dims_the_disabled_one() {
        for (w, h) in [(200, 55), (120, 40), (80, 24)] {
            let out = render(&sources_state(), w, h);
            for want in ["Brain › Sources", "3 sources", "default", "LOCAL FOLDER", "team-mlops", "GITHUB WIKI", "2 to push", "research-archive (off)", "S3", "local only"] {
                assert!(out.contains(want), "{w}x{h} missing {want:?}:\n{out}");
            }
            assert!(out.contains(" SOURCES "), "the mode pill names the sub-view:\n{out}");
        }
        let mut st = sources_state();
        st.brain.sources.catalogues[2].name = "research-archive-longer".into();
        let out = render(&st, 200, 55);
        assert!(out.contains("research-archive-longe… S3"), "a full name cell keeps a gap before KIND:\n{out}");
        let out = render(&sources_state(), 120, 40);
        assert!(out.contains("never synced · every 10 min") && out.contains("S sync all"), "{out}");
    }

    #[test]
    fn sources_detail_pane_shows_the_selected_source() {
        let mut st = sources_state();
        st.brain.sources.cursor = 1;
        st.brain.sources.detail = true;
        let out = render(&st, 120, 40);
        for want in ["Wiki URL", "Local cache", "Pending", "2 local change(s) to push", "ambient git credentials", "e edit · s sync"] {
            assert!(out.contains(want), "missing {want:?}:\n{out}");
        }
        st.brain.sources.start_sync(&["team-mlops".into()]);
        let out = render(&st, 80, 24);
        assert!(out.contains("syncing…"), "SYNC keeps its column beside the detail pane at 80 cols:\n{out}");
    }

    #[test]
    fn credentialed_and_clone_form_wiki_urls_render_as_wikis() {
        for remote in ["https://bob:tok@github.com/acme/handbook/wiki", "https://github.com/acme/handbook.wiki.git"] {
            let mut st = sources_state();
            st.brain.sources.catalogues[1].remote = Some(remote.into());
            st.brain.sources.cursor = 1;
            st.brain.sources.detail = true;
            let out = render(&st, 120, 40);
            for want in ["GITHUB WIKI", "Wiki URL", "ambient git credentials"] {
                assert!(out.contains(want), "{remote}: missing {want:?}:\n{out}");
            }
            assert!(!out.contains("s3://bucket/prefix"), "{remote}:\n{out}");
        }
    }

    #[test]
    fn sources_view_and_form_never_show_a_credential() {
        use crate::tui::sources::{tests::source, SourceForm, SyncDone};
        let mut st = sources_state();
        let mut leaky = source("leaky", Some("s3://bucket/brain"), true);
        leaky.endpoint = Some("https://AKIA:SECRET1@r2.example.com/?X-Amz-Signature=SECRET2".into());
        let wiki = source("wiki", Some("https://bob:SECRET3@github.com/acme/brain/wiki"), true);
        let mut cats = st.brain.sources.catalogues.clone();
        cats.extend([leaky.clone(), wiki.clone()]);
        st.brain.sources.set_catalogues(cats, Vec::new());
        st.brain.sources.start_sync(&["wiki".into()]);
        let failed = SyncDone { name: "wiki".into(), path: wiki.path.clone(), result: Err("fetch https://bob:SECRET4@github.com/acme/brain.wiki.git: 403".into()) };
        let _ = st.brain.sources.sync_done(&failed, "09:41");

        let mut screens = vec![render(&st, 200, 55)];
        for i in 3..5 {
            st.brain.sources.cursor = i;
            st.brain.sources.detail = true;
            screens.push(render(&st, 200, 55));
        }
        for c in [&leaky, &wiki] {
            let mut form = SourceForm::edit(c);
            for _ in 0..form.fields().len() {
                st.modal = Some(Modal::SourceForm(form.clone()));
                screens.push(render(&st, 200, 55));
                form.next_focus(false);
            }
        }
        for out in &screens {
            assert!(!out.contains("SECRET"), "a credential reached the screen:\n{out}");
        }
        assert!(screens.iter().any(|s| s.contains("bob:••••@github.com")), "the masked URL is still shown");
    }

    #[test]
    fn source_form_shows_the_type_specific_fields_and_the_error() {
        use crate::tui::sources::SourceForm;
        let mut st = sources_state();
        let mut form = SourceForm::add();
        form.form.source_type = ninox_ui_shared::brain_sources::SourceType::S3;
        form.form.error = Some("S3 location must start with s3://".into());
        st.modal = Some(Modal::SourceForm(form));
        let out = render(&st, 120, 40);
        for want in ["add source", "Region", "Endpoint", "Local cache", "AWS profile/SSO", "✗ S3 location must start with s3://", "↵ save"] {
            assert!(out.contains(want), "missing {want:?}:\n{out}");
        }
    }

    #[test]
    fn remove_source_confirm_says_the_remote_is_untouched() {
        let mut st = sources_state();
        st.modal = Some(Modal::Confirm(Pending::RemoveBrainSource("team-mlops".into())));
        let out = render(&st, 120, 40);
        assert!(out.contains("Remove brain source team-mlops?") && out.contains("remote and its local cache are not touched"), "{out}");
    }
}
