//! The Brain reading pane's write controls (`BrainEdit.dc.html`,
//! `BrainNew.dc.html`, `BrainDelete.dc.html`): Edit/Delete in read mode, the
//! in-place editor, the inline delete confirm, "+ New" and its draft row.

use iced::widget::{button, column, container, pick_list, row, text, text_editor, text_input, Space};
use iced::{Alignment, Background, Border, Color, Element, Length, Padding};
use ninox_core::brain::{kind_label, normalize_kind};
use ninox_core::BrainEntry;

use super::brain_editor::{BrainEditor, EditTarget};
use super::brain_list::kind_color;
use super::shell::tokens;
use crate::app::{App, Message};
use crate::style::{MONO, SANS, SANS_BOLD};

/// The kinds a new entry can be, as `normalize_kind` keys, in the board's order.
pub const NEW_KINDS: [&str; 6] = ["patterns", "decisions", "errors", "architecture", "concepts", "hazards"];

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color { r: r as f32 / 255.0, g: g as f32 / 255.0, b: b as f32 / 255.0, a: 1.0 }
}
const PRIMARY_BG: Color = rgb(0x12, 0x3f, 0x3b);
const PRIMARY_BORDER: Color = rgb(0x2a, 0x7d, 0x75);
const PRIMARY_FG: Color = rgb(0xbf, 0xf3, 0xec);
const DANGER_BG: Color = rgb(0x3a, 0x14, 0x20);
const DANGER_BORDER: Color = rgb(0x7d, 0x2a, 0x3c);
const DANGER_FG: Color = rgb(0xff, 0xc2, 0xcf);
const CONFIRM_BG: Color = rgb(0x1f, 0x0f, 0x15);
const INPUT_BORDER: Color = rgb(0x2f, 0x37, 0x50);
const KEY_BORDER: Color = rgb(0x33, 0x3b, 0x50);
const KEY_BG: Color = rgb(0x0f, 0x13, 0x1b);
const KEY_FG: Color = rgb(0xa3, 0xaa, 0xbb);
const CHIP_FG: Color = rgb(0xb4, 0xba, 0xcb);
const REPO_BG: Color = rgb(0x14, 0x1c, 0x30);
const REPO_BORDER: Color = rgb(0x2a, 0x3a, 0x63);
const BODY_FG: Color = rgb(0xc8, 0xcc, 0xd6);

pub fn name_input_id() -> text_input::Id {
    text_input::Id::new("brain-edit-name")
}

/// The kind "+ New" starts with: the Kinds filter's, when it's one of
/// [`NEW_KINDS`], else patterns.
pub fn default_new_kind(app: &App) -> String {
    app.brain_view
        .kind_filter
        .as_deref()
        .map(normalize_kind)
        .filter(|k| NEW_KINDS.contains(&k.as_str()))
        .unwrap_or_else(|| "patterns".to_string())
}

#[derive(Clone, Copy, PartialEq)]
enum Tone {
    Plain,
    Primary,
    Danger,
    Ghost,
}

fn key_chip<'a>(label: &'static str, primary: bool) -> Element<'a, Message> {
    container(text(label).size(10).font(super::help_overlay::chip_font(label)).color(if primary { PRIMARY_FG } else { KEY_FG }))
        .padding([1, 5])
        .style(move |_| container::Style {
            background: (!primary).then_some(Background::Color(KEY_BG)),
            border: Border { color: if primary { PRIMARY_BORDER } else { KEY_BORDER }, width: 1.0, radius: 4.0.into() },
            ..Default::default()
        })
        .into()
}

fn btn<'a>(label: &str, key: Option<&'static str>, tone: Tone, on_press: Option<Message>) -> Element<'a, Message> {
    let enabled = on_press.is_some();
    let (bg, border, fg) = match tone {
        Tone::Plain => (tokens::PANEL2, tokens::BORDER, tokens::TEXT),
        Tone::Primary => (PRIMARY_BG, PRIMARY_BORDER, PRIMARY_FG),
        Tone::Danger => (DANGER_BG, DANGER_BORDER, DANGER_FG),
        Tone::Ghost => (Color::TRANSPARENT, Color::TRANSPARENT, tokens::DIM),
    };
    let fg = if enabled { fg } else { tokens::DIM2 };
    let mut content = row![text(label.to_string()).size(12).font(SANS_BOLD).color(fg).wrapping(iced::widget::text::Wrapping::None)]
        .spacing(6)
        .align_y(Alignment::Center);
    if let Some(k) = key {
        content = content.push(key_chip(k, tone == Tone::Primary));
    }
    button(content)
        .on_press_maybe(on_press)
        .padding([5, 12])
        .style(move |_t, status| {
            let hovered = enabled && matches!(status, button::Status::Hovered);
            button::Style {
                background: Some(Background::Color(if hovered && tone != Tone::Primary && tone != Tone::Danger { tokens::PANEL2 } else { bg })),
                border: Border { color: if hovered { fg } else { border }, width: 1.0, radius: 6.0.into() },
                text_color: fg,
                ..Default::default()
            }
        })
        .into()
}

/// "+ New" in the List toolbar; disabled under All sources, where a new
/// entry has no single catalogue to go to.
pub fn new_button<'a>(app: &App) -> Element<'a, Message> {
    let enabled = app.active_catalogue.is_some() && app.brain_view.editor.is_none();
    btn("+ New", None, Tone::Primary, enabled.then(|| Message::BrainNewStart(default_new_kind(app))))
}

/// The inline delete confirm, shown in place of the read actions on a line
/// of its own so a narrow reading pane never squeezes it.
pub fn delete_confirm<'a>(app: &App, e: &BrainEntry) -> Option<Element<'a, Message>> {
    if app.brain_view.delete_confirm.as_deref() != Some(e.id.as_str()) {
        return None;
    }
    let busy = app.brain_view.job_running.is_some();
    Some(
        container(
            row![
                text("Delete this entry and its file?").size(12).font(SANS).color(DANGER_FG).width(Length::Fill),
                btn("Cancel", None, Tone::Ghost, Some(Message::BrainDeleteCancel)),
                btn(if busy { "Deleting…" } else { "Delete" }, None, Tone::Danger, (!busy).then_some(Message::BrainDeleteConfirm)),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        )
        .padding(Padding { top: 3.0, right: 4.0, bottom: 3.0, left: 10.0 })
        .width(Length::Fill)
        .style(|_| container::Style {
            background: Some(Background::Color(CONFIRM_BG)),
            border: Border { color: DANGER_BORDER, width: 1.0, radius: 6.0.into() },
            ..Default::default()
        })
        .into(),
    )
}

/// Read mode's actions; none while the delete confirm stands in for them.
pub fn read_actions<'a>(app: &App, e: &BrainEntry, full_path: String) -> Vec<Element<'a, Message>> {
    if app.brain_view.delete_confirm.as_deref() == Some(e.id.as_str()) {
        return Vec::new();
    }
    vec![
        btn("Show in graph", None, Tone::Plain, Some(Message::BrainShowInGraph(e.id.clone()))),
        btn("Open file", None, Tone::Plain, Some(Message::OpenInEditor(full_path))),
        btn("Edit", Some("e"), Tone::Plain, Some(Message::BrainEditStart(e.id.clone()))),
        btn("Delete", None, Tone::Ghost, Some(Message::BrainDeleteRequest(e.id.clone()))),
    ]
}

/// The last write's outcome, above the reading pane's content.
pub fn notice<'a>(app: &App) -> Option<Element<'a, Message>> {
    let (msg, color) = match app.brain_view.notice.as_ref()? {
        Ok(m) => (m.clone(), tokens::GREEN),
        Err(e) => (e.clone(), tokens::RED),
    };
    Some(text(msg).size(11.5).font(SANS).color(color).into())
}

/// The draft row "+ New" puts at the top of the entry list.
pub fn draft_row<'a>() -> Element<'a, Message> {
    container(
        column![
            text("New entry · unsaved").size(12.5).font(SANS).color(tokens::AMBER_BADGE_FG),
            text("draft").size(10.5).font(MONO).color(tokens::DIM2),
        ]
        .spacing(4),
    )
    .padding([10, 14])
    .width(Length::Fill)
    .style(|_| container::Style {
        background: Some(Background::Color(tokens::PANEL2)),
        border: Border { color: tokens::BORDER, width: 1.0, radius: 0.0.into() },
        ..Default::default()
    })
    .into()
}

fn chip<'a>(label: &str, repo: bool, on_remove: Option<Message>) -> Element<'a, Message> {
    let (bg, border, fg) = if repo { (REPO_BG, REPO_BORDER, tokens::BLUE) } else { (tokens::PANEL2, tokens::BORDER, CHIP_FG) };
    container(
        row![
            text(label.to_string()).size(10.5).font(SANS).color(fg),
            button(text("✕").size(9.5).font(crate::style::glyph_font_for("✕")).color(tokens::DIM2))
                .on_press_maybe(on_remove)
                .padding(0)
                .style(|_t, _s| button::Style { background: None, text_color: tokens::DIM2, ..Default::default() }),
        ]
        .spacing(5)
        .align_y(Alignment::Center),
    )
    .padding([2, 8])
    .style(move |_| container::Style {
        background: Some(Background::Color(bg)),
        border: Border { color: border, width: 1.0, radius: 10.0.into() },
        ..Default::default()
    })
    .into()
}

fn add_chip<'a>(placeholder: &'static str, value: &str, on_input: Option<fn(String) -> Message>, on_submit: Message) -> Element<'a, Message> {
    text_input(placeholder, value)
        .on_input_maybe(on_input)
        .on_submit(on_submit)
        .size(10.5)
        .font(SANS)
        .padding([2, 8])
        .width(Length::Fixed(96.0))
        .style(|_t, status| {
            let focused = matches!(status, text_input::Status::Focused);
            text_input::Style {
                background: Background::Color(Color::TRANSPARENT),
                border: Border { color: if focused { PRIMARY_BORDER } else { KEY_BORDER }, width: 1.0, radius: 10.0.into() },
                icon: tokens::DIM2,
                placeholder: tokens::DIM2,
                value: tokens::TEXT,
                selection: tokens::TEAL,
            }
        })
        .into()
}

fn tag_label<'a>(t: &'static str) -> Element<'a, Message> {
    text(t).size(11).font(SANS).color(tokens::DIM2).into()
}

/// ⌘S saves and esc cancels from inside the body editor too.
fn body_key_binding(press: text_editor::KeyPress) -> Option<text_editor::Binding<Message>> {
    use iced::keyboard::{key::Named, Key};
    match &press.key {
        Key::Character(c) if press.modifiers.command() && c.as_str().eq_ignore_ascii_case("s") => {
            Some(text_editor::Binding::Custom(Message::BrainEditSave))
        }
        Key::Named(Named::Escape) => Some(text_editor::Binding::Custom(Message::BrainEditCancel)),
        _ => text_editor::Binding::from_key_press(press),
    }
}

/// The reading pane in edit/new mode: same layout as read mode, with the
/// fields swapped in place.
pub fn editor_pane<'a>(app: &'a App, ed: &'a BrainEditor) -> Element<'a, Message> {
    let is_new = matches!(ed.target, EditTarget::New { .. });
    let (kind, crumb_file, crumb_note): (String, String, Option<&str>) = match &ed.target {
        EditTarget::Existing { id, .. } => {
            let kind = app
                .brain_view
                .entries
                .iter()
                .find(|e| &e.id == id)
                .map(|e| normalize_kind(&e.entry_type))
                .unwrap_or_default();
            (kind, id.rsplit('/').next().unwrap_or(id).to_string(), None)
        }
        EditTarget::New { kind } => {
            let file = match ed.to_job() {
                Ok(job) => job.id().rsplit('/').next().unwrap_or_default().to_string(),
                Err(_) => "…".to_string(),
            };
            (kind.clone(), file, Some("path from the name · never overwrites (adds -2)"))
        }
    };

    let mut crumb = row![
        text(format!("brain › {kind} › ")).size(10.5).font(MONO).color(tokens::DIM2),
        text(crumb_file).size(10.5).font(MONO).color(if is_new { tokens::TEXT } else { tokens::DIM2 }),
        super::fleet_focus::badge(if is_new { "NEW" } else { "EDITING" }.to_string(), tokens::AMBER_BADGE_BG, tokens::AMBER_BADGE_FG),
    ]
    .spacing(4)
    .align_y(Alignment::Center);
    if let Some(note) = crumb_note {
        crumb = crumb.push(text(note).size(10.5).font(MONO).color(tokens::DIM2));
    }

    // Read-only while its save runs, so nothing typed meanwhile is lost.
    let busy = app.brain_view.job_running.is_some();
    let name = text_input(if is_new { "Entry name" } else { "" }, &ed.name)
        .id(name_input_id())
        .on_input_maybe((!busy).then_some(Message::BrainEditName))
        .size(20)
        .font(SANS_BOLD)
        .padding([4, 8])
        .style(|_t, status| {
            let focused = matches!(status, text_input::Status::Focused);
            text_input::Style {
                background: Background::Color(tokens::BG),
                border: Border { color: if focused { PRIMARY_BORDER } else { INPUT_BORDER }, width: if focused { 2.0 } else { 1.0 }, radius: 6.0.into() },
                icon: tokens::DIM,
                placeholder: tokens::DIM2,
                value: tokens::TEXT,
                selection: tokens::TEAL,
            }
        });

    let kind_slot: Element<Message> = if is_new {
        let options: Vec<String> = NEW_KINDS.iter().map(|k| kind_label(k).to_uppercase()).collect();
        let selected = Some(kind_label(&kind).to_uppercase());
        pick_list(options, selected, |label| {
            let key = NEW_KINDS.iter().find(|k| kind_label(k).to_uppercase() == label).copied().unwrap_or("patterns");
            Message::BrainEditKind(key.to_string())
        })
        .text_size(11)
        .font(SANS_BOLD)
        .padding([3, 6])
        .style(move |_t, _s| pick_list::Style {
            text_color: kind_color(&kind),
            placeholder_color: tokens::DIM,
            handle_color: tokens::DIM,
            background: Background::Color(tokens::BG),
            border: Border { color: INPUT_BORDER, width: 1.0, radius: 6.0.into() },
        })
        .into()
    } else {
        let c = kind_color(&kind);
        super::fleet_focus::badge(kind_label(&kind).to_uppercase(), Color { a: 0.18, ..c }, c)
    };

    let mut meta: Vec<Element<Message>> = vec![kind_slot, tag_label("repos")];
    let live = !busy;
    meta.extend(ed.repos.iter().map(|r| chip(r, true, live.then(|| Message::BrainEditRepoRemove(r.clone())))));
    meta.push(add_chip("+ repo", &ed.repo_draft, live.then_some(Message::BrainEditRepoDraft as fn(String) -> Message), Message::BrainEditRepoAdd));
    meta.push(Space::new(8, 0).into());
    meta.push(tag_label("tags"));
    meta.extend(ed.tags.iter().map(|t| chip(t, false, live.then(|| Message::BrainEditTagRemove(t.clone())))));
    meta.push(add_chip("+ tag", &ed.tag_draft, live.then_some(Message::BrainEditTagDraft as fn(String) -> Message), Message::BrainEditTagAdd));
    meta.push(Space::new(Length::Fill, 0).into());
    meta.push(btn("Cancel", Some("esc"), Tone::Ghost, Some(Message::BrainEditCancel)));
    let save_label = match (busy, is_new) {
        (true, _) => "Saving…",
        (false, true) => "Create",
        (false, false) => "Save",
    };
    meta.push(btn(save_label, Some("⌘S"), Tone::Primary, (!busy).then_some(Message::BrainEditSave)));

    let mut body = text_editor(&ed.body);
    if !busy {
        body = body.on_action(Message::BrainEditBody);
    }
    let body = body
        .key_binding(body_key_binding)
        .font(MONO)
        .size(12)
        .line_height(1.65)
        .padding([12, 14])
        .height(Length::Fill)
        .style(|_t, status| {
            let focused = matches!(status, text_editor::Status::Focused);
            text_editor::Style {
                background: Background::Color(tokens::BG),
                border: Border { color: if focused { PRIMARY_BORDER } else { INPUT_BORDER }, width: 1.0, radius: 6.0.into() },
                icon: tokens::DIM,
                placeholder: tokens::DIM2,
                value: BODY_FG,
                selection: Color { a: 0.35, ..tokens::TEAL },
            }
        });

    let mut top = column![crumb, Space::new(0, 8), name, Space::new(0, 10), row(meta).spacing(6).align_y(Alignment::Center).wrap()]
        .spacing(0);
    if let Some(e) = &ed.error {
        top = top.push(Space::new(0, 8)).push(text(e.clone()).size(11.5).font(SANS).color(tokens::RED));
    }

    let footer_right = if is_new {
        let catalogue = app.active_catalogue.and_then(|i| app.catalogues.get(i)).map(|c| c.name.clone()).unwrap_or_default();
        format!("Creates in catalogue {catalogue} · disabled under All sources")
    } else {
        "Save writes the file, syncs the source, then reindexes this entry".to_string()
    };
    let footer = container(
        row![
            text("Markdown body · frontmatter keys other than name, tags and repos are kept as they are").size(11).font(SANS).color(tokens::DIM),
            Space::new(Length::Fill, 0),
            text(footer_right).size(11).font(SANS).color(tokens::DIM),
        ]
        .spacing(14)
        .align_y(Alignment::Center),
    )
    .padding([10, 28])
    .width(Length::Fill)
    .style(|_| container::Style {
        background: Some(Background::Color(tokens::PANEL)),
        border: Border { color: tokens::BORDER, width: 1.0, radius: 0.0.into() },
        ..Default::default()
    });

    column![
        container(column![top, Space::new(0, 12), body, Space::new(0, 12)])
            .padding(Padding { top: 18.0, right: 28.0, bottom: 0.0, left: 28.0 })
            .height(Length::Fill),
        footer,
    ]
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}
