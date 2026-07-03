use iced::{
    widget::{button, column, container, pick_list, row, scrollable, text, text_input, Space},
    Alignment, Background, Border, Color, Element, Length,
};

use crate::app::{App, BrainMode, Message};
use crate::style::{
    dotted_rule, hard_shadow, heavy_frame, hline, micro_label, shadow_alpha, stamp, vline, MONO,
    MONO_MEDIUM, SERIF, SERIF_ITALIC, SERIF_MEDIUM,
};
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

    let board = container(super::brain_pinboard::pinboard_canvas(app))
        .width(Length::Fill)
        .height(Length::Fill)
        .style(move |_theme| crate::style::heavy_frame(s));

    row![rail, board]
        .spacing(16)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(iced::Padding { top: 16.0, right: 28.0, bottom: 22.0, left: 28.0 })
        .into()
}

/// Catalogue mode: 272px card-catalogue drawers on the left, markdown
/// reading pane on the right (mockup `.cat-body`/`.drawers`/`.reading`).
fn catalogue_body(app: &App) -> Element<'_, Message> {
    row![drawers_rail(app), reading_pane(app)]
        .spacing(16)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(iced::Padding { top: 16.0, right: 28.0, bottom: 22.0, left: 28.0 })
        .into()
}

/// The drawers rail: volume plate atop one drawer per category (taxonomy
/// order), each expandable to a sorted list of matching entries.
fn drawers_rail(app: &App) -> Element<'_, Message> {
    let s = &app.scheme;

    let filtered_entries: Vec<BrainEntry> = app
        .brain_view
        .entries
        .iter()
        .filter(|e| matches_filter(e, &app.brain_view.filter))
        .cloned()
        .collect();

    let body: Element<Message> = if filtered_entries.is_empty() {
        container(
            text(if app.brain_view.entries.is_empty() {
                "No entries yet. Write a Markdown file into the brain directory, then Reindex."
            } else {
                "No entries match this filter."
            })
            .size(12)
            .font(SERIF_ITALIC)
            .color(s.faint),
        )
        .padding([24, 16])
        .into()
    } else {
        let drawers: Vec<Element<Message>> = categories(&filtered_entries)
            .into_iter()
            .map(|(cat, count)| drawer(app, &cat, count, &filtered_entries))
            .collect();
        column(drawers).into()
    };

    container(column![volume_plate(app), scrollable(body).height(Length::Fill)])
        .width(Length::Fixed(272.0))
        .height(Length::Fill)
        .style(move |_theme| heavy_frame(s))
        .into()
}

/// One drawer: header (caret, category dot, name, count, pull) plus, when
/// open, its sorted entries (mockup `.drawer`/`.drawer-h`/`.dentry`).
fn drawer<'a>(
    app: &'a App,
    cat: &str,
    count: usize,
    filtered_entries: &[BrainEntry],
) -> Element<'a, Message> {
    let s = &app.scheme;
    let color = category_color(s, cat);
    let is_open = app.brain_view.open_drawers.contains(cat);
    let caret = if is_open { "▾" } else { "▸" };

    let pull = container(Space::new(22, 7)).style(move |_theme| container::Style {
        border: Border { color: s.rule_dark, width: 1.5, radius: 4.0.into() },
        ..Default::default()
    });

    let header = button(
        row![
            text(caret).size(9).color(s.faint).width(Length::Fixed(10.0)),
            text("●").size(9).color(color),
            text(cat.to_string()).size(15).font(SERIF),
            Space::new(Length::Fill, 0),
            text(count.to_string()).size(9.5).font(MONO).color(s.faint),
            pull,
        ]
        .spacing(10)
        .align_y(Alignment::Center),
    )
    .on_press(Message::BrainToggleDrawer(cat.to_string()))
    .width(Length::Fill)
    .padding([10, 15])
    .style(move |_theme, status| {
        let hovered = matches!(status, button::Status::Hovered);
        button::Style {
            background: Some(Background::Color(if hovered { s.paper } else { Color::TRANSPARENT })),
            text_color: if hovered { s.ink } else { color },
            border: Border::default(),
            ..Default::default()
        }
    });

    let mut children: Vec<Element<Message>> = vec![header.into()];

    if is_open {
        let mut entries: Vec<&BrainEntry> =
            filtered_entries.iter().filter(|e| e.entry_type == cat).collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        children.extend(entries.into_iter().map(|e| dentry_row(app, e)));
    }

    children.push(hline(s.rule, 1.0));

    column(children).into()
}

/// One entry in an open drawer. Selected = accent 3px left bar + `card` bg
/// + `MONO_MEDIUM`; hovered = `paper_2` bg (visibly distinct from the
/// transparent resting state) — an old regression collapsed hover to a
/// no-op, so this must render differently in all three states.
fn dentry_row<'a>(app: &'a App, entry: &BrainEntry) -> Element<'a, Message> {
    let s = &app.scheme;
    let is_selected = app.brain_view.selected.as_deref() == Some(entry.id.as_str());
    let id = entry.id.clone();
    let name = entry.name.clone();
    let updated: String = entry.updated.as_deref().unwrap_or("").chars().take(10).collect();
    let bar_color = if is_selected { s.accent } else { Color::TRANSPARENT };

    button(
        row![
            vline(bar_color, 3.0),
            container(
                row![
                    text(name).size(10.5).font(if is_selected { MONO_MEDIUM } else { MONO }),
                    Space::new(Length::Fill, 0),
                    text(updated).size(8.5).font(MONO).color(s.faint),
                ]
                .align_y(Alignment::Center),
            )
            .padding(iced::Padding { top: 4.0, right: 16.0, bottom: 4.0, left: 44.0 })
            .width(Length::Fill),
        ]
        .height(Length::Fixed(22.0)),
    )
    .on_press(Message::BrainSelectEntry(id))
    .width(Length::Fill)
    .padding(0)
    .style(move |_theme, status| {
        let hovered = matches!(status, button::Status::Hovered);
        button::Style {
            background: Some(Background::Color(if is_selected {
                s.card
            } else if hovered {
                s.paper_2
            } else {
                Color::TRANSPARENT
            })),
            text_color: if is_selected || hovered { s.ink } else { s.ink_2 },
            border: Border::default(),
            ..Default::default()
        }
    })
    .into()
}

/// One `dt`/`dd` row of the reading pane's frontmatter description list.
fn fm_row<'a>(s: &ColorScheme, key: &str, value: String) -> Element<'a, Message> {
    row![
        container(micro_label(key, s.faint)).width(Length::Fixed(88.0)),
        text(value).size(11).font(MONO).color(s.ink_2),
    ]
    .align_y(Alignment::Start)
    .into()
}

/// A "cited by" backlink chip: mono id in a rule-bordered pill; press
/// navigates to that entry.
fn backlink_chip<'a>(s: &'a ColorScheme, entry: &'a BrainEntry) -> Element<'a, Message> {
    let id = entry.id.clone();
    button(text(entry.id.clone()).size(10.5).font(MONO))
        .on_press(Message::BrainSelectEntry(id))
        .padding([3, 9])
        .style(move |_theme, status| {
            let hovered = matches!(status, button::Status::Hovered);
            button::Style {
                background: None,
                text_color: if hovered { s.ink } else { s.ink_2 },
                border: Border {
                    color: if hovered { s.ink } else { s.rule_dark },
                    width: 1.0,
                    radius: 2.0.into(),
                },
                ..Default::default()
            }
        })
        .into()
}

/// The reading pane: crumb, title + type stamp, frontmatter dl, rendered
/// markdown body (wikilinks clickable), and a "cited by" backlinks section.
/// Empty state when nothing is selected (mockup `.reading`).
fn reading_pane(app: &App) -> Element<'_, Message> {
    let s = &app.scheme;

    let selected = app
        .brain_view
        .selected
        .as_ref()
        .and_then(|id| app.brain_view.entries.iter().find(|e| &e.id == id));

    let content: Element<Message> = match selected {
        None => container(text("Nothing pinned tonight.").size(15).font(SERIF_ITALIC).color(s.faint))
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into(),
        Some(e) => {
            let crumb = text(format!(
                "brain / {} / {}",
                e.entry_type,
                e.id.rsplit('/').next().unwrap_or(&e.id)
            ))
            .size(9.5)
            .font(MONO)
            .color(s.faint);

            let title = row![
                text(&e.name).size(32).font(SERIF_MEDIUM).color(s.ink),
                Space::new(14, 0),
                stamp(&e.entry_type, category_color(s, &e.entry_type)),
            ]
            .align_y(Alignment::Center);

            let mut fm_rows: Vec<Element<Message>> = vec![fm_row(s, "type", e.entry_type.clone())];
            if !e.tags.is_empty() {
                fm_rows.push(fm_row(s, "tags", e.tags.join(", ")));
            }
            if !e.repos.is_empty() {
                fm_rows.push(fm_row(s, "repos", e.repos.join(", ")));
            }
            if let Some(updated) = &e.updated {
                fm_rows.push(fm_row(s, "updated", updated.clone()));
            }
            let frontmatter = column![
                hline(s.ink, 2.0),
                column(fm_rows).spacing(6).padding([10, 2]),
                hline(s.rule_dark, 1.0),
            ];

            let body = container(
                iced::widget::markdown::view(
                    &app.brain_view.markdown,
                    iced::widget::markdown::Settings::default(),
                    iced::widget::markdown::Style::from_palette(app.scheme.iced_theme().palette()),
                )
                .map(Message::BrainLinkClicked),
            )
            .max_width(640.0);

            let backs = backlinks_for(&app.brain_view.entries, e);
            let backlinks: Element<Message> = if backs.is_empty() {
                Space::new(0, 0).into()
            } else {
                let chips: Vec<Element<Message>> =
                    backs.iter().map(|b| backlink_chip(s, b)).collect();
                column![
                    Space::new(0, 22),
                    dotted_rule(s.rule_dark),
                    Space::new(0, 12),
                    micro_label(&format!("Cited by — {} specimens", backs.len()), s.faint).size(9.0),
                    Space::new(0, 8),
                    row(chips).spacing(6).wrap(),
                ]
                .into()
            };

            column![
                crumb,
                Space::new(0, 14),
                title,
                Space::new(0, 16),
                frontmatter,
                Space::new(0, 18),
                body,
                backlinks,
            ]
            .into()
        }
    };

    container(scrollable(content).height(Length::Fill))
        .width(Length::Fill)
        .height(Length::Fill)
        .padding([28, 36])
        .style(move |_theme| heavy_frame(s))
        .into()
}
