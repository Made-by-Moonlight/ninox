//! Orchestrators' registered plan docs on the Plans tab: a card per
//! orchestrator, and the selected one rendered by the same `plan_pane` the
//! orchestrator's own Plan panel uses.

use iced::widget::{button, column, container, mouse_area, row, text, Space};
use iced::{Alignment, Background, Border, Element, Length};

use ninox_ui_shared::orchestrator_plans::{self, FileHealth, PlanEntry};

use super::fleet_focus::badge;
use super::shell::tokens;
use crate::app::{App, Message};

#[derive(Debug, Default)]
pub struct OrchestratorPlansUi {
    pub entries: Vec<PlanEntry>,
    pub selected: Option<String>,
    pub show_ended: bool,
}

impl OrchestratorPlansUi {
    pub fn reload(&mut self, store: &ninox_core::store::Store) {
        self.entries = orchestrator_plans::load(store);
        if let Some(id) = &self.selected {
            if !orchestrator_plans::visible(&self.entries, self.show_ended).iter().any(|e| &e.orchestrator_id == id) {
                self.selected = None;
            }
        }
    }

    pub fn selected_entry(&self) -> Option<&PlanEntry> {
        let id = self.selected.as_ref()?;
        self.entries.iter().find(|e| &e.orchestrator_id == id)
    }
}

fn small(s: impl Into<String>, color: iced::Color) -> Element<'static, Message> {
    text(s.into()).size(10.5).font(crate::style::SANS).color(color).into()
}

fn status_badge(entry: &PlanEntry) -> Element<'static, Message> {
    match (entry.file, entry.live) {
        (FileHealth::Missing, _) => badge("FILE MISSING".into(), tokens::AMBER_BADGE_BG, tokens::AMBER_BADGE_FG),
        (FileHealth::Unreadable, _) => badge("UNREADABLE".into(), tokens::AMBER_BADGE_BG, tokens::AMBER_BADGE_FG),
        (FileHealth::Ok, true) => badge("LIVE".into(), tokens::BLUE_BADGE_BG, tokens::BLUE),
        (FileHealth::Ok, false) => badge("ENDED".into(), tokens::PANEL2, tokens::DIM),
    }
}

const CARD_ON_BG: iced::Color = tokens::hex(0x161b29);

/// `~/…` for paths under the home directory, and the tail of an overlong
/// path behind a `…`, so a card's path stays on one line in the 420px list.
fn display_path(path: &str) -> String {
    shorten_path(path, std::env::var_os("HOME").as_deref())
}

fn shorten_path(path: &str, home: Option<&std::ffi::OsStr>) -> String {
    const MAX_CHARS: usize = 52;
    let shown = match home.and_then(|h| std::path::Path::new(path).strip_prefix(h).ok().map(|r| r.to_path_buf())) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.to_string(),
    };
    let n = shown.chars().count();
    if n <= MAX_CHARS {
        return shown;
    }
    let tail: String = shown.chars().skip(n - (MAX_CHARS - 1)).collect();
    format!("…{tail}")
}

/// Live (non-terminal) sessions this orchestrator owns.
fn worker_count(app: &App, entry: &PlanEntry) -> usize {
    app.sessions
        .values()
        .filter(|s| s.orchestrator_id.as_deref() == Some(entry.orchestrator_id.as_str()) && !s.status.is_terminal())
        .count()
}

fn card_meta(app: &App, entry: &PlanEntry) -> String {
    if entry.file == FileHealth::Missing {
        let when = super::plan_panel::updated_label(Some(entry.updated_at));
        return when.replacen("updated", "registered", 1);
    }
    let mut line = super::plan_panel::updated_label(Some(entry.updated_at));
    if entry.live {
        let n = worker_count(app, entry);
        line.push_str(&format!(" · {n} worker{}", if n == 1 { "" } else { "s" }));
    }
    line
}

fn card<'a>(app: &'a App, entry: &'a PlanEntry) -> Element<'a, Message> {
    let selected = app.orch_plans.selected.as_deref() == Some(entry.orchestrator_id.as_str());
    let body = column![
        row![
            status_badge(entry),
            text(entry.name.clone()).size(14).font(crate::style::SANS_SEMIBOLD).color(tokens::TEXT),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
        text(display_path(&entry.file_path)).size(10.5).font(crate::style::MONO).color(tokens::DIM2).wrapping(iced::widget::text::Wrapping::None),
        small(card_meta(app, entry), tokens::DIM),
    ]
    .spacing(5);
    let pad = if selected { 10 } else { 11 };
    let card = container(body)
        .padding(iced::Padding { top: pad as f32, bottom: pad as f32, left: pad as f32 + 1.0, right: pad as f32 + 1.0 })
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(Background::Color(if selected { CARD_ON_BG } else { tokens::PANEL })),
            border: Border { color: if selected { tokens::VIOLET } else { tokens::BORDER }, width: if selected { 2.0 } else { 1.0 }, radius: 8.0.into() },
            ..Default::default()
        });
    mouse_area(card).on_press(Message::PlansSelectOrchestratorPlan(entry.orchestrator_id.clone())).into()
}

fn section_title<'a>(title: &'static str, count: String) -> Element<'a, Message> {
    row![
        crate::style::micro_label(title, tokens::DIM2),
        text(count).size(11).font(crate::style::SANS).color(tokens::DIM),
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .into()
}

/// The "Orchestrator plans" section: title with live count, ended toggle and
/// cards.
pub fn section<'a>(app: &'a App) -> Vec<Element<'a, Message>> {
    let ui = &app.orch_plans;
    let ended = ui.entries.iter().filter(|e| !e.live).count();
    let live = ui.entries.len() - ended;
    let mut items: Vec<Element<Message>> = vec![row![
        section_title("Orchestrator plans", format!("{live} live")),
        Space::with_width(Length::Fill),
        button(
            text(if ui.show_ended { "Hide ended".to_string() } else { format!("Show ended ({ended})") })
                .size(11)
                .font(crate::style::SANS_SEMIBOLD)
                .color(tokens::hex(0xa3aabb))
        )
        .on_press(Message::PlansToggleShowEnded)
        .padding([3, 9])
        .style(|_, status| button::Style {
            background: matches!(status, button::Status::Hovered).then_some(Background::Color(tokens::PANEL2)),
            border: Border { radius: 5.0.into(), ..Default::default() },
            ..Default::default()
        }),
    ]
    .align_y(Alignment::Center)
    .into()];
    let visible = orchestrator_plans::visible(&ui.entries, ui.show_ended);
    if visible.is_empty() {
        items.push(small("No live orchestrator has registered a plan doc (`ninox plan register <file>`).", tokens::DIM));
    }
    for entry in visible {
        items.push(card(app, entry));
    }
    items
}

/// The selected orchestrator's plan: header (state, name, file, "Open in
/// editor", updated, "Open orchestrator") over the doc.
pub fn detail<'a>(app: &'a App) -> Option<Element<'a, Message>> {
    let entry = app.orch_plans.selected_entry()?;
    let editor_is_terminal = ninox_ui_shared::editor::is_terminal(app.config.editor);
    let can_open = !editor_is_terminal && entry.file != FileHealth::Missing;
    let file_name = std::path::Path::new(&entry.file_path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| entry.file_path.clone());
    let editor_link = button(text("Open in editor ↗").size(12).font(crate::style::SANS).color(if can_open { tokens::BLUE } else { tokens::DIM2 }))
        .on_press_maybe(can_open.then(|| Message::OpenInEditor(entry.file_path.clone())))
        .padding(0)
        .style(|_, _| button::Style { background: None, ..Default::default() });
    let editor_link: Element<Message> = if editor_is_terminal {
        iced::widget::tooltip(
            editor_link,
            container(
                text("Neovim is a terminal editor — open this file from the nx TUI's e key instead")
                    .size(11)
                    .font(crate::style::SANS)
                    .color(tokens::DIM),
            )
            .width(Length::Fixed(220.0))
            .padding([10, 12])
            .style(|_| container::Style {
                background: Some(Background::Color(tokens::PANEL2)),
                border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
                ..Default::default()
            }),
            iced::widget::tooltip::Position::Bottom,
        )
        .gap(6)
        .into()
    } else {
        editor_link.into()
    };
    let open = button(text("Open orchestrator").size(11).font(crate::style::SANS_SEMIBOLD).color(tokens::TEXT))
        .on_press(Message::PlansOpenOrchestrator(entry.orchestrator_id.clone()))
        .padding([3, 9])
        .style(|_, _| button::Style {
            background: Some(Background::Color(tokens::PANEL2)),
            border: Border { color: tokens::BORDER, width: 1.0, radius: 5.0.into() },
            ..Default::default()
        });
    let header = container(
        row![
            status_badge(entry),
            text(entry.name.clone()).size(14).font(crate::style::SANS_BOLD).color(tokens::TEXT),
            text(file_name).size(11.5).font(crate::style::MONO).color(tokens::DIM),
            editor_link,
            Space::with_width(Length::Fill),
            text(super::plan_panel::updated_label(Some(entry.updated_at))).size(11.5).font(crate::style::SANS).color(tokens::DIM),
            open,
        ]
        .spacing(10)
        .align_y(Alignment::Center),
    )
    .padding([12, 22])
    .width(Length::Fill)
    .style(|_| container::Style {
        background: Some(Background::Color(tokens::PANEL)),
        border: Border { color: tokens::BORDER, width: 1.0, radius: 0.0.into() },
        ..Default::default()
    });
    Some(
        column![header, crate::components::plan_panel::plan_pane(app, &entry.orchestrator_id, false)]
            .width(Length::Fill)
            .height(Length::Fill)
            .into(),
    )
}

#[cfg(test)]
mod display_path_tests {
    use super::shorten_path;
    use std::ffi::OsStr;

    #[test]
    fn home_prefix_matches_whole_components_only() {
        let home = Some(OsStr::new("/Users/alice"));
        assert_eq!(shorten_path("/Users/alice/plan.md", home), "~/plan.md");
        assert_eq!(shorten_path("/Users/dev/plan.md", home), "/Users/dev/plan.md");
    }
}
