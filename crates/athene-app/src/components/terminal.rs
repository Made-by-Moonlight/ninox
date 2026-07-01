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
// TerminalWidget — iced canvas Program
// ---------------------------------------------------------------------------

/// A view of a `TerminalState` that can be used as an iced Canvas widget.
pub struct TerminalWidget<'a> {
    pub state: &'a TerminalState,
    pub font_size: f32,
    pub terminal_bg: IcedColor,
    pub terminal_fg: IcedColor,
    pub cursor_color: IcedColor,
}

impl<'a> iced::widget::canvas::Program<Message> for TerminalWidget<'a> {
    type State = ();

    fn update(
        &self,
        _state: &mut Self::State,
        event: iced::widget::canvas::Event,
        _bounds: Rectangle,
        _cursor: iced::mouse::Cursor,
    ) -> (iced::widget::canvas::event::Status, Option<Message>) {
        use iced::keyboard::Event as KeyEvent;
        use iced::widget::canvas::Event;

        // Emit RawKey so the handler can apply APP_CURSOR-aware conversion.
        let Event::Keyboard(KeyEvent::KeyPressed { key, modifiers, text, .. }) = event else {
            return (iced::widget::canvas::event::Status::Ignored, None);
        };
        let msg = Message::RawKey {
            key,
            modifiers,
            text: text.map(|t| t.as_str().to_string()),
        };
        (iced::widget::canvas::event::Status::Captured, Some(msg))
    }

    fn draw(
        &self,
        _state: &Self::State,
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

                    if is_cursor {
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
