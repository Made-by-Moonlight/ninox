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

/// Cap on captured lines kept in `TerminalState::extra_history`, bounding
/// memory for long-running sessions.
const MAX_EXTRA_HISTORY: usize = 5000;

pub struct TerminalState {
    pub term: Term<EventProxy>,
    pub cache: Cache,
    parser: Processor,
    /// Lines evicted from the live viewport by a full-screen redraw that
    /// never goes through the terminal's native scrollback mechanism. Ink-
    /// based TUIs (Claude Code, and most modern CLI agents) redraw their
    /// whole screen using absolute/relative cursor addressing — `ESC[H`,
    /// `ESC[nB`, `ESC[nG`, per-line `ESC[K` — rather than real linefeeds or
    /// `ESC[2J`. Nothing about that sequence trips alacritty's own
    /// scroll-into-history logic (which only fires on linefeed-driven
    /// overflow or an explicit erase-display op), so content clipped off
    /// the top during such a redraw would otherwise vanish with no trace.
    /// `process` detects this and preserves the outgoing lines here, oldest
    /// first, so scrolling can still reach them.
    extra_history: std::collections::VecDeque<String>,
    /// How many lines deep into `extra_history` the view is scrolled, on
    /// top of whatever native grid history scrolling has already covered.
    extra_offset: usize,
}

impl TerminalState {
    /// Feed raw bytes from the PTY into the VTE parser → terminal state.
    ///
    /// A single PTY read can contain *many* full-screen redraws back to
    /// back — dozens in one ~4 KiB chunk during active streaming — so we
    /// can't just diff the viewport once around the whole call; that would
    /// only ever see the net effect of the first redraw's "before" against
    /// the last redraw's "after", silently losing everything in between.
    /// Instead, split on every `ESC[H` (cursor home, which is how Ink-style
    /// TUIs begin a full-screen redraw) and diff at each one individually.
    pub fn process(&mut self, bytes: &[u8]) {
        const HOME: &[u8] = b"\x1b[H";
        const ERASE_DISPLAY: &[u8] = b"\x1b[2J";
        const ERASE_SAVED: &[u8] = b"\x1b[3J";

        let splits: Vec<usize> = if bytes.len() >= HOME.len() {
            bytes.windows(HOME.len())
                .enumerate()
                .filter_map(|(i, w)| (w == HOME).then_some(i))
                .collect()
        } else {
            Vec::new()
        };

        if splits.is_empty() {
            self.parser.advance(&mut self.term, bytes);
            self.cache.clear();
            return;
        }

        if splits[0] > 0 {
            self.parser.advance(&mut self.term, &bytes[..splits[0]]);
        }

        for (i, &start) in splits.iter().enumerate() {
            let end = splits.get(i + 1).copied().unwrap_or(bytes.len());
            let segment = &bytes[start..end];
            let before = viewport_rows(&self.term);
            self.parser.advance(&mut self.term, segment);
            let after = viewport_rows(&self.term);

            // ESC[2J / ESC[3J (erase-display, erase-saved-lines) are what
            // real `clear` implementations send — Ink itself never emits
            // either (its redraws are pure cursor addressing, see
            // `detect_shift`). Seeing one means the user explicitly asked
            // for a clean slate: wipe whatever we've captured outside
            // alacritty's own scrollback, and don't run shift-detection on
            // this transition — it's a deliberate erase, not an eviction.
            if segment.windows(4).any(|w| w == ERASE_DISPLAY || w == ERASE_SAVED) {
                self.extra_history.clear();
                self.extra_offset = 0;
            } else {
                self.capture_evicted_content(&before, &after);
            }
        }

        self.cache.clear();
    }

    /// Detect a genuine eviction via `detect_shift` and preserve whichever
    /// lines it identifies as having scrolled off, in order.
    fn capture_evicted_content(&mut self, before: &[String], after: &[String]) {
        let Some(k) = detect_shift(before, after) else { return };
        for line in &before[..k] {
            let trimmed = line.trim_end();
            // Skip lines that are still visible somewhere in the new frame
            // (persistent chrome like the input box) — only preserve what's
            // actually being evicted.
            if !trimmed.is_empty() && !after.iter().any(|a| a.trim_end() == trimmed) {
                self.extra_history.push_back(trimmed.to_string());
            }
        }
        while self.extra_history.len() > MAX_EXTRA_HISTORY {
            self.extra_history.pop_front();
        }
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
        let history_size   = grid.history_size() as i32;
        // Combined depth into history: native grid scrolling, then whatever
        // extra_history scrolling covers beyond that (see `TerminalState::scroll`).
        let total_offset = display_offset + self.state.extra_offset as i32;

        let term_bg = self.terminal_bg;
        let term_fg = self.terminal_fg;
        let cursor_color = self.cursor_color;

        let geometry = self.state.cache.draw(renderer, bounds.size(), |frame: &mut Frame| {
            // Background fill.
            let bg_all = Path::rectangle(iced::Point::ORIGIN, bounds.size());
            frame.fill(&bg_all, term_bg);

            // Render each cell. When scrolled into history, shift line indices by
            // total_offset so Line(0 - offset) accesses the history buffer; once
            // that runs out, fall back to `extra_history` (see its doc comment).
            for row in 0..rows {
                use alacritty_terminal::index::{Column, Line};

                let logical_line = row as i32 - total_offset;
                let y = row as f32 * cell_h;

                if logical_line < -history_size {
                    // Beyond native history — render a captured plain-text
                    // line from `extra_history`, if one exists this far back.
                    let lines_past = (-logical_line - history_size - 1) as usize;
                    let Some(text) = self.state.extra_line(lines_past) else { continue };
                    let chars: Vec<char> = text.chars().collect();

                    for col in 0..cols {
                        let ch = chars.get(col).copied().unwrap_or(' ');
                        if ch == ' ' || ch == '\0' { continue; }
                        let x = col as f32 * cell_w;
                        let cp = ch as u32;
                        let font = if (0xE000..=0xF8FF).contains(&cp) { NERD_FONT } else { iced::Font::MONOSPACE };
                        frame.fill_text(iced::widget::canvas::Text {
                            content: ch.to_string(),
                            position: iced::Point::new(x, y),
                            color: term_fg,
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
                    continue;
                }

                let line = Line(logical_line);
                for col in 0..cols {
                    let column = Column(col);
                    let cell = &grid[line][column];

                    let x = col as f32 * cell_w;

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

/// Detect whether `after` looks like `before` shifted up by some number of
/// lines — i.e. `before`'s tail reappears as `after`'s head — and if so,
/// return how many lines were evicted off the top.
///
/// Ink's redraws produce many overlapping, partially-settled frames for the
/// same logical scroll event (a line being typed out character by character
/// still touches rows above it via cursor addressing). Capturing "any line
/// that doesn't appear anywhere in the new frame" — the previous approach —
/// scoops up whichever transient snapshot a diff happened to land on, in
/// whatever order events fired, producing scrollback that reads as a
/// shuffled mess. Requiring a tail/head alignment instead only commits to
/// an eviction once the shift is unambiguous, and yields the evicted lines
/// in their original, correct order.
///
/// The alignment doesn't need to be byte-perfect: a status/spinner line
/// (elapsed time, token count) can tick over in the very same redraw that
/// evicts content, landing inside the "tail" region and mismatching on its
/// own. Allow a handful of such mismatches rather than requiring an exact
/// match, or almost every real eviction would fail to be recognized at all.
///
/// Prefers the *largest* shift with the fewest mismatches, so one call
/// captures as much genuinely-evicted content as possible; requires at
/// least one non-blank line in the shifted-off region for confidence.
///
/// Also requires a minimum amount of non-blank content in the *matched*
/// region. Without this, a still-growing response (content filling
/// previously-blank rows, nothing evicted yet) trivially "matches" at
/// almost any shift — mostly-blank rows match mostly-blank rows by
/// coincidence — which fires a false eviction and later re-captures the
/// same lines for real once they're genuinely evicted, producing
/// duplicates.
fn detect_shift(before: &[String], after: &[String]) -> Option<usize> {
    const MAX_MISMATCHES: usize = 8;
    const MIN_NON_BLANK_MATCH_EVIDENCE: usize = 3;

    let rows = before.len();
    (1..rows)
        .rev()
        .filter(|&k| before[..k].iter().any(|l| !l.trim().is_empty()))
        .filter_map(|k| {
            let tail = &before[k..];
            let head = &after[..rows - k];
            let mismatches = tail.iter().zip(head.iter()).filter(|(b, a)| b != a).count();
            let non_blank_evidence = tail.iter().filter(|l| !l.trim().is_empty()).count();
            (mismatches <= MAX_MISMATCHES && non_blank_evidence >= MIN_NON_BLANK_MATCH_EVIDENCE)
                .then_some((k, mismatches))
        })
        .min_by_key(|&(k, mismatches)| (mismatches, std::cmp::Reverse(k)))
        .map(|(k, _)| k)
}

/// Render the current (unscrolled) viewport as one plain-text line per row —
/// used by `TerminalState::process` to detect evicted content and capture
/// outgoing lines before they're overwritten.
fn viewport_rows(term: &Term<EventProxy>) -> Vec<String> {
    use alacritty_terminal::index::{Column, Line};

    let grid = term.grid();
    let cols = grid.columns();
    let rows = grid.screen_lines();

    (0..rows)
        .map(|row| {
            let line = Line(row as i32);
            (0..cols).map(|col| grid[line][Column(col)].c).collect::<String>()
        })
        .collect()
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
            extra_history: std::collections::VecDeque::new(),
            extra_offset: 0,
        }
    }

    /// Scroll the terminal display by `delta` lines (positive = up into
    /// history). Drains room in the native grid history first, then spills
    /// into `extra_history`; scrolling back down drains `extra_history`
    /// first, since it holds content further back than anything native
    /// scrolling can reach.
    pub fn scroll(&mut self, delta: i32) {
        use alacritty_terminal::grid::Scroll;

        if delta > 0 {
            let native_room =
                self.term.grid().history_size() as i32 - self.term.grid().display_offset() as i32;
            let into_native = delta.min(native_room.max(0));
            if into_native > 0 {
                self.term.grid_mut().scroll_display(Scroll::Delta(into_native));
            }
            let remaining = (delta - into_native) as usize;
            if remaining > 0 {
                self.extra_offset = (self.extra_offset + remaining).min(self.extra_history.len());
            }
        } else if delta < 0 {
            let want_down = (-delta) as usize;
            let from_extra = want_down.min(self.extra_offset);
            self.extra_offset -= from_extra;
            let remaining = (want_down - from_extra) as i32;
            if remaining > 0 {
                self.term.grid_mut().scroll_display(Scroll::Delta(-remaining));
            }
        }
        self.cache.clear();
    }

    /// Jump the display back down to the live viewport.
    pub fn scroll_to_bottom(&mut self) {
        use alacritty_terminal::grid::Scroll;
        self.extra_offset = 0;
        self.term.grid_mut().scroll_display(Scroll::Bottom);
        self.cache.clear();
    }

    /// Whether the display is currently scrolled up into history.
    pub fn is_scrolled_back(&self) -> bool {
        self.extra_offset > 0 || self.term.grid().display_offset() > 0
    }

    /// Combined scroll depth (native grid history + `extra_history`), and a
    /// lookup from a logical line index (0 = oldest available) into the text
    /// of that line, for rows that fall outside what the native grid can
    /// represent. Used by rendering to blend `extra_history` in once native
    /// history is exhausted.
    fn extra_line(&self, lines_past_native_history: usize) -> Option<&str> {
        let len = self.extra_history.len();
        if lines_past_native_history >= len {
            return None;
        }
        // `lines_past_native_history == 0` means "one line further back than
        // the oldest native history line", which is the *newest* entry in
        // `extra_history`.
        self.extra_history.get(len - 1 - lines_past_native_history).map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_clear_wipes_extra_history() {
        // `clear` (or `tput clear`) sends ESC[2J/ESC[3J — real sequences
        // Ink never emits — so seeing one means the user explicitly asked
        // for a clean slate, not just another TUI redraw.
        let mut s = TerminalState::new(80, 5);
        let frame = |lines: &[&str]| {
            let mut f = Vec::new();
            f.extend_from_slice(b"\x1b[H");
            f.extend_from_slice(lines.join("\r\n").as_bytes());
            f
        };
        s.process(&frame(&["a", "b", "c", "d", "e"]));
        s.process(&frame(&["b", "c", "d", "e", "f"])); // evicts "a"
        assert!(!s.extra_history.is_empty());

        s.process(b"\x1b[H\x1b[2J");
        assert!(s.extra_history.is_empty(), "clear should wipe captured scrollback too");
    }

    #[test]
    fn real_capture_produces_no_duplicate_numbers() {
        // A response that's still growing into unused blank rows (nothing
        // evicted yet) can trivially "match" a shift by coincidence, since
        // mostly-blank tails match mostly-blank tails almost anywhere —
        // firing a false eviction that gets re-captured for real once the
        // content is genuinely evicted later. Assert every number appears
        // in extra_history at most once.
        let mut s = TerminalState::new(140, 50);
        for chunk in CLAUDE_OVERFLOW_CAPTURE.chunks(4096) {
            s.process(chunk);
        }
        let mut seen = std::collections::HashSet::new();
        for line in &s.extra_history {
            let trimmed = line.trim();
            if trimmed.parse::<u32>().is_ok() || trimmed.strip_prefix("⏺ ").is_some_and(|n| n.parse::<u32>().is_ok()) {
                assert!(seen.insert(trimmed.to_string()), "{trimmed:?} captured more than once");
            }
        }
    }

    /// Raw PTY bytes captured from a real `claude` session (140x50) asked to
    /// print two number sequences back to back (1-40, then 100-180) — long
    /// enough that Ink's redraw evicts earlier content from the 50-row pane.
    /// No `ESC[2J`, no real linefeeds anywhere in it; redraws are pure
    /// cursor-addressed rewrites, and the eviction boundary isn't at a fixed
    /// row (it drifts). This is the exact shape of stream that broke the
    /// naive "diff once per process() call" and "row 0 changed" heuristics.
    const CLAUDE_OVERFLOW_CAPTURE: &[u8] = include_bytes!("testdata/claude_overflow_capture.bin");

    #[test]
    fn real_capture_all_content_reachable_across_pty_chunk_boundaries() {
        let mut s = TerminalState::new(140, 50);
        // Feed it the way tmux pipe-pane actually delivers it: ~4 KiB reads,
        // not one giant call — a single chunk can contain dozens of Ink
        // redraws, and `process` must catch evictions within a chunk, not
        // just at its edges.
        for chunk in CLAUDE_OVERFLOW_CAPTURE.chunks(4096) {
            s.process(chunk);
        }

        let mut lines: Vec<String> = s.extra_history.iter().cloned().collect();
        lines.extend(viewport_rows(&s.term));
        let trimmed: Vec<&str> = lines.iter().map(|l| l.trim()).collect();

        // The capture asked for 1-40, then 100-180 — no 41-99 was ever
        // printed, so don't assert on it.
        let mut last_pos = 0;
        for n in (1..=40).chain(100..=180) {
            let n = n.to_string();
            let bullet = format!("⏺ {n}"); // the first line of a response is bullet-prefixed
            let pos = trimmed.iter().position(|l| *l == n || *l == bullet).unwrap_or_else(|| {
                panic!("number {n} should be reachable somewhere in scrollback + live view")
            });
            // Not just reachable — in the right chronological order. A
            // naive "capture anything that vanished" heuristic can find
            // every number while still scattering them out of sequence,
            // which reads as a shuffled mess rather than a scrollable
            // transcript.
            assert!(
                pos >= last_pos,
                "number {n} appeared out of order (at {pos}, expected >= {last_pos})"
            );
            last_pos = pos;
        }
    }

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

    /// Build a redraw frame the way real Ink-based TUIs (Claude Code) send
    /// them: `ESC[H` (cursor home) followed by each line's text, a per-line
    /// erase, and cursor-relative moves to the next line — no `ESC[2J`, no
    /// literal newlines. Captured directly from a live `claude` session.
    fn ink_frame(lines: &[&str]) -> Vec<u8> {
        let mut f = Vec::new();
        f.extend_from_slice(b"\x1b[H");
        for (i, l) in lines.iter().enumerate() {
            f.extend_from_slice(l.as_bytes());
            f.extend_from_slice(b"\x1b[K");
            if i + 1 < lines.len() {
                f.extend_from_slice(b"\r\x1b[1B");
            }
        }
        f
    }

    #[test]
    fn cursor_addressed_redraw_preserves_evicted_content() {
        // No ESC[2J and no real linefeeds anywhere — exactly how Claude
        // Code's Ink renderer redraws. Content sliding out the top must
        // still be captured since alacritty's native scrollback is never
        // triggered by this style of redraw.
        let mut s = TerminalState::new(80, 5);

        s.process(&ink_frame(&["one", "two", "three", "four", "five"]));
        assert!(s.extra_history.is_empty(), "nothing evicted by the first frame");

        s.process(&ink_frame(&["two", "three", "four", "five", "six"]));
        assert!(
            s.extra_history.iter().any(|l| l == "one"),
            "content evicted by a real redraw should be preserved, got {:?}",
            s.extra_history
        );
    }

    #[test]
    fn in_place_status_tick_does_not_flood_extra_history() {
        // Only the last line (a spinner/elapsed-time counter) changes each
        // redraw — row 0 and the rest of the content stay identical. This
        // must not be mistaken for content being evicted.
        let mut s = TerminalState::new(80, 5);
        s.process(&ink_frame(&["one", "two", "three", "four", "Cogitated for 1s"]));
        for n in 2..10 {
            let last = format!("Cogitated for {n}s");
            s.process(&ink_frame(&["one", "two", "three", "four", &last]));
        }
        assert!(
            s.extra_history.is_empty(),
            "status-line-only changes must not be captured, got {:?}",
            s.extra_history
        );
    }

    #[test]
    fn identical_redraws_are_deduped() {
        let mut s = TerminalState::new(80, 5);
        s.process(&ink_frame(&["a", "b", "c", "d", "e"]));
        s.process(&ink_frame(&["b", "c", "d", "e", "f"])); // evicts "a"
        let count_after_evict = s.extra_history.len();
        assert!(count_after_evict > 0);

        // Reissuing the identical frame must not duplicate the capture.
        s.process(&ink_frame(&["b", "c", "d", "e", "f"]));
        assert_eq!(s.extra_history.len(), count_after_evict);
    }

    #[test]
    fn growing_cursor_addressed_content_is_all_reachable() {
        // A transcript sliding through a fixed-height window one line at a
        // time, exactly like Claude Code showing only the last N lines —
        // every message that ever appeared must remain scrollable.
        let mut s = TerminalState::new(80, 5);
        for i in 0u32..30 {
            let start = i.saturating_sub(4);
            let lines: Vec<String> = (start..=i).map(|n| format!("message {n}")).collect();
            let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
            s.process(&ink_frame(&refs));
        }
        for n in 0..25 {
            assert!(
                s.extra_history.iter().any(|l| l == &format!("message {n}")),
                "message {n} should still be reachable in history, got {:?}",
                s.extra_history
            );
        }
    }

    #[test]
    fn scrolling_reaches_extra_history_and_resets_to_bottom() {
        let mut s = TerminalState::new(80, 5);
        for i in 0u32..30 {
            let start = i.saturating_sub(4);
            let lines: Vec<String> = (start..=i).map(|n| format!("message {n}")).collect();
            let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
            s.process(&ink_frame(&refs));
        }
        assert!(!s.is_scrolled_back());

        s.scroll(100);
        assert!(s.is_scrolled_back());
        assert!(s.extra_offset > 0, "scrolling up should spill into extra_history");

        s.scroll_to_bottom();
        assert!(!s.is_scrolled_back());
        assert_eq!(s.extra_offset, 0);
    }
}
