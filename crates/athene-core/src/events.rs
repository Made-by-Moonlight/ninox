use crate::{github::GitHubClient, store::Store, types::*};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{broadcast, Mutex};

#[derive(Debug, Clone)]
pub enum Event {
    OrchestratorSpawned(Orchestrator),
    OrchestratorRemoved(OrchestratorId),
    SessionUpdated(Session),
    SessionSpawned(Session),
    SessionDone(SessionId),
    TerminalOutput { session_id: SessionId, bytes: Vec<u8> },
    CiUpdated      { pr_id: PrId, status: CIStatus },
    PrOpened       { session_id: SessionId, pr: PR },
    ReviewComment  { pr_id: PrId, comment: Comment },
    Notification(Notification),
}

pub struct Engine {
    pub store: Arc<Store>,
    tx: broadcast::Sender<Event>,
    pty_writers:  Mutex<HashMap<SessionId, tokio::sync::mpsc::UnboundedSender<Vec<u8>>>>,
    /// Per-session cancellation senders for active FIFO reader tasks.
    /// Sending () to the stored sender stops the running reader immediately.
    stream_cancel: Mutex<HashMap<SessionId, tokio::sync::oneshot::Sender<()>>>,
    /// Optional GitHub API client. None when no token is configured.
    pub github: Option<GitHubClient>,
}

impl Engine {
    pub fn new(store: Arc<Store>) -> Arc<Self> {
        let (tx, _) = broadcast::channel(256);
        Arc::new(Self {
            store,
            tx,
            pty_writers:   Mutex::new(HashMap::new()),
            stream_cancel: Mutex::new(HashMap::new()),
            github:        None,
        })
    }

    pub fn new_with_github(store: Arc<Store>, token: String) -> Arc<Self> {
        let (tx, _) = broadcast::channel(256);
        let github = GitHubClient::new(token).ok();
        Arc::new(Self {
            store,
            tx,
            pty_writers:   Mutex::new(HashMap::new()),
            stream_cancel: Mutex::new(HashMap::new()),
            github,
        })
    }

    /// Cancel any running FIFO reader for `session_id` and return a fresh
    /// cancellation receiver for the new reader.  Call this at the top of
    /// every `start_streaming` invocation.
    pub async fn register_stream(
        &self,
        session_id: SessionId,
    ) -> tokio::sync::oneshot::Receiver<()> {
        let mut map = self.stream_cancel.lock().await;
        if let Some(old_tx) = map.remove(&session_id) {
            let _ = old_tx.send(());
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        map.insert(session_id, tx);
        rx
    }

    pub fn emit(&self, event: Event) {
        let _ = self.tx.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    pub async fn register_pty_writer(
        &self,
        session_id: SessionId,
        writer: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    ) {
        self.pty_writers.lock().await.insert(session_id, writer);
    }

    pub async fn get_pty_writer(
        &self,
        session_id: &str,
    ) -> Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>> {
        self.pty_writers.lock().await.get(session_id).cloned()
    }

    /// Kill all worker sessions belonging to an orchestrator, delete from DB, emit events.
    pub async fn remove_orchestrator(&self, orchestrator_id: &str) -> anyhow::Result<()> {
        let workers = self.store.sessions_by_orchestrator(orchestrator_id)?;
        for session in &workers {
            let _ = crate::tmux::kill_session(&session.id).await;
        }
        // Also kill the orchestrator's own tmux session (same id as orchestrator).
        let _ = crate::tmux::kill_session(orchestrator_id).await;
        self.store.delete_orchestrator(orchestrator_id)?;
        for session in workers {
            self.emit(Event::SessionDone(session.id));
        }
        self.emit(Event::OrchestratorRemoved(orchestrator_id.to_string()));
        Ok(())
    }

    /// Kill the tmux session and delete it from the DB entirely.
    pub async fn remove_session(&self, session_id: &str) -> anyhow::Result<()> {
        let _ = crate::tmux::kill_session(session_id).await;
        self.store.delete_session(session_id)?;
        self.emit(Event::SessionDone(session_id.to_string()));
        Ok(())
    }

    /// Kill the tmux session, mark it Terminated in the DB, and emit SessionUpdated.
    pub async fn terminate_session(&self, session_id: &str) -> anyhow::Result<()> {
        // Best-effort tmux kill (session may already be dead).
        let _ = crate::tmux::kill_session(session_id).await;

        if let Some(mut session) = self.store.get_session(session_id)? {
            session.status = crate::types::SessionStatus::Terminated;
            self.store.upsert_session(&session)?;
            self.emit(Event::SessionUpdated(session));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use tempfile::tempdir;

    #[tokio::test]
    async fn emit_received_by_subscriber() {
        let store = Arc::new(Store::open(tempdir().unwrap().into_path().join("t.db")).unwrap());
        let engine = Engine::new(store);
        let mut rx = engine.subscribe();
        engine.emit(Event::SessionDone("s1".into()));
        let event = rx.recv().await.unwrap();
        assert!(matches!(event, Event::SessionDone(id) if id == "s1"));
    }

    #[tokio::test]
    async fn terminate_emits_session_updated() {
        let store = Arc::new(Store::open(tempdir().unwrap().keep().join("t.db")).unwrap());
        let session = crate::types::Session {
            id: "s1".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: crate::types::SessionStatus::Working,
            agent_type: "c".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
        };
        store.upsert_session(&session).unwrap();
        let engine = Engine::new(store);
        let mut rx = engine.subscribe();

        engine.terminate_session("s1").await.unwrap();

        let evt = rx.recv().await.unwrap();
        if let Event::SessionUpdated(s) = evt {
            assert!(matches!(s.status, crate::types::SessionStatus::Terminated));
        } else {
            panic!("expected SessionUpdated");
        }
    }
}
