//! On-demand scrollback backed by tmux pane history (the source of truth).
//! Lines are fetched in chunks via `capture-pane -e`, parsed once into
//! styled cells, and cached for the lifetime of the view.

use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::Color;
use std::collections::VecDeque;

pub const FETCH_CHUNK: i64 = 300;

#[derive(Debug, Clone, PartialEq)]
pub struct StyledCell {
    pub c:     char,
    pub fg:    Color,
    pub bg:    Color,
    pub flags: Flags,
}

pub type StyledLine = Vec<StyledCell>;

/// Parse `capture-pane -e` output (SGR-styled text, \n separated) into
/// styled lines by replaying it through a throwaway emulator at pane width.
pub fn parse_capture(bytes: &[u8], cols: u16) -> Vec<StyledLine> {
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::{Column, Line};

    // Empty capture (zero history or failed capture) must add nothing to the cache.
    if bytes.is_empty() {
        return Vec::new();
    }

    let n_lines = bytes.iter().filter(|&&b| b == b'\n').count().max(1);
    // +1: every captured line (including the last) ends in \n, so the final
    // \r\n we feed below asks the cursor to move one line past the last
    // line of content. Without a spare row, that overflow scrolls the
    // grid — pushing the *first* content line off into history and
    // corrupting the order. The spare row absorbs it and is discarded when
    // we only read back `n_lines` rows.
    let height = (n_lines + 1).min(u16::MAX as usize) as u16;
    let mut state = crate::components::terminal::TerminalState::new(cols, height, None);
    // capture-pane emits bare \n; the emulator needs \r\n to reset columns.
    let mut feed = Vec::with_capacity(bytes.len() + n_lines);
    for &b in bytes {
        if b == b'\n' { feed.push(b'\r'); }
        feed.push(b);
    }
    state.process(&feed);

    let grid = state.term.grid();
    let rows = grid.screen_lines();
    let mut out = Vec::with_capacity(n_lines.min(rows));
    for row in 0..rows.min(n_lines) {
        let line = Line(row as i32);
        let mut cells: StyledLine = (0..grid.columns())
            .map(|col| {
                let cell = &grid[line][Column(col)];
                StyledCell { c: cell.c, fg: cell.fg, bg: cell.bg, flags: cell.flags }
            })
            .collect();
        // Trim trailing default-blank cells so rendering can skip them.
        while cells.last().is_some_and(|c| c.c == ' ' || c.c == '\0') {
            cells.pop();
        }
        out.push(cells);
    }
    out
}

/// Cached history + scroll position for one terminal view.
#[derive(Default)]
pub struct Scrollback {
    /// Cached history lines, oldest first.
    pub lines: VecDeque<StyledLine>,
    /// How many lines above the live screen the view is scrolled. 0 = live.
    pub offset: usize,
    /// Most negative tmux history index fetched so far (0 = nothing yet).
    pub fetched_to: i64,
    /// All available history has been fetched.
    pub top_reached: bool,
    /// A capture-pane fetch is in flight; don't issue another.
    pub fetch_pending: bool,
}

impl Scrollback {
    /// n=0 → the line directly above the live screen.
    pub fn line_above(&self, n: usize) -> Option<&StyledLine> {
        let len = self.lines.len();
        if n < len { self.lines.get(len - 1 - n) } else { None }
    }

    /// Scroll up by `delta`; clamps to cached lines. Returns true when the
    /// caller should fetch an older chunk (cache edge hit, more exists).
    pub fn scroll_up(&mut self, delta: usize) -> bool {
        let want = self.offset + delta;
        self.offset = want.min(self.lines.len());
        want > self.lines.len() && !self.top_reached && !self.fetch_pending
    }

    pub fn scroll_down(&mut self, delta: usize) {
        self.offset = self.offset.saturating_sub(delta);
    }

    /// Prepend an older chunk fetched from tmux.
    pub fn absorb(&mut self, older: Vec<StyledLine>, fetched_to: i64, top_reached: bool) {
        for line in older.into_iter().rev() {
            self.lines.push_front(line);
        }
        self.fetched_to = fetched_to;
        self.top_reached = top_reached;
        self.fetch_pending = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_capture_preserves_text_and_color() {
        // Two lines as capture-pane -e emits them: SGR + text + \n.
        let bytes = b"\x1b[31mred line\x1b[0m\nplain line\n";
        let lines = parse_capture(bytes, 40);
        assert_eq!(lines.len(), 2);
        let text: String = lines[0].iter().map(|c| c.c).collect();
        assert_eq!(text.trim_end(), "red line");
        use alacritty_terminal::vte::ansi::{Color, NamedColor};
        assert_eq!(lines[0][0].fg, Color::Named(NamedColor::Red));
        let text1: String = lines[1].iter().map(|c| c.c).collect();
        assert_eq!(text1.trim_end(), "plain line");
    }

    #[test]
    fn parse_capture_drops_trailing_blank_padding() {
        // The throwaway grid is taller than the content; blank tail rows
        // must not become phantom history lines.
        let lines = parse_capture(b"only\n", 40);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn parse_capture_empty_input_returns_no_lines() {
        // Zero history or a failed capture-pane must not cache phantom lines.
        assert!(parse_capture(b"", 40).is_empty());
    }

    #[test]
    fn scroll_bookkeeping_requests_fetch_at_cache_edge() {
        let mut sb = Scrollback::default();
        // Empty cache: any scroll up needs a fetch.
        assert!(sb.scroll_up(3));
        assert_eq!(sb.offset, 0, "offset must not exceed cached lines");

        sb.absorb(vec![vec![]; 100], -100, false);
        assert!(!sb.scroll_up(50), "within cache: no fetch needed");
        assert_eq!(sb.offset, 50);
        assert!(sb.scroll_up(60), "beyond cache: fetch needed");
        assert_eq!(sb.offset, 100, "clamped to cached lines");

        sb.scroll_down(30);
        assert_eq!(sb.offset, 70);
        sb.scroll_down(1000);
        assert_eq!(sb.offset, 0);
    }

    #[test]
    fn top_reached_stops_fetch_requests() {
        let mut sb = Scrollback::default();
        sb.absorb(vec![vec![]; 10], -10, true);
        assert!(!sb.scroll_up(500), "no more history exists; no fetch");
        assert_eq!(sb.offset, 10);
    }

    #[test]
    fn line_above_indexes_newest_first() {
        let mut sb = Scrollback::default();
        let mk = |ch: char| vec![StyledCell {
            c: ch,
            fg: alacritty_terminal::vte::ansi::Color::Named(alacritty_terminal::vte::ansi::NamedColor::Foreground),
            bg: alacritty_terminal::vte::ansi::Color::Named(alacritty_terminal::vte::ansi::NamedColor::Background),
            flags: alacritty_terminal::term::cell::Flags::empty(),
        }];
        // Oldest-first storage: a then b; b is directly above the screen.
        sb.absorb(vec![mk('a'), mk('b')], -2, true);
        assert_eq!(sb.line_above(0).unwrap()[0].c, 'b');
        assert_eq!(sb.line_above(1).unwrap()[0].c, 'a');
        assert!(sb.line_above(2).is_none());
    }
}
