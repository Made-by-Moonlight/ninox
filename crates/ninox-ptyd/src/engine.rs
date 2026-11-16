//! The terminal emulator behind each pane, hidden behind [`TerminalEngine`]
//! so `alacritty_terminal` can be swapped for another engine without touching
//! the host.
//!
//! The host feeds every byte the application writes into the engine, so the
//! engine sees the application's own escape sequences — DEC 2026
//! synchronized updates arrive intact and are applied atomically by the
//! parser (bytes inside an update are buffered until its end, or until
//! [`TerminalEngine::sync_deadline`] passes and the host calls
//! [`TerminalEngine::flush_sync`]).

use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use alacritty_terminal::event::{Event as TermEvent, EventListener, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line as GridLine};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color as VteColor, NamedColor, Processor, Rgb};

use crate::protocol::{Color, Cursor, Line, Modes, Run, ScreenSnapshot, Style};
use crate::render::lines_to_capture;

/// Default scrollback depth per pane.
pub const DEFAULT_SCROLLBACK: usize = 10_000;

/// Smallest grid alacritty accepts; requests below are clamped up.
const MIN_COLS: u16 = 2;
const MIN_ROWS: u16 = 1;

pub trait TerminalEngine: Send {
    /// Parse application output.
    fn feed(&mut self, bytes: &[u8]);
    /// Bytes the emulator wants written back to the PTY (DA/DSR/DECRQM
    /// replies, cursor position reports, …), drained. Applications such as
    /// Claude Code block on these, so the host must deliver them.
    fn take_replies(&mut self) -> Vec<u8>;
    /// Whether OSC 10/11/12 colour queries are answered by this engine.
    /// The host turns this off while a raw subscriber's real terminal is
    /// attached (and passes the query through to it), so the application
    /// gets exactly one, accurate, reply.
    fn set_answer_color_queries(&mut self, on: bool);
    fn resize(&mut self, cols: u16, rows: u16);
    fn size(&self) -> (u16, u16);
    /// Visible screen plus up to `scrollback` lines above it. `seq` is left
    /// at 0 for the caller to fill.
    fn snapshot(&self, scrollback: usize) -> ScreenSnapshot;
    fn modes(&self) -> Modes;
    fn title(&self) -> Option<String>;
    /// Lines of scrollback currently held above the visible screen.
    fn history_size(&self) -> usize;
    /// `capture-pane -S start -E end -e`: lines `start..=end` relative to
    /// the top of the visible screen (negative = scrollback), clamped to
    /// what exists, each followed by `\n`.
    fn history_ansi(&self, start: i64, end: i64) -> Vec<u8>;
    /// Cheap digest of everything a renderer would show (cells, cursor,
    /// modes, title, history depth). Equal digests mean nothing visible
    /// changed.
    fn fingerprint(&self) -> u64;
    /// When an unterminated synchronized update must be force-applied.
    fn sync_deadline(&self) -> Option<Instant>;
    fn flush_sync(&mut self);
}

/// Cell geometry assumed when answering pixel-size queries (`CSI 14 t`);
/// the host has no real font.
const CELL_PX: (u16, u16) = (8, 16);

/// Colours reported for OSC 10/11/12 queries when the application never set
/// them: a neutral dark theme.
const DEFAULT_FG: Rgb = Rgb { r: 0xd8, g: 0xd8, b: 0xd8 };
const DEFAULT_BG: Rgb = Rgb { r: 0x1e, g: 0x1e, b: 0x1e };

type ColorFormatter = Arc<dyn Fn(Rgb) -> String + Sync + Send>;
type SizeFormatter = Arc<dyn Fn(WindowSize) -> String + Sync + Send>;

#[derive(Default)]
struct ListenerState {
    replies: Vec<u8>,
    title: Option<String>,
    color_requests: Vec<(usize, ColorFormatter)>,
    size_requests: Vec<SizeFormatter>,
}

#[derive(Clone, Default)]
struct Listener(Arc<Mutex<ListenerState>>);

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        let mut st = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match event {
            TermEvent::PtyWrite(text) => st.replies.extend_from_slice(text.as_bytes()),
            TermEvent::Title(t) => st.title = Some(t),
            TermEvent::ResetTitle => st.title = None,
            TermEvent::ColorRequest(idx, f) => st.color_requests.push((idx, f)),
            TermEvent::TextAreaSizeRequest(f) => st.size_requests.push(f),
            _ => {}
        }
    }
}

struct Dims {
    cols: usize,
    rows: usize,
}

impl Dimensions for Dims {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

pub struct AlacrittyEngine {
    term: Term<Listener>,
    parser: Processor,
    listener: Listener,
    answer_colors: bool,
    scrollback_limit: usize,
}

impl AlacrittyEngine {
    pub fn new(cols: u16, rows: u16, scrollback: usize) -> Self {
        let (cols, rows) = clamp_size(cols, rows);
        let listener = Listener::default();
        let config = Config { scrolling_history: scrollback, kitty_keyboard: true, ..Config::default() };
        let term = Term::new(config, &Dims { cols: cols as usize, rows: rows as usize }, listener.clone());
        Self { term, parser: Processor::new(), listener, answer_colors: true, scrollback_limit: scrollback }
    }

    fn listener(&self) -> std::sync::MutexGuard<'_, ListenerState> {
        self.listener.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn grid_line(&self, line: i32) -> Line {
        let grid = self.term.grid();
        let row = &grid[GridLine(line)];
        let cols = grid.columns();
        // (text, style) per visible cell; wide-char spacers are skipped so a
        // wide glyph contributes one entry that occupies two columns.
        let mut cells: Vec<(String, Style)> = Vec::with_capacity(cols);
        let mut keep = 0usize;
        for col in 0..cols {
            let cell = &row[Column(col)];
            if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                continue;
            }
            let style = cell_style(cell);
            let mut text = String::new();
            if cell.flags.contains(Flags::HIDDEN) {
                text.push(' ');
            } else {
                text.push(cell.c);
                if let Some(zw) = cell.zerowidth() {
                    text.extend(zw.iter());
                }
            }
            let blank = text == " " && style == Style::default();
            cells.push((text, style));
            if !blank {
                keep = cells.len();
            }
        }
        let mut runs: Vec<Run> = Vec::new();
        for (text, style) in cells.into_iter().take(keep) {
            match runs.last_mut() {
                Some(r) if r.style == style => r.text.push_str(&text),
                _ => runs.push(Run { text, style }),
            }
        }
        let wrapped = cols > 0 && row[Column(cols - 1)].flags.contains(Flags::WRAPLINE);
        Line { runs, wrapped }
    }
}

fn clamp_size(cols: u16, rows: u16) -> (u16, u16) {
    (cols.max(MIN_COLS), rows.max(MIN_ROWS))
}

fn cell_style(cell: &Cell) -> Style {
    let f = cell.flags;
    Style {
        fg: map_color(cell.fg),
        bg: map_color(cell.bg),
        bold: f.contains(Flags::BOLD),
        dim: f.contains(Flags::DIM),
        italic: f.contains(Flags::ITALIC),
        underline: f.intersects(Flags::ALL_UNDERLINES),
        inverse: f.contains(Flags::INVERSE),
        strikethrough: f.contains(Flags::STRIKEOUT),
    }
}

fn map_color(c: VteColor) -> Color {
    match c {
        VteColor::Spec(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
        VteColor::Indexed(i) => Color::Indexed(i),
        VteColor::Named(n) => {
            let idx = n as usize;
            if idx < 16 {
                Color::Indexed(idx as u8)
            } else if (NamedColor::DimBlack as usize..=NamedColor::DimWhite as usize).contains(&idx) {
                Color::Indexed((idx - NamedColor::DimBlack as usize) as u8)
            } else {
                Color::Default
            }
        }
    }
}

fn color_key(c: VteColor) -> u32 {
    match c {
        VteColor::Named(n) => n as u32,
        VteColor::Indexed(i) => 0x1000 | i as u32,
        VteColor::Spec(Rgb { r, g, b }) => 0x0100_0000 | (r as u32) << 16 | (g as u32) << 8 | b as u32,
    }
}

fn kitty_flags(mode: TermMode) -> u8 {
    let mut f = 0u8;
    if mode.contains(TermMode::DISAMBIGUATE_ESC_CODES) {
        f |= 1;
    }
    if mode.contains(TermMode::REPORT_EVENT_TYPES) {
        f |= 2;
    }
    if mode.contains(TermMode::REPORT_ALTERNATE_KEYS) {
        f |= 4;
    }
    if mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC) {
        f |= 8;
    }
    if mode.contains(TermMode::REPORT_ASSOCIATED_TEXT) {
        f |= 16;
    }
    f
}

impl TerminalEngine for AlacrittyEngine {
    fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    fn take_replies(&mut self) -> Vec<u8> {
        let (cols, rows) = self.size();
        let colors = *self.term.colors();
        let answer_colors = self.answer_colors;
        let mut st = self.listener();
        let mut out = std::mem::take(&mut st.replies);
        for (idx, f) in st.color_requests.drain(..) {
            if !answer_colors {
                continue;
            }
            let rgb = colors[idx].unwrap_or(match idx {
                i if i == NamedColor::Background as usize => DEFAULT_BG,
                _ => DEFAULT_FG,
            });
            out.extend_from_slice(f(rgb).as_bytes());
        }
        for f in st.size_requests.drain(..) {
            let ws = WindowSize { num_lines: rows, num_cols: cols, cell_width: CELL_PX.0, cell_height: CELL_PX.1 };
            out.extend_from_slice(f(ws).as_bytes());
        }
        out
    }

    fn set_answer_color_queries(&mut self, on: bool) {
        self.answer_colors = on;
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        let (cols, rows) = clamp_size(cols, rows);
        self.term.resize(Dims { cols: cols as usize, rows: rows as usize });
    }

    fn size(&self) -> (u16, u16) {
        let g = self.term.grid();
        (g.columns() as u16, g.screen_lines() as u16)
    }

    fn snapshot(&self, scrollback: usize) -> ScreenSnapshot {
        let grid = self.term.grid();
        let rows = grid.screen_lines();
        let hist = grid.history_size().min(scrollback);
        let mut lines = Vec::with_capacity(hist + rows);
        for l in -(hist as i32)..rows as i32 {
            lines.push(self.grid_line(l));
        }
        let point = grid.cursor.point;
        let (cols, rows_u16) = self.size();
        ScreenSnapshot {
            cols,
            rows: rows_u16,
            seq: 0,
            lines,
            scrollback_len: hist,
            cursor: Cursor {
                row: point.line.0.clamp(0, rows as i32 - 1) as u16,
                col: point.column.0.min(cols as usize - 1) as u16,
                visible: self.term.mode().contains(TermMode::SHOW_CURSOR),
            },
            modes: self.modes(),
            title: self.title(),
        }
    }

    fn modes(&self) -> Modes {
        let m = *self.term.mode();
        Modes {
            alt_screen: m.contains(TermMode::ALT_SCREEN),
            app_cursor: m.contains(TermMode::APP_CURSOR),
            bracketed_paste: m.contains(TermMode::BRACKETED_PASTE),
            mouse_reporting: m.intersects(TermMode::MOUSE_MODE),
            sgr_mouse: m.contains(TermMode::SGR_MOUSE),
            kitty_keyboard: kitty_flags(m),
        }
    }

    fn title(&self) -> Option<String> {
        self.listener().title.clone()
    }

    fn history_size(&self) -> usize {
        self.term.grid().history_size()
    }

    fn history_ansi(&self, start: i64, end: i64) -> Vec<u8> {
        let grid = self.term.grid();
        let top = -(grid.history_size() as i64);
        let bottom = grid.screen_lines() as i64 - 1;
        let (start, end) = if start <= end { (start, end) } else { (end, start) };
        let start = start.clamp(top, bottom);
        let end = end.clamp(top, bottom);
        let lines: Vec<Line> = (start..=end).map(|l| self.grid_line(l as i32)).collect();
        lines_to_capture(&lines)
    }

    fn fingerprint(&self) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        let grid = self.term.grid();
        let cols = grid.columns();
        for l in 0..grid.screen_lines() as i32 {
            let row = &grid[GridLine(l)];
            for c in 0..cols {
                let cell = &row[Column(c)];
                cell.c.hash(&mut h);
                color_key(cell.fg).hash(&mut h);
                color_key(cell.bg).hash(&mut h);
                cell.flags.bits().hash(&mut h);
                if let Some(zw) = cell.zerowidth() {
                    zw.hash(&mut h);
                }
            }
        }
        grid.cursor.point.line.0.hash(&mut h);
        grid.cursor.point.column.0.hash(&mut h);
        grid.history_size().hash(&mut h);
        self.term.mode().bits().hash(&mut h);
        self.listener().title.hash(&mut h);
        h.finish()
    }

    fn sync_deadline(&self) -> Option<Instant> {
        self.parser.sync_timeout().sync_timeout()
    }

    fn flush_sync(&mut self) {
        self.parser.stop_sync(&mut self.term);
    }
}

impl AlacrittyEngine {
    /// Configured scrollback depth.
    pub fn scrollback_limit(&self) -> usize {
        self.scrollback_limit
    }
}
