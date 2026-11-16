//! The Remotes tab — a list of configured SSH-connected machines
//! (`AppConfig.remote_machines.machines`) joined with their last-observed
//! connection health, mirroring the TUI's Remotes tab
//! (`ninox-tui/src/tui/remotes.rs`/`view.rs:1664-1698`). Replaces
//! `View::Remotes`'s placeholder.
//!
//! Styled with the v1 shell's dark/sans token set (`shell::tokens`), the
//! same one `worker_types.rs`/`brain_list.rs` use — not the Field Notes
//! serif/warm system (`crate::style`'s `ColorScheme`-driven helpers).

use iced::widget::{button, column, container, row, scrollable, text, text_input, Canvas, Space};
use iced::{Alignment, Background, Border, Color, Element, Length};

use ninox_core::types::{MachineHealth, Session};
use ninox_ui_shared::machine::RemoteRow;

use super::shell::tokens;
use crate::app::{App, Message};
use crate::style::{MONO, SANS};

/// Remotes tab state: the joined machine list plus whatever in-flight
/// remove-confirm or add-remote flow is active.
#[derive(Debug, Clone, Default)]
pub struct RemotesState {
    pub rows: Vec<RemoteRow>,
    /// `[remote_machines] enabled` — whether the health poller probes at all.
    pub monitoring_enabled: bool,
    /// The machine id a "Remove" click is asking to confirm, if any.
    pub remove_confirm: Option<String>,
    pub add: Option<AddRemoteForm>,
    pub notice: Option<Result<String, String>>,
}

/// The synthetic `SessionId` the embedded `machine add` subprocess's
/// `AttachedClient`/`TerminalState` are keyed under in `state.clients`/
/// `state.terminals` — there's no real ninox session backing this PTY.
pub const MACHINE_ADD_SESSION_ID: &str = "__ninox_machine_add__";

/// The embedded `machine add` terminal canvas's fixed on-screen size
/// (pixels) — shared by `add_remote_modal`'s `Canvas` sizing and
/// `App::spawn_machine_add`'s PTY grid dimensions so the two can never
/// drift apart (the pattern `terminal::cell_size`'s own doc comment
/// recommends). Unlike the main session view, this PTY never resizes.
pub const MACHINE_ADD_CANVAS_SIZE: (f32, f32) = (560.0, 340.0);

/// The PTY grid size (cols, rows) that exactly fills
/// `MACHINE_ADD_CANVAS_SIZE` at the terminal's font size.
pub fn machine_add_cols_rows() -> (u16, u16) {
    let (cell_w, cell_h) = crate::components::terminal::cell_size(crate::components::terminal::FONT_SIZE);
    let (w, h) = MACHINE_ADD_CANVAS_SIZE;
    ((w / cell_w) as u16, (h / cell_h) as u16)
}

/// The "Add remote" form: host/label fields while editing, `running` once
/// the embedded `machine add` subprocess has been spawned (at which point
/// the modal shows its terminal instead of the fields).
#[derive(Debug, Clone, Default)]
pub struct AddRemoteForm {
    pub host: String,
    pub label: String,
    pub error: Option<String>,
    pub running: bool,
    /// Set once the embedded subprocess exits (or is orphaned by navigating
    /// away mid-add): the terminal's last frame stays on screen alongside
    /// this result and a Close button, mirroring the TUI's own
    /// `machine_add_suspended`, which prints "Press Enter to return to
    /// ninox…" and keeps its output on screen rather than discarding it the
    /// instant the subprocess exits.
    pub finished: Option<Result<String, String>>,
}

/// The real `ninox machine add <host> [--label <label>]` argv the TUI's own
/// `machine_add_suspended` (`ninox-tui/src/tui/mod.rs`) shells out to —
/// built the same way so the embedded-terminal subprocess runs identically.
pub fn machine_add_argv(exe: &std::path::Path, host: &str, label: Option<&str>) -> Vec<String> {
    let mut argv = vec![exe.display().to_string(), "machine".into(), "add".into(), host.into()];
    if let Some(label) = label {
        let label = label.trim();
        if !label.is_empty() {
            argv.push("--label".into());
            argv.push(label.into());
        }
    }
    argv
}

pub fn remotes_view(app: &App) -> Element<'_, Message> {
    let header = row![
        column![
            text("Remotes").size(13).font(SANS).color(tokens::TEXT),
            text("SSH-connected machines this fleet can reach.").font(SANS).size(11).color(tokens::DIM),
        ]
        .spacing(3),
        Space::new(Length::Fill, 0),
        small_button("+ Add remote", tokens::TEAL, Message::RemoteAddOpen),
    ]
    .spacing(10)
    .align_y(Alignment::Center)
    .padding([12, 20]);

    let notice: Option<Element<'_, Message>> = app.remotes.notice.as_ref().map(|n| {
        let (msg, color) = match n {
            Ok(msg) => (msg.clone(), tokens::GREEN),
            Err(msg) => (msg.clone(), tokens::RED),
        };
        container(text(msg).size(12).font(SANS).color(color))
            .padding([8, 20])
            .width(Length::Fill)
            .into()
    });

    let body: Element<'_, Message> = if app.remotes.rows.is_empty() {
        container(
            column![
                text("No remote machines configured").size(13).font(SANS).color(tokens::DIM),
                text("ninox machine add").font(MONO).size(11).color(tokens::DIM2),
            ]
            .spacing(6)
            .align_x(Alignment::Center),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .into()
    } else {
        scrollable(
            column(app.remotes.rows.iter().map(|r| remote_row(app, r)).collect::<Vec<_>>())
                .spacing(2)
                .padding(14),
        )
        .width(Length::Fill)
        .into()
    };

    let mut layout = column![header, crate::style::hline(tokens::BORDER, 1.0)];
    if let Some(notice) = notice {
        layout = layout.push(notice);
    }
    layout.push(body)
        .height(Length::Fill)
        .into()
}

fn remote_row<'a>(app: &App, row_data: &RemoteRow) -> Element<'a, Message> {
    if app.remotes.remove_confirm.as_deref() == Some(row_data.id.as_str()) {
        return remove_confirm_row(row_data);
    }

    let (word, color) = health_status(row_data, app.remotes.monitoring_enabled);
    let status = container(
        row![
            text("●").size(11).color(color),
            text(word).font(SANS).size(11.5).color(color),
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    )
    .width(Length::Fixed(STATUS_COLUMN_WIDTH));
    let live = live_sessions_on(app.sessions.values(), &row_data.id);
    row![
        status,
        text(row_data.label.clone()).font(SANS).size(13).color(tokens::TEXT),
        text(row_data.ssh_target.clone()).font(MONO).size(11.5).color(tokens::DIM),
        text(format!("· {}", row_data.remote_session)).font(SANS).size(11.5).color(tokens::DIM),
        Space::new(Length::Fill, 0),
        text(session_count_label(live)).font(SANS).size(11.5).color(tokens::DIM2),
        ghost_remove_button(Message::RemoteRemoveRequest(row_data.id.clone())),
    ]
    .spacing(12)
    .align_y(Alignment::Center)
    .padding([6, 10])
    .into()
}

/// Fixed so labels line up down the list whichever status word a row shows
/// ("unreachable" is the widest).
const STATUS_COLUMN_WIDTH: f32 = 104.0;

/// Health dot colour + word for a row, using the Legend's connection-badge
/// colours (green/amber/red). `Reconnecting` is a failed probe still in
/// backoff — the host didn't answer, so it already reads "unreachable"
/// (amber = still retrying, red = backoff capped). `monitoring_enabled` is
/// `[remote_machines] enabled`: with it off the poller never probes, so
/// every row is "disabled" rather than a "checking" that never resolves.
pub fn health_status(row: &RemoteRow, monitoring_enabled: bool) -> (&'static str, Color) {
    if !monitoring_enabled || !row.enabled {
        return ("disabled", tokens::GRAY);
    }
    match row.health {
        Some(MachineHealth::Connected) => ("reachable", tokens::GREEN),
        Some(MachineHealth::Reconnecting) => ("unreachable", tokens::AMBER),
        Some(MachineHealth::Unreachable) => ("unreachable", tokens::RED),
        None => ("checking", tokens::DIM),
    }
}

/// Live (non-terminal) sessions running on the remote `machine_id`.
pub fn live_sessions_on<'a>(sessions: impl IntoIterator<Item = &'a Session>, machine_id: &str) -> usize {
    sessions
        .into_iter()
        .filter(|s| s.machine_id.as_deref() == Some(machine_id) && !s.status.is_terminal())
        .count()
}

pub fn session_count_label(n: usize) -> String {
    match n {
        0 => "—".to_string(),
        1 => "1 session".to_string(),
        n => format!("{n} sessions"),
    }
}

/// A quiet text-only "Remove" that only turns red on hover — a solid red
/// button on every row is too loud for a non-urgent action.
fn ghost_remove_button<'a>(on_press: Message) -> Element<'a, Message> {
    button(text("Remove").font(SANS).size(11.5))
        .on_press(on_press)
        .padding([4, 8])
        .style(|_theme, status| {
            let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
            button::Style {
                background: hovered.then_some(Background::Color(Color { a: 0.10, ..tokens::RED })),
                text_color: if hovered { tokens::RED } else { tokens::DIM2 },
                border: Border { radius: 4.0.into(), ..Default::default() },
                ..Default::default()
            }
        })
        .into()
}

fn remove_confirm_row<'a>(row_data: &RemoteRow) -> Element<'a, Message> {
    row![
        text(format!("Remove {}? This won't touch the remote machine.", row_data.label))
            .font(SANS)
            .size(12)
            .color(tokens::TEXT),
        Space::new(Length::Fill, 0),
        small_button("Cancel", tokens::DIM, Message::RemoteRemoveCancel),
        small_button("Confirm", tokens::RED, Message::RemoteRemoveConfirm),
    ]
    .spacing(10)
    .align_y(Alignment::Center)
    .padding([6, 10])
    .into()
}

fn small_button<'a>(label: &str, text_color: Color, on_press: Message) -> Element<'a, Message> {
    button(text(label.to_string()).font(SANS).size(11.5).color(text_color))
        .on_press(on_press)
        .padding([5, 10])
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

/// The "Add remote" modal: a host/label form while editing, or — once
/// `RemoteAddSubmit` has spawned the real `ninox machine add` subprocess —
/// its live terminal, so the SSH host-key prompt, install offer, and
/// session picker pass through to the user exactly as the TUI's own
/// `machine_add_suspended` shows them (never auto-accepted). Follows
/// `catalogue_modal.rs`'s backdrop+card structure, in v1 tokens.
pub fn add_remote_modal<'a>(app: &'a App, form: &'a AddRemoteForm) -> Element<'a, Message> {
    let header = container(text("Add remote").size(15).font(SANS).color(tokens::TEXT))
        .padding([16, 20])
        .width(Length::Fill)
        .style(|_| container::Style { background: Some(Background::Color(tokens::PANEL2)), ..Default::default() });

    let (canvas_w, canvas_h) = MACHINE_ADD_CANVAS_SIZE;

    let body: Element<'a, Message> = if form.running {
        let Some(term_state) = app.terminals.get(MACHINE_ADD_SESSION_ID) else {
            return backdrop(container(column![header]).width(Length::Fixed(canvas_w)));
        };
        let terminal: Element<'a, Message> = Canvas::new(crate::components::terminal::TerminalWidget {
            state: term_state,
            session_id: MACHINE_ADD_SESSION_ID.to_string(),
            font_size: crate::components::terminal::FONT_SIZE,
            terminal_bg: tokens::BG,
            terminal_fg: tokens::TEXT,
            cursor_color: tokens::TEAL,
            ansi: app.scheme.ansi,
            session_ids: vec![],
            focused: true,
        })
        .width(Length::Fixed(canvas_w))
        .height(Length::Fixed(canvas_h))
        .into();

        match &form.finished {
            // Still running: just the live terminal, no buttons — Esc is
            // the only way out while the process is live (it kills it).
            None => terminal,
            // Exited (or orphaned by navigating away mid-add): freeze the
            // terminal on screen next to the inferred result, same reason
            // the TUI's own `machine_add_suspended` keeps its output up
            // until the user presses Enter rather than discarding it the
            // instant the subprocess exits.
            Some(result) => {
                let (msg, color) = match result {
                    Ok(msg) => (msg.clone(), tokens::GREEN),
                    Err(msg) => (msg.clone(), tokens::RED),
                };
                column![
                    terminal,
                    Space::new(0, 10),
                    text(msg).size(12).font(SANS).color(color),
                    Space::new(0, 10),
                    row![
                        Space::new(Length::Fill, 0),
                        small_button("Close", tokens::TEAL, Message::RemoteAddClose),
                    ],
                ]
                .padding(iced::Padding { top: 0.0, right: 20.0, bottom: 16.0, left: 20.0 })
                .into()
            }
        }
    } else {
        let error_line: Option<Element<'a, Message>> = form.error.as_ref().map(|msg| {
            text(msg.clone()).size(11.5).font(SANS).color(tokens::RED).into()
        });

        let mut fields = column![
            text("Host").size(11).font(SANS).color(tokens::DIM2),
            text_input("user@host or an ssh config alias", &form.host)
                .on_input(Message::RemoteAddHostChanged)
                .font(MONO)
                .size(13)
                .padding([6, 8])
                .style(input_style),
            Space::new(0, 10),
            text("Label (optional)").size(11).font(SANS).color(tokens::DIM2),
            text_input("", &form.label)
                .on_input(Message::RemoteAddLabelChanged)
                .font(SANS)
                .size(13)
                .padding([6, 8])
                .style(input_style),
        ]
        .spacing(4)
        .padding([18, 20]);
        if let Some(err) = error_line {
            fields = fields.push(Space::new(0, 10)).push(err);
        }
        fields = fields.push(Space::new(0, 16)).push(
            row![
                Space::new(Length::Fill, 0),
                small_button("Cancel", tokens::DIM, Message::RemoteAddCancel),
                small_button("Add", tokens::TEAL, Message::RemoteAddSubmit),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        );
        fields.into()
    };

    backdrop(
        container(column![header, crate::style::hline(tokens::BORDER, 1.0), body])
            .width(Length::Fixed(canvas_w))
            .style(|_| container::Style {
                background: Some(Background::Color(tokens::PANEL)),
                border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
                ..Default::default()
            }),
    )
}

fn backdrop<'a>(modal: container::Container<'a, Message>) -> Element<'a, Message> {
    container(modal)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .style(|_| container::Style {
            background: Some(Background::Color(Color { a: 0.6, ..Color::BLACK })),
            ..Default::default()
        })
        .into()
}

fn input_style(_theme: &iced::Theme, _status: text_input::Status) -> text_input::Style {
    text_input::Style {
        background: Background::Color(tokens::PANEL2),
        border: Border { color: tokens::BORDER, width: 1.0, radius: 4.0.into() },
        icon: tokens::DIM,
        placeholder: tokens::DIM2,
        value: tokens::TEXT,
        selection: tokens::TEAL,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninox_core::types::{ActivityState, Session, SessionStatus};

    fn row(enabled: bool, health: Option<MachineHealth>) -> RemoteRow {
        RemoteRow {
            id: "m1".into(), label: "box".into(), ssh_target: "u@box".into(),
            remote_session: "default".into(), enabled, health,
        }
    }

    fn session_on(machine_id: Option<&str>, status: SessionStatus) -> Session {
        Session {
            id: "s".into(), orchestrator_id: Some("o".into()), name: "s-name".into(),
            repo: "o/r".into(), status, agent_type: "claude-code".into(), cost_usd: 0.0,
            started_at: 0, pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None, context_used_pct: None,
            context_total_tokens: None, context_window_size: None, claude_session_id: None,
            summary: None, terminal_at: None, gate_status: None, merged_at: None,
            activity: ActivityState::Unknown, activity_note: None, activity_since: None,
            machine_id: machine_id.map(str::to_string), worker_type: None,
            success_criteria: Vec::new(), completion_outcome: None, self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        }
    }

    #[test]
    fn health_status_words_and_colours() {
        assert_eq!(health_status(&row(true, Some(MachineHealth::Connected)), true), ("reachable", tokens::GREEN));
        // A failed probe still in backoff (`Reconnecting`) already means the
        // host didn't answer — a fake host must read "unreachable" from its
        // first failed probe, not only after the backoff caps out.
        assert_eq!(health_status(&row(true, Some(MachineHealth::Reconnecting)), true), ("unreachable", tokens::AMBER));
        assert_eq!(health_status(&row(true, Some(MachineHealth::Unreachable)), true), ("unreachable", tokens::RED));
        assert_eq!(health_status(&row(true, None), true), ("checking", tokens::DIM));
    }

    #[test]
    fn health_status_is_disabled_for_a_paused_profile_or_monitoring_off() {
        assert_eq!(health_status(&row(false, Some(MachineHealth::Connected)), true), ("disabled", tokens::GRAY));
        // `[remote_machines] enabled = false`: the poller never probes, so
        // "checking" would be a lie that never resolves.
        assert_eq!(health_status(&row(true, None), false), ("disabled", tokens::GRAY));
    }

    #[test]
    fn live_sessions_on_counts_only_live_sessions_on_that_machine() {
        let sessions = [
            session_on(Some("m1"), SessionStatus::Working),
            session_on(Some("m1"), SessionStatus::Working),
            session_on(Some("m1"), SessionStatus::Terminated),
            session_on(Some("m2"), SessionStatus::Working),
            session_on(None, SessionStatus::Working),
        ];
        assert_eq!(live_sessions_on(sessions.iter(), "m1"), 2);
        assert_eq!(live_sessions_on(sessions.iter(), "m3"), 0);
    }

    #[test]
    fn session_count_label_uses_a_dash_for_none() {
        assert_eq!(session_count_label(0), "—");
        assert_eq!(session_count_label(1), "1 session");
        assert_eq!(session_count_label(3), "3 sessions");
    }

    #[test]
    fn machine_add_argv_includes_label_only_when_given() {
        let exe = std::path::Path::new("/usr/local/bin/ninox");
        assert_eq!(
            machine_add_argv(exe, "example.com", None),
            vec!["/usr/local/bin/ninox", "machine", "add", "example.com"],
        );
        assert_eq!(
            machine_add_argv(exe, "example.com", Some("prod")),
            vec!["/usr/local/bin/ninox", "machine", "add", "example.com", "--label", "prod"],
        );
    }
}
