//! Drawing. Pure over `TuiState` + the frame's `Layout`.
//!
//! Field Notes in a terminal: paper chrome, ink text, one vermilion accent
//! for "this needs you" and focus, status colour only on status glyphs.
//! Structure comes from whitespace and single rules, not boxes.

use ninox_core::types::{ActivityState, GateCheck, SessionStatus};
use ratatui::{
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use super::keys::prefix_label;
use super::layout::{Layout, CHEVRON_W};
use super::palette::Palette;
use super::state::{
    Attention, Backend, Focus, Level, Modal, Mode, Pending, PtydState, Row, Section, SideItem, TuiState, View, HELP,
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
    }
}

fn activity_label(a: ActivityState) -> &'static str {
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

pub fn ago(now_ms: i64, t_ms: i64) -> String {
    let s = ((now_ms - t_ms) / 1000).max(0);
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
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
/// line saying how to fix it.
fn empty_state(f: &mut Frame, area: Rect, p: &Palette, title: &str, hint: Vec<Span<'static>>) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let lines = vec![
        Line::from(Span::styled("⬡", Style::default().fg(p.accent))),
        Line::from(""),
        Line::from(Span::styled(title.to_string(), Style::default().fg(p.ink).add_modifier(Modifier::BOLD))),
        Line::from(hint),
    ];
    let h = (lines.len() as u16).min(area.height);
    let y = area.y + (area.height - h) / 2;
    f.render_widget(
        Paragraph::new(lines).alignment(Alignment::Center).wrap(Wrap { trim: true }),
        Rect { y, height: area.bottom() - y, ..area },
    );
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
    f.render_widget(Block::default().style(Style::default().bg(p.paper).fg(p.ink)), f.area());
    draw_header(f, lay, st);
    let mut cursor = None;
    match st.view {
        View::Board => cursor = draw_board(f, st, lay),
        View::Overview => draw_overview(f, st, lay),
        View::PrWatches => draw_prs(f, lay, st),
        View::Brain => draw_brain(f, lay, st),
        View::Settings => draw_settings(f, lay, st),
    }
    draw_footer(f, lay.footer, st);
    if let Some(modal) = &st.modal {
        draw_modal(f, f.area(), st, modal);
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
            (Style::default().fg(p.accent), Style::default().fg(p.ink).add_modifier(Modifier::BOLD), p.card)
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

    let dot = |up: bool| Style::default().fg(if up { p.working } else { p.ci_failed });
    let health = vec![
        Span::styled("ptyd ", Style::default().fg(p.faint)),
        Span::styled("●", dot(st.ptyd == PtydState::Up)),
        Span::styled(" engine ", Style::default().fg(p.faint)),
        Span::styled("●", dot(st.daemon_up)),
        Span::raw(" "),
    ];
    let live = st.rows.iter().filter(|r| !r.session.status.is_terminal()).count();
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
    counts.push(Span::raw("   "));
    let text = Rect { x: lay.header_text_x, width: area.right().saturating_sub(lay.header_text_x), ..area };
    let mut right = counts;
    right.extend(health.clone());
    let right = if spans_width(&right) < text.width as usize { right } else { health };
    f.render_widget(Paragraph::new(Line::from(right).alignment(Alignment::Right)), text);
}

fn draw_footer(f: &mut Frame, area: Rect, st: &TuiState) {
    let p = &st.palette;
    let pre = prefix_label(st.prefix);
    let (pill, fg, bg) = match st.mode {
        Mode::Prefix => (" PREFIX ", p.paper, p.accent),
        Mode::Scroll => (" SCROLL ", p.paper, p.review),
        Mode::Normal => match (st.view, st.focus) {
            (View::Board, Focus::Pane) if st.report_shown() => (" REPORT ", p.ink, p.rule),
            (View::Board, Focus::Pane) => (" AGENT ", p.paper, p.working),
            (View::Board, _) => (" FLEET ", p.ink, p.rule),
            (View::Overview, _) => (" OVERVIEW ", p.ink, p.rule),
            (View::Brain, _) => (" BRAIN ", p.ink, p.rule),
            (View::PrWatches, _) => (" PRS ", p.ink, p.rule),
            (View::Settings, _) => (" SETTINGS ", p.ink, p.rule),
        },
    };
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(pill, Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
    ];
    if let Some(n) = &st.notice {
        let color = match n.level {
            Level::Info => p.ink_2,
            Level::Warn => p.review,
            Level::Error => p.ci_failed,
        };
        spans.push(Span::styled("● ", Style::default().fg(color)));
        spans.push(Span::styled(n.text.clone(), Style::default().fg(p.ink)));
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
            (_, View::Overview, _) => vec![("arrows".into(), "move"), ("↵".into(), "open"), ("esc".into(), "back")],
            (_, View::PrWatches, _) if st.prs.rows.is_empty() => vec![("1-5".into(), "tabs"), ("esc".into(), "back"), ("?".into(), "keys")],
            (_, View::PrWatches, _) => vec![
                ("j/k".into(), "move"),
                ("↵/o".into(), "open in browser"),
                ("s".into(), "show session"),
                ("esc".into(), "back"),
                ("?".into(), "keys"),
            ],
            (_, View::Brain, _) if st.brain.editing_query => vec![("↵".into(), "search"), ("esc".into(), "stop typing")],
            (_, View::Brain, _) if st.brain.open => {
                vec![("j/k".into(), "scroll"), ("e".into(), "edit"), ("h".into(), "back to list"), ("/".into(), "search"), ("esc".into(), "close")]
            }
            (_, View::Brain, _) => vec![
                ("j/k".into(), "move"),
                ("↵".into(), "read"),
                ("space".into(), "fold"),
                ("t".into(), "group"),
                ("e".into(), "edit"),
                ("a".into(), "new"),
                ("D".into(), "delete"),
                ("/".into(), "search"),
            ],
            (_, View::Board, focus) if st.report_shown() => {
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
            (_, View::Board, Focus::Pane) => vec![
                ("typing".into(), "goes to the agent"),
                ("Ctrl+]".into(), "back to the fleet"),
                ("Ctrl+] then x".into(), "kill"),
                ("Ctrl+] then ?".into(), "all keys"),
            ],
            (_, View::Settings, _) if st.settings.editing.is_some() => {
                vec![("↵".into(), "save"), ("esc".into(), "cancel"), ("Ctrl+u".into(), "clear")]
            }
            (_, View::Settings, _) => vec![
                ("j/k".into(), "move"),
                ("↵/space".into(), "change"),
                ("←/→".into(), "cycle"),
                ("e".into(), "open in $EDITOR"),
                ("esc".into(), "back"),
            ],
            (_, View::Board, _) if st.rows.is_empty() => vec![("n".into(), "spawn an orchestrator"), ("?".into(), "keys"), ("q".into(), "detach")],
            (_, View::Board, _) => {
                let mut v = vec![("↵".into(), "open"), ("j/k".into(), "move"), ("x".into(), "kill/remove")];
                if !st.needs_rows().is_empty() {
                    v.push(("!".into(), "needs you"));
                }
                v.extend([("space".into(), "fold"), ("g".into(), "go to"), ("n".into(), "new"), ("?".into(), "keys")]);
                v
            }
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

fn session_line(st: &TuiState, idx: usize, width: usize, kind: LineKind) -> Line<'static> {
    let p = &st.palette;
    let Some(r) = st.rows.get(idx) else { return Line::from("") };
    let s = &r.session;
    let terminal = s.status.is_terminal() && s.status != SessionStatus::Interrupted;
    let header = kind == LineKind::Tree && r.is_orchestrator;
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
            left.push(Span::styled(if folded { "▸ " } else { "▾ " }, Style::default().fg(p.faint)));
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
        name_style = Style::default().fg(p.faint);
    }
    left.push(Span::styled(name, name_style));
    spread(left, right, width)
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
            let line = session_line(st, *row, w - close.map_or(0, |c| c.width as usize), kind);
            if selected {
                let row_rect = Rect { x: side.x, y, width: area.right() + 1 - side.x, height: 1 };
                fill(f, row_rect, p.card);
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
        empty_state(f, list, p, "No sessions yet", key_hint(p, "n", "spawns an orchestrator"));
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

fn draw_board(f: &mut Frame, st: &TuiState, lay: &Layout) -> Option<ratatui::layout::Position> {
    let p = &st.palette;
    draw_sidebar(f, st, lay);
    if let Some(insp) = lay.inspector {
        draw_inspector(f, insp, st);
    }
    let (Some(outer), Some(inner)) = (lay.pane, lay.pane_inner) else { return None };
    let focused = st.focus == Focus::Pane;
    let Some(row) = st.selected_row() else {
        empty_state(f, outer, p, "No agents running yet", {
            let mut h = key_hint(p, "n", "spawns an orchestrator");
            h.push(Span::styled("  ·  from an agent pane: Ctrl+] then n", Style::default().fg(p.faint)));
            h
        });
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
                empty_state(f, inner, p, &msg, hint);
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

fn draw_inspector(f: &mut Frame, area: Rect, st: &TuiState) {
    let p = &st.palette;
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(p.rule))
        .padding(ratatui::widgets::Padding::horizontal(1))
        .title(Span::styled(" INSPECTOR", Style::default().fg(p.faint).add_modifier(Modifier::BOLD)));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Some(r) = st.selected_row() else { return };
    let s = &r.session;
    let kv = |k: &str, v: String| {
        Line::from(vec![
            Span::styled(format!("{:<10}", k.to_uppercase()), Style::default().fg(p.faint)),
            Span::styled(v, Style::default().fg(p.ink)),
        ])
    };
    let mut lines = vec![
        Line::from(""),
        kv("id", s.id.clone()),
        kv("name", s.name.clone()),
        kv(
            "role",
            if r.is_orchestrator {
                "orchestrator".into()
            } else {
                r.group.as_ref().map(|g| format!("worker of {g}")).unwrap_or_else(|| "standalone".into())
            },
        ),
        kv("status", r.status().to_string()),
        kv(
            "activity",
            format!(
                "{}{}",
                activity_label(s.activity),
                s.activity_since.map(|t| format!(" for {}", ago(st.now_ms, t))).unwrap_or_default()
            ),
        ),
    ];
    if let Some(n) = &s.activity_note {
        lines.push(kv("note", n.clone()));
    }
    lines.push(kv("repo", s.repo.clone()));
    if let Some(w) = &s.workspace_path {
        lines.push(kv("workspace", w.clone()));
    }
    if let Some(pr) = s.pr_number {
        lines.push(kv("pr", format!("#{pr}")));
    }
    if let Some(g) = &s.gate_status {
        lines.push(kv("gate", format!("ci {:?} · review {:?} · merge {:?}", g.ci, g.review, g.mergeable).to_lowercase()));
    }
    lines.push(kv("cost", format!("${:.2}", s.cost_usd)));
    if let Some(pct) = s.context_used_pct {
        lines.push(kv("context", format!("{pct:.0}%")));
    }
    lines.push(kv("harness", format!("{}{}", s.agent_type, s.model.as_ref().map(|m| format!(" · {m}")).unwrap_or_default())));
    if s.started_at > 0 {
        lines.push(kv("started", format!("{} ago", ago(st.now_ms, s.started_at))));
    }
    lines.push(kv(
        "backend",
        match st.panes.get(r.id()) {
            Some(pi) => format!(
                "ptyd · pid {} · {}x{}{}",
                pi.pid,
                pi.cols,
                pi.rows,
                if pi.alive { String::new() } else { format!(" · exited {:?}", pi.exit_code) }
            ),
            None => "tmux".into(),
        },
    ));
    if let Some(t) = st.panes.get(r.id()).and_then(|pi| pi.title.clone()) {
        lines.push(kv("title", t));
    }
    let unread = st.unread(r.id());
    if unread > 0 {
        lines.push(kv("unread", format!("{unread} messages")));
    }
    if let Some(sum) = &s.summary {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(sum.clone(), Style::default().fg(p.ink_2))));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn draw_overview(f: &mut Frame, st: &TuiState, lay: &Layout) {
    let p = &st.palette;
    let rows = st.overview_rows();
    if lay.tiles.is_empty() {
        if rows.is_empty() {
            empty_state(f, lay.body, p, "No live agents to watch", key_hint(p, "n", "spawns an orchestrator · esc back to the fleet"));
        } else {
            empty_state(f, lay.body, p, "Too small for the overview", key_hint(p, "esc", "back to the fleet"));
        }
        return;
    }
    for t in &lay.tiles {
        let Some(&idx) = rows.get(t.index) else { continue };
        let Some(row) = st.rows.get(idx) else { continue };
        let selected = t.index == st.overview_sel;
        let title_w = t.outer.width.saturating_sub(4) as usize;
        let mut block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(if selected { p.accent } else { p.rule }))
            .title(pane_header(st, row, title_w, &Layout::default(), selected, None));
        if let Some(need) = st.needs_you(row) {
            block = block.title_bottom(Line::from(Span::styled(format!(" ⚑ {} ", need.label()), Style::default().fg(p.accent))).right_aligned());
        }
        f.render_widget(block, t.outer);
        let target = st.pane_target(row.id()).unwrap_or_else(|| row.id().to_string());
        match st.views.get(&target).or_else(|| st.views.get(row.id())) {
            Some(v) if st.backend_of(row.id()) != Backend::Legacy || v.live.is_some() || v.checkpoint.is_some() => {
                v.render(t.inner, f.buffer_mut(), false);
            }
            _ => {
                let label = match st.backend_of(row.id()) {
                    Backend::Legacy if row.session.status.is_terminal() => "tmux session — not running",
                    Backend::Legacy => "tmux session — starting view…",
                    _ => "connecting…",
                };
                f.render_widget(Paragraph::new(Span::styled(label, Style::default().fg(p.faint))), t.inner);
            }
        }
    }
}

fn pr_state_color(r: &super::prs::PrRow, p: &Palette) -> Color {
    match &r.status {
        None => p.faint,
        Some(SessionStatus::CiFailed) => p.ci_failed,
        Some(SessionStatus::ReviewPending) => p.review,
        Some(SessionStatus::Mergeable) => p.mergeable,
        Some(SessionStatus::Done) => p.done,
        Some(SessionStatus::Terminated | SessionStatus::Interrupted) => p.faint,
        Some(_) => p.pr_open,
    }
}

fn draw_prs(f: &mut Frame, lay: &Layout, st: &TuiState) {
    use super::prs::{state_word, Line as PrLine};
    let p = &st.palette;
    let Some(pl) = &lay.prs else { return };
    let v = &st.prs;
    let faint = Style::default().fg(p.faint);
    let watching = match v.watching {
        Some(true) => vec![Span::styled("PR watching ", faint), Span::styled("on", Style::default().fg(p.working))],
        Some(false) => vec![
            Span::styled("PR watching off", Style::default().fg(p.review)),
            Span::styled(" — session PRs only; [pr_watch] enabled = true adds ninox open --pr watches", faint),
        ],
        None => Vec::new(),
    };
    let n = v.rows.len();
    let open = v.rows.iter().filter(|r| !matches!(state_word(r), "merged" | "session ended" | "watching")).count();
    let counts = vec![Span::styled(format!("{n} PR{} · {open} open", if n == 1 { "" } else { "s" }), Style::default().fg(p.ink_2))];
    let summary = if spans_width(&counts) + spans_width(&watching) + 2 <= pl.summary.width as usize { spread(counts, watching, pl.summary.width as usize) } else { Line::from(counts) };
    f.render_widget(Paragraph::new(summary), pl.summary);
    if pl.summary.height > 0 && lay.body.height > 2 {
        let under = Rect { y: pl.summary.y + 1, ..pl.summary };
        f.render_widget(Paragraph::new(Span::styled("─".repeat(under.width as usize), Style::default().fg(p.rule))), under);
    }
    if v.rows.is_empty() {
        let mut hint = vec![Span::styled("a worker's PR shows up here once it opens one · ", faint)];
        hint.extend(key_hint(p, "ninox open --pr <url>", ""));
        hint.push(Span::styled(if v.watching == Some(true) { "watches any other" } else { "watches any other, once [pr_watch] enabled = true" }, faint));
        empty_state(f, pl.list, p, "No PRs yet", hint);
        return;
    }
    let sel = v.selected();
    let button = Style::default().fg(p.ink).bg(p.paper_2).add_modifier(Modifier::BOLD);
    for (k, line) in pl.lines.iter().enumerate().skip(pl.offset).take(pl.list.height as usize) {
        let y = pl.list.y + (k - pl.offset) as u16;
        let row = Rect { y, height: 1, ..pl.list };
        match line {
            PrLine::Gap => {}
            PrLine::Repo { repo, count } => {
                let line = Line::from(vec![
                    Span::styled(truncate(repo, row.width.saturating_sub(6) as usize), Style::default().fg(p.ink_2).add_modifier(Modifier::BOLD)),
                    Span::styled(format!("  ({count})"), faint),
                ]);
                f.render_widget(Paragraph::new(line), row);
            }
            PrLine::Pr(i) => {
                let Some(r) = v.rows.get(*i) else { continue };
                let selected = *i == sel;
                if selected {
                    let bg = Rect { x: row.x.saturating_sub(1), width: row.width + 1, ..row };
                    fill(f, bg, p.card);
                    f.render_widget(Paragraph::new(Span::styled("▌", Style::default().fg(p.accent))), Rect { width: 1, ..bg });
                }
                let stop = [pl.sessions.iter().find(|(j, _)| j == i), pl.opens.iter().find(|(j, _)| j == i)]
                    .into_iter()
                    .flatten()
                    .map(|(_, r)| r.x)
                    .min()
                    .unwrap_or(row.right());
                let room = stop.saturating_sub(row.x + 1) as usize;
                let num_w = super::layout::PR_NUMBER_W as usize;
                let mut left = vec![
                    Span::raw("  "),
                    Span::styled(format!("{:<num_w$}", format!("#{}", r.number)), Style::default().fg(p.accent).add_modifier(Modifier::BOLD | Modifier::UNDERLINED)),
                ];
                left.push(Span::styled(format!("● {:<14}", state_word(r)), Style::default().fg(pr_state_color(r, p))));
                if let Some(g) = &r.gate {
                    for (label, check) in [("CI", &g.ci), ("review", &g.review), ("merge", &g.mergeable)] {
                        let (glyph, color) = ci_glyph(check, p).unwrap_or(("·", p.faint));
                        left.push(Span::styled(format!("{glyph} "), Style::default().fg(color)));
                        left.push(Span::styled(format!("{label}  "), faint));
                    }
                }
                if r.watched && r.status.is_some() {
                    left.push(Span::styled("watched  ", faint));
                }
                let title = r.title.clone().unwrap_or_default();
                let mut title_style = Style::default().fg(p.ink);
                if selected {
                    title_style = title_style.add_modifier(Modifier::BOLD);
                }
                let used = spans_width(&left);
                // Gate columns give way before the title disappears.
                if used + 12 > room {
                    left.truncate(3);
                }
                let used = spans_width(&left);
                left.push(Span::styled(truncate(&title, room.saturating_sub(used)), title_style));
                f.render_widget(Paragraph::new(Line::from(left)), Rect { width: room as u16, ..row });
                if let Some((_, rect)) = pl.sessions.iter().find(|(j, _)| j == i) {
                    let name = r.session_name.clone().or_else(|| r.session.clone()).unwrap_or_default();
                    f.render_widget(Paragraph::new(Span::styled(truncate(&name, rect.width as usize), Style::default().fg(p.ink_2))), *rect);
                }
                if let Some((_, rect)) = pl.opens.iter().find(|(j, _)| j == i) {
                    f.render_widget(Paragraph::new(Span::styled(super::layout::PR_OPEN_LABEL, button)), *rect);
                }
            }
        }
    }
}

/// A plain terminal rendering of markdown: headings, bullets, quotes, code.
pub fn markdown_lines(body: &str, p: &Palette) -> Vec<Line<'static>> {
    let mut text = body;
    if let Some(rest) = text.strip_prefix("---\n") {
        if let Some(end) = rest.find("\n---") {
            text = rest[end + 4..].trim_start_matches(['\n', '-']);
        }
    }
    let mut out = Vec::new();
    let mut in_code = false;
    for raw in text.lines() {
        if raw.trim_start().starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            out.push(Line::from(Span::styled(format!("  {raw}"), Style::default().fg(p.ink_2))));
            continue;
        }
        let line = if let Some(h) = raw.strip_prefix("# ") {
            Line::from(Span::styled(h.to_string(), Style::default().fg(p.ink).add_modifier(Modifier::BOLD | Modifier::UNDERLINED)))
        } else if let Some(h) = raw.strip_prefix("## ") {
            Line::from(Span::styled(h.to_string(), Style::default().fg(p.accent).add_modifier(Modifier::BOLD)))
        } else if let Some(h) = raw.strip_prefix("### ").or_else(|| raw.strip_prefix("#### ")) {
            Line::from(Span::styled(h.to_string(), Style::default().add_modifier(Modifier::BOLD)))
        } else if let Some(q) = raw.strip_prefix("> ") {
            Line::from(Span::styled(format!("│ {q}"), Style::default().fg(p.ink_2).add_modifier(Modifier::ITALIC)))
        } else {
            let trimmed = raw.trim_start();
            let indent = &raw[..raw.len() - trimmed.len()];
            let (lead, rest) = match trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")) {
                Some(rest) => (format!("{indent}• "), rest),
                None => (indent.to_string(), trimmed),
            };
            let mut spans = vec![Span::raw(lead)];
            spans.extend(inline_spans(rest, p));
            Line::from(spans)
        };
        out.push(line);
    }
    out
}

fn inline_spans(text: &str, p: &Palette) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let (mut code, mut bold) = (false, false);
    let mut buf = String::new();
    let mut chars = text.chars().peekable();
    let flush = |buf: &mut String, spans: &mut Vec<Span<'static>>, code: bool, bold: bool| {
        if buf.is_empty() {
            return;
        }
        let mut style = Style::default();
        if code {
            style = style.fg(p.ink_2);
        }
        if bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        spans.push(Span::styled(std::mem::take(buf), style));
    };
    while let Some(c) = chars.next() {
        if c == '`' {
            flush(&mut buf, &mut spans, code, bold);
            code = !code;
        } else if c == '*' && !code && chars.peek() == Some(&'*') {
            chars.next();
            flush(&mut buf, &mut spans, code, bold);
            bold = !bold;
        } else {
            buf.push(c);
        }
    }
    flush(&mut buf, &mut spans, code, bold);
    spans
}

fn draw_brain(f: &mut Frame, lay: &Layout, st: &TuiState) {
    use super::brain::{Grouping, Item, UNTAGGED};
    let p = &st.palette;
    let Some(bl) = &lay.brain else { return };
    let b = &st.brain;
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
            vec![Span::styled(format!("{n} entr{} · {}", if n == 1 { "y" } else { "ies" }, b.grouping.label()), Style::default().fg(p.faint))]
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
        empty_state(f, whole, p, "The brain is unavailable", vec![Span::styled(e.clone(), Style::default().fg(p.ci_failed))]);
        return;
    }
    if bl.items.is_empty() {
        if b.entries.is_empty() {
            empty_state(f, whole, p, "The brain is empty", {
                let mut h = key_hint(p, "a", "adds an entry");
                h.push(Span::styled(" · agents file notes with ", Style::default().fg(p.faint)));
                h.extend(key_hint(p, "ninox brain add", ""));
                h
            });
        } else {
            empty_state(f, whole, p, "No entries match", key_hint(p, "/", "edits the search"));
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
            fill(f, row, p.card);
            if list_focused {
                f.render_widget(Paragraph::new(Span::styled("▌", Style::default().fg(p.accent))), Rect { width: 1, ..row });
            }
        }
        let line = match item {
            Item::Group { key, count, folded } => {
                let style = if key == UNTAGGED { Style::default().fg(p.faint) } else { Style::default().fg(p.ink_2) };
                let count = format!("  ({count})");
                Line::from(vec![
                    Span::styled(if *folded { "▸ " } else { "▾ " }, Style::default().fg(p.faint)),
                    Span::styled(truncate(key, list_w.saturating_sub(CHEVRON_W as usize + count.len())), style.add_modifier(Modifier::BOLD)),
                    Span::styled(count, Style::default().fg(p.faint)),
                ])
            }
            Item::Entry { idx, .. } if searching => {
                let Some(e) = b.entries.get(*idx) else { continue };
                let hit = hits.get(idx);
                let mut spans = vec![
                    Span::styled(if hit.is_some_and(|h| h.related) { " ≈ " } else { " " }, Style::default().fg(p.faint)),
                    Span::styled(truncate(&e.name, list_w.saturating_sub(3).max(1)), Style::default().fg(p.ink).add_modifier(if selected { Modifier::BOLD } else { Modifier::empty() })),
                ];
                if let Some((n, text)) = hit.and_then(|h| h.line.as_ref()) {
                    let room = list_w.saturating_sub(spans_width(&spans) + 2);
                    if room > 6 {
                        spans.push(Span::styled(format!("  {n}:"), Style::default().fg(p.faint)));
                        let room = room.saturating_sub(format!("{n}:").len());
                        spans.extend(highlight_terms(&truncate(text, room), &b.query, p));
                    }
                }
                Line::from(spans)
            }
            Item::Entry { group, idx } => {
                let Some(e) = b.entries.get(*idx) else { continue };
                let indent = if group.is_some() { "    " } else { " " };
                let kind = if b.grouping == Grouping::Type { Vec::new() } else { vec![Span::styled(e.entry_type.clone(), Style::default().fg(p.faint))] };
                let name_w = list_w.saturating_sub(spans_width(&kind) + indent.len() + 1).max(list_w.min(NAME_MIN));
                let mut name_style = Style::default().fg(p.ink);
                if selected {
                    name_style = name_style.add_modifier(Modifier::BOLD);
                }
                spread(vec![Span::raw(indent), Span::styled(truncate(&e.name, name_w), name_style)], kind, list_w)
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
                Line::from(Span::styled(e.name.clone(), Style::default().fg(p.ink).add_modifier(Modifier::BOLD))),
                Line::from(Span::styled(
                    format!("{} · {} · {tags}{}", e.id, e.entry_type, e.updated.as_ref().map(|u| format!(" · {u}")).unwrap_or_default()),
                    Style::default().fg(p.faint),
                )),
                Line::from(""),
            ];
            lines.extend(markdown_lines(&e.body, p));
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).scroll((b.scroll, 0)), bl.doc_body);
        }
        Some(Item::Group { key, count, .. }) => {
            let what = match b.grouping {
                Grouping::Tag if key == UNTAGGED => "untagged".to_string(),
                Grouping::Tag => format!("tagged #{key}"),
                _ => format!("of type {key}"),
            };
            let mut lines = vec![
                Line::from(Span::styled(format!("{count} entr{} {what}", if *count == 1 { "y" } else { "ies" }), Style::default().fg(p.ink).add_modifier(Modifier::BOLD))),
                Line::from(""),
            ];
            let members = b.entries.iter().filter(|e| match b.grouping {
                Grouping::Tag if key == UNTAGGED => e.tags.is_empty(),
                Grouping::Tag => e.tags.contains(key),
                _ => &e.entry_type == key,
            });
            for e in members {
                lines.push(Line::from(vec![Span::styled("• ", Style::default().fg(p.faint)), Span::styled(e.name.clone(), Style::default().fg(p.ink_2))]));
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
fn highlight_terms(text: &str, query: &str, p: &super::palette::Palette) -> Vec<Span<'static>> {
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
    let (plain, hit) = (Style::default().fg(p.faint), Style::default().fg(p.accent).add_modifier(Modifier::BOLD));
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
    let label_w = super::layout::SETTINGS_LABEL_W as usize;
    let w = sl.list.width as usize;
    for (k, line) in sl.lines.iter().skip(sl.offset).take(sl.list.height as usize).enumerate() {
        let r = Rect { y: sl.list.y + k as u16, height: 1, ..sl.list };
        let text = match *line {
            SettingsLine::Gap => continue,
            SettingsLine::Heading(h) => Line::from(Span::styled(h.to_uppercase(), faint.add_modifier(Modifier::BOLD))),
            SettingsLine::Error => {
                let e = v.error.clone().unwrap_or_default();
                Line::from(Span::styled(truncate(&format!("{}✗ {e}", " ".repeat(label_w + 2)), w), Style::default().fg(p.ci_failed)))
            }
            SettingsLine::Field(i) => {
                let Some(fd) = v.fields.get(i) else { continue };
                let sel = i == v.selected;
                if sel {
                    fill(f, r, p.card);
                }
                let label_style = if sel { Style::default().fg(p.ink).add_modifier(Modifier::BOLD) } else { Style::default().fg(p.ink_2) };
                let bar = if sel { Span::styled("▌ ", Style::default().fg(p.accent)) } else { Span::raw("  ") };
                let mut spans = vec![bar, Span::styled(format!("{:<label_w$}", truncate(&fd.label, label_w)), label_style)];
                let vw = w.saturating_sub(label_w + 2);
                match (fd.kind, sel.then_some(v.editing.as_ref()).flatten()) {
                    (_, Some(buf)) => {
                        let shown = truncate(&format!("{buf}▏"), vw);
                        spans.push(Span::styled(shown, Style::default().fg(p.ink).bg(p.paper_2).add_modifier(Modifier::BOLD)));
                    }
                    (Kind::Toggle, None) => {
                        let on = fd.value == "on";
                        let (g, c) = if on { ("● on", p.working) } else { ("○ off", p.faint) };
                        spans.push(Span::styled(g, Style::default().fg(c).add_modifier(if on { Modifier::BOLD } else { Modifier::empty() })));
                        if fd.locked {
                            spans.push(Span::styled("  always", faint));
                        }
                    }
                    (Kind::Choice, None) => {
                        let shown = truncate(&fd.value, vw.saturating_sub(4));
                        if sel {
                            spans.push(Span::styled("‹ ", faint));
                            spans.push(Span::styled(shown, Style::default().fg(p.accent).add_modifier(Modifier::BOLD)));
                            spans.push(Span::styled(" ›", faint));
                        } else {
                            spans.push(Span::styled(shown, Style::default().fg(p.ink)));
                        }
                    }
                    (Kind::Text, None) if fd.value.is_empty() => spans.push(Span::styled("harness default", faint)),
                    (Kind::Text, None) => spans.push(Span::styled(truncate(&fd.value, vw), Style::default().fg(p.ink))),
                    (Kind::Action, None) => spans.push(Span::styled(if sel { "↵ open" } else { "" }, Style::default().fg(p.accent))),
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

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
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

fn draw_modal(f: &mut Frame, area: Rect, st: &TuiState, modal: &Modal) {
    let p = &st.palette;
    let faint = Style::default().fg(p.faint);
    match modal {
        Modal::Confirm(pending) => {
            let (q, detail) = st.confirm_text(pending);
            let (r, yes, no, all) = super::layout::confirm_rects(area, q.width().max(detail.width()) as u16, st.confirm_offers_all(pending));
            let title = match pending {
                Pending::Kill(_) => "kill",
                Pending::Remove(_) => "remove",
                Pending::Reap(_) => "reap",
                Pending::DeleteBrain(_) => "delete",
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
            if let Some(all) = all {
                buttons.push(Span::raw(" ".repeat((all.x.saturating_sub(no.right())) as usize)));
                buttons.extend(button("a", "Remove all", false));
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
        Modal::Restore(s) => {
            let text = format!("Restore fleet ({} workers, {} orchestrators)?  y / n", s.workers, s.orchestrators);
            let r = centered(area, text.width() as u16 + 4, 3);
            f.render_widget(Clear, r);
            f.render_widget(Paragraph::new(text).block(modal_block(p, "fleet restore")), r);
        }
        Modal::Spawn(m) => {
            let r = centered(area, (area.width * 2 / 3).max(44), 6);
            let active = Style::default().fg(p.ink).add_modifier(Modifier::BOLD);
            let idle = Style::default().fg(p.ink_2);
            let (ns, ps) = if m.on_prompt_field { (idle, active) } else { (active, idle) };
            let caret = |on: bool| if on { "▏" } else { "" };
            let label = |on: bool| Style::default().fg(if on { p.accent } else { p.faint });
            let lines = vec![
                Line::from(vec![Span::styled("name   ", label(!m.on_prompt_field)), Span::styled(format!("{}{}", m.name, caret(!m.on_prompt_field)), ns)]),
                Line::from(vec![Span::styled("brief  ", label(m.on_prompt_field)), Span::styled(format!("{}{}", m.prompt, caret(m.on_prompt_field)), ps)]),
                Line::from(""),
                Line::from(Span::styled("tab switch field · ↵ next / spawn · esc cancel", faint)),
            ];
            f.render_widget(Clear, r);
            f.render_widget(Paragraph::new(lines).block(modal_block(p, "spawn an orchestrator")), r);
        }
        Modal::Help => {
            let pre = prefix_label(st.prefix);
            let mut lines = vec![Line::from(vec![
                Span::styled("prefix ", faint),
                Span::styled(pre, Style::default().fg(p.accent).add_modifier(Modifier::BOLD)),
                Span::styled("  (change it in Settings, 5)   ", faint),
                Span::styled("Ctrl+]", Style::default().fg(p.accent).add_modifier(Modifier::BOLD)),
                Span::styled(" always returns to the fleet", faint),
                Span::styled(format!("   ninox v{}", env!("CARGO_PKG_VERSION")), faint),
            ])];
            for (group, keys) in HELP {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(group.to_string(), Style::default().fg(p.accent).add_modifier(Modifier::BOLD))));
                for (k, v) in keys.iter() {
                    lines.push(Line::from(vec![
                        Span::styled(format!("  {k:<34}"), Style::default().fg(p.ink).add_modifier(Modifier::BOLD)),
                        Span::styled(*v, Style::default().fg(p.ink_2)),
                    ]));
                }
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("any key closes", faint)));
            let r = centered(area, 104, lines.len() as u16 + 2);
            f.render_widget(Clear, r);
            f.render_widget(Paragraph::new(lines).block(modal_block(p, "keys")), r);
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
                let mut line = session_line(st, i, inner.width as usize, LineKind::Flat);
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::layout;
    use crate::tui::pane::{tests::snap, PaneView};
    use crate::tui::state::{Row, TuiState};

    fn render(st: &TuiState, w: u16, h: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let lay = layout::compute(ratatui::layout::Rect::new(0, 0, w, h), st);
        terminal.draw(|f| draw(f, st, &lay)).unwrap();
        let buf = terminal.backend().buffer().clone();
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
    fn board_draws_sidebar_rows_and_the_focused_live_pane() {
        let mut st = st_with(&["alpha", "beta"]);
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
        assert!(out.contains("FLEET"), "{out}");
        assert!(out.contains("alpha") && out.contains("beta"));
        assert!(out.contains("hello from agent"));
        assert!(out.contains("tmux"), "beta is not in ptyd → tagged tmux");
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
        let title = out.lines().nth(1).unwrap();
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
    fn settings_render_as_an_editable_form() {
        let mut st = st_with(&["alpha"]);
        st.view = View::Settings;
        st.settings.reload(Ok(Default::default()));
        let out = render(&st, 140, 50);
        assert!(out.contains("SESSION RUNTIME") && out.contains("Runtime for new sessions"), "{out}");
        assert!(out.contains("‹ tmux ›"), "the selected choice shows its arrows: {out}");
        assert!(out.contains("[ e Open in $EDITOR ]"));
        assert!(out.contains(&format!("ninox v{}", env!("CARGO_PKG_VERSION"))), "settings show the running version: {out}");
        assert!(out.contains("Sessions run on Ninox's private tmux server"), "help for the selection: {out}");
        assert!(out.contains("HARNESSES") && out.contains("claude-code") && out.contains("always"));
        st.settings.selected = 1;
        st.settings.editing = Some("Ctrl+b".into());
        st.settings.error = Some("prefix \"Ctrl+b\" collides with screen/tmux".into());
        let out = render(&st, 140, 50);
        assert!(out.contains("Ctrl+b▏") && out.contains("✗ prefix \"Ctrl+b\" collides"), "{out}");
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
        for tab in ["1 Fleet", "2 Overview", "3 PRs", "4 Brain", "5 Settings"] {
            assert!(header.contains(tab), "{header}");
        }
        assert!(out.contains("inside tmux"), "{out}");
        assert!(out.contains("◀ fleet"), "{out}");
        assert!(out.contains("Ctrl+]"), "the footer names the escape chord: {out}");
    }

    #[test]
    fn restored_checkpoint_is_marked() {
        let mut st = st_with(&["a"]);
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
    fn overview_and_modals_render() {
        let mut st = st_with(&["a", "b", "c"]);
        st.view = View::Overview;
        let out = render(&st, 100, 30);
        assert!(out.contains("tmux session — starting view…"), "{out}");
        st.view = View::Board;
        st.modal = Some(Modal::Help);
        assert!(render(&st, 100, 30).contains("detach"));
        st.modal = Some(Modal::Goto(Default::default()));
        assert!(render(&st, 100, 30).contains("go to"));
        st.modal = Some(Modal::Restore(crate::tui::backend::RestoreSummary { workers: 3, orchestrators: 1, interrupted_at: None, flagged_at: None }));
        assert!(render(&st, 100, 30).contains("Restore fleet (3 workers, 1 orchestrators)?"));
    }

    #[test]
    fn empty_fleet_and_small_terminal_do_not_panic() {
        let st = TuiState::default();
        let out = render(&st, 100, 20);
        assert!(out.contains("No agents running") && out.contains("n spawns an orchestrator"), "{out}");
        for (w, h) in [(1, 1), (10, 3), (30, 5)] {
            render(&st_with(&["x"]), w, h);
        }
    }

    /// An orchestrator with three workers (one blocked, one with a failing
    /// PR, one ended), a standalone session and an interrupted one.
    fn fleet() -> TuiState {
        use crate::test_fixtures::session;
        use ninox_core::types::{GateStatus, SessionStatus as S};
        let row = |id: &str, orch: Option<&str>, status: S| Row {
            session: ninox_core::types::Session { name: format!("{id} with a rather long name"), ..session(id, orch, status) },
            is_orchestrator: orch == Some(id),
            group: orch.map(str::to_string),
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
        let st = fleet();
        let out = render(&st, 120, 30);
        let lines: Vec<&str> = out.lines().collect();
        let at = |needle: &str| lines.iter().position(|l| l.contains(needle)).unwrap_or_else(|| panic!("{needle} missing:\n{out}"));
        let needs = at("NEEDS YOU");
        assert!(needs < at("STANDALONE"));
        for reason in ["blocked", "CI failed", "interrupted", "✉ 2"] {
            assert!(at(reason) > needs, "{reason}");
        }
        assert!(out.contains("#142 ✗"), "PR and CI glyph on the row: {out}");
        assert!(out.contains("ended"), "an ended worker stays visible until reaped: {out}");
        assert!(out.contains("◉1"), "the group header rolls its workers up: {out}");
        assert!(out.contains("✗ w-ci"), "a failing PR is the row's glyph when the agent reports nothing: {out}");
    }

    #[test]
    fn a_folded_group_hides_its_workers_but_keeps_its_header() {
        let mut st = fleet();
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
                Some(Modal::Spawn(Default::default())),
                Some(Modal::Confirm(Pending::Kill("w-ci".into()))),
            ]
        };
        for (w, h) in [(40, 10), (80, 24), (1, 1), (12, 4), (220, 60)] {
            for empty in [false, true] {
                for view in [View::Board, View::Overview, View::PrWatches, View::Brain, View::Settings] {
                    for focus in [Focus::Sidebar, Focus::Pane] {
                        for modal in modals() {
                            let mut st = if empty { TuiState::default() } else { fleet() };
                            st.view = view;
                            st.focus = focus;
                            st.inspector = true;
                            st.selected = st.rows.len().saturating_sub(1).min(2);
                            st.modal = modal;
                            if !empty {
                                st.settings.reload(Ok(Default::default()));
                                st.settings.selected = 1;
                                if focus == Focus::Pane {
                                    st.settings.editing = Some("Ctrl+b".into());
                                    st.settings.error = Some("prefix \"Ctrl+b\" collides".into());
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
        assert!(header.contains("5 Settings") && header.contains("engine"), "all tabs fit at 80 columns: {header}");
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
        let st = ended_report(true, true);
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
        let out = render(&ended_report(false, false), 120, 30);
        assert!(out.contains("no workspace recorded"), "{out}");
        assert!(!out.contains("PULL REQUEST") && !out.contains("Open PR"), "{out}");
        assert!(out.contains("[ x Remove ]"));
        let mut st = ended_report(false, false);
        st.reports.clear();
        let out = render(&st, 120, 30);
        assert!(out.contains("loading…"), "before the report arrives: {out}");
    }

    #[test]
    fn report_shows_an_exited_ptyd_panes_last_screen() {
        let mut st = ended_report(false, true);
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
    fn brain_renders_a_tag_tree_with_buttons_and_an_indexing_note() {
        let mut st = TuiState { view: View::Brain, ..Default::default() };
        st.brain = crate::tui::brain::tests::sample();
        st.brain.move_cursor(5);
        let out = render(&st, 100, 20);
        assert!(out.contains("4 entries · by tag") && out.contains("[ a New entry ]"), "{out}");
        assert!(out.contains("▾ tmux  (2)") && out.contains("▾ untagged  (1)"), "{out}");
        assert!(out.contains("[ e Edit ]  [ D Delete ]") && out.contains("#tmux #runtime"), "{out}");
        assert!(out.contains("e edit") && out.contains("a new") && out.contains("D delete"), "footer hints: {out}");
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
    fn prs_render_grouped_with_state_and_open_buttons() {
        let mut st = TuiState { view: View::PrWatches, ..Default::default() };
        let out = render(&st, 120, 20);
        assert!(out.contains("No PRs yet") && out.contains("ninox open --pr <url>"), "{out}");
        st.prs.rows = crate::tui::prs::tests::sample();
        st.prs.rows[2].gate = Some(ninox_core::types::GateStatus {
            ci: GateCheck::Failing,
            review: GateCheck::Pending,
            mergeable: GateCheck::Passing,
            since: 0,
        });
        st.prs.watching = Some(false);
        st.prs.select(2);
        let out = render(&st, 120, 20);
        assert!(out.contains("4 PRs · 2 open") && out.contains("PR watching off"), "{out}");
        assert!(out.contains("acme/web  (2)") && out.contains("zed/lib  (1)"), "{out}");
        let line = out.lines().find(|l| l.contains("#142")).unwrap();
        assert!(line.contains("CI failed") && line.contains("✗ CI") && line.contains("Fix token refresh"), "{line}");
        assert!(line.contains("w1 name") && line.contains("[ ↗ Open ]") && line.contains("▌"), "{line}");
        assert!(out.contains("↵/o open in browser") && out.contains("s show session"), "{out}");
        for (w, h) in [(1, 1), (12, 4), (40, 10), (220, 60)] {
            assert_eq!(render(&st, w, h).lines().count(), h as usize);
        }
    }

    #[test]
    fn markdown_reader_strips_frontmatter_and_styles_blocks() {
        let lines = markdown_lines("---\nname: x\n---\n# Title\n- item with `code`\n```\nlet a = 1;\n```\n", &Palette::default());
        let text: Vec<String> = lines.iter().map(|l| l.spans.iter().map(|s| s.content.to_string()).collect()).collect();
        assert_eq!(text, vec!["Title", "• item with code", "  let a = 1;"]);
    }

    #[test]
    fn ago_formats() {
        assert_eq!(ago(61_000, 0), "1m");
        assert_eq!(ago(0, 5_000), "0s");
    }
}
