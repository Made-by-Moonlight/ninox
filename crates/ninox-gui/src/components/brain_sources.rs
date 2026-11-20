//! The v1 Brain tab's Sources view (`BrainSources.dc.html`): a card per
//! `CatalogueRef` (name, health, type, location, entry count, last sync,
//! pending pushes, "Include in search"), an Add-source card, and a 380px
//! edit panel. The source logic lives in `ninox_ui_shared::brain_sources`,
//! shared with the TUI; this module is only the iced rendering.

use ninox_core::config::CatalogueRef;
use ninox_ui_shared::brain_sources::{self as shared, Health, SourceEditForm, SourceType};

use iced::widget::{button, column, container, pick_list, row, scrollable, text, text_input, Space};
use iced::{Alignment, Background, Border, Color, Element, Length};

use super::shell::tokens;
use crate::app::{App, Message};

fn type_badge_colors(t: SourceType) -> (Color, Color) {
    match t {
        SourceType::Local => (tokens::BLUE_BADGE_BG, tokens::BLUE),
        SourceType::GithubWiki => (tokens::VIOLET_BADGE_BG, tokens::VIOLET_BADGE_FG),
        SourceType::S3 => (tokens::AMBER_BADGE_BG, tokens::AMBER_BADGE_FG),
    }
}

fn type_badge<'a>(t: SourceType) -> Element<'a, Message> {
    let (bg, fg) = type_badge_colors(t);
    super::fleet_focus::badge(t.label().to_string(), bg, fg)
}

fn panel_title<'a>(label: &'a str) -> Element<'a, Message> {
    text(label.to_uppercase()).size(10.5).font(crate::style::SANS).color(tokens::DIM2).into()
}

fn source_card<'a>(app: &'a App, c: &'a CatalogueRef) -> Element<'a, Message> {
    let source_type = SourceType::from_remote(c.remote.as_deref());
    let status = app.source_status.get(&c.path);
    let healthy = shared::health(status) == Health::Healthy;

    let mut rows: Vec<Element<Message>> = Vec::new();

    let header_action: Element<Message> = if !healthy {
        button(text("Retry").size(10.5).font(crate::style::SANS).color(tokens::TEXT))
            .on_press(Message::SourcesSyncNow(c.name.clone()))
            .padding([3, 8])
            .style(|_t: &iced::Theme, _s| button::Style {
                background: Some(Background::Color(tokens::PANEL2)),
                border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
                text_color: tokens::TEXT,
                ..Default::default()
            })
            .into()
    } else {
        // The only "Include in search" control on the card — a second copy
        // used to live in the footer row too, which read as two separate
        // toggles even though only this one was ever wired to actually
        // flip anything.
        button(
            row![
                text(if c.enabled { "●" } else { "○" }).size(9).color(if c.enabled { tokens::TEAL } else { tokens::DIM }),
                Space::new(6, 0),
                text("Include in search").size(10.5).font(crate::style::SANS).color(tokens::DIM),
            ]
            .align_y(Alignment::Center),
        )
        .on_press(Message::SourcesToggleEnabled(c.name.clone()))
        .padding(0)
        .style(|_t: &iced::Theme, _s| button::Style { background: None, ..Default::default() })
        .into()
    };

    rows.push(
        row![
            text("●").size(9).color(if healthy { tokens::GREEN } else { tokens::RED }),
            Space::new(6, 0),
            text(c.name.clone()).size(13).font(crate::style::SANS).color(tokens::TEXT),
            Space::new(8, 0),
            type_badge(source_type),
            Space::new(Length::Fill, 0),
            header_action,
        ]
        .align_y(Alignment::Center)
        .into(),
    );

    rows.push(Space::new(0, 6).into());
    rows.push(text(shared::location_label(c)).size(10.5).font(crate::style::MONO).color(tokens::DIM).into());

    let entry_count = status.map(|s| s.entry_count).unwrap_or(0);
    let meta = format!("{entry_count} entries · {}", shared::sync_label(c, status, shared::now_unix()));
    rows.push(text(meta).size(10).font(crate::style::SANS).color(tokens::DIM2).into());

    let pending = shared::pending_pushes(status);
    if let Some(err) = status.and_then(|s| s.last_error.as_ref()) {
        rows.push(Space::new(0, 4).into());
        rows.push(text(format!("last sync failed · {}", shared::redact_credentials(err))).size(10.5).font(crate::style::SANS).color(tokens::RED).into());
    } else if pending > 0 {
        rows.push(Space::new(0, 4).into());
        rows.push(
            text(format!("{pending} pending push{}", if pending == 1 { "" } else { "es" }))
                .size(10.5)
                .font(crate::style::SANS)
                .color(tokens::AMBER)
                .into(),
        );
    }

    rows.push(Space::new(0, 8).into());
    rows.push(
        row![
            button(text(if status.map(|s| s.syncing).unwrap_or(false) { "Syncing…" } else { "Sync now" }).size(10.5).font(crate::style::SANS).color(tokens::TEXT))
                .on_press_maybe((!status.map(|s| s.syncing).unwrap_or(false)).then(|| Message::SourcesSyncNow(c.name.clone())))
                .padding([4, 9])
                .style(|_t, _s| button::Style {
                    background: Some(Background::Color(tokens::PANEL2)),
                    border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
                    text_color: tokens::TEXT,
                    ..Default::default()
                }),
            button(text("Edit").size(10.5).font(crate::style::SANS).color(tokens::TEXT))
                .on_press(Message::SourcesOpenEdit(c.name.clone()))
                .padding([4, 9])
                .style(|_t, _s| button::Style {
                    background: Some(Background::Color(Color::TRANSPARENT)),
                    border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
                    text_color: tokens::TEXT,
                    ..Default::default()
                }),
        ]
        .spacing(8)
        .align_y(Alignment::Center)
        .into(),
    );

    container(column(rows).spacing(2).padding(14))
        .width(Length::Fill)
        .style(|_| container::Style {
            background: Some(Background::Color(tokens::PANEL)),
            border: Border { color: tokens::BORDER, width: 1.0, radius: 8.0.into() },
            ..Default::default()
        })
        .into()
}

fn add_source_card<'a>() -> Element<'a, Message> {
    button(
        container(text("+ Add source").size(12).font(crate::style::SANS).color(tokens::DIM))
            .center_x(Length::Fill)
            .center_y(Length::Fill),
    )
    .on_press(Message::SourcesOpenAdd)
    .width(Length::Fill)
    .height(Length::Fixed(64.0))
    .style(|_t, status| button::Style {
        background: Some(Background::Color(if matches!(status, button::Status::Hovered) { tokens::PANEL2 } else { Color::TRANSPARENT })),
        border: Border { color: tokens::BORDER, width: 1.0, radius: 8.0.into() },
        text_color: tokens::DIM,
        ..Default::default()
    })
    .into()
}

fn labeled_field<'a>(label: &'a str, field: Element<'a, Message>) -> Element<'a, Message> {
    column![panel_title(label), Space::new(0, 4), field].spacing(0).into()
}

fn styled_pick_list<'a>(
    options: &'a [&'static str],
    selected: &'a str,
    on_select: impl Fn(String) -> Message + 'a,
) -> Element<'a, Message> {
    pick_list(options, Some(selected), move |v| on_select(v.to_string()))
        .text_size(12)
        .padding([6, 8])
        .style(|_t: &iced::Theme, _status| pick_list::Style {
            text_color: tokens::TEXT,
            placeholder_color: tokens::DIM,
            handle_color: tokens::DIM,
            background: Background::Color(tokens::PANEL2),
            border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
        })
        .menu_style(|_t: &iced::Theme| iced::widget::overlay::menu::Style {
            background: Background::Color(tokens::PANEL2),
            border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
            text_color: tokens::TEXT,
            selected_text_color: tokens::BG,
            selected_background: Background::Color(tokens::TEAL),
        })
        .into()
}

fn text_field<'a>(value: &str, placeholder: &'a str, on_change: impl Fn(String) -> Message + 'a) -> Element<'a, Message> {
    text_input(placeholder, value)
        .on_input(on_change)
        .size(12)
        .padding([6, 8])
        .style(|_t, _status| text_input::Style {
            background: Background::Color(tokens::PANEL2),
            border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
            icon: tokens::DIM,
            placeholder: tokens::DIM,
            value: tokens::TEXT,
            selection: tokens::TEAL,
        })
        .into()
}

/// Multi-line rules editor: ~7 visible lines, scrolling past that.
fn rules_editor<'a>(
    content: &'a iced::widget::text_editor::Content,
    placeholder: &'a str,
    on_action: impl Fn(iced::widget::text_editor::Action) -> Message + 'a,
) -> Element<'a, Message> {
    iced::widget::text_editor(content)
        .placeholder(placeholder)
        .on_action(on_action)
        .height(Length::Fixed(124.0))
        .size(12)
        .padding([6, 8])
        .style(|_t, status| {
            let focused = matches!(status, iced::widget::text_editor::Status::Focused);
            iced::widget::text_editor::Style {
                background: Background::Color(tokens::PANEL2),
                border: Border { color: if focused { tokens::TEAL } else { tokens::BORDER }, width: 1.0, radius: 6.0.into() },
                icon: tokens::DIM,
                placeholder: tokens::DIM,
                value: tokens::TEXT,
                selection: tokens::TEAL,
            }
        })
        .into()
}

fn activity_log(app: &App) -> Element<'_, Message> {
    let mut rows: Vec<Element<Message>> = vec![panel_title("Recent sync activity"), Space::new(0, 6).into()];
    if app.source_sync_log.is_empty() {
        rows.push(text("No syncs yet this session.").size(10.5).font(crate::style::SANS).color(tokens::DIM2).into());
    } else {
        for line in app.source_sync_log.iter().take(10) {
            rows.push(text(shared::redact_credentials(line)).size(10.5).font(crate::style::MONO).color(tokens::DIM).into());
        }
    }
    container(column(rows).spacing(3).padding(14))
        .width(Length::Fill)
        .style(|_| container::Style { background: Some(Background::Color(tokens::PANEL)), border: Border { color: tokens::BORDER, width: 1.0, radius: 8.0.into() }, ..Default::default() })
        .into()
}

fn edit_panel<'a>(app: &'a App, form: &'a SourceEditForm) -> Element<'a, Message> {
    let is_editing = form.editing.is_some();
    let mut rows: Vec<Element<Message>> = vec![
        text(if is_editing { "Edit source" } else { "Add source" }).size(14).font(crate::style::SANS).color(tokens::TEXT).into(),
        Space::new(0, 14).into(),
        labeled_field("Name", text_field(&form.name, "e.g. team-wiki", Message::SourcesFormName)),
        Space::new(0, 10).into(),
        labeled_field(
            "Type",
            super::fleet_islands::seg_control(vec![
                ("Local folder".to_string(), form.source_type == SourceType::Local, Message::SourcesFormSetType(SourceType::Local)),
                ("GitHub wiki".to_string(), form.source_type == SourceType::GithubWiki, Message::SourcesFormSetType(SourceType::GithubWiki)),
                ("S3".to_string(), form.source_type == SourceType::S3, Message::SourcesFormSetType(SourceType::S3)),
            ]),
        ),
        Space::new(0, 10).into(),
    ];

    match form.source_type {
        SourceType::Local => {
            rows.push(labeled_field("Folder path", text_field(&shared::redact_credentials(&form.location), "/path/to/brain", Message::SourcesFormLocation)));
        }
        SourceType::GithubWiki => {
            rows.push(labeled_field("Wiki URL", text_field(&shared::redact_credentials(&form.location), "https://github.com/owner/repo/wiki", Message::SourcesFormLocation)));
            rows.push(text("Uses your ambient git credentials — nothing is stored here.").size(10).font(crate::style::SANS).color(tokens::DIM2).into());
            rows.push(Space::new(0, 10).into());
            rows.push(labeled_field("Local cache", text_field(&form.cache_path, "/path/to/cache", Message::SourcesFormCachePath)));
        }
        SourceType::S3 => {
            rows.push(labeled_field("s3://bucket/prefix", text_field(&shared::redact_credentials(&form.location), "s3://bucket/prefix", Message::SourcesFormLocation)));
            rows.push(text("Uses your AWS profile/SSO — nothing is stored here.").size(10).font(crate::style::SANS).color(tokens::DIM2).into());
            rows.push(Space::new(0, 10).into());
            rows.push(
                row![
                    labeled_field("Region", text_field(&form.region, "eu-west-1", Message::SourcesFormRegion)),
                    labeled_field("Endpoint", text_field(&shared::redact_credentials(&form.endpoint), "optional, for R2/MinIO", Message::SourcesFormEndpoint)),
                ]
                .spacing(10)
                .into(),
            );
            rows.push(Space::new(0, 10).into());
            rows.push(labeled_field("Local cache", text_field(&form.cache_path, "/path/to/cache", Message::SourcesFormCachePath)));
        }
    }

    rows.push(Space::new(0, 10).into());
    rows.push(
        row![
            labeled_field("Sync", styled_pick_list(&["manual", "on_launch", "every_10_min"], &form.cadence, Message::SourcesFormCadence)),
            labeled_field("Access", styled_pick_list(&["read_write", "read_only"], &form.access, Message::SourcesFormAccess)),
        ]
        .spacing(10)
        .into(),
    );

    rows.push(Space::new(0, 16).into());
    rows.push(labeled_field(shared::BRAIN_RULES_LABEL, rules_editor(&app.sources_brain_rules_editor, "what belongs in this brain, what doesn't", Message::SourcesFormBrainRules)));
    rows.push(text(shared::BRAIN_RULES_HINT).size(10).font(crate::style::SANS).color(tokens::DIM2).into());
    rows.push(Space::new(0, 10).into());
    rows.push(labeled_field(shared::OVERRIDE_LABEL, rules_editor(&app.sources_rules_override_editor, "e.g. no transcripts, no raw logs", Message::SourcesFormRulesOverride)));
    rows.push(text(shared::OVERRIDE_HINT).size(10).font(crate::style::SANS).color(tokens::DIM2).into());

    if !matches!(form.source_type, SourceType::Local) {
        rows.push(Space::new(0, 8).into());
        rows.push(
            text("Conflicts are kept as a *.conflict-<who>-<time> copy next to the original — nothing is ever silently overwritten.")
                .size(10)
                .font(crate::style::SANS)
                .color(tokens::DIM2)
                .into(),
        );
    }

    if let Some(err) = &form.error {
        rows.push(Space::new(0, 10).into());
        rows.push(text(shared::redact_credentials(err)).size(11).font(crate::style::SANS).color(tokens::RED).into());
    }

    rows.push(Space::new(0, 16).into());
    let mut footer = row![
        button(text("Save").size(11.5).font(crate::style::SANS).color(tokens::BG))
            .on_press(Message::SourcesFormSave)
            .padding([6, 14])
            .style(|_t, _s| button::Style { background: Some(Background::Color(tokens::TEAL)), border: Border { radius: 6.0.into(), ..Default::default() }, text_color: tokens::BG, ..Default::default() }),
        button(text("Cancel").size(11.5).font(crate::style::SANS).color(tokens::TEXT))
            .on_press(Message::SourcesFormCancel)
            .padding([6, 14])
            .style(|_t, _s| button::Style { background: Some(Background::Color(tokens::PANEL2)), border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() }, text_color: tokens::TEXT, ..Default::default() }),
    ]
    .spacing(8);

    if is_editing {
        footer = footer.push(Space::new(Length::Fill, 0)).push(
            button(text("Remove").size(11.5).font(crate::style::SANS).color(tokens::RED))
                .on_press(Message::SourcesRemove(form.editing.clone().unwrap_or_default()))
                .padding([6, 14])
                .style(|_t, _s| button::Style { background: None, border: Border { color: tokens::RED, width: 1.0, radius: 6.0.into() }, text_color: tokens::RED, ..Default::default() }),
        );
    }
    rows.push(footer.into());

    if is_editing {
        rows.push(Space::new(0, 6).into());
        rows.push(text("Remove only forgets this source locally — it never touches the remote.").size(9.5).font(crate::style::SANS).color(tokens::DIM2).into());
    }

    container(scrollable(column(rows).spacing(0).padding(18)))
        .width(Length::Fixed(380.0))
        .height(Length::Fill)
        .style(|_| container::Style { background: Some(Background::Color(tokens::PANEL)), border: Border { color: tokens::BORDER, width: 1.0, radius: 0.0.into() }, ..Default::default() })
        .into()
}

pub fn sources_view(app: &App) -> Element<'_, Message> {
    let cards: Vec<Element<Message>> = app
        .catalogues
        .iter()
        .map(|c| source_card(app, c))
        .chain(std::iter::once(add_source_card()))
        .collect();

    // One full-width card per row rather than a multi-column grid: at the
    // widths this panel actually has to work with (the canvas minus an
    // open 380px edit panel), a fixed card width left a card-sized column
    // with a dead gap beside it instead of using the space — `Length::Fill`
    // on each card, stacked in a plain column, always uses what's there.
    let mut content_rows: Vec<Element<Message>> = cards;
    content_rows.push(Space::new(0, 14).into());
    content_rows.push(activity_log(app));

    let body = container(
        scrollable(
            column(content_rows).spacing(14).width(Length::Fill).padding(iced::Padding { top: 18.0, right: 24.0, bottom: 18.0, left: 18.0 }),
        )
        .width(Length::Fill),
    )
    .width(Length::Fill)
    .height(Length::Fill);

    let main: Element<Message> = if let Some(form) = &app.sources_edit {
        row![body, edit_panel(app, form)].width(Length::Fill).height(Length::Fill).into()
    } else {
        row![body].width(Length::Fill).height(Length::Fill).into()
    };

    super::brain_list::toolbar_wrap(app, main)
}
