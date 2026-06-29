use crate::{store::Store, types::*};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{broadcast, Mutex};

#[derive(Debug, Clone)]
pub enum Event {
    OrchestratorSpawned(Orchestrator),
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
    pty_writers: Mutex<HashMap<SessionId, tokio::sync::mpsc::UnboundedSender<Vec<u8>>>>,
}

impl Engine {
    pub fn new(store: Arc<Store>) -> Arc<Self> {
        let (tx, _) = broadcast::channel(256);
        Arc::new(Self { store, tx, pty_writers: Mutex::new(HashMap::new()) })
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
