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

/// Minimum number of historical non-zero `cost_usd` samples (for the exact
/// harness/model pairing) required before the footer estimate switches from
/// the static rough-prior range to a data-driven average.
const MIN_HISTORY_SAMPLES: usize = 3;

/// Rough static per-session priors for claude-code's curated models —
/// relative pricing only (fable-5 highest, haiku-4.5 lowest), **not** wired
/// to any pricing API. Everything else has no prior and builds its
/// estimate from filed history.
pub fn static_prior(harness: &str, model: Option<&str>) -> Option<(f64, f64)> {
    if harness != "claude-code" {
        return None;
    }
    match model {
        Some("claude-fable-5")   => Some((4.0, 8.0)),
        Some("claude-opus-4-8")  => Some((2.0, 4.0)),
        Some("claude-sonnet-5")  => Some((1.0, 2.0)),
        Some("claude-haiku-4-5") => Some((0.4, 1.2)),
        _                        => None,
    }
}

/// Footer cost-estimate text. Falls back to the static prior range until at
/// least `MIN_HISTORY_SAMPLES` non-zero cost samples exist for the exact
/// harness/model pairing (see `Store::cost_samples`, populated by the usage
/// poller — `ninox_core::lifecycle::usage`/`poller::poll_usage`), then shows
/// the historical per-session average. Pure function — no I/O — so it's
/// unit-testable without a live store.
pub fn estimate_text(prior: Option<(f64, f64)>, historical_costs: &[f64]) -> String {
    if historical_costs.len() >= MIN_HISTORY_SAMPLES {
        let avg = historical_costs.iter().sum::<f64>() / historical_costs.len() as f64;
        format!("≈ ${avg:.2} / session · from {} filed", historical_costs.len())
    } else if let Some((lo, hi)) = prior {
        format!("est. ${lo:.0}–{hi:.0} / session")
    } else {
        "est. — builds from filed sessions".to_string()
    }
}

/// The model a confirm would launch with: custom text (trimmed, non-empty)
/// wins over the picker selection, which wins over the spec's default.
pub fn effective_model(form: &SpawnForm, spec: &ninox_core::harness::HarnessSpec) -> Option<String> {
    if let Some(c) = &form.custom_model {
        let t = c.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    form.model.clone().or_else(|| spec.model.clone())
}

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
    /// Index into `AppConfig::catalogue_options()` — applies to both kinds.
    pub catalogue_idx: usize,
    /// Human-readable refusal reason from the last confirm attempt, if any.
    /// Cleared whenever the user edits any field. Rendered above the footer.
    pub error:         Option<String>,
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
            catalogue_idx: 0,
            error:         None,
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// A transparent, underlined `text_input` matching the ledger-field pattern
/// used elsewhere (see `filter_bar::filter_bar`) but sized for a serif form
/// field. Delegates to the shared helper (also used by `catalogue_modal`).
fn styled_input<'a>(s: &'a ColorScheme, placeholder: &'a str, value: &'a str) -> text_input::TextInput<'a, Message> {
    style::serif_underlined_input(s, placeholder, value)
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
            style::toggle_segment_glyph(
                s,
                "⬡",
                "Orchestrator",
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

    // ── Agent chips + Model picker ───────────────────────────────────────────
    // One chip per ENABLED registry harness; picking a chip picks the
    // harness, with a model picker beside it (options: models_cmd discovery
    // → known_models → configured value, then `custom…` last).
    let registry = state.config.registry();
    let spec = registry.spec(&form.harness);
    let agent_chips = row(registry.enabled_names().into_iter().map(|name| {
        let selected = name == form.harness;
        chip(s, name.clone(), selected, Message::SpawnFormHarness(name))
    }))
    .spacing(8);

    let discovered = state.model_lists.get(&form.harness).and_then(|m| m.as_deref());
    let configured = form.model.clone().or_else(|| spec.model.clone());
    let options = crate::models::model_options(&spec, discovered, configured.as_deref());
    let picker_selected = if form.custom_model.is_some() {
        Some(crate::models::CUSTOM_SENTINEL.to_string())
    } else {
        configured
    };
    let model_picker = pick_list(options, picker_selected, Message::SpawnFormModel)
        .placeholder("harness default")
        .font(MONO)
        .text_size(12)
        .padding([6, 10])
        .style(pick_style(s));

    let agent_col = column![micro_label("Agent", s.ink_2), Space::new(0, 8), agent_chips].spacing(0);
    let mut model_col = column![micro_label("Model", s.ink_2), Space::new(0, 8), model_picker].spacing(0);
    if form.custom_model.is_some() {
        model_col = model_col.push(Space::new(0, 6)).push(
            text_input("model id", form.custom_model.as_deref().unwrap_or(""))
                .on_input(Message::SpawnFormCustomModel)
                .font(MONO)
                .size(12)
                .padding([4, 2])
                .style(style::underlined_input_style(s)),
        );
    }
    let agent_field = row![agent_col, Space::new(16, 0), model_col].align_y(Alignment::Start);

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

    let spawn_label_color = if can_submit { s.card } else { s.faint };
    let spawn_button = button(
        row![
            text("SPAWN ").size(12).font(SANS_BOLD).color(spawn_label_color),
            text("⬡").size(12).font(style::GLYPH).color(spawn_label_color),
        ]
        .align_y(Alignment::Center),
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

    let est_model = effective_model(form, &spec);
    let historical_costs = state.engine.store
        .cost_samples(&form.harness, est_model.as_deref())
        .unwrap_or_default();
    let estimate = estimate_text(static_prior(&form.harness, est_model.as_deref()), &historical_costs);

    let footer = row![
        text(estimate).size(10).font(MONO).color(s.faint),
        Space::new(Length::Fill, 0),
        cancel_button,
        spawn_button,
    ]
    .spacing(12)
    .align_y(Alignment::Center);

    // ── Guard-refusal feedback: a confirm attempt was blocked. Rendered
    // above the footer so it reads as "why the button didn't do anything"
    // rather than a form-field-level validation error.
    let error_line: Option<Element<Message>> = form.error.as_ref().map(|msg| {
        row![
            text("⚑").size(11).font(style::GLYPH).color(s.accent),
            Space::new(6, 0),
            text(msg.clone()).size(11).font(SANS_BOLD).color(s.accent),
        ]
        .align_y(Alignment::Center)
        .into()
    });

    // Final layout: Entry type → Name → Workspace (standalone only) →
    // Catalogue → Agent·Model → error (if any) → footer.
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
        .push(agent_field);
    if let Some(err) = error_line {
        body = body.push(Space::new(0, 14)).push(err);
    }
    body = body.push(Space::new(0, 22)).push(footer);

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_priors_cover_claude_models_only() {
        assert_eq!(static_prior("claude-code", Some("claude-fable-5")),   Some((4.0, 8.0)));
        assert_eq!(static_prior("claude-code", Some("claude-opus-4-8")),  Some((2.0, 4.0)));
        assert_eq!(static_prior("claude-code", Some("claude-sonnet-5")),  Some((1.0, 2.0)));
        assert_eq!(static_prior("claude-code", Some("claude-haiku-4-5")), Some((0.4, 1.2)));
        assert_eq!(static_prior("claude-code", None), None);
        assert_eq!(static_prior("codex", Some("gpt-4o")), None);
    }

    #[test]
    fn estimate_text_falls_back_to_static_range_when_no_history() {
        assert_eq!(estimate_text(Some((2.0, 4.0)), &[]), "est. $2–4 / session");
    }

    #[test]
    fn estimate_text_falls_back_below_min_sample_count() {
        // 2 samples — one short of MIN_HISTORY_SAMPLES (3).
        assert_eq!(estimate_text(Some((2.0, 4.0)), &[1.0, 3.0]), "est. $2–4 / session");
    }

    #[test]
    fn estimate_text_uses_historical_average_at_min_sample_count() {
        assert_eq!(estimate_text(Some((4.0, 8.0)), &[3.0, 5.0, 4.0]),
                   "≈ $4.00 / session · from 3 filed");
        // History wins even with no prior (e.g. a custom harness).
        assert_eq!(estimate_text(None, &[3.0, 5.0, 4.0]),
                   "≈ $4.00 / session · from 3 filed");
    }

    #[test]
    fn estimate_text_without_prior_or_history_says_so() {
        assert_eq!(estimate_text(None, &[]), "est. — builds from filed sessions");
    }

    #[test]
    fn effective_model_prefers_custom_text() {
        let spec = ninox_core::harness::HarnessSpec { model: Some("spec-m".into()), ..Default::default() };
        let mut f = SpawnForm {
            model:        Some("picked".into()),
            custom_model: Some("  typed  ".into()),
            ..SpawnForm::default()
        };
        assert_eq!(effective_model(&f, &spec).as_deref(), Some("typed"));
        f.custom_model = Some("   ".into());          // blank custom → picker value
        assert_eq!(effective_model(&f, &spec).as_deref(), Some("picked"));
        f.model = None;                                // nothing picked → spec default
        assert_eq!(effective_model(&f, &spec).as_deref(), Some("spec-m"));
    }

    #[test]
    fn default_form_selects_claude_code() {
        assert_eq!(SpawnForm::default().harness, "claude-code");
    }
}
