//! The v1 Brain tab's List view (`Brain.dc.html`): toolbar, Kinds column,
//! entry list, reading pane. Also hosts `toolbar_wrap`, shared with the
//! Graph/Sources placeholders in `brain_panel` so the segmented control
//! navigates between all three modes before PR2/PR3 give Graph/Sources real
//! bodies.
//!
//! Kind normalization (`ninox_core::brain::normalize_kind`/`kind_label`)
//! fixes the brief's "decision"/"decisions" duplicate-row bug:
//! `BrainEntry::entry_type` comes from frontmatter `type:` (often singular)
//! or the containing directory name (always plural) — without normalizing,
//! the Kinds column showed both spellings as separate rows with split
//! counts. It lives in `ninox-core` (not here) so the TUI and CLI, which
//! still group/filter on the raw string, have a shared function to adopt
//! rather than a second GUI-only copy of the same rule.

use std::collections::HashSet;

use iced::{
    widget::{button, column, container, pick_list, row, scrollable, text, text_input, Space},
    Alignment, Background, Border, Color, Element, Length,
};

use ninox_core::brain::{
    count_for_kind, kind_label, kinds_sorted_by_count, normalize_kind, top_values, visible_entries,
};
use ninox_core::BrainEntry;

use super::shell::tokens;
use crate::app::{App, BrainSort, Message};

const KINDS_COLUMN_WIDTH: f32 = 228.0;
const ENTRY_LIST_WIDTH: f32 = 430.0;
const CHIP_LIMIT: usize = 4;

// ---------------------------------------------------------------------------
// Pure logic — filtering, sorting. Unit-tested below.
// ---------------------------------------------------------------------------

/// Kind-dot color (`Brain.dc.html`'s `.kind .dot` rows). GUI-only — the
/// kind/label normalization it colors lives in `ninox_core::brain`. Nine of
/// the twelve kinds get a distinct color; Project/Reference/Feedback are
/// deliberately the same muted gray as the fallback in `Brain.dc.html`'s own
/// mockup, so they're spelled out explicitly rather than left to fall
/// through — a future thirteenth kind lands on the same gray, but by
/// drifting into the wildcard arm rather than by design.
pub fn kind_color(key: &str) -> Color {
    match key {
        "decisions" => tokens::AMBER,
        "errors" => tokens::RED,
        "architecture" => tokens::GREEN,
        "repos" => tokens::BLUE,
        "patterns" => tokens::VIOLET,
        "relationships" => tokens::TEAL,
        "concepts" => tokens::PINK,
        "symbols" => tokens::CYAN,
        "hazards" => tokens::ORANGE,
        "project" | "reference" | "feedback" => tokens::DIM,
        _ => tokens::DIM,
    }
}

/// Count of distinct kinds, for the toolbar's "K kinds" — cheaper than
/// `kinds_sorted_by_count(entries).len()` (no per-kind label lookup or
/// sort) since the toolbar renders on every mode, including the Graph/
/// Sources placeholders that never need the sorted Kinds-column data.
fn distinct_kind_count(entries: &[BrainEntry]) -> usize {
    entries.iter().map(|e| normalize_kind(&e.entry_type)).collect::<HashSet<_>>().len()
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

pub(crate) fn mode_seg(app: &App) -> Element<'_, Message> {
    use crate::app::BrainMode;
    super::fleet_islands::seg_control(vec![
        ("List".to_string(), app.brain_view.mode == BrainMode::List, Message::BrainSetMode(BrainMode::List)),
        ("Graph".to_string(), app.brain_view.mode == BrainMode::Graph, Message::BrainSetMode(BrainMode::Graph)),
        ("Sources".to_string(), app.brain_view.mode == BrainMode::Sources, Message::BrainSetMode(BrainMode::Sources)),
    ])
}

/// Stable id for the search field's `text_input` — `Cmd+F` focuses it while
/// the Brain tab is active (see `app.rs`'s `RawKey` handling; the global
/// shortcut map is `/` ninox prompt, `Cmd+K` top-bar search, `Cmd+F` Brain
/// search).
pub fn search_input_id() -> iced::widget::text_input::Id {
    iced::widget::text_input::Id::new("brain-search")
}

fn search_field(app: &App) -> Element<'_, Message> {
    let input = text_input("Search", &app.brain_view.filter)
        .id(search_input_id())
        .on_input(Message::BrainFilterQuery)
        .size(12)
        .font(crate::style::SANS)
        .padding([6, 8])
        .style(|_t, _status| text_input::Style {
            background: Background::Color(tokens::PANEL2),
            border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
            icon: tokens::DIM,
            placeholder: tokens::DIM,
            value: tokens::TEXT,
            selection: tokens::TEAL,
        });
    container(
        row![
            text("⌕").size(13).font(crate::style::GLYPH).color(tokens::DIM),
            Space::new(6, 0),
            input,
        ]
        .align_y(Alignment::Center)
        .padding([0, 4]),
    )
    // `Fill` with a cap, not a flat `Fixed(300.0)`: this is the one toolbar
    // element meant to flex — everything else (the segmented control, the
    // catalogue select, Reindex) must keep its natural width even when a
    // long catalogue name widens the select, or the row has nowhere to
    // take the slack from and squeezes a button below the width its own
    // label needs (iced then wraps the label mid-word despite
    // `Wrapping::None`, since that only controls how text wraps, not
    // whether the surrounding row lets it keep its natural size).
    .width(Length::Fill)
    .max_width(300.0)
    .style(|_| container::Style {
        background: Some(Background::Color(tokens::PANEL2)),
        border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
        ..Default::default()
    })
    .into()
}

/// Rough characters-per-220px at the select's 11.5px font — `pick_list`
/// sizes itself to its *widest option*, not just the current selection, so
/// an untruncated long catalogue name widens the closed box regardless of
/// what's selected (same reasoning as `brain_graph`'s cluster-label
/// truncation; no real text measurement is available outside a `Renderer`).
const CATALOGUE_SELECT_MAX_CHARS: usize = 28;
const CATALOGUE_SELECT_MAX_WIDTH: f32 = 220.0;

fn truncate_catalogue_name(name: &str) -> String {
    if name.chars().count() <= CATALOGUE_SELECT_MAX_CHARS {
        name.to_string()
    } else {
        let truncated: String = name.chars().take(CATALOGUE_SELECT_MAX_CHARS).collect();
        format!("{}…", truncated.trim_end())
    }
}

/// The "All sources" pseudo-option's label — index 0 in `catalogue_select`'s
/// list, ahead of every real `CatalogueRef`.
const ALL_SOURCES: &str = "All sources";

fn catalogue_select(app: &App) -> Element<'_, Message> {
    // The select is meant to size to its content (short lists of short
    // names should stay compact) rather than always claiming
    // `CATALOGUE_SELECT_MAX_WIDTH` — truncating the option text bounds the
    // *content* width, and the `container::max_width` below is a hard
    // backstop in case the character-count estimate above runs long.
    let mut names: Vec<String> = vec![ALL_SOURCES.to_string()];
    names.extend(app.catalogues.iter().map(|c| truncate_catalogue_name(&c.name)));
    let selected = match app.active_catalogue {
        Some(idx) => names.get(idx + 1).cloned(),
        None => Some(ALL_SOURCES.to_string()),
    };
    let lookup = names.clone();
    let list = pick_list(names, selected, move |chosen| {
        let idx = lookup.iter().position(|n| n == &chosen).unwrap_or(0);
        Message::BrainSwitchCatalogue(idx.checked_sub(1))
    })
    .font(crate::style::SANS)
    .text_size(11.5)
    .padding([5, 8])
    .width(Length::Shrink)
    .style(|_t, _status| pick_list::Style {
        text_color: tokens::TEXT,
        placeholder_color: tokens::DIM,
        handle_color: tokens::DIM,
        background: Background::Color(tokens::PANEL2),
        border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
    });
    let list = container(list).max_width(CATALOGUE_SELECT_MAX_WIDTH);
    // `BrainSources.dc.html`'s "+ Add source" becomes the primary way to add
    // a catalogue once PR3 lands — this "+" is a temporary bridge so adding
    // one doesn't regress to unreachable in the meantime (`catalogue_modal`
    // itself is untouched, just its only GUI entry point moved here from
    // the removed Field-Notes volume plate).
    let add_button = button(text("+").size(13).font(crate::style::SANS).color(tokens::DIM))
        .on_press(Message::CatalogueModalOpen)
        .padding([2, 7])
        .style(|_t, status| button::Style {
            background: None,
            text_color: if matches!(status, button::Status::Hovered) { tokens::TEAL } else { tokens::DIM },
            border: Border::default(),
            ..Default::default()
        });

    row![
        text("Catalogue").size(11.5).font(crate::style::SANS).color(tokens::DIM),
        Space::new(6, 0),
        list,
        add_button,
    ]
    .align_y(Alignment::Center)
    .into()
}

fn reindex_button<'a>() -> Element<'a, Message> {
    button(
        text("Reindex")
            .size(12)
            .font(crate::style::SANS)
            .color(tokens::TEXT)
            .wrapping(iced::widget::text::Wrapping::None),
    )
    .on_press(Message::BrainReindex)
    .padding([6, 12])
    .width(Length::Shrink)
        .style(|_t, status| button::Style {
            background: Some(Background::Color(if matches!(status, button::Status::Hovered) {
                tokens::PANEL2
            } else {
                tokens::PANEL
            })),
            border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
            text_color: tokens::TEXT,
            ..Default::default()
        })
        .into()
}

/// A 30px toolbar action button — `BrainSources.dc.html`'s "Sync all" /
/// "+ Add source" pair, sized so they match whatever text size surrounds
/// them regardless of each button's own natural (unconstrained) height.
fn toolbar_button<'a>(label: &'a str, msg: Message, primary: bool) -> Element<'a, Message> {
    // Symmetric padding with no explicit `height` — like `reindex_button`
    // below — rather than a fixed height with zero vertical padding, which
    // left the label pinned to the button's top edge (`Button` has no
    // `align_y`/`center_y` of its own to correct for that).
    button(
        text(label)
            .size(11.5)
            .font(crate::style::SANS)
            .color(if primary { tokens::BG } else { tokens::TEXT })
            .wrapping(iced::widget::text::Wrapping::None),
    )
    .on_press(msg)
    .padding([7, 12])
    .width(Length::Shrink)
    .style(move |_t, _s| {
            if primary {
                button::Style { background: Some(Background::Color(tokens::TEAL)), border: Border { radius: 6.0.into(), ..Default::default() }, text_color: tokens::BG, ..Default::default() }
            } else {
                button::Style {
                    background: Some(Background::Color(tokens::PANEL2)),
                    border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
                    text_color: tokens::TEXT,
                    ..Default::default()
                }
            }
        })
        .into()
}

fn toolbar(app: &App) -> Element<'_, Message> {
    use crate::app::BrainMode;

    let summary = if app.brain_view.mode == BrainMode::Sources {
        let total = app.catalogues.len();
        let needs_attention = app.source_status.values().filter(|s| s.last_error.is_some()).count();
        if needs_attention > 0 {
            format!("{total} sources · {needs_attention} need{} attention", if needs_attention == 1 { "s" } else { "" })
        } else {
            format!("{total} sources")
        }
    } else {
        let ago = app.brain_view.last_indexed.map(|t| ninox_ui_shared::brain_sources::format_ago(t.elapsed())).unwrap_or_else(|| "never".to_string());
        format!("{} entries · {} kinds · indexed {}", app.brain_view.entries.len(), distinct_kind_count(&app.brain_view.entries), ago)
    };

    let mut controls = row![
        text("Brain").size(13).font(crate::style::SANS).color(tokens::TEXT),
        text(summary).size(11.5).font(crate::style::SANS).color(tokens::DIM),
        mode_seg(app),
        Space::new(Length::Fill, 0),
    ]
    .spacing(12)
    .align_y(Alignment::Center);

    // Scope/Hops (Graph mode) live as an overlay on the canvas itself,
    // matching `BrainGraph.dc.html` exactly — not here. Cramming them into
    // this row alongside List's own controls is what caused the toolbar to
    // overflow at 1024px (Reindex and the search field both clipped).
    controls = match app.brain_view.mode {
        BrainMode::List => controls
            .push(search_field(app))
            .push(catalogue_select(app))
            .push(reindex_button())
            .push(super::brain_edit_view::new_button(app)),
        BrainMode::Graph => controls,
        BrainMode::Sources => controls
            .push(toolbar_button("Sync all", Message::SourcesSyncAll, false))
            .push(toolbar_button("+ Add source", Message::SourcesOpenAdd, true)),
    };

    column![
        container(controls.padding([0, 18])).center_y(Length::Fixed(48.0)).width(Length::Fill),
        // `Border` has no per-side width, so a bottom-only rule needs its
        // own 1px strip rather than `container::Style.border` (which would
        // draw a full box around the toolbar on every side).
        container(Space::new(0, 0)).width(Length::Fill).height(Length::Fixed(1.0)).style(|_| container::Style {
            background: Some(Background::Color(tokens::BORDER)),
            ..Default::default()
        }),
    ]
    .into()
}

/// Wraps arbitrary `body` content with the shared Brain toolbar (used by
/// List/Graph/Sources alike — same component, same height, same bottom
/// rule — so the segmented control and toolbar chrome never drift apart
/// between the three modes).
pub fn toolbar_wrap<'a>(app: &'a App, body: Element<'a, Message>) -> Element<'a, Message> {
    column![toolbar(app), body].width(Length::Fill).height(Length::Fill).into()
}

fn panel_title<'a>(label: &'a str) -> Element<'a, Message> {
    text(label.to_uppercase()).size(10.5).font(crate::style::SANS).color(tokens::DIM2).into()
}

fn kind_row(app: &App, key: Option<String>, label: String, count: usize) -> Element<'_, Message> {
    let is_active = app.brain_view.kind_filter == key;
    let dot_color = key.as_deref().map(kind_color).unwrap_or(tokens::TEXT);
    let owned_key = key;
    button(
        row![
            text("●").size(8).color(dot_color),
            text(label).size(12.5).font(crate::style::SANS).color(if is_active { tokens::TEXT } else { Color { r: 0xc8 as f32 / 255.0, g: 0xcc as f32 / 255.0, b: 0xd6 as f32 / 255.0, a: 1.0 } }),
            Space::new(Length::Fill, 0),
            text(count.to_string()).size(10.5).font(crate::style::MONO).color(tokens::DIM2),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    )
    .on_press(Message::BrainSetKindFilter(owned_key))
    .width(Length::Fill)
    .padding([6, 10])
    .style(move |_t, status| button::Style {
        background: Some(Background::Color(if is_active {
            Color { r: 0x1b as f32 / 255.0, g: 0x22 as f32 / 255.0, b: 0x36 as f32 / 255.0, a: 1.0 }
        } else if matches!(status, button::Status::Hovered) {
            tokens::PANEL2
        } else {
            Color::TRANSPARENT
        })),
        border: Border { radius: 6.0.into(), ..Default::default() },
        ..Default::default()
    })
    .into()
}

fn repo_chip(app: &App, name: String) -> Element<'_, Message> {
    let is_active = app.brain_view.repo_filter.as_deref() == Some(name.as_str());
    let label = if is_active { format!("{name} ✕") } else { name.clone() };
    // Advanced: Inter has no "✕", and Basic shaping doesn't fall back.
    button(text(label).size(10.5).font(crate::style::SANS).shaping(iced::widget::text::Shaping::Advanced).color(if is_active { tokens::BLUE } else { Color { r: 0xb4 as f32 / 255.0, g: 0xba as f32 / 255.0, b: 0xcb as f32 / 255.0, a: 1.0 } }))
        .on_press(Message::BrainSetRepoFilter(Some(name)))
        .padding([2, 9])
        .style(move |_t, _status| button::Style {
            background: Some(Background::Color(if is_active {
                Color { r: 0x1f as f32 / 255.0, g: 0x3a as f32 / 255.0, b: 0x6e as f32 / 255.0, a: 1.0 }
            } else {
                tokens::PANEL2
            })),
            border: Border {
                color: if is_active { Color { r: 0x2a as f32 / 255.0, g: 0x3a as f32 / 255.0, b: 0x63 as f32 / 255.0, a: 1.0 } } else { tokens::BORDER },
                width: 1.0,
                radius: 10.0.into(),
            },
            ..Default::default()
        })
        .into()
}

fn tag_chip(app: &App, name: String) -> Element<'_, Message> {
    let is_active = app.brain_view.tag_filter.as_deref() == Some(name.as_str());
    button(text(name.clone()).size(10.5).font(crate::style::SANS).color(if is_active { tokens::TEXT } else { Color { r: 0xb4 as f32 / 255.0, g: 0xba as f32 / 255.0, b: 0xcb as f32 / 255.0, a: 1.0 } }))
        .on_press(Message::BrainSetTagFilter(Some(name)))
        .padding([2, 9])
        .style(move |_t, _status| button::Style {
            background: Some(Background::Color(if is_active { tokens::PANEL } else { tokens::PANEL2 })),
            border: Border { color: if is_active { tokens::TEAL } else { tokens::BORDER }, width: 1.0, radius: 10.0.into() },
            ..Default::default()
        })
        .into()
}

fn inert_more_chip<'a>() -> Element<'a, Message> {
    container(text("more…").size(10.5).font(crate::style::SANS).color(tokens::DIM2))
        .padding([2, 9])
        .style(|_| container::Style {
            background: Some(Background::Color(tokens::PANEL2)),
            border: Border { color: tokens::BORDER, width: 1.0, radius: 10.0.into() },
            ..Default::default()
        })
        .into()
}

fn kinds_column(app: &App) -> Element<'_, Message> {
    let kinds = kinds_sorted_by_count(&app.brain_view.entries);
    let total = app.brain_view.entries.len();

    let mut rows: Vec<Element<Message>> =
        vec![panel_title("Kinds"), kind_row(app, None, "All entries".to_string(), total)];
    rows.extend(kinds.into_iter().map(|(key, label, count)| kind_row(app, Some(key), label, count)));

    let kind_scope: Vec<&BrainEntry> = app
        .brain_view
        .entries
        .iter()
        .filter(|e| app.brain_view.kind_filter.as_deref().is_none_or(|k| normalize_kind(&e.entry_type) == k))
        .collect();

    let (repos, _) = top_values(kind_scope.iter().copied(), |e| &e.repos, CHIP_LIMIT);
    if !repos.is_empty() {
        rows.push(Space::new(0, 10).into());
        rows.push(panel_title("Repo"));
        let mut chip_row: Vec<Element<Message>> = repos.iter().map(|(name, _)| repo_chip(app, name.clone())).collect();
        chip_row.push(inert_more_chip());
        rows.push(row(chip_row).spacing(6).wrap().into());
    }

    let (tags, _) = top_values(kind_scope.iter().copied(), |e| &e.tags, CHIP_LIMIT);
    if !tags.is_empty() {
        rows.push(Space::new(0, 10).into());
        rows.push(panel_title("Tag"));
        let mut chip_row: Vec<Element<Message>> = tags.iter().map(|(name, _)| tag_chip(app, name.clone())).collect();
        chip_row.push(inert_more_chip());
        rows.push(row(chip_row).spacing(6).wrap().into());
    }

    container(scrollable(column(rows).spacing(2).padding([14, 10])))
        .width(Length::Fixed(KINDS_COLUMN_WIDTH))
        .height(Length::Fill)
        .style(|_| container::Style {
            background: Some(Background::Color(tokens::PANEL)),
            border: Border { color: tokens::BORDER, width: 1.0, radius: 0.0.into() },
            ..Default::default()
        })
        .into()
}

fn sort_seg(app: &App) -> Element<'_, Message> {
    super::fleet_islands::seg_control(vec![
        ("Recent".to_string(), app.brain_view.sort == BrainSort::Recent, Message::BrainSetSort(BrainSort::Recent)),
        ("A–Z".to_string(), app.brain_view.sort == BrainSort::Az, Message::BrainSetSort(BrainSort::Az)),
        ("Most linked".to_string(), app.brain_view.sort == BrainSort::MostLinked, Message::BrainSetSort(BrainSort::MostLinked)),
    ])
}

fn entry_list_header<'a>(app: &'a App, shown: usize, total: usize) -> Element<'a, Message> {
    let label = app
        .brain_view
        .kind_filter
        .as_deref()
        .map(kind_label)
        .unwrap_or_else(|| "All entries".to_string());
    let dot: Element<Message> = match app.brain_view.kind_filter.as_deref() {
        Some(k) => text("●").size(9).color(kind_color(k)).into(),
        None => Space::new(0, 0).into(),
    };
    let mut meta = format!("{shown} of {total}");
    if let Some(r) = &app.brain_view.repo_filter {
        meta = format!("repo {r} · {meta}");
    }
    if let Some(t) = &app.brain_view.tag_filter {
        meta = format!("tag {t} · {meta}");
    }

    container(
        row![
            dot,
            text(label).size(12.5).font(crate::style::SANS).color(tokens::TEXT),
            text(meta).size(11).font(crate::style::SANS).color(tokens::DIM2),
            Space::new(Length::Fill, 0),
            sort_seg(app),
        ]
        .spacing(8)
        .align_y(Alignment::Center)
        .padding([10, 14]),
    )
    .style(|_| container::Style {
        border: Border { color: tokens::BORDER, width: 1.0, radius: 0.0.into() },
        ..Default::default()
    })
    .into()
}

/// Plain dim text naming the catalogue an entry came from — shown only in
/// "All sources" mode (`BrainScope::All`), where it's the one piece of
/// context the catalogue select (fixed to "All sources") can't convey.
/// Deliberately NOT a pill like the repo/tag chips below: a catalogue is
/// often named after what it's about (e.g. a "beta" catalogue full of
/// "beta"-tagged entries), so a same-styled source chip and tag chip
/// reading the same word were indistinguishable at a glance (found in
/// review). The "from " prefix plus plain text (no border/background)
/// keeps it legible without looking like another tag.
fn source_chip<'a>(source: &str) -> Element<'a, Message> {
    text(format!("from {source}")).size(10.5).font(crate::style::SANS).color(tokens::DIM2).into()
}

fn entry_row<'a>(app: &'a App, e: &'a BrainEntry) -> Element<'a, Message> {
    let is_selected = app.brain_view.selected.as_deref() == Some(e.id.as_str());
    let id = e.id.clone();
    let updated = e.updated.as_deref().unwrap_or("").to_string();
    let mut meta: Vec<Element<Message>> = vec![text(updated).size(10.5).font(crate::style::MONO).color(tokens::DIM2).into()];
    if app.brain.is_all() && !e.source.is_empty() {
        meta.push(text("·").size(10.5).color(tokens::DIM2).into());
        meta.push(source_chip(&e.source));
    }
    if let Some(repo) = e.repos.first() {
        meta.push(text("·").size(10.5).color(tokens::DIM2).into());
        meta.push(
            container(text(repo.clone()).size(10.5).font(crate::style::SANS).color(tokens::BLUE))
                .padding([1, 7])
                .style(|_| container::Style {
                    background: Some(Background::Color(Color { r: 0x14 as f32 / 255.0, g: 0x1c as f32 / 255.0, b: 0x30 as f32 / 255.0, a: 1.0 })),
                    border: Border { color: Color { r: 0x2a as f32 / 255.0, g: 0x3a as f32 / 255.0, b: 0x63 as f32 / 255.0, a: 1.0 }, width: 1.0, radius: 10.0.into() },
                    ..Default::default()
                })
                .into(),
        );
    }
    for t in e.tags.iter().take(2) {
        meta.push(
            container(text(t.clone()).size(10.5).font(crate::style::SANS).color(Color { r: 0xb4 as f32 / 255.0, g: 0xba as f32 / 255.0, b: 0xcb as f32 / 255.0, a: 1.0 }))
                .padding([1, 7])
                .style(|_| container::Style {
                    background: Some(Background::Color(tokens::PANEL2)),
                    border: Border { color: tokens::BORDER, width: 1.0, radius: 10.0.into() },
                    ..Default::default()
                })
                .into(),
        );
    }

    button(
        column![
            text(super::brain_panel::display_title(e)).size(12.5).font(crate::style::SANS).color(tokens::TEXT),
            row(meta).spacing(6).align_y(Alignment::Center),
        ]
        .spacing(4)
        .padding([9, 14]),
    )
    .on_press(Message::BrainSelectEntry(id))
    .width(Length::Fill)
    .padding(0)
    .style(move |_t, status| button::Style {
        background: Some(Background::Color(if is_selected {
            Color { r: 0x15 as f32 / 255.0, g: 0x2a as f32 / 255.0, b: 0x33 as f32 / 255.0, a: 1.0 }
        } else if matches!(status, button::Status::Hovered) {
            tokens::PANEL2
        } else {
            Color::TRANSPARENT
        })),
        border: if is_selected {
            Border { color: Color { r: 0x2a as f32 / 255.0, g: 0x7d as f32 / 255.0, b: 0x75 as f32 / 255.0, a: 1.0 }, width: 1.0, radius: 0.0.into() }
        } else {
            Border::default()
        },
        ..Default::default()
    })
    .into()
}

fn entry_list(app: &App) -> Element<'_, Message> {
    let visible = visible_entries(
        &app.brain_view.entries,
        &app.brain_view.edges,
        app.brain_view.kind_filter.as_deref(),
        app.brain_view.repo_filter.as_deref(),
        app.brain_view.tag_filter.as_deref(),
        &app.brain_view.filter,
        app.brain_view.sort,
    );
    let total = count_for_kind(&app.brain_view.entries, app.brain_view.kind_filter.as_deref());

    let body: Element<Message> = if visible.is_empty() {
        container(text("No entries match this filter.").size(12.5).font(crate::style::SANS).color(tokens::DIM))
            .padding([24, 16])
            .into()
    } else {
        let rows: Vec<Element<Message>> = visible.iter().map(|e| entry_row(app, e)).collect();
        scrollable(column(rows)).height(Length::Fill).into()
    };
    let new_draft = app
        .brain_view
        .editor
        .as_ref()
        .is_some_and(|ed| matches!(ed.target, super::brain_editor::EditTarget::New { .. }));
    let body: Element<Message> = if new_draft { column![super::brain_edit_view::draft_row(), body].into() } else { body };

    container(column![entry_list_header(app, visible.len(), total), body])
        .width(Length::Fixed(ENTRY_LIST_WIDTH))
        .height(Length::Fill)
        .style(|_| container::Style {
            border: Border { color: tokens::BORDER, width: 1.0, radius: 0.0.into() },
            ..Default::default()
        })
        .into()
}

fn footer_chip<'a>(e: &'a BrainEntry) -> Element<'a, Message> {
    let id = e.id.clone();
    let key = normalize_kind(&e.entry_type);
    button(
        row![text("●").size(7).color(kind_color(&key)), Space::new(5, 0), text(super::brain_panel::display_title(e)).size(11).font(crate::style::SANS)]
            .align_y(Alignment::Center),
    )
    .on_press(Message::BrainSelectEntry(id))
    .padding([3, 9])
    .style(|_t, status| button::Style {
        background: None,
        text_color: if matches!(status, button::Status::Hovered) { tokens::TEXT } else { tokens::DIM },
        border: Border { color: if matches!(status, button::Status::Hovered) { tokens::TEXT } else { tokens::BORDER }, width: 1.0, radius: 10.0.into() },
        ..Default::default()
    })
    .into()
}

fn reading_pane(app: &App) -> Element<'_, Message> {
    if let Some(ed) = &app.brain_view.editor {
        return super::brain_edit_view::editor_pane(app, ed);
    }
    let selected =
        app.brain_view.selected.as_ref().and_then(|id| app.brain_view.entries.iter().find(|e| &e.id == id));

    // `None` returns early, before the `scrollable` below: that widget's
    // content must have a bounded height (iced's own `validate()` panics
    // otherwise — "scrollable content must not fill its vertical scrolling
    // axis"), which a `center_y(Length::Fill)` placeholder violates. There's
    // nothing to scroll when nothing's selected anyway, so it renders
    // directly in the outer `Length::Fill` container instead.
    let Some(e) = selected else {
        return container(text("Nothing selected.").size(13).font(crate::style::SANS).color(tokens::DIM))
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into();
    };

    let content: Element<Message> = {
            let key = normalize_kind(&e.entry_type);
            let crumb = text(format!("brain › {} › {}", key, e.id.rsplit('/').next().unwrap_or(&e.id)))
                .size(10.5)
                .font(crate::style::MONO)
                .color(tokens::DIM2);
            let title = text(super::brain_panel::display_title(e)).size(20).font(crate::style::SANS).color(tokens::TEXT);

            let full_path = app.brain.file_path(&e.id).to_string_lossy().to_string();
            let mut meta: Vec<Element<Message>> = vec![
                super::fleet_focus::badge(kind_label(&key).to_uppercase(), {
                    let c = kind_color(&key);
                    Color { a: 0.18, ..c }
                }, kind_color(&key)),
            ];
            if let Some(u) = &e.updated {
                meta.push(text(format!("updated {u}")).size(11).font(crate::style::MONO).color(tokens::DIM).into());
            }
            if app.brain.is_all() && !e.source.is_empty() {
                meta.push(source_chip(&e.source));
            }
            for repo in &e.repos {
                meta.push(
                    container(text(repo.clone()).size(10.5).font(crate::style::SANS).color(tokens::BLUE))
                        .padding([1, 7])
                        .style(|_| container::Style {
                            background: Some(Background::Color(Color { r: 0x14 as f32 / 255.0, g: 0x1c as f32 / 255.0, b: 0x30 as f32 / 255.0, a: 1.0 })),
                            border: Border { color: Color { r: 0x2a as f32 / 255.0, g: 0x3a as f32 / 255.0, b: 0x63 as f32 / 255.0, a: 1.0 }, width: 1.0, radius: 10.0.into() },
                            ..Default::default()
                        })
                        .into(),
                );
            }
            for t in &e.tags {
                meta.push(
                    container(text(t.clone()).size(10.5).font(crate::style::SANS).color(Color { r: 0xb4 as f32 / 255.0, g: 0xba as f32 / 255.0, b: 0xcb as f32 / 255.0, a: 1.0 }))
                        .padding([1, 7])
                        .style(|_| container::Style {
                            background: Some(Background::Color(tokens::PANEL2)),
                            border: Border { color: tokens::BORDER, width: 1.0, radius: 10.0.into() },
                            ..Default::default()
                        })
                        .into(),
                );
            }
            meta.push(Space::new(Length::Fill, 0).into());
            meta.extend(super::brain_edit_view::read_actions(app, e, full_path));

            let body = container(
                super::brain_markdown::view(
                    &app.brain_view.markdown,
                    iced::widget::markdown::Settings {
                        code_size: 12.0.into(),
                        ..iced::widget::markdown::Settings::with_text_size(14.0)
                    },
                    crate::components::markdown_style::inline_code_style(tokens::TEAL, 2.0),
                    tokens::DIM2,
                )
                .map(Message::BrainLinkClicked),
            )
            .max_width(720.0);

            let backs = &app.brain_view.backlinks;
            let outs = &app.brain_view.outlinks;
            let mut footer_cols: Vec<Element<Message>> = Vec::new();
            if !backs.is_empty() {
                let chips: Vec<Element<Message>> = backs.iter().map(footer_chip).collect();
                footer_cols.push(
                    column![
                        text(format!("Cited by · {}", backs.len())).size(10).font(crate::style::SANS).color(tokens::DIM2),
                        Space::new(0, 8),
                        row(chips).spacing(6).wrap(),
                    ]
                    .into(),
                );
            }
            if !outs.is_empty() {
                let chips: Vec<Element<Message>> = outs.iter().map(footer_chip).collect();
                footer_cols.push(
                    column![
                        text(format!("Links to · {}", outs.len())).size(10).font(crate::style::SANS).color(tokens::DIM2),
                        Space::new(0, 8),
                        row(chips).spacing(6).wrap(),
                    ]
                    .into(),
                );
            }

            let mut sections: Vec<Element<Message>> = Vec::new();
            if let Some(n) = super::brain_edit_view::notice(app) {
                sections.push(n);
                sections.push(Space::new(0, 8).into());
            }
            sections.extend([
                crumb.into(),
                Space::new(0, 8).into(),
                title.into(),
                Space::new(0, 10).into(),
                row(meta).spacing(6).align_y(Alignment::Center).wrap().into(),
            ]);
            if let Some(confirm) = super::brain_edit_view::delete_confirm(app, e) {
                sections.push(Space::new(0, 10).into());
                sections.push(confirm);
            }
            sections.extend([
                Space::new(0, 18).into(),
                body.into(),
            ]);
            if !footer_cols.is_empty() {
                sections.push(Space::new(0, 22).into());
                sections.push(row(footer_cols).spacing(40).into());
            }
            column(sections).into()
    };

    // `scrollable`'s vertical scrollbar overlays the right edge of its own
    // content width (iced doesn't reserve extra space for it) — at the
    // reading pane's typical ~370px width, that overlaps the last few
    // characters of anything running the full width (code-review finding:
    // "…okrs.mc", "Linear sync" and a Notion URL all clipped). Giving the
    // scrolled content its own right padding, roughly the scrollbar's
    // width, keeps real content clear of it instead of widening the outer
    // container (which would just move the clipping edge, not remove it).
    const SCROLLBAR_GUTTER: f32 = 16.0;
    container(
        scrollable(container(content).padding(iced::Padding { top: 0.0, right: SCROLLBAR_GUTTER, bottom: 0.0, left: 0.0 }))
            .height(Length::Fill),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .padding([20, 28])
    .style(|_| container::Style { ..Default::default() })
    .into()
}

fn list_body(app: &App) -> Element<'_, Message> {
    row![kinds_column(app), entry_list(app), reading_pane(app)].width(Length::Fill).height(Length::Fill).into()
}

pub fn list_view(app: &App) -> Element<'_, Message> {
    toolbar_wrap(app, list_body(app))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, ty: &str, name: &str) -> BrainEntry {
        BrainEntry { id: id.into(), entry_type: ty.into(), name: name.into(), tags: vec![], repos: vec![], updated: None, body: String::new(), source: String::new() }
    }

    // `normalize_kind`/`kind_label`/`kinds_sorted_by_count`/`top_values`/
    // `count_for_kind`/`resolve_edges`/`visible_entries`/`BrainSort` are
    // unit-tested in `ninox_core::brain` — this module only tests what
    // stays GUI-local (the kind-dot coloring and toolbar wording this file
    // still owns).

    #[test]
    fn distinct_kind_count_merges_singular_and_plural_like_kinds_sorted_by_count() {
        let entries = vec![
            entry("a.md", "decision", "A"),
            entry("b.md", "decisions", "B"),
            entry("c.md", "errors", "C"),
        ];
        assert_eq!(distinct_kind_count(&entries), 2);
    }

    #[test]
    fn truncate_catalogue_name_leaves_short_names_untouched() {
        assert_eq!(truncate_catalogue_name("default"), "default");
    }

    #[test]
    fn truncate_catalogue_name_clips_a_long_name_with_an_ellipsis() {
        let long = "a-forty-character-catalogue-name-right-xy";
        assert!(long.chars().count() > CATALOGUE_SELECT_MAX_CHARS);
        let out = truncate_catalogue_name(long);
        assert_eq!(out.chars().count(), CATALOGUE_SELECT_MAX_CHARS + 1);
        assert!(out.ends_with('…'));
    }
}
