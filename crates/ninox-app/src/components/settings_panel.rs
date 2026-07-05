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
