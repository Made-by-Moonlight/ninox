use std::{collections::{HashMap, VecDeque}, sync::Arc};

use athene_core::{
    events::{Engine, Event},
    types::*,
};
use iced::{Element, Subscription, Task, Theme};
use tokio::sync::broadcast;

use crate::components::{session_detail::DetailPanel, terminal::TerminalState};

const MAX_NOTIFICATIONS: usize = 50;

// ---------------------------------------------------------------------------
// View state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct SidebarState {
    pub selected_orchestrator: Option<OrchestratorId>,
}

#[derive(Debug, Clone)]
pub enum View {
    FleetBoard { scope: Option<OrchestratorId> },
    SessionDetail { session_id: SessionId, panel: DetailPanel },
}

impl Default for View {
    fn default() -> Self {
        View::FleetBoard { scope: None }
    }
}

// ---------------------------------------------------------------------------
// App model
// ---------------------------------------------------------------------------

pub struct App {
    pub engine:          Arc<Engine>,
    pub orchestrators:   Vec<Orchestrator>,
    pub sessions:        HashMap<SessionId, Session>,
    pub prs:             HashMap<PrId, PR>,
    pub ci_status:       HashMap<PrId, CIStatus>,
    pub review_threads:  HashMap<PrId, Vec<Comment>>,
    pub notifications:   VecDeque<Notification>,
    pub sidebar:         SidebarState,
    pub view:            View,
    pub terminals:       HashMap<SessionId, TerminalState>,
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Message {
    EngineEvent(Event),
    NavigateFleet { scope: Option<OrchestratorId> },
    NavigateSession(SessionId),
    TerminalInput { session_id: SessionId, bytes: Vec<u8> },
    SelectOrchestrator(Option<OrchestratorId>),
    DismissNotification(String),
    SpawnSession,
}

// ---------------------------------------------------------------------------
// Impl
// ---------------------------------------------------------------------------

impl App {
    pub fn new(engine: Arc<Engine>) -> (Self, Task<Message>) {
        let app = Self {
            engine,
            orchestrators:  vec![],
            sessions:       HashMap::new(),
            prs:            HashMap::new(),
            ci_status:      HashMap::new(),
            review_threads: HashMap::new(),
            notifications:  VecDeque::new(),
            sidebar:        SidebarState::default(),
            view:           View::default(),
            terminals:      HashMap::new(),
        };
        (app, Task::none())
    }

    /// Elm-style consuming update — used by unit tests.
    pub fn update(self, message: Message) -> (Self, Task<Message>) {
        let mut state = self;
        let task = Self::apply(&mut state, message);
        (state, task)
    }

    /// Iced-compatible mutable update — passed to `iced::application()`.
    pub fn iced_update(state: &mut Self, message: Message) -> Task<Message> {
        Self::apply(state, message)
    }

    /// Shared mutation logic.
    fn apply(state: &mut Self, message: Message) -> Task<Message> {
        match message {
            Message::EngineEvent(event) => Self::handle_engine_event(state, event),

            Message::NavigateFleet { scope } => {
                state.view = View::FleetBoard { scope };
                Task::none()
            }

            Message::NavigateSession(id) => {
                state.view = View::SessionDetail { session_id: id, panel: DetailPanel::default() };
                Task::none()
            }

            Message::SelectOrchestrator(id) => {
                state.sidebar.selected_orchestrator = id;
                Task::none()
            }

            Message::TerminalInput { session_id, bytes } => {
                if let Some(term) = state.terminals.get(&session_id) {
                    if let Some(sender) = &term.pty_sender {
                        let _ = sender.send(bytes);
                    }
                }
                Task::none()
            }

            Message::DismissNotification(id) => {
                state.notifications.retain(|n| n.id != id);
                Task::none()
            }

            Message::SpawnSession => {
                // Spawn UI to be implemented in a later task.
                Task::none()
            }
        }
    }

    fn handle_engine_event(state: &mut Self, event: Event) -> Task<Message> {
        match event {
            Event::SessionSpawned(session) => {
                state.sessions.insert(session.id.clone(), session);
            }

            Event::SessionUpdated(session) => {
                state.sessions.insert(session.id.clone(), session);
            }

            Event::SessionDone(id) => {
                if let Some(s) = state.sessions.get_mut(&id) {
                    s.status = SessionStatus::Done;
                }
                state.terminals.remove(&id);
            }

            Event::TerminalOutput { session_id, bytes } => {
                if let Some(term) = state.terminals.get_mut(&session_id) {
                    term.process(&bytes);
                }
            }

            Event::CiUpdated { pr_id, status } => {
                state.ci_status.insert(pr_id, status);
            }

            Event::PrOpened { session_id, pr } => {
                if let Some(s) = state.sessions.get_mut(&session_id) {
                    s.pr_number = Some(pr.number);
                    s.pr_id     = Some(pr.id);
                }
                state.prs.insert(pr.id, pr);
            }

            Event::ReviewComment { pr_id, comment } => {
                state.review_threads
                    .entry(pr_id)
                    .or_default()
                    .push(comment);
            }

            Event::Notification(n) => {
                if state.notifications.len() >= MAX_NOTIFICATIONS {
                    state.notifications.pop_front();
                }
                state.notifications.push_back(n);
            }
        }
        Task::none()
    }

    /// View — sidebar + fleet board or session detail.
    pub fn iced_view(state: &Self) -> Element<'_, Message> {
        use iced::widget::{column, container, row};
        use crate::components::{
            fleet_board::fleet_board,
            session_detail::session_detail,
            sidebar::sidebar,
        };
        use crate::theme::{BG_BASE, BG_SURFACE, TEXT_MUTED};
        use iced::{Background, Border, Length};

        let titlebar = container(
            iced::widget::text("⬡ Athene")
                .size(13)
                .color(TEXT_MUTED),
        )
        .padding([6, 16])
        .width(Length::Fill)
        .style(move |_theme| container::Style {
            background: Some(Background::Color(BG_SURFACE)),
            border: Border {
                color: crate::theme::BORDER,
                width: 0.0,
                radius: 0.0.into(),
            },
            ..Default::default()
        });

        let main: Element<Message> = match &state.view {
            View::FleetBoard { scope } => fleet_board(state, scope.as_ref()),
            View::SessionDetail { session_id, panel } => session_detail(state, session_id, panel),
        };

        container(
            column![
                titlebar,
                row![sidebar(state), main].height(Length::Fill),
            ]
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .style(move |_theme| container::Style {
            background: Some(Background::Color(BG_BASE)),
            ..Default::default()
        })
        .into()
    }

    /// Subscription that drives `Message::EngineEvent` from the engine broadcast channel.
    pub fn subscription(state: &Self) -> Subscription<Message> {
        let mut rx: broadcast::Receiver<Event> = state.engine.subscribe();
        Subscription::run_with_id(
            "engine-events",
            async_stream::stream! {
                loop {
                    match rx.recv().await {
                        Ok(event)  => yield Message::EngineEvent(event),
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed)    => break,
                    }
                }
            },
        )
    }

    /// Theme accessor for the iced `.theme()` builder.
    pub fn theme(_state: &Self) -> Theme {
        crate::theme::athene_theme()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use athene_core::{events::Engine, store::Store};
    use tempfile::tempdir;

    fn test_engine() -> Arc<Engine> {
        let s = Arc::new(
            Store::open(tempdir().unwrap().into_path().join("t.db")).unwrap(),
        );
        Engine::new(s)
    }

    fn base(engine: Arc<Engine>) -> App {
        App {
            engine,
            orchestrators:  vec![],
            sessions:       HashMap::new(),
            prs:            HashMap::new(),
            ci_status:      HashMap::new(),
            review_threads: HashMap::new(),
            notifications:  VecDeque::new(),
            sidebar:        SidebarState::default(),
            view:           View::FleetBoard { scope: None },
            terminals:      HashMap::new(),
        }
    }

    #[test]
    fn session_spawned_inserts() {
        let e = test_engine();
        let m = base(e);
        let s = Session {
            id:              "s1".into(),
            orchestrator_id: None,
            name:            "w".into(),
            repo:            "r".into(),
            status:          SessionStatus::Working,
            agent_type:      "c".into(),
            cost_usd:        0.0,
            started_at:      0,
            pr_number:       None,
            pr_id:           None,
            workspace_path:  None,
            pid:             None,
        };
        let (updated, _) = m.update(Message::EngineEvent(Event::SessionSpawned(s)));
        assert!(updated.sessions.contains_key("s1"));
    }

    #[test]
    fn notifications_capped_at_50() {
        let e = test_engine();
        let mut m = base(e);
        for i in 0..55u32 {
            let (next, _) = m.update(Message::EngineEvent(Event::Notification(Notification {
                id:         i.to_string(),
                kind:       NotificationKind::WorkerDone,
                title:      "t".into(),
                body:       "b".into(),
                session_id: None,
            })));
            m = next;
        }
        assert_eq!(m.notifications.len(), 50);
    }
}
