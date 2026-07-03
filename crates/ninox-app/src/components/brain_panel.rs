use iced::{
    widget::{button, column, container, row, scrollable, text, text_input, Space},
    Alignment, Background, Border, Color, Element, Length,
};

use crate::app::{App, Message};
use ninox_core::BrainEntry;

fn matches_filter(entry: &BrainEntry, filter: &str) -> bool {
    if filter.is_empty() {
        return true;
    }
    let filter = filter.to_lowercase();
    entry.name.to_lowercase().contains(&filter)
        || entry.id.to_lowercase().contains(&filter)
        || entry.entry_type.to_lowercase().contains(&filter)
        || entry.tags.iter().any(|t| t.to_lowercase().contains(&filter))
}

fn entry_row<'a>(app: &'a App, entry: &'a BrainEntry) -> Element<'a, Message> {
    let s = &app.scheme;
    let is_selected = app.brain_view.selected.as_deref() == Some(entry.id.as_str());
    let id = entry.id.clone();

    button(
        column![
            text(&entry.name).size(12).color(s.text_primary),
            text(&entry.id).size(10).color(s.text_muted),
        ]
        .spacing(2),
    )
    .on_press(Message::BrainSelectEntry(id))
    .width(Length::Fill)
    .style(move |_theme, status| button::Style {
        background: Some(Background::Color(if is_selected {
            s.bg_elevated
        } else {
            match status {
                button::Status::Hovered => s.bg_elevated,
                _ => s.bg_surface,
            }
        })),
        text_color: s.text_primary,
        border: Border { color: Color::TRANSPARENT, width: 0.0, radius: 0.0.into() },
        ..Default::default()
    })
    .padding([8, 12])
    .into()
}

fn section<'a>(app: &'a App, entry_type: &str, entries: Vec<&'a BrainEntry>) -> Element<'a, Message> {
    let s = &app.scheme;

    let heading = container(
        text(format!("{entry_type} ({})", entries.len())).size(10).color(s.text_muted),
    )
    .padding([6, 12])
    .width(Length::Fill)
    .style(move |_theme| container::Style {
        background: Some(Background::Color(s.bg_elevated)),
        ..Default::default()
    });

    let rows: Vec<Element<Message>> = entries.iter().map(|e| entry_row(app, e)).collect();

    column(std::iter::once(heading.into()).chain(rows)).into()
}

/// Full-screen master-detail view for browsing the brain's Markdown knowledge store.
pub fn brain_panel(app: &App) -> Element<'_, Message> {
    let s = &app.scheme;

    let back_btn = button(text("← Fleet").size(12).color(s.text_secondary))
        .on_press(Message::NavigateFleet { scope: None })
        .style(|_t, _s| button::Style {
            background: None,
            border: Border::default(),
            ..Default::default()
        })
        .padding([4, 0]);

    let reindex_btn = button(text("Reindex").size(11).color(s.accent))
        .on_press(Message::BrainReindex)
        .style(move |_theme, _status| button::Style {
            background: None,
            text_color: s.accent,
            border: Border { color: s.accent, width: 1.0, radius: 4.0.into() },
            ..Default::default()
        })
        .padding([2, 8]);

    let header = container(
        row![
            back_btn,
            Space::new(16, 0),
            text("Brain").size(16).color(s.text_primary),
            Space::new(Length::Fill, 0),
            text(format!("{} entries", app.brain_view.entries.len())).size(12).color(s.text_muted),
            Space::new(12, 0),
            reindex_btn,
        ]
        .align_y(Alignment::Center),
    )
    .padding([12, 20])
    .width(Length::Fill)
    .style(move |_theme| container::Style {
        background: Some(Background::Color(s.bg_base)),
        border: Border { color: s.border, width: 0.0, radius: 0.0.into() },
        ..Default::default()
    });

    let filter_input = container(
        text_input("Filter entries...", &app.brain_view.filter)
            .on_input(Message::BrainFilterQuery)
            .padding([4, 8])
            .size(12)
            .width(Length::Fill),
    )
    .padding([8, 20])
    .width(Length::Fill)
    .style(move |_theme| container::Style {
        background: Some(Background::Color(s.bg_base)),
        border: Border { color: s.border, width: 0.0, radius: 0.0.into() },
        ..Default::default()
    });

    let filtered: Vec<&BrainEntry> = app
        .brain_view
        .entries
        .iter()
        .filter(|e| matches_filter(e, &app.brain_view.filter))
        .collect();

    let list: Element<Message> = if filtered.is_empty() {
        container(
            text(if app.brain_view.entries.is_empty() {
                "No entries yet. Write a Markdown file into the brain directory, then Reindex."
            } else {
                "No entries match this filter."
            })
            .size(13)
            .color(s.text_muted),
        )
        .padding([40, 20])
        .width(Length::Fill)
        .into()
    } else {
        let mut by_type: Vec<(&str, Vec<&BrainEntry>)> = Vec::new();
        for entry in &filtered {
            match by_type.iter_mut().find(|(t, _)| *t == entry.entry_type.as_str()) {
                Some((_, v)) => v.push(entry),
                None => by_type.push((entry.entry_type.as_str(), vec![*entry])),
            }
        }
        by_type.sort_by_key(|(t, _)| t.to_string());

        let sections: Vec<Element<Message>> =
            by_type.into_iter().map(|(t, entries)| section(app, t, entries)).collect();

        scrollable(column(sections)).height(Length::Fill).into()
    };

    let left = container(list)
        .width(Length::Fixed(280.0))
        .height(Length::Fill)
        .style(move |_theme| container::Style {
            background: Some(Background::Color(s.bg_surface)),
            border: Border { color: s.border, width: 1.0, radius: 0.0.into() },
            ..Default::default()
        });

    let detail = detail_pane(app);

    column![
        header,
        filter_input,
        row![left, detail].height(Length::Fill).width(Length::Fill),
    ]
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

fn detail_pane(app: &App) -> Element<'_, Message> {
    let s = &app.scheme;

    let selected = app
        .brain_view
        .selected
        .as_ref()
        .and_then(|id| app.brain_view.entries.iter().find(|e| &e.id == id));

    let content: Element<Message> = match selected {
        None => container(
            text("Select an entry to view its contents.").size(13).color(s.text_muted),
        )
        .padding(20)
        .into(),
        Some(entry) => {
            let mut meta_rows: Vec<Element<Message>> = vec![
                row![
                    text("Type").size(10).color(s.text_muted).width(Length::Fixed(80.0)),
                    text(&entry.entry_type).size(12).color(s.text_primary),
                ]
                .into(),
                row![
                    text("Path").size(10).color(s.text_muted).width(Length::Fixed(80.0)),
                    text(&entry.id).size(12).color(s.text_secondary),
                ]
                .into(),
            ];
            if let Some(updated) = &entry.updated {
                meta_rows.push(
                    row![
                        text("Updated").size(10).color(s.text_muted).width(Length::Fixed(80.0)),
                        text(updated).size(12).color(s.text_secondary),
                    ]
                    .into(),
                );
            }
            if !entry.tags.is_empty() {
                meta_rows.push(
                    row![
                        text("Tags").size(10).color(s.text_muted).width(Length::Fixed(80.0)),
                        text(entry.tags.join(", ")).size(12).color(s.text_secondary),
                    ]
                    .into(),
                );
            }
            if !entry.repos.is_empty() {
                meta_rows.push(
                    row![
                        text("Repos").size(10).color(s.text_muted).width(Length::Fixed(80.0)),
                        text(entry.repos.join(", ")).size(12).color(s.text_secondary),
                    ]
                    .into(),
                );
            }

            column![
                text(&entry.name).size(18).color(s.text_primary),
                Space::new(0, 8),
                column(meta_rows).spacing(4),
                Space::new(0, 16),
                container(Space::new(Length::Fill, 1)).width(Length::Fill).style(move |_theme| {
                    container::Style { background: Some(Background::Color(s.border)), ..Default::default() }
                }),
                Space::new(0, 16),
                text(&entry.body).size(12).color(s.text_primary).font(iced::Font::MONOSPACE),
            ]
            .padding(20)
            .into()
        }
    };

    container(scrollable(content).height(Length::Fill))
        .width(Length::Fill)
        .height(Length::Fill)
        .style(move |_theme| container::Style {
            background: Some(Background::Color(s.bg_base)),
            ..Default::default()
        })
        .into()
}
