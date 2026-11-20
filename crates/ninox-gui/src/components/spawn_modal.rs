//! Spawn dialog (G4, round-1 try-out feedback: "the Spawn view doesn't
//! match the new style") — v1 flat dark tokens (`components::shell::tokens`),
//! matching `Spawn.dc.html`. Replaces the old warm "Field Notes" journal-
//! entry theme (serif title, hard offset shadows) this file
//! used before.
//!
//! Humans spawn orchestrators or standalone sessions; workers are spawned
//! only by orchestrators themselves (via `ninox spawn`), never from here —
//! which is why a chosen worker `Type` here only ever sets `Session::
//! worker_type` (so it's recorded/queryable), not the fuller `run_spawn`
//! machinery (task-prompt role-brief composition, success criteria): there
//! is no task prompt on an interactive spawn for a type's brief to attach
//! to.

use iced::{
    widget::{button, column, container, pick_list, row, text, text_editor, text_input, Space},
    Alignment, Background, Border, Color, Element, Length, Padding,
};

use super::shell::tokens;
use crate::{
    app::{App, Message},
    style::{GLYPH, GLYPH_ALT, MONO, SANS},
};

// ---------------------------------------------------------------------------
// Form state
// ---------------------------------------------------------------------------

/// What kind of session this spawn creates, the default worker type, and
/// the cost-estimate math — shared with the TUI's own spawn form.
pub use ninox_ui_shared::spawn_form::{effective_model, effective_model_line, estimate_text, resolve_dialog_agent, static_prior, SpawnKind, DEFAULT_WORKER_TYPE};

#[derive(Debug, Clone)]
pub struct SpawnForm {
    pub kind:          SpawnKind,
    pub name:          String,
    /// Standalone kind only — user-supplied workspace path (tilde-expanded
    /// at confirm time). Required to confirm a standalone spawn.
    pub workspace:     String,
    /// Selected harness name (one of the registry's enabled harnesses).
    pub harness:       String,
    /// Model chosen in the picker. `None` = harness default.
    pub model:         Option<String>,
    /// `Some` while the picker is in `custom…` mode; holds the typed text.
    pub custom_model:  Option<String>,
    /// Catalogue names this tree reads — applies to both kinds. Seeded from
    /// `spawn_form::brain_choices` when the dialog opens.
    pub brain_reads:   Vec<String>,
    /// One of `brain_reads`, writable; empty when none is.
    pub brain_write:   String,
    /// Worker type name (`AppConfig::worker_type_registry().
    /// user_facing_names()` — never a hidden type) — recorded on the
    /// spawned `Session`, see this module's doc comment for the exact
    /// (deliberately limited) scope that has today.
    pub worker_type:   String,
    /// Human-readable refusal reason from the last confirm attempt, if any.
    /// Cleared whenever the user edits any field. Rendered above the footer.
    pub error:         Option<String>,
    /// Set once the user picks a harness / a model. Until then that picker
    /// mirrors the resolved default for the chosen kind and type, and a
    /// confirm resolves afresh instead of pinning the default as explicit.
    pub harness_picked: bool,
    pub model_picked:   bool,
}

impl Default for SpawnForm {
    fn default() -> Self {
        Self {
            kind:          SpawnKind::default(),
            name:          String::new(),
            workspace:     String::new(),
            harness:       "claude-code".to_string(),
            model:         None,
            custom_model:  None,
            brain_reads:   vec!["default".to_string()],
            brain_write:   "default".to_string(),
            worker_type:   DEFAULT_WORKER_TYPE.to_string(),
            error:         None,
            harness_picked: false,
            model_picked:   false,
        }
    }
}

impl SpawnForm {
    /// A blank custom field falls back to the picker's own selection.
    fn picked_model(&self) -> Option<String> {
        self.custom_model.as_deref().map(str::trim).filter(|c| !c.is_empty()).map(str::to_string)
            .or_else(|| self.model.clone())
    }

    pub fn resolved(&self, config: &ninox_core::config::AppConfig) -> ninox_core::model_resolution::ResolvedAgent {
        let model = self.picked_model();
        resolve_dialog_agent(
            config, self.kind, &self.worker_type,
            self.harness_picked.then_some(self.harness.as_str()),
            self.model_picked.then_some(model.as_deref()),
        )
    }

    /// Points whichever pickers the user hasn't set at the resolved default.
    pub fn sync_to_resolved(&mut self, config: &ninox_core::config::AppConfig) {
        let resolved = self.resolved(config);
        if !self.harness_picked {
            self.harness = resolved.agent.harness;
        }
        if !self.model_picked {
            self.model = resolved.agent.model;
            self.custom_model = None;
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// The dialog's header + footer height, measured from the v1 render.
const MODAL_CHROME_HEIGHT: f32 = 120.0;
/// Space kept between the dialog and the window edge (arbitrary).
const MODAL_MIN_MARGIN: f32 = 16.0;

/// Enter types a newline; ⌘⏎ still submits, as the Spawn button says.
fn prompt_key_binding(press: text_editor::KeyPress) -> Option<text_editor::Binding<Message>> {
    let submit = press.modifiers.command()
        && matches!(press.key, iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter));
    if submit {
        Some(text_editor::Binding::Custom(Message::SpawnFormConfirm))
    } else {
        text_editor::Binding::from_key_press(press)
    }
}

pub(crate) fn field_label<'a>(label: &'a str) -> Element<'a, Message> {
    text(label).size(10.5).font(SANS).color(tokens::DIM).into()
}

pub(crate) fn field_style(status: text_input::Status) -> text_input::Style {
    let focused = status == text_input::Status::Focused;
    text_input::Style {
        background: Background::Color(tokens::BG),
        border: Border {
            color: if focused { tokens::TEAL } else { tokens::BORDER },
            width: if focused { 2.0 } else { 1.0 },
            radius: 6.0.into(),
        },
        icon: tokens::DIM,
        placeholder: tokens::DIM,
        value: tokens::TEXT,
        selection: tokens::VIOLET,
    }
}

fn pick_style(_theme: &iced::Theme, status: pick_list::Status) -> pick_list::Style {
    let open = matches!(status, pick_list::Status::Opened);
    pick_list::Style {
        text_color: tokens::TEXT,
        placeholder_color: tokens::DIM,
        background: Background::Color(tokens::BG),
        border: Border { color: if open { tokens::TEAL } else { tokens::BORDER }, width: 1.0, radius: 6.0.into() },
        handle_color: tokens::DIM,
    }
}

/// The "esc"/"⌘⏎" keycap pill, in the caller's font for its glyphs (same
/// pattern as `prompt_bar::keycap` and `global_search`'s `⌘K` chip).
fn keycap<'a>(label: impl Into<String>, font: iced::Font) -> Element<'a, Message> {
    container(text(label.into()).size(10).font(font).color(tokens::DIM))
        .padding(Padding { top: 2.0, right: 6.0, bottom: 2.0, left: 6.0 })
        .style(|_| container::Style {
            background: Some(Background::Color(tokens::PANEL2)),
            border: Border { color: tokens::BORDER, width: 1.0, radius: 4.0.into() },
            ..Default::default()
        })
        .into()
}

/// Teal-selected choice card border/background — `Spawn.dc.html`'s
/// `.choice-on` (not in `tokens.rs`: a component-local accent, same as
/// `fleet_islands`'s/`brain_list`'s own locally-defined selected-state
/// colors rather than a shared token).
pub(crate) const CARD_SELECTED_BG: Color = Color { r: 0x12 as f32 / 255.0, g: 0x28 as f32 / 255.0, b: 0x26 as f32 / 255.0, a: 1.0 };
pub(crate) const CARD_SELECTED_BORDER: Color = Color { r: 0x2a as f32 / 255.0, g: 0x7d as f32 / 255.0, b: 0x75 as f32 / 255.0, a: 1.0 };

fn choice_card<'a>(title: &'a str, description: &'a str, selected: bool, on_press: Message) -> Element<'a, Message> {
    button(
        column![
            text(title).size(13).font(SANS).color(tokens::TEXT),
            text(description).size(11.5).font(SANS).color(tokens::DIM),
        ]
        .spacing(4),
    )
    .on_press(on_press)
    .width(Length::Fill)
    .padding(Padding { top: 11.0, right: 13.0, bottom: 11.0, left: 13.0 })
    .style(move |_t, _status| {
        button::Style {
            background: Some(Background::Color(if selected { CARD_SELECTED_BG } else { tokens::PANEL2 })),
            border: Border {
                color: if selected { CARD_SELECTED_BORDER } else { tokens::BORDER },
                width: 1.0,
                radius: 8.0.into(),
            },
            ..Default::default()
        }
    })
    .into()
}

/// A bordered pill toggle — the Agent row (one per enabled harness) and the
/// Brain catalogue row (≤3 options), matching `Spawn.dc.html`'s `.pill`/
/// `.pill-on`.
fn pill<'a>(label: String, selected: bool, on_press: Message) -> Element<'a, Message> {
    let text_color = Color { r: 0xc8 as f32 / 255.0, g: 0xcc as f32 / 255.0, b: 0xd6 as f32 / 255.0, a: 1.0 };
    let selected_text = Color { r: 0xbf as f32 / 255.0, g: 0xf3 as f32 / 255.0, b: 0xec as f32 / 255.0, a: 1.0 };
    button(text(label).size(12).font(SANS).color(if selected { selected_text } else { text_color }))
        .on_press(on_press)
        .padding(Padding { top: 6.0, right: 12.0, bottom: 6.0, left: 12.0 })
        .style(move |_t, status| {
            let hovered = matches!(status, button::Status::Hovered);
            button::Style {
                background: Some(Background::Color(if selected {
                    CARD_SELECTED_BG
                } else if hovered {
                    tokens::BORDER
                } else {
                    tokens::PANEL2
                })),
                border: Border { color: if selected { CARD_SELECTED_BORDER } else { tokens::BORDER }, width: 1.0, radius: 16.0.into() },
                ..Default::default()
            }
        })
        .into()
}

pub fn spawn_modal<'a>(state: &'a App, form: &'a SpawnForm) -> Element<'a, Message> {
    let can_submit = !form.name.trim().is_empty()
        && (form.kind == SpawnKind::Orchestrator || !form.workspace.trim().is_empty());

    // ── Header ────────────────────────────────────────────────────────────
    let header = container(
        row![
            text("Spawn a session").size(16).font(SANS).color(tokens::TEXT),
            Space::new(Length::Fill, 0),
            keycap("esc", MONO),
        ]
        .align_y(Alignment::Center),
    )
    .padding(Padding { top: 16.0, right: 20.0, bottom: 16.0, left: 20.0 })
    .width(Length::Fill);

    // ── Kind: two description cards ──────────────────────────────────────
    let kind_field = column![
        field_label("Kind"),
        Space::new(0, 8),
        row![
            choice_card(
                "Orchestrator",
                "Coordinates workers; gets its own island in Fleet.",
                form.kind == SpawnKind::Orchestrator,
                Message::SpawnFormKind(SpawnKind::Orchestrator),
            ),
            choice_card(
                "Standalone",
                "One agent in a workspace you pick, no workers.",
                form.kind == SpawnKind::Standalone,
                Message::SpawnFormKind(SpawnKind::Standalone),
            ),
        ]
        .spacing(10),
    ]
    .spacing(0);

    // ── Name ──────────────────────────────────────────────────────────────
    let name_field = column![
        field_label("Name"),
        Space::new(0, 6),
        text_input("e.g. billing-migration", &form.name)
            .on_input(Message::SpawnFormName)
            .on_submit_maybe(can_submit.then_some(Message::SpawnFormConfirm))
            .size(13)
            .font(SANS)
            .padding(Padding { top: 9.0, right: 11.0, bottom: 9.0, left: 11.0 })
            .style(|_t, status| field_style(status)),
        Space::new(0, 4),
        text("Shown on its island. Letters, digits and dashes.").size(11).font(SANS).color(tokens::DIM2),
    ]
    .spacing(0);

    // ── Workspace: required for standalone; an optional folder for an
    // orchestrator (blank keeps ninox's own) ────────────────────────────────
    let standalone = form.kind == SpawnKind::Standalone;
    let workspace_field: Element<Message> = {
        let input = text_input(
            if standalone { "~/proj/my-repo" } else { "~/proj/my-repo (optional)" },
            &form.workspace,
        )
        .on_input(Message::SpawnFormWorkspace)
        .on_submit_maybe(can_submit.then_some(Message::SpawnFormConfirm))
        .font(MONO)
        .size(13)
        .padding(Padding { top: 9.0, right: 11.0, bottom: 9.0, left: 11.0 })
        .style(|_t, status| field_style(status));
        let mut line = row![input].spacing(8).align_y(Alignment::Center);
        if cfg!(target_os = "macos") {
            line = line.push(
                button(text("Browse…").size(12).font(SANS).color(tokens::TEXT))
                    .on_press(Message::SpawnFormBrowse)
                    .padding(Padding { top: 8.0, right: 12.0, bottom: 8.0, left: 12.0 }),
            );
        }
        let mut col = column![field_label(if standalone { "Workspace" } else { "Workspace folder" }), Space::new(0, 6), line];
        if !standalone {
            col = col
                .push(Space::new(0, 4))
                .push(
                    text("An existing folder to run in, such as a git worktree. Blank uses ninox's own. \
                          ninox only adds .claude/settings.local.json entries and .claude/skills/ninox-*.")
                        .size(11)
                        .font(SANS)
                        .color(tokens::DIM2),
                );
        }
        col.spacing(0).into()
    };

    // ── Agent + Model ─────────────────────────────────────────────────────
    let registry = state.config.registry();
    let spec = registry.spec(&form.harness);
    let agent_pills = row(registry.enabled_names().into_iter().map(|name| {
        let selected = name == form.harness;
        pill(name.clone(), selected, Message::SpawnFormHarness(name))
    }))
    .spacing(8);

    let discovered = state.model_lists.get(&form.harness).and_then(|m| m.as_deref());
    let configured = form.model.clone().or_else(|| spec.model.clone());
    let options = ninox_ui_shared::models::model_options(&spec, discovered, configured.as_deref());
    let picker_selected = if form.custom_model.is_some() {
        Some(ninox_ui_shared::models::CUSTOM_SENTINEL.to_string())
    } else {
        configured
    };
    let model_picker = pick_list(options, picker_selected, Message::SpawnFormModel)
        .placeholder("harness default")
        .font(MONO)
        .text_size(12)
        .padding(Padding { top: 8.0, right: 10.0, bottom: 8.0, left: 10.0 })
        .width(Length::Fill)
        .style(pick_style);

    let agent_col = column![field_label("Agent"), Space::new(0, 8), agent_pills].spacing(0).width(Length::FillPortion(1));
    let mut model_col = column![field_label("Model"), Space::new(0, 8), model_picker].spacing(0).width(Length::FillPortion(1));
    if form.custom_model.is_some() {
        model_col = model_col.push(Space::new(0, 6)).push(
            text_input("model id", form.custom_model.as_deref().unwrap_or(""))
                .on_input(Message::SpawnFormCustomModel)
                .font(MONO)
                .size(12)
                .padding(Padding { top: 6.0, right: 8.0, bottom: 6.0, left: 8.0 })
                .style(|_t, status| field_style(status)),
        );
    }
    let agent_row = row![agent_col, Space::new(14, 0), model_col].align_y(Alignment::Start);
    let model_line = text(effective_model_line(&form.resolved(&state.config))).size(11).font(MONO).color(tokens::DIM);
    let agent_field = column![agent_row, Space::new(0, 6), model_line].spacing(0);

    // ── Brains ────────────────────────────────────────────────────────────
    let choices = ninox_ui_shared::spawn_form::brain_choices(&state.config);
    let read_chips = row(choices.iter().map(|c| {
        let label = if c.writable { c.name.clone() } else { format!("{} · read-only", c.name) };
        pill(label, form.brain_reads.contains(&c.name), Message::SpawnFormToggleBrain(c.name.clone()))
    }))
    .spacing(8)
    .wrap();
    let write_opts = ninox_ui_shared::spawn_form::write_options(&choices, &form.brain_reads);
    let write_row: Element<Message> = if write_opts.is_empty() {
        text("select a writable brain").size(12).font(SANS).color(tokens::RED).into()
    } else {
        row(write_opts.into_iter().map(|n| {
            let selected = n == form.brain_write;
            pill(n.clone(), selected, Message::SpawnFormBrainWrite(n))
        }))
        .spacing(8)
        .wrap()
        .into()
    };
    let catalogue_field: Element<Message> = column![
        field_label("Brains this tree reads"),
        Space::new(0, 8),
        read_chips,
        Space::new(0, 12),
        field_label("Writes to"),
        Space::new(0, 8),
        write_row,
    ]
    .spacing(0)
    .into();

    // ── Type (MLOPS-5416), Standalone only — an Orchestrator kind's type is
    // "Orchestrator" by definition (it's what `can_spawn` resolution keys
    // on), so there's nothing to pick for it; recorded implicitly in
    // SpawnFormConfirm rather than offered here (round-2 review on PR
    // #282). Never a hidden type (e.g. the prompt agent):
    // `user_facing_names()`, not `names()`. ───────────────────────────────
    let type_field: Option<Element<Message>> = (form.kind == SpawnKind::Standalone).then(|| {
        let worker_types = state.config.worker_type_registry().user_facing_names();
        let type_selected = worker_types.iter().find(|n| *n == &form.worker_type).cloned()
            .or_else(|| worker_types.first().cloned());
        column![
            field_label("Type"),
            Space::new(0, 6),
            pick_list(worker_types, type_selected, Message::SpawnFormWorkerType)
                .font(SANS)
                .text_size(13)
                .padding(Padding { top: 8.0, right: 10.0, bottom: 8.0, left: 10.0 })
                .width(Length::Fill)
                .style(pick_style),
        ]
        .spacing(0)
        .into()
    });

    // ── Initial prompt (optional, multi-line) ────────────────────────────
    let prompt_field = column![
        field_label("Initial prompt"),
        Space::new(0, 6),
        text_editor(&state.spawn_prompt)
            .placeholder("Optional. What should it work on first?")
            .on_action(Message::SpawnFormPrompt)
            .key_binding(prompt_key_binding)
            .height(Length::Fixed(88.0))
            .size(13)
            .font(SANS)
            .padding(Padding { top: 9.0, right: 11.0, bottom: 9.0, left: 11.0 })
            .style(|_t, status| {
                let focused = matches!(status, text_editor::Status::Focused);
                text_editor::Style {
                    background: Background::Color(tokens::BG),
                    border: Border {
                        color: if focused { tokens::TEAL } else { tokens::BORDER },
                        width: if focused { 2.0 } else { 1.0 },
                        radius: 6.0.into(),
                    },
                    icon: tokens::DIM,
                    placeholder: tokens::DIM,
                    value: tokens::TEXT,
                    selection: tokens::VIOLET,
                }
            }),
        Space::new(0, 4),
        text("Sent as its first message once it's ready.").size(11).font(SANS).color(tokens::DIM2),
    ]
    .spacing(0);

    // ── Footer: cost estimate + ghost Cancel + primary Spawn ⌘⏎ ──────────
    let cancel_button = button(text("Cancel").size(12).font(SANS).color(tokens::DIM))
        .on_press(Message::SpawnFormCancel)
        .padding(Padding { top: 8.0, right: 16.0, bottom: 8.0, left: 16.0 })
        .style(|_t, status| {
            let hovered = matches!(status, button::Status::Hovered);
            button::Style {
                background: Some(Background::Color(if hovered { tokens::PANEL2 } else { Color::TRANSPARENT })),
                text_color: tokens::DIM,
                border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
                ..Default::default()
            }
        });

    let spawn_teal_text = Color { r: 0xbf as f32 / 255.0, g: 0xf3 as f32 / 255.0, b: 0xec as f32 / 255.0, a: 1.0 };
    let spawn_button = button(
        row![
            text("Spawn").size(12).font(SANS),
            Space::new(8, 0),
            keycap("⌘", GLYPH_ALT),
            keycap("⏎", GLYPH),
        ]
        .align_y(Alignment::Center),
    )
    .on_press_maybe(can_submit.then_some(Message::SpawnFormConfirm))
    .padding(Padding { top: 8.0, right: 16.0, bottom: 8.0, left: 16.0 })
    .style(move |_t, status| {
        let hovered = can_submit && matches!(status, button::Status::Hovered);
        button::Style {
            background: Some(Background::Color(if can_submit { CARD_SELECTED_BG } else { tokens::PANEL2 })),
            text_color: if can_submit { spawn_teal_text } else { tokens::DIM2 },
            border: Border {
                color: if can_submit { CARD_SELECTED_BORDER } else { tokens::BORDER },
                width: if hovered { 1.5 } else { 1.0 },
                radius: 6.0.into(),
            },
            ..Default::default()
        }
    });

    let est_model = effective_model(form.custom_model.as_deref(), form.model.as_deref(), &spec);
    let historical_costs = state.engine.store
        .cost_samples(&form.harness, est_model.as_deref())
        .unwrap_or_default();
    let estimate = estimate_text(static_prior(&form.harness, est_model.as_deref()), &historical_costs);

    let footer = container(
        row![
            text(estimate).size(11.5).font(MONO).color(tokens::DIM),
            Space::new(Length::Fill, 0),
            cancel_button,
            Space::new(8, 0),
            spawn_button,
        ]
        .spacing(0)
        .align_y(Alignment::Center),
    )
    .padding(Padding { top: 14.0, right: 20.0, bottom: 14.0, left: 20.0 })
    .width(Length::Fill)
    .style(|_| container::Style {
        background: Some(Background::Color(tokens::PANEL2)),
        ..Default::default()
    });

    // ── Guard-refusal feedback ────────────────────────────────────────────
    let error_line: Option<Element<Message>> = form.error.as_ref().map(|msg| {
        text(msg.clone()).size(11.5).font(SANS).color(tokens::RED).into()
    });

    // Final layout: Kind → Name → Workspace → Agent·Model
    // → Brain catalogue → Type (standalone only) → Initial prompt → error
    // (if any) → footer.
    let mut body = column![kind_field, Space::new(0, 18), name_field]
        .padding(Padding { top: 18.0, right: 20.0, bottom: 18.0, left: 20.0 })
        .spacing(0);
    body = body
        .push(Space::new(0, 18))
        .push(workspace_field)
        .push(Space::new(0, 18))
        .push(agent_field)
        .push(Space::new(0, 18))
        .push(catalogue_field);
    if let Some(ty) = type_field {
        body = body.push(Space::new(0, 18)).push(ty);
    }
    body = body.push(Space::new(0, 18)).push(prompt_field);
    if let Some(err) = error_line {
        body = body.push(Space::new(0, 14)).push(err);
    }

    let header_rule = container(Space::new(Length::Fill, 1)).style(|_| container::Style {
        background: Some(Background::Color(tokens::BORDER)),
        ..Default::default()
    });

    // Header and footer stay put; the fields scroll once the dialog would
    // outgrow the window (Standalone with the prompt is ~760px tall).
    let body_max = (state.window_height - MODAL_CHROME_HEIGHT - 2.0 * MODAL_MIN_MARGIN).max(160.0);
    let body = container(iced::widget::scrollable(body).height(Length::Shrink)).max_height(body_max);
    let modal = container(column![header, header_rule, body, footer])
        .width(Length::Fixed(500.0))
        .style(|_| container::Style {
            background: Some(Background::Color(tokens::PANEL)),
            border: Border { color: Color { r: 0x2f as f32 / 255.0, g: 0x37 as f32 / 255.0, b: 0x50 as f32 / 255.0, a: 1.0 }, width: 1.0, radius: 12.0.into() },
            shadow: iced::Shadow { color: Color { a: 0.6, ..Color::BLACK }, offset: iced::Vector::new(0.0, 24.0), blur_radius: 60.0 },
            ..Default::default()
        });

    container(modal)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .style(|_| container::Style {
            background: Some(Background::Color(Color { r: 5.0 / 255.0, g: 7.0 / 255.0, b: 11.0 / 255.0, a: 0.66 })),
            ..Default::default()
        })
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    // static_prior / estimate_text / effective_model are now covered by
    // ninox-ui-shared::spawn_form's own tests.

    #[test]
    fn default_form_selects_claude_code() {
        assert_eq!(SpawnForm::default().harness, "claude-code");
    }

    #[test]
    fn default_form_selects_implementor_as_the_worker_type() {
        assert_eq!(SpawnForm::default().worker_type, "Implementor");
    }
}
