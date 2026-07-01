use iced::{
    widget::{column, container, rich_text, scrollable, span, text, Space},
    Background, Border, Element, Length,
};

use crate::{app::Message, theme::ColorScheme};
use athene_core::types::{CIStatus, Comment, Session, PR};

/// Info panel — shows PR metadata, CI status, and review comments.
pub fn info_panel<'a>(
    _session: &'a Session,
    pr: Option<&'a PR>,
    ci: Option<&'a CIStatus>,
    comments: &'a [Comment],
    s: &'a ColorScheme,
) -> Element<'a, Message> {
    let mut items: Vec<Element<'a, Message>> = Vec::new();

    match pr {
        None => {
            items.push(
                container(text("No PR yet").size(13).color(s.text_muted))
                    .width(Length::Fill)
                    .padding([20, 16])
                    .into(),
            );
        }
        Some(pr) => {
            let pr_title = format!("PR #{} — {}", pr.number, pr.title);
            items.push(
                container(
                    column![
                        text(pr_title).size(14).color(s.text_primary),
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
                .padding([12, 16])
                .style(move |_theme| container::Style {
                    background: Some(Background::Color(s.bg_elevated)),
                    border: Border { color: s.border, width: 1.0, radius: 4.0.into() },
                    ..Default::default()
                })
                .into(),
            );

            if !pr.body.is_empty() {
                let body_text = if pr.body.chars().count() > 300 {
                    format!("{}…", pr.body.chars().take(300).collect::<String>())
                } else {
                    pr.body.clone()
                };
                items.push(Space::new(0, 8).into());
                items.push(
                    container(text(body_text).size(12).color(s.text_secondary))
                        .width(Length::Fill)
                        .padding([10, 16])
                        .into(),
                );
            }

            if let Some(ci) = ci {
                items.push(Space::new(0, 12).into());
                let ci_label = format!("CI: {}/{} passing", ci.passing, ci.total);
                let ci_color = if ci.failing > 0 {
                    s.status_red
                } else if ci.pending > 0 {
                    s.status_yellow
                } else {
                    s.status_green
                };
                items.push(
                    container(text(ci_label).size(13).color(ci_color))
                        .width(Length::Fill)
                        .padding([10, 16])
                        .style(move |_theme| container::Style {
                            background: Some(Background::Color(s.bg_elevated)),
                            border: Border { color: s.border, width: 1.0, radius: 4.0.into() },
                            ..Default::default()
                        })
                        .into(),
                );
            }

            if !comments.is_empty() {
                items.push(Space::new(0, 12).into());
                items.push(text("Review Comments").size(11).color(s.text_muted).into());
                items.push(Space::new(0, 6).into());

                for comment in comments {
                    let location = match (&comment.path, comment.line) {
                        (Some(path), Some(line)) => format!("{} · {}:{}", comment.author, path, line),
                        (Some(path), None) => format!("{} · {}", comment.author, path),
                        _ => comment.author.clone(),
                    };
                    items.push(
                        container(
                            column![
                                text(location).size(11).color(s.accent),
                                Space::new(0, 4),
                                text(comment.body.as_str()).size(12).color(s.text_secondary),
                            ],
                        )
                        .width(Length::Fill)
                        .padding([10, 12])
                        .style(move |_theme| container::Style {
                            background: Some(Background::Color(s.bg_elevated)),
                            border: Border { color: s.border, width: 1.0, radius: 4.0.into() },
                            ..Default::default()
                        })
                        .into(),
                    );
                    items.push(Space::new(0, 6).into());
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
        background: Some(Background::Color(s.bg_surface)),
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
