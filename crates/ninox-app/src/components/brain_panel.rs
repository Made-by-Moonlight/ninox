use iced::{
    widget::{button, column, container, pick_list, row, scrollable, text, text_input, Space},
    Alignment, Background, Border, Color, Element, Length,
};

use crate::app::{App, BrainMode, Message};
use crate::style::{hard_shadow, hline, micro_label, shadow_alpha, MONO, SERIF, SERIF_ITALIC};
use crate::theme::ColorScheme;
use ninox_core::BrainEntry;

// ---------------------------------------------------------------------------
// Pure helpers (wikilinks, backlinks, categories) — TDD'd in `mod tests` below.
// ---------------------------------------------------------------------------

/// Order per spec §1 "brain category colors".
pub const TAXONOMY: &[&str] = &[
    "repos", "symbols", "concepts", "patterns",
    "decisions", "architecture", "relationships", "errors",
];

pub fn category_color(s: &ColorScheme, ty: &str) -> Color {
    match ty {
        "repos"         => s.status_pr_open,
        "symbols"       => s.status_working,
        "concepts"      => s.status_review,
        "architecture"  => s.status_mergeable,
        "patterns"      => s.cat_pattern,
        "decisions"     => s.cat_decision,
        "relationships" => s.cat_relationship,
        "errors"        => s.cat_error,
        _               => s.faint,
    }
}

/// `[[target]]` occurrences, in order.
#[allow(dead_code)] // TODO(field-notes): consumed by the Task 12 reading pane (backlinks)
pub fn extract_wikilinks(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find("[[") {
        let after = &rest[start + 2..];
        match after.find("]]") {
            Some(end) => {
                let target = after[..end].trim();
                if !target.is_empty() { out.push(target.to_string()); }
                rest = &after[end + 2..];
            }
            None => break,
        }
    }
    out
}

/// `[[x]]` → `[x](ninox-brain:x)` so the markdown widget renders clickable links.
pub fn preprocess_wikilinks(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(start) = rest.find("[[") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("]]") {
            Some(end) => {
                let target = after[..end].trim();
                out.push_str(&format!("[{target}](ninox-brain:{target})"));
                rest = &after[end + 2..];
            }
            None => { out.push_str("[["); rest = after; }
        }
    }
    out.push_str(rest);
    out
}

/// Does `link` refer to `entry`? Match on name, full id, or id stem.
fn link_matches(link: &str, entry: &BrainEntry) -> bool {
    let stem = entry.id.rsplit('/').next().unwrap_or(&entry.id).trim_end_matches(".md");
    let link_stem = link.rsplit('/').next().unwrap_or(link);
    link == entry.name || link == entry.id || link_stem == stem || link_stem == entry.name
}

/// Resolve a clicked wikilink to an entry id.
pub fn resolve_link<'a>(entries: &'a [BrainEntry], link: &str) -> Option<&'a BrainEntry> {
    entries.iter().find(|e| link_matches(link, e))
}

/// Entries whose bodies wikilink to `target` ("Cited by").
#[allow(dead_code)] // TODO(field-notes): consumed by the Task 12 reading pane (backlinks)
pub fn backlinks_for<'a>(entries: &'a [BrainEntry], target: &BrainEntry) -> Vec<&'a BrainEntry> {
    entries.iter()
        .filter(|e| e.id != target.id)
        .filter(|e| extract_wikilinks(&e.body).iter().any(|l| link_matches(l, target)))
        .collect()
}

/// (category, count), taxonomy order first, then alphabetic for unknown types.
pub fn categories(entries: &[BrainEntry]) -> Vec<(String, usize)> {
    let mut counts: std::collections::HashMap<&str, usize> = Default::default();
    for e in entries { *counts.entry(e.entry_type.as_str()).or_default() += 1; }
    let mut out: Vec<(String, usize)> = Vec::new();
    for t in TAXONOMY {
        if let Some(n) = counts.remove(t) { out.push(((*t).to_string(), n)); }
    }
    let mut rest: Vec<_> = counts.into_iter().collect();
    rest.sort_by(|a, b| a.0.cmp(b.0));
    out.extend(rest.into_iter().map(|(t, n)| (t.to_string(), n)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, ty: &str, name: &str, body: &str) -> ninox_core::brain::BrainEntry {
        ninox_core::brain::BrainEntry {
            id: id.into(), entry_type: ty.into(), name: name.into(),
            tags: vec![], repos: vec![], updated: None, body: body.into(),
        }
    }

    #[test]
    fn extracts_wikilinks() {
        assert_eq!(
            extract_wikilinks("passes [[frame-alignment]] before [[errors/scrollback-dup]] runs"),
            vec!["frame-alignment".to_string(), "errors/scrollback-dup".to_string()]
        );
        assert!(extract_wikilinks("no links [here] or [[unclosed").is_empty());
    }

    #[test]
    fn preprocesses_wikilinks_to_brain_urls() {
        assert_eq!(
            preprocess_wikilinks("see [[frame-alignment]]."),
            "see [frame-alignment](ninox-brain:frame-alignment)."
        );
    }

    #[test]
    fn backlinks_match_by_name_or_id_stem() {
        let a = entry("symbols/scrollback-buffer.md", "symbols", "ScrollbackBuffer", "…");
        let b = entry("concepts/frame-alignment.md", "concepts", "frame-alignment",
                      "owned by [[ScrollbackBuffer]]");
        let c = entry("errors/scrollback-dup.md", "errors", "scrollback-dup", "unrelated");
        let all = vec![a.clone(), b.clone(), c];
        let backs = backlinks_for(&all, &a);
        assert_eq!(backs.len(), 1);
        assert_eq!(backs[0].id, b.id);
    }

    #[test]
    fn categories_are_counted_and_ordered_by_taxonomy() {
        let all = vec![
            entry("symbols/a.md", "symbols", "a", ""),
            entry("symbols/b.md", "symbols", "b", ""),
            entry("errors/x.md", "errors", "x", ""),
        ];
        let cats = categories(&all);
        assert_eq!(cats, vec![("symbols".to_string(), 2), ("errors".to_string(), 1)]);
    }
}

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
            text(&entry.name).size(12).color(s.ink),
            text(&entry.id).size(10).color(s.faint),
        ]
        .spacing(2),
    )
    .on_press(Message::BrainSelectEntry(id))
    .width(Length::Fill)
    .style(move |_theme, status| button::Style {
        background: Some(Background::Color(if is_selected {
            s.card
        } else {
            match status {
                button::Status::Hovered => s.card,
                _ => s.card,
            }
        })),
        text_color: s.ink,
        border: Border { color: Color::TRANSPARENT, width: 0.0, radius: 0.0.into() },
        ..Default::default()
    })
    .padding([8, 12])
    .into()
}

fn section<'a>(app: &'a App, entry_type: &str, entries: Vec<&'a BrainEntry>) -> Element<'a, Message> {
    let s = &app.scheme;

    let heading = container(
        text(format!("{entry_type} ({})", entries.len())).size(10).color(s.faint),
    )
    .padding([6, 12])
    .width(Length::Fill)
    .style(move |_theme| container::Style {
        background: Some(Background::Color(s.card)),
        ..Default::default()
    });

    let rows: Vec<Element<Message>> = entries.iter().map(|e| entry_row(app, e)).collect();

    column(std::iter::once(heading.into()).chain(rows)).into()
}

/// One segment of the ✦/☰ mode toggle: micro-label typography, ink fill when active.
fn mode_segment<'a>(s: &'a ColorScheme, label: &str, mode: BrainMode, active: bool) -> Element<'a, Message> {
    button(micro_label(label, if active { s.card } else { s.ink_2 }))
        .on_press(Message::BrainSetMode(mode))
        .padding([5, 14])
        .style(move |_theme, status| button::Style {
            background: Some(Background::Color(if active { s.ink } else { Color::TRANSPARENT })),
            text_color: if active {
                s.card
            } else if status == button::Status::Hovered {
                s.ink
            } else {
                s.ink_2
            },
            border: Border::default(),
            ..Default::default()
        })
        .into()
}

/// Joined ✦ PINBOARD / ☰ CATALOGUE segments in a 1.5px ink frame with a hard
/// 2×2 shadow (mockup `.brain-mode`).
fn mode_toggle(app: &App) -> Element<'_, Message> {
    let s = &app.scheme;
    let (card_a, _, _) = shadow_alpha(s);
    container(
        row![
            mode_segment(s, "✦ Pinboard", BrainMode::Pinboard, app.brain_view.mode == BrainMode::Pinboard),
            crate::style::vline(s.ink, 1.5),
            mode_segment(s, "☰ Catalogue", BrainMode::Catalogue, app.brain_view.mode == BrainMode::Catalogue),
        ]
        .height(Length::Shrink),
    )
    .style(move |_theme| container::Style {
        border: Border { color: s.ink, width: 1.5, radius: 2.0.into() },
        shadow: hard_shadow(s, 2.0, 2.0, card_a),
        ..Default::default()
    })
    .into()
}

/// Underlined "⌕ search specimens…" field wired to the existing filter query.
fn search_field(app: &App) -> Element<'_, Message> {
    let s = &app.scheme;
    let input = text_input("search specimens…", &app.brain_view.filter)
        .on_input(Message::BrainFilterQuery)
        .size(12)
        .padding([4, 2])
        .style(move |_t, _st| text_input::Style {
            background: Background::Color(Color::TRANSPARENT),
            border: Border::default(),
            icon: s.faint,
            placeholder: s.faint,
            value: s.ink,
            selection: Color { a: 0.35, ..s.accent },
        });
    column![
        row![text("⌕").size(13).color(s.faint), Space::new(6, 0), input].align_y(Alignment::Center),
        hline(s.ink, 1.5),
    ]
    .width(Length::Fixed(230.0))
    .into()
}

/// Folio header: "The *brain*", SPECIMENS count, ✦/☰ mode toggle, underlined
/// search, and a micro-label Reindex affordance.
fn folio(app: &App) -> Element<'_, Message> {
    let s = &app.scheme;
    let reindex_btn = button(micro_label("Reindex", s.ink_2))
        .on_press(Message::BrainReindex)
        .padding([4, 10])
        .style(move |_theme, status| button::Style {
            background: None,
            text_color: if status == button::Status::Hovered { s.ink } else { s.ink_2 },
            border: Border { color: s.rule_dark, width: 1.0, radius: 2.0.into() },
            ..Default::default()
        });

    row![
        text("The ").size(34).font(SERIF).color(s.ink),
        text("brain").size(34).font(SERIF_ITALIC).color(s.ink),
        Space::new(18, 0),
        text(format!("SPECIMENS — {}", app.brain_view.entries.len()))
            .size(10.5).font(MONO).color(s.faint),
        Space::new(18, 0),
        mode_toggle(app),
        Space::new(Length::Fill, 0),
        search_field(app),
        Space::new(18, 0),
        reindex_btn,
    ]
    .align_y(Alignment::End)
    .padding(iced::Padding { top: 22.0, right: 28.0, bottom: 8.0, left: 28.0 })
    .into()
}

/// Volume plate — which catalogue is open. Lives at the head of the
/// rail/drawers, never the folio (mockup `.volplate`): a paper-2 strip with a
/// CATALOGUE micro-label, mono `⌂ name`, faint ▾, and a 1px rule-dark bottom
/// edge. Becomes a pick_list when more than one catalogue is configured.
fn volume_plate(app: &App) -> Element<'_, Message> {
    let s = &app.scheme;
    let name = app
        .catalogues
        .get(app.active_catalogue)
        .map(|c| c.name.as_str())
        .unwrap_or("default");

    let switcher: Element<Message> = if app.catalogues.len() > 1 {
        let names: Vec<String> = app.catalogues.iter().map(|c| format!("⌂ {}", c.name)).collect();
        let selected = names.get(app.active_catalogue).cloned();
        let lookup = names.clone();
        pick_list(names, selected, move |chosen| {
            let idx = lookup.iter().position(|n| n == &chosen).unwrap_or(0);
            Message::BrainSwitchCatalogue(idx)
        })
        .font(MONO)
        .text_size(11)
        .padding([2, 6])
        .style(move |_theme, status| pick_list::Style {
            text_color: if status == pick_list::Status::Hovered { s.accent } else { s.ink },
            placeholder_color: s.faint,
            handle_color: s.faint,
            background: Background::Color(Color::TRANSPARENT),
            border: Border::default(),
        })
        .into()
    } else {
        // Single catalogue: the plate renders inert.
        row![
            text(format!("⌂ {name}")).size(11).font(MONO).color(s.ink),
            Space::new(8, 0),
            text("▾").size(10).color(s.faint),
        ]
        .align_y(Alignment::Center)
        .into()
    };

    column![
        container(
            row![
                micro_label("Catalogue", s.faint).size(8.5),
                Space::new(Length::Fill, 0),
                switcher,
            ]
            .align_y(Alignment::Center),
        )
        .padding([8, 14])
        .width(Length::Fill)
        .style(move |_theme| container::Style {
            background: Some(Background::Color(s.paper_2)),
            ..Default::default()
        }),
        hline(s.rule_dark, 1.0),
    ]
    .width(Length::Fill)
    .into()
}

/// Full-screen brain view: Field Notes folio over a mode-dependent body.
pub fn brain_panel(app: &App) -> Element<'_, Message> {
    let body: Element<Message> = match app.brain_view.mode {
        BrainMode::Catalogue => catalogue_body(app),
        BrainMode::Pinboard  => pinboard_body(app),
    };

    column![folio(app), body]
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Pinboard placeholder: taxonomy rail (volume plate + category counts)
/// beside an empty heavy frame — the specimen-board canvas lands in Task 13.
fn pinboard_body(app: &App) -> Element<'_, Message> {
    let s = &app.scheme;
    let (card_a, _, _) = shadow_alpha(s);

    let cat_rows: Vec<Element<Message>> = categories(&app.brain_view.entries)
        .into_iter()
        .map(|(ty, n)| {
            let color = category_color(s, &ty);
            container(
                row![
                    text("●").size(9).color(color),
                    Space::new(10, 0),
                    text(ty).size(12).color(s.ink_2),
                    Space::new(Length::Fill, 0),
                    text(n.to_string()).size(9.5).font(MONO).color(s.faint),
                ]
                .align_y(Alignment::Center),
            )
            .padding([3, 16])
            .width(Length::Fill)
            .into()
        })
        .collect();

    let rail = container(column![
        volume_plate(app),
        container(text("brain/ — taxonomy").size(14).font(SERIF_ITALIC).color(s.faint))
            .padding([10, 16]),
        scrollable(column(cat_rows)).height(Length::Fill),
    ])
    .width(Length::Fixed(215.0))
    .height(Length::Fill)
    .style(move |_theme| container::Style {
        background: Some(Background::Color(s.card)),
        border: Border { color: s.rule_dark, width: 1.0, radius: 2.0.into() },
        shadow: hard_shadow(s, 2.0, 3.0, card_a),
        ..Default::default()
    });

    let board = container(
        text("Nothing pinned tonight.").size(15).font(SERIF_ITALIC).color(s.faint),
    )
    .center_x(Length::Fill)
    .center_y(Length::Fill)
    .style(move |_theme| crate::style::heavy_frame(s));

    row![rail, board]
        .spacing(16)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(iced::Padding { top: 16.0, right: 28.0, bottom: 22.0, left: 28.0 })
        .into()
}

/// Catalogue mode: volume plate + the existing grouped list on the left,
/// reading pane on the right (drawer/reading-pane restyle lands in Task 12).
fn catalogue_body(app: &App) -> Element<'_, Message> {
    let s = &app.scheme;
    let (_, hero_a, _) = shadow_alpha(s);

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
            .color(s.faint),
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
        // Taxonomy order first, unknown types alphabetically after (mirrors
        // `categories()`).
        by_type.sort_by_key(|(t, _)| match TAXONOMY.iter().position(|x| x == t) {
            Some(i) => (0, format!("{i:03}")),
            None    => (1, t.to_string()),
        });

        let sections: Vec<Element<Message>> =
            by_type.into_iter().map(|(t, entries)| section(app, t, entries)).collect();

        scrollable(column(sections)).height(Length::Fill).into()
    };

    let left = container(column![volume_plate(app), list])
        .width(Length::Fixed(272.0))
        .height(Length::Fill)
        .style(move |_theme| container::Style {
            background: Some(Background::Color(s.card)),
            border: Border { color: s.ink, width: 2.0, radius: 3.0.into() },
            shadow: hard_shadow(s, 4.0, 5.0, hero_a),
            ..Default::default()
        });

    let detail = detail_pane(app);

    row![left, detail]
        .spacing(16)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(iced::Padding { top: 16.0, right: 28.0, bottom: 22.0, left: 28.0 })
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
            text("Select an entry to view its contents.").size(13).color(s.faint),
        )
        .padding(20)
        .into(),
        Some(entry) => {
            let mut meta_rows: Vec<Element<Message>> = vec![
                row![
                    text("Type").size(10).color(s.faint).width(Length::Fixed(80.0)),
                    text(&entry.entry_type).size(12).color(s.ink),
                ]
                .into(),
                row![
                    text("Path").size(10).color(s.faint).width(Length::Fixed(80.0)),
                    text(&entry.id).size(12).color(s.ink_2),
                ]
                .into(),
            ];
            if let Some(updated) = &entry.updated {
                meta_rows.push(
                    row![
                        text("Updated").size(10).color(s.faint).width(Length::Fixed(80.0)),
                        text(updated).size(12).color(s.ink_2),
                    ]
                    .into(),
                );
            }
            if !entry.tags.is_empty() {
                meta_rows.push(
                    row![
                        text("Tags").size(10).color(s.faint).width(Length::Fixed(80.0)),
                        text(entry.tags.join(", ")).size(12).color(s.ink_2),
                    ]
                    .into(),
                );
            }
            if !entry.repos.is_empty() {
                meta_rows.push(
                    row![
                        text("Repos").size(10).color(s.faint).width(Length::Fixed(80.0)),
                        text(entry.repos.join(", ")).size(12).color(s.ink_2),
                    ]
                    .into(),
                );
            }

            column![
                text(&entry.name).size(18).color(s.ink),
                Space::new(0, 8),
                column(meta_rows).spacing(4),
                Space::new(0, 16),
                container(Space::new(Length::Fill, 1)).width(Length::Fill).style(move |_theme| {
                    container::Style { background: Some(Background::Color(s.rule_dark)), ..Default::default() }
                }),
                Space::new(0, 16),
                text(&entry.body).size(12).color(s.ink).font(iced::Font::MONOSPACE),
            ]
            .padding(20)
            .into()
        }
    };

    container(scrollable(content).height(Length::Fill))
        .width(Length::Fill)
        .height(Length::Fill)
        .style(move |_theme| container::Style {
            background: Some(Background::Color(s.paper)),
            ..Default::default()
        })
        .into()
}
