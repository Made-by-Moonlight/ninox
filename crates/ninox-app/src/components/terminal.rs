use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::Term;
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Processor, Rgb};
use iced::widget::canvas::{Cache, Frame, Geometry, Path};
use iced::{Color as IcedColor, Rectangle, Size, Theme};

use crate::app::Message;
use crate::components::terminal_layout::{layout_line, LogicalCell, VisualLine};

const NERD_FONT: iced::Font = iced::Font {
    family: iced::font::Family::Name("Symbols Nerd Font Mono"),
    weight: iced::font::Weight::Normal,
    stretch: iced::font::Stretch::Normal,
    style: iced::font::Style::Normal,
};

pub const TERM_FONT_BYTES: &[u8] = include_bytes!("../../assets/fonts/JetBrainsMono-Regular.ttf");

pub const TERM_FONT: iced::Font = iced::Font {
    family:  iced::font::Family::Name("JetBrains Mono"),
    weight:  iced::font::Weight::Normal,
    stretch: iced::font::Stretch::Normal,
    style:   iced::font::Style::Normal,
};

const CONTEXTUAL_FONT_ANCHOR: char = '\u{feff}';
const LTR_FONT_ANCHOR: char = '.';

fn term_font_has_glyph(character: char) -> bool {
    use std::sync::OnceLock;
    static FACE: OnceLock<ttf_parser::Face<'static>> = OnceLock::new();
    FACE.get_or_init(|| {
        ttf_parser::Face::parse(TERM_FONT_BYTES, 0).expect("bundled terminal font parses")
    })
    .glyph_index(character)
    .is_some()
}

fn is_nerd_codepoint(character: char) -> bool {
    let codepoint = character as u32;
    (0xE000..=0xF8FF).contains(&codepoint)
        || (0xF0000..=0xFFFFD).contains(&codepoint)
        || (0x100000..=0x10FFFD).contains(&codepoint)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GlyphRendering {
    Basic,
    Contextual,
    Fallback,
}

fn glyph_rendering(character: char) -> GlyphRendering {
    if character.is_ascii() || is_nerd_codepoint(character) {
        GlyphRendering::Basic
    } else if term_font_has_glyph(character) {
        GlyphRendering::Contextual
    } else {
        GlyphRendering::Fallback
    }
}

fn font_for_cell(character: char, flags: Flags) -> iced::Font {
    if is_nerd_codepoint(character) {
        NERD_FONT
    } else {
        iced::Font {
            weight: if flags.intersects(Flags::BOLD) {
                iced::font::Weight::Bold
            } else {
                iced::font::Weight::Normal
            },
            style: if flags.contains(Flags::ITALIC) {
                iced::font::Style::Italic
            } else {
                iced::font::Style::Normal
            },
            ..TERM_FONT
        }
    }
}

fn draw_advanced_cell_glyph(
    frame: &mut Frame,
    character: char,
    mut text: iced::widget::canvas::Text,
    cell_width: f32,
    target_width: f32,
    cell_height: f32,
) {
    // Seed both shaping and the cold glyph atlas with a real bundled glyph.
    // Clip it outside the cell, then place the requested glyph at the cell
    // origin by compensating for JetBrains Mono's one-cell advance.
    let clip = Rectangle::new(text.position, Size::new(target_width, cell_height));
    text.content = format!("{LTR_FONT_ANCHOR}{character}");
    text.position.x -= cell_width;
    text.shaping = iced::widget::text::Shaping::Advanced;
    frame.with_clip(clip, |frame| frame.fill_text(text));
}

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
        let face =
            ttf_parser::Face::parse(TERM_FONT_BYTES, 0).expect("bundled terminal font parses");
        let upem = face.units_per_em() as f32;
        let advance = face
            .glyph_index('M')
            .and_then(|g| face.glyph_hor_advance(g))
            .expect("monospace advance") as f32;
        let height =
            (face.ascender() as f32 - face.descender() as f32 + face.line_gap() as f32).max(upem);
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

const SYNC_START: &[u8] = b"\x1b[?2026h";
const SYNC_END: &[u8] = b"\x1b[?2026l";
const EMPTY_SYNC_FRAME: &[u8] = b"\x1b[?2026h\x1b[?2026l";

#[derive(Debug, Default)]
struct TerminalOutputFramer {
    buffered:        Vec<u8>,
    deferred:        Vec<u8>,
    sync_open:       bool,
    recovery_token:  u64,
}

#[derive(Debug, Default)]
pub(crate) struct TerminalProcessResult {
    pub recovery_token: Option<u64>,
    #[cfg(test)]
    committed:          bool,
}

impl TerminalOutputFramer {
    fn push(&mut self, bytes: &[u8]) -> (Vec<u8>, Option<u64>) {
        let was_pending = self.is_pending();
        self.buffered.extend_from_slice(bytes);
        let mut ready = Vec::new();

        loop {
            if self.sync_open {
                let Some(end) = find_bytes(&self.buffered, SYNC_END) else {
                    break;
                };
                let frame: Vec<_> = self.buffered.drain(..end + SYNC_END.len()).collect();
                self.sync_open = false;
                self.finish_frame(frame, &mut ready);
                continue;
            }

            if let Some(start) = find_bytes(&self.buffered, SYNC_START) {
                let unframed: Vec<_> = self.buffered.drain(..start).collect();
                self.finish_unframed(unframed, &mut ready);
                self.sync_open = true;
                continue;
            }

            let retained = marker_prefix_suffix_len(&self.buffered, SYNC_START);
            let unframed: Vec<_> = self
                .buffered
                .drain(..self.buffered.len().saturating_sub(retained))
                .collect();
            self.finish_unframed(unframed, &mut ready);
            break;
        }

        let is_pending = self.is_pending();
        let recovery_token = if !was_pending && is_pending {
            self.recovery_token = self.recovery_token.wrapping_add(1);
            Some(self.recovery_token)
        } else {
            None
        };
        (ready, recovery_token)
    }

    fn recover(&mut self, token: u64) -> Vec<u8> {
        if token != self.recovery_token || !self.is_pending() {
            return Vec::new();
        }
        self.recover_bytes()
    }

    fn recover_bytes(&mut self) -> Vec<u8> {
        let mut ready = std::mem::take(&mut self.deferred);
        if self.sync_open {
            ready.append(&mut self.buffered);
            // Complete the parser transaction synthetically. Recovery is one
            // atomic commit, never a replay of each intermediate fragment.
            ready.extend_from_slice(SYNC_END);
        } else {
            // Ground state retains only a possible prefix of SYNC_START.
            // Dropping an abandoned partial escape is safer than poisoning
            // the VTE parser and mistaking later plain bytes for its suffix.
            self.buffered.clear();
        }
        self.sync_open = false;
        ready
    }

    fn finish_frame(&mut self, frame: Vec<u8>, ready: &mut Vec<u8>) {
        // tmux can emit an empty outer transaction after an unframed pane
        // repaint. It carries no terminal state, so advancing alacritty would
        // only invalidate and redraw an unchanged iced canvas.
        if frame == EMPTY_SYNC_FRAME {
            return;
        }
        let has_text = contains_terminal_text(&frame);
        if !self.deferred.is_empty() {
            self.deferred.extend_from_slice(&frame);
            if has_text {
                ready.append(&mut self.deferred);
            }
        } else if !has_text && contains_structural_csi(&frame) {
            // tmux 3.7 can leak an erase from an inner synchronized update
            // into its own balanced outer frame. Hold that destructive-only
            // frame until tmux's final content repaint arrives.
            self.deferred = frame;
        } else {
            ready.extend_from_slice(&frame);
        }
    }

    fn finish_unframed(&mut self, bytes: Vec<u8>, ready: &mut Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        if self.deferred.is_empty() {
            ready.extend_from_slice(&bytes);
            return;
        }

        let has_text = contains_terminal_text(&bytes);
        self.deferred.extend_from_slice(&bytes);
        if has_text {
            ready.append(&mut self.deferred);
        }
    }

    fn is_pending(&self) -> bool {
        self.sync_open || !self.buffered.is_empty() || !self.deferred.is_empty()
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

fn marker_prefix_suffix_len(bytes: &[u8], marker: &[u8]) -> usize {
    (1..marker.len())
        .rev()
        .find(|&length| bytes.ends_with(&marker[..length]))
        .unwrap_or(0)
}

fn terminal_controls(bytes: &[u8], mut visit_csi: impl FnMut(u8)) -> bool {
    let mut index = 0;
    let mut has_text = false;
    while index < bytes.len() {
        match bytes[index] {
            0x1b if bytes.get(index + 1) == Some(&b'[') => {
                index += 2;
                while index < bytes.len() {
                    let byte = bytes[index];
                    index += 1;
                    if (0x40..=0x7e).contains(&byte) {
                        visit_csi(byte);
                        break;
                    }
                }
            }
            0x1b if bytes.get(index + 1) == Some(&b']') => {
                index += 2;
                while index < bytes.len() {
                    if bytes[index] == 0x07 {
                        index += 1;
                        break;
                    }
                    if bytes[index] == 0x1b && bytes.get(index + 1) == Some(&b'\\') {
                        index += 2;
                        break;
                    }
                    index += 1;
                }
            }
            0x1b => index = (index + 2).min(bytes.len()),
            byte if byte < 0x20 || byte == 0x7f => index += 1,
            _ => {
                has_text = true;
                index += 1;
            }
        }
    }
    has_text
}

fn contains_terminal_text(bytes: &[u8]) -> bool {
    terminal_controls(bytes, |_| {})
}

fn contains_structural_csi(bytes: &[u8]) -> bool {
    let mut structural = false;
    terminal_controls(bytes, |final_byte| {
        structural |= matches!(final_byte, b'@' | b'J' | b'K' | b'L' | b'M' | b'P' | b'S' | b'T' | b'X');
    });
    structural
}

pub struct TerminalState {
    pub term: Term<EventProxy>,
    pub cache: Cache,
    /// On-demand tmux history cache + scroll position for this view. The
    /// live alacritty grid never accumulates scrollback of its own (the
    /// client stream is a full-screen tmux UI), so all history rendering
    /// comes from here.
    pub scrollback: crate::components::scrollback::Scrollback,
    parser: Processor,
    output_framer: TerminalOutputFramer,
    #[cfg(test)]
    output_commits: usize,
}

impl TerminalState {
    pub fn new(
        cols: u16,
        rows: u16,
        reply: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    ) -> Self {
        use alacritty_terminal::term::{test::TermSize, Config};
        let size = TermSize::new(cols as usize, rows as usize);
        let config = Config {
            kitty_keyboard: true,
            // tmux owns the durable 100k-row history. Retaining a second
            // emulator copy makes an initial attach proportional to the whole
            // chat and defeats bounded page hydration.
            scrolling_history: 0,
            ..Config::default()
        };
        let term = Term::new(config, &size, EventProxy(reply));
        Self {
            term,
            cache: Cache::new(),
            scrollback: Default::default(),
            parser: Processor::new(),
            output_framer: TerminalOutputFramer::default(),
            #[cfg(test)]
            output_commits: 0,
        }
    }

    /// Feed raw bytes from the attached tmux client into the emulator.
    pub(crate) fn process(&mut self, bytes: &[u8]) -> TerminalProcessResult {
        let (ready, recovery_token) = self.output_framer.push(bytes);
        #[cfg(test)]
        let committed = self.commit_output(&ready);
        #[cfg(not(test))]
        self.commit_output(&ready);
        TerminalProcessResult {
            recovery_token,
            #[cfg(test)]
            committed,
        }
    }

    pub(crate) fn recover_output(&mut self, token: u64) -> bool {
        let ready = self.output_framer.recover(token);
        self.commit_output(&ready)
    }

    fn commit_output(&mut self, bytes: &[u8]) -> bool {
        if bytes.is_empty() {
            return false;
        }
        let before = self
            .is_scrolled_back()
            .then(|| visible_styled_lines(&self.term));
        self.parser.advance(&mut self.term, bytes);
        if let Some(before) = before {
            let after = visible_styled_lines(&self.term);
            let scrolled = detect_live_scroll(&before, &after);
            self.scrollback.append_live_scrolled(scrolled);
        }
        self.cache.clear();
        #[cfg(test)]
        {
            self.output_commits += 1;
        }
        true
    }

    #[cfg(test)]
    fn output_commit_count(&self) -> usize {
        self.output_commits
    }

    /// Paint a bounded `capture-pane` tail as one synchronized first frame.
    /// The hidden tmux client's stateful redraw follows through the regular
    /// stream, but the user sees the current end before that process starts.
    pub fn hydrate_viewport_tail(&mut self, capture: &ninox_core::tmux::ViewportTailCapture) {
        let mut frame = Vec::with_capacity(capture.bytes.len() + 64);
        frame.extend_from_slice(b"\x1b[?2026h\x1b[H\x1b[2J");
        for &byte in &capture.bytes {
            if byte == b'\n' {
                frame.push(b'\r');
            }
            frame.push(byte);
        }
        use std::fmt::Write;
        write!(
            StringWriter(&mut frame),
            "\x1b[{};{}H\x1b[?2026l",
            capture.cursor_y.saturating_add(1),
            capture.cursor_x.saturating_add(1)
        )
        .expect("writing to Vec cannot fail");
        self.process(&frame);
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
        self.scrollback.reset();
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
        self.scrollback.reset();
        self.cache.clear();
    }
}

struct StringWriter<'a>(&'a mut Vec<u8>);

impl std::fmt::Write for StringWriter<'_> {
    fn write_str(&mut self, value: &str) -> std::fmt::Result {
        self.0.extend_from_slice(value.as_bytes());
        Ok(())
    }
}

fn visible_styled_lines(term: &Term<EventProxy>) -> Vec<crate::components::scrollback::StyledLine> {
    use alacritty_terminal::{
        grid::Dimensions,
        index::{Column, Line},
    };
    let grid = term.grid();
    (0..grid.screen_lines())
        .map(|row| {
            (0..grid.columns())
                .map(|column| {
                    let cell = &grid[Line(row as i32)][Column(column)];
                    crate::components::scrollback::StyledCell {
                        c: cell.c,
                        zerowidth: cell.zerowidth().unwrap_or_default().to_vec(),
                        fg: cell.fg,
                        bg: cell.bg,
                        flags: cell.flags,
                        hyperlink: cell.hyperlink().map(|link| link.uri().to_string()),
                    }
                })
                .collect()
        })
        .collect()
}

fn detect_live_scroll(
    before: &[crate::components::scrollback::StyledLine],
    after: &[crate::components::scrollback::StyledLine],
) -> Vec<crate::components::scrollback::StyledLine> {
    if before.len() != after.len() || before == after {
        return Vec::new();
    }
    for shifted in 1..before.len() {
        if before[shifted..] == after[..before.len() - shifted] {
            return before[..shifted].to_vec();
        }
    }
    Vec::new()
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
    /// Logical anchor cell used for links and source-text compatibility.
    anchor: Option<(usize, usize)>,
    /// Logical end cell used for source-text compatibility.
    end: Option<(usize, usize)>,
    /// Visual endpoints preserve the exact painted span on BiDi rows.
    visual_anchor: Option<(usize, usize)>,
    visual_end:    Option<(usize, usize)>,
    dragging: bool,
    /// Whether the cursor moved after the press (distinguishes click from drag).
    moved: bool,
    /// Whether the cell under the cursor is currently part of a clickable
    /// link — drives the pointer cursor via `mouse_interaction`.
    hovering_link: bool,
    /// Trackpads emit many sub-row pixel deltas. Preserve the fractional
    /// motion instead of truncating every event independently.
    scroll_pixel_remainder: f32,
}

impl SelectionState {
    fn normalized_range(
        anchor: Option<(usize, usize)>,
        end: Option<(usize, usize)>,
    ) -> Option<((usize, usize), (usize, usize))> {
        let (a_col, a_row) = anchor?;
        let (e_col, e_row) = end?;
        if a_row < e_row || (a_row == e_row && a_col <= e_col) {
            Some(((a_col, a_row), (e_col, e_row)))
        } else {
            Some(((e_col, e_row), (a_col, a_row)))
        }
    }

    /// Normalised logical endpoints for legacy/programmatic selections.
    fn range(&self) -> Option<((usize, usize), (usize, usize))> {
        Self::normalized_range(self.anchor, self.end)
    }

    /// Normalised painted endpoints, falling back to logical coordinates for
    /// programmatic selections that predate visual tracking.
    fn visual_range(&self) -> Option<((usize, usize), (usize, usize))> {
        Self::normalized_range(self.visual_anchor, self.visual_end).or_else(|| self.range())
    }

    fn pixel_to_cell(
        x: f32,
        y: f32,
        cell_w: f32,
        cell_h: f32,
        cols: usize,
        rows: usize,
    ) -> (usize, usize) {
        let col = ((x / cell_w) as usize).min(cols.saturating_sub(1));
        let row = ((y / cell_h) as usize).min(rows.saturating_sub(1));
        (col, row)
    }

    fn consume_scroll_delta(&mut self, delta: &iced::mouse::ScrollDelta, cell_height: f32) -> i32 {
        if !cell_height.is_finite() || cell_height <= 0.0 {
            return 0;
        }
        let pixels = match delta {
            iced::mouse::ScrollDelta::Lines { y, .. } => *y * 3.0 * cell_height,
            iced::mouse::ScrollDelta::Pixels { y, .. } => *y,
        };
        if !pixels.is_finite() || pixels == 0.0 {
            return 0;
        }
        if self.scroll_pixel_remainder != 0.0
            && self.scroll_pixel_remainder.signum() != pixels.signum()
        {
            self.scroll_pixel_remainder = 0.0;
        }

        let total = self.scroll_pixel_remainder + pixels;
        let lines = (total / cell_height).trunc() as i32;
        self.scroll_pixel_remainder = total - lines as f32 * cell_height;
        lines
    }
}

fn renderable_cursor(term: &Term<EventProxy>) -> alacritty_terminal::term::RenderableCursor {
    term.renderable_content().cursor
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DisplayCell {
    c:         char,
    zerowidth: Vec<char>,
    fg:        Color,
    bg:        Color,
    flags:     Flags,
}

impl Default for DisplayCell {
    fn default() -> Self {
        Self {
            c:         ' ',
            zerowidth: Vec::new(),
            fg:        Color::Named(NamedColor::Foreground),
            bg:        Color::Named(NamedColor::Background),
            flags:     Flags::empty(),
        }
    }
}

#[cfg(test)]
#[derive(Clone, Debug, PartialEq)]
struct RowDrawCommands {
    cells:          Vec<DisplayCell>,
    layout:         VisualLine,
    link_spans:     Vec<crate::components::links::LinkSpan>,
    logical:        i32,
    cursor:         Option<(usize, CursorShape)>,
    selected_cells: Vec<bool>,
    colors:         Vec<Option<Rgb>>,
    font_size:      f32,
    terminal_bg:    IcedColor,
    terminal_fg:    IcedColor,
    cursor_color:   IcedColor,
    ansi:           [IcedColor; 16],
}

fn visual_layout(cells: &[DisplayCell]) -> VisualLine {
    let logical: Vec<_> = cells
        .iter()
        .enumerate()
        .filter(|(_, cell)| !cell.flags.contains(Flags::WIDE_CHAR_SPACER))
        .map(|(column, cell)| {
            let mut text = String::from(if cell.c == '\0' { ' ' } else { cell.c });
            text.extend(cell.zerowidth.iter().copied());
            LogicalCell {
                column,
                text,
                width: if cell.flags.contains(Flags::WIDE_CHAR) { 2 } else { 1 },
            }
        })
        .collect();
    layout_line(&logical, cells.len())
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
    fn row_display_cells(&self, row: usize) -> Option<Vec<DisplayCell>> {
        use alacritty_terminal::index::{Column, Line};

        let grid = self.state.term.grid();
        let cols = grid.columns();
        if row >= grid.screen_lines() {
            return None;
        }
        let logical = row as i32 - self.state.scrollback.offset as i32;
        if logical < 0 {
            let history = self.state.scrollback.line_above((-logical - 1) as usize)?;
            let mut cells: Vec<_> = history
                .iter()
                .take(cols)
                .map(|cell| DisplayCell {
                    c:         cell.c,
                    zerowidth: cell.zerowidth.clone(),
                    fg:        cell.fg,
                    bg:        cell.bg,
                    flags:     cell.flags,
                })
                .collect();
            cells.resize(cols, DisplayCell::default());
            Some(cells)
        } else {
            Some(
                (0..cols)
                    .map(|column| {
                        let cell = &grid[Line(logical)][Column(column)];
                        DisplayCell {
                            c:         cell.c,
                            zerowidth: cell.zerowidth().unwrap_or_default().to_vec(),
                            fg:        cell.fg,
                            bg:        cell.bg,
                            flags:     cell.flags,
                        }
                    })
                    .collect(),
            )
        }
    }

    fn pixel_to_logical_cell(
        &self,
        x: f32,
        y: f32,
        cell_w: f32,
        cell_h: f32,
        cols: usize,
        rows: usize,
    ) -> (usize, usize) {
        let (visual_col, row) =
            SelectionState::pixel_to_cell(x, y, cell_w, cell_h, cols, rows);
        self.visual_to_logical_cell(visual_col, row)
    }

    fn visual_to_logical_cell(&self, visual_col: usize, row: usize) -> (usize, usize) {
        let logical_col = self
            .row_display_cells(row)
            .map(|cells| visual_layout(&cells).visual_to_logical[visual_col])
            .unwrap_or(visual_col);
        (logical_col, row)
    }

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
            cells
                .iter()
                .map(|c| LinkCell {
                    c: c.c,
                    hyperlink: c.hyperlink.as_deref(),
                })
                .collect()
        } else {
            use alacritty_terminal::index::{Column, Line};
            let line = Line(logical);
            *hyperlink_storage = (0..cols)
                .map(|c| grid[line][Column(c)].hyperlink())
                .collect();
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
    fn link_at_logical(&self, col: usize, row: usize) -> Option<String> {
        let mut hyperlink_storage = Vec::new();
        crate::components::links::link_at(&self.row_link_cells(row, &mut hyperlink_storage), col)
    }

    /// A `CopyToClipboard` message for `sel`'s current selection, or `None`
    /// if there's no selection or it's blank. Shared by the drag-release
    /// auto-copy and the explicit Cmd+C / Ctrl+Shift+C shortcut so the two
    /// paths can't drift apart.
    fn copy_message(&self, sel: &SelectionState) -> Option<Message> {
        let selected = if sel.visual_anchor.is_some() && sel.visual_end.is_some() {
            sel.visual_range()
                .map(|((sc, sr), (ec, er))| self.extract_visual_selection(sc, sr, ec, er))
        } else {
            sel.range()
                .map(|((sc, sr), (ec, er))| extract_selection(self.state, sc, sr, ec, er))
        };
        selected
            .filter(|s| !s.trim().is_empty())
            .map(Message::CopyToClipboard)
    }

    fn extract_visual_selection(
        &self,
        start_col: usize,
        start_row: usize,
        end_col: usize,
        end_row: usize,
    ) -> String {
        let cols = self.state.term.grid().columns();
        let rows = self.state.term.grid().screen_lines();
        let mut out = String::new();

        for row in start_row..=end_row.min(rows.saturating_sub(1)) {
            let Some(cells) = self.row_display_cells(row) else {
                continue;
            };
            let col_start = if row == start_row { start_col } else { 0 };
            let col_end = if row == end_row {
                end_col
            } else {
                cols.saturating_sub(1)
            }
            .min(cols.saturating_sub(1));
            let layout = visual_layout(&cells);
            let mut logical_columns =
                layout.visual_to_logical[col_start.min(col_end)..=col_end].to_vec();
            logical_columns.sort_unstable();
            logical_columns.dedup();

            let mut line_text = String::new();
            for logical_col in logical_columns {
                let cell = &cells[logical_col];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                line_text.push(if cell.c == '\0' { ' ' } else { cell.c });
                line_text.extend(cell.zerowidth.iter().copied());
            }
            out.push_str(line_text.trim_end());
            let wrapped = cells
                .last()
                .is_some_and(|cell| cell.flags.contains(Flags::WRAPLINE));
            if row < end_row && !wrapped {
                out.push('\n');
            }
        }
        out
    }

    #[cfg(test)]
    fn row_draw_commands(&self, selection: &SelectionState) -> Vec<RowDrawCommands> {
        use alacritty_terminal::index::Line;
        use alacritty_terminal::term::color::COUNT;

        let grid = self.state.term.grid();
        let offset = self.state.scrollback.offset as i32;
        let cursor = renderable_cursor(&self.state.term);
        let colors = self.state.term.colors();
        let colors: Vec<_> = (0..COUNT).map(|index| colors[index]).collect();

        (0..grid.screen_lines())
            .filter_map(|row| {
                let logical = row as i32 - offset;
                let cells = self.row_display_cells(row)?;
                let cursor = (offset == 0
                    && logical >= 0
                    && cursor.point.line == Line(logical))
                    .then_some((cursor.point.column.0, cursor.shape));

                Some(RowDrawCommands {
                    layout: visual_layout(&cells),
                    link_spans: self.row_link_spans(row),
                    selected_cells: (0..grid.columns())
                        .map(|column| cell_is_selected(selection, row, column))
                        .collect(),
                    cells,
                    logical,
                    cursor,
                    colors: colors.clone(),
                    font_size: self.font_size,
                    terminal_bg: self.terminal_bg,
                    terminal_fg: self.terminal_fg,
                    cursor_color: self.cursor_color,
                    ansi: self.ansi,
                })
            })
            .collect()
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
                    let visual =
                        SelectionState::pixel_to_cell(pos.x, pos.y, cell_w, cell_h, cols, rows);
                    let logical = self.visual_to_logical_cell(visual.0, visual.1);
                    state.anchor = Some(logical);
                    state.end = Some(logical);
                    state.visual_anchor = Some(visual);
                    state.visual_end = Some(visual);
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
                    .map(|pos| {
                        self.pixel_to_logical_cell(pos.x, pos.y, cell_w, cell_h, cols, rows)
                    })
                    .is_some_and(|(col, row)| self.link_at_logical(col, row).is_some());
                state.hovering_link = hovering;

                if state.dragging {
                    if let Some(pos) = cursor.position_in(bounds) {
                        let visual =
                            SelectionState::pixel_to_cell(pos.x, pos.y, cell_w, cell_h, cols, rows);
                        let logical = self.visual_to_logical_cell(visual.0, visual.1);
                        if state.visual_anchor != Some(visual) {
                            state.moved = true;
                        }
                        state.end = Some(logical);
                        state.visual_end = Some(visual);
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
                        let visual_col = ((pos.x - bounds.x) / cell_w) as usize;
                        let visual_col = visual_col.min(cols.saturating_sub(1));
                        if pos.y < bounds.y {
                            let col = self
                                .row_display_cells(0)
                                .map(|cells| visual_layout(&cells).visual_to_logical[visual_col])
                                .unwrap_or(visual_col);
                            state.end = Some((col, 0));
                            state.visual_end = Some((visual_col, 0));
                            state.moved = true;
                            self.state.cache.clear();
                            return (
                                iced::widget::canvas::event::Status::Captured,
                                Some(Message::ScrollTerminal {
                                    session_id: self.session_id.clone(),
                                    delta: 3,
                                    local: true,
                                }),
                            );
                        } else if pos.y > bounds.y + bounds.height {
                            let row = rows.saturating_sub(1);
                            let col = self
                                .row_display_cells(row)
                                .map(|cells| visual_layout(&cells).visual_to_logical[visual_col])
                                .unwrap_or(visual_col);
                            state.end = Some((col, row));
                            state.visual_end = Some((visual_col, row));
                            state.moved = true;
                            self.state.cache.clear();
                            return (
                                iced::widget::canvas::event::Status::Captured,
                                Some(Message::ScrollTerminal {
                                    session_id: self.session_id.clone(),
                                    delta: -3,
                                    local: true,
                                }),
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
                    if let (Some((col, row)), Some(pos)) =
                        (state.anchor, cursor.position_in(bounds))
                    {
                        let _ = pos; // bounds-checked via anchor
                        if let Some(url) = self.link_at_logical(col, row) {
                            state.anchor = None;
                            state.end    = None;
                            state.visual_anchor = None;
                            state.visual_end = None;
                            return (
                                iced::widget::canvas::event::Status::Captured,
                                Some(Message::OpenUrl(url)),
                            );
                        }
                        let word = word_at(self.state.term.grid(), col, row);
                        if self.session_ids.iter().any(|id| id == &word) {
                            state.anchor = None;
                            state.end    = None;
                            state.visual_anchor = None;
                            state.visual_end = None;
                            return (
                                iced::widget::canvas::event::Status::Captured,
                                Some(Message::NavigateSession(word)),
                            );
                        }
                    }
                    state.anchor = None;
                    state.end    = None;
                    state.visual_anchor = None;
                    state.visual_end = None;
                    return (iced::widget::canvas::event::Status::Captured, None);
                }
                // Drag — copy the selection.
                return (
                    iced::widget::canvas::event::Status::Captured,
                    self.copy_message(state),
                );
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
                return (
                    iced::widget::canvas::event::Status::Captured,
                    self.copy_message(state),
                );
            }

            // Emit RawKey so the handler can apply APP_CURSOR-aware conversion.
            Event::Keyboard(KeyEvent::KeyPressed {
                key,
                modifiers,
                text,
                ..
            }) => {
                let msg = Message::RawKey {
                    key: key.clone(),
                    modifiers: *modifiers,
                    text: text.as_ref().map(|t| t.as_str().to_string()),
                };
                return (iced::widget::canvas::event::Status::Captured, Some(msg));
            }

            Event::Mouse(MouseEvent::WheelScrolled { delta }) => {
                if cursor.position_in(bounds).is_none() {
                    return (iced::widget::canvas::event::Status::Ignored, None);
                }

                // Positive y = scroll up into history. Fractional trackpad
                // motion is retained in canvas state until it crosses a row.
                let lines = state.consume_scroll_delta(delta, cell_h);
                if lines != 0 {
                    return (
                        iced::widget::canvas::event::Status::Captured,
                        Some(Message::ScrollTerminal {
                            session_id: self.session_id.clone(),
                            delta: lines,
                            local: false,
                        }),
                    );
                }
                return (iced::widget::canvas::event::Status::Captured, None);
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
        let cursor = renderable_cursor(term);
        let cursor_point = cursor.point;
        // How many rows the view is scrolled up into tmux history. The
        // native alacritty grid holds no scrollback of its own (the client
        // stream is a full-screen tmux UI), so this offset is entirely
        // driven by `Scrollback`, not `grid.display_offset()`.
        let offset = self.state.scrollback.offset as i32;

        let term_bg = self.terminal_bg;
        let term_fg = self.terminal_fg;
        let cursor_color = self.cursor_color;
        // `renderable_content` applies both DECSCUSR shape and DEC private
        // cursor visibility. TUIs hide the cursor while repainting; drawing
        // the raw grid cursor exposes their transient write position.
        let cursor_shape = cursor.shape;

        let geometry = self
            .state
            .cache
            .draw(renderer, bounds.size(), |frame: &mut Frame| {
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
                let Some(cells) = self.row_display_cells(row) else {
                    continue;
                };
                let layout = visual_layout(&cells);
                let link_spans = self.row_link_spans(row);
                let mut shaped = vec![false; cols];
                for run in layout.runs.iter().filter(|run| run.rtl) {
                    shaped[run.visual_start..(run.visual_start + run.visual_width).min(cols)]
                        .fill(true);
                }
                for (visual_col, is_shaped) in shaped.iter().copied().enumerate() {
                    let logical_col = layout.visual_to_logical[visual_col];
                    let cell = &cells[logical_col];
                    let x = visual_col as f32 * cell_w;

                    // The cursor is suppressed whenever the view is scrolled
                    // back — it lives on the live screen, not in history.
                    let is_cursor = offset == 0
                        && logical >= 0
                        && cursor_point.line == Line(logical)
                        && cursor_point.column == Column(logical_col);
                    let is_selected = cell_is_selected(sel, row, visual_col);
                    let is_link = link_spans
                        .iter()
                        .any(|span| {
                            logical_col >= span.start_col && logical_col <= span.end_col
                        });

                    draw_cell(
                        frame,
                        x,
                        y,
                        cell_w,
                        cell_h,
                        self.font_size,
                        if is_shaped || !cell.zerowidth.is_empty() { ' ' } else { cell.c },
                        cell.fg,
                        cell.bg,
                        cell.flags,
                        is_cursor,
                        cursor_shape,
                        is_selected,
                        is_link,
                        colors,
                        &self.ansi,
                        term_bg,
                        term_fg,
                        cursor_color,
                    );
                    if !is_shaped && !cell.zerowidth.is_empty() {
                        draw_combining_cell(
                            frame,
                            x,
                            y,
                            cell_w,
                            cell_h,
                            self.font_size,
                            cell,
                            is_cursor && cursor_shape == CursorShape::Block,
                            colors,
                            &self.ansi,
                            term_bg,
                            term_fg,
                        );
                    }
                }
                draw_glyph_runs(
                    frame,
                    &layout,
                    &cells,
                    y,
                    cell_w,
                    cell_h,
                    self.font_size,
                    logical,
                    offset,
                    cursor_point,
                    cursor_shape,
                    colors,
                    &self.ansi,
                    term_bg,
                    term_fg,
                );
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
    sel.visual_range()
        .map(|((sc, sr), (ec, er))| {
        let in_row = row >= sr && row <= er;
            if !in_row {
                return false;
            }
            if sr == er {
                col >= sc && col <= ec
            } else if row == sr {
                col >= sc
            } else if row == er {
                col <= ec
            } else {
                true
            }
        })
        .unwrap_or(false)
}

/// Draw a 1px decoration stroke (underline/strikeout/undercurl segments).
fn stroke_line(frame: &mut Frame, x1: f32, y1: f32, x2: f32, y2: f32, color: IcedColor) {
    let path = Path::line(iced::Point::new(x1, y1), iced::Point::new(x2, y2));
    frame.stroke(
        &path,
        iced::widget::canvas::Stroke::default()
            .with_width(1.0)
            .with_color(color),
    );
}

fn resolved_cell_colors(
    fg_color: Color,
    bg_color: Color,
    flags: Flags,
    colors: &alacritty_terminal::term::color::Colors,
    ansi: &[IcedColor; 16],
    term_bg: IcedColor,
    term_fg: IcedColor,
) -> (IcedColor, IcedColor) {
    let mut fg = ansi_to_iced(fg_color, colors, ansi, term_bg, term_fg);
    let mut bg = ansi_to_iced(bg_color, colors, ansi, term_bg, term_fg);
    if flags.contains(Flags::INVERSE) {
        std::mem::swap(&mut fg, &mut bg);
    }
    if flags.contains(Flags::DIM) {
        fg.a *= 0.6;
    }
    if flags.contains(Flags::HIDDEN) {
        fg = bg;
    }
    (fg, bg)
}

fn shaped_run_width(content: &str, font: iced::Font, font_size: f32) -> f32 {
    use iced::advanced::graphics::text::{self, cosmic_text};

    let mut font_system = text::font_system().write().expect("write font system");
    let mut buffer = cosmic_text::BufferLine::new(
        content,
        cosmic_text::LineEnding::default(),
        cosmic_text::AttrsList::new(text::to_attributes(font)),
        cosmic_text::Shaping::Advanced,
    );
    buffer
        .layout(
            font_system.raw(),
            font_size,
            None,
            cosmic_text::Wrap::None,
            None,
            4,
        )
        .iter()
        .map(|line| line.w)
        .fold(0.0, f32::max)
}

#[allow(clippy::too_many_arguments)]
fn draw_combining_cell(
    frame: &mut Frame,
    x: f32,
    y: f32,
    cell_w: f32,
    cell_h: f32,
    font_size: f32,
    cell: &DisplayCell,
    block_cursor: bool,
    colors: &alacritty_terminal::term::color::Colors,
    ansi: &[IcedColor; 16],
    term_bg: IcedColor,
    term_fg: IcedColor,
) {
    let font = font_for_cell(cell.c, cell.flags);
    let (mut fg, _) =
        resolved_cell_colors(cell.fg, cell.bg, cell.flags, colors, ansi, term_bg, term_fg);
    if block_cursor {
        fg = term_bg;
    }
    let mut content = String::from(CONTEXTUAL_FONT_ANCHOR);
    content.push(cell.c);
    content.extend(cell.zerowidth.iter().copied());
    let natural_width = shaped_run_width(&content, font, font_size);
    let target_width = if cell.flags.contains(Flags::WIDE_CHAR) {
        2.0 * cell_w
    } else {
        cell_w
    };
    let scale_x = if natural_width > 0.0 { target_width / natural_width } else { 1.0 };
    let clip = Rectangle::new(iced::Point::new(x, y), Size::new(target_width, cell_h));
    frame.with_clip(clip, |frame| {
        frame.with_save(|frame| {
            frame.scale_nonuniform(iced::Vector::new(scale_x, 1.0));
            frame.fill_text(iced::widget::canvas::Text {
                content,
                position: iced::Point::new(x / scale_x, y),
                color: fg,
                size: iced::Pixels(font_size),
                font,
                horizontal_alignment: iced::alignment::Horizontal::Left,
                vertical_alignment: iced::alignment::Vertical::Top,
                line_height: iced::widget::text::LineHeight::Relative(cell_h / font_size),
                shaping: iced::widget::text::Shaping::Advanced,
            });
        });
    });
}

#[allow(clippy::too_many_arguments)]
fn draw_glyph_runs(
    frame: &mut Frame,
    layout: &VisualLine,
    cells: &[DisplayCell],
    y: f32,
    cell_w: f32,
    cell_h: f32,
    font_size: f32,
    logical_row: i32,
    scroll_offset: i32,
    cursor_point: alacritty_terminal::index::Point,
    cursor_shape: CursorShape,
    colors: &alacritty_terminal::term::color::Colors,
    ansi: &[IcedColor; 16],
    term_bg: IcedColor,
    term_fg: IcedColor,
) {
    use alacritty_terminal::index::{Column, Line};

    for run in layout.runs.iter().filter(|run| run.rtl) {
        let start_x = run.visual_start as f32 * cell_w;
        let end_x = (run.visual_start + run.visual_width) as f32 * cell_w;
        let content = format!("{CONTEXTUAL_FONT_ANCHOR}{}", run.text);

        // Render the same fully shaped run through style-span clips. This keeps
        // joining context across SGR boundaries while preserving each span's
        // color and fixed cell geometry.
        let run_end = run.visual_start + run.visual_width;
        let glyph_style = |visual_col: usize| {
            let logical_col = layout.visual_to_logical[visual_col];
            let cell = &cells[logical_col];
            let (mut fg, _) = resolved_cell_colors(
                cell.fg, cell.bg, cell.flags, colors, ansi, term_bg, term_fg,
            );
            let block_cursor = scroll_offset == 0
                && logical_row >= 0
                && cursor_point.line == Line(logical_row)
                && cursor_point.column == Column(logical_col)
                && cursor_shape == CursorShape::Block;
            if block_cursor {
                fg = term_bg;
            }
            (font_for_cell(cell.c, cell.flags), fg)
        };
        let mut visual_col = run.visual_start;
        while visual_col < run_end {
            let (font, fg) = glyph_style(visual_col);
            let mut span_end = visual_col + 1;
            while span_end < run_end && glyph_style(span_end) == (font, fg) {
                span_end += 1;
            }
            let clip = Rectangle::new(
                iced::Point::new(visual_col as f32 * cell_w, y),
                Size::new((span_end - visual_col) as f32 * cell_w, cell_h),
            );
            let natural_width = shaped_run_width(&content, font, font_size);
            let scale_x = if natural_width > 0.0 {
                (run.visual_width as f32 * cell_w) / natural_width
            } else {
                1.0
            };
            frame.with_clip(clip, |frame| {
                frame.with_save(|frame| {
                    frame.scale_nonuniform(iced::Vector::new(scale_x, 1.0));
                    frame.fill_text(iced::widget::canvas::Text {
                        content: content.clone(),
                        position: iced::Point::new(
                            if run.rtl { end_x / scale_x } else { start_x / scale_x },
                            y,
                        ),
                        color: fg,
                        size: iced::Pixels(font_size),
                        font,
                        horizontal_alignment: if run.rtl {
                            iced::alignment::Horizontal::Right
                        } else {
                            iced::alignment::Horizontal::Left
                        },
                        vertical_alignment: iced::alignment::Vertical::Top,
                        line_height: iced::widget::text::LineHeight::Relative(cell_h / font_size),
                        shaping: iced::widget::text::Shaping::Advanced,
                    });
                });
            });
            visual_col = span_end;
        }
    }
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
    let (fg, bg) =
        resolved_cell_colors(fg_color, bg_color, flags, colors, ansi, term_bg, term_fg);

    // Block cursor is a filled rect with an inverted glyph — the historical
    // behavior. Beam/Underline/HollowBlock draw the cell normally and
    // overlay a thin cursor mark instead of taking over the whole cell.
    let block_cursor = is_cursor && cursor_shape == CursorShape::Block;

    if is_selected {
        let sel_rect = Path::rectangle(iced::Point::new(x, y), Size::new(cell_w, cell_h));
        frame.fill(
            &sel_rect,
            IcedColor {
                r: 0.941,
                g: 0.753,
                b: 0.412,
                a: 0.35,
            },
        ); // #f0c069 amber, field notes
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
                frame.fill(
                    &Path::rectangle(iced::Point::new(x, y), Size::new(2.0, cell_h)),
                    cursor_color,
                );
            }
            CursorShape::Underline => {
                frame.fill(
                    &Path::rectangle(
                        iced::Point::new(x, y + cell_h - 2.0),
                        Size::new(cell_w, 2.0),
                    ),
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

        let font = font_for_cell(c, flags);
        let rendering = glyph_rendering(c);
        let text = iced::widget::canvas::Text {
            content: c.to_string(),
            position: iced::Point::new(x, y),
            color: glyph_fg,
            size: iced::Pixels(font_size),
            font,
            horizontal_alignment: iced::alignment::Horizontal::Left,
            vertical_alignment: iced::alignment::Vertical::Top,
            line_height: iced::widget::text::LineHeight::Relative(cell_h / font_size),
            shaping: iced::widget::text::Shaping::Basic,
        };
        if rendering == GlyphRendering::Basic {
            frame.fill_text(text);
        } else {
            let target_width = if flags.contains(Flags::WIDE_CHAR) {
                2.0 * cell_w
            } else {
                cell_w
            };
            draw_advanced_cell_glyph(frame, c, text, cell_w, target_width, cell_h);
        }
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
        stroke_line(
            frame,
            x + cell_w / 2.0,
            baseline - 2.0,
            x + cell_w,
            baseline,
            fg,
        );
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
pub fn word_at(
    grid: &alacritty_terminal::grid::Grid<alacritty_terminal::term::cell::Cell>,
    col: usize,
    row: usize,
) -> String {
    use alacritty_terminal::index::{Column, Line};

    let cols = grid.columns();
    let rows = grid.screen_lines();
    if row >= rows || col >= cols {
        return String::new();
    }

    let is_word = |c: char| c.is_alphanumeric() || c == '-';

    let mut start = col;
    while start > 0 {
        let c = grid[Line(row as i32)][Column(start - 1)].c;
        if !is_word(c) && c != '\0' {
            break;
        }
        start -= 1;
    }
    let mut end = col;
    while end + 1 < cols {
        let c = grid[Line(row as i32)][Column(end + 1)].c;
        if !is_word(c) && c != '\0' {
            break;
        }
        end += 1;
    }

    (start..=end)
        .map(|c| {
        let ch = grid[Line(row as i32)][Column(c)].c;
            if ch == '\0' {
                ' '
            } else {
                ch
            }
        })
        .collect::<String>()
        .trim()
        .to_string()
}

/// Extract the text under a viewport selection, mirroring the draw path's
/// row indexing exactly: when the view is scrolled back
/// (`state.scrollback.offset > 0`), rows above the live screen must read
/// from the cached tmux history rather than the live grid, or the
/// highlighted text and the copied clipboard text diverge.
pub fn extract_selection(
    state: &TerminalState,
    start_col: usize,
    start_row: usize,
    end_col: usize,
    end_row: usize,
) -> String {
    use alacritty_terminal::index::{Column, Line};

    let grid = state.term.grid();
    let cols = grid.columns();
    let rows = grid.screen_lines();
    let offset = state.scrollback.offset as i32;
    let mut out = String::new();

    for row in start_row..=end_row.min(rows.saturating_sub(1)) {
        let col_start = if row == start_row { start_col } else { 0 };
        let col_end = if row == end_row {
            end_col
        } else {
            cols.saturating_sub(1)
        };
        let col_end   = col_end.min(cols.saturating_sub(1));

        let logical = row as i32 - offset;
        let mut line_text = String::new();
        let wrapped;
        if logical < 0 {
            // History row — same index math as the draw path.
            if let Some(cells) = state.scrollback.line_above((-logical - 1) as usize) {
                wrapped = cells
                    .last()
                    .is_some_and(|cell| cell.flags.contains(Flags::WRAPLINE));
                for cell in cells.iter().skip(col_start).take(col_end + 1 - col_start) {
                    if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                        continue;
                    }
                    line_text.push(if cell.c == '\0' { ' ' } else { cell.c });
                    line_text.extend(cell.zerowidth.iter().copied());
                }
            } else {
                wrapped = false;
            }
        } else {
            let line = Line(logical);
            wrapped = grid[line][Column(cols.saturating_sub(1))]
                .flags
                .contains(Flags::WRAPLINE);
            for col in col_start..=col_end {
                let cell = &grid[line][Column(col)];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                line_text.push(if cell.c == '\0' { ' ' } else { cell.c });
                line_text.extend(cell.zerowidth().unwrap_or_default().iter().copied());
            }
        }
        // Strip trailing spaces from each line.
        let trimmed = line_text.trim_end();
        out.push_str(trimmed);
        if row < end_row && !wrapped {
            out.push('\n');
        }
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
    fn viewport_tail_hydrates_in_one_paint_without_emulator_history() {
        use alacritty_terminal::{
            grid::Dimensions,
            index::{Column, Line, Point},
        };
        let mut state = TerminalState::new(20, 4, None);
        let capture = ninox_core::tmux::ViewportTailCapture {
            start: 46,
            end: 49,
            pane_width: 20,
            pane_height: 50,
            cursor_x: 6,
            cursor_y: 3,
            bytes: b"tail-1\ntail-2\ntail-3\nprompt".to_vec(),
        };
        state.hydrate_viewport_tail(&capture);

        assert_eq!(state.output_commit_count(), 1);
        assert_eq!(state.term.grid().history_size(), 0);
        assert_eq!(state.term.grid().cursor.point, Point::new(Line(3), Column(6)));
        let rows = visible_styled_lines(&state.term);
        assert_eq!(rows[0][0].c, 't');
        assert_eq!(rows[3][0].c, 'p');
    }

    #[test]
    fn synchronized_fragments_only_advance_one_visible_frame() {
        let mut state = TerminalState::new(20, 3, None);
        state.process(b"stable");
        let stable_commits = state.output_commit_count();
        state.process(b"\x1b[?20");
        assert_eq!(state.output_commit_count(), stable_commits);
        state.process(b"26h\x1b[Hnew");
        assert_eq!(state.output_commit_count(), stable_commits);
        state.process(b" tail");
        assert_eq!(state.output_commit_count(), stable_commits);
        state.process(b"\x1b[?2026l");
        assert_eq!(state.output_commit_count(), stable_commits + 1);
    }

    #[test]
    fn separate_steady_sync_frames_remain_distinct_from_tail_hydration() {
        let mut state = TerminalState::new(20, 3, None);
        let capture = ninox_core::tmux::ViewportTailCapture {
            start: 0,
            end: 2,
            pane_width: 20,
            pane_height: 3,
            cursor_x: 0,
            cursor_y: 2,
            bytes: b"chat tail\n\nwaiting".to_vec(),
        };
        state.hydrate_viewport_tail(&capture);
        assert_eq!(state.output_commit_count(), 1);

        state.process(b"\x1b[?2026h\x1b[3;1H\x1b[2Kworking-1\x1b[?2026l");
        state.process(b"\x1b[?2026h\x1b[3;1H\x1b[2Kworking-2\x1b[?2026l");
        assert_eq!(
            state.output_commit_count(),
            3,
            "producer-delimited steady frames intentionally remain separate paints"
        );
    }

    #[test]
    fn live_output_preserves_scrolled_viewport_anchor() {
        let mut state = TerminalState::new(8, 3, None);
        state.process(b"one\r\ntwo\r\nthree");
        state.scrollback.absorb(vec![vec![]; 10], -10, false);
        state.scroll(2);
        let anchor = state.scrollback.line_above(state.scrollback.offset - 1).cloned();

        state.process(b"\r\nfour");

        assert_eq!(state.scrollback.offset, 3);
        assert_eq!(
            state.scrollback.line_above(state.scrollback.offset - 1),
            anchor.as_ref()
        );
        assert_eq!(state.scrollback.lines.len(), 11);
    }

    #[test]
    fn trackpad_pixel_deltas_accumulate_across_events() {
        let mut state = SelectionState::default();
        let delta = iced::mouse::ScrollDelta::Pixels { x: 0.0, y: 6.0 };
        assert_eq!(state.consume_scroll_delta(&delta, 18.0), 0);
        assert_eq!(state.consume_scroll_delta(&delta, 18.0), 0);
        assert_eq!(state.consume_scroll_delta(&delta, 18.0), 1);
        assert_eq!(state.scroll_pixel_remainder, 0.0);
    }

    #[test]
    fn trackpad_remainder_survives_rows_and_resets_on_reversal() {
        let mut state = SelectionState::default();
        let up = iced::mouse::ScrollDelta::Pixels { x: 0.0, y: 10.0 };
        assert_eq!(state.consume_scroll_delta(&up, 16.0), 0);
        assert_eq!(state.consume_scroll_delta(&up, 16.0), 1);
        assert_eq!(state.scroll_pixel_remainder, 4.0);

        let down = iced::mouse::ScrollDelta::Pixels { x: 0.0, y: -12.0 };
        assert_eq!(state.consume_scroll_delta(&down, 16.0), 0);
        assert_eq!(state.scroll_pixel_remainder, -12.0);
    }

    #[test]
    fn line_wheel_keeps_three_row_step() {
        let mut state = SelectionState::default();
        let delta = iced::mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 };
        assert_eq!(state.consume_scroll_delta(&delta, 18.0), 3);
    }

    #[test]
    fn wheel_targets_keep_sidebar_and_terminal_positions_isolated() {
        use iced::widget::canvas::Program;

        fn dispatch_wheel(
            terminal: &mut TerminalState,
            sidebar_offset: &mut usize,
            pointer: iced::Point,
            sidebar_bounds: Rectangle,
            terminal_bounds: Rectangle,
        ) -> iced::widget::canvas::event::Status {
            if sidebar_bounds.contains(pointer) {
                *sidebar_offset += 3;
            }

            let widget = test_widget(terminal);
            let mut canvas_state = SelectionState::default();
            let (status, message) = widget.update(
                &mut canvas_state,
                iced::widget::canvas::Event::Mouse(iced::mouse::Event::WheelScrolled {
                    delta: iced::mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 },
                }),
                terminal_bounds,
                iced::mouse::Cursor::Available(pointer),
            );

            if let Some(Message::ScrollTerminal { delta, .. }) = message {
                terminal.scroll(delta);
            }
            status
        }

        let sidebar_bounds =
            Rectangle::new(iced::Point::ORIGIN, Size::new(240.0, 600.0));
        let terminal_bounds =
            Rectangle::new(iced::Point::new(240.0, 0.0), Size::new(800.0, 600.0));
        let mut terminal = TerminalState::new(80, 24, None);
        terminal.scrollback.absorb(vec![vec![]; 100], -100, true);
        let mut sidebar_offset = 0;

        let sidebar_status = dispatch_wheel(
            &mut terminal,
            &mut sidebar_offset,
            iced::Point::new(120.0, 300.0),
            sidebar_bounds,
            terminal_bounds,
        );
        assert_eq!(sidebar_status, iced::widget::canvas::event::Status::Ignored);
        assert_eq!(sidebar_offset, 3);
        assert_eq!(
            terminal.scrollback.offset, 0,
            "sidebar-targeted wheel must not move terminal history"
        );

        let terminal_status = dispatch_wheel(
            &mut terminal,
            &mut sidebar_offset,
            iced::Point::new(640.0, 300.0),
            sidebar_bounds,
            terminal_bounds,
        );
        assert_eq!(terminal_status, iced::widget::canvas::event::Status::Captured);
        assert_eq!(
            sidebar_offset, 3,
            "terminal-targeted wheel must not move the sidebar"
        );
        assert_eq!(terminal.scrollback.offset, 3);
    }

    #[test]
    fn terminal_font_and_parser_preserve_common_unicode_punctuation() {
        let face = ttf_parser::Face::parse(TERM_FONT_BYTES, 0).unwrap();
        for character in ['\'', '’', '‘', '“', '”', '—', '→', '✓'] {
            assert!(
                face.glyph_index(character).is_some(),
                "terminal font is missing {character:?}"
            );
        }

        let mut state = TerminalState::new(80, 24, None);
        let sample = "→ don’t ‘lose’ “quotes” — ✓ \u{e0b0}\u{e0b2} ⠘⠣";
        state.process(sample.as_bytes());
        use alacritty_terminal::index::{Column, Line};
        let rendered: String = (0..sample.chars().count())
            .map(|column| state.term.grid()[Line(0)][Column(column)].c)
            .collect();
        assert_eq!(rendered, sample);
    }

    #[test]
    fn unicode_punctuation_uses_context_without_changing_fallback_policy() {
        assert!(term_font_has_glyph(CONTEXTUAL_FONT_ANCHOR));
        assert_eq!(glyph_rendering('\''), GlyphRendering::Basic);
        assert_eq!(glyph_rendering('\u{e0b0}'), GlyphRendering::Basic);
        for character in ['’', '‘', '“', '”', '—', '→', '✓'] {
            assert_eq!(glyph_rendering(character), GlyphRendering::Contextual);
        }
        for character in ['⠘', '⠣', '⠰', '⠳'] {
            assert_eq!(glyph_rendering(character), GlyphRendering::Fallback);
        }

        let mut state = TerminalState::new(20, 1, None);
        state.process("⠰⠳ Running".as_bytes());
        use alacritty_terminal::index::{Column, Line};
        assert_eq!(state.term.grid()[Line(0)][Column(0)].c, '⠰');
        assert_eq!(state.term.grid()[Line(0)][Column(1)].c, '⠳');
    }

    #[test]
    fn cold_font_system_keeps_contextual_glyph_at_cell_origin() {
        use iced::advanced::graphics::text::cosmic_text;

        let face = ttf_parser::Face::parse(TERM_FONT_BYTES, 0).unwrap();
        let anchor = face.glyph_index(CONTEXTUAL_FONT_ANCHOR).unwrap();
        assert_eq!(face.glyph_hor_advance(anchor), Some(0));
        assert!(face.glyph_bounding_box(anchor).is_none());

        let mut db = cosmic_text::fontdb::Database::new();
        db.load_font_data(TERM_FONT_BYTES.to_vec());
        let mut fonts = cosmic_text::FontSystem::new_with_locale_and_db("en-US".into(), db);
        let mut buffer =
            cosmic_text::Buffer::new(&mut fonts, cosmic_text::Metrics::new(13.0, 18.0));
        let content = format!("{CONTEXTUAL_FONT_ANCHOR}’");
        buffer.set_text(
            &mut fonts,
            &content,
            cosmic_text::Attrs::new()
                .family(cosmic_text::Family::Name("JetBrains Mono")),
            cosmic_text::Shaping::Advanced,
        );

        let visible = buffer
            .layout_runs()
            .next()
            .unwrap()
            .glyphs
            .iter()
            .find(|glyph| glyph.start >= CONTEXTUAL_FONT_ANCHOR.len_utf8())
            .unwrap();
        assert_eq!(visible.x, 0.0);
        assert_ne!(visible.glyph_id, 0);
    }

    #[test]
    fn cold_renderer_rasterizes_ltr_anchor_and_places_special_glyphs_at_cell_origin() {
        use iced::advanced::graphics::text::cosmic_text;

        let mut db = cosmic_text::fontdb::Database::new();
        db.load_font_data(TERM_FONT_BYTES.to_vec());
        let mut fonts = cosmic_text::FontSystem::new_with_locale_and_db("en-US".into(), db);
        let attrs =
            cosmic_text::Attrs::new().family(cosmic_text::Family::Name("JetBrains Mono"));
        let mut rasterizer = cosmic_text::SwashCache::new();
        let cell_width = cell_size(FONT_SIZE).0;

        for character in ['→', '’', '‘', '“', '”', '—', '✓'] {
            let mut buffer =
                cosmic_text::Buffer::new(&mut fonts, cosmic_text::Metrics::new(13.0, 18.0));
            let content = format!("{LTR_FONT_ANCHOR}{character}");
            buffer.set_text(&mut fonts, &content, attrs, cosmic_text::Shaping::Advanced);
            let glyphs = buffer.layout_runs().next().unwrap().glyphs;
            let anchor = glyphs[0].clone();
            let visible = glyphs
                .iter()
                .find(|glyph| glyph.start >= LTR_FONT_ANCHOR.len_utf8())
                .unwrap()
                .clone();

            let anchor_image = rasterizer
                .get_image(&mut fonts, anchor.physical((0.0, 0.0), 1.0).cache_key)
                .as_ref()
                .unwrap();
            assert!(anchor_image.placement.width > 0);
            assert!(anchor_image.placement.height > 0);
            assert!(!anchor_image.data.is_empty());
            assert!((visible.x - cell_width).abs() < 0.01);
            assert_ne!(visible.glyph_id, 0);
        }
    }

    #[test]
    fn cold_renderer_resolves_spinner_fallback_and_powerline_glyphs() {
        use iced::advanced::graphics::text::cosmic_text;

        let nerd_face = ttf_parser::Face::parse(
            include_bytes!("../../assets/fonts/SymbolsNerdFontMono-Regular.ttf"),
            0,
        )
        .unwrap();
        for character in ['\u{e0b0}', '\u{e0b2}'] {
            assert!(nerd_face.glyph_index(character).is_some());
        }

        let mut fonts = cosmic_text::FontSystem::new();
        fonts.db_mut().load_font_data(TERM_FONT_BYTES.to_vec());
        let attrs =
            cosmic_text::Attrs::new().family(cosmic_text::Family::Name("JetBrains Mono"));
        let mut rasterizer = cosmic_text::SwashCache::new();
        for character in ['⠘', '⠣'] {
            let mut buffer =
                cosmic_text::Buffer::new(&mut fonts, cosmic_text::Metrics::new(13.0, 18.0));
            let content = format!("{LTR_FONT_ANCHOR}{character}");
            buffer.set_text(&mut fonts, &content, attrs, cosmic_text::Shaping::Advanced);
            let visible = buffer
                .layout_runs()
                .next()
                .unwrap()
                .glyphs
                .iter()
                .find(|glyph| glyph.start >= LTR_FONT_ANCHOR.len_utf8())
                .unwrap()
                .clone();
            let image = rasterizer
                .get_image(&mut fonts, visible.physical((0.0, 0.0), 1.0).cache_key)
                .as_ref()
                .unwrap();
            assert!(image.placement.width > 0);
            assert!(image.placement.height > 0);
            assert!(!image.data.is_empty());
        }
    }

    fn shaped_glyphs_by_character(text: &str, per_cell: bool) -> Vec<u16> {
        use iced::advanced::graphics::text::cosmic_text;

        let mut fonts = cosmic_text::FontSystem::new();
        fonts.db_mut().load_font_data(TERM_FONT_BYTES.to_vec());
        let attrs =
            cosmic_text::Attrs::new().family(cosmic_text::Family::Name("JetBrains Mono"));

        if per_cell {
            return text
                .chars()
                .map(|character| {
                    let mut buffer =
                        cosmic_text::Buffer::new(&mut fonts, cosmic_text::Metrics::new(13.0, 18.0));
                    let content = format!("{CONTEXTUAL_FONT_ANCHOR}{character}");
                    buffer.set_text(
                        &mut fonts,
                        &content,
                        attrs,
                        cosmic_text::Shaping::Advanced,
                    );
                    buffer
                        .layout_runs()
                        .next()
                        .unwrap()
                        .glyphs
                        .iter()
                        .find(|glyph| glyph.start >= CONTEXTUAL_FONT_ANCHOR.len_utf8())
                        .unwrap()
                        .glyph_id
                })
                .collect();
        }

        let mut buffer =
            cosmic_text::Buffer::new(&mut fonts, cosmic_text::Metrics::new(13.0, 18.0));
        buffer.set_text(&mut fonts, text, attrs, cosmic_text::Shaping::Advanced);
        let glyphs = buffer.layout_runs().next().unwrap().glyphs;
        text.char_indices()
            .map(|(start, _)| {
                glyphs
                    .iter()
                    .find(|glyph| glyph.start <= start && start < glyph.end)
                    .unwrap()
                    .glyph_id
            })
            .collect()
    }

    #[test]
    fn fragmented_utf8_reaches_logical_terminal_cells_unchanged() {
        use alacritty_terminal::index::{Column, Line};

        let text = "مرحبا שלום";
        let mut state = TerminalState::new(20, 1, None);
        for fragment in text.as_bytes().chunks(2) {
            state.process(fragment);
        }

        let cells: String = (0..text.chars().count())
            .map(|column| state.term.grid()[Line(0)][Column(column)].c)
            .collect();
        assert_eq!(cells, text);
    }

    #[test]
    fn arabic_cells_are_contextually_shaped() {
        let text = "مرحبا";
        let isolated = shaped_glyphs_by_character(text, true);
        let contextual = shaped_glyphs_by_character(text, false);
        let layout = visual_layout(
            &text
                .chars()
                .map(|c| DisplayCell { c, ..DisplayCell::default() })
                .collect::<Vec<_>>(),
        );

        assert_ne!(isolated, contextual, "test must distinguish joining forms");
        assert_eq!(layout.runs.len(), 1);
        assert_eq!(layout.runs[0].text, text);
        assert_eq!(
            shaped_glyphs_by_character(&layout.runs[0].text, false),
            contextual
        );
    }

    #[test]
    fn proportional_rtl_fallback_is_normalized_to_terminal_cell_width() {
        let text = format!("{CONTEXTUAL_FONT_ANCHOR}مرحبا");
        let natural_width = shaped_run_width(&text, TERM_FONT, FONT_SIZE);
        let target_width = 5.0 * cell_size(FONT_SIZE).0;
        assert!(natural_width > 0.0);
        let scale = target_width / natural_width;
        assert!((natural_width * scale - target_width).abs() < f32::EPSILON);
    }

    #[test]
    fn mixed_direction_line_is_painted_in_unicode_visual_order() {
        use alacritty_terminal::index::{Column, Line};

        let logical = "English مرحبا 123, שלום!";
        let mut state = TerminalState::new(40, 1, None);
        state.process(logical.as_bytes());
        let currently_painted: String = (0..logical.chars().count())
            .map(|column| state.term.grid()[Line(0)][Column(column)].c)
            .collect();
        assert_eq!(
            currently_painted, logical,
            "the emulator grid must stay in logical order"
        );

        let bidi = unicode_bidi::BidiInfo::new(logical, None);
        let paragraph = &bidi.paragraphs[0];
        let expected = bidi.reorder_line(paragraph, paragraph.range.clone());
        let cells = (0..logical.chars().count())
            .map(|column| {
                let cell = &state.term.grid()[Line(0)][Column(column)];
                DisplayCell {
                    c:         cell.c,
                    zerowidth: cell.zerowidth().unwrap_or_default().to_vec(),
                    fg:        cell.fg,
                    bg:        cell.bg,
                    flags:     cell.flags,
                }
            })
            .collect::<Vec<_>>();
        let layout = visual_layout(&cells);
        let visually_painted: String = layout.visual_to_logical
            [..logical.chars().count()]
            .iter()
            .map(|&column| state.term.grid()[Line(0)][Column(column)].c)
            .collect();
        assert_eq!(
            visually_painted, expected,
            "the visual mapping must apply line-level BiDi"
        );
    }

    #[test]
    fn mixed_direction_cursor_and_hit_testing_map_back_to_logical_cells() {
        let logical = "abc مرحبا 123";
        let mut state = TerminalState::new(20, 2, None);
        state.process(logical.as_bytes());
        state.process(b"\x1b[1;6H");
        let widget = test_widget(&state);
        let cells = widget.row_display_cells(0).unwrap();
        let layout = visual_layout(&cells);
        let cursor_logical = state.term.grid().cursor.point.column.0;
        let cursor_visual = layout.logical_to_visual[cursor_logical];
        assert_eq!(layout.visual_to_logical[cursor_visual], cursor_logical);

        let (cell_w, cell_h) = cell_size(FONT_SIZE);
        for visual in 0..logical.chars().count() {
            let hit = widget.pixel_to_logical_cell(
                (visual as f32 + 0.5) * cell_w,
                cell_h / 2.0,
                cell_w,
                cell_h,
                20,
                2,
            );
            assert_eq!(hit, (layout.visual_to_logical[visual], 0));
        }
    }

    #[test]
    fn mixed_direction_drag_selects_visual_cells_and_copies_source_order() {
        use iced::mouse::Button;
        use iced::widget::canvas::Program;

        let mut state = TerminalState::new(20, 2, None);
        state.process("abc אבג xyz".as_bytes());
        let widget = test_widget(&state);
        let (cell_w, cell_h) = cell_size(FONT_SIZE);
        let bounds =
            Rectangle::new(iced::Point::ORIGIN, Size::new(20.0 * cell_w, 2.0 * cell_h));
        let mut selection = SelectionState::default();
        let start = iced::Point::new(3.5 * cell_w, 0.5 * cell_h);
        let end = iced::Point::new(4.5 * cell_w, 0.5 * cell_h);

        widget.update(
            &mut selection,
            iced::widget::canvas::Event::Mouse(iced::mouse::Event::ButtonPressed(Button::Left)),
            bounds,
            iced::mouse::Cursor::Available(start),
        );
        widget.update(
            &mut selection,
            iced::widget::canvas::Event::Mouse(iced::mouse::Event::CursorMoved { position: end }),
            bounds,
            iced::mouse::Cursor::Available(end),
        );

        let cells = widget.row_display_cells(0).unwrap();
        let layout = visual_layout(&cells);
        let highlighted: Vec<_> = layout
            .visual_to_logical
            .iter()
            .enumerate()
            .filter_map(|(visual, _)| {
                cell_is_selected(&selection, 0, visual).then_some(visual)
            })
            .collect();
        assert_eq!(highlighted, vec![3, 4]);
        assert!(matches!(
            widget.copy_message(&selection),
            Some(Message::CopyToClipboard(text)) if text == " ג"
        ));
    }

    #[test]
    fn selection_copy_stays_logical_and_preserves_combining_and_bidi_controls() {
        let logical = "A\u{2067}ש\u{05b8}לום\u{2069} 123";
        let mut state = TerminalState::new(30, 2, None);
        state.process(logical.as_bytes());

        let copied = extract_selection(&state, 0, 0, 10, 0);
        assert_eq!(copied, logical);
        assert!(copied.contains('\u{2067}'));
        assert!(copied.contains('\u{2069}'));
        assert!(copied.contains('\u{05b8}'));
    }

    #[test]
    fn latin_emoji_cjk_and_combining_clusters_keep_terminal_widths() {
        use alacritty_terminal::index::{Column, Line};

        let logical = "A e\u{301} 🙂 界";
        let mut state = TerminalState::new(20, 2, None);
        state.process(logical.as_bytes());
        let grid = state.term.grid();
        assert_eq!(grid[Line(0)][Column(2)].c, 'e');
        assert_eq!(
            grid[Line(0)][Column(2)].zerowidth().unwrap_or_default(),
            &['\u{301}']
        );
        assert!(grid[Line(0)][Column(4)].flags.contains(Flags::WIDE_CHAR));
        assert!(
            grid[Line(0)][Column(5)]
                .flags
                .contains(Flags::WIDE_CHAR_SPACER)
        );
        assert!(grid[Line(0)][Column(7)].flags.contains(Flags::WIDE_CHAR));
        assert!(
            grid[Line(0)][Column(8)]
                .flags
                .contains(Flags::WIDE_CHAR_SPACER)
        );

        let layout = visual_layout(&test_widget(&state).row_display_cells(0).unwrap());
        for logical in 0..20 {
            let visual = layout.logical_to_visual[logical];
            assert_eq!(layout.visual_to_logical[visual], logical);
        }
    }

    #[test]
    fn wraps_and_resize_reflow_recompute_visual_layout_from_logical_grid() {
        use alacritty_terminal::index::{Column, Line};

        let mut state = TerminalState::new(8, 3, None);
        state.process("abc مرحبا xyz".as_bytes());
        assert!(
            state.term.grid()[Line(0)][Column(7)]
                .flags
                .contains(Flags::WRAPLINE)
        );
        let before = visual_layout(&test_widget(&state).row_display_cells(0).unwrap());
        assert_ne!(before.visual_to_logical, (0..8).collect::<Vec<_>>());

        state.resize(12, 3);
        let widget = test_widget(&state);
        for row in 0..3 {
            let layout = visual_layout(&widget.row_display_cells(row).unwrap());
            for logical in 0..12 {
                let visual = layout.logical_to_visual[logical];
                assert_eq!(layout.visual_to_logical[visual], logical);
            }
        }
    }

    #[test]
    fn render_cursor_honors_tui_visibility_during_repaint() {
        let mut state = TerminalState::new(80, 24, None);
        assert_ne!(renderable_cursor(&state.term).shape, CursorShape::Hidden);

        state.process(b"\x1b[?25l");
        state.process(b"\x1b[5;10Hrepainting");
        let hidden = renderable_cursor(&state.term);
        assert_eq!(hidden.shape, CursorShape::Hidden);
        assert_eq!(hidden.point, state.term.grid().cursor.point);

        state.process(b"\x1b[?25h");
        let visible = renderable_cursor(&state.term);
        assert_ne!(visible.shape, CursorShape::Hidden);
        assert_eq!(visible.point, state.term.grid().cursor.point);
    }

    #[test]
    fn cursor_tui_sync_frame_hides_delayed_bottom_row_fragments() {
        use alacritty_terminal::index::{Column, Line};

        fn bottom_rows(state: &TerminalState) -> String {
            let grid = state.term.grid();
            (2..4)
                .map(|row| {
                    (0..grid.columns())
                        .map(|column| grid[Line(row)][Column(column)].c)
                        .collect::<String>()
                        .trim_end()
                        .to_string()
                })
                .collect::<Vec<_>>()
                .join("\n")
        }

        let mut state = TerminalState::new(40, 4, None);
        state.process("\x1b[3;1H⠹ Working…\x1b[4;1H❯ waiting".as_bytes());
        let stable = bottom_rows(&state);

        // Cursor TUI repaint split across independent PTY reads. The first
        // escape is itself fragmented, and later chunks model pauses longer
        // than the app's coalescing quiet window without wall-clock sleeps.
        for fragment in [
            b"\x1b[?20".as_slice(),
            b"26h\x1b[3;1H\x1b[2KRead",
            b"ing app.rs\x1b[4;1H\x1b[2K\xe2\x9d\xaf work",
        ] {
            state.process(fragment);
            assert_eq!(bottom_rows(&state), stable);
        }

        state.process(b"ing\x1b[?2026l");
        assert_eq!(bottom_rows(&state), "Reading app.rs\n❯ working");
    }

    #[test]
    fn tmux_split_sync_replay_does_not_commit_the_intermediate_clear() {
        use alacritty_terminal::index::{Column, Line};

        fn bottom_rows(state: &TerminalState) -> String {
            let grid = state.term.grid();
            (2..4)
                .map(|row| {
                    (0..grid.columns())
                        .map(|column| grid[Line(row)][Column(column)].c)
                        .collect::<String>()
                        .trim_end()
                        .to_string()
                })
                .collect::<Vec<_>>()
                .join("\n")
        }

        let mut state = TerminalState::new(40, 4, None);
        state.process("\x1b[3;1H⠹ Working…\x1b[4;1H❯ waiting\x1b[H".as_bytes());
        let stable = bottom_rows(&state);
        let stable_commits = state.output_commit_count();

        // Raw hidden-client reads captured from tmux 3.7b while an inner
        // synchronized frame crossed pane reads. tmux closes an outer frame
        // around the leaked erase, then emits a cursor-only frame before the
        // final repaint. Neither intermediate state is a provider frame.
        let clear = state.process(b"\x1b[?2026h\x1b[J\x1b[?2026l");
        assert!(!clear.committed);
        assert_eq!(bottom_rows(&state), stable);
        let cursor =
            state.process(b"\x1b[?2026h\x1b[?25l\x1b[?12l\x1b[?25h\x1b[4;7H\x1b[?2026l");
        assert!(!cursor.committed);
        assert_eq!(bottom_rows(&state), stable);
        assert_eq!(state.output_commit_count(), stable_commits);

        let final_frame = state.process(
            b"\x1b[?2026h\x1b[?25l\x1b[Hfirst rows\x1b[K\r\n\x1b[K\r\n\
              \x1b[K\r\nbottom-final\x1b[K\x1b[?12l\x1b[?25h\x1b[4;13H\x1b[?2026l",
        );
        assert!(final_frame.committed);
        assert_eq!(state.output_commit_count(), stable_commits + 1);
        assert_eq!(bottom_rows(&state), "\nbottom-final");
    }

    #[test]
    fn fragmented_balanced_sync_frame_has_one_renderer_commit() {
        use alacritty_terminal::index::{Column, Line};

        let mut state = TerminalState::new(20, 2, None);
        state.process(b"old");
        let before = state.output_commit_count();

        let first = state.process(b"\x1b[?20");
        assert!(!first.committed);
        assert!(first.recovery_token.is_some());
        let second = state.process(b"26h\x1b[H\x1b[2Knew");
        assert!(!second.committed);
        let third = state.process(b" value\x1b[?2026l");

        assert!(third.committed);
        assert_eq!(state.output_commit_count(), before + 1);
        let row: String = (0..20)
            .map(|column| state.term.grid()[Line(0)][Column(column)].c)
            .collect();
        assert_eq!(row.trim_end(), "new value");
    }

    #[test]
    fn missing_sync_terminator_recovers_as_one_atomic_commit() {
        use alacritty_terminal::index::{Column, Line};

        let mut state = TerminalState::new(20, 2, None);
        state.process(b"stable");
        let before = state.output_commit_count();
        let pending = state.process(b"\x1b[?2026h\x1b[H\x1b[2Krecovered");
        let token = pending.recovery_token.expect("open frame needs recovery");

        assert!(!pending.committed);
        assert_eq!(state.term.grid()[Line(0)][Column(0)].c, 's');
        assert!(state.recover_output(token));
        assert_eq!(state.output_commit_count(), before + 1);
        let row: String = (0..20)
            .map(|column| state.term.grid()[Line(0)][Column(column)].c)
            .collect();
        assert_eq!(row.trim_end(), "recovered");
        assert!(!state.recover_output(token), "recovery must be idempotent");
    }

    #[test]
    fn abandoned_partial_sync_marker_does_not_poison_later_unframed_output() {
        use alacritty_terminal::index::{Column, Line};

        let mut state = TerminalState::new(20, 2, None);
        let token = state
            .process(b"\x1b[?20")
            .recovery_token
            .expect("partial marker needs bounded recovery");

        assert!(!state.recover_output(token));
        let plain = state.process(b"plain");
        assert!(plain.committed);
        let row: String = (0..20)
            .map(|column| state.term.grid()[Line(0)][Column(column)].c)
            .collect();
        assert_eq!(row.trim_end(), "plain");
    }

    #[test]
    fn multiple_sync_frames_in_one_read_share_one_renderer_commit() {
        use alacritty_terminal::index::{Column, Line};

        let mut state = TerminalState::new(20, 2, None);
        let result = state.process(
            b"\x1b[?2026h\x1b[Hfirst\x1b[?2026l\
              \x1b[?2026h\x1b[H\x1b[2Ksecond\x1b[?2026l",
        );

        assert!(result.committed);
        assert_eq!(state.output_commit_count(), 1);
        let row: String = (0..20)
            .map(|column| state.term.grid()[Line(0)][Column(column)].c)
            .collect();
        assert_eq!(row.trim_end(), "second");
    }

    #[test]
    fn trailing_empty_tmux_sync_frame_does_not_reinvalidate_live_repaint() {
        let mut state = TerminalState::new(40, 4, None);
        state.process(b"stable");
        let before = state.output_commit_count();

        // Live tmux 3.7b output: Cursor's complete structural repaint arrives
        // unframed, then a separate balanced frame with no payload follows.
        let repaint = state.process(
            b"\x1b[2A\x1b[KRunning\r\n\x1b[Kstatus\r\n\x1b[Kprompt",
        );
        assert!(repaint.committed);
        assert_eq!(state.output_commit_count(), before + 1);

        let empty_frame = state.process(b"\x1b[?2026h\x1b[?2026l");
        assert!(!empty_frame.committed);
        assert_eq!(state.output_commit_count(), before + 1);
    }

    #[test]
    fn unframed_input_echo_and_logs_commit_without_sync_recovery_delay() {
        let mut state = TerminalState::new(40, 4, None);

        let echo = state.process(b"typed");
        assert!(echo.committed);
        assert!(echo.recovery_token.is_none());
        let logs = state.process(b"\r\nlog-1\r\nlog-2\r\n");
        assert!(logs.committed);
        assert!(logs.recovery_token.is_none());
        assert_eq!(state.output_commit_count(), 2);
    }

    #[test]
    fn one_unframed_read_with_rapid_bottom_row_replacements_commits_only_final_state() {
        use alacritty_terminal::index::{Column, Line};

        let mut state = TerminalState::new(20, 2, None);
        let result =
            state.process(b"\x1b[2;1Hpartial\rreplacement\rfinal\x1b[K");

        assert!(result.committed);
        assert_eq!(state.output_commit_count(), 1);
        let row: String = (0..20)
            .map(|column| state.term.grid()[Line(1)][Column(column)].c)
            .collect();
        assert_eq!(row.trim_end(), "final");
    }

    #[test]
    fn reconnect_state_does_not_inherit_pending_sync_recovery() {
        let mut old = TerminalState::new(20, 2, None);
        let token = old
            .process(b"\x1b[?2026h\x1b[Hpartial")
            .recovery_token
            .unwrap();

        let mut replacement = TerminalState::new(20, 2, None);
        let fresh = replacement.process(b"fresh");

        assert!(fresh.committed);
        assert!(!replacement.recover_output(token));
        assert_eq!(replacement.output_commit_count(), 1);
    }

    #[test]
    fn deferred_tmux_repaint_preserves_rtl_visual_mapping() {
        let mut state = TerminalState::new(20, 2, None);
        state.process("old עברית".as_bytes());
        let before = state.output_commit_count();
        state.process(b"\x1b[?2026h\x1b[H\x1b[J\x1b[?2026l");
        let final_frame =
            state.process("\x1b[?2026h\x1b[Hחדש שלום\x1b[?2026l".as_bytes());

        assert!(final_frame.committed);
        assert_eq!(state.output_commit_count(), before + 1);
        let cells = test_widget(&state).row_display_cells(0).unwrap();
        let layout = visual_layout(&cells);
        assert_ne!(
            &layout.visual_to_logical[..8],
            &(0..8).collect::<Vec<_>>()[..]
        );
    }

    #[test]
    fn destructive_only_sync_frame_has_bounded_atomic_recovery() {
        use alacritty_terminal::index::{Column, Line};

        let mut state = TerminalState::new(20, 2, None);
        state.process(b"clear me");
        let before = state.output_commit_count();
        let pending = state.process(b"\x1b[?2026h\x1b[H\x1b[J\x1b[?2026l");
        let token = pending.recovery_token.expect("destructive frame needs recovery");

        assert!(!pending.committed);
        assert_eq!(state.term.grid()[Line(0)][Column(0)].c, 'c');
        assert!(state.recover_output(token));
        assert_eq!(state.output_commit_count(), before + 1);
        assert_eq!(state.term.grid()[Line(0)][Column(0)].c, ' ');
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
        assert!(
            reply.starts_with(b"\x1b[?"),
            "unexpected kitty reply: {reply:?}"
        );
    }

    #[test]
    fn ansi_to_iced_uses_theme_palette_for_named_colors() {
        use alacritty_terminal::vte::ansi::{Color, NamedColor};
        let colors = alacritty_terminal::term::color::Colors::default();
        let mut ansi = [IcedColor::BLACK; 16];
        ansi[1] = IcedColor::from_rgb8(0x12, 0x34, 0x56); // themed "red"
        let out = ansi_to_iced(
            Color::Named(NamedColor::Red),
            &colors,
            &ansi,
            IcedColor::BLACK,
            IcedColor::WHITE,
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
            text.chars()
                .map(|c| StyledCell {
                c,
                zerowidth: Vec::new(),
                fg: Color::Named(NamedColor::Foreground),
                bg: Color::Named(NamedColor::Background),
                flags: Flags::empty(),
                hyperlink: None,
                })
                .collect()
        };
        // Oldest first — "history two" sits directly above the live screen.
        s.scrollback.absorb(
            vec![mk_line("history one"), mk_line("history two")],
            -2,
            true,
        );
        s.scrollback.offset = 2;

        // Top two viewport rows (0, 1) are both scrolled into history at
        // this offset — row 0 → history one, row 1 → history two.
        let text = extract_selection(&s, 0, 0, 19, 1);
        assert_eq!(text, "history one\nhistory two");
    }

    #[test]
    fn history_copy_does_not_insert_newline_at_soft_wrap() {
        let mut state = TerminalState::new(4, 3, None);
        let mut lines = crate::components::scrollback::parse_capture(b"abcdefgh", 4);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].last().unwrap().flags.contains(Flags::WRAPLINE));
        state.scrollback.absorb(std::mem::take(&mut lines), -2, true);
        state.scrollback.offset = 2;
        assert_eq!(extract_selection(&state, 0, 0, 3, 1), "abcdefgh");
    }

    #[test]
    fn scroll_to_bottom_resets_the_whole_scrollback_cache() {
        let mut s = TerminalState::new(80, 24, None);
        s.scrollback.absorb(vec![vec![]; 10], -10, true);
        s.scrollback.offset = 5;
        s.scrollback.fetch_pending = true;
        assert!(!s.scrollback.lines.is_empty());

        s.scroll_to_bottom();

        assert!(
            s.scrollback.lines.is_empty(),
            "cached history must be dropped"
        );
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

        assert!(
            s.scrollback.lines.is_empty(),
            "cached history parsed at the old width must be dropped"
        );
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
        assert!(
            (h - 13.0 * 1.4).abs() > 0.01,
            "height must be measured, not the 1.4 guess"
        );
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

    fn captured_cursor_footer_redraw(spinner: &str) -> Vec<u8> {
        let mut frame =
            b"\x1b[71;1H\x1b[7A\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[64;2H"
                .to_vec();
        frame.extend_from_slice(spinner.as_bytes());
        frame.extend_from_slice(b" Running  357.54k tokens\r\n ");
        frame.extend("▄".repeat(80).as_bytes());
        frame.extend_from_slice(b"\x1b[66;1H  \xe2\x86\x92 Add a follow-up");
        frame.extend_from_slice(b"\x1b[67;1H ");
        frame.extend("▀".repeat(80).as_bytes());
        frame.extend_from_slice(
            b"\x1b[68;1H  1 task\x1b[69;1H  GPT-5.6 Sol 272K High \xc2\xb7 MAX \xc2\xb7 71.7% \xc2\xb7 1 file edited",
        );
        frame.extend_from_slice(
            b"\x1b[70;1H  ~/dev/ninox-w1 \xc2\xb7 mu/ninox/terminal-flicker-visual-root-cause",
        );
        frame
    }

    #[test]
    fn captured_cursor_spinner_changes_only_its_row_draw_commands() {
        let mut state = TerminalState::new(157, 71, None);
        state.process(&captured_cursor_footer_redraw("⠠⠜"));
        let selection = SelectionState::default();
        let before = test_widget(&state).row_draw_commands(&selection);

        state.process(&captured_cursor_footer_redraw("⠰⠰"));
        let after = test_widget(&state).row_draw_commands(&selection);
        let changed: Vec<_> = before
            .iter()
            .zip(&after)
            .enumerate()
            .filter_map(|(row, (before, after))| (before != after).then_some(row))
            .collect();

        assert_eq!(
            changed,
            [63],
            "unchanged footer rows must retain their draw commands"
        );
    }

    #[test]
    fn unchanged_grid_has_stable_row_draw_commands() {
        let mut state = TerminalState::new(80, 24, None);
        state.process(b"static shell output\r\nunchanged");
        let selection = SelectionState::default();

        let first = test_widget(&state).row_draw_commands(&selection);
        let second = test_widget(&state).row_draw_commands(&selection);

        assert_eq!(first, second);
    }

    #[test]
    fn simple_shell_output_changes_only_its_repainted_row() {
        let mut state = TerminalState::new(80, 24, None);
        state.process(b"before");
        let selection = SelectionState::default();
        let before = test_widget(&state).row_draw_commands(&selection);

        state.process(b"\rbash: after");
        let after = test_widget(&state).row_draw_commands(&selection);
        let changed: Vec<_> = before
            .iter()
            .zip(&after)
            .enumerate()
            .filter_map(|(row, (before, after))| (before != after).then_some(row))
            .collect();

        assert_eq!(changed, [0]);
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
        assert_eq!(widget.link_at_logical(0, 0), None); // inside "see "
        assert_eq!(
            widget.link_at_logical(5, 0),
            Some("http://example.com/path".to_string())
        );
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
            iced::widget::canvas::Event::Mouse(iced::mouse::Event::CursorMoved {
                position: hover_pos,
            }),
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
            iced::widget::canvas::Event::Mouse(iced::mouse::Event::CursorMoved {
                position: no_link_pos,
            }),
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
    fn key_pressed(
        key: iced::keyboard::Key,
        modifiers: iced::keyboard::Modifiers,
    ) -> iced::widget::canvas::Event {
        iced::widget::canvas::Event::Keyboard(iced::keyboard::Event::KeyPressed {
            key: key.clone(),
            modified_key: key,
            physical_key: iced::keyboard::key::Physical::Unidentified(
                iced::keyboard::key::NativeCode::Unidentified,
            ),
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
            key_pressed(
                iced::keyboard::Key::Character("c".into()),
                iced::keyboard::Modifiers::LOGO,
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
    fn cmd_c_with_no_selection_is_swallowed_not_typed() {
        use iced::widget::canvas::Program;

        let s = TerminalState::new(80, 5, None);
        let widget = test_widget(&s);
        let bounds = Rectangle::new(iced::Point::ORIGIN, Size::new(800.0, 100.0));
        let mut state = SelectionState::default();

        let (status, message) = widget.update(
            &mut state,
            key_pressed(
                iced::keyboard::Key::Character("c".into()),
                iced::keyboard::Modifiers::LOGO,
            ),
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
            key_pressed(
                iced::keyboard::Key::Character("c".into()),
                iced::keyboard::Modifiers::CTRL,
            ),
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
        let bounds = Rectangle::new(
            iced::Point::new(0.0, 100.0),
            Size::new(80.0 * cell_w, 24.0 * cell_h),
        );
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
        assert_eq!(
            state.end,
            Some((3, 0)),
            "selection should extend to the top row"
        );
        assert!(state.moved);
        match message {
            Some(Message::ScrollTerminal { delta, local, .. }) => {
                assert!(delta > 0, "should scroll up into history");
                assert!(
                    local,
                    "drag auto-scroll must target local scrollback, not the PTY"
                );
            }
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
        let bounds = Rectangle::new(
            iced::Point::new(0.0, 100.0),
            Size::new(80.0 * cell_w, 24.0 * cell_h),
        );
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
        assert_eq!(
            state.end,
            Some((3, 23)),
            "selection should extend to the bottom row"
        );
        assert!(state.moved);
        match message {
            Some(Message::ScrollTerminal { delta, local, .. }) => {
                assert!(delta < 0, "should scroll down toward live");
                assert!(
                    local,
                    "drag auto-scroll must target local scrollback, not the PTY"
                );
            }
            other => panic!("expected ScrollTerminal, got {other:?}"),
        }
    }

    #[test]
    fn drag_autoscroll_stays_local_even_when_pty_mouse_mode_is_on() {
        use iced::mouse::Button;
        use iced::widget::canvas::Program;

        let mut s = TerminalState::new(80, 24, None);
        s.process(b"\x1b[?1000h"); // inner app enables mouse reporting (vim mouse=a, htop, ...)
        // In this mode a real wheel event WOULD be encoded for the PTY —
        // exactly the routing drag auto-scroll must bypass.
        assert!(
            crate::input::encode_wheel(3, 0, 0, s.term.mode()).is_some(),
            "precondition: mouse mode routes wheel events to the PTY"
        );
        let widget = test_widget(&s);
        let (cell_w, cell_h) = cell_size(FONT_SIZE);
        let bounds = Rectangle::new(
            iced::Point::new(0.0, 100.0),
            Size::new(80.0 * cell_w, 24.0 * cell_h),
        );
        let mut state = SelectionState::default();

        let start = iced::Point::new(cell_w * 2.0, bounds.y + cell_h * 2.0);
        widget.update(
            &mut state,
            iced::widget::canvas::Event::Mouse(iced::mouse::Event::ButtonPressed(Button::Left)),
            bounds,
            iced::mouse::Cursor::Available(start),
        );

        let above = iced::Point::new(cell_w * 3.0, bounds.y - 20.0);
        let (_, message) = widget.update(
            &mut state,
            iced::widget::canvas::Event::Mouse(iced::mouse::Event::CursorMoved { position: above }),
            bounds,
            iced::mouse::Cursor::Available(above),
        );
        match message {
            Some(Message::ScrollTerminal { local: true, .. }) => {}
            other => panic!("expected a local ScrollTerminal, got {other:?}"),
        }

        // A real wheel event keeps PTY-aware routing (local: false).
        let inside = iced::Point::new(cell_w * 3.0, bounds.y + cell_h * 3.0);
        let (_, message) = widget.update(
            &mut state,
            iced::widget::canvas::Event::Mouse(iced::mouse::Event::WheelScrolled {
                delta: iced::mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 },
            }),
            bounds,
            iced::mouse::Cursor::Available(inside),
        );
        match message {
            Some(Message::ScrollTerminal { local: false, .. }) => {}
            other => panic!("expected a PTY-aware ScrollTerminal, got {other:?}"),
        }
    }
}
