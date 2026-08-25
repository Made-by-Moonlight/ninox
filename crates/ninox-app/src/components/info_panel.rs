use std::collections::HashMap;

use iced::{
    widget::{column, container, rich_text, row, scrollable, span, text, text_editor, Space},
    Alignment, Background, Element, Length,
};

use crate::{app::Message, theme::ColorScheme};
use ninox_core::types::{CIStatus, Comment, Session, PR};

/// Serif-italic heading over a dotted rule — the "h3" treatment used
/// throughout the info panels. `sub`, when present, is right-aligned mono.
fn heading<'a>(label: &'a str, sub: Option<String>, s: &'a ColorScheme) -> Element<'a, Message> {
    column![
        row![
            text(label).size(17).font(crate::style::SERIF_ITALIC).color(s.ink),
            Space::new(Length::Fill, 0),
            match sub {
                Some(sub) => Element::from(text(sub).size(9.5).font(crate::style::MONO).color(s.faint)),
                None => Space::new(0, 0).into(),
            },
        ]
        .align_y(Alignment::Center),
        Space::new(0, 4),
        crate::style::dotted_rule(s.rule_dark),
    ]
    .into()
}

/// Info panel — shows PR metadata, CI status, and review comments.
pub fn info_panel<'a>(
    _session: &'a Session,
    pr: Option<&'a PR>,
    ci: Option<&'a CIStatus>,
    comments: &'a [Comment],
    // Read-only `text_editor` buffer per comment id, so comment bodies are
    // selectable and copyable. Keyed by `Comment::id`; a comment with no
    // entry yet simply renders nothing for its body this frame.
    comment_editors: &'a HashMap<i64, text_editor::Content>,
    s: &'a ColorScheme,
) -> Element<'a, Message> {
    let mut items: Vec<Element<'a, Message>> = Vec::new();

    match pr {
        None => {
            items.push(
                container(text("No PR yet").size(13).color(s.faint))
                    .width(Length::Fill)
                    .padding([20, 16])
                    .into(),
            );
        }
        Some(pr) => {
            items.push(
                container(
                    column![
                        heading("Pull Request", None, s),
                        Space::new(0, 8),
                        text(format!("#{} — {}", pr.number, pr.title))
                            .size(14)
                            .font(crate::style::SERIF_MEDIUM)
                            .color(s.ink),
                        Space::new(0, 4),
                        rich_text![
                            span(pr.url.as_str())
                                .color(s.accent)
                                .underline(true)
                                .link(Message::OpenUrl(pr.url.to_string()))
                        ]
                        .size(11),
                    ],
                )
                .width(Length::Fill)
                .padding([14, 16])
                .style(move |_theme| crate::style::card_style(s))
                .into(),
            );

            if !pr.body.is_empty() {
                let body_text = if pr.body.chars().count() > 300 {
                    format!("{}…", pr.body.chars().take(300).collect::<String>())
                } else {
                    pr.body.clone()
                };
                items.push(Space::new(0, 10).into());
                items.push(
                    container(text(body_text).size(12).color(s.ink_2))
                        .width(Length::Fill)
                        .padding([14, 16])
                        .style(move |_theme| crate::style::card_style(s))
                        .into(),
                );
            }

            if let Some(ci) = ci {
                items.push(Space::new(0, 12).into());
                let ci_color = crate::style::ci_color(s, ci);
                let glyph = if ci.failing > 0 {
                    "✗"
                } else if ci.pending > 0 {
                    "◌"
                } else {
                    "✓"
                };
                items.push(
                    container(
                        column![
                            heading("CI", None, s),
                            Space::new(0, 8),
                            row![
                                text(glyph).size(14).color(ci_color).width(Length::Fixed(14.0)),
                                text(format!("{}/{} passing", ci.passing, ci.total))
                                    .size(12)
                                    .font(crate::style::SANS)
                                    .color(s.ink_2),
                            ]
                            .align_y(Alignment::Center),
                        ],
                    )
                    .width(Length::Fill)
                    .padding([14, 16])
                    .style(move |_theme| crate::style::card_style(s))
                    .into(),
                );
            }

            if !comments.is_empty() {
                items.push(Space::new(0, 12).into());
                items.push(heading("Marginalia", Some(format!("{} comments", comments.len())), s));

                // One card per comment — no shared block, no dotted dividers.
                for comment in comments {
                    let location = match (&comment.path, comment.line) {
                        (Some(path), Some(line)) => format!("{path}:{line}"),
                        (Some(path), None) => path.clone(),
                        _ => String::new(),
                    };
                    items.push(Space::new(0, 8).into());
                    items.push(
                        container(
                            column![
                                row![
                                    text(comment.author.clone())
                                        .size(12)
                                        .font(crate::style::SANS_BOLD)
                                        .color(s.accent),
                                    if !location.is_empty() {
                                        Element::from(row![
                                            Space::new(8, 0),
                                            text(location).size(9.5).font(crate::style::MONO).color(s.faint),
                                        ])
                                    } else {
                                        Space::new(0, 0).into()
                                    },
                                ]
                                .align_y(Alignment::Center),
                                Space::new(0, 4),
                                match comment_editors.get(&comment.id) {
                                    // `on_action` is what makes the editor
                                    // interactive at all — without it the
                                    // widget is inert and cannot be selected.
                                    // Edits are dropped in `update`, so this
                                    // stays read-only.
                                    Some(content) => Element::from(
                                        text_editor(content)
                                            .on_action(move |action| {
                                                Message::CommentAction(comment.id, action)
                                            })
                                            .size(12)
                                            .padding(0)
                                            .style(move |_theme, _status| text_editor::Style {
                                                background: Background::Color(
                                                    iced::Color::TRANSPARENT,
                                                ),
                                                border: iced::Border::default(),
                                                icon: s.faint,
                                                placeholder: s.faint,
                                                value: s.ink_2,
                                                selection: iced::Color {
                                                    a: 0.30,
                                                    ..s.accent
                                                },
                                            }),
                                    ),
                                    None => Space::new(0, 0).into(),
                                },
                            ],
                        )
                        .width(Length::Fill)
                        .padding([14, 16])
                        .style(move |_theme| crate::style::card_style(s))
                        .into(),
                    );
                }
            }
        }
    }

    container(
        scrollable(
            container(column(items))
                .width(Length::Fill)
                .padding([12, 12]),
        )
        .width(Length::Fill)
        .height(Length::Fill),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .style(move |_theme| container::Style {
        background: Some(Background::Color(s.card)),
        ..Default::default()
    })
    .into()
}

#[cfg(test)]
mod tests {
    #[test]
    fn ci_format() {
        assert_eq!(format!("CI: {}/{} passing", 3u32, 4u32), "CI: 3/4 passing");
    }

    #[test]
    fn cost_format() {
        assert_eq!(format!("${:.2}", 0.427f64), "$0.43");
    }
}
