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
}
