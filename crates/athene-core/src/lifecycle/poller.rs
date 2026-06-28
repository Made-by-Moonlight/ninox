use crate::{
    events::{Engine, Event},
    lifecycle::probe::is_pid_alive,
    types::SessionStatus,
};
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

pub struct Poller {
    engine: Arc<Engine>,
}

impl Poller {
    pub fn new(engine: Arc<Engine>) -> Self {
        Self { engine }
    }

    pub async fn start(self, token: CancellationToken) {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        loop {
            tokio::select! {
                _ = token.cancelled() => break,
                _ = interval.tick() => self.poll().await,
            }
        }
    }

    async fn poll(&self) {
        let Ok(sessions) = self.engine.store.list_sessions() else {
            return;
        };
        for mut session in sessions {
            if matches!(session.status, SessionStatus::Done | SessionStatus::Terminated) {
                continue;
            }
            if let Some(pid) = session.pid {
                if !is_pid_alive(pid) {
                    session.status = SessionStatus::Terminated;
                    let _ = self.engine.store.upsert_session(&session);
                    self.engine.emit(Event::SessionUpdated(session));
                }
            }
        }
    }
}
