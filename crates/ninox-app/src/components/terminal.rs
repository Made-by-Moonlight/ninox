use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::Term;
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Processor, Rgb};
use iced::widget::canvas::{Cache, Frame, Geometry, Path};
use iced::{Color as IcedColor, Rectangle, Size, Theme};

use crate::app::Message;

const NERD_FONT: iced::Font = iced::Font {
    family: iced::font::Family::Name("Symbols Nerd Font Mono"),
    weight: iced::font::Weight::Normal,
    stretch: iced::font::Stretch::Normal,
    style: iced::font::Style::Normal,
};

pub const TERM_FONT_BYTES: &[u8] =
    include_bytes!("../../assets/fonts/JetBrainsMono-Regular.ttf");

pub const TERM_FONT: iced::Font = iced::Font {
    family:  iced::font::Family::Name("JetBrains Mono"),
    weight:  iced::font::Weight::Normal,
    stretch: iced::font::Stretch::Normal,
    style:   iced::font::Style::Normal,
};

/// The single source of truth for the terminal's font size — every layout
/// computation (canvas rendering, mouse hit-testing, and the tmux grid
/// sizing in `app::App::resize_terminals`) must derive cell dimensions from
/// this constant via `cell_size()` so they can never drift apart.
pub const FONT_SIZE: f32 = 13.0;

/// Monospace cell size (width, height) in pixels, measured once from the
/// bundled font's tables — canvas drawing, hit-testing, and PTY sizing all
/// derive from this so they can never drift apart.
pub fn cell_size(font_size: f32) -> (f32, f32) {
    use std::sync::OnceLock;
    static RATIOS: OnceLock<(f32, f32)> = OnceLock::new();
    let (w, h) = *RATIOS.get_or_init(|| {
        let face = ttf_parser::Face::parse(TERM_FONT_BYTES, 0)
            .expect("bundled terminal font parses");
        let upem = face.units_per_em() as f32;
        let advance = face
            .glyph_index('M')
            .and_then(|g| face.glyph_hor_advance(g))
            .expect("monospace advance") as f32;
        let height = (face.ascender() as f32 - face.descender() as f32
            + face.line_gap() as f32).max(upem);
        (advance / upem, height / upem)
    });
    (font_size * w, font_size * h)
}

// ---------------------------------------------------------------------------
// EventProxy
// ---------------------------------------------------------------------------

/// Forwards emulator-generated replies (cursor position reports, device
/// attributes, kitty keyboard responses) back to the PTY. `None` (tests,
/// sessions with no attached client) silently drops them.
#[derive(Clone)]
pub struct EventProxy(Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>);

impl alacritty_terminal::event::EventListener for EventProxy {
    fn send_event(&self, event: alacritty_terminal::event::Event) {
        if let alacritty_terminal::event::Event::PtyWrite(text) = event {
            if let Some(tx) = &self.0 {
                let _ = tx.send(text.into_bytes());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// TerminalState — holds the terminal buffer + PTY sender
// ---------------------------------------------------------------------------

pub struct TerminalState {
    pub term: Term<EventProxy>,
    pub cache: Cache,
    /// On-demand tmux history cache + scroll position for this view. The
    /// live alacritty grid never accumulates scrollback of its own (the
    /// client stream is a full-screen tmux UI), so all history rendering
    /// comes from here.
    pub scrollback: crate::components::scrollback::Scrollback,
    parser: Processor,
}

impl TerminalState {
    pub fn new(
        cols: u16,
        rows: u16,
        reply: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    ) -> Self {
        use alacritty_terminal::term::{Config, test::TermSize};
        let size = TermSize::new(cols as usize, rows as usize);
        let config = Config { kitty_keyboard: true, ..Config::default() };
        let term = Term::new(config, &size, EventProxy(reply));
        Self {
            term,
            cache: Cache::new(),
            scrollback: Default::default(),
            parser: Processor::new(),
        }
    }

    /// Feed raw bytes from the attached tmux client into the emulator.
    pub fn process(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
        self.cache.clear();
    }

    /// Scroll by `delta` lines (positive = up). Returns true when older
    /// history must be fetched from tmux.
    pub fn scroll(&mut self, delta: i32) -> bool {
        let needs_fetch = if delta > 0 {
            self.scrollback.scroll_up(delta as usize)
        } else {
            self.scrollback.scroll_down((-delta) as usize);
            false
        };
        self.cache.clear();
        needs_fetch
    }

    /// Jump the display back down to the live viewport.
    pub fn scroll_to_bottom(&mut self) {
        self.scrollback.offset = 0;
        self.cache.clear();
    }

    /// Whether the display is currently scrolled up into history.
    pub fn is_scrolled_back(&self) -> bool {
        self.scrollback.offset > 0
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

fn rgb_to_iced(rgb: Rgb) -> IcedColor {
    IcedColor::from_rgb8(rgb.r, rgb.g, rgb.b)
}

/// Convert an alacritty `Color` to an iced `Color`, consulting the dynamic
/// color table for indexed colors where possible, and otherwise the active
/// theme's 16-entry ANSI palette (`ColorScheme::ansi`).
pub fn ansi_to_iced(
    color: Color,
    colors: &alacritty_terminal::term::color::Colors,
    ansi: &[IcedColor; 16],
    bg: IcedColor,
    fg: IcedColor,
) -> IcedColor {
    match color {
        Color::Named(named) => {
            // Prefer the dynamic table entry if present.
            if let Some(rgb) = colors[named] {
                return rgb_to_iced(rgb);
            }
            let idx = named as usize;
            if idx < 16 {
                return ansi[idx];
            }
            // Foreground / Background fallbacks use the active theme colors.
            match named {
                NamedColor::Foreground | NamedColor::BrightForeground => fg,
                NamedColor::Background => bg,
                _ => fg,
            }
        }
        Color::Spec(rgb) => rgb_to_iced(rgb),
        Color::Indexed(idx) => {
            if let Some(rgb) = colors[idx as usize] {
                return rgb_to_iced(rgb);
            }
            // 256-color cube / grayscale fallback.
            if idx < 16 {
                ansi[idx as usize]
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
    /// The active theme's 16-entry ANSI palette — see `ColorScheme::ansi`.
    pub ansi: [IcedColor; 16],
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
        let cursor_point = grid.cursor.point;
        // How many rows the view is scrolled up into tmux history. The
        // native alacritty grid holds no scrollback of its own (the client
        // stream is a full-screen tmux UI), so this offset is entirely
        // driven by `Scrollback`, not `grid.display_offset()`.
        let offset = self.state.scrollback.offset as i32;

        let term_bg = self.terminal_bg;
        let term_fg = self.terminal_fg;
        let cursor_color = self.cursor_color;
        // Shape + blink from DECSCUSR. Blink is deliberately not animated —
        // see the module-level note on steady cursor rendering.
        let cursor_shape = term.cursor_style().shape;

        let geometry = self.state.cache.draw(renderer, bounds.size(), |frame: &mut Frame| {
            // Background fill.
            let bg_all = Path::rectangle(iced::Point::ORIGIN, bounds.size());
            frame.fill(&bg_all, term_bg);

            // Compose the viewport as history-above + live-below: rows whose
            // logical index is negative are served from the tmux scrollback
            // cache, the rest from the live grid.
            for row in 0..rows {
                use alacritty_terminal::index::{Column, Line};

                let logical = row as i32 - offset;
                let y = row as f32 * cell_h;

                if logical < 0 {
                    // History line fetched from tmux capture-pane. Content
                    // between the cached snapshot and the live screen may be
                    // stale/missing until jump-to-latest — accepted
                    // trade-off, not a bug (see Scrollback docs).
                    let Some(cells) = self.state.scrollback.line_above((-logical - 1) as usize)
                    else {
                        continue;
                    };
                    for (col, cell) in cells.iter().enumerate().take(cols) {
                        let x = col as f32 * cell_w;
                        let is_selected = cell_is_selected(sel, row, col);
                        draw_cell(
                            frame, x, y, cell_w, cell_h, self.font_size,
                            cell.c, cell.fg, cell.bg, cell.flags,
                            false, // cursor never draws in history
                            cursor_shape, is_selected,
                            colors, &self.ansi, term_bg, term_fg, cursor_color,
                        );
                    }
                    continue;
                }

                let line = Line(logical);
                for col in 0..cols {
                    let column = Column(col);
                    let cell = &grid[line][column];
                    let x = col as f32 * cell_w;

                    // The cursor is suppressed whenever the view is scrolled
                    // back — it lives on the live screen, not in history.
                    let is_cursor =
                        offset == 0 && cursor_point.line == line && cursor_point.column == column;
                    let is_selected = cell_is_selected(sel, row, col);

                    draw_cell(
                        frame, x, y, cell_w, cell_h, self.font_size,
                        cell.c, cell.fg, cell.bg, cell.flags,
                        is_cursor, cursor_shape, is_selected,
                        colors, &self.ansi, term_bg, term_fg, cursor_color,
                    );
                }
            }
        });

        vec![geometry]
    }
}

/// Whether canvas cell (col, row) falls inside the current drag selection.
fn cell_is_selected(sel: &SelectionState, row: usize, col: usize) -> bool {
    sel.range().map(|((sc, sr), (ec, er))| {
        let in_row = row >= sr && row <= er;
        if !in_row { return false; }
        if sr == er { col >= sc && col <= ec }
        else if row == sr { col >= sc }
        else if row == er { col <= ec }
        else { true }
    }).unwrap_or(false)
}

/// Draw a 1px decoration stroke (underline/strikeout/undercurl segments).
fn stroke_line(frame: &mut Frame, x1: f32, y1: f32, x2: f32, y2: f32, color: IcedColor) {
    let path = Path::line(iced::Point::new(x1, y1), iced::Point::new(x2, y2));
    frame.stroke(&path, iced::widget::canvas::Stroke::default().with_width(1.0).with_color(color));
}

/// Draw one terminal cell (background/cursor/selection rect + glyph +
/// decorations). Shared by live grid rows and tmux-history rows so both
/// render identically — style resolution (fg/bg/flags → colors+font) lives
/// here, next to the draw calls, so the two paths can't drift apart.
#[allow(clippy::too_many_arguments)]
fn draw_cell(
    frame: &mut Frame,
    x: f32,
    y: f32,
    cell_w: f32,
    cell_h: f32,
    font_size: f32,
    c: char,
    fg_color: Color,
    bg_color: Color,
    flags: Flags,
    is_cursor: bool,
    cursor_shape: CursorShape,
    is_selected: bool,
    colors: &alacritty_terminal::term::color::Colors,
    ansi: &[IcedColor; 16],
    term_bg: IcedColor,
    term_fg: IcedColor,
    cursor_color: IcedColor,
) {
    // Resolve colors, then apply attribute transforms.
    let mut fg = ansi_to_iced(fg_color, colors, ansi, term_bg, term_fg);
    let mut bg = ansi_to_iced(bg_color, colors, ansi, term_bg, term_fg);
    if flags.contains(Flags::INVERSE) { std::mem::swap(&mut fg, &mut bg); }
    if flags.contains(Flags::DIM)     { fg.a *= 0.6; }
    if flags.contains(Flags::HIDDEN)  { fg = bg; }

    // Block cursor is a filled rect with an inverted glyph — the historical
    // behavior. Beam/Underline/HollowBlock draw the cell normally and
    // overlay a thin cursor mark instead of taking over the whole cell.
    let block_cursor = is_cursor && cursor_shape == CursorShape::Block;

    if is_selected {
        let sel_rect = Path::rectangle(iced::Point::new(x, y), Size::new(cell_w, cell_h));
        frame.fill(&sel_rect, IcedColor { r: 0.27, g: 0.52, b: 0.80, a: 0.5 });
    } else if block_cursor {
        let cursor_rect = Path::rectangle(iced::Point::new(x, y), Size::new(cell_w, cell_h));
        frame.fill(&cursor_rect, cursor_color);
    } else if bg != term_bg {
        let bg_rect = Path::rectangle(iced::Point::new(x, y), Size::new(cell_w, cell_h));
        frame.fill(&bg_rect, bg);
    }

    if is_cursor && !is_selected {
        match cursor_shape {
            CursorShape::Block => {} // handled above
            CursorShape::Beam => {
                frame.fill(&Path::rectangle(iced::Point::new(x, y), Size::new(2.0, cell_h)), cursor_color);
            }
            CursorShape::Underline => {
                frame.fill(
                    &Path::rectangle(iced::Point::new(x, y + cell_h - 2.0), Size::new(cell_w, 2.0)),
                    cursor_color,
                );
            }
            CursorShape::HollowBlock => {
                stroke_line(frame, x, y, x + cell_w, y, cursor_color);
                stroke_line(frame, x, y + cell_h, x + cell_w, y + cell_h, cursor_color);
                stroke_line(frame, x, y, x, y + cell_h, cursor_color);
                stroke_line(frame, x + cell_w, y, x + cell_w, y + cell_h, cursor_color);
            }
            CursorShape::Hidden => {}
        }
    }

    // Foreground text.
    if c != ' ' && c != '\0' {
        let glyph_fg = if block_cursor { term_bg } else { fg };

        let cp = c as u32;
        let font = if (0xE000..=0xF8FF).contains(&cp) {
            NERD_FONT
        } else {
            iced::Font {
                weight: if flags.intersects(Flags::BOLD) { iced::font::Weight::Bold }
                        else { iced::font::Weight::Normal },
                style:  if flags.contains(Flags::ITALIC) { iced::font::Style::Italic }
                        else { iced::font::Style::Normal },
                ..TERM_FONT
            }
        };
        frame.fill_text(iced::widget::canvas::Text {
            content: c.to_string(),
            position: iced::Point::new(x, y),
            color: glyph_fg,
            size: iced::Pixels(font_size),
            font,
            horizontal_alignment: iced::alignment::Horizontal::Left,
            vertical_alignment: iced::alignment::Vertical::Top,
            line_height: iced::widget::text::LineHeight::Relative(cell_h / font_size),
            shaping: iced::widget::text::Shaping::Advanced,
        });
    }

    // Decoration strokes — drawn in the resolved (post-transform) fg color.
    let baseline = y + cell_h - 2.0;
    if flags.contains(Flags::UNDERLINE) {
        stroke_line(frame, x, baseline, x + cell_w, baseline, fg);
    }
    if flags.contains(Flags::DOUBLE_UNDERLINE) {
        stroke_line(frame, x, baseline - 2.0, x + cell_w, baseline - 2.0, fg);
        stroke_line(frame, x, baseline,       x + cell_w, baseline,       fg);
    }
    if flags.contains(Flags::UNDERCURL) {
        // Two-segment zigzag per cell — reads as a curl at terminal sizes.
        stroke_line(frame, x, baseline, x + cell_w / 2.0, baseline - 2.0, fg);
        stroke_line(frame, x + cell_w / 2.0, baseline - 2.0, x + cell_w, baseline, fg);
    }
    if flags.contains(Flags::STRIKEOUT) {
        let mid = y + cell_h * 0.55;
        stroke_line(frame, x, mid, x + cell_w, mid, fg);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_advances_cursor() {
        let mut s = TerminalState::new(80, 24, None);
        s.process(b"hello");
        assert_eq!(s.term.grid().cursor.point.column.0, 5);
    }

    #[test]
    fn process_ansi_no_panic() {
        let mut s = TerminalState::new(80, 24, None);
        s.process(b"\x1b[31mred\x1b[0m");
    }

    #[test]
    fn emulator_query_responses_are_forwarded_to_reply_channel() {
        // The inner app (via tmux) queries the terminal — e.g. DSR 6 (cursor
        // position report). The emulator's answer must reach the reply
        // channel; dropping it hangs TUIs that wait for the response.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut s = TerminalState::new(80, 24, Some(tx));
        s.process(b"\x1b[6n"); // Device Status Report: cursor position
        let reply = rx.try_recv().expect("DSR must produce a reply");
        assert_eq!(reply, b"\x1b[1;1R".to_vec());
    }

    #[test]
    fn kitty_keyboard_query_is_answered() {
        // Claude Code probes kitty keyboard support with CSI ? u. A reply
        // is what makes Shift+Enter negotiation work end-to-end.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut s = TerminalState::new(80, 24, Some(tx));
        s.process(b"\x1b[?u");
        let reply = rx.try_recv().expect("kitty query must produce a reply");
        assert!(reply.starts_with(b"\x1b[?"), "unexpected kitty reply: {reply:?}");
    }

    #[test]
    fn ansi_to_iced_uses_theme_palette_for_named_colors() {
        use alacritty_terminal::vte::ansi::{Color, NamedColor};
        let colors = alacritty_terminal::term::color::Colors::default();
        let mut ansi = [IcedColor::BLACK; 16];
        ansi[1] = IcedColor::from_rgb8(0x12, 0x34, 0x56); // themed "red"
        let out = ansi_to_iced(
            Color::Named(NamedColor::Red), &colors, &ansi,
            IcedColor::BLACK, IcedColor::WHITE,
        );
        assert_eq!(out, IcedColor::from_rgb8(0x12, 0x34, 0x56));
    }

    #[test]
    fn every_theme_defines_a_full_palette() {
        for scheme in [crate::theme::light(), crate::theme::dark(), crate::theme::warm_dark()] {
            // 16 distinct-ish entries; at minimum not all default black.
            assert!(scheme.ansi.iter().any(|c| *c != iced::Color::BLACK));
            assert_eq!(scheme.ansi.len(), 16);
        }
    }

    #[test]
    fn cell_size_comes_from_font_metrics() {
        let (w, h) = cell_size(13.0);
        // JetBrains Mono: advance 600/1000 upem → width exactly 0.6em.
        assert!((w - 13.0 * 0.6).abs() < 0.01, "width {w}");
        // Height = (ascender - descender + line_gap)/upem — sane range, and
        // NOT the old hardcoded 1.4 approximation.
        assert!(h > 13.0 * 1.1 && h < 13.0 * 1.5, "height {h}");
        assert!((h - 13.0 * 1.4).abs() > 0.01, "height must be measured, not the 1.4 guess");
    }
}
