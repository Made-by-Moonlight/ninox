use iced::{
    widget::{column, container, scrollable, text, Space},
    Background, Border, Element, Length,
};

use crate::{
    app::Message,
    theme::{ACCENT_AMBER, BG_ELEVATED, BG_SURFACE, BORDER, STATUS_GREEN, STATUS_RED, STATUS_YELLOW, TEXT_MUTED, TEXT_PRIMARY, TEXT_SECONDARY},
};

use athene_core::types::{CIStatus, Comment, Session, PR};

/// Info panel — shows PR metadata, CI status, and review comments.
pub fn info_panel<'a>(
    _session: &'a Session,
    pr: Option<&'a PR>,
    ci: Option<&'a CIStatus>,
    comments: &'a [Comment],
) -> Element<'a, Message> {
    let mut items: Vec<Element<'a, Message>> = Vec::new();

    match pr {
        None => {
            // Empty state
            items.push(
                container(
                    text("No PR yet")
                        .size(13)
                        .color(TEXT_MUTED),
                )
                .width(Length::Fill)
                .padding([20, 16])
                .into(),
            );
        }
        Some(pr) => {
            // --- PR section ---
            let pr_title = format!("PR #{} — {}", pr.number, pr.title);
            items.push(
                container(
                    column![
                        text(pr_title).size(14).color(TEXT_PRIMARY),
                        Space::new(0, 4),
                        text(pr.url.as_str()).size(11).color(ACCENT_AMBER),
                    ],
                )
                .width(Length::Fill)
                .padding([12, 16])
                .style(|_theme| container::Style {
                    background: Some(Background::Color(BG_ELEVATED)),
                    border: Border {
                        color: BORDER,
                        width: 1.0,
                        radius: 4.0.into(),
                    },
                    ..Default::default()
                })
                .into(),
            );

            // PR body (truncated at 300 chars)
            if !pr.body.is_empty() {
                let body_text = if pr.body.chars().count() > 300 {
                    format!("{}…", pr.body.chars().take(300).collect::<String>())
                } else {
                    pr.body.clone()
                };
                items.push(Space::new(0, 8).into());
                items.push(
                    container(
                        text(body_text).size(12).color(TEXT_SECONDARY),
                    )
                    .width(Length::Fill)
                    .padding([10, 16])
                    .into(),
                );
            }

            // --- CI section ---
            if let Some(ci) = ci {
                items.push(Space::new(0, 12).into());
                let ci_label = format!("CI: {}/{} passing", ci.passing, ci.total);
                let ci_color = if ci.failing > 0 {
                    STATUS_RED
                } else if ci.pending > 0 {
                    STATUS_YELLOW
                } else {
                    STATUS_GREEN
                };
                items.push(
                    container(
                        text(ci_label).size(13).color(ci_color),
                    )
                    .width(Length::Fill)
                    .padding([10, 16])
                    .style(|_theme| container::Style {
                        background: Some(Background::Color(BG_ELEVATED)),
                        border: Border {
                            color: BORDER,
                            width: 1.0,
                            radius: 4.0.into(),
                        },
                        ..Default::default()
                    })
                    .into(),
                );
            }

            // --- Review comments ---
            if !comments.is_empty() {
                items.push(Space::new(0, 12).into());
                items.push(
                    text("Review Comments")
                        .size(11)
                        .color(TEXT_MUTED)
                        .into(),
                );
                items.push(Space::new(0, 6).into());

                for comment in comments {
                    let mut comment_parts: Vec<Element<'a, Message>> = Vec::new();

                    // Author line
                    let location = match (&comment.path, comment.line) {
                        (Some(path), Some(line)) => format!("{} · {}:{}", comment.author, path, line),
                        (Some(path), None) => format!("{} · {}", comment.author, path),
                        _ => comment.author.clone(),
                    };
                    comment_parts.push(
                        text(location).size(11).color(ACCENT_AMBER).into(),
                    );
                    comment_parts.push(Space::new(0, 4).into());
                    comment_parts.push(
                        text(comment.body.as_str()).size(12).color(TEXT_SECONDARY).into(),
                    );

                    items.push(
                        container(
                            column(comment_parts),
                        )
                        .width(Length::Fill)
                        .padding([10, 12])
                        .style(|_theme| container::Style {
                            background: Some(Background::Color(BG_ELEVATED)),
                            border: Border {
                                color: BORDER,
                                width: 1.0,
                                radius: 4.0.into(),
                            },
                            ..Default::default()
                        })
                        .into(),
                    );
                    items.push(Space::new(0, 6).into());
                }
            }
        }
    }

    let content = container(
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
    .style(|_theme| container::Style {
        background: Some(Background::Color(BG_SURFACE)),
        ..Default::default()
    });

    content.into()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

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
