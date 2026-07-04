//! Enriched spawn modal — a "journal entry" over a dimmed backdrop.
//! Spec: docs/design-concepts/field-notes-design.md §"Spawn modal".
//!
//! Humans spawn orchestrators or standalone sessions; workers are spawned
//! only by orchestrators themselves (via `ninox spawn`), never from here.

use iced::{
    widget::{button, column, container, pick_list, row, text, text_input, Space},
    Alignment, Background, Border, Color, Element, Length,
};

use crate::{
    app::{App, Message},
    style::{self, hard_shadow, hline, micro_label, shadow_alpha, MONO, SANS, SANS_BOLD, SERIF, SERIF_ITALIC},
    theme::ColorScheme,
};

// ---------------------------------------------------------------------------
// Form state
// ---------------------------------------------------------------------------

/// What kind of session this spawn creates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SpawnKind {
    /// A new orchestrator in its own subdirectory of the orchestrator root.
    #[default]
    Orchestrator,
    /// An interactive, unattached session working in a user-supplied
    /// workspace (isolated via a git worktree when the workspace is a repo).
    Standalone,
}

/// A canned agent harness + model pairing offered as a chip in the modal.
#[derive(Debug, Clone)]
pub struct AgentPreset {
    pub label:   &'static str,
    pub harness: &'static str,
    pub model:   Option<&'static str>,
}

pub const AGENT_PRESETS: &[AgentPreset] = &[
    AgentPreset { label: "claude · fable-5",   harness: "claude-code", model: Some("claude-fable-5") },
    AgentPreset { label: "claude · opus-4.8",  harness: "claude-code", model: Some("claude-opus-4-8") },
    AgentPreset { label: "claude · haiku-4.5", harness: "claude-code", model: Some("claude-haiku-4-5") },
];

#[derive(Debug, Clone, Default)]
pub struct SpawnForm {
    pub kind:          SpawnKind,
    pub name:          String,
    /// Standalone kind only — user-supplied workspace path (tilde-expanded
    /// at confirm time). Required to confirm a standalone spawn.
    pub workspace:     String,
    /// Index into `AGENT_PRESETS`.
    pub agent_idx:     usize,
    /// Index into `AppConfig::catalogue_options()` — applies to both kinds.
    pub catalogue_idx: usize,
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// A transparent, underlined `text_input` matching the ledger-field pattern
/// used elsewhere (see `filter_bar::filter_bar`) but sized for a serif form field.
fn styled_input<'a>(s: &'a ColorScheme, placeholder: &'a str, value: &'a str) -> text_input::TextInput<'a, Message> {
    text_input(placeholder, value)
        .font(SERIF)
        .size(16)
        .padding([4, 2])
        .style(style::underlined_input_style(s))
}

fn pick_style<'a>(s: &'a ColorScheme) -> impl Fn(&iced::Theme, pick_list::Status) -> pick_list::Style + 'a {
    move |_theme, _status| pick_list::Style {
        text_color: s.ink,
        placeholder_color: s.faint,
        handle_color: s.ink_2,
        background: Background::Color(Color::TRANSPARENT),
        border: Border { color: s.rule_dark, width: 1.0, radius: 2.0.into() },
    }
}

/// A pill-style toggle button shared by the agent and catalogue chip rows
/// (the one pill exception in the design language; the Entry-type toggle
/// uses the bordered `style::segmented_frame`/`toggle_segment` pair instead —
/// see `kind_toggle` below).
fn chip<'a>(s: &'a ColorScheme, label: String, selected: bool, on_press: Message) -> Element<'a, Message> {
    let text_color = if selected { s.card } else { s.ink_2 };
    let border_active = Border { color: s.ink, width: 1.5, radius: 14.0.into() };
    let border_inactive = Border { color: s.rule_dark, width: 1.5, radius: 14.0.into() };
    button(text(label).size(11).font(if selected { SANS_BOLD } else { SANS }).color(text_color))
        .on_press(on_press)
        .padding([7, 16])
        .style(style::segment_style(s, selected, s.ink_2, None, border_active, border_inactive))
        .into()
}

pub fn spawn_modal<'a>(state: &'a App, form: &'a SpawnForm) -> Element<'a, Message> {
    let s = &state.scheme;
    let entry_no = state.sessions.len() + 1;
    let can_submit = !form.name.trim().is_empty()
        && (form.kind == SpawnKind::Orchestrator || !form.workspace.trim().is_empty());

    // ── Header: journal-entry title strip ──────────────────────────────────
    let header = container(
        row![
            text("Spawn a ").size(23).font(SERIF).color(s.ink),
            text("session").size(23).font(SERIF_ITALIC).color(s.ink),
            Space::new(Length::Fill, 0),
            text(format!("ENTRY № {entry_no}")).size(10).font(MONO).color(s.faint),
        ]
        .align_y(Alignment::Center),
    )
    .padding([18, 22])
    .width(Length::Fill)
    .style(move |_theme| container::Style {
        background: Some(Background::Color(s.paper_2)),
        ..Default::default()
    });

    // ── Entry type: two borderless segments in a shared bordered frame
    // (mockup `.bm`/`.brain-mode` — matches brain_panel's mode toggle rather
    // than the pill `chip()` style, which would double its 1.5px border
    // where two touching segments meet) ─────────────────────────────────────
    let kind_toggle = column![
        micro_label("Entry type", s.ink_2),
        Space::new(0, 8),
        style::segmented_frame(s, vec![
            style::toggle_segment(
                s,
                "⬡ Orchestrator",
                form.kind == SpawnKind::Orchestrator,
                Message::SpawnFormKind(SpawnKind::Orchestrator),
            ),
            style::toggle_segment(
                s,
                "Standalone",
                form.kind == SpawnKind::Standalone,
                Message::SpawnFormKind(SpawnKind::Standalone),
            ),
        ]),
    ]
    .spacing(0);

    // ── Name ────────────────────────────────────────────────────────────────
    let name_field = column![
        micro_label("Name", s.ink_2),
        Space::new(0, 6),
        styled_input(s, "e.g. theme-tokens", &form.name)
            .on_input(Message::SpawnFormName)
            .on_submit_maybe(can_submit.then_some(Message::SpawnFormConfirm)),
        hline(s.rule_dark, 1.5),
    ]
    .spacing(4);

    // ── Workspace (standalone only): mono underlined path input ─────────────
    // Repo derives from the dir's git remote at spawn time; the session gets
    // an isolated worktree off it when it's a git repo.
    let workspace_field: Option<Element<Message>> = (form.kind == SpawnKind::Standalone).then(|| {
        column![
            micro_label("Workspace", s.ink_2),
            Space::new(0, 6),
            text_input("~/proj/my-repo", &form.workspace)
                .on_input(Message::SpawnFormWorkspace)
                .on_submit_maybe(can_submit.then_some(Message::SpawnFormConfirm))
                .font(MONO)
                .size(13)
                .padding([6, 2])
                .style(style::underlined_input_style(s)),
            hline(s.rule_dark, 1.5),
        ]
        .spacing(4)
        .into()
    });

    // ── Catalogue (both kinds): which brain this session thinks with ────────
    let catalogues = state.config.catalogue_options();
    let catalogue_field: Element<Message> = if catalogues.len() <= 3 {
        let chips = row(catalogues.iter().enumerate().map(|(i, cat)| {
            chip(s, cat.name.clone(), i == form.catalogue_idx, Message::SpawnFormCatalogue(i))
        }))
        .spacing(8);
        column![micro_label("Catalogue", s.ink_2), Space::new(0, 8), chips].spacing(0).into()
    } else {
        let names: Vec<String> = catalogues.iter().map(|c| c.name.clone()).collect();
        let selected = names.get(form.catalogue_idx).cloned();
        let lookup = names.clone();
        column![
            micro_label("Catalogue", s.ink_2),
            Space::new(0, 6),
            pick_list(names, selected, move |chosen| {
                let idx = lookup.iter().position(|n| n == &chosen).unwrap_or(0);
                Message::SpawnFormCatalogue(idx)
            })
            .font(SERIF)
            .text_size(14)
            .padding([6, 10])
            .width(Length::Fill)
            .style(pick_style(s)),
        ]
        .spacing(4)
        .into()
    };

    // ── Agent · Model chips ──────────────────────────────────────────────────
    let agent_chips = row(AGENT_PRESETS.iter().enumerate().map(|(i, preset)| {
        chip(s, preset.label.to_string(), i == form.agent_idx, Message::SpawnFormAgent(i))
    }))
    .spacing(8);
    let agent_field = column![
        micro_label("Agent · Model", s.ink_2),
        Space::new(0, 8),
        agent_chips,
    ]
    .spacing(0);

    // ── Footer: cost estimate + ghost Cancel + primary Spawn ────────────────
    let cancel_button = button(text("Cancel").size(11).font(SANS_BOLD).color(s.ink_2))
        .on_press(Message::SpawnFormCancel)
        .padding([9, 18])
        .style(move |_theme, status| button::Style {
            background: None,
            text_color: s.ink_2,
            border: Border {
                color: if status == button::Status::Hovered { s.ink } else { s.rule_dark },
                width: 1.5,
                radius: 2.0.into(),
            },
            ..Default::default()
        });

    let spawn_button = button(
        text("SPAWN ⬡")
            .size(12)
            .font(SANS_BOLD)
            .color(if can_submit { s.card } else { s.faint }),
    )
    .on_press_maybe(can_submit.then_some(Message::SpawnFormConfirm))
    .padding([9, 20])
    .style(move |_theme, status| {
        let hovered = can_submit && status == button::Status::Hovered;
        let offset = if hovered { 4.0 } else { 3.0 };
        let (card_a, _, _) = shadow_alpha(s);
        button::Style {
            background: Some(Background::Color(if can_submit { s.accent } else { s.card })),
            text_color: if can_submit { s.card } else { s.faint },
            border: Border { color: s.rule_dark, width: 1.0, radius: 2.0.into() },
            shadow: hard_shadow(s, offset, offset, card_a),
        }
    });

    let footer = row![
        text("est. $2–4 / session").size(10).font(MONO).color(s.faint),
        Space::new(Length::Fill, 0),
        cancel_button,
        spawn_button,
    ]
    .spacing(12)
    .align_y(Alignment::Center);

    // Final layout: Entry type → Name → Workspace (standalone only) →
    // Catalogue → Agent·Model → footer.
    let mut body = column![kind_toggle, Space::new(0, 18), name_field]
        .padding([20, 24])
        .spacing(0);
    if let Some(ws) = workspace_field {
        body = body.push(Space::new(0, 18)).push(ws);
    }
    body = body
        .push(Space::new(0, 18))
        .push(catalogue_field)
        .push(Space::new(0, 18))
        .push(agent_field)
        .push(Space::new(0, 22))
        .push(footer);

    let modal = container(column![header, hline(s.ink, 2.0), body])
        .width(Length::Fixed(470.0))
        .style(move |_theme| {
            let mut frame = style::heavy_frame(s);
            let (_, _, modal_a) = shadow_alpha(s);
            frame.shadow = hard_shadow(s, 8.0, 10.0, modal_a);
            frame
        });

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
