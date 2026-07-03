use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::Term;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, Rgb};
use iced::widget::canvas::{Cache, Frame, Geometry, Path};
use iced::{Color as IcedColor, Rectangle, Size, Theme};

use crate::app::Message;

const NERD_FONT: iced::Font = iced::Font {
    family: iced::font::Family::Name("Symbols Nerd Font Mono"),
    weight: iced::font::Weight::Normal,
    stretch: iced::font::Stretch::Normal,
    style: iced::font::Style::Normal,
};

/// The single source of truth for the terminal's font size — every layout
/// computation (canvas rendering, mouse hit-testing, and the tmux grid
/// sizing in `app::App::resize_terminals`) must derive cell dimensions from
/// this constant via `cell_size()` so they can never drift apart.
pub const FONT_SIZE: f32 = 13.0;

/// Monospace cell size (width, height) in pixels for a given font size —
/// the same approximation used everywhere a terminal cell is measured.
pub fn cell_size(font_size: f32) -> (f32, f32) {
    (font_size * 0.6, font_size * 1.4)
}

// ---------------------------------------------------------------------------
// EventProxy
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct EventProxy;

impl alacritty_terminal::event::EventListener for EventProxy {
    fn send_event(&self, _: alacritty_terminal::event::Event) {}
}

// ---------------------------------------------------------------------------
// TerminalState — holds the terminal buffer + PTY sender
// ---------------------------------------------------------------------------

pub struct TerminalState {
    pub term: Term<EventProxy>,
    pub cache: Cache,
    parser: Processor,
    /// Viewport content as of the last time we let an ESC[2J actually scroll
    /// it into scrollback history. Lets `process` tell a genuine content
    /// change (push it before it's erased) apart from a TUI reissuing a
    /// full-screen redraw identical to one already preserved (suppress the
    /// duplicate). See `process` for why this exists.
    last_pushed_frame: Option<String>,
}

impl TerminalState {
    /// Feed raw bytes from the PTY into the VTE parser → terminal state.
    pub fn process(&mut self, bytes: &[u8]) {
        // ESC[2J (erase entire screen) calls clear_viewport(), which scrolls
        // the current viewport into scrollback history. TUI apps like Claude
        // Code redraw their whole screen via ESC[2J on every update — fine
        // when the outgoing screen carries content we haven't preserved yet,
        // but if the TUI reissues an *identical* frame (e.g. a spinner tick
        // with no textual change), passing every ESC[2J through floods
        // scrollback with repeated copies of the same screen. Suppressing
        // every ESC[2J unconditionally (the previous fix) swings too far the
        // other way: well-behaved TUIs clip their rendering to the terminal's
        // row count and never overflow it naturally, so nothing ever scrolls
        // into history and scrollback never grows at all.
        //
        // Instead, only push when the outgoing viewport differs from the
        // last frame we chose to preserve — this collapses runs of identical
        // redraws to a single scrollback copy while still capturing content
        // the moment it's actually superseded by something new.
        //
        // Note: split sequences across chunk boundaries are not handled here;
        // in practice tmux pipe-pane delivers complete sequences in single
        // 4 KiB reads.
        const NEEDLE: &[u8] = b"\x1b[2J";
        const REPLACE: &[u8] = b"\x1b[H\x1b[0J";

        if bytes.windows(NEEDLE.len()).any(|w| w == NEEDLE) {
            let mut i = 0;
            while let Some(rel) = bytes[i..].windows(NEEDLE.len()).position(|w| w == NEEDLE) {
                if rel > 0 {
                    self.parser.advance(&mut self.term, &bytes[i..i + rel]);
                }
                let outgoing = viewport_text(&self.term);
                if self.last_pushed_frame.as_ref() == Some(&outgoing) {
                    self.parser.advance(&mut self.term, REPLACE);
                } else {
                    self.parser.advance(&mut self.term, NEEDLE);
                    self.last_pushed_frame = Some(outgoing);
                }
                i += rel + NEEDLE.len();
            }
            self.parser.advance(&mut self.term, &bytes[i..]);
        } else {
            self.parser.advance(&mut self.term, bytes);
        }
        self.cache.clear();
    }

    /// Resize the terminal grid to match a new canvas size.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        use alacritty_terminal::term::test::TermSize;
        let size = TermSize::new(cols as usize, rows as usize);
        self.term.resize(size);
        self.cache.clear();
    }
}

// ---------------------------------------------------------------------------
// Color conversion
// ---------------------------------------------------------------------------

/// Default terminal color palette (xterm-256 approximations for named colors).
const DEFAULT_PALETTE: &[(u8, u8, u8)] = &[
    (0x28, 0x28, 0x28),   // 0  Black
    (0xcc, 0x24, 0x1d),   // 1  Red
    (0x98, 0x97, 0x1a),   // 2  Green
    (0xd7, 0x99, 0x21),   // 3  Yellow
    (0x45, 0x85, 0x88),   // 4  Blue
    (0xb1, 0x62, 0x86),   // 5  Magenta
    (0x68, 0x9d, 0x6a),   // 6  Cyan
    (0xa8, 0x99, 0x84),   // 7  White
    (0x92, 0x83, 0x74),   // 8  BrightBlack
    (0xfb, 0x49, 0x34),   // 9  BrightRed
    (0xb8, 0xbb, 0x26),   // 10 BrightGreen
    (0xfa, 0xbd, 0x2f),   // 11 BrightYellow
    (0x83, 0xa5, 0x98),   // 12 BrightBlue
    (0xd3, 0x86, 0x9b),   // 13 BrightMagenta
    (0x8e, 0xc0, 0x7c),   // 14 BrightCyan
    (0xeb, 0xdb, 0xb2),   // 15 BrightWhite
];

fn rgb_to_iced(rgb: Rgb) -> IcedColor {
    IcedColor::from_rgb8(rgb.r, rgb.g, rgb.b)
}

fn named_to_iced(named: NamedColor, bg: IcedColor, fg: IcedColor) -> IcedColor {
    // Use our default palette for the first 16 named colors.
    let idx = named as usize;
    if idx < DEFAULT_PALETTE.len() {
        let (r, g, b) = DEFAULT_PALETTE[idx];
        return IcedColor::from_rgb8(r, g, b);
    }
    // Foreground / Background fallbacks use the active theme colors.
    match named {
        NamedColor::Foreground | NamedColor::BrightForeground => fg,
        NamedColor::Background => bg,
        _ => fg,
    }
}

/// Convert an alacritty `Color` to an iced `Color`, consulting the dynamic
/// color table for indexed colors where possible.
pub fn ansi_to_iced(
    color: Color,
    colors: &alacritty_terminal::term::color::Colors,
    bg: IcedColor,
    fg: IcedColor,
) -> IcedColor {
    match color {
        Color::Named(named) => {
            // Prefer the dynamic table entry if present.
            if let Some(rgb) = colors[named] {
                return rgb_to_iced(rgb);
            }
            named_to_iced(named, bg, fg)
        }
        Color::Spec(rgb) => rgb_to_iced(rgb),
        Color::Indexed(idx) => {
            if let Some(rgb) = colors[idx as usize] {
                return rgb_to_iced(rgb);
            }
            // 256-color cube / grayscale fallback.
            if idx < 16 {
                let (r, g, b) = DEFAULT_PALETTE[idx as usize];
                IcedColor::from_rgb8(r, g, b)
            } else if idx < 232 {
                let n = idx - 16;
                let b = (n % 6) * 51;
                let g = ((n / 6) % 6) * 51;
                let r = (n / 36) * 51;
                IcedColor::from_rgb8(r, g, b)
            } else {
                let v = 8 + (idx - 232) * 10;
                IcedColor::from_rgb8(v, v, v)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// SelectionState — tracks mouse drag selection within the canvas
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
pub struct SelectionState {
    /// Anchor cell (col, row) where the drag started.
    anchor: Option<(usize, usize)>,
    /// Current end cell while dragging.
    end: Option<(usize, usize)>,
    dragging: bool,
    /// Whether the cursor moved after the press (distinguishes click from drag).
    moved: bool,
}

impl SelectionState {
    /// Normalised (start, end) in reading order, or None if no selection.
    fn range(&self) -> Option<((usize, usize), (usize, usize))> {
        let (a_col, a_row) = self.anchor?;
        let (e_col, e_row) = self.end?;
        if a_row < e_row || (a_row == e_row && a_col <= e_col) {
            Some(((a_col, a_row), (e_col, e_row)))
        } else {
            Some(((e_col, e_row), (a_col, a_row)))
        }
    }

    fn pixel_to_cell(x: f32, y: f32, cell_w: f32, cell_h: f32, cols: usize, rows: usize) -> (usize, usize) {
        let col = ((x / cell_w) as usize).min(cols.saturating_sub(1));
        let row = ((y / cell_h) as usize).min(rows.saturating_sub(1));
        (col, row)
    }
}

// ---------------------------------------------------------------------------
// TerminalWidget — iced canvas Program
// ---------------------------------------------------------------------------

/// A view of a `TerminalState` that can be used as an iced Canvas widget.
pub struct TerminalWidget<'a> {
    pub state:       &'a TerminalState,
    pub session_id:  String,
    pub font_size:   f32,
    pub terminal_bg: IcedColor,
    pub terminal_fg: IcedColor,
    pub cursor_color: IcedColor,
    /// Known session IDs — a single click on a matching word navigates to that session.
    pub session_ids: Vec<String>,
}

impl<'a> iced::widget::canvas::Program<Message> for TerminalWidget<'a> {
    type State = SelectionState;

    fn update(
        &self,
        state: &mut Self::State,
        event: iced::widget::canvas::Event,
        bounds: Rectangle,
        cursor: iced::mouse::Cursor,
    ) -> (iced::widget::canvas::event::Status, Option<Message>) {
        use iced::keyboard::Event as KeyEvent;
        use iced::mouse::{Button, Event as MouseEvent};
        use iced::widget::canvas::Event;

        let (cell_w, cell_h) = cell_size(self.font_size);
        let cols = self.state.term.grid().columns();
        let rows = self.state.term.grid().screen_lines();

        match &event {
            Event::Mouse(MouseEvent::ButtonPressed(Button::Left)) => {
                if let Some(pos) = cursor.position_in(bounds) {
                    let cell = SelectionState::pixel_to_cell(pos.x, pos.y, cell_w, cell_h, cols, rows);
                    state.anchor   = Some(cell);
                    state.end      = Some(cell);
                    state.dragging = true;
                    state.moved    = false;
                }
                return (iced::widget::canvas::event::Status::Captured, None);
            }

            Event::Mouse(MouseEvent::CursorMoved { .. }) if state.dragging => {
                if let Some(pos) = cursor.position_in(bounds) {
                    let cell = SelectionState::pixel_to_cell(pos.x, pos.y, cell_w, cell_h, cols, rows);
                    if state.anchor != Some(cell) {
                        state.moved = true;
                    }
                    state.end = Some(cell);
                    self.state.cache.clear();
                }
                return (iced::widget::canvas::event::Status::Captured, None);
            }

            Event::Mouse(MouseEvent::ButtonReleased(Button::Left)) if state.dragging => {
                state.dragging = false;
                if !state.moved {
                    // Single click — check for a session ID under the cursor.
                    if let (Some((col, row)), Some(pos)) = (state.anchor, cursor.position_in(bounds)) {
                        let _ = pos; // bounds-checked via anchor
                        let word = word_at(self.state.term.grid(), col, row);
                        if self.session_ids.iter().any(|id| id == &word) {
                            state.anchor = None;
                            state.end    = None;
                            return (iced::widget::canvas::event::Status::Captured,
                                    Some(Message::NavigateSession(word)));
                        }
                    }
                    state.anchor = None;
                    state.end    = None;
                    return (iced::widget::canvas::event::Status::Captured, None);
                }
                // Drag — copy the selection.
                let text = state.range().map(|((sc, sr), (ec, er))| {
                    extract_selection(&self.state.term, sc, sr, ec, er)
                });
                if let Some(t) = text.filter(|s| !s.trim().is_empty()) {
                    return (iced::widget::canvas::event::Status::Captured,
                            Some(Message::CopyToClipboard(t)));
                }
                return (iced::widget::canvas::event::Status::Captured, None);
            }

            // Emit RawKey so the handler can apply APP_CURSOR-aware conversion.
            Event::Keyboard(KeyEvent::KeyPressed { key, modifiers, text, .. }) => {
                let msg = Message::RawKey {
                    key: key.clone(),
                    modifiers: *modifiers,
                    text: text.as_ref().map(|t| t.as_str().to_string()),
                };
                return (iced::widget::canvas::event::Status::Captured, Some(msg));
            }

            Event::Mouse(MouseEvent::WheelScrolled { delta }) => {
                use iced::mouse::ScrollDelta;
                // Positive y = scroll up (into history). Convert to integer line delta.
                let lines = match delta {
                    ScrollDelta::Lines { y, .. }  => (*y * 3.0) as i32,
                    ScrollDelta::Pixels { y, .. } => (*y / cell_h) as i32,
                };
                if lines != 0 {
                    return (
                        iced::widget::canvas::event::Status::Captured,
                        Some(Message::ScrollTerminal { session_id: self.session_id.clone(), delta: lines }),
                    );
                }
            }

            _ => {}
        }

        (iced::widget::canvas::event::Status::Ignored, None)
    }

    fn draw(
        &self,
        sel: &Self::State,
        renderer: &iced::Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: iced::mouse::Cursor,
    ) -> Vec<Geometry> {
        // Cell dimensions based on font_size (monospace approximation).
        let (cell_w, cell_h) = cell_size(self.font_size);

        let term = &self.state.term;
        let grid = term.grid();
        let colors = term.colors();
        let cols = grid.columns();
        let rows = grid.screen_lines();
        let cursor_point   = grid.cursor.point;
        let display_offset = grid.display_offset() as i32;

        let term_bg = self.terminal_bg;
        let term_fg = self.terminal_fg;
        let cursor_color = self.cursor_color;

        let geometry = self.state.cache.draw(renderer, bounds.size(), |frame: &mut Frame| {
            // Background fill.
            let bg_all = Path::rectangle(iced::Point::ORIGIN, bounds.size());
            frame.fill(&bg_all, term_bg);

            // Render each cell. When scrolled into history, shift line indices by
            // display_offset so Line(0 - offset) accesses the history buffer.
            for row in 0..rows {
                for col in 0..cols {
                    use alacritty_terminal::index::{Column, Line};

                    let line   = Line(row as i32 - display_offset);
                    let column = Column(col);
                    let cell = &grid[line][column];

                    let x = col as f32 * cell_w;
                    let y = row as f32 * cell_h;

                    // Background.
                    let bg = ansi_to_iced(cell.bg, colors, term_bg, term_fg);
                    let is_cursor = cursor_point.line == line && cursor_point.column == column;

                    let is_selected = sel.range().map(|((sc, sr), (ec, er))| {
                        let in_row = row >= sr && row <= er;
                        if !in_row { return false; }
                        if sr == er { col >= sc && col <= ec }
                        else if row == sr { col >= sc }
                        else if row == er { col <= ec }
                        else { true }
                    }).unwrap_or(false);

                    if is_selected {
                        let sel_rect = Path::rectangle(
                            iced::Point::new(x, y),
                            Size::new(cell_w, cell_h),
                        );
                        frame.fill(&sel_rect, IcedColor { r: 0.27, g: 0.52, b: 0.80, a: 0.5 });
                    } else if is_cursor {
                        let cursor_rect = Path::rectangle(
                            iced::Point::new(x, y),
                            Size::new(cell_w, cell_h),
                        );
                        frame.fill(&cursor_rect, cursor_color);
                    } else if bg != term_bg {
                        let bg_rect = Path::rectangle(
                            iced::Point::new(x, y),
                            Size::new(cell_w, cell_h),
                        );
                        frame.fill(&bg_rect, bg);
                    }

                    // Foreground text.
                    let ch = cell.c;
                    if ch != ' ' && ch != '\0' {
                        let fg = if is_cursor {
                            term_bg
                        } else {
                            ansi_to_iced(cell.fg, colors, term_bg, term_fg)
                        };

                        let cp = ch as u32;
                        let font = if (0xE000..=0xF8FF).contains(&cp) {
                            NERD_FONT
                        } else {
                            iced::Font::MONOSPACE
                        };
                        frame.fill_text(iced::widget::canvas::Text {
                            content: ch.to_string(),
                            position: iced::Point::new(x, y),
                            color: fg,
                            size: iced::Pixels(self.font_size),
                            font,
                            horizontal_alignment: iced::alignment::Horizontal::Left,
                            vertical_alignment: iced::alignment::Vertical::Top,
                            line_height: iced::widget::text::LineHeight::Relative(
                                cell_h / self.font_size,
                            ),
                            shaping: iced::widget::text::Shaping::Advanced,
                        });
                    }
                }
            }
        });

        vec![geometry]
    }
}

// ---------------------------------------------------------------------------
// Text extraction
// ---------------------------------------------------------------------------

/// Extract the word (alphanumeric + hyphen) under a cell — used to detect
/// session IDs like `worker-1782906415516` for click-to-navigate.
pub fn word_at(grid: &alacritty_terminal::grid::Grid<alacritty_terminal::term::cell::Cell>, col: usize, row: usize) -> String {
    use alacritty_terminal::index::{Column, Line};

    let cols = grid.columns();
    let rows = grid.screen_lines();
    if row >= rows || col >= cols { return String::new(); }

    let is_word = |c: char| c.is_alphanumeric() || c == '-';

    let mut start = col;
    while start > 0 {
        let c = grid[Line(row as i32)][Column(start - 1)].c;
        if !is_word(c) && c != '\0' { break; }
        start -= 1;
    }
    let mut end = col;
    while end + 1 < cols {
        let c = grid[Line(row as i32)][Column(end + 1)].c;
        if !is_word(c) && c != '\0' { break; }
        end += 1;
    }

    (start..=end).map(|c| {
        let ch = grid[Line(row as i32)][Column(c)].c;
        if ch == '\0' { ' ' } else { ch }
    }).collect::<String>().trim().to_string()
}

pub fn extract_selection(
    term: &Term<EventProxy>,
    start_col: usize, start_row: usize,
    end_col: usize,   end_row: usize,
) -> String {
    use alacritty_terminal::index::{Column, Line};

    let grid = term.grid();
    let cols = grid.columns();
    let rows = grid.screen_lines();
    let mut out = String::new();

    for row in start_row..=end_row.min(rows.saturating_sub(1)) {
        let col_start = if row == start_row { start_col } else { 0 };
        let col_end   = if row == end_row   { end_col   } else { cols.saturating_sub(1) };

        let mut line_text = String::new();
        for col in col_start..=col_end.min(cols.saturating_sub(1)) {
            let cell = &grid[Line(row as i32)][Column(col)];
            line_text.push(if cell.c == '\0' { ' ' } else { cell.c });
        }
        // Strip trailing spaces from each line.
        let trimmed = line_text.trim_end();
        out.push_str(trimmed);
        if row < end_row { out.push('\n'); }
    }
    out
}

// ---------------------------------------------------------------------------
// Viewport snapshotting
// ---------------------------------------------------------------------------

/// Render the current (unscrolled) viewport as plain text — used by
/// `TerminalState::process` to detect when a TUI reissues a full-screen
/// redraw identical to one it has already preserved in scrollback.
fn viewport_text(term: &Term<EventProxy>) -> String {
    use alacritty_terminal::index::{Column, Line};

    let grid = term.grid();
    let cols = grid.columns();
    let rows = grid.screen_lines();
    let mut out = String::with_capacity(rows * (cols + 1));

    for row in 0..rows {
        let line = Line(row as i32);
        for col in 0..cols {
            out.push(grid[line][Column(col)].c);
        }
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

impl TerminalState {
    pub fn new(cols: u16, rows: u16) -> Self {
        use alacritty_terminal::term::{Config, test::TermSize};
        let size = TermSize::new(cols as usize, rows as usize);
        let term = Term::new(Config::default(), &size, EventProxy);
        Self {
            term,
            cache: Cache::new(),
            parser: Processor::new(),
            last_pushed_frame: None,
        }
    }

    /// Scroll the terminal display by `delta` lines (positive = up into history).
    pub fn scroll(&mut self, delta: i32) {
        use alacritty_terminal::grid::Scroll;
        self.term.grid_mut().scroll_display(Scroll::Delta(delta));
        self.cache.clear();
    }

    /// Jump the display back down to the live viewport.
    pub fn scroll_to_bottom(&mut self) {
        use alacritty_terminal::grid::Scroll;
        self.term.grid_mut().scroll_display(Scroll::Bottom);
        self.cache.clear();
    }

    /// Whether the display is currently scrolled up into history.
    pub fn is_scrolled_back(&self) -> bool {
        self.term.grid().display_offset() > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_advances_cursor() {
        let mut s = TerminalState::new(80, 24);
        s.process(b"hello");
        // After printing 5 chars, cursor should be at column 5.
        assert_eq!(s.term.grid().cursor.point.column.0, 5);
    }

    #[test]
    fn process_ansi_no_panic() {
        let mut s = TerminalState::new(80, 24);
        s.process(b"\x1b[31mred\x1b[0m");
    }

    /// Does any line currently in scrollback history contain `needle`?
    fn history_contains(term: &Term<EventProxy>, needle: &str) -> bool {
        use alacritty_terminal::index::{Column, Line};
        let grid = term.grid();
        let cols = grid.columns();
        let history = grid.history_size() as i32;
        (1..=history).any(|n| {
            let mut line_text = String::with_capacity(cols);
            for col in 0..cols {
                line_text.push(grid[Line(-n)][Column(col)].c);
            }
            line_text.contains(needle)
        })
    }

    #[test]
    fn identical_redraws_are_deduped_but_content_survives_eventual_change() {
        // Simulate an Ink-style TUI (like Claude Code) that redraws its whole
        // screen via ESC[2J + cursor-home on every update.
        let mut s = TerminalState::new(80, 10);
        let make_frame = |text: &str| {
            let mut f = Vec::new();
            f.extend_from_slice(b"\x1b[2J\x1b[H");
            f.extend_from_slice(text.as_bytes());
            f
        };

        s.process(&make_frame("first response"));
        s.process(&make_frame("first response")); // establishes the baseline push
        let after_first = s.term.grid().history_size();

        // Reissuing the identical frame (e.g. a spinner tick with no
        // textual change) must not add further copies to scrollback.
        for _ in 0..5 {
            s.process(&make_frame("first response"));
        }
        assert_eq!(
            s.term.grid().history_size(),
            after_first,
            "identical redraws must not flood scrollback with duplicate frames"
        );

        // The conversation moves on — the old message must still be
        // reachable in scrollback, not silently discarded.
        s.process(&make_frame("second response"));
        assert!(
            history_contains(&s.term, "first response"),
            "content must survive being scrolled out by a genuine redraw"
        );
    }

    #[test]
    fn growing_tui_content_enters_scrollback() {
        // A TUI that redraws the whole screen via ESC[2J on every update,
        // appending one more line of "conversation" each time — genuinely
        // new content every frame, not a duplicate.
        let mut s = TerminalState::new(80, 10);
        for i in 0..30 {
            let mut frame = Vec::new();
            frame.extend_from_slice(b"\x1b[2J\x1b[H");
            for line in 0..=i {
                frame.extend_from_slice(format!("message {line}\r\n").as_bytes());
            }
            s.process(&frame);
        }
        assert!(
            s.term.grid().history_size() > 0,
            "growing TUI content should be scrollable, but history_size() is 0"
        );
    }

    #[test]
    fn clipped_tui_content_enters_scrollback() {
        // Well-behaved TUIs (Ink/ratatui/blessed) clip their own rendering to
        // the terminal's row count and never emit more lines than fit on
        // screen — they manage their own internal scroll/pager and rely on
        // the terminal only for the *current* viewport. Each frame here is
        // always <= 10 rows, but represents genuinely NEW conversation
        // content sliding through a 10-row window (message 0..9, then
        // message 1..10, etc.) — exactly like Claude Code showing only the
        // last N lines of an ever-growing transcript.
        let mut s = TerminalState::new(80, 10);
        for i in 0u32..30 {
            let mut frame = Vec::new();
            frame.extend_from_slice(b"\x1b[2J\x1b[H");
            let start = i.saturating_sub(9);
            let lines: Vec<String> = (start..=i).map(|line| format!("message {line}")).collect();
            // Join with \r\n *between* lines only — a real full-frame redraw
            // leaves the cursor on the last row, it doesn't overflow past it.
            frame.extend_from_slice(lines.join("\r\n").as_bytes());
            s.process(&frame);
        }
        assert!(
            s.term.grid().history_size() > 0,
            "clipped TUI content should still be scrollable, but history_size() is 0"
        );
    }
}
