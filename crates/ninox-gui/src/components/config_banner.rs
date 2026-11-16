//! Persistent warning row shown when config.toml fails to parse — stays
//! until the file is fixed and the app restarts (no live file-watching).
//! No dismiss control: `App::config_warning` only clears on a fresh, valid
//! load. See MLOPS-5457.

use iced::widget::{button, container, row, text, Space};
use iced::{Alignment, Background, Border, Element, Length};

use crate::app::{App, Message};
use crate::components::shell::tokens;

const PAD: f32 = 8.0;
const TEXT_SIZE: f32 = 12.0;
/// Laid-out height for a one-line message (iced lines are 1.3× the size).
pub(crate) const BANNER_HEIGHT: f32 = 2.0 * PAD + TEXT_SIZE * 1.3;

pub fn config_banner(message: &str) -> Element<'_, Message> {
    container(text(message).size(TEXT_SIZE).color(tokens::BG))
        .width(Length::Fill)
        .padding(PAD)
        .style(|_| container::Style {
            background: Some(Background::Color(tokens::AMBER)),
            ..Default::default()
        })
        .into()
}

/// Whether [`server_banner`] shows: a restart in flight, or a server alert
/// (an expired login, an older build) to act on.
pub(crate) fn server_banner_shown(app: &App) -> bool {
    app.server.restarting || !app.server.alerts.is_empty()
}

/// Same row height as the config banner. Non-blocking: the app stays usable
/// while the server restarts, and sessions keep running throughout.
pub fn server_banner(app: &App) -> Option<Element<'_, Message>> {
    if app.server.restarting {
        return Some(banner_row(
            "Restarting server… running sessions keep running".to_string(),
            Vec::new(),
            tokens::TEAL,
        ));
    }
    let alert = app.server.alerts.first()?;
    let mut actions = vec![("Restart server".to_string(), Some(Message::OpenServerRestart))];
    if !alert.sessions.is_empty() {
        let n = alert.sessions.len();
        let label = if app.server.restarting_sessions {
            "Restarting sessions…".to_string()
        } else {
            format!("Restart affected session{} ({n})", if n == 1 { "" } else { "s" })
        };
        let msg = (!app.server.restarting_sessions).then(|| Message::RestartAffectedSessions(alert.sessions.clone()));
        actions.push((label, msg));
    }
    Some(banner_row(alert.line(), actions, tokens::AMBER))
}

fn banner_row(message: String, actions: Vec<(String, Option<Message>)>, color: iced::Color) -> Element<'static, Message> {
    let mut items: Vec<Element<'static, Message>> =
        vec![text(message).size(TEXT_SIZE).color(tokens::BG).into(), Space::with_width(Length::Fill).into()];
    for (label, msg) in actions {
        items.push(Space::with_width(6).into());
        items.push(
            button(text(label).size(TEXT_SIZE - 1.0).color(tokens::BG))
                .on_press_maybe(msg)
                .padding([0, 8])
                .style(|_t, _s| button::Style {
                    background: None,
                    text_color: tokens::BG,
                    border: Border { color: tokens::BG, width: 1.0, radius: 4.0.into() },
                    ..Default::default()
                })
                .into(),
        );
    }
    container(row(items).align_y(Alignment::Center))
        .width(Length::Fill)
        .height(Length::Fixed(BANNER_HEIGHT))
        .padding([0.0, PAD])
        .align_y(iced::alignment::Vertical::Center)
        .style(move |_| container::Style { background: Some(Background::Color(color)), ..Default::default() })
        .into()
}
