//! Attach/inspect a catalogue's S3-compatible remote — the GUI equivalent of
//! `ninox brain remote set` / `status` / `unset`. Opened from the small
//! remote badge beside the volume plate (`brain_panel::remote_badge`).
//! Mirrors `catalogue_modal`'s journal-entry styling.

use iced::{
    widget::{button, column, container, row, text, text_input, Space},
    Alignment, Background, Border, Color, Element, Length,
};

use crate::{
    app::{App, Message},
    style::{self, hard_shadow, hline, micro_label, shadow_alpha, MONO, SANS_BOLD, SERIF, SERIF_ITALIC},
};

// ---------------------------------------------------------------------------
// Form state
// ---------------------------------------------------------------------------

/// Attach-a-remote form state, used when the active catalogue has no
/// `.sync.toml` yet. Once attached, the modal switches to `status_view`
/// (driven by `App::remote_status`, not this form).
#[derive(Debug, Clone, Default)]
pub struct RemoteForm {
    pub url: String,
    pub endpoint: String,
    pub region: String,
    pub ttl: String,
    /// Refusal reason from the last confirm/sync attempt, if any. Cleared
    /// whenever the user edits a field.
    pub error: Option<String>,
    /// True while the attach-and-sync (or manual re-sync) `Task::future` is
    /// in flight — disables the form/buttons and shows a "Syncing…" state.
    pub syncing: bool,
}

/// Outcome of a full `BrainSync::sync()`, as plain fields rather than the
/// core `SyncReport` — `Message` derives `Clone` and this avoids asking
/// `ninox-core` for a derive it otherwise has no use for.
#[derive(Debug, Clone, Copy, Default)]
pub struct RemoteSyncSummary {
    pub pulled: usize,
    pub pushed: usize,
    pub deleted_local: usize,
    pub deleted_remote: usize,
    pub conflicts: usize,
}

impl RemoteSyncSummary {
    pub fn from_report(report: &ninox_core::brain_sync::SyncReport) -> Self {
        Self {
            pulled: report.pulled,
            pushed: report.pushed,
            deleted_local: report.deleted_local,
            deleted_remote: report.deleted_remote,
            conflicts: report.conflicts.len(),
        }
    }

    pub fn describe(&self) -> String {
        format!(
            "pulled {}, pushed {}, deleted {} local / {} remote, {} conflict{}",
            self.pulled,
            self.pushed,
            self.deleted_local,
            self.deleted_remote,
            self.conflicts,
            if self.conflicts == 1 { "" } else { "s" },
        )
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

pub fn remote_modal<'a>(state: &'a App, form: &'a RemoteForm) -> Element<'a, Message> {
    let inner = match &state.remote_status {
        Some(status) => status_view(state, status, form),
        None => attach_view(state, form),
    };
    backdrop(state, inner)
}

fn backdrop<'a>(state: &'a App, modal: Element<'a, Message>) -> Element<'a, Message> {
    let s = &state.scheme;
    let backdrop_alpha = if s.dark { 0.55 } else { 0.45 };
    container(modal)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .style(move |_theme| container::Style {
            background: Some(Background::Color(Color { a: backdrop_alpha, ..s.shadow })),
            ..Default::default()
        })
        .into()
}

fn header<'a>(s: &'a crate::theme::ColorScheme, plain: &str, italic: &str) -> Element<'a, Message> {
    container(
        row![
            text(plain.to_string()).size(23).font(SERIF).color(s.ink),
            text(italic.to_string()).size(23).font(SERIF_ITALIC).color(s.ink),
        ]
        .align_y(Alignment::Center),
    )
    .padding([18, 22])
    .width(Length::Fill)
    .style(move |_theme| container::Style {
        background: Some(Background::Color(s.paper_2)),
        ..Default::default()
    })
    .into()
}

fn ghost_button<'a>(s: &'a crate::theme::ColorScheme, label: &str, msg: Option<Message>) -> Element<'a, Message> {
    let enabled = msg.is_some();
    button(text(label.to_string()).size(11).font(SANS_BOLD).color(if enabled { s.ink_2 } else { s.faint }))
        .on_press_maybe(msg)
        .padding([9, 18])
        .style(move |_theme, status| button::Style {
            background: None,
            text_color: if enabled { s.ink_2 } else { s.faint },
            border: Border {
                color: if enabled && status == button::Status::Hovered { s.ink } else { s.rule_dark },
                width: 1.5,
                radius: 2.0.into(),
            },
            ..Default::default()
        })
        .into()
}

fn primary_button<'a>(s: &'a crate::theme::ColorScheme, label: String, glyph: &'static str, msg: Option<Message>) -> Element<'a, Message> {
    let can_press = msg.is_some();
    let label_color = if can_press { s.card } else { s.faint };
    button(
        row![
            text(format!("{label} ")).size(12).font(SANS_BOLD).color(label_color),
            text(glyph).size(12).font(style::GLYPH).color(label_color),
        ]
        .align_y(Alignment::Center),
    )
    .on_press_maybe(msg)
    .padding([9, 20])
    .style(move |_theme, status| {
        let hovered = can_press && status == button::Status::Hovered;
        let offset = if hovered { 4.0 } else { 3.0 };
        let (card_a, _, _) = shadow_alpha(s);
        button::Style {
            background: Some(Background::Color(if can_press { s.accent } else { s.card })),
            text_color: label_color,
            border: Border { color: s.rule_dark, width: 1.0, radius: 2.0.into() },
            shadow: hard_shadow(s, offset, offset, card_a),
        }
    })
    .into()
}

fn error_line<'a>(s: &crate::theme::ColorScheme, msg: &str) -> Element<'a, Message> {
    row![
        text("⚑").size(11).font(style::GLYPH).color(s.accent),
        Space::new(6, 0),
        text(msg.to_string()).size(11).font(SANS_BOLD).color(s.accent),
    ]
    .align_y(Alignment::Center)
    .into()
}

/// Form shown when the active catalogue has no `.sync.toml` — attaching one
/// runs the same initial full sync as `ninox brain remote set` (spec §5):
/// a fresh bucket is published from the local brain, an existing one's
/// entries are pulled down (three-way diff handles overlap/conflicts).
fn attach_view<'a>(state: &'a App, form: &'a RemoteForm) -> Element<'a, Message> {
    let s = &state.scheme;
    let can_submit = !form.syncing && !form.url.trim().is_empty();

    let url_field = column![
        micro_label("Remote — s3://bucket/prefix", s.ink_2),
        Space::new(0, 6),
        text_input("s3://bucket/prefix", &form.url)
            .on_input(Message::RemoteFormUrl)
            .on_submit_maybe(can_submit.then_some(Message::RemoteFormConfirm))
            .font(MONO)
            .size(13)
            .padding([6, 2])
            .style(style::underlined_input_style(s)),
        hline(s.rule_dark, 1.5),
    ]
    .spacing(4);

    let detail_field = |label: &'static str, placeholder: &'static str, value: &'a String, on_input: fn(String) -> Message| {
        column![
            micro_label(label, s.ink_2),
            Space::new(0, 6),
            text_input(placeholder, value)
                .on_input(on_input)
                .on_submit_maybe(can_submit.then_some(Message::RemoteFormConfirm))
                .font(MONO)
                .size(12)
                .padding([6, 2])
                .style(style::underlined_input_style(s)),
            hline(s.rule_dark, 1.5),
        ]
        .spacing(4)
        .width(Length::FillPortion(1))
    };

    let details_row = row![
        detail_field("Endpoint", "optional (R2/MinIO)", &form.endpoint, Message::RemoteFormEndpoint),
        Space::new(14, 0),
        detail_field("Region", "optional", &form.region, Message::RemoteFormRegion),
        Space::new(14, 0),
        detail_field("Cache TTL (secs)", "0", &form.ttl, Message::RemoteFormTtl),
    ];

    let cancel = ghost_button(s, "Cancel", (!form.syncing).then_some(Message::RemoteFormCancel));
    let confirm_label = if form.syncing { "Syncing…" } else { "Attach & Sync" };
    let confirm = primary_button(s, confirm_label.to_string(), "⛓", can_submit.then_some(Message::RemoteFormConfirm));
    let footer = row![Space::new(Length::Fill, 0), cancel, confirm]
        .spacing(12)
        .align_y(Alignment::Center);

    let mut body = column![url_field, Space::new(0, 14), details_row]
        .padding([20, 24])
        .spacing(0);
    if let Some(err) = &form.error {
        body = body.push(Space::new(0, 14)).push(error_line(s, err));
    }
    body = body.push(Space::new(0, 22)).push(footer);

    container(column![header(s, "Attach a ", "remote"), hline(s.ink, 2.0), body])
        .width(Length::Fixed(460.0))
        .style(move |_theme| {
            let mut frame = style::heavy_frame(s);
            let (_, _, modal_a) = shadow_alpha(s);
            frame.shadow = hard_shadow(s, 8.0, 10.0, modal_a);
            frame
        })
        .into()
}

fn status_row<'a>(s: &crate::theme::ColorScheme, label: &str, value: String) -> Element<'a, Message> {
    row![
        container(micro_label(label, s.faint)).width(Length::Fixed(120.0)),
        text(value).size(12).font(MONO).color(s.ink_2),
    ]
    .align_y(Alignment::Start)
    .into()
}

/// Status shown once `.sync.toml` exists — offline-only reads (spec's
/// `ninox brain remote status`), plus Sync-now / Detach actions.
fn status_view<'a>(state: &'a App, status: &'a ninox_core::brain_sync::RemoteStatus, form: &'a RemoteForm) -> Element<'a, Message> {
    let s = &state.scheme;

    let last_check = if status.last_check_unix == 0 {
        "never".to_string()
    } else {
        ninox_core::brain_sync::rfc3339(status.last_check_unix)
    };

    let mut rows: Vec<Element<Message>> = vec![
        status_row(s, "remote", status.remote.clone()),
        status_row(s, "cache ttl", format!("{}s", status.cache_ttl_secs)),
        status_row(s, "generation", status.generation.to_string()),
        status_row(s, "last check", last_check),
        status_row(s, "pending pushes", status.pending_pushes.len().to_string()),
        status_row(s, "live conflicts", status.conflict_files.len().to_string()),
    ];
    if let Some(summary) = &state.remote_last_sync {
        rows.push(status_row(s, "last sync", summary.describe()));
    }

    let mut body = column(rows).spacing(8).padding([20, 24]);
    if let Some(err) = &form.error {
        body = body.push(Space::new(0, 14)).push(error_line(s, err));
    }

    let close = ghost_button(s, "Close", (!form.syncing).then_some(Message::RemoteFormCancel));
    let detach = ghost_button(s, "Detach", (!form.syncing).then_some(Message::RemoteDetach));
    let sync_label = if form.syncing { "Syncing…" } else { "Sync now" };
    let sync_now = primary_button(s, sync_label.to_string(), "⟲", (!form.syncing).then_some(Message::RemoteSyncNow));
    let footer = row![Space::new(Length::Fill, 0), close, detach, sync_now]
        .spacing(12)
        .align_y(Alignment::Center);
    body = body.push(Space::new(0, 18)).push(footer);

    container(column![header(s, "The ", "remote"), hline(s.ink, 2.0), body])
        .width(Length::Fixed(460.0))
        .style(move |_theme| {
            let mut frame = style::heavy_frame(s);
            let (_, _, modal_a) = shadow_alpha(s);
            frame.shadow = hard_shadow(s, 8.0, 10.0, modal_a);
            frame
        })
        .into()
}
