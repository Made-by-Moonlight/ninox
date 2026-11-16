//! The Worker Types tab — `WorkerTypes.dc.html`'s list+form editor, over
//! `ninox_core::worker_type_cli`'s backend (landed in MLOPS-5416 PR1).
//! Replaces `View::WorkerTypes`'s placeholder.
//!
//! Styled with the v1 shell's dark/sans token set (`shell::tokens`), the
//! same one `brain_list.rs` uses — not the Field Notes serif/warm system
//! (`crate::style`'s `ColorScheme`-driven helpers), which the user rejected
//! for this tab in review.
//!
//! Every mutating call below passes `caller_session_id: None`: the GUI has
//! no ninox-managed pane of its own, so by `authorize_mutation`'s own
//! contract it IS "the user" — unconditionally authorized, including for
//! `can_manage_types`, which no agent caller may ever set. (`Fields` has no
//! `hidden` field, so this tab can't create or reveal a hidden/system type
//! either way — `list()` filters those out, matching the CLI and TUI.)
//! See `worker_type_cli::authorize_mutation`'s doc comment.

use iced::widget::{button, column, container, pick_list, row, scrollable, text, text_editor, text_input, Space};
use iced::{Alignment, Background, Border, Color, Element, Length};

use ninox_core::worker_type_cli::{self, Entry, Fields};

use super::shell::tokens;
use crate::app::{App, Message};
use crate::style::{self, MONO, SANS, SANS_BOLD};

/// One editable draft: either a snapshot of an existing type (`is_new:
/// false`, `original_name: Some(canonical name)`) or a blank "+ New type"
/// draft (`is_new: true`, `original_name: None`).
pub struct WorkerTypeForm {
    pub is_new:           bool,
    pub original_name:    Option<String>,
    pub built_in:         bool,
    pub name:             String,
    pub use_when:         String,
    /// What `use_when` was when the form loaded: saving an untouched form
    /// must not pin a built-in's description into config.
    pub use_when_loaded:  String,
    pub brief:            text_editor::Content,
    pub harness:          String,
    pub model:            String,
    pub allow:            Vec<String>,
    pub deny:             Vec<String>,
    pub ask:              Vec<String>,
    /// Pending text for each rule list's "add a chip" input — not yet
    /// committed to `allow`/`deny`/`ask`.
    pub allow_draft:      String,
    pub deny_draft:       String,
    pub ask_draft:        String,
    /// `["*"]` for the wildcard, otherwise explicit type names — see
    /// `worker_type_cli::toggle_can_spawn`.
    pub can_spawn:        Vec<String>,
    pub can_manage_types: bool,
    /// The Ninox prompt type: single instance, name/can_spawn/governance
    /// locked, `brief` is an addendum, `deny` holds only the extras on top of
    /// `locked_deny`.
    pub system:           bool,
    pub locked_allow:     Vec<String>,
    pub locked_deny:      Vec<String>,
}

impl WorkerTypeForm {
    pub fn blank() -> Self {
        Self {
            is_new: true, original_name: None, built_in: false,
            name: String::new(), use_when: String::new(), use_when_loaded: String::new(), brief: text_editor::Content::new(),
            harness: String::new(), model: String::new(),
            allow: Vec::new(), deny: Vec::new(), ask: Vec::new(),
            allow_draft: String::new(), deny_draft: String::new(), ask_draft: String::new(),
            can_spawn: Vec::new(),
            can_manage_types: false,
            system: false,
            locked_allow: Vec::new(),
            locked_deny: Vec::new(),
        }
    }

    pub fn from_entry(entry: &Entry) -> Self {
        let spec = &entry.spec;
        let system = ninox_core::worker_type::is_system_type(&spec.name);
        let locked_deny = spec.locked_deny_rules();
        let locked_allow = spec.locked_allow_rules();
        let brief = if system { spec.brief_addendum.as_deref() } else { spec.brief_template.as_deref() };
        Self {
            is_new: false, original_name: Some(spec.name.clone()), built_in: entry.built_in,
            name: ninox_core::worker_type::display_name(&spec.name).to_string(),
            use_when: spec.use_when.clone().unwrap_or_default(),
            use_when_loaded: spec.use_when.clone().unwrap_or_default(),
            brief: text_editor::Content::with_text(brief.unwrap_or("")),
            harness: spec.harness.clone(),
            model: spec.model.clone().unwrap_or_default(),
            allow: spec.permissions.allow.iter().filter(|r| !locked_allow.contains(r)).cloned().collect(),
            deny: spec.permissions.deny.iter().filter(|r| !locked_deny.contains(r)).cloned().collect(),
            ask: spec.permissions.ask.clone(),
            allow_draft: String::new(), deny_draft: String::new(), ask_draft: String::new(),
            can_spawn: spec.can_spawn.clone(),
            can_manage_types: spec.can_manage_types,
            system,
            locked_allow,
            locked_deny,
        }
    }

    /// The `Fields` this draft would apply — shared by `create`/`edit`
    /// dispatch (`WorkerTypeSave`'s handler picks which to call).
    fn to_fields(&self) -> Fields {
        Fields {
            // `text_editor::Content::text()` always ends in a trailing
            // newline (one per line, joined) — trimmed here so a one-line
            // brief doesn't silently gain a `\n` nobody typed.
            brief: Some(self.brief.text().trim_end_matches('\n').to_string()),
            use_when: (!self.system && self.use_when.trim() != self.use_when_loaded.trim()).then(|| self.use_when.trim().to_string()),
            harness: (!self.harness.trim().is_empty()).then(|| self.harness.trim().to_string()),
            model: Some(self.model.trim().to_string()),
            allow: self.allow.clone(),
            deny: self.deny.clone(),
            ask: self.ask.clone(),
            clear_allow: self.allow.is_empty(),
            clear_deny: self.deny.is_empty(),
            clear_ask: self.ask.is_empty(),
            can_spawn: (!self.system).then(|| self.can_spawn.join(",")),
            can_manage_types: (!self.system).then_some(self.can_manage_types),
            permissions_enabled: None,
        }
    }
}

pub struct WorkerTypesState {
    pub entries:     Vec<Entry>,
    pub form:        WorkerTypeForm,
    pub error:       Option<String>,
    pub notice:      Option<String>,
    pub import_path: String,
}

impl Default for WorkerTypesState {
    fn default() -> Self {
        Self { entries: Vec::new(), form: WorkerTypeForm::blank(), error: None, notice: None, import_path: String::new() }
    }
}

impl WorkerTypesState {
    /// Re-reads the list from disk and rebuilds the form for whichever name
    /// is already selected (or the first entry, the first time). Called on
    /// `NavigateWorkerTypes` and after every successful mutation, so the
    /// tab always reflects what's actually on disk rather than an
    /// optimistic in-memory guess.
    ///
    /// On a read failure (e.g. a malformed config), leaves `entries`/`form`
    /// untouched and surfaces the error via `self.error` instead of
    /// silently falling back to an empty list and a blank draft — a reload
    /// failure right after a successful save/delete/etc. would otherwise
    /// look like that action wiped everything out.
    pub fn refresh(&mut self, keep_selected: bool) {
        let entries = match worker_type_cli::editor_list() {
            Ok(entries) => entries,
            Err(e) => {
                self.error = Some(e.to_string());
                return;
            }
        };
        let selected = keep_selected.then(|| self.form.original_name.clone()).flatten();
        self.entries = entries;
        match selected.and_then(|name| self.entries.iter().find(|e| e.spec.name == name)) {
            Some(entry) => self.form = WorkerTypeForm::from_entry(entry),
            None => match self.entries.first() {
                Some(entry) => self.form = WorkerTypeForm::from_entry(entry),
                None => self.form = WorkerTypeForm::blank(),
            },
        }
    }
}

/// Comma-separated, trimmed, empties dropped — mirrors
/// `ninox-tui/src/tui/worker_types.rs`'s `join_rules`/`split_rules`, and
/// lets a rule chip row's "add" input still accept a pasted comma list
/// (e.g. "Read, Grep") and split it into separate chips in one go.
pub fn split_rules(s: &str) -> Vec<String> {
    s.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect()
}

/// Commits `draft`'s rule(s) into `list` (deduped, draft split on commas so
/// "A, B" adds two chips at once) and clears `draft`.
pub fn add_rule(list: &mut Vec<String>, draft: &mut String) {
    for r in split_rules(draft) {
        if !list.contains(&r) {
            list.push(r);
        }
    }
    draft.clear();
}

/// Groups `entries` into (system, built-ins, customs) for the list's
/// labeled sections, alphabetical within each (already alphabetical from
/// `worker_type_cli::list()`, preserved here).
pub fn grouped(entries: &[Entry]) -> (Vec<&Entry>, Vec<&Entry>, Vec<&Entry>) {
    let (system, rest): (Vec<&Entry>, Vec<&Entry>) =
        entries.iter().partition(|e| ninox_core::worker_type::is_system_type(&e.spec.name));
    let (built_in, custom) = rest.into_iter().partition(|e| e.built_in);
    (system, built_in, custom)
}

pub const SYSTEM_BADGE: &str = "System · single instance";


/// Writes `spec`'s resolved form to `<exports dir>/<slug>.toml`, numbered
/// past an existing export that belongs to a DIFFERENT type (so "Security
/// Reviewer" and "security-reviewer" never silently clobber each other) —
/// mirrors `ninox-tui`'s `export_type`/`export_path`.
pub fn export_to_file(name: &str) -> anyhow::Result<std::path::PathBuf> {
    let snippet = worker_type_cli::export(&[name.to_string()])?;
    let dir = ninox_core::config::AppConfig::worker_type_exports_dir();
    std::fs::create_dir_all(&dir)?;
    let base = ninox_core::slugify(name);
    let mut path = dir.join(format!("{base}.toml"));
    let mut n = 2;
    while path.exists() {
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        if existing.contains(&format!("name = \"{name}\"")) {
            break;
        }
        path = dir.join(format!("{base}-{n}.toml"));
        n += 1;
    }
    std::fs::write(&path, snippet)?;
    Ok(path)
}

/// Runs the save: `create` for a new draft, `edit` for an existing one.
/// Shared by `App::apply`'s `WorkerTypeSave` handler and its tests.
pub fn save(store: &ninox_core::store::Store, form: &WorkerTypeForm) -> anyhow::Result<ninox_core::worker_type::WorkerTypeSpec> {
    let fields = form.to_fields();
    if form.is_new {
        // `create`'s own `brief` parameter is immediately superseded by
        // `fields.brief` (always `Some` here) inside `apply_fields` — passed
        // through anyway so `create` never sees an empty-then-overwritten
        // brief as a meaningful state change.
        let brief = fields.brief.clone().unwrap_or_default();
        worker_type_cli::create(store, None, form.name.trim(), &brief, fields)
    } else {
        let name = form.original_name.as_deref().unwrap_or_default();
        worker_type_cli::edit(store, None, name, fields)
    }
}

// ---------------------------------------------------------------------------
// View
// ---------------------------------------------------------------------------

const LIST_WIDTH: f32 = 300.0;

fn input_style(_theme: &iced::Theme, _status: text_input::Status) -> text_input::Style {
    text_input::Style {
        background: Background::Color(tokens::PANEL2),
        border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
        icon: tokens::DIM, placeholder: tokens::DIM, value: tokens::TEXT,
        selection: tokens::TEAL,
    }
}

fn pick_list_style(_theme: &iced::Theme, _status: pick_list::Status) -> pick_list::Style {
    pick_list::Style {
        text_color: tokens::TEXT, placeholder_color: tokens::DIM, handle_color: tokens::DIM,
        background: Background::Color(tokens::PANEL2),
        border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
    }
}

pub fn worker_types_view(app: &App) -> Element<'_, Message> {
    let (system, built_in, custom) = grouped(&app.worker_types.entries);

    let import_control = container(
        row![
            style::micro_label("import", tokens::DIM2),
            text_input("path to .toml file", &app.worker_types.import_path)
                .on_input(Message::WorkerTypeImportPathChanged)
                .font(MONO).size(11).padding([4, 6]).width(Length::Fixed(190.0))
                .style(|_theme, _status| text_input::Style {
                    background: Background::Color(Color::TRANSPARENT),
                    border: Border::default(),
                    icon: tokens::DIM, placeholder: tokens::DIM, value: tokens::TEXT,
                    selection: tokens::TEAL,
                }),
        ]
        .spacing(8)
        .align_y(Alignment::Center)
        .padding([0, 4]),
    )
    .style(|_| container::Style {
        background: Some(Background::Color(tokens::PANEL2)),
        border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
        ..Default::default()
    });

    let header = row![
        column![
            text("Worker Types").size(13).font(SANS).color(tokens::TEXT),
            text("named, reusable configs — brief template, permission profile, default harness. Not a closed set.")
                .font(SANS).size(11).color(tokens::DIM),
        ].spacing(3),
        Space::new(Length::Fill, 0),
        import_control,
        action_button("Import", tokens::TEXT, Message::WorkerTypeImport),
        action_button("+ New type", tokens::TEAL, Message::WorkerTypeNewDraft),
    ]
    .spacing(10)
    .align_y(Alignment::Center)
    .padding([12, 20]);

    let list = scrollable(
        column![
            section_label("System"),
            column(system.into_iter().map(|e| type_row(app, e)).collect::<Vec<_>>()).spacing(2),
            Space::new(0, 14),
            section_label("Built-in"),
            column(built_in.into_iter().map(|e| type_row(app, e)).collect::<Vec<_>>()).spacing(2),
            Space::new(0, 14),
            section_label("Custom"),
            column(custom.into_iter().map(|e| type_row(app, e)).collect::<Vec<_>>()).spacing(2),
        ]
        .spacing(4)
        .padding(14),
    )
    .width(Length::Fixed(LIST_WIDTH));

    let list_pane = container(list)
        .width(Length::Fixed(LIST_WIDTH))
        .height(Length::Fill)
        .style(|_| container::Style {
            background: Some(Background::Color(tokens::PANEL)),
            border: Border { color: tokens::BORDER, width: 1.0, ..Default::default() },
            ..Default::default()
        });

    let form_pane = container(form_view(app))
        .width(Length::Fill)
        .height(Length::Fill)
        .padding([18, 26]);

    column![
        header,
        style::hline(tokens::BORDER, 1.0),
        row![list_pane, form_pane].height(Length::Fill),
    ]
    .height(Length::Fill)
    .into()
}

fn section_label<'a>(label: &str) -> Element<'a, Message> {
    style::micro_label(label, tokens::DIM2).into()
}

fn type_row<'a>(app: &'a App, entry: &'a Entry) -> Element<'a, Message> {
    let selected = app.worker_types.form.original_name.as_deref() == Some(entry.spec.name.as_str());
    let system = ninox_core::worker_type::is_system_type(&entry.spec.name);
    let (badge, badge_color) = match (system, entry.built_in) {
        (true, _) => (SYSTEM_BADGE, tokens::AMBER),
        (false, true) => ("built-in", tokens::GREEN),
        (false, false) => ("custom", tokens::TEAL),
    };
    let label = column![
        row![
            style::stamp(badge, badge_color),
            text(ninox_core::worker_type::display_name(&entry.spec.name).to_string()).font(SANS).size(12.5).color(tokens::TEXT),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
        text(ninox_core::model_resolution::type_model_summary(&entry.spec, &app.config)).font(MONO).size(10.5).color(tokens::DIM2),
    ]
    .spacing(3);

    button(label)
        .on_press(Message::WorkerTypeSelect(entry.spec.name.clone()))
        .padding([8, 10])
        .width(Length::Fill)
        .style(move |_theme, status| {
            let hovered = matches!(status, button::Status::Hovered);
            button::Style {
                background: Some(Background::Color(if selected {
                    tokens::PANEL2
                } else if hovered {
                    Color { a: 0.5, ..tokens::PANEL2 }
                } else {
                    Color::TRANSPARENT
                })),
                text_color: tokens::TEXT,
                border: Border {
                    color: if selected { tokens::TEAL } else { Color::TRANSPARENT },
                    width: 1.0, radius: 5.0.into(),
                },
                ..Default::default()
            }
        })
        .into()
}

fn field_label<'a>(label: &str) -> Element<'a, Message> {
    column![Space::new(0, 14), style::micro_label(label, tokens::DIM2)].spacing(4).into()
}

/// A plain bordered action button — unlike `iced::widget::button`'s own
/// unstyled default (which rendered as a solid fill with no visible text
/// against this tab's background), every button here gets an explicit
/// style, mirroring `type_row`'s own bordered-pill treatment.
fn action_button<'a>(label: &str, text_color: Color, on_press: Message) -> Element<'a, Message> {
    // Advanced: the toggles' "✕" isn't in Inter, and Basic shaping doesn't
    // fall back.
    button(text(label.to_string()).font(SANS_BOLD).size(11.5).shaping(iced::widget::text::Shaping::Advanced).color(text_color))
        .on_press(on_press)
        .padding([8, 14])
        .style(move |_theme, status| {
            let hovered = matches!(status, button::Status::Hovered);
            button::Style {
                background: Some(Background::Color(if hovered { tokens::PANEL2 } else { Color::TRANSPARENT })),
                text_color,
                border: Border { color: tokens::BORDER, width: 1.0, radius: 4.0.into() },
                ..Default::default()
            }
        })
        .into()
}

/// One rule chip: the rule text plus a "✕" that removes it — tapping
/// anywhere on the chip removes it, same affordance as Brain's active
/// tag/repo filter chips.
fn rule_chip<'a>(label: String, on_remove: Message) -> Element<'a, Message> {
    // Advanced: Inter has no "✕", and Basic shaping doesn't fall back.
    button(text(format!("{label} ✕")).size(10.5).font(SANS_BOLD).shaping(iced::widget::text::Shaping::Advanced).color(tokens::TEXT))
        .on_press(on_remove)
        .padding([3, 10])
        .style(|_theme, status| button::Style {
            background: Some(Background::Color(if matches!(status, button::Status::Hovered) {
                Color { a: 0.5, ..tokens::PANEL2 }
            } else {
                tokens::PANEL2
            })),
            border: Border { color: tokens::BORDER, width: 1.0, radius: 10.0.into() },
            ..Default::default()
        })
        .into()
}

/// One `allow`/`deny`/`ask` row: existing rules as removable chips, plus an
/// inline input to add a new one (Enter or the adjoining `+` button) — a
/// comma-separated string typed by hand (the old design) gave no sense of
/// how many rules there were or which one a given substring belonged to.
fn rule_list_row<'a>(
    kind: &str,
    rules: &'a [String],
    draft: &'a str,
    on_draft_change: fn(String) -> Message,
    on_add: Message,
    on_remove: fn(usize) -> Message,
) -> Element<'a, Message> {
    let mut chips = row![container(style::micro_label(kind, tokens::DIM2)).width(Length::Fixed(40.0))]
        .spacing(6)
        .align_y(Alignment::Center);
    for (i, rule) in rules.iter().enumerate() {
        chips = chips.push(rule_chip(rule.clone(), on_remove(i)));
    }
    chips = chips.push(
        text_input("add rule…", draft)
            .on_input(on_draft_change)
            .on_submit(on_add.clone())
            .font(MONO).size(11).padding([4, 8]).width(Length::Fixed(130.0))
            .style(input_style),
    );
    chips = chips.push(
        button(text("+").size(13).font(SANS).color(tokens::DIM))
            .on_press(on_add)
            .padding([2, 7])
            .style(|_theme, status| button::Style {
                background: None,
                text_color: if matches!(status, button::Status::Hovered) { tokens::TEAL } else { tokens::DIM },
                border: Border::default(),
                ..Default::default()
            }),
    );
    chips.into()
}

/// Built-in rules a system type always keeps: plain chips with no ✕.
fn locked_rules_row<'a>(kind: &str, rules: &'a [String]) -> Element<'a, Message> {
    let chip = |rule: &'a String| -> Element<'a, Message> {
        container(text(rule.clone()).size(10.5).font(MONO).color(tokens::DIM))
            .padding([3, 10])
            .style(|_| container::Style {
                border: Border { color: tokens::BORDER, width: 1.0, radius: 10.0.into() },
                ..Default::default()
            })
            .into()
    };
    column![
        text(format!("{kind} — built-in, always applied")).size(10.5).font(SANS).color(tokens::DIM2),
        row(rules.iter().map(chip)).spacing(6).wrap(),
    ]
    .spacing(6)
    .into()
}

/// The `*`/type-name chip row for `can_spawn` — a multi-select rather than
/// a hand-typed comma list, wrapped in fixed-size rows since iced 0.13 has
/// no built-in flex-wrap layout.
const CAN_SPAWN_CHIPS_PER_ROW: usize = 5;

fn can_spawn_chip<'a>(label: String, active: bool) -> Element<'a, Message> {
    let msg = Message::WorkerTypeCanSpawnToggle(label.clone());
    button(text(label).size(10.5).font(SANS).color(tokens::TEXT))
        .on_press(msg)
        .padding([3, 10])
        .style(move |_theme, _status| button::Style {
            background: Some(Background::Color(if active { tokens::PANEL } else { tokens::PANEL2 })),
            border: Border { color: if active { tokens::TEAL } else { tokens::BORDER }, width: 1.0, radius: 10.0.into() },
            ..Default::default()
        })
        .into()
}

fn can_spawn_selector(app: &App) -> Element<'_, Message> {
    let form = &app.worker_types.form;
    let wildcard_on = form.can_spawn.iter().any(|n| n == "*");
    let mut chips = vec![can_spawn_chip("*".to_string(), wildcard_on)];
    for entry in app.worker_types.entries.iter().filter(|e| !ninox_core::worker_type::is_system_type(&e.spec.name)) {
        let name = entry.spec.name.clone();
        let on = !wildcard_on && form.can_spawn.iter().any(|n| n == &name);
        chips.push(can_spawn_chip(name, on));
    }

    let mut rows = column![].spacing(6);
    let mut iter = chips.into_iter();
    loop {
        let chunk: Vec<_> = iter.by_ref().take(CAN_SPAWN_CHIPS_PER_ROW).collect();
        if chunk.is_empty() { break; }
        rows = rows.push(row(chunk).spacing(6));
    }
    rows.into()
}

fn form_view(app: &App) -> Element<'_, Message> {
    let form = &app.worker_types.form;

    let badge = if form.system {
        style::stamp(SYSTEM_BADGE, tokens::AMBER)
    } else {
        style::stamp(
            if form.is_new { "new" } else if form.built_in { "built-in" } else { "custom" },
            if form.is_new || !form.built_in { tokens::TEAL } else { tokens::GREEN },
        )
    };

    let title: Element<Message> = if form.is_new {
        text_input("Name", &form.name)
            .on_input(Message::WorkerTypeNameChanged)
            .font(SANS).size(18)
            .style(input_style)
            .into()
    } else {
        text(form.name.clone()).font(SANS).size(18).color(tokens::TEXT).into()
    };

    let header = row![badge, title].spacing(10).align_y(Alignment::Center);

    let harness_names = app.config.registry().enabled_names();
    let harness_sel = harness_names.iter().find(|n| **n == form.harness).cloned();
    let harness_pick = pick_list(harness_names, harness_sel, Message::WorkerTypeHarnessChanged)
        .font(MONO).text_size(12).padding([6, 10])
        .style(pick_list_style);

    let inherited = ninox_core::model_resolution::type_model_summary(
        &ninox_core::worker_type::WorkerTypeSpec { name: form.name.clone(), harness: form.harness.clone(), ..Default::default() },
        &app.config,
    );
    let model_placeholder = inherited.trim_start_matches("model: ").to_string();
    let model_input = text_input(&model_placeholder, &form.model)
        .on_input(Message::WorkerTypeModelChanged)
        .font(MONO).size(12).padding([6, 10])
        .style(input_style);

    let brief_editor = container(
        text_editor(&form.brief)
            .on_action(Message::WorkerTypeBriefAction)
            .font(SANS).size(12.5).padding(8)
            .style(|_theme, _status| text_editor::Style {
                background: Background::Color(Color::TRANSPARENT),
                border: Border::default(),
                icon: tokens::DIM, placeholder: tokens::DIM, value: tokens::TEXT,
                selection: Color { a: 0.35, ..tokens::TEAL },
            }),
    )
    .style(|_| container::Style {
        background: Some(Background::Color(tokens::PANEL2)),
        border: Border { color: tokens::BORDER, width: 1.0, radius: 4.0.into() },
        ..Default::default()
    })
    .height(Length::Fixed(90.0));

    let toggle = |label: &'static str, on: bool, on_press: Message| {
        let word = if on { format!("✓ {label}") } else { format!("✕ {label}") };
        action_button(&word, if on { tokens::GREEN } else { tokens::DIM }, on_press)
    };

    let mut actions = row![action_button("Save", tokens::TEAL, Message::WorkerTypeSave)].spacing(10);
    if form.built_in {
        actions = actions.push(action_button("Reset", tokens::TEXT, Message::WorkerTypeReset));
    } else if !form.is_new {
        actions = actions.push(action_button("Delete", tokens::RED, Message::WorkerTypeDelete));
    }
    if !form.is_new {
        actions = actions.push(action_button("Export", tokens::TEXT, Message::WorkerTypeExport));
    }

    let mut body = column![header].spacing(6);
    if form.system {
        body = body.push(text(ninox_core::worker_type::SYSTEM_APPLIES_NOTE).font(SANS).size(11.5).color(tokens::DIM2));
    }
    if !form.system {
        body = body.push(field_label("Use when"));
        body = body.push(
            text_input("one line: when orchestrators, the planner and the Ninox prompt should pick this type", &form.use_when)
                .on_input(Message::WorkerTypeUseWhenChanged)
                .font(SANS).size(12.5).padding([6, 10])
                .style(input_style),
        );
    }
    body = body.push(field_label(if form.system { "Additional instructions (appended to the built-in brief)" } else { "Brief template" }));
    body = body.push(brief_editor);
    body = body.push(field_label("Default harness / model"));
    if form.system {
        let fixed = text(form.harness.clone()).font(MONO).size(12).color(tokens::DIM);
        body = body.push(row![container(fixed).padding([6, 10]), model_input].spacing(10).align_y(Alignment::Center));
        body = body.push(text("Harness fixed: its safety rules are enforced by Claude Code.").font(SANS).size(11).color(tokens::DIM2));
    } else {
        body = body.push(row![harness_pick, model_input].spacing(10));
    }
    body = body.push(field_label("Permission profile"));
    if !form.locked_allow.is_empty() {
        body = body.push(locked_rules_row("allow", &form.locked_allow));
    }
    body = body.push(rule_list_row(if form.system { "+allow" } else { "allow" }, &form.allow, &form.allow_draft, Message::WorkerTypeAllowDraftChanged, Message::WorkerTypeAllowRuleAdd, Message::WorkerTypeAllowRuleRemove));
    if !form.locked_deny.is_empty() {
        body = body.push(locked_rules_row("deny", &form.locked_deny));
    }
    body = body.push(rule_list_row(if form.system { "+deny" } else { "deny" }, &form.deny, &form.deny_draft, Message::WorkerTypeDenyDraftChanged, Message::WorkerTypeDenyRuleAdd, Message::WorkerTypeDenyRuleRemove));
    body = body.push(rule_list_row("ask", &form.ask, &form.ask_draft, Message::WorkerTypeAskDraftChanged, Message::WorkerTypeAskRuleAdd, Message::WorkerTypeAskRuleRemove));
    if form.system {
        body = body.push(field_label("Locked"));
        body = body.push(
            text(format!(
                "Exactly one instance; never spawnable (not by `ninox spawn`, not by any type's can spawn). \
                 Name, harness, built-in allow and deny rules (above) and can manage types ({}) are fixed.",
                if form.can_manage_types { "yes" } else { "no" },
            ))
            .font(SANS).size(11.5).color(tokens::DIM2),
        );
    } else {
        body = body.push(field_label("Can spawn"));
        body = body.push(can_spawn_selector(app));
        body = body.push(field_label("Governance"));
        body = body.push(row![
            toggle("can manage types", form.can_manage_types, Message::WorkerTypeToggleCanManageTypes),
        ].spacing(10));
    }

    if let Some(err) = &app.worker_types.error {
        body = body.push(column![Space::new(0, 10), text(err.clone()).font(SANS).size(11.5).color(tokens::RED)]);
    }
    if let Some(notice) = &app.worker_types.notice {
        body = body.push(column![Space::new(0, 10), text(notice.clone()).font(SANS).size(11.5).color(tokens::GREEN)]);
    }

    body = body.push(column![Space::new(0, 18), actions]);

    scrollable(body).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninox_core::store::Store;

    fn scratch_store() -> Store {
        Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap()
    }

    fn in_scratch_config<T>(f: impl FnOnce() -> T) -> T {
        let _iso = ninox_core::test_isolation::isolate();
        f()
    }

    #[test]
    fn split_rules_trims_and_drops_empties() {
        assert_eq!(split_rules("Read, Grep,  Bash(ls:*) "), vec!["Read", "Grep", "Bash(ls:*)"]);
        assert_eq!(split_rules(""), Vec::<String>::new());
    }

    #[test]
    fn add_rule_splits_a_pasted_comma_list_and_dedupes_against_existing() {
        let mut list = vec!["Read".to_string()];
        let mut draft = "Read, Grep, Bash(ls:*)".to_string();
        add_rule(&mut list, &mut draft);
        assert_eq!(list, vec!["Read".to_string(), "Grep".to_string(), "Bash(ls:*)".to_string()]);
        assert!(draft.is_empty());
    }

    #[test]
    fn grouped_partitions_built_in_from_custom() {
        in_scratch_config(|| {
            let entries = worker_type_cli::list().unwrap();
            let (_system, built_in, custom) = grouped(&entries);
            assert!(built_in.iter().all(|e| e.built_in));
            assert!(custom.iter().all(|e| !e.built_in));
            assert!(built_in.iter().any(|e| e.spec.name == "Implementor"));
        });
    }

    #[test]
    fn to_fields_reflects_the_form() {
        let mut form = WorkerTypeForm::blank();
        form.brief = text_editor::Content::with_text("Do it: {prompt}");
        form.harness = "codex".into();
        form.model = "o3".into();
        form.allow = vec!["Read".to_string(), "Grep".to_string()];
        form.can_spawn = vec!["Researcher".to_string()];
        form.can_manage_types = true;

        let fields = form.to_fields();
        assert_eq!(fields.brief.as_deref(), Some("Do it: {prompt}"));
        assert_eq!(fields.harness.as_deref(), Some("codex"));
        assert_eq!(fields.model.as_deref(), Some("o3"));
        assert_eq!(fields.allow, vec!["Read".to_string(), "Grep".to_string()]);
        assert_eq!(fields.can_spawn.as_deref(), Some("Researcher"));
        assert_eq!(fields.can_manage_types, Some(true));
    }

    #[test]
    fn save_creates_a_new_type_when_is_new() {
        in_scratch_config(|| {
            let store = scratch_store();
            let mut form = WorkerTypeForm::blank();
            form.name = "GuiCreated".into();
            form.brief = text_editor::Content::with_text("x: {prompt}");
            let spec = save(&store, &form).unwrap();
            assert_eq!(spec.name, "GuiCreated");
            assert!(worker_type_cli::show("GuiCreated").is_ok());
        });
    }

    #[test]
    fn save_edits_an_existing_type_when_not_new() {
        in_scratch_config(|| {
            let store = scratch_store();
            let entry = worker_type_cli::show("Evaluator").unwrap();
            let mut form = WorkerTypeForm::from_entry(&entry);
            form.model = "gpt-5".into();
            let spec = save(&store, &form).unwrap();
            assert_eq!(spec.name, "Evaluator");
            assert_eq!(spec.model.as_deref(), Some("gpt-5"));
        });
    }

    #[test]
    fn refresh_preserves_state_and_surfaces_the_error_on_a_read_failure() {
        in_scratch_config(|| {
            let mut state = WorkerTypesState::default();
            state.refresh(true);
            assert!(state.error.is_none());
            assert!(!state.entries.is_empty());
            let before = state.entries.len();

            let config_path = ninox_core::config::AppConfig::config_path();
            std::fs::write(&config_path, "this is not valid toml [[[").unwrap();
            state.refresh(true);

            assert!(state.error.is_some());
            assert_eq!(state.entries.len(), before, "a failed reload must not blank the existing list");
        });
    }

    #[test]
    fn export_to_file_writes_under_the_exports_dir() {
        in_scratch_config(|| {
            let path = export_to_file("Researcher").unwrap();
            assert!(path.exists());
            assert!(path.starts_with(ninox_core::config::AppConfig::worker_type_exports_dir()));
            let contents = std::fs::read_to_string(&path).unwrap();
            assert!(contents.contains("name = \"Researcher\""));
        });
    }
}
