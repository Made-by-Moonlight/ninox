//! Turning [`ScreenSnapshot`]s and [`Line`]s back into bytes: ANSI repaints
//! for raw subscribers, plain text for `ninox read`, and `capture-pane -e`
//! style history.

use crate::protocol::*;

impl ScreenSnapshot {
    /// Full repaint of the visible screen as ANSI: clear, every visible line
    /// with SGR styles, cursor position/visibility. Scrollback is not emitted.
    ///
    /// The repaint is wrapped in a DEC 2026 synchronized update so terminals
    /// that support it show it atomically. Input-encoding modes (bracketed
    /// paste, application cursor keys, mouse reporting) are re-asserted so a
    /// freshly attached terminal encodes the user's input the way the
    /// application expects; the alternate screen is deliberately not entered
    /// (the attach bridge owns the outer terminal's screen choice).
    pub fn to_ansi(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.rows as usize * (self.cols as usize + 16));
        out.extend_from_slice(b"\x1b[?2026h\x1b[?25l\x1b[0m\x1b[H\x1b[2J");
        for (row, line) in self.visible_lines().iter().enumerate() {
            if line.runs.is_empty() {
                continue;
            }
            out.extend_from_slice(format!("\x1b[{};1H", row + 1).as_bytes());
            write_line_sgr(&mut out, line);
        }
        let m = self.modes;
        let set = |out: &mut Vec<u8>, on: bool, code: &str| {
            out.extend_from_slice(format!("\x1b[?{code}{}", if on { 'h' } else { 'l' }).as_bytes());
        };
        set(&mut out, m.app_cursor, "1");
        set(&mut out, m.bracketed_paste, "2004");
        if m.mouse_reporting {
            // Which of 1000/1002/1003 is not tracked separately; 1002
            // (button-event tracking) is the common choice of agent TUIs.
            out.extend_from_slice(b"\x1b[?1002h");
        } else {
            out.extend_from_slice(b"\x1b[?1000l\x1b[?1002l\x1b[?1003l");
        }
        set(&mut out, m.sgr_mouse, "1006");
        let row = self.cursor.row.min(self.rows.saturating_sub(1)) + 1;
        let col = self.cursor.col.min(self.cols.saturating_sub(1)) + 1;
        out.extend_from_slice(format!("\x1b[{row};{col}H").as_bytes());
        if self.cursor.visible {
            out.extend_from_slice(b"\x1b[?25h");
        }
        out.extend_from_slice(b"\x1b[?2026l");
        out
    }

    /// Visible screen (plus included scrollback) as plain text, one `\n` per
    /// line, trailing blanks trimmed. For `ninox read` and prompt detection.
    pub fn to_plain_text(&self) -> String {
        let mut out = String::new();
        for line in &self.lines {
            out.push_str(line_plain_text(line).trim_end());
            out.push('\n');
        }
        out
    }

    /// The last `rows` lines (the visible screen).
    pub fn visible_lines(&self) -> &[Line] {
        let start = self.scrollback_len.min(self.lines.len());
        &self.lines[start..]
    }
}

pub fn line_plain_text(line: &Line) -> String {
    line.runs.iter().map(|r| r.text.as_str()).collect()
}

/// One line with SGR styling and no line terminator. Ends with an SGR reset
/// when any style was active so the next line starts clean.
pub fn write_line_sgr(out: &mut Vec<u8>, line: &Line) {
    let mut current = Style::default();
    for run in &line.runs {
        if run.style != current {
            write_sgr(out, &run.style);
            current = run.style;
        }
        out.extend_from_slice(run.text.as_bytes());
    }
    if current != Style::default() {
        out.extend_from_slice(b"\x1b[0m");
    }
}

/// Full (reset-based) SGR for `style`; simpler and more robust than diffing.
pub fn write_sgr(out: &mut Vec<u8>, style: &Style) {
    let mut s = String::from("\x1b[0");
    if style.bold {
        s.push_str(";1");
    }
    if style.dim {
        s.push_str(";2");
    }
    if style.italic {
        s.push_str(";3");
    }
    if style.underline {
        s.push_str(";4");
    }
    if style.inverse {
        s.push_str(";7");
    }
    if style.strikethrough {
        s.push_str(";9");
    }
    push_color(&mut s, style.fg, 30, 90, 38);
    push_color(&mut s, style.bg, 40, 100, 48);
    s.push('m');
    out.extend_from_slice(s.as_bytes());
}

fn push_color(s: &mut String, color: Color, base: u8, bright: u8, extended: u8) {
    match color {
        Color::Default => {}
        Color::Indexed(n) if n < 8 => s.push_str(&format!(";{}", base + n)),
        Color::Indexed(n) if n < 16 => s.push_str(&format!(";{}", bright + n - 8)),
        Color::Indexed(n) => s.push_str(&format!(";{extended};5;{n}")),
        Color::Rgb(r, g, b) => s.push_str(&format!(";{extended};2;{r};{g};{b}")),
    }
}

/// `capture-pane -e` style rendering: every line followed by `\n`.
pub fn lines_to_capture(lines: &[Line]) -> Vec<u8> {
    let mut out = Vec::new();
    for line in lines {
        write_line_sgr(&mut out, line);
        out.push(b'\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(lines: Vec<Line>, scrollback_len: usize) -> ScreenSnapshot {
        ScreenSnapshot {
            cols: 10,
            rows: (lines.len() - scrollback_len) as u16,
            seq: 1,
            lines,
            scrollback_len,
            cursor: Cursor { row: 1, col: 2, visible: true },
            modes: Modes { bracketed_paste: true, ..Default::default() },
            title: None,
        }
    }

    fn line(text: &str, style: Style) -> Line {
        Line { runs: vec![Run { text: text.into(), style }], wrapped: false }
    }

    #[test]
    fn plain_text_trims_and_keeps_line_count() {
        let s = snap(vec![line("old  ", Style::default()), line("a b  ", Style::default()), Line::default()], 1);
        assert_eq!(s.to_plain_text(), "old\na b\n\n");
        assert_eq!(s.visible_lines().len(), 2);
    }

    #[test]
    fn ansi_repaint_has_styles_modes_and_cursor() {
        let red = Style { fg: Color::Indexed(1), bold: true, ..Default::default() };
        let rgb = Style { bg: Color::Rgb(1, 2, 3), ..Default::default() };
        let s = snap(vec![line("hi", red), line("x", rgb)], 0);
        let a = String::from_utf8(s.to_ansi()).unwrap();
        assert!(a.contains("\x1b[1;1H\x1b[0;1;31mhi\x1b[0m"), "{a:?}");
        assert!(a.contains("\x1b[2;1H\x1b[0;48;2;1;2;3mx\x1b[0m"), "{a:?}");
        assert!(a.contains("\x1b[?2004h"));
        assert!(a.ends_with("\x1b[2;3H\x1b[?25h\x1b[?2026l"), "{a:?}");
    }

    #[test]
    fn sgr_color_ranges() {
        let mut out = Vec::new();
        write_sgr(&mut out, &Style { fg: Color::Indexed(9), bg: Color::Indexed(200), ..Default::default() });
        assert_eq!(out, b"\x1b[0;91;48;5;200m");
    }
}
