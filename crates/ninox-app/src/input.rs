//! Keyboard/mouse/paste → terminal byte encoding, honoring the modes the
//! inner application negotiated (read from the live alacritty Term).
//!
//! Modified functional keys are always emitted in kitty CSI-u form: ninox
//! only ever talks to its own tmux server (extended-keys always), which
//! forwards them as CSI-u to every application, whether or not it
//! negotiated extended keys — `extended-keys always` never downgrades,
//! it always forwards. Legacy default-socket sessions may not understand
//! CSI-u; they degrade exactly as they did before this feature existed.

use alacritty_terminal::term::TermMode;
use iced::keyboard::{key::Named, Key, Modifiers};

/// Toolkit-neutral key identity, so the Iced canvas and the ratatui TUI
/// share one byte encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyInput {
    Char(char),
    Enter,
    Escape,
    Backspace,
    Delete,
    Insert,
    Tab,
    /// Shift+Tab as reported by toolkits that fold it into its own key.
    BackTab,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    F(u8),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Mods {
    pub shift: bool,
    pub alt:   bool,
    pub ctrl:  bool,
    pub logo:  bool,
}

impl Mods {
    pub const NONE: Self = Self { shift: false, alt: false, ctrl: false, logo: false };

    /// xterm/kitty modifier parameter: 1 + bitfield(shift=1, alt=2, ctrl=4, super=8).
    fn param(self) -> u32 {
        1 + (self.shift as u32)
            + ((self.alt as u32) << 1)
            + ((self.ctrl as u32) << 2)
            + ((self.logo as u32) << 3)
    }
}

/// How modified keys are spelled for the receiving application.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum KeyProtocol {
    /// Plain xterm encoding for apps that never asked for more. Modified
    /// Enter degrades to the spellings agent TUIs accept as "newline"
    /// (ESC CR for Shift/Alt, LF for Ctrl).
    #[default]
    Legacy,
    /// Modified Enter/Escape/Backspace/Tab always as kitty CSI-u, everything
    /// else legacy: what ninox's private tmux server (`extended-keys
    /// always`) forwards to every application.
    TmuxExtended,
    /// The application pushed kitty keyboard flags (disambiguate): Escape
    /// and every Ctrl/Alt-modified key are CSI-u too.
    Kitty,
}

/// The input-relevant modes of the receiving terminal.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputModes {
    pub app_cursor:      bool,
    pub bracketed_paste: bool,
    pub mouse_reporting: bool,
    pub sgr_mouse:       bool,
    pub protocol:        KeyProtocol,
}

impl InputModes {
    pub fn from_term_mode(mode: &TermMode) -> Self {
        Self {
            app_cursor:      mode.contains(TermMode::APP_CURSOR),
            bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
            mouse_reporting: mode.intersects(TermMode::MOUSE_MODE),
            sgr_mouse:       mode.contains(TermMode::SGR_MOUSE),
            protocol:        KeyProtocol::TmuxExtended,
        }
    }
}

/// kitty CSI-u codepoint for functional keys that need disambiguation.
fn functional_code(key: KeyInput) -> Option<u32> {
    Some(match key {
        KeyInput::Enter     => 13,
        KeyInput::Escape    => 27,
        KeyInput::Backspace => 127,
        KeyInput::Tab       => 9,
        _ => return None,
    })
}

fn ctrl_byte(ch: char) -> Option<u8> {
    Some(match ch {
        'a'..='z' => ch as u8 - b'a' + 1,
        'A'..='Z' => ch as u8 - b'A' + 1,
        ' ' | '@' | '2' => 0x00,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' => 0x1e,
        '_' | '/' | '7' => 0x1f,
        '8' | '?' => 0x7f,
        _ => return None,
    })
}

/// Bytes for one key press. `text` is the toolkit's shift-resolved text,
/// preferred over the bare character for unmodified/shifted input.
pub fn encode(key: KeyInput, m: Mods, text: Option<&str>, modes: &InputModes) -> Option<Vec<u8>> {
    let mods = m.param();
    let kitty = modes.protocol == KeyProtocol::Kitty;

    // Shift+Tab keeps its classic backtab spelling outside kitty mode
    // (universally understood; CSI-u tab is not).
    let backtab = matches!(key, KeyInput::BackTab) || (matches!(key, KeyInput::Tab) && m == Mods { shift: true, ..Mods::NONE });
    if backtab && !kitty {
        return Some(b"\x1b[Z".to_vec());
    }
    let key = if matches!(key, KeyInput::BackTab) { KeyInput::Tab } else { key };
    let mods = if backtab { m.param().max(2) } else { mods };

    if let Some(code) = functional_code(key) {
        let csi_u = match modes.protocol {
            KeyProtocol::Kitty => mods > 1 || key == KeyInput::Escape,
            KeyProtocol::TmuxExtended => mods > 1,
            KeyProtocol::Legacy => false,
        };
        if csi_u {
            return Some(if mods > 1 {
                format!("\x1b[{code};{mods}u").into_bytes()
            } else {
                format!("\x1b[{code}u").into_bytes()
            });
        }
    }

    if let KeyInput::Char(ch) = key {
        if kitty && (m.ctrl || m.alt) {
            // kitty reports the unshifted key; shift lives in the modifier.
            let base = ch.to_lowercase().next().unwrap_or(ch);
            return Some(format!("\x1b[{};{mods}u", base as u32).into_bytes());
        }
        if m.ctrl {
            if let Some(b) = ctrl_byte(ch) {
                let mut v = Vec::with_capacity(2);
                if m.alt {
                    v.push(0x1b);
                }
                v.push(b);
                return Some(v);
            }
        }
        let mut base = text
            .filter(|t| !t.is_empty())
            .map(|t| t.as_bytes().to_vec())
            .unwrap_or_else(|| ch.to_string().into_bytes());
        if m.alt {
            base.insert(0, 0x1b);
        }
        return Some(base);
    }

    // Cursor-ish keys: modified → CSI 1;<mods><final>; plain → mode-sensitive.
    let cursor = |fin: char| -> Vec<u8> {
        if mods > 1 {
            format!("\x1b[1;{mods}{fin}").into_bytes()
        } else if modes.app_cursor {
            format!("\x1bO{fin}").into_bytes()
        } else {
            format!("\x1b[{fin}").into_bytes()
        }
    };
    let tilde = |n: u8| -> Vec<u8> {
        if mods > 1 {
            format!("\x1b[{n};{mods}~").into_bytes()
        } else {
            format!("\x1b[{n}~").into_bytes()
        }
    };

    let bytes = match key {
        KeyInput::Enter => match (m.ctrl, m.shift || m.alt) {
            (true, _) => b"\n".to_vec(),
            (false, true) => b"\x1b\r".to_vec(),
            (false, false) => b"\r".to_vec(),
        },
        KeyInput::Escape => b"\x1b".to_vec(),
        KeyInput::Backspace => match (m.ctrl, m.alt) {
            (true, _) => vec![0x08],
            (false, true) => b"\x1b\x7f".to_vec(),
            (false, false) => vec![0x7f],
        },
        KeyInput::Tab => {
            if m.alt { b"\x1b\t".to_vec() } else { b"\t".to_vec() }
        }
        KeyInput::Up => cursor('A'),
        KeyInput::Down => cursor('B'),
        KeyInput::Right => cursor('C'),
        KeyInput::Left => cursor('D'),
        KeyInput::Home => cursor('H'),
        KeyInput::End => cursor('F'),
        KeyInput::Insert => tilde(2),
        KeyInput::Delete => tilde(3),
        KeyInput::PageUp => tilde(5),
        KeyInput::PageDown => tilde(6),
        KeyInput::F(n @ 1..=4) => {
            let fin = (b'P' + n - 1) as char;
            if mods > 1 {
                format!("\x1b[1;{mods}{fin}").into_bytes()
            } else {
                format!("\x1bO{fin}").into_bytes()
            }
        }
        KeyInput::F(n @ 5..=12) => tilde([15, 17, 18, 19, 20, 21, 23, 24][(n - 5) as usize]),
        KeyInput::F(_) | KeyInput::Char(_) | KeyInput::BackTab => return None,
    };
    Some(bytes)
}

fn iced_key_input(key: &Key) -> Option<KeyInput> {
    Some(match key {
        Key::Named(Named::Enter)      => KeyInput::Enter,
        Key::Named(Named::Escape)     => KeyInput::Escape,
        Key::Named(Named::Backspace)  => KeyInput::Backspace,
        Key::Named(Named::Delete)     => KeyInput::Delete,
        Key::Named(Named::Tab)        => KeyInput::Tab,
        Key::Named(Named::ArrowUp)    => KeyInput::Up,
        Key::Named(Named::ArrowDown)  => KeyInput::Down,
        Key::Named(Named::ArrowRight) => KeyInput::Right,
        Key::Named(Named::ArrowLeft)  => KeyInput::Left,
        Key::Named(Named::Home)       => KeyInput::Home,
        Key::Named(Named::End)        => KeyInput::End,
        Key::Named(Named::PageUp)     => KeyInput::PageUp,
        Key::Named(Named::PageDown)   => KeyInput::PageDown,
        Key::Character(c) => KeyInput::Char(c.chars().next()?),
        _ => return None,
    })
}

pub fn encode_key(
    key:       &Key,
    modifiers: Modifiers,
    text:      Option<&str>,
    mode:      &TermMode,
) -> Option<Vec<u8>> {
    let m = Mods {
        shift: modifiers.shift(),
        alt:   modifiers.alt(),
        ctrl:  modifiers.control(),
        logo:  modifiers.logo(),
    };
    let modes = InputModes::from_term_mode(mode);
    match iced_key_input(key) {
        // Plain Enter keeps CR under tmux: only *modified* Enter is CSI-u
        // there, and the legacy ESC-CR/LF fallbacks never apply.
        Some(k) => encode(k, m, text, &modes),
        None => text.filter(|t| !t.is_empty()).map(|t| t.as_bytes().to_vec()),
    }
}

/// Wrap pasted text in bracketed-paste markers when the app asked for them.
pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        let mut v = b"\x1b[200~".to_vec();
        v.extend(text.as_bytes());
        v.extend_from_slice(b"\x1b[201~");
        v
    } else {
        text.as_bytes().to_vec()
    }
}

pub fn encode_paste(text: &str, mode: &TermMode) -> Vec<u8> {
    paste_bytes(text, mode.contains(TermMode::BRACKETED_PASTE))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseAction {
    Press,
    Release,
    Drag,
}

/// Encode a mouse event for an application with mouse reporting on, or
/// `None` when it has none (the event is ninox's to handle). col/row are
/// 0-based cells relative to the pane.
pub fn encode_mouse(
    button: MouseButton,
    action: MouseAction,
    m: Mods,
    col: u16,
    row: u16,
    modes: &InputModes,
) -> Option<Vec<u8>> {
    if !modes.mouse_reporting {
        return None;
    }
    let mut code: u32 = match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        MouseButton::WheelUp => 64,
        MouseButton::WheelDown => 65,
    };
    if action == MouseAction::Drag {
        code += 32;
    }
    code += (m.shift as u32) * 4 + (m.alt as u32) * 8 + (m.ctrl as u32) * 16;
    let (x, y) = (col as u32 + 1, row as u32 + 1);
    if modes.sgr_mouse {
        let fin = if action == MouseAction::Release { 'm' } else { 'M' };
        return Some(format!("\x1b[<{code};{x};{y}{fin}").into_bytes());
    }
    // X10/normal encoding: release has no button identity, and coordinates
    // past 223 cannot be represented.
    if x > 223 || y > 223 {
        return None;
    }
    let code = if action == MouseAction::Release { 3 + (code & !3) } else { code };
    Some(vec![0x1b, b'[', b'M', (32 + code) as u8, (32 + x) as u8, (32 + y) as u8])
}

/// SGR-encode a wheel event for the inner app, or None if ninox's own
/// scrollback should consume the wheel. col/row are 0-based cells.
pub fn encode_wheel(lines_up: i32, col: usize, row: usize, mode: &TermMode) -> Option<Vec<u8>> {
    if !mode.intersects(TermMode::MOUSE_MODE) {
        return None;
    }
    let button = if lines_up > 0 { 64 } else { 65 };
    Some(format!("\x1b[<{button};{};{}M", col + 1, row + 1).into_bytes())
}

/// Bytes to send the PTY for a `ScrollTerminal` message, or None if the
/// scroll should act on ninox's own scrollback. A `local` scroll
/// (drag-select auto-scroll) must never reach the PTY — under a
/// mouse-mode TUI the wheel bytes would scroll the inner app instead of
/// extending the selection into local history.
pub fn scroll_pty_bytes(local: bool, lines_up: i32, mode: &TermMode) -> Option<Vec<u8>> {
    if local { None } else { encode_wheel(lines_up, 0, 0, mode) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::term::TermMode;
    use iced::keyboard::{key::Named, Key, Modifiers};

    fn enc(key: Key, m: Modifiers, text: Option<&str>, mode: TermMode) -> Option<Vec<u8>> {
        encode_key(&key, m, text, &mode)
    }

    #[test]
    fn plain_enter_is_cr() {
        assert_eq!(enc(Key::Named(Named::Enter), Modifiers::empty(), None, TermMode::empty()),
                   Some(b"\r".to_vec()));
    }

    #[test]
    fn shift_enter_is_csi_u() {
        // THE multi-line-input fix: distinguishable from plain Enter.
        assert_eq!(enc(Key::Named(Named::Enter), Modifiers::SHIFT, None, TermMode::empty()),
                   Some(b"\x1b[13;2u".to_vec()));
    }

    #[test]
    fn ctrl_enter_and_alt_enter_are_csi_u() {
        assert_eq!(enc(Key::Named(Named::Enter), Modifiers::CTRL, None, TermMode::empty()),
                   Some(b"\x1b[13;5u".to_vec()));
        assert_eq!(enc(Key::Named(Named::Enter), Modifiers::ALT, None, TermMode::empty()),
                   Some(b"\x1b[13;3u".to_vec()));
    }

    #[test]
    fn arrows_respect_app_cursor_mode() {
        assert_eq!(enc(Key::Named(Named::ArrowUp), Modifiers::empty(), None, TermMode::empty()),
                   Some(b"\x1b[A".to_vec()));
        assert_eq!(enc(Key::Named(Named::ArrowUp), Modifiers::empty(), None, TermMode::APP_CURSOR),
                   Some(b"\x1bOA".to_vec()));
    }

    #[test]
    fn modified_arrows_use_xterm_modifier_encoding() {
        // Shift+Up = CSI 1;2A regardless of APP_CURSOR (xterm behavior).
        assert_eq!(enc(Key::Named(Named::ArrowUp), Modifiers::SHIFT, None, TermMode::APP_CURSOR),
                   Some(b"\x1b[1;2A".to_vec()));
    }

    #[test]
    fn ctrl_letters_are_caret_codes() {
        assert_eq!(enc(Key::Character("c".into()), Modifiers::CTRL, Some("c"), TermMode::empty()),
                   Some(vec![0x03]));
    }

    #[test]
    fn alt_character_gets_esc_prefix() {
        assert_eq!(enc(Key::Character("b".into()), Modifiers::ALT, Some("b"), TermMode::empty()),
                   Some(b"\x1bb".to_vec()));
    }

    #[test]
    fn plain_text_passes_through() {
        assert_eq!(enc(Key::Character("~".into()), Modifiers::SHIFT, Some("~"), TermMode::empty()),
                   Some(b"~".to_vec()));
    }

    #[test]
    fn shift_tab_is_backtab() {
        assert_eq!(enc(Key::Named(Named::Tab), Modifiers::SHIFT, None, TermMode::empty()),
                   Some(b"\x1b[Z".to_vec()));
    }

    #[test]
    fn paste_is_bracketed_only_when_mode_set() {
        assert_eq!(encode_paste("a\nb", &TermMode::empty()), b"a\nb".to_vec());
        assert_eq!(encode_paste("a\nb", &TermMode::BRACKETED_PASTE),
                   b"\x1b[200~a\nb\x1b[201~".to_vec());
    }

    #[test]
    fn local_scroll_never_reaches_the_pty_even_in_mouse_mode() {
        let mouse = TermMode::MOUSE_MODE | TermMode::SGR_MOUSE;
        // Precondition: a non-local scroll in mouse mode *does* go to the PTY.
        assert!(scroll_pty_bytes(false, 3, &mouse).is_some());
        // The drag-autoscroll path opts out regardless of mode.
        assert_eq!(scroll_pty_bytes(true, 3, &mouse), None);
        assert_eq!(scroll_pty_bytes(true, -3, &mouse), None);
        // Without mouse mode, non-local scrolls stay local too.
        assert_eq!(scroll_pty_bytes(false, 3, &TermMode::empty()), None);
    }

    #[test]
    fn wheel_goes_to_app_only_in_mouse_mode() {
        assert_eq!(encode_wheel(1, 5, 3, &TermMode::empty()), None);
        // SGR mouse wheel-up at 1-based col 6, row 4.
        assert_eq!(
            encode_wheel(1, 5, 3, &(TermMode::MOUSE_MODE | TermMode::SGR_MOUSE)),
            Some(b"\x1b[<64;6;4M".to_vec())
        );
        assert_eq!(
            encode_wheel(-1, 5, 3, &(TermMode::MOUSE_MODE | TermMode::SGR_MOUSE)),
            Some(b"\x1b[<65;6;4M".to_vec())
        );
    }

    mod neutral {
        use super::super::*;

        fn legacy() -> InputModes { InputModes::default() }
        fn kitty() -> InputModes { InputModes { protocol: KeyProtocol::Kitty, ..Default::default() } }
        const SHIFT: Mods = Mods { shift: true, ..Mods::NONE };
        const CTRL: Mods = Mods { ctrl: true, ..Mods::NONE };
        const ALT: Mods = Mods { alt: true, ..Mods::NONE };

        fn e(k: KeyInput, m: Mods, modes: InputModes) -> Vec<u8> {
            encode(k, m, None, &modes).expect("encodes")
        }

        #[test]
        fn ctrl_space_is_nul() {
            assert_eq!(e(KeyInput::Char(' '), CTRL, legacy()), vec![0]);
        }

        #[test]
        fn legacy_modified_enter_uses_newline_spellings_agents_accept() {
            assert_eq!(e(KeyInput::Enter, Mods::NONE, legacy()), b"\r");
            assert_eq!(e(KeyInput::Enter, SHIFT, legacy()), b"\x1b\r");
            assert_eq!(e(KeyInput::Enter, ALT, legacy()), b"\x1b\r");
            assert_eq!(e(KeyInput::Enter, CTRL, legacy()), b"\n");
        }

        #[test]
        fn kitty_mode_uses_csi_u_for_escape_and_modified_keys() {
            assert_eq!(e(KeyInput::Escape, Mods::NONE, kitty()), b"\x1b[27u");
            assert_eq!(e(KeyInput::Enter, SHIFT, kitty()), b"\x1b[13;2u");
            assert_eq!(e(KeyInput::Enter, Mods::NONE, kitty()), b"\r");
            assert_eq!(e(KeyInput::Char('c'), CTRL, kitty()), b"\x1b[99;5u");
            assert_eq!(e(KeyInput::BackTab, SHIFT, kitty()), b"\x1b[9;2u");
            // Unmodified text stays text even under kitty.
            assert_eq!(e(KeyInput::Char('x'), Mods::NONE, kitty()), b"x");
        }

        #[test]
        fn legacy_escape_and_backtab() {
            assert_eq!(e(KeyInput::Escape, Mods::NONE, legacy()), b"\x1b");
            assert_eq!(e(KeyInput::BackTab, SHIFT, legacy()), b"\x1b[Z");
        }

        #[test]
        fn backspace_variants() {
            assert_eq!(e(KeyInput::Backspace, Mods::NONE, legacy()), vec![0x7f]);
            assert_eq!(e(KeyInput::Backspace, CTRL, legacy()), vec![0x08]);
            assert_eq!(e(KeyInput::Backspace, ALT, legacy()), b"\x1b\x7f");
        }

        #[test]
        fn ctrl_alt_letter_is_esc_prefixed_caret() {
            assert_eq!(e(KeyInput::Char('x'), Mods { ctrl: true, alt: true, ..Mods::NONE }, legacy()), vec![0x1b, 0x18]);
        }

        #[test]
        fn home_end_follow_app_cursor() {
            assert_eq!(e(KeyInput::Home, Mods::NONE, legacy()), b"\x1b[H");
            let app = InputModes { app_cursor: true, ..legacy() };
            assert_eq!(e(KeyInput::End, Mods::NONE, app), b"\x1bOF");
            assert_eq!(e(KeyInput::End, SHIFT, app), b"\x1b[1;2F");
        }

        #[test]
        fn function_and_tilde_keys() {
            assert_eq!(e(KeyInput::F(1), Mods::NONE, legacy()), b"\x1bOP");
            assert_eq!(e(KeyInput::F(4), CTRL, legacy()), b"\x1b[1;5S");
            assert_eq!(e(KeyInput::F(5), Mods::NONE, legacy()), b"\x1b[15~");
            assert_eq!(e(KeyInput::F(12), SHIFT, legacy()), b"\x1b[24;2~");
            assert_eq!(e(KeyInput::Insert, Mods::NONE, legacy()), b"\x1b[2~");
            assert_eq!(e(KeyInput::PageDown, CTRL, legacy()), b"\x1b[6;5~");
            assert_eq!(encode(KeyInput::F(13), Mods::NONE, None, &legacy()), None);
        }

        #[test]
        fn paste_bracketing() {
            assert_eq!(paste_bytes("hi", false), b"hi");
            assert_eq!(paste_bytes("hi", true), b"\x1b[200~hi\x1b[201~");
        }

        #[test]
        fn mouse_encoding_sgr_and_x10() {
            let off = legacy();
            assert_eq!(encode_mouse(MouseButton::Left, MouseAction::Press, Mods::NONE, 0, 0, &off), None);
            let sgr = InputModes { mouse_reporting: true, sgr_mouse: true, ..legacy() };
            assert_eq!(encode_mouse(MouseButton::Left, MouseAction::Press, Mods::NONE, 4, 2, &sgr).unwrap(), b"\x1b[<0;5;3M");
            assert_eq!(encode_mouse(MouseButton::Left, MouseAction::Release, Mods::NONE, 4, 2, &sgr).unwrap(), b"\x1b[<0;5;3m");
            assert_eq!(encode_mouse(MouseButton::Left, MouseAction::Drag, CTRL, 0, 0, &sgr).unwrap(), b"\x1b[<48;1;1M");
            assert_eq!(encode_mouse(MouseButton::WheelDown, MouseAction::Press, Mods::NONE, 0, 0, &sgr).unwrap(), b"\x1b[<65;1;1M");
            let x10 = InputModes { mouse_reporting: true, ..legacy() };
            assert_eq!(encode_mouse(MouseButton::Right, MouseAction::Press, Mods::NONE, 0, 1, &x10).unwrap(), vec![0x1b, b'[', b'M', 34, 33, 34]);
            assert_eq!(encode_mouse(MouseButton::Right, MouseAction::Release, Mods::NONE, 0, 1, &x10).unwrap(), vec![0x1b, b'[', b'M', 35, 33, 34]);
            assert_eq!(encode_mouse(MouseButton::Left, MouseAction::Press, Mods::NONE, 300, 1, &x10), None);
        }
    }
}
