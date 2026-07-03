//! Keyboard/mouse/paste → terminal byte encoding, honoring the modes the
//! inner application negotiated (read from the live alacritty Term).
//!
//! Modified functional keys are always emitted in kitty CSI-u form: ninox
//! only ever talks to its own tmux server (extended-keys always), which
//! forwards them to applications that requested them and downgrades them
//! for applications that didn't — identical to a native extended-keys
//! terminal. Legacy default-socket sessions may not understand CSI-u; they
//! degrade exactly as they did before this feature existed.

use alacritty_terminal::term::TermMode;
use iced::keyboard::{key::Named, Key, Modifiers};

/// xterm/kitty modifier parameter: 1 + bitfield(shift=1, alt=2, ctrl=4, super=8).
fn modifier_param(m: Modifiers) -> u32 {
    1 + (m.shift() as u32)
        + ((m.alt() as u32) << 1)
        + ((m.control() as u32) << 2)
        + ((m.logo() as u32) << 3)
}

/// kitty CSI-u codepoint for functional keys that need disambiguation.
fn functional_code(key: &Key) -> Option<u32> {
    Some(match key {
        Key::Named(Named::Enter)     => 13,
        Key::Named(Named::Escape)    => 27,
        Key::Named(Named::Backspace) => 127,
        Key::Named(Named::Tab)       => 9,
        _ => return None,
    })
}

pub fn encode_key(
    key:       &Key,
    modifiers: Modifiers,
    text:      Option<&str>,
    mode:      &TermMode,
) -> Option<Vec<u8>> {
    let mods = modifier_param(modifiers);

    // Modified functional keys → CSI-u. Shift+Tab keeps its classic
    // backtab encoding (universally understood; CSI-u tab is not).
    if mods > 1 && !(matches!(key, Key::Named(Named::Tab)) && modifiers == Modifiers::SHIFT) {
        if let Some(code) = functional_code(key) {
            return Some(format!("\x1b[{code};{mods}u").into_bytes());
        }
    }

    // Ctrl+letter → caret notation (Ctrl+A=0x01 … Ctrl+Z=0x1A, Ctrl+[=ESC …).
    if modifiers.control() {
        if let Key::Character(c) = key {
            if let Some(ch) = c.chars().next() {
                let b = match ch {
                    'a'..='z' => Some(vec![(ch as u8) - b'a' + 1]),
                    'A'..='Z' => Some(vec![(ch as u8) - b'A' + 1]),
                    '['       => Some(b"\x1b".to_vec()),
                    '\\'      => Some(b"\x1c".to_vec()),
                    ']'       => Some(b"\x1d".to_vec()),
                    '^' | '6' => Some(b"\x1e".to_vec()),
                    '_'       => Some(b"\x1f".to_vec()),
                    _ => None,
                };
                if b.is_some() { return b; }
            }
        }
    }

    // Arrows: modified → xterm CSI 1;<mods><ABCD>; plain → mode-sensitive.
    let arrow = |letter: char| -> Vec<u8> {
        if mods > 1 {
            format!("\x1b[1;{mods}{letter}").into_bytes()
        } else if mode.contains(TermMode::APP_CURSOR) {
            format!("\x1bO{letter}").into_bytes()
        } else {
            format!("\x1b[{letter}").into_bytes()
        }
    };

    let bytes: Vec<u8> = match key {
        Key::Named(Named::Enter)      => b"\r".to_vec(),
        Key::Named(Named::Escape)     => b"\x1b".to_vec(),
        Key::Named(Named::Backspace)  => b"\x7f".to_vec(),
        Key::Named(Named::Delete)     => b"\x1b[3~".to_vec(),
        Key::Named(Named::Tab) if modifiers.shift() => b"\x1b[Z".to_vec(),
        Key::Named(Named::Tab)        => b"\t".to_vec(),
        Key::Named(Named::ArrowUp)    => arrow('A'),
        Key::Named(Named::ArrowDown)  => arrow('B'),
        Key::Named(Named::ArrowRight) => arrow('C'),
        Key::Named(Named::ArrowLeft)  => arrow('D'),
        Key::Named(Named::Home)       => b"\x1b[H".to_vec(),
        Key::Named(Named::End)        => b"\x1b[F".to_vec(),
        Key::Named(Named::PageUp)     => b"\x1b[5~".to_vec(),
        Key::Named(Named::PageDown)   => b"\x1b[6~".to_vec(),
        // Alt+char → ESC prefix; otherwise prefer `text` (shift-resolved).
        Key::Character(c) => {
            let base = text.map(|t| t.as_bytes().to_vec())
                           .unwrap_or_else(|| c.as_str().as_bytes().to_vec());
            if modifiers.alt() {
                let mut v = b"\x1b".to_vec();
                v.extend(base);
                v
            } else {
                base
            }
        }
        _ => text.map(|t| t.as_bytes().to_vec()).unwrap_or_default(),
    };
    if bytes.is_empty() { None } else { Some(bytes) }
}

/// Wrap pasted text in bracketed-paste markers when the app asked for them.
pub fn encode_paste(text: &str, mode: &TermMode) -> Vec<u8> {
    if mode.contains(TermMode::BRACKETED_PASTE) {
        let mut v = b"\x1b[200~".to_vec();
        v.extend(text.as_bytes());
        v.extend_from_slice(b"\x1b[201~");
        v
    } else {
        text.as_bytes().to_vec()
    }
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
}
