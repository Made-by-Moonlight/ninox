//! Settings, in the v1 settings-page layout (`WorkerTypes.dc.html`): a
//! header, a section list on the left and the selected section's fields on
//! the right. Field labels, help and validation come from
//! `ninox_ui_shared::settings_form`, the same model the TUI's Settings tab
//! uses, so both apps accept and refuse exactly the same values.

use iced::{
    widget::{button, column, container, pick_list, row, scrollable, text, text_input, Space},
    Alignment, Background, Border, Color, Element, Length,
};
use ninox_core::config::{EditorChoice, SendMechanism, ThemeVariant};
use ninox_ui_shared::settings_form::{self as form, Field, FieldId};

use super::fleet_islands::seg_control;
use super::shell::tokens;
use crate::{
    app::{App, Message},
    style::{self, MONO, SANS, SANS_BOLD},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettingsSection {
    #[default]
    Appearance,
    Agents,
    Harnesses,
    Messaging,
    Runtime,
    SessionLimits,
    Terminal,
    Fleet,
    Engine,
    Version,
}

impl SettingsSection {
    /// The left list, grouped. New sections (session limits, restarting the
    /// server) slot into these groups.
    pub const GROUPS: [(&'static str, &'static [SettingsSection]); 3] = [
        ("General", &[Self::Appearance, Self::Agents, Self::Harnesses, Self::Messaging]),
        ("Sessions", &[Self::Runtime, Self::SessionLimits, Self::Terminal, Self::Fleet]),
        ("System", &[Self::Engine, Self::Version]),
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::Appearance => "Appearance",
            Self::Agents => "Agents",
            Self::Harnesses => "Harnesses",
            Self::Messaging => "Messaging",
            Self::Runtime => "Session runtime",
            Self::SessionLimits => "Session limits",
            Self::Terminal => "Terminal",
            Self::Fleet => "Fleet",
            Self::Engine => "Engine",
            Self::Version => "Version",
        }
    }

    fn blurb(self) -> &'static str {
        match self {
            Self::Appearance => "How the desktop app looks.",
            Self::Agents => "What new orchestrators and workers run, and where workspaces open.",
            Self::Harnesses => "The agent CLIs ninox can launch. Off hides one from every picker.",
            Self::Messaging => "How orchestrators and workers reach each other, and how PRs are watched.",
            Self::Runtime => "Which process hosts new sessions.",
            Self::SessionLimits => "How many agent sessions may run at once on each machine.",
            Self::Terminal => "The terminal UI (nx) and agent panes.",
            Self::Fleet => "Restoring, reaping and keeping sessions.",
            Self::Engine => "The background server every ninox app talks to.",
            Self::Version => "This build, and updates.",
        }
    }
}

/// Settings-view UI state.
#[derive(Debug, Clone, Default)]
pub struct SettingsState {
    /// `Some` while the Workers model picker is in `custom…` mode.
    pub worker_custom: Option<String>,
    pub section: SettingsSection,
    /// Typed-but-uncommitted text per field (Enter or Save commits).
    pub drafts: Vec<(FieldId, String)>,
    /// Why the last change to a field was refused.
    pub errors: Vec<(FieldId, String)>,
}

impl SettingsState {
    pub fn draft(&self, id: &FieldId) -> Option<&str> {
        self.drafts.iter().find(|(f, _)| f == id).map(|(_, t)| t.as_str())
    }

    pub fn set_draft(&mut self, id: FieldId, text: String) {
        self.errors.retain(|(f, _)| *f != id);
        match self.drafts.iter_mut().find(|(f, _)| *f == id) {
            Some((_, t)) => *t = text,
            None => self.drafts.push((id, text)),
        }
    }

    pub fn take_draft(&mut self, id: &FieldId) -> Option<String> {
        let i = self.drafts.iter().position(|(f, _)| f == id)?;
        Some(self.drafts.remove(i).1)
    }

    pub fn error(&self, id: &FieldId) -> Option<&str> {
        self.errors.iter().find(|(f, _)| f == id).map(|(_, e)| e.as_str())
    }

    pub fn set_result(&mut self, id: FieldId, result: Result<(), String>) {
        self.errors.retain(|(f, _)| *f != id);
        if let Err(e) = result {
            self.errors.push((id, e));
        }
    }
}

const LIST_WIDTH: f32 = 240.0;
const FORM_MAX_WIDTH: f32 = 620.0;

pub fn settings_panel(app: &App) -> Element<'_, Message> {
    let header = row![
        column![
            text("Settings").size(13).font(SANS_BOLD).color(tokens::TEXT),
            text("config.toml — every change saves as you make it.")
                .font(SANS)
                .size(11)
                .color(tokens::DIM),
        ]
        .spacing(3),
        Space::new(Length::Fill, 0),
    ]
    .align_y(Alignment::Center)
    .padding([12, 20]);

    let mut list = column![].spacing(2).padding(14);
    for (i, (group, sections)) in SettingsSection::GROUPS.iter().enumerate() {
        if i > 0 {
            list = list.push(Space::new(0, 12));
        }
        list = list.push(style::micro_label(group, tokens::DIM2)).push(Space::new(0, 4));
        for section in *sections {
            list = list.push(section_row(*section, app.settings.section == *section));
        }
    }
    let list_pane = container(scrollable(list))
        .width(Length::Fixed(LIST_WIDTH))
        .height(Length::Fill)
        .style(|_| container::Style {
            background: Some(Background::Color(tokens::PANEL)),
            border: Border { color: tokens::BORDER, width: 1.0, ..Default::default() },
            ..Default::default()
        });

    let section = app.settings.section;
    let body = column![
        text(section.title()).size(16).font(SANS_BOLD).color(tokens::TEXT),
        Space::new(0, 4),
        text(section.blurb()).size(11).font(SANS).color(tokens::DIM2),
        section_body(app, section),
    ]
    .spacing(0)
    .max_width(FORM_MAX_WIDTH);
    let form_pane = scrollable(container(body).padding([20, 28]).width(Length::Fill)).height(Length::Fill);

    column![header, style::hline(tokens::BORDER, 1.0), row![list_pane, form_pane].height(Length::Fill)]
        .height(Length::Fill)
        .into()
}

fn section_row<'a>(section: SettingsSection, selected: bool) -> Element<'a, Message> {
    button(text(section.title()).font(SANS).size(12.5).color(if selected { tokens::TEXT } else { tokens::DIM }))
        .on_press(Message::SettingsSection(section))
        .padding([8, 10])
        .width(Length::Fill)
        .style(move |_t, status| {
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
                border: Border { color: if selected { tokens::TEAL } else { Color::TRANSPARENT }, width: 1.0, radius: 5.0.into() },
                ..Default::default()
            }
        })
        .into()
}

fn section_body(app: &App, section: SettingsSection) -> Element<'_, Message> {
    match section {
        SettingsSection::Appearance => appearance(app),
        SettingsSection::Agents => agents(app),
        SettingsSection::Harnesses => harnesses(app),
        SettingsSection::Messaging => messaging(app),
        SettingsSection::Runtime => runtime(app),
        SettingsSection::SessionLimits => session_limits(app),
        SettingsSection::Terminal => column![text_field(app, FieldId::Prefix)].into(),
        SettingsSection::Fleet => fleet(app),
        SettingsSection::Engine => column![text_field(app, FieldId::Port), server(app)].into(),
        SettingsSection::Version => column![version(app), choice_field(app, FieldId::UpdateChannel)].spacing(16).into(),
    }
}

// ── Field building blocks ─────────────────────────────────────────────────

fn shared_field(app: &App, id: &FieldId) -> Option<Field> {
    form::fields(&app.config).into_iter().find(|f| f.id == *id)
}

fn field_label<'a>(label: &str) -> Element<'a, Message> {
    column![Space::new(0, 18), style::micro_label(label, tokens::DIM2)].spacing(6).into()
}

fn help<'a>(t: impl Into<String>) -> Element<'a, Message> {
    text(t.into()).size(11).font(SANS).color(tokens::DIM2).into()
}

fn error_line<'a>(app: &App, id: &FieldId) -> Option<Element<'a, Message>> {
    app.settings.error(id).map(|e| text(e.to_string()).size(11).font(SANS).color(tokens::RED).into())
}

fn labelled<'a>(label: &str, control: Element<'a, Message>, help_text: Option<String>, error: Option<Element<'a, Message>>) -> Element<'a, Message> {
    let mut col = column![field_label(label), control].spacing(6);
    if let Some(h) = help_text.filter(|h| !h.is_empty()) {
        col = col.push(help(h));
    }
    if let Some(e) = error {
        col = col.push(e);
    }
    col.into()
}

fn input_style(status: text_input::Status) -> text_input::Style {
    let focused = matches!(status, text_input::Status::Focused);
    text_input::Style {
        background: Background::Color(tokens::PANEL2),
        border: Border { color: if focused { tokens::TEAL } else { tokens::BORDER }, width: 1.0, radius: 6.0.into() },
        icon: tokens::DIM,
        placeholder: tokens::DIM,
        value: tokens::TEXT,
        selection: tokens::TEAL,
    }
}

fn pick_style(_t: &iced::Theme, status: pick_list::Status) -> pick_list::Style {
    let active = matches!(status, pick_list::Status::Hovered | pick_list::Status::Opened);
    pick_list::Style {
        text_color: tokens::TEXT,
        placeholder_color: tokens::DIM,
        handle_color: tokens::DIM,
        background: Background::Color(tokens::PANEL2),
        border: Border { color: if active { tokens::TEAL } else { tokens::BORDER }, width: 1.0, radius: 6.0.into() },
    }
}

fn menu_style(_t: &iced::Theme) -> iced::widget::overlay::menu::Style {
    iced::widget::overlay::menu::Style {
        background: Background::Color(tokens::PANEL2),
        border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
        text_color: tokens::TEXT,
        selected_text_color: tokens::BG,
        selected_background: Background::Color(tokens::TEAL),
    }
}

fn picker<'a, T>(options: Vec<T>, selected: Option<T>, on_select: impl Fn(T) -> Message + 'a) -> Element<'a, Message>
where
    T: ToString + PartialEq + Clone + 'a,
{
    pick_list(options, selected, on_select)
        .font(SANS)
        .text_size(12.5)
        .padding([7, 10])
        .width(Length::Fixed(280.0))
        .style(pick_style)
        .menu_style(menu_style)
        .into()
}

fn small_button<'a>(label: &str, color: Color, on_press: Option<Message>) -> Element<'a, Message> {
    let enabled = on_press.is_some();
    button(text(label.to_string()).font(SANS_BOLD).size(11.5).color(if enabled { color } else { tokens::DIM2 }))
        .on_press_maybe(on_press)
        .padding([7, 14])
        .style(move |_t, status| {
            let hovered = enabled && matches!(status, button::Status::Hovered);
            button::Style {
                background: Some(Background::Color(if hovered { tokens::PANEL2 } else { Color::TRANSPARENT })),
                text_color: color,
                border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
                ..Default::default()
            }
        })
        .into()
}

/// A shared Choice field as a segmented control (few options).
fn choice_field(app: &App, id: FieldId) -> Element<'_, Message> {
    let Some(f) = shared_field(app, &id) else { return Space::new(0, 0).into() };
    let options = f
        .options
        .iter()
        .map(|o| (o.clone(), *o == f.value, Message::SettingsSet { id: id.clone(), text: o.clone() }))
        .collect();
    labelled(&f.label, seg_control(options), Some(f.help.clone()), error_line(app, &id))
}

/// A shared on/off field.
fn toggle_field(app: &App, id: FieldId) -> Element<'_, Message> {
    let Some(f) = shared_field(app, &id) else { return Space::new(0, 0).into() };
    let on = f.value == "on";
    let options = vec![
        ("On".to_string(), on, Message::SettingsSet { id: id.clone(), text: "on".into() }),
        ("Off".to_string(), !on, Message::SettingsSet { id: id.clone(), text: "off".into() }),
    ];
    labelled(&f.label, seg_control(options), Some(f.help.clone()), error_line(app, &id))
}

/// A shared free-text field: typed into a draft, committed with Enter or
/// Save. Suggestions, if the field has any, apply in one click.
fn text_field(app: &App, id: FieldId) -> Element<'_, Message> {
    let Some(f) = shared_field(app, &id) else { return Space::new(0, 0).into() };
    let draft = app.settings.draft(&id).map(str::to_string);
    let dirty = draft.as_deref().is_some_and(|d| d != f.value);
    let value = draft.unwrap_or_else(|| f.value.clone());
    let on_input = {
        let id = id.clone();
        move |t| Message::SettingsDraft { id: id.clone(), text: t }
    };
    let input = text_input("", &value)
        .on_input(on_input)
        .on_submit(Message::SettingsCommit(id.clone()))
        .font(MONO)
        .size(12.5)
        .padding([7, 10])
        .width(Length::Fixed(280.0))
        .style(|_t, status| input_style(status));
    let mut control = row![input].spacing(8).align_y(Alignment::Center);
    if dirty {
        control = control
            .push(small_button("Save", tokens::TEAL, Some(Message::SettingsCommit(id.clone()))))
            .push(small_button("Revert", tokens::DIM, Some(Message::SettingsRevert(id.clone()))));
    }
    let mut col = column![control.wrap()].spacing(8);
    let suggestions: Vec<String> = f.options.iter().filter(|o| !o.is_empty()).cloned().collect();
    if !suggestions.is_empty() {
        col = col.push(seg_control(
            suggestions
                .into_iter()
                .map(|o| (o.clone(), o == f.value, Message::SettingsSet { id: id.clone(), text: o }))
                .collect(),
        ));
    }
    labelled(&f.label, col.into(), Some(f.help.clone()), error_line(app, &id))
}

// ── Sections ──────────────────────────────────────────────────────────────

fn appearance(app: &App) -> Element<'_, Message> {
    let options = [ThemeVariant::Light, ThemeVariant::Dark, ThemeVariant::Ninox]
        .into_iter()
        .map(|v| {
            let label = match v {
                ThemeVariant::Light => "Light",
                ThemeVariant::Dark => "Dark",
                ThemeVariant::Ninox => "Ninox",
            };
            (label.to_string(), app.active_variant == v, Message::SwitchTheme(v))
        })
        .collect();
    let theme_file = app.config.theme_file.clone().unwrap_or_else(|| "themes/field-notes.toml".to_string());
    labelled(
        "Theme",
        seg_control(options),
        Some(format!("Colours the desktop's terminal panes and the terminal UI; desktop chrome uses the v1 palette. Theme file: {theme_file}")),
        None,
    )
}

fn model_picker<'a>(
    app: &'a App,
    harness: &str,
    configured: Option<String>,
    custom: bool,
    placeholder: String,
    on_select: impl Fn(String) -> Message + 'a,
) -> Element<'a, Message> {
    let registry = app.config.registry();
    let spec = registry.spec(harness);
    let discovered = app.model_lists.get(harness).and_then(|m| m.as_deref());
    let options = ninox_ui_shared::models::model_options(&spec, discovered, configured.as_deref());
    let selected = if custom { Some(ninox_ui_shared::models::CUSTOM_SENTINEL.to_string()) } else { configured };
    pick_list(options, selected, on_select)
        .placeholder(placeholder)
        .font(MONO)
        .text_size(12.5)
        .padding([7, 10])
        .width(Length::Fixed(280.0))
        .style(pick_style)
        .menu_style(menu_style)
        .into()
}

fn custom_model_input<'a>(value: &str, on_input: impl Fn(String) -> Message + 'a, on_submit: Message) -> Element<'a, Message> {
    text_input("model id", value)
        .on_input(on_input)
        .on_submit(on_submit)
        .font(MONO)
        .size(12.5)
        .padding([7, 10])
        .width(Length::Fixed(280.0))
        .style(|_t, status| input_style(status))
        .into()
}

fn agents(app: &App) -> Element<'_, Message> {
    let registry = app.config.registry();

    let orch_harness = picker(registry.enabled_names(), Some(app.config.orchestrator.harness.clone()), |h| Message::SettingsSet {
        id: FieldId::OrchHarness,
        text: h,
    });
    let orch_help = shared_field(app, &FieldId::OrchHarness).map(|f| f.help);

    let orch_custom = app.settings.draft(&FieldId::OrchModel).map(str::to_string);
    let mut orch_model = column![model_picker(
        app,
        &app.config.orchestrator.harness,
        app.config.orchestrator.model.clone(),
        orch_custom.is_some(),
        ninox_core::model_resolution::unset_setting_placeholder(ninox_core::model_resolution::Role::Orchestrator, &app.config),
        |m| {
            if m == ninox_ui_shared::models::CUSTOM_SENTINEL {
                Message::SettingsDraft { id: FieldId::OrchModel, text: String::new() }
            } else {
                Message::SettingsSet { id: FieldId::OrchModel, text: m }
            }
        },
    )]
    .spacing(8);
    if let Some(v) = &orch_custom {
        orch_model = orch_model.push(
            row![
                custom_model_input(
                    v,
                    |t| Message::SettingsDraft { id: FieldId::OrchModel, text: t },
                    Message::SettingsCommit(FieldId::OrchModel),
                ),
                small_button("Save", tokens::TEAL, Some(Message::SettingsCommit(FieldId::OrchModel))),
                small_button("Cancel", tokens::DIM, Some(Message::SettingsRevert(FieldId::OrchModel))),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        );
    }

    let worker_opts: Vec<String> =
        registry.enabled_names().into_iter().filter(|n| registry.spec(n).worker_args.is_some()).collect();
    let worker_sel = worker_opts.iter().find(|n| **n == app.config.worker.harness).cloned();
    let worker_harness = picker(worker_opts, worker_sel, Message::SettingsWorkerHarness);
    let worker_help = shared_field(app, &FieldId::WorkerHarness).map(|f| f.help);

    let mut worker_model = column![model_picker(
        app,
        &app.config.worker.harness,
        app.config.worker.model.clone(),
        app.settings.worker_custom.is_some(),
        ninox_core::model_resolution::unset_setting_placeholder(ninox_core::model_resolution::Role::Worker, &app.config),
        Message::SettingsWorkerModel,
    )]
    .spacing(8);
    if let Some(v) = &app.settings.worker_custom {
        worker_model = worker_model.push(custom_model_input(v, Message::SettingsWorkerCustomModel, Message::SettingsWorkerCustomCommit));
    }

    let editor = picker(EditorChoice::ALL.to_vec(), Some(app.config.editor), Message::SettingsEditor);
    let editor_help = shared_field(app, &FieldId::Editor).map(|f| f.help);

    let heading = column![
        Space::new(0, 18),
        text("Default models").size(13).font(SANS_BOLD).color(tokens::TEXT),
        Space::new(0, 4),
        text(ninox_ui_shared::settings_form::MODEL_OVERRIDES).size(11).font(SANS).color(tokens::DIM2),
    ];
    column![
        heading,
        labelled("Orchestrator harness", orch_harness, orch_help, error_line(app, &FieldId::OrchHarness)),
        labelled(
            "Orchestrator model",
            orch_model.into(),
            Some("Default for new orchestrators. Unset shows the built-in default for the harness.".into()),
            error_line(app, &FieldId::OrchModel),
        ),
        labelled("Worker harness", worker_harness, worker_help, None),
        labelled(
            "Worker model",
            worker_model.into(),
            Some("Default for workers, standalone sessions and the Ninox prompt. Unset shows the built-in default for the harness.".into()),
            None,
        ),
        labelled("Open workspaces in", editor, editor_help, None),
    ]
    .into()
}

fn harnesses(app: &App) -> Element<'_, Message> {
    let registry = app.config.registry();
    let mut rows = column![Space::new(0, 18)].spacing(8);
    for name in registry.names() {
        let spec = registry.spec(&name);
        let locked = name == ninox_core::config::AppConfig::LOCKED_HARNESS;
        let enabled = spec.enabled;
        let binary = spec.binary.clone().unwrap_or_else(|| name.clone());
        let workers = if spec.worker_args.is_some() { "can run workers" } else { "orchestrators only" };
        let toggle = if locked {
            text("always on").size(11).font(SANS).color(tokens::DIM2).into()
        } else {
            seg_control(vec![
                ("On".to_string(), enabled, if enabled { Message::Noop } else { Message::SettingsToggleHarness(name.clone()) }),
                ("Off".to_string(), !enabled, if enabled { Message::SettingsToggleHarness(name.clone()) } else { Message::Noop }),
            ])
        };
        rows = rows.push(
            container(
                row![
                    column![
                        text(name.clone()).size(13).font(SANS_BOLD).color(if enabled { tokens::TEXT } else { tokens::DIM }),
                        text(format!("runs {binary} · {workers}")).size(11).font(SANS).color(tokens::DIM2),
                    ]
                    .spacing(3),
                    Space::new(Length::Fill, 0),
                    toggle,
                ]
                .align_y(Alignment::Center),
            )
            .padding([10, 12])
            .width(Length::Fill)
            .style(|_| container::Style {
                background: Some(Background::Color(tokens::PANEL)),
                border: Border { color: tokens::BORDER, width: 1.0, radius: 7.0.into() },
                ..Default::default()
            }),
        );
    }
    rows.into()
}

fn messaging(app: &App) -> Element<'_, Message> {
    let mechanism = app.config.send_mechanism();
    let pr_watch = app.config.pr_watch.enabled;
    let pr_help = shared_field(app, &FieldId::PrWatch).map(|f| f.help);
    column![
        labelled(
            "Send mechanism",
            picker(SendMechanism::ALL.to_vec(), Some(mechanism), Message::SettingsSetSendMechanism),
            Some(mechanism.description().to_string()),
            None,
        ),
        labelled(
            "Consolidated PR watching",
            seg_control(vec![
                ("On".to_string(), pr_watch, if pr_watch { Message::Noop } else { Message::SettingsTogglePrWatch }),
                ("Off".to_string(), !pr_watch, if pr_watch { Message::SettingsTogglePrWatch } else { Message::Noop }),
            ]),
            pr_help,
            None,
        ),
    ]
    .into()
}

fn runtime(app: &App) -> Element<'_, Message> {
    let backend = app.config.runtime.backend;
    labelled(
        "Runtime for new sessions",
        picker(ninox_core::runtime::Backend::ALL.to_vec(), Some(backend), Message::SettingsSetRuntimeBackend),
        Some(backend.description().to_string()),
        None,
    )
}

/// One row per location (this machine, then each remote) with an editable
/// cap and an Unlimited toggle. The poller reads it on its next tick.
fn session_limits(app: &App) -> Element<'_, Message> {
    let mut locations: Vec<(Option<String>, String)> = vec![(None, "This machine".to_string())];
    locations.extend(app.config.remote_machines.machines.iter().map(|m| (Some(m.id.clone()), m.label.clone())));

    let mut rows = column![Space::new(0, 18)].spacing(8);
    for (machine, label) in locations {
        let limit = app.config.limits.limit_for(machine.as_deref());
        let unlimited = limit.is_none();
        let id = FieldId::SessionLimit(machine.clone());
        let draft = app.settings.draft(&id).map(str::to_string);
        let dirty = draft.as_deref().is_some_and(|d| Some(d.to_string()) != limit.map(|n| n.to_string()));
        let value = draft.unwrap_or_else(|| limit.map(|n| n.to_string()).unwrap_or_default());
        let input: Element<Message> = if unlimited {
            container(text("∞").size(13).font(crate::style::glyph_font_for("∞")).color(tokens::DIM))
                .width(Length::Fixed(72.0))
                .padding([7, 10])
                .into()
        } else {
            let (m_in, m_commit) = (machine.clone(), machine.clone());
            text_input("6", &value)
                .on_input(move |v| Message::SettingsLimitInput(m_in.clone(), v))
                .on_submit(Message::SettingsLimitCommit(m_commit))
                .font(MONO)
                .size(12.5)
                .padding([7, 10])
                .width(Length::Fixed(72.0))
                .style(|_t, status| input_style(status))
                .into()
        };
        let toggle_msg = Message::SettingsLimitToggleUnlimited(machine.clone());
        let unlimited_seg = seg_control(vec![
            ("Limit".to_string(), !unlimited, if unlimited { toggle_msg.clone() } else { Message::Noop }),
            ("Unlimited".to_string(), unlimited, if unlimited { Message::Noop } else { toggle_msg }),
        ]);
        let mut line = row![
            text(label).size(13).font(SANS).color(tokens::TEXT).width(Length::Fill),
            input,
        ]
        .spacing(10)
        .align_y(Alignment::Center);
        if dirty {
            line = line.push(small_button("Save", tokens::TEAL, Some(Message::SettingsLimitCommit(machine.clone()))));
        }
        let mut cell = column![line.push(unlimited_seg)].spacing(6);
        if let Some(e) = error_line(app, &id) {
            cell = cell.push(e);
        }
        rows = rows.push(
            container(cell)
                .padding([10, 12])
                .width(Length::Fill)
                .style(|_| container::Style {
                    background: Some(Background::Color(tokens::PANEL)),
                    border: Border { color: tokens::BORDER, width: 1.0, radius: 7.0.into() },
                    ..Default::default()
                }),
        );
    }
    rows.push(help(
        "A whole number, or unlimited (0 also means unlimited). Spawns over the limit are queued and start \
         in order as sessions finish. Changes apply within a few seconds; lowering a limit never stops \
         running sessions.",
    ))
    .into()
}

fn fleet(app: &App) -> Element<'_, Message> {
    let live = crate::app::live_session_count(&app.sessions);
    let status = if live == 0 {
        "No live agents.".to_string()
    } else {
        format!("{live} live agent{}.", if live == 1 { "" } else { "s" })
    };
    let actions = labelled(
        "Actions",
        row![
            small_button(
                if app.restart_all_in_progress { "Restarting…" } else { "Restart all agents" },
                tokens::TEXT,
                (live > 0 && !app.restart_all_in_progress).then_some(Message::RequestRestartAll),
            ),
            small_button("Restore fleet…", tokens::TEXT, Some(Message::RestoreFleetOpen)),
        ]
        .spacing(8)
        .into(),
        Some(format!("{status} Restarting picks up a newer ninox, harness or MCP config.")),
        None,
    );
    column![
        choice_field(app, FieldId::RestorePolicy),
        choice_field(app, FieldId::WorkerCompletion),
        text_field(app, FieldId::RetentionDays),
        text_field(app, FieldId::GithubPollInterval),
        text_field(app, FieldId::GithubDetailRefresh),
        github_budget_line(app),
        toggle_field(app, FieldId::BrainHarvest),
        actions,
    ]
    .into()
}

fn github_budget_line(app: &App) -> Element<'_, Message> {
    let (line, color) = match &app.github_budget {
        Some(b) => (
            b.summary_now(),
            if b.level == ninox_core::github_budget::BudgetLevel::Normal { tokens::TEXT } else { tokens::AMBER },
        ),
        None => ("GitHub budget: not read yet".to_string(), tokens::TEXT),
    };
    text(line).size(12).font(SANS).color(color).into()
}

/// Which build answers on the port, and restarting it — to pick up a new
/// build or a fresh login (aws sso, gh, claude) without touching sessions.
fn server(app: &App) -> Element<'_, Message> {
    let server = &app.server;
    let status = match &server.info {
        Some(info) if info.pid == std::process::id() => format!("{} · hosted by this app", info.label()),
        Some(info) => format!("{} · pid {}", info.label(), info.pid),
        None => "Not answering".to_string(),
    };
    let status_color = if server.info.is_some() { tokens::TEXT } else { tokens::AMBER };

    let action: Element<Message> = if server.restarting {
        text("Restarting server…").size(12).font(SANS).color(tokens::TEAL).into()
    } else if server.confirming {
        row![
            text("Restart the server? Running sessions keep running.").size(12).font(SANS).color(tokens::TEXT),
            small_button("Restart", tokens::TEAL, Some(Message::ConfirmRestartServer)),
            small_button("Cancel", tokens::DIM, Some(Message::CancelRestartServer)),
        ]
        .spacing(8)
        .align_y(Alignment::Center)
        .wrap()
        .into()
    } else {
        small_button("Restart server", tokens::TEXT, Some(Message::RequestRestartServer))
    };

    let mut col = column![
        text(status).size(12.5).font(MONO).color(status_color),
        action,
    ]
    .spacing(8);
    for alert in &server.alerts {
        let mut block = column![text(alert.line()).size(12).font(SANS).color(tokens::AMBER)].spacing(6);
        if !alert.sessions.is_empty() {
            let label = if server.restarting_sessions { "Restarting sessions…" } else { "Restart affected sessions" };
            let msg = (!server.restarting_sessions).then(|| Message::RestartAffectedSessions(alert.sessions.clone()));
            block = block.push(
                row![
                    text(alert.sessions.join(", ")).size(11.5).font(MONO).color(tokens::DIM),
                    small_button(label, tokens::TEXT, msg),
                ]
                .spacing(10)
                .align_y(Alignment::Center)
                .wrap(),
            );
        }
        col = col.push(
            container(block).padding([10, 12]).width(Length::Fill).style(|_| container::Style {
                background: Some(Background::Color(tokens::AMBER_BADGE_BG)),
                border: Border { color: Color { a: 0.5, ..tokens::AMBER }, width: 1.0, radius: 6.0.into() },
                ..Default::default()
            }),
        );
    }
    match &server.result {
        Some(Ok(summary)) => col = col.push(text(summary.clone()).size(12).font(SANS).color(tokens::GREEN)),
        Some(Err(e)) => col = col.push(text(e.clone()).size(12).font(SANS).color(tokens::RED)),
        None => {}
    }
    let help_text = shared_field(app, &FieldId::RestartServer).map(|f| f.help);
    labelled("Server", col.into(), help_text, None)
}

fn version(app: &App) -> Element<'_, Message> {
    use crate::app::VersionCheckState;
    let (line, color, action): (String, Color, Option<Element<Message>>) = match &app.version_check {
        VersionCheckState::NotChecked | VersionCheckState::Checking => ("Checking for updates…".into(), tokens::DIM, None),
        VersionCheckState::UpToDate => ("Up to date.".into(), tokens::GREEN, None),
        VersionCheckState::Failed => {
            ("The update check failed.".into(), tokens::AMBER, Some(small_button("Retry", tokens::TEXT, Some(Message::NavigateSettings))))
        }
        VersionCheckState::Installed => {
            ("Update installed.".into(), tokens::GREEN, Some(small_button("Restart now", tokens::TEAL, Some(Message::RestartApp))))
        }
        VersionCheckState::UpdateAvailable(latest) => (
            format!("Update available: {latest}."),
            tokens::TEAL,
            Some(small_button(
                if app.update_in_progress { "Updating…" } else { "Update now" },
                tokens::TEAL,
                (!app.update_in_progress).then_some(Message::ApplyUpdate),
            )),
        ),
    };
    let status = row![text(line).size(12.5).font(SANS).color(color)].push_maybe(action).spacing(12).align_y(Alignment::Center);
    labelled(&format!("ninox {}", crate::app::NINOX_VERSION), status.into(), None, None)
}
