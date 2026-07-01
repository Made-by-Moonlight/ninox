use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::Term;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, Rgb};
use iced::widget::canvas::{Cache, Frame, Geometry, Path};
use iced::{Color as IcedColor, Rectangle, Size, Theme};

use crate::app::Message;

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
}

impl TerminalState {
    /// Feed raw bytes from the PTY into the VTE parser → terminal state.
    pub fn process(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
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
    pub state: &'a TerminalState,
    pub font_size: f32,
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

        let cell_w = self.font_size * 0.6;
        let cell_h = self.font_size * 1.4;
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
        let cell_w = self.font_size * 0.6;
        let cell_h = self.font_size * 1.4;

        let term = &self.state.term;
        let grid = term.grid();
        let colors = term.colors();
        let cols = grid.columns();
        let rows = grid.screen_lines();
        let cursor_point = grid.cursor.point;

        let term_bg = self.terminal_bg;
        let term_fg = self.terminal_fg;
        let cursor_color = self.cursor_color;

        let geometry = self.state.cache.draw(renderer, bounds.size(), |frame: &mut Frame| {
            // Background fill.
            let bg_all = Path::rectangle(iced::Point::ORIGIN, bounds.size());
            frame.fill(&bg_all, term_bg);

            // Render each cell.
            for row in 0..rows {
                for col in 0..cols {
                    use alacritty_terminal::index::{Column, Line};

                    let line = Line(row as i32);
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

                        frame.fill_text(iced::widget::canvas::Text {
                            content: ch.to_string(),
                            position: iced::Point::new(x, y),
                            color: fg,
                            size: iced::Pixels(self.font_size),
                            font: iced::Font::MONOSPACE,
                            horizontal_alignment: iced::alignment::Horizontal::Left,
                            vertical_alignment: iced::alignment::Vertical::Top,
                            line_height: iced::widget::text::LineHeight::Relative(
                                cell_h / self.font_size,
                            ),
                            shaping: iced::widget::text::Shaping::Basic,
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
        }
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
}
