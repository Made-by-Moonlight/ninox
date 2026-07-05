use iced::{
    widget::{button, column, container, pick_list, row, scrollable, text, text_input, Space},
    Alignment, Background, Border, Color, Element, Length,
};

use crate::app::{App, BrainMode, Message};
use crate::style::{
    dotted_rule, hard_shadow, heavy_frame, hline, micro_label, shadow_alpha, stamp, vline, MONO,
    MONO_MEDIUM, SANS, SANS_BOLD, SERIF, SERIF_ITALIC, SERIF_MEDIUM,
};
use crate::theme::ColorScheme;
use ninox_core::BrainEntry;

// ---------------------------------------------------------------------------
// Pure helpers (wikilinks, backlinks, categories) — TDD'd in `mod tests` at
// the bottom of this file.
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

/// Parse the raw text between `[[` and `]]`, handling Obsidian's optional
/// `|alias` display-text override and `#heading`/`#^block` suffix. Returns
/// `(target, display)`:
/// - `target` is used for resolution/backlink matching, with any
///   `#heading`/`#^block` suffix stripped (section-level navigation is out
///   of scope — links resolve to the whole entry).
/// - `display` is the text rendered in the reading pane: the alias when
///   present (`[[target#sec|label]]` → `label`), otherwise the original
///   text as typed, `#section` included (`[[note#sec]]` → `note#sec`).
///
/// Embeds (`![[x]]`) are not special-cased — the leading `!` is left alone
/// and `[[x]]` is parsed/linked normally, so an embed renders as a literal
/// `!` immediately followed by a clickable link (out of scope: no image/
/// block transclusion).
fn parse_wikilink_content(content: &str) -> (String, String) {
    let (before_pipe, display) = match content.find('|') {
        Some(i) => (&content[..i], content[i + 1..].trim().to_string()),
        None => (content, content.trim().to_string()),
    };
    let target = match before_pipe.find('#') {
        Some(i) => before_pipe[..i].trim().to_string(),
        None => before_pipe.trim().to_string(),
    };
    (target, display)
}

/// Percent-encode the characters that would otherwise break the
/// `[text](dest)` CommonMark link syntax when used as a link *destination*
/// (a bare space ends the destination early; unbalanced `(`/`)` end it too)
/// or collide with our own `%`-escaping. Deliberately not a general URL
/// encoder — this only needs to round-trip wikilink targets between
/// `preprocess_wikilinks` and the `BrainLinkClicked` handler, which reverses
/// it with `percent_decode_wikilink_target` before calling `resolve_link`.
fn percent_encode_wikilink_target(target: &str) -> String {
    let mut out = String::with_capacity(target.len());
    for ch in target.chars() {
        match ch {
            ' ' => out.push_str("%20"),
            '(' => out.push_str("%28"),
            ')' => out.push_str("%29"),
            '%' => out.push_str("%25"),
            _ => out.push(ch),
        }
    }
    out
}

/// Reverses `percent_encode_wikilink_target`. Unknown/malformed `%xx`
/// sequences are passed through literally rather than dropped.
pub fn percent_decode_wikilink_target(target: &str) -> String {
    let mut out = String::with_capacity(target.len());
    let mut chars = target.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        let hex: String = chars.by_ref().take(2).collect();
        match u8::from_str_radix(&hex, 16) {
            Ok(byte) => out.push(byte as char),
            Err(_) => { out.push('%'); out.push_str(&hex); }
        }
    }
    out
}

/// `[[x]]` → `[x](ninox-brain:x)` so the markdown widget renders clickable
/// links — plus Obsidian's `[[x|alias]]` (display = alias) and
/// `[[x#heading]]` (display keeps the `#heading`, link resolves to `x`; see
/// `parse_wikilink_content`). The destination is always the bare, percent-
/// encoded target (see `percent_encode_wikilink_target`) since a raw target
/// containing spaces or parens is invalid CommonMark and truncates the link
/// at the first space.
pub fn preprocess_wikilinks(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(start) = rest.find("[[") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("]]") {
            Some(end) => {
                let (target, display) = parse_wikilink_content(&after[..end]);
                let encoded = percent_encode_wikilink_target(&target);
                out.push_str(&format!("[{display}](ninox-brain:{encoded})"));
                rest = &after[end + 2..];
            }
            None => { out.push_str("[["); rest = after; }
        }
    }
    out.push_str(rest);
    out
}

/// Plain-text preview snippet for the pinboard hover-preview slip: the first
/// `max_chars` characters of `body` with markdown noise lightly stripped —
/// leading `#`/`##`/… ATX header lines dropped entirely, wikilinks reduced
/// to readable text (`[[target|alias]]` → `alias`, `[[target]]` → `target`,
/// mirroring `parse_wikilink_content`'s pipe split, minus its heading
/// handling — a snippet has no use for `#section` anchors), and runs of
/// whitespace (including the newlines left by dropped header lines)
/// collapsed to single spaces. Stripping/collapsing happens before
/// truncation so the `max_chars` cut lands in clean text, never mid-syntax;
/// an ellipsis is appended only when the result was actually truncated.
pub fn preview_snippet(body: &str, max_chars: usize) -> String {
    let no_headers = body
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join(" ");

    let mut stripped = String::with_capacity(no_headers.len());
    let mut rest = no_headers.as_str();
    while let Some(start) = rest.find("[[") {
        stripped.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("]]") {
            Some(end) => {
                let content = &after[..end];
                let (target, display) = parse_wikilink_content(content);
                stripped.push_str(if content.contains('|') { &display } else { &target });
                rest = &after[end + 2..];
            }
            None => {
                stripped.push_str("[[");
                rest = after;
            }
        }
    }
    stripped.push_str(rest);

    let collapsed = stripped.split_whitespace().collect::<Vec<_>>().join(" ");

    if collapsed.chars().count() <= max_chars {
        collapsed
    } else {
        let truncated: String = collapsed.chars().take(max_chars).collect();
        format!("{}…", truncated.trim_end())
    }
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

/// Joined ✦ PINBOARD / ☰ CATALOGUE segments in a 1.5px ink frame with a hard
/// 2×2 shadow (mockup `.brain-mode`) — same bordered-frame pattern
/// spawn_modal's Entry-type toggle uses (`crate::style::segmented_frame`).
fn mode_toggle(app: &App) -> Element<'_, Message> {
    let s = &app.scheme;
    crate::style::segmented_frame(s, vec![
        crate::style::toggle_segment_glyph(
            s, "✦", "Pinboard", app.brain_view.mode == BrainMode::Pinboard,
            Message::BrainSetMode(BrainMode::Pinboard),
        ),
        crate::style::toggle_segment_glyph(
            s, "☰", "Catalogue", app.brain_view.mode == BrainMode::Catalogue,
            Message::BrainSetMode(BrainMode::Catalogue),
        ),
    ])
}

/// Underlined "⌕ search specimens…" field wired to the existing filter
/// query, with a ✕ clear affordance once a query is typed (matches
/// `filter_bar::filter_bar`).
fn search_field(app: &App) -> Element<'_, Message> {
    let s = &app.scheme;
    let has_filter = !app.brain_view.filter.is_empty();
    let input = text_input("search specimens…", &app.brain_view.filter)
        .on_input(Message::BrainFilterQuery)
        .size(12)
        .padding([4, 2])
        .style(crate::style::underlined_input_style(s));

    let mut field_row = row![
        text("⌕").size(13).font(crate::style::GLYPH).color(s.faint),
        Space::new(6, 0),
        input
    ]
        .align_y(Alignment::Center);
    if has_filter {
        field_row = field_row.push(
            button(text("✕").size(10).color(s.faint))
                .on_press(Message::BrainFilterQuery(String::new()))
                .padding(0)
                .style(|_t, _st| button::Style {
                    background: None,
                    border: Border::default(),
                    ..Default::default()
                }),
        );
    }

    column![field_row, hline(s.ink, 1.5)].width(Length::Fixed(230.0)).into()
}

/// Micro-label Reindex affordance, bordered in `rule_dark`, `ink`-colored
/// on hover.
fn reindex_btn(app: &App) -> Element<'_, Message> {
    let s = &app.scheme;
    button(micro_label("Reindex", s.ink_2))
        .on_press(Message::BrainReindex)
        .padding([4, 10])
        .style(move |_theme, status| button::Style {
            background: None,
            text_color: if status == button::Status::Hovered { s.ink } else { s.ink_2 },
            border: Border { color: s.rule_dark, width: 1.0, radius: 2.0.into() },
            ..Default::default()
        })
        .into()
}

/// Folio header: "The *brain*", SPECIMENS count, ✦/☰ mode toggle, underlined
/// search, and a micro-label Reindex affordance. Wraps onto two rows at
/// narrow widths via `folio::folio_scaffold` — see that module for why.
fn folio(app: &App) -> Element<'_, Message> {
    let count = app.brain_view.entries.len();
    crate::components::folio::folio_scaffold(
        app,
        move || {
            let s = &app.scheme;
            row![
                text("The ").size(34).font(SERIF).color(s.ink),
                text("brain").size(34).font(SERIF_ITALIC).color(s.ink),
                Space::new(18, 0),
                text(format!("SPECIMENS — {count}"))
                    .size(10.5)
                    .font(MONO)
                    .color(s.faint)
                    .wrapping(iced::widget::text::Wrapping::None),
            ]
            .align_y(Alignment::End)
            .into()
        },
        move || vec![mode_toggle(app), search_field(app), reindex_btn(app)],
    )
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

    // ⌂ renders separately in the glyph font — inside pick_list option
    // strings it would tofu in Spline Sans Mono (missing U+2302).
    let house = || text("⌂").size(11).font(crate::style::GLYPH).color(s.ink);
    let switcher: Element<Message> = if app.catalogues.len() > 1 {
        let names: Vec<String> = app.catalogues.iter().map(|c| c.name.clone()).collect();
        let selected = names.get(app.active_catalogue).cloned();
        let lookup = names.clone();
        let list = pick_list(names, selected, move |chosen| {
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
        });
        row![house(), Space::new(5, 0), list]
            .align_y(Alignment::Center)
            .into()
    } else {
        // Single catalogue: the plate renders inert.
        row![
            house(),
            Space::new(5, 0),
            text(name.to_string()).size(11).font(MONO).color(s.ink),
            Space::new(8, 0),
            text("▾").size(10).color(s.faint),
        ]
        .align_y(Alignment::Center)
        .into()
    };

    // Add-a-catalogue affordance: visible always (even with a single, inert
    // catalogue) — the only way to file a new one. Micro-sized, faint →
    // accent on hover (mockup "Adding a catalogue").
    let add_button = button(text("+").size(13).font(SANS_BOLD))
        .on_press(Message::CatalogueModalOpen)
        .padding([1, 7])
        .style(move |_theme, status| button::Style {
            background: None,
            text_color: if status == button::Status::Hovered { s.accent } else { s.faint },
            border: Border::default(),
            ..Default::default()
        });

    column![
        container(
            row![
                micro_label("Catalogue", s.faint).size(8.5),
                Space::new(Length::Fill, 0),
                switcher,
                Space::new(10, 0),
                add_button,
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

/// Fixed-position hover preview slip (mockup `.bpreview`): shown at the
/// bottom-right of the pinboard whenever a node is hovered, regardless of
/// where the cursor actually is (per the mockup, it does not follow the
/// cursor). 240px wide, `paper_2` background, 1.5px ink border, radius 2,
/// hard 3×3 card-tier shadow. Mockup applies `transform: rotate(1deg)` —
/// iced can't rotate widgets (see `stamp()`'s note on the same limitation),
/// so the slip renders unrotated; accepted deviation.
fn hover_preview_slip<'a>(s: &'a ColorScheme, entry: &'a BrainEntry) -> Element<'a, Message> {
    let (card_a, _, _) = shadow_alpha(s);

    let micro = text(format!("● {} — {}", entry.entry_type, entry.id).to_uppercase())
        .size(8.5)
        .font(SANS_BOLD)
        .color(category_color(s, &entry.entry_type));
    let name = text(entry.name.clone()).size(16).font(SERIF_MEDIUM).color(s.ink);
    let snippet = text(preview_snippet(&entry.body, 160)).size(11).font(SANS).color(s.ink_2);

    let slip = container(
        column![micro, Space::new(0, 5), name, Space::new(0, 4), snippet],
    )
    .width(Length::Fixed(240.0))
    .padding([12, 14])
    .style(move |_theme| container::Style {
        background: Some(Background::Color(s.paper_2)),
        border: Border { color: s.ink, width: 1.5, radius: 2.0.into() },
        shadow: hard_shadow(s, 3.0, 3.0, card_a),
        ..Default::default()
    });

    container(slip)
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(Alignment::End)
        .align_y(Alignment::End)
        .padding([14, 14])
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

    let board_frame = container(super::brain_pinboard::pinboard_canvas(app))
        .width(Length::Fill)
        .height(Length::Fill)
        .style(move |_theme| crate::style::heavy_frame(s));

    // Hovered id may no longer exist (a reindex/catalogue switch can drop or
    // rename entries out from under a stale hover) — resolve defensively and
    // simply skip the slip rather than panicking or showing stale content.
    let hovered_entry = app
        .brain_view
        .hovered
        .as_deref()
        .and_then(|id| app.brain_view.entries.iter().find(|e| e.id == id));
    let board: Element<Message> = match hovered_entry {
        Some(e) => iced::widget::stack![board_frame, hover_preview_slip(s, e)].into(),
        None => board_frame.into(),
    };

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
                "The specimen drawers are empty — run ninox brain index."
            } else {
                "No entries match this filter."
            })
            .size(15)
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
///   transparent resting state) — an old regression collapsed hover to a
///   no-op, so this must render differently in all three states.
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

/// A specimen chip: mono id in a rule-bordered pill; press navigates to that
/// entry. Shared by both the "cited by" backlinks row and the "related"
/// row — same chip design, different source list.
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

    let is_empty = selected.is_none();
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

            let backs = &app.brain_view.backlinks;
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

            let related = &app.brain_view.related;
            let related_section: Element<Message> = if related.is_empty() {
                Space::new(0, 0).into()
            } else {
                let chips: Vec<Element<Message>> =
                    related.iter().map(|b| backlink_chip(s, b)).collect();
                column![
                    Space::new(0, 22),
                    dotted_rule(s.rule_dark),
                    Space::new(0, 12),
                    micro_label(&format!("Related — {} specimens", related.len()), s.faint).size(9.0),
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
                related_section,
            ]
            .into()
        }
    };

    // The empty state centers itself with Fill dimensions, which iced's
    // scrollable forbids on its scroll axis (debug_assert panic) — so only
    // real entry content (Shrink-height column) goes inside the scrollable.
    let pane: Element<Message> = if is_empty {
        content
    } else {
        scrollable(content).height(Length::Fill).into()
    };
    container(pane)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding([28, 36])
        .style(move |_theme| heavy_frame(s))
        .into()
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
    fn preprocesses_wikilinks_to_brain_urls() {
        assert_eq!(
            preprocess_wikilinks("see [[frame-alignment]]."),
            "see [frame-alignment](ninox-brain:frame-alignment)."
        );
    }

    #[test]
    fn preprocesses_aliased_link_using_alias_as_display_text() {
        assert_eq!(
            preprocess_wikilinks("see [[frame-alignment|the pattern]]."),
            "see [the pattern](ninox-brain:frame-alignment)."
        );
    }

    #[test]
    fn preprocesses_heading_link_displaying_the_full_original_text() {
        // No alias — the display text is exactly as typed, `#section`
        // included, while the link destination resolves to the bare target.
        assert_eq!(
            preprocess_wikilinks("see [[frame-alignment#Rationale]]."),
            "see [frame-alignment#Rationale](ninox-brain:frame-alignment)."
        );
    }

    #[test]
    fn preprocesses_combined_heading_and_alias_link() {
        assert_eq!(
            preprocess_wikilinks("see [[a#b|c]]."),
            "see [c](ninox-brain:a)."
        );
    }

    #[test]
    fn embeds_render_as_literal_bang_plus_link_out_of_scope() {
        // `![[x]]` is not treated as a transclusion — the leading `!` is
        // left untouched and `[[x]]` is preprocessed normally, so an embed
        // renders as a literal "!" immediately followed by a clickable link.
        assert_eq!(
            preprocess_wikilinks("before ![[diagram]] after"),
            "before ![diagram](ninox-brain:diagram) after"
        );
    }

    #[test]
    fn preprocesses_wikilinks_with_spaces_to_valid_commonmark() {
        // A raw space in the destination would end the CommonMark link at
        // the first word ("[my target](ninox-brain:my target)." parses as a
        // link to "my" followed by literal text) — the destination must be
        // percent-encoded while the visible link text stays readable.
        assert_eq!(
            preprocess_wikilinks("see [[my target]]."),
            "see [my target](ninox-brain:my%20target)."
        );
    }

    #[test]
    fn percent_encode_and_decode_wikilink_targets_round_trip() {
        for target in ["my target", "a (parenthetical) target", "100% done", "plain"] {
            let encoded = percent_encode_wikilink_target(target);
            assert_eq!(percent_decode_wikilink_target(&encoded), target);
        }
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

    #[test]
    fn preview_snippet_drops_leading_header_lines() {
        assert_eq!(
            preview_snippet("# Title\n\nRing buffer behind terminal pane.", 200),
            "Ring buffer behind terminal pane."
        );
    }

    #[test]
    fn preview_snippet_drops_multiple_header_levels_anywhere_in_the_body() {
        assert_eq!(
            preview_snippet("## Overview\nFirst line.\n### Details\nSecond line.", 200),
            "First line. Second line."
        );
    }

    #[test]
    fn preview_snippet_reduces_aliased_wikilinks_to_the_alias() {
        assert_eq!(
            preview_snippet("Ring buffer behind [[TerminalPane|the pane]]; frames align.", 200),
            "Ring buffer behind the pane; frames align."
        );
    }

    #[test]
    fn preview_snippet_reduces_bare_wikilinks_to_the_target() {
        assert_eq!(
            preview_snippet("Owned by [[ScrollbackBuffer]] exclusively.", 200),
            "Owned by ScrollbackBuffer exclusively."
        );
    }

    #[test]
    fn preview_snippet_collapses_whitespace() {
        assert_eq!(preview_snippet("a\n\n  b\t\tc   d", 200), "a b c d");
    }

    #[test]
    fn preview_snippet_truncates_with_ellipsis_when_over_max_chars() {
        let body = "a".repeat(200);
        let snippet = preview_snippet(&body, 160);
        assert_eq!(snippet.chars().count(), 161); // 160 chars + the ellipsis char
        assert!(snippet.ends_with('…'));
        assert_eq!(&snippet[..160], "a".repeat(160).as_str());
    }

    #[test]
    fn preview_snippet_leaves_short_text_untouched_without_ellipsis() {
        let snippet = preview_snippet("short body", 160);
        assert_eq!(snippet, "short body");
        assert!(!snippet.ends_with('…'));
    }
}
