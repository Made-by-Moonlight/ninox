use std::collections::HashMap;

use iced::{
    widget::{column, container, rich_text, row, scrollable, span, text, text_editor, Space},
    Alignment, Background, Border, Element, Length,
};

use crate::{
    app::Message,
    components::{fleet_detail::ci_summary_color, selectable_markdown::selectable_markdown, shell::tokens},
    style::{MONO, SANS, SANS_BOLD, SANS_SEMIBOLD},
};
use ninox_core::types::{CIStatus, Comment, PR};

/// Uppercase section title with an optional right-aligned mono count.
fn heading<'a>(label: &'a str, sub: Option<String>) -> Element<'a, Message> {
    row![
        text(label.to_uppercase()).size(10.5).font(SANS_BOLD).color(tokens::DIM2),
        Space::new(Length::Fill, 0),
        match sub {
            Some(sub) => Element::from(text(sub).size(10).font(MONO).color(tokens::DIM2)),
            None => Space::new(0, 0).into(),
        },
    ]
    .align_y(Alignment::Center)
    .into()
}

fn card<'a>(content: Element<'a, Message>) -> Element<'a, Message> {
    container(content)
        .width(Length::Fill)
        .padding([12, 14])
        .style(|_| container::Style {
            background: Some(Background::Color(tokens::PANEL2)),
            border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
            ..Default::default()
        })
        .into()
}

/// Info panel — shows PR metadata, CI status, and review comments.
pub fn info_panel<'a>(
    pr: Option<&'a PR>,
    ci: Option<&'a CIStatus>,
    comments: &'a [Comment],
    // Read-only `text_editor` buffer per comment id, so comment bodies are
    // selectable and copyable. Keyed by `Comment::id`; a comment with no
    // entry yet simply renders nothing for its body this frame.
    comment_editors: &'a HashMap<i64, text_editor::Content>,
    brains: String,
) -> Element<'a, Message> {
    let mut items: Vec<Element<'a, Message>> = vec![
        card(text(brains).size(12).font(SANS).color(tokens::DIM).into()),
        Space::new(0, 16).into(),
    ];

    match pr {
        None => {
            items.push(text("No PR yet").size(13).font(SANS).color(tokens::DIM).into());
        }
        Some(pr) => {
            items.push(heading("Pull request", None));
            items.push(Space::new(0, 8).into());
            items.push(card(
                column![
                    text(format!("#{} — {}", pr.number, pr.title)).size(14).font(SANS_SEMIBOLD).color(tokens::TEXT),
                    Space::new(0, 4),
                    rich_text![
                        span(pr.url.as_str())
                            .color(tokens::BLUE)
                            .underline(true)
                            .link(Message::OpenUrl(pr.url.to_string()))
                    ]
                    .size(11)
                    .font(MONO),
                ]
                .into(),
            ));

            if !pr.body.is_empty() {
                let body_text = if pr.body.chars().count() > 300 {
                    format!("{}…", pr.body.chars().take(300).collect::<String>())
                } else {
                    pr.body.clone()
                };
                items.push(Space::new(0, 8).into());
                items.push(card(text(body_text).size(12).font(SANS).color(tokens::DIM).into()));
            }

            if let Some(ci) = ci {
                let glyph = if ci.failing > 0 {
                    "✗"
                } else if ci.pending > 0 {
                    "◌"
                } else {
                    "✓"
                };
                items.push(Space::new(0, 16).into());
                items.push(heading("CI", None));
                items.push(Space::new(0, 8).into());
                items.push(card(
                    row![
                        // Advanced: Inter has no "◌".
                        text(glyph).size(14).color(ci_summary_color(ci)).width(Length::Fixed(18.0)).shaping(iced::widget::text::Shaping::Advanced),
                        text(format!("{}/{} passing", ci.passing, ci.total)).size(12).font(SANS).color(tokens::TEXT),
                    ]
                    .align_y(Alignment::Center)
                    .into(),
                ));
            }

            if !comments.is_empty() {
                items.push(Space::new(0, 16).into());
                items.push(heading("Marginalia", Some(format!("{} comments", comments.len()))));

                for comment in comments {
                    let location = match (&comment.path, comment.line) {
                        (Some(path), Some(line)) => format!("{path}:{line}"),
                        (Some(path), None) => path.clone(),
                        _ => String::new(),
                    };
                    items.push(Space::new(0, 8).into());
                    items.push(card(
                        column![
                            row![
                                text(comment.author.clone()).size(12).font(SANS_SEMIBOLD).color(tokens::VIOLET_BADGE_FG),
                                Space::new(8, 0),
                                text(location).size(10).font(MONO).color(tokens::DIM2),
                            ]
                            .align_y(Alignment::Center),
                            Space::new(0, 4),
                            match comment_editors.get(&comment.id) {
                                // `on_action` is what makes the editor
                                // interactive at all — without it the
                                // widget is inert and cannot be selected.
                                // Edits are dropped in `update`, so this
                                // stays read-only.
                                Some(content) => selectable_markdown(content, move |action| {
                                    Message::CommentAction(comment.id, action)
                                }),
                                None => Space::new(0, 0).into(),
                            },
                        ]
                        .into(),
                    ));
                }
            }
        }
    }

    container(
        scrollable(container(column(items)).width(Length::Fill).padding([16, 20]))
            .width(Length::Fill)
            .height(Length::Fill),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .style(|_| container::Style { background: Some(Background::Color(tokens::BG)), ..Default::default() })
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
