use std::{collections::{HashMap, VecDeque}, sync::Arc, time::{SystemTime, UNIX_EPOCH}};

use athene_core::{
    events::{Engine, Event},
    types::*,
};
use iced::{Element, Subscription, Task, Theme};
use tokio::sync::broadcast;

use crate::components::{session_detail::DetailPanel, spawn_modal::SpawnForm, terminal::TerminalState};

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
    pub spawn_modal:     Option<SpawnForm>,
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
    SpawnSession,
    SpawnFormName(String),
    SpawnFormWorkspace(String),
    SpawnFormConfirm,
    SpawnFormCancel,
    SwitchDetailPanel(crate::components::session_detail::DetailPanel),
    Noop,
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
            spawn_modal:    None,
        };
        (app, Task::none())
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

            Message::SpawnSession => {
                state.spawn_modal = Some(SpawnForm::default());
                Task::none()
            }

            Message::SpawnFormName(v) => {
                if let Some(f) = &mut state.spawn_modal { f.name = v; }
                Task::none()
            }

            Message::SpawnFormWorkspace(v) => {
                if let Some(f) = &mut state.spawn_modal { f.workspace = v; }
                Task::none()
            }

            Message::SpawnFormCancel => {
                state.spawn_modal = None;
                Task::none()
            }

            Message::SpawnFormConfirm => {
                if let Some(form) = state.spawn_modal.take() {
                    let name      = form.name.trim().to_string();
                    let workspace = form.workspace.trim().to_string();
                    if name.is_empty() || workspace.is_empty() {
                        return Task::none();
                    }

                    let ts = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis();

                    let orch = Orchestrator {
                        id:         format!("orch-{ts}"),
                        name:       name.clone(),
                        created_at: ts as i64,
                    };
                    let _ = state.engine.store.upsert_orchestrator(&orch);
                    state.orchestrators.push(orch.clone());
                    state.engine.emit(Event::OrchestratorSpawned(orch.clone()));

                    let session = Session {
                        id:              orch.id.clone(),
                        orchestrator_id: None,
                        name:            name.clone(),
                        repo:            String::new(),
                        status:          SessionStatus::Working,
                        agent_type:      "claude-code".into(),
                        cost_usd:        0.0,
                        started_at:      ts as i64,
                        pr_number:       None,
                        pr_id:           None,
                        workspace_path:  Some(workspace.clone()),
                        pid:             None,
                    };
                    let _ = state.engine.store.upsert_session(&session);
                    state.sessions.insert(session.id.clone(), session.clone());
                    state.engine.emit(Event::SessionSpawned(session));

                    state.view = View::SessionDetail {
                        session_id: orch.id.clone(),
                        panel:      DetailPanel::Terminal,
                    };

                    // Capture values for the async task.
                    let engine  = state.engine.clone();
                    let tmux_id = orch.id.clone();
                    let sid     = orch.id.clone();
                    let ws      = workspace;
                    let nm      = name;
                    let ts_i64  = ts as i64;

                    return Task::future(async move {
                        use athene_core::{pty, tmux, Event as CoreEvent, Session, SessionStatus};

                        if let Err(e) = tmux::create_session(&tmux_id, &ws, "claude", &[]).await {
                            tracing::error!("tmux create failed for {sid}: {e}");
                            return Message::Noop;
                        }

                        // Give the shell a moment to set up the TTY before we open it.
                        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

                        let pid = tmux::list_sessions()
                            .await
                            .ok()
                            .and_then(|ss| ss.into_iter().find(|s| s.id == tmux_id))
                            .and_then(|s| s.pid);

                        let updated = Session {
                            id:              sid.clone(),
                            orchestrator_id: None,
                            name:            nm,
                            repo:            String::new(),
                            status:          SessionStatus::Working,
                            agent_type:      "claude-code".into(),
                            cost_usd:        0.0,
                            started_at:      ts_i64,
                            pr_number:       None,
                            pr_id:           None,
                            workspace_path:  Some(ws),
                            pid,
                        };
                        let _ = engine.store.upsert_session(&updated);

                        if let Err(e) = pty::start_streaming(engine.clone(), sid.clone(), &tmux_id).await {
                            tracing::error!("pty setup failed for {sid}: {e}");
                        }

                        engine.emit(CoreEvent::SessionUpdated(updated));
                        Message::Noop
                    });
                }
                Task::none()
            }

            Message::SwitchDetailPanel(new_panel) => {
                if let View::SessionDetail { panel, .. } = &mut state.view {
                    *panel = new_panel;
                }
                Task::none()
            }

            Message::Noop => Task::none(),
        }
    }

    fn handle_engine_event(state: &mut Self, event: Event) -> Task<Message> {
        match event {
            Event::OrchestratorSpawned(orch) => {
                if !state.orchestrators.iter().any(|o| o.id == orch.id) {
                    state.orchestrators.push(orch);
                }
            }

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
                let title = n.title.clone();
                let body = n.body.clone();
                std::thread::spawn(move || {
                    let _ = notify_rust::Notification::new()
                        .summary(&title)
                        .body(&body)
                        .show();
                });
                state.notifications.push_back(n);
                if state.notifications.len() > MAX_NOTIFICATIONS {
                    state.notifications.pop_front();
                }
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
            spawn_modal::spawn_modal,
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

        let base: Element<Message> = container(
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
        .into();

        if let Some(form) = &state.spawn_modal {
            iced::widget::stack![base, spawn_modal(form)].into()
        } else {
            base
        }
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
impl App {
    pub fn update(self, message: Message) -> (Self, Task<Message>) {
        let mut state = self;
        let task = Self::apply(&mut state, message);
        (state, task)
    }
}

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
            spawn_modal:    None,
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

    #[test]
    fn spawn_form_confirm_inserts_orchestrator_and_navigates() {
        let e = test_engine();
        let mut m = base(e);
        let (next, _) = m.update(Message::SpawnSession);
        m = next;
        assert!(m.spawn_modal.is_some());

        let (next, _) = m.update(Message::SpawnFormName("my-feature".into()));
        m = next;
        let (next, _) = m.update(Message::SpawnFormWorkspace("/tmp".into()));
        m = next;
        let (next, _) = m.update(Message::SpawnFormConfirm);
        m = next;

        assert!(m.spawn_modal.is_none());
        assert_eq!(m.orchestrators.len(), 1);
        assert_eq!(m.orchestrators[0].name, "my-feature");

        // A session with the orchestrator's ID must exist for the terminal view
        let orch_id = &m.orchestrators[0].id;
        assert!(m.sessions.contains_key(orch_id));

        // View should be the session detail for that orchestrator
        assert!(matches!(&m.view, View::SessionDetail { session_id, .. } if session_id == orch_id));
    }

    #[test]
    fn spawn_form_cancel_clears_modal() {
        let e = test_engine();
        let mut m = base(e);
        let (next, _) = m.update(Message::SpawnSession);
        m = next;
        let (next, _) = m.update(Message::SpawnFormCancel);
        m = next;
        assert!(m.spawn_modal.is_none());
        assert!(m.orchestrators.is_empty());
    }
}
