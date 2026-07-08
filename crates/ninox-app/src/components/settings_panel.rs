//! Settings — "The appendix" (spec §V): a single narrow column of cards
//! reached from the sidebar footer. Theme dots live here (relocated from
//! the footer); harness registry toggles and the worker default follow in
//! their own cards.

use ninox_core::config::ThemeVariant;
use iced::{
    widget::{button, column, container, row, scrollable, text, Space},
    Alignment, Background, Border, Element, Length,
};

use crate::{
    app::{App, Message},
    style::{card_style, hline, micro_label, MONO, SERIF, SERIF_ITALIC},
};

/// Settings-view UI state (custom-model input text for the Workers card).
#[derive(Debug, Clone, Default)]
pub struct SettingsState {
    /// `Some` while the Workers model picker is in `custom…` mode.
    pub worker_custom: Option<String>,
}

/// Column width — "a single narrow column (~720px) of cards".
const COLUMN_W: f32 = 720.0;

pub fn settings_panel(app: &App) -> Element<'_, Message> {
    let s = &app.scheme;

    let folio = crate::components::folio::folio_scaffold(
        app,
        move || {
            row![
                text("The ").size(34).font(SERIF).color(s.ink),
                text("appendix").size(34).font(SERIF_ITALIC).color(s.ink),
            ]
            .align_y(Alignment::End)
            .into()
        },
        move || vec![micro_label("Settings", s.faint).size(10.0).into()],
    );

    let cards = column![
        theme_card(app),
        harnesses_card(app),
        workers_card(app),
        version_card(app),
    ]
    .spacing(18)
    .width(Length::Fixed(COLUMN_W));

    column![
        folio,
        hline(s.ink, 2.0),
        scrollable(
            container(cards)
                .width(Length::Fill)
                .center_x(Length::Fill)
                .padding([24, 28]),
        )
        .height(Length::Fill),
    ]
    .width(Length::Fill)
    .into()
}

/// Shared card scaffold: micro-label heading over a rule, then the body.
fn card<'a>(app: &'a App, label: &'a str, body: Element<'a, Message>) -> Element<'a, Message> {
    let s = &app.scheme;
    container(
        column![
            micro_label(label, s.ink_2),
            Space::new(0, 10),
            hline(s.rule_dark, 1.0),
            Space::new(0, 14),
            body,
        ],
    )
    .padding([18, 22])
    .width(Length::Fill)
    .style(move |_theme| card_style(s))
    .into()
}

/// Theme card: the light/dark/ninox dots (relocated from the sidebar
/// footer) + a mono pointer to the active theme file.
fn theme_card(app: &App) -> Element<'_, Message> {
    let s = &app.scheme;
    let mut dots = row![].spacing(8).align_y(Alignment::Center);
    for variant in [ThemeVariant::Light, ThemeVariant::Dark, ThemeVariant::Ninox] {
        let selected = app.active_variant == variant;
        let fill = match variant {
            ThemeVariant::Light => crate::theme::light().paper,
            ThemeVariant::Dark | ThemeVariant::Ninox => crate::theme::dark().paper,
        };
        let label = match variant {
            ThemeVariant::Light => "light",
            ThemeVariant::Dark  => "dark",
            ThemeVariant::Ninox => "ninox",
        };
        dots = dots.push(
            button(
                row![
                    container(Space::new(0, 0)).width(14).height(Length::Fixed(14.0)).style(
                        move |_| container::Style {
                            background: Some(Background::Color(fill)),
                            border: Border {
                                color:  if selected { s.accent } else { s.ink },
                                width:  if selected { 2.0 } else { 1.5 },
                                radius: 7.0.into(),
                            },
                            ..Default::default()
                        },
                    ),
                    Space::new(6, 0),
                    text(label).size(11).font(crate::style::SANS)
                        .color(if selected { s.ink } else { s.ink_2 }),
                ]
                .align_y(Alignment::Center),
            )
            .on_press(Message::SwitchTheme(variant))
            .style(|_t, _st| button::Style { background: None, border: Border::default(), ..Default::default() })
            .padding([2, 4]),
        );
    }

    let theme_file = app.config.theme_file.clone()
        .unwrap_or_else(|| "themes/field-notes.toml".to_string());

    card(app, "Theme", column![
        dots,
        Space::new(0, 12),
        text(theme_file).size(10).font(MONO).color(s.faint),
    ]
    .spacing(0)
    .into())
}

/// Harnesses card: one row per registry harness — ink-fill toggle, serif
/// name, mono binary, `workers ✓/–` marker. NO model field here by design:
/// interactive spawns always choose their model in the Spawn modal.
fn harnesses_card(app: &App) -> Element<'_, Message> {
    let s = &app.scheme;
    let registry = app.config.registry();
    let mut rows = column![].spacing(10);
    for name in registry.names() {
        let spec = registry.spec(&name);
        let locked = name == "claude-code";
        let enabled = spec.enabled;
        let binary = spec.binary.clone().unwrap_or_else(|| name.clone());
        let worker_capable = spec.worker_args.is_some();
        let workers = if worker_capable { "workers ✓" } else { "workers –" };

        let toggle = button(Space::new(0, 0))
            .on_press_maybe((!locked).then(|| Message::SettingsToggleHarness(name.clone())))
            .width(Length::Fixed(30.0))
            .height(Length::Fixed(16.0))
            .padding(0)
            .style(move |_t, status| button::Style {
                background: enabled.then_some(Background::Color(s.ink)),
                text_color: s.ink,
                border: Border {
                    color: if matches!(status, button::Status::Hovered) && !locked { s.accent } else { s.ink },
                    width: 1.5,
                    radius: 8.0.into(),
                },
                ..Default::default()
            });

        let name_label = text(name.clone()).size(14).font(SERIF)
            .color(if enabled { s.ink } else { s.ink_2 });
        let suffix: Element<Message> = if locked {
            text("default").size(9).font(MONO).color(s.faint).into()
        } else {
            Space::new(0, 0).into()
        };

        rows = rows.push(
            row![
                toggle,
                Space::new(12, 0),
                name_label,
                Space::new(8, 0),
                suffix,
                Space::new(Length::Fill, 0),
                text(binary).size(10).font(MONO).color(s.faint),
                Space::new(14, 0),
                text(workers).size(10).font(MONO)
                    .color(if worker_capable { s.ink_2 } else { s.faint }),
            ]
            .align_y(Alignment::Center),
        );
    }
    card(app, "Harnesses", rows.into())
}

/// Workers card — the one unmanned decision: what `ninox spawn` launches
/// when orchestrator agents spawn workers. Harness picker (enabled,
/// worker-capable harnesses only) + model picker (select with a `custom…`
/// escape hatch). Maps to config `[worker]`.
fn workers_card(app: &App) -> Element<'_, Message> {
    use iced::widget::{pick_list, text_input};
    let s = &app.scheme;
    let registry = app.config.registry();

    let harness_opts: Vec<String> = registry.enabled_names().into_iter()
        .filter(|n| registry.spec(n).worker_args.is_some())
        .collect();
    let harness_sel = harness_opts.iter()
        .find(|n| **n == app.config.worker.harness)
        .cloned();
    let harness_pick = pick_list(harness_opts, harness_sel, Message::SettingsWorkerHarness)
        .font(MONO)
        .text_size(12)
        .padding([6, 10])
        .style(crate::style::pick_style(s));

    let spec = registry.spec(&app.config.worker.harness);
    let discovered = app.model_lists.get(&app.config.worker.harness).and_then(|m| m.as_deref());
    let configured = app.config.worker.model.clone().or_else(|| spec.model.clone());
    let model_opts = crate::models::model_options(&spec, discovered, configured.as_deref());
    let model_sel = if app.settings.worker_custom.is_some() {
        Some(crate::models::CUSTOM_SENTINEL.to_string())
    } else {
        configured
    };
    let model_pick = pick_list(model_opts, model_sel, Message::SettingsWorkerModel)
        .placeholder("harness default")
        .font(MONO)
        .text_size(12)
        .padding([6, 10])
        .style(crate::style::pick_style(s));

    let mut body = column![
        row![
            column![micro_label("Harness", s.faint), Space::new(0, 6), harness_pick].spacing(0),
            Space::new(20, 0),
            column![micro_label("Model", s.faint), Space::new(0, 6), model_pick].spacing(0),
        ]
        .align_y(Alignment::Start),
    ]
    .spacing(0);
    if let Some(v) = &app.settings.worker_custom {
        body = body.push(Space::new(0, 10)).push(
            text_input("model id", v)
                .on_input(Message::SettingsWorkerCustomModel)
                .on_submit(Message::SettingsWorkerCustomCommit)
                .font(MONO)
                .size(12)
                .padding([4, 2])
                .style(crate::style::underlined_input_style(s)),
        );
    }
    card(app, "Workers", body.into())
}

/// A small filled pill button for the version card's update action —
/// mirrors `notification_panel`'s `action_button` but kept local to this
/// file, matching how each component here owns its own small button styles
/// (see `harnesses_card`'s toggle, `workers_card`'s picker styling, etc.)
/// rather than sharing one across files.
fn pill_button<'a>(label: &'a str, message: Option<Message>, s: &crate::theme::ColorScheme) -> Element<'a, Message> {
    let text_color = s.card;
    let fill = s.status_done;
    button(text(label).size(11).font(crate::style::SANS_BOLD).color(text_color))
        .on_press_maybe(message)
        .padding([4, 10])
        .style(move |_theme, _status| button::Style {
            background: Some(Background::Color(fill)),
            border: Border { radius: 3.0.into(), ..Default::default() },
            ..Default::default()
        })
        .into()
}

/// Version card: the running build's own version (also the quickest way to
/// tell what a user reporting an issue is actually on) plus a fresh
/// on-demand registry check (`ensure_version_check`, fired on every
/// `NavigateSettings`) — same `lifecycle::update_check` source the
/// background poller uses, so "up to date" here means the same thing it
/// means in the notification panel.
fn version_card(app: &App) -> Element<'_, Message> {
    use crate::app::VersionCheckState;
    let s = &app.scheme;

    let version_line = text(format!("ninox {}", crate::app::NINOX_VERSION))
        .size(14)
        .font(SERIF)
        .color(s.ink);

    let status: Element<Message> = match &app.version_check {
        VersionCheckState::NotChecked | VersionCheckState::Checking => {
            text("Checking for updates…").size(11).font(MONO).color(s.faint).into()
        }
        VersionCheckState::UpToDate => {
            text("Up to date").size(11).font(MONO).color(s.status_done).into()
        }
        VersionCheckState::Failed => row![
            text("Update check failed").size(11).font(MONO).color(s.faint),
            Space::new(10, 0),
            pill_button("Retry", Some(Message::NavigateSettings), s),
        ]
        .align_y(Alignment::Center)
        .into(),
        VersionCheckState::Installed => row![
            text("Update installed").size(11).font(MONO).color(s.status_done),
            Space::new(10, 0),
            pill_button("Restart now", Some(Message::RestartApp), s),
        ]
        .align_y(Alignment::Center)
        .into(),
        VersionCheckState::UpdateAvailable(latest) => row![
            text(format!("Update available — {latest}")).size(11).font(MONO).color(s.status_done),
            Space::new(10, 0),
            pill_button(
                if app.update_in_progress { "Updating…" } else { "Update now" },
                (!app.update_in_progress).then_some(Message::ApplyUpdate),
                s,
            ),
        ]
        .align_y(Alignment::Center)
        .into(),
    };

    card(app, "Version", column![version_line, Space::new(0, 8), status].spacing(0).into())
}
