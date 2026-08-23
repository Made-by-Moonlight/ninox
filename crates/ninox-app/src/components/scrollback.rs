//! On-demand scrollback backed by tmux pane history (the source of truth).
//! Lines are fetched in bounded, overlapping pages via `capture-pane -e`.
//! Overlap is reconciled against styled cells, so tmux-relative coordinates
//! may move while live output continues without duplicating page boundaries.

use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::Color;
use std::collections::VecDeque;

pub const FETCH_CHUNK: i64 = 256;
pub const FETCH_THRESHOLD: usize = 64;
pub const PAGE_OVERLAP: i64 = 8;
pub const MAX_CAPTURE_LINES: usize = (FETCH_CHUNK + PAGE_OVERLAP) as usize;

#[derive(Debug, Clone, PartialEq)]
pub struct StyledCell {
    pub c:         char,
    pub zerowidth: Vec<char>,
    pub fg:        Color,
    pub bg:        Color,
    pub flags:     Flags,
    pub hyperlink: Option<String>,
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

    // A page captured with `-J` can contain few newline bytes while replaying
    // into many soft-wrapped physical rows. Allocate exactly the bounded page
    // ceiling plus one spare row; never derive work from unbounded byte count.
    let height = (MAX_CAPTURE_LINES + 1) as u16;
    let mut state = crate::components::terminal::TerminalState::new(cols, height, None);
    // capture-pane emits bare \n; the emulator needs \r\n to reset columns.
    let capture = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let mut feed = Vec::with_capacity(capture.len() + capture.iter().filter(|&&b| b == b'\n').count());
    for &b in capture {
        if b == b'\n' {
            feed.push(b'\r');
        }
        feed.push(b);
    }
    state.process(&feed);

    let grid = state.term.grid();
    let rows = (grid.cursor.point.line.0.max(0) as usize + 1).min(grid.screen_lines());
    let mut out = Vec::with_capacity(rows);
    for row in 0..rows {
        let line = Line(row as i32);
        let mut cells: StyledLine = (0..grid.columns())
            .map(|col| {
                let cell = &grid[line][Column(col)];
                StyledCell {
                    c:         cell.c,
                    zerowidth: cell.zerowidth().unwrap_or_default().to_vec(),
                    fg:        cell.fg,
                    bg:        cell.bg,
                    flags:     cell.flags,
                    hyperlink: cell.hyperlink().map(|h| h.uri().to_string()),
                }
            })
            .collect();
        // Trim trailing default-blank cells so rendering can skip them.
        while cells.last().is_some_and(|cell| {
            use alacritty_terminal::vte::ansi::NamedColor;
            (cell.c == ' ' || cell.c == '\0')
                && cell.zerowidth.is_empty()
                && cell.flags.is_empty()
                && cell.hyperlink.is_none()
                && cell.fg == Color::Named(NamedColor::Foreground)
                && cell.bg == Color::Named(NamedColor::Background)
        }) {
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
    /// User-requested offset, which may temporarily exceed `lines.len()`
    /// while an older-history fetch is in flight.
    requested_offset: usize,
    /// Most negative tmux history index fetched so far (0 = nothing yet).
    pub fetched_to: i64,
    /// All available history has been fetched.
    pub top_reached: bool,
    /// A capture-pane fetch is in flight; don't issue another.
    pub fetch_pending: bool,
    /// History size at the capture that established `fetched_to`. A later
    /// request shifts the tmux-relative boundary by newly appended rows.
    pub history_size: i64,
    /// Invalidates captures from a resize, reconnect, clear-history, or reset.
    epoch: u64,
    next_request_id: u64,
    pending_request_id: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FetchCursor {
    pub epoch: u64,
    pub request_id: u64,
    pub fetched_to: i64,
    pub history_size: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbsorbOutcome {
    Applied { prepended: usize },
    Stale,
    Truncated,
}

impl Scrollback {
    /// n=0 → the line directly above the live screen.
    pub fn line_above(&self, n: usize) -> Option<&StyledLine> {
        let len = self.lines.len();
        if n < len {
            self.lines.get(len - 1 - n)
        } else {
            None
        }
    }

    /// Scroll up by `delta`; clamps to cached lines. Returns true when an
    /// older page should be prefetched. Fetching before the edge means a
    /// prepend does not move the visible anchor.
    pub fn scroll_up(&mut self, delta: usize) -> bool {
        self.requested_offset = self.requested_offset.saturating_add(delta);
        if self.top_reached {
            self.requested_offset = self.requested_offset.min(self.lines.len());
        }
        self.offset = self.requested_offset.min(self.lines.len());
        self.needs_fetch()
    }

    pub fn scroll_down(&mut self, delta: usize) {
        self.requested_offset = self.requested_offset.saturating_sub(delta);
        self.offset = self.requested_offset.min(self.lines.len());
    }

    pub fn needs_fetch(&self) -> bool {
        if self.fetch_pending || self.top_reached || self.requested_offset == 0 {
            return false;
        }
        self.lines.is_empty()
            || self.requested_offset > self.lines.len()
            || self.lines.len().saturating_sub(self.offset) <= FETCH_THRESHOLD
    }

    /// Reserve one fetch. Repeated wheel events only update scroll intent;
    /// they cannot create concurrent captures for the same view.
    pub fn begin_fetch(&mut self) -> Option<FetchCursor> {
        if !self.needs_fetch() {
            return None;
        }
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1);
        self.pending_request_id = Some(request_id);
        self.fetch_pending = true;
        Some(FetchCursor {
            epoch: self.epoch,
            request_id,
            fetched_to: self.fetched_to,
            history_size: self.history_size,
        })
    }

    /// Range for the next bounded capture at current tmux `history_size`.
    pub fn capture_range(cursor: FetchCursor, history_size: i64) -> Option<(i64, i64)> {
        if history_size <= 0 {
            return None;
        }
        if cursor.fetched_to == 0 {
            return Some(((-FETCH_CHUNK).max(-history_size), -1));
        }

        // Negative tmux indices move away from zero as live rows enter
        // history. Shift the known boundary before asking for its overlap.
        let appended = (history_size - cursor.history_size).max(0);
        let boundary = cursor.fetched_to - appended;
        let start = (boundary - FETCH_CHUNK).max(-history_size);
        let end = (boundary + PAGE_OVERLAP - 1).min(-1);
        (start <= end).then_some((start, end))
    }

    /// Apply one overlapping page. `older` is oldest-first and may contain
    /// the first cached rows at its end. The longest exact styled overlap is
    /// removed before prepend, preserving wrapping, wide/combining cells,
    /// hyperlinks, and BiDi logical cells as one identity.
    pub fn absorb_page(
        &mut self,
        cursor: FetchCursor,
        mut older: Vec<StyledLine>,
        fetched_to: i64,
        history_size: i64,
        top_reached: bool,
    ) -> AbsorbOutcome {
        if cursor.epoch != self.epoch
            || self.pending_request_id != Some(cursor.request_id)
        {
            return AbsorbOutcome::Stale;
        }
        self.fetch_pending = false;
        self.pending_request_id = None;

        if history_size < cursor.history_size || older.len() > MAX_CAPTURE_LINES {
            self.reset();
            return AbsorbOutcome::Truncated;
        }

        let prepend_count = if self.lines.is_empty() {
            older.len()
        } else {
            let existing: Vec<_> = self.lines.iter().collect();
            let max_match = (PAGE_OVERLAP as usize)
                .min(existing.len())
                .min(older.len());
            let mut boundary = None;
            for match_len in (1..=max_match).rev() {
                for position in (0..=older.len() - match_len).rev() {
                    if older[position..position + match_len]
                        .iter()
                        .zip(existing.iter())
                        .all(|(left, right)| left == *right)
                    {
                        boundary = Some(position);
                        break;
                    }
                }
                if boundary.is_some() {
                    break;
                }
            }
            let Some(boundary) = boundary else {
                // The anchor disappeared (clear-history, history-limit
                // eviction, or a race larger than the overlap window).
                self.reset();
                return AbsorbOutcome::Truncated;
            };
            older.truncate(boundary);
            older.len()
        };

        for line in older.into_iter().rev() {
            self.lines.push_front(line);
        }
        self.fetched_to = fetched_to;
        self.history_size = history_size;
        self.top_reached = top_reached;
        if self.top_reached {
            self.requested_offset = self.requested_offset.min(self.lines.len());
        }
        self.offset = self.requested_offset.min(self.lines.len());
        AbsorbOutcome::Applied { prepended: prepend_count }
    }

    pub fn absorb_truncated(&mut self, cursor: FetchCursor) -> AbsorbOutcome {
        if cursor.epoch != self.epoch
            || self.pending_request_id != Some(cursor.request_id)
        {
            return AbsorbOutcome::Stale;
        }
        self.reset();
        AbsorbOutcome::Truncated
    }

    /// Add rows that the live emulator demonstrably scrolled off-screen.
    /// Increasing both length and offset by the same amount preserves every
    /// viewport coordinate while the user reads history.
    pub fn append_live_scrolled(&mut self, mut newer: Vec<StyledLine>) -> usize {
        if self.offset == 0 || newer.is_empty() {
            return 0;
        }
        let max_overlap = self.lines.len().min(newer.len());
        let duplicate = (1..=max_overlap)
            .rev()
            .find(|&count| {
                self.lines
                    .iter()
                    .skip(self.lines.len() - count)
                    .zip(newer.iter())
                    .all(|(left, right)| left == right)
            })
            .unwrap_or(0);
        newer.drain(..duplicate);
        let added = newer.len();
        self.lines.extend(newer);
        self.offset = self.offset.saturating_add(added);
        self.requested_offset = self.requested_offset.saturating_add(added);
        added
    }

    /// Prepend an older chunk fetched from tmux. Kept for small local tests
    /// and callers that already own an exact, non-overlapping page.
    #[cfg(test)]
    pub fn absorb(&mut self, older: Vec<StyledLine>, fetched_to: i64, top_reached: bool) {
        for line in older.into_iter().rev() {
            self.lines.push_front(line);
        }
        self.fetched_to = fetched_to;
        self.top_reached = top_reached;
        self.fetch_pending = false;
        if self.top_reached {
            self.requested_offset = self.requested_offset.min(self.lines.len());
        }
        self.offset = self.requested_offset.min(self.lines.len());
    }

    pub fn reset(&mut self) {
        self.lines.clear();
        self.offset = 0;
        self.requested_offset = 0;
        self.fetched_to = 0;
        self.top_reached = false;
        self.fetch_pending = false;
        self.history_size = 0;
        self.epoch = self.epoch.wrapping_add(1);
        self.pending_request_id = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeTmuxHistory {
        rows: Vec<StyledLine>,
        captures: Vec<(i64, i64)>,
    }

    impl FakeTmuxHistory {
        fn new(count: usize) -> Self {
            Self {
                rows: (0..count).map(numbered_line).collect(),
                captures: Vec::new(),
            }
        }

        fn capture(&mut self, cursor: FetchCursor) -> (Vec<StyledLine>, i64, i64, bool) {
            let history_size = self.rows.len() as i64;
            let (start, end) = Scrollback::capture_range(cursor, history_size).unwrap();
            self.captures.push((start, end));
            let absolute_start = (history_size + start) as usize;
            let absolute_end = (history_size + end) as usize;
            (
                self.rows[absolute_start..=absolute_end].to_vec(),
                start,
                history_size,
                start == -history_size,
            )
        }

        fn append_live(&mut self, count: usize) {
            let start = self.rows.len();
            self.rows.extend((start..start + count).map(numbered_line));
        }
    }

    fn numbered_line(number: usize) -> StyledLine {
        vec![StyledCell {
            c: 'x',
            zerowidth: Vec::new(),
            fg: Color::Named(alacritty_terminal::vte::ansi::NamedColor::Foreground),
            bg: Color::Named(alacritty_terminal::vte::ansi::NamedColor::Background),
            flags: Flags::empty(),
            hyperlink: Some(number.to_string()),
        }]
    }

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
    fn parse_capture_keeps_final_line_when_tmux_trims_terminal_newline() {
        let lines = parse_capture(b"one\ntwo", 40);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1][0].c, 't');
    }

    #[test]
    fn parse_joined_capture_restores_soft_wrap_flags() {
        let lines = parse_capture(b"abcdefgh\nnext", 4);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].last().unwrap().flags.contains(Flags::WRAPLINE));
        assert!(!lines[1].last().unwrap().flags.contains(Flags::WRAPLINE));
        assert_eq!(lines[2][0].c, 'n');
    }

    #[test]
    fn parse_capture_empty_input_returns_no_lines() {
        // Zero history or a failed capture-pane must not cache phantom lines.
        assert!(parse_capture(b"", 40).is_empty());
    }

    #[test]
    fn parse_capture_preserves_blank_history_rows() {
        let lines = parse_capture(b"\n\n\n", 40);
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().all(Vec::is_empty));
    }

    #[test]
    fn parse_capture_preserves_hyperlink() {
        let bytes = b"\x1b]8;;http://example.com\x1b\\click me\x1b]8;;\x1b\\\n";
        let lines = parse_capture(bytes, 40);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0][0].hyperlink.as_deref(), Some("http://example.com"));
        assert_eq!(lines[0][7].hyperlink.as_deref(), Some("http://example.com"));
    }

    #[test]
    fn parse_capture_preserves_rtl_and_combining_cells() {
        let lines = parse_capture("مرحبا ש\u{05b8}לום\n".as_bytes(), 40);
        let text: String = lines[0]
            .iter()
            .filter(|cell| !cell.flags.contains(Flags::WIDE_CHAR_SPACER))
            .flat_map(|cell| {
                std::iter::once(cell.c).chain(cell.zerowidth.iter().copied())
            })
            .collect();
        assert_eq!(text, "مرحبا ש\u{05b8}לום");
    }

    #[test]
    fn parse_capture_preserves_cjk_emoji_wide_spacers_and_combining_cells() {
        let lines = parse_capture("界🙂e\u{301}".as_bytes(), 8);
        assert!(lines[0][0].flags.contains(Flags::WIDE_CHAR));
        assert!(lines[0][1].flags.contains(Flags::WIDE_CHAR_SPACER));
        assert!(lines[0][2].flags.contains(Flags::WIDE_CHAR));
        assert!(lines[0][3].flags.contains(Flags::WIDE_CHAR_SPACER));
        assert_eq!(lines[0][4].zerowidth, vec!['\u{301}']);
    }

    #[test]
    fn scroll_bookkeeping_requests_fetch_at_cache_edge() {
        let mut sb = Scrollback::default();
        // Empty cache: any scroll up needs a fetch.
        assert!(sb.scroll_up(3));
        assert_eq!(sb.offset, 0, "offset must not exceed cached lines");

        sb.absorb(vec![vec![]; 100], -100, false);
        assert_eq!(sb.offset, 3, "initial intent applies when history arrives");
        assert!(!sb.scroll_up(30), "well within cache: no fetch needed");
        assert!(sb.scroll_up(10), "threshold crossing prefetches before the edge");
        assert_eq!(sb.offset, 43);
        sb.fetch_pending = true;
        assert!(!sb.scroll_up(60), "one in-flight fetch bounds repeated scroll work");
        assert_eq!(sb.offset, 100, "clamped to cached lines");

        sb.scroll_down(30);
        assert_eq!(sb.offset, 73);
        sb.scroll_down(1000);
        assert_eq!(sb.offset, 0);
    }

    #[test]
    fn scrolling_back_down_while_fetching_reduces_pending_intent() {
        let mut sb = Scrollback::default();
        assert!(sb.scroll_up(20));
        sb.fetch_pending = true;
        sb.scroll_down(15);
        sb.absorb(vec![vec![]; 100], -100, false);
        assert_eq!(sb.offset, 5);
    }

    #[test]
    fn capture_ranges_are_tail_first_bounded_and_shift_with_live_output() {
        let mut sb = Scrollback::default();
        assert!(sb.scroll_up(3));
        let first = sb.begin_fetch().unwrap();
        assert_eq!(Scrollback::capture_range(first, 50_000), Some((-256, -1)));

        let lines = vec![vec![]; 256];
        assert_eq!(
            sb.absorb_page(first, lines, -256, 50_000, false),
            AbsorbOutcome::Applied { prepended: 256 }
        );
        sb.scroll_up(200);
        let second = sb.begin_fetch().unwrap();
        assert_eq!(
            Scrollback::capture_range(second, 50_007),
            Some((-519, -256)),
            "seven live rows shift the old -256 boundary to -263"
        );
    }

    #[test]
    fn deterministic_fake_tmux_pages_long_chat_without_full_capture_or_race_duplicates() {
        let mut backend = FakeTmuxHistory::new(50_000);
        let mut sb = Scrollback::default();
        assert!(sb.scroll_up(3));
        let first = sb.begin_fetch().unwrap();
        let (page, start, size, top) = backend.capture(first);
        assert_eq!(page.len(), FETCH_CHUNK as usize);
        assert_eq!(backend.captures, vec![(-256, -1)]);
        assert_eq!(page.first().unwrap()[0].hyperlink.as_deref(), Some("49744"));
        assert_eq!(
            sb.absorb_page(first, page, start, size, top),
            AbsorbOutcome::Applied { prepended: 256 }
        );

        // Live PTY output advances every negative tmux coordinate between
        // pages. The overlapping request still finds the exact styled anchor.
        backend.append_live(5);
        sb.scroll_up(210);
        let second = sb.begin_fetch().unwrap();
        let (page, start, size, top) = backend.capture(second);
        assert_eq!(page.len(), MAX_CAPTURE_LINES);
        assert_eq!(backend.captures[1], (-517, -254));
        assert_eq!(
            sb.absorb_page(second, page, start, size, top),
            AbsorbOutcome::Applied { prepended: 256 }
        );
        assert_eq!(sb.lines.len(), 512);
        let ids: Vec<_> = sb
            .lines
            .iter()
            .map(|line| line[0].hyperlink.as_deref().unwrap())
            .collect();
        assert!(ids.windows(2).all(|pair| pair[0] != pair[1]));
        assert_eq!(ids.first().copied(), Some("49488"));
        assert_eq!(ids.last().copied(), Some("49999"));
    }

    #[test]
    fn overlapping_page_deduplicates_and_preserves_visible_anchor() {
        let mk = |value: usize| {
            vec![StyledCell {
                c: char::from_u32('A' as u32 + (value % 26) as u32).unwrap(),
                zerowidth: vec![char::from_u32(0x300 + (value % 16) as u32).unwrap()],
                fg: Color::Named(alacritty_terminal::vte::ansi::NamedColor::Foreground),
                bg: Color::Named(alacritty_terminal::vte::ansi::NamedColor::Background),
                flags: Flags::empty(),
                hyperlink: None,
            }]
        };
        let mut sb = Scrollback::default();
        assert!(sb.scroll_up(3));
        let first = sb.begin_fetch().unwrap();
        sb.absorb_page(first, (100..356).map(&mk).collect(), -256, 10_000, false);
        sb.scroll_up(210);
        let before = sb.line_above(sb.offset - 1).cloned();
        let offset = sb.offset;
        let second = sb.begin_fetch().unwrap();
        let page = (0..108).map(&mk).chain((100..108).map(&mk)).collect();
        assert_eq!(
            sb.absorb_page(second, page, -512, 10_000, false),
            AbsorbOutcome::Applied { prepended: 108 }
        );
        assert_eq!(sb.offset, offset);
        assert_eq!(sb.line_above(sb.offset - 1), before.as_ref());
        assert_eq!(sb.lines.len(), 364);
    }

    #[test]
    fn stale_resize_response_and_truncated_history_cannot_mutate_view() {
        let mut sb = Scrollback::default();
        assert!(sb.scroll_up(5));
        let stale = sb.begin_fetch().unwrap();
        sb.reset();
        assert_eq!(
            sb.absorb_page(stale, vec![vec![]], -1, 1, true),
            AbsorbOutcome::Stale
        );

        assert!(sb.scroll_up(5));
        let first = sb.begin_fetch().unwrap();
        sb.absorb_page(first, vec![vec![]; 100], -100, 1_000, false);
        sb.scroll_up(50);
        let next = sb.begin_fetch().unwrap();
        assert_eq!(
            sb.absorb_page(next, vec![vec![]; 20], -120, 0, true),
            AbsorbOutcome::Truncated
        );
        assert_eq!(sb.offset, 0);
        assert!(sb.lines.is_empty());
    }

    #[test]
    fn stale_truncation_cannot_reset_a_superseding_fetch() {
        let mut sb = Scrollback::default();
        assert!(sb.scroll_up(5));
        let stale = sb.begin_fetch().unwrap();
        sb.reset();
        assert!(sb.scroll_up(5));
        let current = sb.begin_fetch().unwrap();

        assert_eq!(sb.absorb_truncated(stale), AbsorbOutcome::Stale);
        assert!(sb.fetch_pending);
        assert_eq!(
            sb.absorb_truncated(current),
            AbsorbOutcome::Truncated
        );
        assert!(!sb.fetch_pending);
        assert_eq!(sb.offset, 0);
    }

    #[test]
    fn live_scroll_append_preserves_anchor_and_deduplicates_boundary() {
        let line = |c| vec![StyledCell {
            c,
            zerowidth: Vec::new(),
            fg: Color::Named(alacritty_terminal::vte::ansi::NamedColor::Foreground),
            bg: Color::Named(alacritty_terminal::vte::ansi::NamedColor::Background),
            flags: Flags::empty(),
            hyperlink: None,
        }];
        let mut sb = Scrollback::default();
        sb.absorb(vec![line('a'), line('b'), line('c')], -3, false);
        sb.scroll_up(2);
        let anchor = sb.line_above(sb.offset - 1).cloned();
        assert_eq!(sb.append_live_scrolled(vec![line('c'), line('d')]), 1);
        assert_eq!(sb.offset, 3);
        assert_eq!(sb.line_above(sb.offset - 1), anchor.as_ref());
        assert_eq!(sb.lines.len(), 4);
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
        let mk = |ch: char| {
            vec![StyledCell {
            c: ch,
            zerowidth: Vec::new(),
                fg: alacritty_terminal::vte::ansi::Color::Named(
                    alacritty_terminal::vte::ansi::NamedColor::Foreground,
                ),
                bg: alacritty_terminal::vte::ansi::Color::Named(
                    alacritty_terminal::vte::ansi::NamedColor::Background,
                ),
            flags: alacritty_terminal::term::cell::Flags::empty(),
            hyperlink: None,
            }]
        };
        // Oldest-first storage: a then b; b is directly above the screen.
        sb.absorb(vec![mk('a'), mk('b')], -2, true);
        assert_eq!(sb.line_above(0).unwrap()[0].c, 'b');
        assert_eq!(sb.line_above(1).unwrap()[0].c, 'a');
        assert!(sb.line_above(2).is_none());
    }
}
