//! The `?` keyboard reference. `SECTIONS` is the one list of shortcuts: the
//! overlay renders its chips from each `Chord`, and `app::tests` replays
//! every chord through the real `RawKey` handler so a listed key can't go
//! dead.

use iced::widget::{column, container, mouse_area, row, scrollable, text, Space};
use iced::{Alignment, Background, Border, Color, Element, Length};

use super::shell::tokens;
use crate::app::Message;
use crate::style::{MONO, SANS};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(&'static str),
    Escape,
    Enter,
    Up,
    Down,
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chord {
    pub key: Key,
    /// ⌘ on macOS, Ctrl elsewhere (`Modifiers::command`).
    pub cmd: bool,
    pub shift: bool,
    /// The Control key on every platform, unlike `cmd`.
    pub ctrl: bool,
}

const fn key(c: &'static str) -> Chord {
    Chord {
        key: Key::Char(c),
        cmd: false,
        shift: false,
        ctrl: false,
    }
}
const fn named(key: Key) -> Chord {
    Chord {
        key,
        cmd: false,
        shift: false,
        ctrl: false,
    }
}
const fn cmd(c: &'static str) -> Chord {
    Chord {
        key: Key::Char(c),
        cmd: true,
        shift: false,
        ctrl: false,
    }
}

const fn ctrl(c: &'static str) -> Chord {
    Chord {
        key: Key::Char(c),
        cmd: false,
        shift: false,
        ctrl: true,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Anywhere,
    Fleet,
    Session,
    Brain,
    BrainGraph,
    Spawn,
}

impl Scope {
    pub fn title(self) -> &'static str {
        match self {
            Scope::Anywhere => "Everywhere",
            Scope::Fleet => "Fleet",
            Scope::Session => "Session page",
            Scope::Brain => "Brain",
            Scope::BrainGraph => "Brain graph",
            Scope::Spawn => "Spawn dialog",
        }
    }
}

pub struct Shortcut {
    pub chords: &'static [Chord],
    pub action: &'static str,
}

pub struct Section {
    pub scope: Scope,
    pub shortcuts: &'static [Shortcut],
}

pub const SECTIONS: &[Section] = &[
    Section {
        scope: Scope::Anywhere,
        shortcuts: &[
            Shortcut {
                chords: &[key("/")],
                action: "Focus the prompt",
            },
            Shortcut {
                chords: &[cmd("k")],
                action: "Focus search",
            },
            Shortcut {
                chords: &[Chord {
                    key: Key::Char("k"),
                    cmd: true,
                    shift: true,
                    ctrl: false,
                }],
                action: "Clear context",
            },
            Shortcut {
                chords: &[key("["), cmd("b")],
                action: "Toggle the tree",
            },
            Shortcut {
                chords: &[key("1"), key("2"), key("3"), key("4"), key("5"), key("6")],
                action: "Switch tabs",
            },
            Shortcut {
                chords: &[named(Key::Escape)],
                action: "Go back",
            },
            Shortcut {
                chords: &[named(Key::Up), named(Key::Down)],
                action: "Recall prompt history",
            },
            Shortcut {
                chords: &[key("t")],
                action: "Switch light and dark theme",
            },
            Shortcut {
                chords: &[cmd("="), cmd("-"), cmd("0")],
                action: "Zoom in, out, reset",
            },
            Shortcut {
                chords: &[key("?")],
                action: "Show this reference",
            },
        ],
    },
    Section {
        scope: Scope::Fleet,
        shortcuts: &[
            Shortcut {
                chords: &[named(Key::Enter)],
                action: "Open the selected island",
            },
            Shortcut {
                chords: &[named(Key::Escape)],
                action: "Clear the selection, then close the island",
            },
        ],
    },
    Section {
        scope: Scope::Session,
        shortcuts: &[
            Shortcut {
                chords: &[key("c")],
                action: "Type into the terminal",
            },
            Shortcut {
                chords: &[ctrl("]")],
                action: "Release the terminal",
            },
            Shortcut {
                chords: &[named(Key::Escape)],
                action: "Back to the fleet",
            },
        ],
    },
    Section {
        scope: Scope::Brain,
        shortcuts: &[Shortcut {
            chords: &[cmd("f")],
            action: "Focus Brain search",
        }],
    },
    Section {
        scope: Scope::BrainGraph,
        shortcuts: &[
            Shortcut {
                chords: &[
                    named(Key::Left),
                    named(Key::Right),
                    named(Key::Up),
                    named(Key::Down),
                ],
                action: "Walk links",
            },
            Shortcut {
                chords: &[named(Key::Escape)],
                action: "Back to clusters",
            },
        ],
    },
    Section {
        scope: Scope::Spawn,
        shortcuts: &[
            Shortcut {
                chords: &[Chord {
                    key: Key::Enter,
                    cmd: true,
                    shift: false,
                    ctrl: false,
                }],
                action: "Spawn",
            },
            Shortcut {
                chords: &[named(Key::Escape)],
                action: "Close",
            },
        ],
    },
];

pub fn label(chord: &Chord) -> String {
    let base = match chord.key {
        Key::Char(c) if chord.cmd => c.to_uppercase(),
        Key::Char(c) => c.to_string(),
        Key::Escape => "esc".into(),
        Key::Enter => "↵".into(),
        Key::Up => "↑".into(),
        Key::Down => "↓".into(),
        Key::Left => "←".into(),
        Key::Right => "→".into(),
    };
    let mut out = String::new();
    if chord.ctrl {
        out.push_str("Ctrl+");
    }
    if chord.cmd {
        out.push_str(if cfg!(target_os = "macos") {
            "⌘"
        } else {
            "Ctrl+"
        });
    }
    if chord.shift {
        out.push_str(if cfg!(target_os = "macos") {
            "⇧"
        } else {
            "Shift+"
        });
    }
    out.push_str(&base);
    out
}

/// Chips with a symbol use `GLYPH_ALT` (Inter: ⌘ ⇧ ↵ and the arrows, plus
/// ASCII); the mono fonts don't all have them.
pub(crate) fn chip_font(label: &str) -> iced::Font {
    if label.is_ascii() {
        MONO
    } else {
        crate::style::GLYPH_ALT
    }
}

fn chip(label: String) -> Element<'static, Message> {
    let font = chip_font(&label);
    container(text(label).size(11.5).font(font).color(tokens::TEXT))
        .padding([2, 7])
        .style(|_| container::Style {
            background: Some(Background::Color(tokens::PANEL2)),
            border: Border {
                color: tokens::BORDER,
                width: 1.0,
                radius: 4.0.into(),
            },
            ..Default::default()
        })
        .into()
}

fn section(s: &'static Section) -> Element<'static, Message> {
    let mut rows = column![text(s.scope.title().to_uppercase())
        .size(10.5)
        .font(SANS)
        .color(tokens::DIM2)]
    .spacing(8);
    for sc in s.shortcuts {
        let chips = row(sc.chords.iter().map(|c| chip(label(c)))).spacing(4);
        rows = rows.push(
            row![
                container(chips).width(Length::Fixed(150.0)),
                text(sc.action).size(13).font(SANS).color(tokens::TEXT),
            ]
            .align_y(Alignment::Center),
        );
    }
    rows.into()
}

pub fn help_overlay() -> Element<'static, Message> {
    let header = container(
        row![
            text("Keyboard").size(15).font(SANS).color(tokens::TEXT),
            Space::with_width(Length::Fill),
            text("esc to close").size(11).font(SANS).color(tokens::DIM2),
        ]
        .align_y(Alignment::Center),
    )
    .padding([16, 20])
    .width(Length::Fill)
    .style(|_| container::Style {
        background: Some(Background::Color(tokens::PANEL2)),
        ..Default::default()
    });

    let mut body = column(SECTIONS.iter().map(section)).spacing(20);
    body = body.push(
        text("In a session terminal every other key goes to the session; ⌘V pastes.")
            .size(11.5)
            .font(SANS)
            .color(tokens::DIM),
    );

    let card = container(column![
        header,
        scrollable(container(body).padding([18, 20]))
    ])
    .width(Length::Fixed(520.0))
    .max_height(640.0)
    .style(|_| container::Style {
        background: Some(Background::Color(tokens::PANEL)),
        border: Border {
            color: tokens::BORDER,
            width: 1.0,
            radius: 8.0.into(),
        },
        ..Default::default()
    });

    // The card swallows its own clicks so only the backdrop closes it.
    let card = mouse_area(card).on_press(Message::Noop);
    mouse_area(
        container(card)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .style(|_| container::Style {
                background: Some(Background::Color(Color {
                    a: 0.6,
                    ..Color::BLACK
                })),
                ..Default::default()
            }),
    )
    .on_press(Message::ToggleHelp)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listed(chord: Chord) -> bool {
        SECTIONS.iter().any(|s| {
            s.scope == Scope::Anywhere && s.shortcuts.iter().any(|sc| sc.chords.contains(&chord))
        })
    }

    #[test]
    fn the_decided_shortcut_map_is_listed_everywhere() {
        for chord in [
            key("/"),
            cmd("k"),
            Chord {
                key: Key::Char("k"),
                cmd: true,
                shift: true,
                ctrl: false,
            },
            key("["),
            key("1"),
            key("6"),
            named(Key::Escape),
            key("?"),
        ] {
            assert!(listed(chord), "{chord:?}");
        }
        let brain = SECTIONS.iter().find(|s| s.scope == Scope::Brain).unwrap();
        let session = SECTIONS.iter().find(|s| s.scope == Scope::Session).unwrap();
        for chord in [key("c"), ctrl("]"), named(Key::Escape)] {
            assert!(session.shortcuts.iter().any(|sc| sc.chords.contains(&chord)), "{chord:?}");
        }
        assert!(brain
            .shortcuts
            .iter()
            .any(|sc| sc.chords.contains(&cmd("f"))));
    }

    #[test]
    fn labels_use_the_platform_command_key() {
        let k = label(&Chord {
            key: Key::Char("k"),
            cmd: true,
            shift: true,
            ctrl: false,
        });
        if cfg!(target_os = "macos") {
            assert_eq!(k, "⌘⇧K");
        } else {
            assert_eq!(k, "Ctrl+Shift+K");
        }
        assert_eq!(label(&named(Key::Escape)), "esc");
        assert_eq!(label(&key("[")), "[");
        assert_eq!(label(&key("t")), "t", "a bare letter is not Shift+letter");
        assert_eq!(label(&ctrl("]")), "Ctrl+]", "Control is spelled out, never ⌘");
        assert_eq!(chip_font("/"), MONO);
        assert_eq!(chip_font("⌘K"), crate::style::GLYPH_ALT);
        assert_eq!(chip_font("↵"), crate::style::GLYPH_ALT);
    }
}
