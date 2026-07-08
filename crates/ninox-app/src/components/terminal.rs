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
    ///
    /// Drops the whole cache rather than just zeroing `offset`. tmux
    /// `capture-pane` line indices are relative to the *current* pane top,
    /// which drifts as live output scrolls into history; a cached anchor
    /// fetched at one live-output position can point at different lines
    /// once more output has streamed. Discarding the cache here (the
    /// natural point where the user leaves the stale scrollback view)
    /// keeps re-entering history from replaying duplicated/misordered
    /// chunks. Known residual trade-off: indices can still drift *within*
    /// one continuous scrolled-back session while output keeps streaming in
    /// the background — accepted for now; see the module doc on
    /// `Scrollback`.
    pub fn scroll_to_bottom(&mut self) {
        self.scrollback = Default::default();
        self.cache.clear();
    }

    /// Whether the display is currently scrolled up into history.
    pub fn is_scrolled_back(&self) -> bool {
        self.scrollback.offset > 0
    }

    /// Resize the terminal grid to match a new canvas size.
    /// Current live-grid size (cols, rows) — the emulator's actual
    /// dimensions, which the session-detail title bar reports (the
    /// app-level `terminal_cols/rows` are only the background/Split
    /// sizing, not necessarily what this session was resized to).
    pub fn grid_size(&self) -> (u16, u16) {
        let grid = self.term.grid();
        (grid.columns() as u16, grid.screen_lines() as u16)
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        use alacritty_terminal::term::test::TermSize;
        let size = TermSize::new(cols as usize, rows as usize);
        self.term.resize(size);
        // Cached history lines were parsed and wrapped at the old column
        // width; keeping them around after a resize would render
        // stale-width lines (see scroll_to_bottom for the same
        // capture-pane-index-drift trade-off this also avoids).
        self.scrollback = Default::default();
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
    /// Whether the cell under the cursor is currently part of a clickable
    /// link — drives the pointer cursor via `mouse_interaction`.
    hovering_link: bool,
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

impl<'a> TerminalWidget<'a> {
    /// The link-detection view of viewport row `row`: each cell's rendered
    /// character plus its OSC 8 hyperlink URI, if any, from either the live
    /// grid or cached scrollback history. Empty if `row` is out of bounds or
    /// the scrollback line isn't cached.
    ///
    /// The live-grid branch has to clone each cell's `Hyperlink` out of the
    /// grid (there's no way to borrow one directly), so callers must pass in
    /// `hyperlink_storage` to own those clones for at least as long as the
    /// returned `LinkCell`s (which borrow their URI strings from it) are used.
    fn row_link_cells<'b>(
        &self,
        row: usize,
        hyperlink_storage: &'b mut Vec<Option<alacritty_terminal::term::cell::Hyperlink>>,
    ) -> Vec<crate::components::links::LinkCell<'b>>
    where
        'a: 'b,
    {
        use crate::components::links::LinkCell;

        let grid = self.state.term.grid();
        let cols = grid.columns();
        let rows = grid.screen_lines();
        if row >= rows {
            return Vec::new();
        }
        let offset = self.state.scrollback.offset as i32;
        let logical = row as i32 - offset;

        if logical < 0 {
            let Some(cells) = self.state.scrollback.line_above((-logical - 1) as usize) else {
                return Vec::new();
            };
            cells.iter().map(|c| LinkCell { c: c.c, hyperlink: c.hyperlink.as_deref() }).collect()
        } else {
            use alacritty_terminal::index::{Column, Line};
            let line = Line(logical);
            *hyperlink_storage = (0..cols).map(|c| grid[line][Column(c)].hyperlink()).collect();
            (0..cols)
                .map(|c| LinkCell {
                    c: grid[line][Column(c)].c,
                    hyperlink: hyperlink_storage[c].as_ref().map(|h| h.uri()),
                })
                .collect()
        }
    }

    /// Every clickable link span in viewport row `row`, from OSC 8
    /// hyperlinks (live grid or cached history) or a bare-URL fallback scan.
    fn row_link_spans(&self, row: usize) -> Vec<crate::components::links::LinkSpan> {
        let mut hyperlink_storage = Vec::new();
        crate::components::links::find_links(&self.row_link_cells(row, &mut hyperlink_storage))
    }

    /// The URL under viewport cell (col, row), if any.
    fn link_at(&self, col: usize, row: usize) -> Option<String> {
        let mut hyperlink_storage = Vec::new();
        crate::components::links::link_at(&self.row_link_cells(row, &mut hyperlink_storage), col)
    }

    /// A `CopyToClipboard` message for `sel`'s current selection, or `None`
    /// if there's no selection or it's blank. Shared by the drag-release
    /// auto-copy and the explicit Cmd+C / Ctrl+Shift+C shortcut so the two
    /// paths can't drift apart.
    fn copy_message(&self, sel: &SelectionState) -> Option<Message> {
        sel.range()
            .map(|((sc, sr), (ec, er))| extract_selection(self.state, sc, sr, ec, er))
            .filter(|s| !s.trim().is_empty())
            .map(Message::CopyToClipboard)
    }
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

            Event::Mouse(MouseEvent::CursorMoved { .. }) => {
                // Hover-link detection runs regardless of drag state, so the pointer
                // cursor still reflects the cell under the mouse mid-drag.
                let hovering = cursor
                    .position_in(bounds)
                    .map(|pos| SelectionState::pixel_to_cell(pos.x, pos.y, cell_w, cell_h, cols, rows))
                    .is_some_and(|(col, row)| self.link_at(col, row).is_some());
                state.hovering_link = hovering;

                if state.dragging {
                    if let Some(pos) = cursor.position_in(bounds) {
                        let cell = SelectionState::pixel_to_cell(pos.x, pos.y, cell_w, cell_h, cols, rows);
                        if state.anchor != Some(cell) {
                            state.moved = true;
                        }
                        state.end = Some(cell);
                        self.state.cache.clear();
                        return (iced::widget::canvas::event::Status::Captured, None);
                    }

                    // Dragging past the top or bottom edge — the natural
                    // gesture to extend a selection into scrollback that
                    // isn't currently on screen. `position_in` returns None
                    // once the pointer leaves `bounds`, so without this the
                    // selection simply stops growing and there's no way to
                    // select anything beyond the visible screen. Extend to
                    // the edge row and nudge the view a few lines toward
                    // the pointer; further movement while still past the
                    // edge keeps extending/scrolling one step per event.
                    if let Some(pos) = cursor.position() {
                        let col = ((pos.x - bounds.x) / cell_w) as usize;
                        let col = col.min(cols.saturating_sub(1));
                        if pos.y < bounds.y {
                            state.end = Some((col, 0));
                            state.moved = true;
                            self.state.cache.clear();
                            return (
                                iced::widget::canvas::event::Status::Captured,
                                Some(Message::ScrollTerminal { session_id: self.session_id.clone(), delta: 3 }),
                            );
                        } else if pos.y > bounds.y + bounds.height {
                            state.end = Some((col, rows.saturating_sub(1)));
                            state.moved = true;
                            self.state.cache.clear();
                            return (
                                iced::widget::canvas::event::Status::Captured,
                                Some(Message::ScrollTerminal { session_id: self.session_id.clone(), delta: -3 }),
                            );
                        }
                    }
                    return (iced::widget::canvas::event::Status::Captured, None);
                }
                return (iced::widget::canvas::event::Status::Ignored, None);
            }

            Event::Mouse(MouseEvent::CursorLeft) => {
                state.hovering_link = false;
                return (iced::widget::canvas::event::Status::Ignored, None);
            }

            Event::Mouse(MouseEvent::ButtonReleased(Button::Left)) if state.dragging => {
                state.dragging = false;
                if !state.moved {
                    // Single click — a link takes priority over session-ID navigation.
                    if let (Some((col, row)), Some(pos)) = (state.anchor, cursor.position_in(bounds)) {
                        let _ = pos; // bounds-checked via anchor
                        if let Some(url) = self.link_at(col, row) {
                            state.anchor = None;
                            state.end    = None;
                            return (iced::widget::canvas::event::Status::Captured,
                                    Some(Message::OpenUrl(url)));
                        }
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
                return (iced::widget::canvas::event::Status::Captured, self.copy_message(state));
            }

            // Cmd+C (macOS) or Ctrl+Shift+C (the Linux/Windows terminal
            // convention, since plain Ctrl+C is reserved for SIGINT) —
            // intercepted here rather than in the app-level RawKey handler
            // because the current selection lives in this widget's
            // canvas-local `SelectionState`, not in app state. Without
            // this, Cmd+C fell through to `encode_key`, which has no
            // functional-key mapping for a bare Character key under only
            // the logo modifier, so it took the plain-text fallback and
            // typed a literal "c" into the terminal instead of copying —
            // and did nothing useful even when there was a selection.
            // Plain Ctrl+C is deliberately left alone: it's the standard
            // SIGINT byte (0x03). Mirrors the Cmd+V / Ctrl+Shift+V split
            // already used for paste below.
            Event::Keyboard(KeyEvent::KeyPressed { key, modifiers, .. })
                if ((modifiers.logo() && !modifiers.control())
                    || (modifiers.control() && modifiers.shift()))
                    && matches!(key, iced::keyboard::Key::Character(c) if c.as_str().eq_ignore_ascii_case("c")) =>
            {
                return (iced::widget::canvas::event::Status::Captured, self.copy_message(state));
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
                    let link_spans = self.row_link_spans(row);
                    for (col, cell) in cells.iter().enumerate().take(cols) {
                        let x = col as f32 * cell_w;
                        let is_selected = cell_is_selected(sel, row, col);
                        let is_link = link_spans.iter().any(|s| col >= s.start_col && col <= s.end_col);
                        draw_cell(
                            frame, x, y, cell_w, cell_h, self.font_size,
                            cell.c, cell.fg, cell.bg, cell.flags,
                            false, // cursor never draws in history
                            cursor_shape, is_selected, is_link,
                            colors, &self.ansi, term_bg, term_fg, cursor_color,
                        );
                    }
                    continue;
                }

                let line = Line(logical);
                let link_spans = self.row_link_spans(row);
                for col in 0..cols {
                    let column = Column(col);
                    let cell = &grid[line][column];
                    let x = col as f32 * cell_w;

                    // The cursor is suppressed whenever the view is scrolled
                    // back — it lives on the live screen, not in history.
                    let is_cursor =
                        offset == 0 && cursor_point.line == line && cursor_point.column == column;
                    let is_selected = cell_is_selected(sel, row, col);
                    let is_link = link_spans.iter().any(|s| col >= s.start_col && col <= s.end_col);

                    draw_cell(
                        frame, x, y, cell_w, cell_h, self.font_size,
                        cell.c, cell.fg, cell.bg, cell.flags,
                        is_cursor, cursor_shape, is_selected, is_link,
                        colors, &self.ansi, term_bg, term_fg, cursor_color,
                    );
                }
            }
        });

        vec![geometry]
    }

    fn mouse_interaction(
        &self,
        state: &Self::State,
        _bounds: Rectangle,
        _cursor: iced::mouse::Cursor,
    ) -> iced::mouse::Interaction {
        if state.hovering_link {
            iced::mouse::Interaction::Pointer
        } else {
            iced::mouse::Interaction::default()
        }
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
    is_link: bool,
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
        frame.fill(&sel_rect, IcedColor { r: 0.941, g: 0.753, b: 0.412, a: 0.35 }); // #f0c069 amber, field notes
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
    if flags.contains(Flags::UNDERLINE) || is_link {
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

/// Extract the text under a viewport selection, mirroring the draw path's
/// row indexing exactly: when the view is scrolled back
/// (`state.scrollback.offset > 0`), rows above the live screen must read
/// from the cached tmux history rather than the live grid, or the
/// highlighted text and the copied clipboard text diverge.
pub fn extract_selection(
    state: &TerminalState,
    start_col: usize, start_row: usize,
    end_col: usize,   end_row: usize,
) -> String {
    use alacritty_terminal::index::{Column, Line};

    let grid = state.term.grid();
    let cols = grid.columns();
    let rows = grid.screen_lines();
    let offset = state.scrollback.offset as i32;
    let mut out = String::new();

    for row in start_row..=end_row.min(rows.saturating_sub(1)) {
        let col_start = if row == start_row { start_col } else { 0 };
        let col_end   = if row == end_row   { end_col   } else { cols.saturating_sub(1) };
        let col_end   = col_end.min(cols.saturating_sub(1));

        let logical = row as i32 - offset;
        let mut line_text = String::new();
        if logical < 0 {
            // History row — same index math as the draw path.
            if let Some(cells) = state.scrollback.line_above((-logical - 1) as usize) {
                for cell in cells.iter().skip(col_start).take(col_end + 1 - col_start) {
                    line_text.push(if cell.c == '\0' { ' ' } else { cell.c });
                }
            }
        } else {
            let line = Line(logical);
            for col in col_start..=col_end {
                let cell = &grid[line][Column(col)];
                line_text.push(if cell.c == '\0' { ' ' } else { cell.c });
            }
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
        for scheme in [crate::theme::light(), crate::theme::dark()] {
            // 16 distinct-ish entries; at minimum not all default black.
            assert!(scheme.ansi.iter().any(|c| *c != iced::Color::BLACK));
            assert_eq!(scheme.ansi.len(), 16);
        }
    }

    #[test]
    fn extract_selection_reads_history_lines_when_scrolled_back() {
        use crate::components::scrollback::StyledCell;
        use alacritty_terminal::vte::ansi::NamedColor;

        let mut s = TerminalState::new(20, 5, None);
        s.process(b"live line one\r\nlive line two\r\n");

        let mk_line = |text: &str| -> Vec<StyledCell> {
            text.chars().map(|c| StyledCell {
                c,
                fg: Color::Named(NamedColor::Foreground),
                bg: Color::Named(NamedColor::Background),
                flags: Flags::empty(),
                hyperlink: None,
            }).collect()
        };
        // Oldest first — "history two" sits directly above the live screen.
        s.scrollback.absorb(vec![mk_line("history one"), mk_line("history two")], -2, true);
        s.scrollback.offset = 2;

        // Top two viewport rows (0, 1) are both scrolled into history at
        // this offset — row 0 → history one, row 1 → history two.
        let text = extract_selection(&s, 0, 0, 19, 1);
        assert_eq!(text, "history one\nhistory two");
    }

    #[test]
    fn scroll_to_bottom_resets_the_whole_scrollback_cache() {
        let mut s = TerminalState::new(80, 24, None);
        s.scrollback.absorb(vec![vec![]; 10], -10, true);
        s.scrollback.offset = 5;
        s.scrollback.fetch_pending = true;
        assert!(!s.scrollback.lines.is_empty());

        s.scroll_to_bottom();

        assert!(s.scrollback.lines.is_empty(), "cached history must be dropped");
        assert_eq!(s.scrollback.offset, 0);
        assert_eq!(s.scrollback.fetched_to, 0);
        assert!(!s.scrollback.top_reached);
        assert!(!s.scrollback.fetch_pending);
    }

    #[test]
    fn resize_resets_the_whole_scrollback_cache() {
        let mut s = TerminalState::new(80, 24, None);
        s.scrollback.absorb(vec![vec![]; 10], -10, true);
        s.scrollback.offset = 5;
        s.scrollback.fetch_pending = true;

        s.resize(100, 30);

        assert!(s.scrollback.lines.is_empty(), "cached history parsed at the old width must be dropped");
        assert_eq!(s.scrollback.offset, 0);
        assert_eq!(s.scrollback.fetched_to, 0);
        assert!(!s.scrollback.top_reached);
        assert!(!s.scrollback.fetch_pending);
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

    fn test_widget<'a>(state: &'a TerminalState) -> TerminalWidget<'a> {
        TerminalWidget {
            state,
            session_id:   String::new(),
            font_size:    FONT_SIZE,
            terminal_bg:  IcedColor::BLACK,
            terminal_fg:  IcedColor::WHITE,
            cursor_color: IcedColor::WHITE,
            ansi:         [IcedColor::BLACK; 16],
            session_ids:  vec![],
        }
    }

    #[test]
    fn row_link_spans_detects_osc8_hyperlink_on_live_grid() {
        let mut s = TerminalState::new(80, 5, None);
        s.process(b"\x1b]8;;http://example.com\x1b\\click me\x1b]8;;\x1b\\");
        let widget = test_widget(&s);
        let spans = widget.row_link_spans(0);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].url, "http://example.com");
        assert_eq!(spans[0].start_col, 0);
        assert_eq!(spans[0].end_col, 7);
    }

    #[test]
    fn row_link_spans_detects_bare_url_on_live_grid() {
        let mut s = TerminalState::new(80, 5, None);
        s.process(b"see http://example.com/path for docs");
        let widget = test_widget(&s);
        let spans = widget.row_link_spans(0);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].url, "http://example.com/path");
    }

    #[test]
    fn link_at_returns_none_outside_any_span() {
        let mut s = TerminalState::new(80, 5, None);
        s.process(b"see http://example.com/path for docs");
        let widget = test_widget(&s);
        assert_eq!(widget.link_at(0, 0), None); // inside "see "
        assert_eq!(widget.link_at(5, 0), Some("http://example.com/path".to_string()));
    }

    #[test]
    fn hovering_a_link_sets_pointer_cursor() {
        use iced::widget::canvas::Program;

        let mut s = TerminalState::new(80, 5, None);
        s.process(b"see http://example.com/path for docs");
        let widget = test_widget(&s);
        let (cell_w, cell_h) = cell_size(FONT_SIZE);
        let bounds = Rectangle::new(iced::Point::ORIGIN, Size::new(80.0 * cell_w, 5.0 * cell_h));
        let mut state = SelectionState::default();

        // Column 5 sits inside "http://example.com/path".
        let hover_pos = iced::Point::new(5.0 * cell_w + 1.0, 0.5 * cell_h);
        widget.update(
            &mut state,
            iced::widget::canvas::Event::Mouse(iced::mouse::Event::CursorMoved { position: hover_pos }),
            bounds,
            iced::mouse::Cursor::Available(hover_pos),
        );
        assert_eq!(
            widget.mouse_interaction(&state, bounds, iced::mouse::Cursor::Available(hover_pos)),
            iced::mouse::Interaction::Pointer
        );

        // Column 0 ("s" of "see") is not a link.
        let no_link_pos = iced::Point::new(0.5 * cell_w, 0.5 * cell_h);
        widget.update(
            &mut state,
            iced::widget::canvas::Event::Mouse(iced::mouse::Event::CursorMoved { position: no_link_pos }),
            bounds,
            iced::mouse::Cursor::Available(no_link_pos),
        );
        assert_eq!(
            widget.mouse_interaction(&state, bounds, iced::mouse::Cursor::Available(no_link_pos)),
            iced::mouse::Interaction::default()
        );
    }

    #[test]
    fn clicking_a_link_emits_open_url() {
        use iced::mouse::Button;
        use iced::widget::canvas::Program;

        let mut s = TerminalState::new(80, 5, None);
        s.process(b"see http://example.com/path for docs");
        let widget = test_widget(&s);
        let (cell_w, cell_h) = cell_size(FONT_SIZE);
        let bounds = Rectangle::new(iced::Point::ORIGIN, Size::new(80.0 * cell_w, 5.0 * cell_h));
        let mut state = SelectionState::default();
        let pos = iced::Point::new(5.0 * cell_w + 1.0, 0.5 * cell_h);
        let cursor = iced::mouse::Cursor::Available(pos);

        widget.update(
            &mut state,
            iced::widget::canvas::Event::Mouse(iced::mouse::Event::ButtonPressed(Button::Left)),
            bounds,
            cursor,
        );
        let (_, message) = widget.update(
            &mut state,
            iced::widget::canvas::Event::Mouse(iced::mouse::Event::ButtonReleased(Button::Left)),
            bounds,
            cursor,
        );
        match message {
            Some(Message::OpenUrl(url)) => assert_eq!(url, "http://example.com/path"),
            other => panic!("expected OpenUrl, got {other:?}"),
        }
    }

    /// Builds a `KeyPressed` canvas event for `key` under `modifiers` —
    /// the fields beyond those two don't affect the copy-shortcut match.
    fn key_pressed(key: iced::keyboard::Key, modifiers: iced::keyboard::Modifiers) -> iced::widget::canvas::Event {
        iced::widget::canvas::Event::Keyboard(iced::keyboard::Event::KeyPressed {
            key: key.clone(),
            modified_key: key,
            physical_key: iced::keyboard::key::Physical::Unidentified(iced::keyboard::key::NativeCode::Unidentified),
            location: iced::keyboard::Location::Standard,
            modifiers,
            text: None,
        })
    }

    #[test]
    fn cmd_c_copies_the_active_selection_instead_of_typing_c() {
        use iced::widget::canvas::Program;

        let mut s = TerminalState::new(80, 5, None);
        s.process(b"hello world");
        let widget = test_widget(&s);
        let bounds = Rectangle::new(iced::Point::ORIGIN, Size::new(800.0, 100.0));
        let mut state = SelectionState {
            anchor: Some((0, 0)),
            end: Some((4, 0)),
            moved: true,
            ..Default::default()
        };

        let (status, message) = widget.update(
            &mut state,
            key_pressed(iced::keyboard::Key::Character("c".into()), iced::keyboard::Modifiers::LOGO),
            bounds,
            iced::mouse::Cursor::Unavailable,
        );
        assert_eq!(status, iced::widget::canvas::event::Status::Captured);
        match message {
            Some(Message::CopyToClipboard(text)) => assert_eq!(text, "hello"),
            other => panic!("expected CopyToClipboard, got {other:?}"),
        }
    }

    #[test]
    fn cmd_c_with_no_selection_is_swallowed_not_typed() {
        use iced::widget::canvas::Program;

        let s = TerminalState::new(80, 5, None);
        let widget = test_widget(&s);
        let bounds = Rectangle::new(iced::Point::ORIGIN, Size::new(800.0, 100.0));
        let mut state = SelectionState::default();

        let (status, message) = widget.update(
            &mut state,
            key_pressed(iced::keyboard::Key::Character("c".into()), iced::keyboard::Modifiers::LOGO),
            bounds,
            iced::mouse::Cursor::Unavailable,
        );
        // Captured (never falls through to RawKey / the PTY) but nothing to copy.
        assert_eq!(status, iced::widget::canvas::event::Status::Captured);
        assert!(message.is_none());
    }

    #[test]
    fn ctrl_c_is_not_treated_as_copy() {
        use iced::widget::canvas::Program;

        let mut s = TerminalState::new(80, 5, None);
        s.process(b"hello world");
        let widget = test_widget(&s);
        let bounds = Rectangle::new(iced::Point::ORIGIN, Size::new(800.0, 100.0));
        let mut state = SelectionState {
            anchor: Some((0, 0)),
            end: Some((4, 0)),
            moved: true,
            ..Default::default()
        };

        // Ctrl+C must still reach RawKey (SIGINT), not be swallowed as copy.
        let (_, message) = widget.update(
            &mut state,
            key_pressed(iced::keyboard::Key::Character("c".into()), iced::keyboard::Modifiers::CTRL),
            bounds,
            iced::mouse::Cursor::Unavailable,
        );
        assert!(matches!(message, Some(Message::RawKey { .. })));
    }

    #[test]
    fn ctrl_shift_c_copies_the_active_selection() {
        use iced::widget::canvas::Program;

        let mut s = TerminalState::new(80, 5, None);
        s.process(b"hello world");
        let widget = test_widget(&s);
        let bounds = Rectangle::new(iced::Point::ORIGIN, Size::new(800.0, 100.0));
        let mut state = SelectionState {
            anchor: Some((0, 0)),
            end: Some((4, 0)),
            moved: true,
            ..Default::default()
        };

        // The Linux/Windows terminal convention (plain Ctrl+C is SIGINT).
        let (status, message) = widget.update(
            &mut state,
            key_pressed(
                iced::keyboard::Key::Character("c".into()),
                iced::keyboard::Modifiers::CTRL | iced::keyboard::Modifiers::SHIFT,
            ),
            bounds,
            iced::mouse::Cursor::Unavailable,
        );
        assert_eq!(status, iced::widget::canvas::event::Status::Captured);
        match message {
            Some(Message::CopyToClipboard(text)) => assert_eq!(text, "hello"),
            other => panic!("expected CopyToClipboard, got {other:?}"),
        }
    }

    #[test]
    fn dragging_above_the_top_edge_extends_selection_and_scrolls_into_history() {
        use iced::mouse::Button;
        use iced::widget::canvas::Program;

        let mut s = TerminalState::new(80, 24, None);
        s.process(b"line one\r\nline two\r\n");
        let widget = test_widget(&s);
        let (cell_w, cell_h) = cell_size(FONT_SIZE);
        let bounds = Rectangle::new(iced::Point::new(0.0, 100.0), Size::new(80.0 * cell_w, 24.0 * cell_h));
        let mut state = SelectionState::default();

        let start = iced::Point::new(cell_w * 2.0, bounds.y + cell_h * 2.0);
        widget.update(
            &mut state,
            iced::widget::canvas::Event::Mouse(iced::mouse::Event::ButtonPressed(Button::Left)),
            bounds,
            iced::mouse::Cursor::Available(start),
        );

        // Drag above the canvas's top edge — outside `bounds`, so
        // `position_in` returns None for this position.
        let above = iced::Point::new(cell_w * 3.0, bounds.y - 20.0);
        let (status, message) = widget.update(
            &mut state,
            iced::widget::canvas::Event::Mouse(iced::mouse::Event::CursorMoved { position: above }),
            bounds,
            iced::mouse::Cursor::Available(above),
        );

        assert_eq!(status, iced::widget::canvas::event::Status::Captured);
        assert_eq!(state.end, Some((3, 0)), "selection should extend to the top row");
        assert!(state.moved);
        match message {
            Some(Message::ScrollTerminal { delta, .. }) => assert!(delta > 0, "should scroll up into history"),
            other => panic!("expected ScrollTerminal, got {other:?}"),
        }
    }

    #[test]
    fn dragging_below_the_bottom_edge_extends_selection_and_scrolls_toward_live() {
        use iced::mouse::Button;
        use iced::widget::canvas::Program;

        let mut s = TerminalState::new(80, 24, None);
        s.process(b"line one\r\nline two\r\n");
        let widget = test_widget(&s);
        let (cell_w, cell_h) = cell_size(FONT_SIZE);
        let bounds = Rectangle::new(iced::Point::new(0.0, 100.0), Size::new(80.0 * cell_w, 24.0 * cell_h));
        let mut state = SelectionState::default();

        let start = iced::Point::new(cell_w * 2.0, bounds.y + cell_h * 2.0);
        widget.update(
            &mut state,
            iced::widget::canvas::Event::Mouse(iced::mouse::Event::ButtonPressed(Button::Left)),
            bounds,
            iced::mouse::Cursor::Available(start),
        );

        // Drag below the canvas's bottom edge — outside `bounds`.
        let below = iced::Point::new(cell_w * 3.0, bounds.y + bounds.height + 20.0);
        let (status, message) = widget.update(
            &mut state,
            iced::widget::canvas::Event::Mouse(iced::mouse::Event::CursorMoved { position: below }),
            bounds,
            iced::mouse::Cursor::Available(below),
        );

        assert_eq!(status, iced::widget::canvas::event::Status::Captured);
        assert_eq!(state.end, Some((3, 23)), "selection should extend to the bottom row");
        assert!(state.moved);
        match message {
            Some(Message::ScrollTerminal { delta, .. }) => assert!(delta < 0, "should scroll down toward live"),
            other => panic!("expected ScrollTerminal, got {other:?}"),
        }
    }
}
