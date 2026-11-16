//! Iced-specific key encoding on top of the shared, toolkit-neutral encoder
//! in `ninox_ui_shared::input` (the ratatui TUI has its own equivalent in
//! `tui::keys`, built on the same shared core).

pub use ninox_ui_shared::input::*;

use alacritty_terminal::term::TermMode;
use iced::keyboard::{key::Named, Key, Modifiers};

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
