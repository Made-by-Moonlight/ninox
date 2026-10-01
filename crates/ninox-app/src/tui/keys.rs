//! crossterm events → pane input bytes, via the shared encoder in
//! `crate::input`, honoring the modes the pane's application negotiated.

use crate::input::{self, InputModes, KeyInput, KeyProtocol, Mods, MouseAction, MouseButton};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton as CtButton, MouseEvent, MouseEventKind};

pub fn input_modes(m: &ninox_ptyd::Modes) -> InputModes {
    InputModes {
        app_cursor: m.app_cursor,
        bracketed_paste: m.bracketed_paste,
        mouse_reporting: m.mouse_reporting,
        sgr_mouse: m.sgr_mouse,
        protocol: if m.kitty_keyboard != 0 { KeyProtocol::Kitty } else { KeyProtocol::Legacy },
    }
}

fn mods(m: KeyModifiers) -> Mods {
    Mods {
        shift: m.contains(KeyModifiers::SHIFT),
        alt: m.contains(KeyModifiers::ALT),
        ctrl: m.contains(KeyModifiers::CONTROL),
        logo: m.contains(KeyModifiers::SUPER),
    }
}

fn key_input(code: KeyCode) -> Option<KeyInput> {
    Some(match code {
        KeyCode::Char(c) => KeyInput::Char(c),
        KeyCode::Null => KeyInput::Char(' '),
        KeyCode::Enter => KeyInput::Enter,
        KeyCode::Esc => KeyInput::Escape,
        KeyCode::Backspace => KeyInput::Backspace,
        KeyCode::Delete => KeyInput::Delete,
        KeyCode::Insert => KeyInput::Insert,
        KeyCode::Tab => KeyInput::Tab,
        KeyCode::BackTab => KeyInput::BackTab,
        KeyCode::Up => KeyInput::Up,
        KeyCode::Down => KeyInput::Down,
        KeyCode::Left => KeyInput::Left,
        KeyCode::Right => KeyInput::Right,
        KeyCode::Home => KeyInput::Home,
        KeyCode::End => KeyInput::End,
        KeyCode::PageUp => KeyInput::PageUp,
        KeyCode::PageDown => KeyInput::PageDown,
        KeyCode::F(n) => KeyInput::F(n),
        _ => return None,
    })
}

pub fn key_bytes(key: &KeyEvent, modes: &ninox_ptyd::Modes) -> Option<Vec<u8>> {
    let k = key_input(key.code)?;
    let mut m = mods(key.modifiers);
    if key.code == KeyCode::Null {
        m.ctrl = true;
    }
    // crossterm already shift-resolves characters ('A' with SHIFT).
    let text = match key.code {
        KeyCode::Char(c) => Some(c.to_string()),
        _ => None,
    };
    input::encode(k, m, text.as_deref(), &input_modes(modes))
}

/// The control byte this key press would send, if it is a bare Ctrl chord.
pub fn ctrl_byte(key: &KeyEvent) -> Option<u8> {
    if key.code == KeyCode::Null {
        return Some(0);
    }
    if !key.modifiers.contains(KeyModifiers::CONTROL) || key.modifiers.contains(KeyModifiers::ALT) {
        return None;
    }
    let KeyCode::Char(_) = key.code else { return None };
    match key_bytes(key, &ninox_ptyd::Modes::default())?.as_slice() {
        [b] if *b < 0x20 || *b == 0x7f => Some(*b),
        _ => None,
    }
}

pub fn is_prefix(key: &KeyEvent, prefix: u8) -> bool {
    ctrl_byte(key) == Some(prefix)
}

/// Human spelling of a prefix byte for hints.
pub fn prefix_label(prefix: u8) -> String {
    match prefix {
        0 => "Ctrl+Space".into(),
        1..=26 => format!("Ctrl+{}", (b'a' + prefix - 1) as char),
        0x1c => "Ctrl+\\".into(),
        0x1d => "Ctrl+]".into(),
        0x1e => "Ctrl+^".into(),
        0x1f => "Ctrl+_".into(),
        b => format!("0x{b:02x}"),
    }
}

/// Bytes for a mouse event at pane-relative cell (col, row), when the
/// pane's application has mouse reporting on.
pub fn mouse_bytes(ev: &MouseEvent, col: u16, row: u16, modes: &ninox_ptyd::Modes) -> Option<Vec<u8>> {
    let button = |b: CtButton| match b {
        CtButton::Left => MouseButton::Left,
        CtButton::Middle => MouseButton::Middle,
        CtButton::Right => MouseButton::Right,
    };
    let (b, action) = match ev.kind {
        MouseEventKind::Down(b) => (button(b), MouseAction::Press),
        MouseEventKind::Up(b) => (button(b), MouseAction::Release),
        MouseEventKind::Drag(b) => (button(b), MouseAction::Drag),
        MouseEventKind::ScrollUp => (MouseButton::WheelUp, MouseAction::Press),
        MouseEventKind::ScrollDown => (MouseButton::WheelDown, MouseAction::Press),
        _ => return None,
    };
    input::encode_mouse(b, action, mods(ev.modifiers), col, row, &input_modes(modes))
}

pub fn paste_bytes(text: &str, modes: &ninox_ptyd::Modes) -> Vec<u8> {
    input::paste_bytes(text, modes.bracketed_paste)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninox_ptyd::Modes;

    fn k(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, m)
    }

    #[test]
    fn prefix_chords_are_recognised() {
        assert!(is_prefix(&k(KeyCode::Char(' '), KeyModifiers::CONTROL), 0));
        assert!(is_prefix(&k(KeyCode::Null, KeyModifiers::NONE), 0));
        assert!(!is_prefix(&k(KeyCode::Char(' '), KeyModifiers::NONE), 0));
        assert!(is_prefix(&k(KeyCode::Char('g'), KeyModifiers::CONTROL), 7));
        assert!(!is_prefix(&k(KeyCode::Char('g'), KeyModifiers::CONTROL | KeyModifiers::ALT), 7));
        // crossterm reports the FS byte (macOS's default prefix) as Ctrl-4.
        assert!(is_prefix(&k(KeyCode::Char('4'), KeyModifiers::CONTROL), 0x1c));
        assert!(is_prefix(&k(KeyCode::Char('\\'), KeyModifiers::CONTROL), 0x1c));
    }

    #[test]
    fn honors_app_cursor_and_kitty_modes() {
        let plain = Modes::default();
        assert_eq!(key_bytes(&k(KeyCode::Up, KeyModifiers::NONE), &plain).unwrap(), b"\x1b[A");
        let app = Modes { app_cursor: true, ..plain };
        assert_eq!(key_bytes(&k(KeyCode::Up, KeyModifiers::NONE), &app).unwrap(), b"\x1bOA");
        let kitty = Modes { kitty_keyboard: 1, ..plain };
        assert_eq!(key_bytes(&k(KeyCode::Enter, KeyModifiers::SHIFT), &kitty).unwrap(), b"\x1b[13;2u");
        assert_eq!(key_bytes(&k(KeyCode::Enter, KeyModifiers::SHIFT), &plain).unwrap(), b"\x1b\r");
    }

    #[test]
    fn shifted_chars_and_backtab() {
        let m = Modes::default();
        assert_eq!(key_bytes(&k(KeyCode::Char('A'), KeyModifiers::SHIFT), &m).unwrap(), b"A");
        assert_eq!(key_bytes(&k(KeyCode::BackTab, KeyModifiers::SHIFT), &m).unwrap(), b"\x1b[Z");
        assert_eq!(key_bytes(&k(KeyCode::Char('c'), KeyModifiers::CONTROL), &m).unwrap(), vec![3]);
        assert_eq!(key_bytes(&k(KeyCode::Char('é'), KeyModifiers::NONE), &m).unwrap(), "é".as_bytes());
    }

    #[test]
    fn paste_and_mouse_follow_pane_modes() {
        let m = Modes { bracketed_paste: true, ..Default::default() };
        assert_eq!(paste_bytes("x", &m), b"\x1b[200~x\x1b[201~");
        let ev = MouseEvent { kind: MouseEventKind::ScrollUp, column: 0, row: 0, modifiers: KeyModifiers::NONE };
        assert_eq!(mouse_bytes(&ev, 3, 4, &Modes::default()), None);
        let mm = Modes { mouse_reporting: true, sgr_mouse: true, ..Default::default() };
        assert_eq!(mouse_bytes(&ev, 3, 4, &mm).unwrap(), b"\x1b[<64;4;5M");
    }

    #[test]
    fn prefix_labels() {
        assert_eq!(prefix_label(0), "Ctrl+Space");
        assert_eq!(prefix_label(7), "Ctrl+g");
    }
}
