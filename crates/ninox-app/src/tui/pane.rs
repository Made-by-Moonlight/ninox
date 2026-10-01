//! Client-side composition of ptyd-owned screens: a `PaneView` holds the
//! latest `ScreenSnapshot` for one session and paints it into a region of a
//! ratatui `Buffer`.

use ninox_ptyd::{checkpoint::Checkpoint, Color as PColor, Line, ScreenSnapshot, Style as PStyle};
use unicode_width::UnicodeWidthChar;
use ratatui::{
    buffer::Buffer,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
};

#[derive(Debug, Clone, Default)]
pub struct PaneView {
    pub live: Option<ScreenSnapshot>,
    /// Last on-disk screen, shown dimmed until the live pane has painted
    /// something (spec §5.5).
    pub checkpoint: Option<Checkpoint>,
    /// `Some(code)` once ptyd reported the process exited.
    pub exited: Option<Option<i32>>,
    /// Lines scrolled up from the live bottom; 0 = following live output.
    pub scroll: usize,
}

/// What a pane region should show right now.
pub enum Display<'a> {
    Live(&'a ScreenSnapshot),
    Restored(&'a ScreenSnapshot),
    Empty,
}

impl PaneView {
    pub fn display(&self) -> Display<'_> {
        match (&self.live, &self.checkpoint) {
            (Some(live), Some(cp)) if !has_content(live) => Display::Restored(&cp.screen),
            (Some(live), _) => Display::Live(live),
            (None, Some(cp)) => Display::Restored(&cp.screen),
            (None, None) => Display::Empty,
        }
    }

    /// Paint into `area`; returns where the terminal cursor belongs when
    /// `focused` and the live cursor is on screen.
    pub fn render(&self, area: Rect, buf: &mut Buffer, focused: bool) -> Option<Position> {
        match self.display() {
            Display::Live(snap) => render_snapshot(
                snap,
                area,
                buf,
                RenderOpts { scroll: self.scroll, dim: false, cursor: focused && self.scroll == 0 },
            ),
            Display::Restored(snap) => {
                render_snapshot(snap, area, buf, RenderOpts { scroll: 0, dim: true, cursor: false });
                None
            }
            Display::Empty => {
                clear(area, buf);
                None
            }
        }
    }

    pub fn is_restored(&self) -> bool {
        matches!(self.display(), Display::Restored(_))
    }

    pub fn snapshot(&self) -> Option<&ScreenSnapshot> {
        match self.display() {
            Display::Live(s) | Display::Restored(s) => Some(s),
            Display::Empty => None,
        }
    }

    /// Text between viewport cells `a` and `b` (`(row, col)`, inclusive) of
    /// a region `height` rows tall.
    pub fn selection_text(&self, height: u16, a: (u16, u16), b: (u16, u16)) -> String {
        let scroll = if self.is_restored() { 0 } else { self.scroll };
        self.snapshot().map(|s| selection_text(s, height, scroll, a, b)).unwrap_or_default()
    }

    /// Column span of the word (run of non-blank cells) under `(row, col)`.
    pub fn word_at(&self, height: u16, row: u16, col: u16) -> Option<(u16, u16)> {
        let snap = self.snapshot()?;
        let scroll = if self.is_restored() { 0 } else { self.scroll };
        let (start, end) = visible_window(snap, height, scroll);
        let idx = start + row as usize;
        if idx >= end {
            return None;
        }
        word_span(&snap.lines[idx], col)
    }

    /// Plain text of what the pane currently shows (for yank).
    pub fn window_text(&self, height: u16) -> String {
        let Some(snap) = self.snapshot() else { return String::new() };
        let (start, end) = visible_window(snap, height, self.scroll);
        let mut out: Vec<String> = snap.lines[start..end]
            .iter()
            .map(|l| l.runs.iter().map(|r| r.text.as_str()).collect::<String>().trim_end().to_string())
            .collect();
        while out.last().is_some_and(|l| l.is_empty()) {
            out.pop();
        }
        out.join("\n")
    }
}

/// True once the screen holds any non-blank text.
pub fn has_content(snap: &ScreenSnapshot) -> bool {
    snap.lines
        .iter()
        .skip(snap.scrollback_len)
        .any(|l| l.runs.iter().any(|r| r.text.chars().any(|c| !c.is_whitespace())))
}

#[derive(Debug, Clone, Copy)]
pub struct RenderOpts {
    pub scroll: usize,
    pub dim: bool,
    pub cursor: bool,
}

/// `[start, end)` indices into `snap.lines` for a viewport `height` rows tall.
///
/// Live (scroll 0): the visible screen, top-aligned when it fits; when the
/// region is shorter (previews, or before a resize lands) it is
/// bottom-anchored — agent prompts live at the bottom — but never hides the
/// cursor row. Scrolled: the window ending `scroll` lines above the live
/// bottom, clamped to the history the snapshot carries.
pub fn visible_window(snap: &ScreenSnapshot, height: u16, scroll: usize) -> (usize, usize) {
    let h = height as usize;
    let total = snap.lines.len();
    let sb = snap.scrollback_len.min(total);
    let screen_end = total;
    if scroll == 0 {
        let rows = screen_end - sb;
        if h >= rows {
            return (sb, screen_end);
        }
        let mut start = screen_end - h;
        let cursor_abs = sb + snap.cursor.row as usize;
        if cursor_abs < start {
            start = cursor_abs;
        }
        return (start, (start + h).min(screen_end));
    }
    let end = screen_end.saturating_sub(scroll.min(sb)).max(h.min(screen_end));
    let start = end.saturating_sub(h);
    (start, end)
}

/// `(start column, text)` per cell; zero-width chars join the cell before.
fn cells(line: &Line) -> Vec<(u16, String)> {
    let mut out: Vec<(u16, String)> = Vec::new();
    let mut col = 0u16;
    for ch in line.runs.iter().flat_map(|r| r.text.chars()) {
        match ch.width().unwrap_or(0) {
            0 => {
                if let Some(last) = out.last_mut() {
                    last.1.push(ch);
                }
            }
            w => {
                out.push((col, ch.to_string()));
                col = col.saturating_add(w as u16);
            }
        }
    }
    out
}

/// `(a, b)` in reading order.
pub fn ordered(a: (u16, u16), b: (u16, u16)) -> ((u16, u16), (u16, u16)) {
    if a <= b { (a, b) } else { (b, a) }
}

/// Text of the viewport cells `a..=b` (`(row, col)`) of the window a region
/// `height` rows tall shows at `scroll`. Soft-wrapped lines join without a
/// newline; hard lines lose trailing blanks.
pub fn selection_text(snap: &ScreenSnapshot, height: u16, scroll: usize, a: (u16, u16), b: (u16, u16)) -> String {
    let (start, end) = visible_window(snap, height, scroll);
    let ((r0, c0), (r1, c1)) = ordered(a, b);
    let mut out = String::new();
    for r in r0..=r1 {
        let idx = start + r as usize;
        if idx >= end {
            break;
        }
        let line = &snap.lines[idx];
        let lo = if r == r0 { c0 } else { 0 };
        let hi = if r == r1 { c1 } else { u16::MAX };
        let text: String = cells(line).into_iter().filter(|(c, _)| (lo..=hi).contains(c)).map(|(_, t)| t).collect();
        let last = r == r1 || idx + 1 >= end;
        if line.wrapped && !last {
            out.push_str(&text);
        } else {
            out.push_str(text.trim_end());
            if !last {
                out.push('\n');
            }
        }
    }
    out
}

fn word_span(line: &Line, col: u16) -> Option<(u16, u16)> {
    let cells = cells(line);
    let blank = |t: &str| t.chars().all(char::is_whitespace);
    let at = cells.iter().rposition(|(c, _)| *c <= col)?;
    if blank(&cells[at].1) {
        return None;
    }
    let mut lo = at;
    while lo > 0 && !blank(&cells[lo - 1].1) {
        lo -= 1;
    }
    let mut hi = at;
    while hi + 1 < cells.len() && !blank(&cells[hi + 1].1) {
        hi += 1;
    }
    Some((cells[lo].0, cells[hi].0))
}

/// Reverse-video the cells `a..=b` (viewport `(row, col)`) inside `area`.
pub fn highlight(area: Rect, buf: &mut Buffer, a: (u16, u16), b: (u16, u16)) {
    let ((r0, c0), (r1, c1)) = ordered(a, b);
    for r in r0..=r1.min(area.height.saturating_sub(1)) {
        let lo = if r == r0 { c0 } else { 0 };
        let hi = if r == r1 { c1 } else { area.width.saturating_sub(1) };
        for c in lo..=hi.min(area.width.saturating_sub(1)) {
            if let Some(cell) = buf.cell_mut((area.x + c, area.y + r)) {
                cell.modifier.toggle(Modifier::REVERSED);
            }
        }
    }
}

fn clear(area: Rect, buf: &mut Buffer) {
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.reset();
            }
        }
    }
}

pub fn render_snapshot(snap: &ScreenSnapshot, area: Rect, buf: &mut Buffer, opts: RenderOpts) -> Option<Position> {
    clear(area, buf);
    if area.width == 0 || area.height == 0 {
        return None;
    }
    let (start, end) = visible_window(snap, area.height, opts.scroll);
    for (row, line) in snap.lines[start..end].iter().enumerate() {
        let y = area.y + row as u16;
        let mut x = area.x;
        for run in &line.runs {
            if x >= area.right() {
                break;
            }
            let mut style = map_style(run.style);
            if opts.dim {
                style = style.add_modifier(Modifier::DIM);
            }
            let max = (area.right() - x) as usize;
            let (nx, _) = buf.set_stringn(x, y, &run.text, max, style);
            x = nx;
        }
        if opts.dim {
            for cx in x..area.right() {
                if let Some(cell) = buf.cell_mut((cx, y)) {
                    cell.modifier.insert(Modifier::DIM);
                }
            }
        }
    }
    if !opts.cursor || !snap.cursor.visible {
        return None;
    }
    let abs = snap.scrollback_len + snap.cursor.row as usize;
    if abs < start || abs >= end || snap.cursor.col >= area.width {
        return None;
    }
    Some(Position { x: area.x + snap.cursor.col, y: area.y + (abs - start) as u16 })
}

pub fn map_color(c: PColor) -> Color {
    match c {
        PColor::Default => Color::Reset,
        PColor::Indexed(i) => Color::Indexed(i),
        PColor::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

pub fn map_style(s: PStyle) -> Style {
    let mut m = Modifier::empty();
    m.set(Modifier::BOLD, s.bold);
    m.set(Modifier::DIM, s.dim);
    m.set(Modifier::ITALIC, s.italic);
    m.set(Modifier::UNDERLINED, s.underline);
    m.set(Modifier::REVERSED, s.inverse);
    m.set(Modifier::CROSSED_OUT, s.strikethrough);
    Style::default().fg(map_color(s.fg)).bg(map_color(s.bg)).add_modifier(m)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ninox_ptyd::{Cursor, Line, Modes, Run};

    pub(crate) fn line(text: &str) -> Line {
        Line { runs: vec![Run { text: text.into(), style: PStyle::default() }], wrapped: false }
    }

    pub(crate) fn snap(lines: &[&str], scrollback: usize, cols: u16) -> ScreenSnapshot {
        ScreenSnapshot {
            cols,
            rows: (lines.len() - scrollback) as u16,
            seq: 1,
            lines: lines.iter().map(|l| line(l)).collect(),
            scrollback_len: scrollback,
            cursor: Cursor { row: 0, col: 0, visible: true },
            modes: Modes::default(),
            title: None,
        }
    }

    fn row_text(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>()
    }

    #[test]
    fn renders_runs_with_colors_and_modifiers() {
        let mut s = snap(&["", ""], 0, 10);
        s.lines[0] = Line {
            runs: vec![
                Run { text: "ab".into(), style: PStyle { fg: PColor::Indexed(1), bold: true, ..Default::default() } },
                Run { text: "c".into(), style: PStyle { bg: PColor::Rgb(1, 2, 3), inverse: true, ..Default::default() } },
            ],
            wrapped: false,
        };
        let area = Rect::new(0, 0, 10, 2);
        let mut buf = Buffer::empty(area);
        render_snapshot(&s, area, &mut buf, RenderOpts { scroll: 0, dim: false, cursor: false });
        assert_eq!(buf[(0, 0)].symbol(), "a");
        assert_eq!(buf[(0, 0)].fg, Color::Indexed(1));
        assert!(buf[(1, 0)].modifier.contains(Modifier::BOLD));
        assert_eq!(buf[(2, 0)].bg, Color::Rgb(1, 2, 3));
        assert!(buf[(2, 0)].modifier.contains(Modifier::REVERSED));
        assert_eq!(buf[(3, 0)].symbol(), " ");
        assert_eq!(buf[(3, 0)].fg, Color::Reset);
    }

    #[test]
    fn wide_chars_occupy_two_cells() {
        let mut s = snap(&[""], 0, 6);
        s.lines[0].runs = vec![
            Run { text: "漢x".into(), style: PStyle::default() },
        ];
        let area = Rect::new(0, 0, 6, 1);
        let mut buf = Buffer::empty(area);
        render_snapshot(&s, area, &mut buf, RenderOpts { scroll: 0, dim: false, cursor: false });
        assert_eq!(buf[(0, 0)].symbol(), "漢");
        assert_eq!(buf[(2, 0)].symbol(), "x");
    }

    #[test]
    fn clips_to_area_and_offsets_by_region_origin() {
        let s = snap(&["hello world"], 0, 11);
        let area = Rect::new(2, 1, 5, 1);
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 3));
        render_snapshot(&s, area, &mut buf, RenderOpts { scroll: 0, dim: false, cursor: false });
        assert_eq!(row_text(&buf, 1), "  hello   ");
    }

    #[test]
    fn cursor_position_is_reported_only_when_on_screen() {
        let mut s = snap(&["a", "b", "c"], 0, 4);
        s.cursor = Cursor { row: 2, col: 1, visible: true };
        let area = Rect::new(1, 1, 4, 3);
        let mut buf = Buffer::empty(Rect::new(0, 0, 6, 5));
        let pos = render_snapshot(&s, area, &mut buf, RenderOpts { scroll: 0, dim: false, cursor: true });
        assert_eq!(pos, Some(Position { x: 2, y: 3 }));
        s.cursor.visible = false;
        assert_eq!(render_snapshot(&s, area, &mut buf, RenderOpts { scroll: 0, dim: false, cursor: true }), None);
    }

    #[test]
    fn short_regions_are_bottom_anchored_but_keep_the_cursor() {
        let mut s = snap(&["1", "2", "3", "4"], 0, 4);
        s.cursor.row = 3;
        assert_eq!(visible_window(&s, 2, 0), (2, 4));
        s.cursor.row = 0;
        assert_eq!(visible_window(&s, 2, 0), (0, 2));
    }

    #[test]
    fn scrolled_window_reads_history_and_clamps_at_top() {
        // 3 scrollback lines + 2 visible.
        let s = snap(&["h1", "h2", "h3", "v1", "v2"], 3, 4);
        assert_eq!(visible_window(&s, 2, 0), (3, 5));
        assert_eq!(visible_window(&s, 2, 1), (2, 4));
        assert_eq!(visible_window(&s, 2, 99), (0, 2));
        let area = Rect::new(0, 0, 4, 2);
        let mut buf = Buffer::empty(area);
        render_snapshot(&s, area, &mut buf, RenderOpts { scroll: 2, dim: false, cursor: true });
        assert_eq!(row_text(&buf, 0), "h2  ");
        assert_eq!(row_text(&buf, 1), "h3  ");
    }

    #[test]
    fn checkpoint_renders_dimmed_until_live_output_arrives() {
        let cp = Checkpoint { pane: "p".into(), saved_ms: 0, screen: snap(&["old screen"], 0, 10) };
        let mut view = PaneView { checkpoint: Some(cp), ..Default::default() };
        let area = Rect::new(0, 0, 10, 1);
        let mut buf = Buffer::empty(area);
        view.render(area, &mut buf, true);
        assert!(view.is_restored());
        assert_eq!(row_text(&buf, 0), "old screen");
        assert!(buf[(0, 0)].modifier.contains(Modifier::DIM));
        assert!(buf[(9, 0)].modifier.contains(Modifier::DIM));

        // A live pane that has not painted yet keeps the checkpoint up.
        view.live = Some(snap(&["   "], 0, 10));
        assert!(view.is_restored());

        view.live = Some(snap(&["fresh"], 0, 10));
        assert!(!view.is_restored());
        let mut buf = Buffer::empty(area);
        view.render(area, &mut buf, true);
        assert_eq!(row_text(&buf, 0), "fresh     ");
        assert!(!buf[(0, 0)].modifier.contains(Modifier::DIM));
    }

    #[test]
    fn selection_text_spans_lines_in_reading_order() {
        let s = snap(&["hello world  ", "second line", "third"], 0, 13);
        assert_eq!(selection_text(&s, 3, 0, (0, 6), (0, 10)), "world");
        assert_eq!(selection_text(&s, 3, 0, (1, 3), (0, 6)), "world\nseco", "a backwards drag reads forwards");
        assert_eq!(selection_text(&s, 3, 0, (0, 0), (2, 40)), "hello world\nsecond line\nthird");
    }

    #[test]
    fn selection_text_joins_soft_wraps_and_handles_wide_chars() {
        let mut s = snap(&["abc", "def", "漢字x"], 0, 3);
        s.lines[0].wrapped = true;
        assert_eq!(selection_text(&s, 3, 0, (0, 1), (1, 1)), "bcde");
        // 漢 covers cols 0-1, 字 2-3, x 4.
        assert_eq!(selection_text(&s, 3, 0, (2, 2), (2, 4)), "字x");
    }

    #[test]
    fn selection_rows_follow_the_scrolled_window() {
        let s = snap(&["h1", "h2", "v1", "v2"], 2, 4);
        assert_eq!(selection_text(&s, 2, 0, (0, 0), (1, 9)), "v1\nv2");
        assert_eq!(selection_text(&s, 2, 2, (0, 0), (1, 9)), "h1\nh2");
    }

    #[test]
    fn word_at_takes_the_non_blank_run() {
        let view = PaneView { live: Some(snap(&["see /tmp/a.log now"], 0, 20)), ..Default::default() };
        assert_eq!(view.word_at(1, 0, 7), Some((4, 13)));
        assert_eq!(view.word_at(1, 0, 3), None, "a blank cell selects nothing");
    }

    #[test]
    fn highlight_reverses_exactly_the_selected_cells() {
        let area = Rect::new(1, 1, 4, 2);
        let mut buf = Buffer::empty(Rect::new(0, 0, 6, 4));
        highlight(area, &mut buf, (1, 1), (0, 2));
        let rev = |x, y| buf[(x, y)].modifier.contains(Modifier::REVERSED);
        assert!(!rev(2, 1) && rev(3, 1) && rev(4, 1));
        assert!(rev(1, 2) && rev(2, 2) && !rev(3, 2));
    }

    #[test]
    fn window_text_trims_trailing_blanks() {
        let view = PaneView { live: Some(snap(&["a  ", "b", "", ""], 0, 4)), ..Default::default() };
        assert_eq!(view.window_text(4), "a\nb");
    }

    #[test]
    fn renders_through_test_backend() {
        let backend = ratatui::backend::TestBackend::new(8, 2);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let view = PaneView { live: Some(snap(&["ninox", "> _"], 0, 8)), ..Default::default() };
        terminal
            .draw(|f| {
                let area = f.area();
                if let Some(p) = view.render(area, f.buffer_mut(), true) {
                    f.set_cursor_position(p);
                }
            })
            .unwrap();
        terminal.backend().assert_buffer_lines(["ninox   ", "> _     "]);
    }
}
