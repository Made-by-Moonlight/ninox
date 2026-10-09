//! An in-memory [`SessionBackend`] that records every call, for testing
//! dispatch without a tmux server or a ptyd host.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use super::{Backend, LiveSession, PaneIdentity, SessionBackend};
use crate::events::Engine;
use crate::types::SessionId;

pub(crate) struct MockBackend {
    pub kind:     Backend,
    pub panes:    Mutex<Vec<String>>,
    pub calls:    Mutex<Vec<String>>,
    pub identity: Option<PaneIdentity>,
    pub fail_kill: bool,
    pub answering: Mutex<bool>,
    /// `ensure_answering` brings an unanswering host up (empty, as after a
    /// reboot).
    pub starts_on_ensure: bool,
}

impl MockBackend {
    pub fn raw(kind: Backend, panes: &[&str]) -> Self {
        Self {
            kind,
            panes: Mutex::new(panes.iter().map(|p| p.to_string()).collect()),
            calls: Mutex::new(Vec::new()),
            identity: None,
            fail_kill: false,
            answering: Mutex::new(true),
            starts_on_ensure: false,
        }
    }

    pub fn new(kind: Backend, panes: &[&str]) -> Arc<Self> {
        Arc::new(Self::raw(kind, panes))
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn record(&self, call: impl Into<String>) {
        self.calls.lock().unwrap().push(call.into());
    }

    fn has(&self, id: &str) -> bool {
        self.panes.lock().unwrap().iter().any(|p| p == id)
    }
}

#[async_trait]
impl SessionBackend for MockBackend {
    fn kind(&self) -> Backend {
        self.kind
    }
    async fn knows(&self, id: &str) -> bool {
        self.has(id)
    }
    async fn create_session(&self, id: &str, _: &str, _: &str, _: &[(&str, &str)]) -> Result<()> {
        self.record(format!("create {id}"));
        self.panes.lock().unwrap().push(id.to_string());
        Ok(())
    }
    async fn kill_session(&self, id: &str) -> Result<()> {
        self.record(format!("kill {id}"));
        anyhow::ensure!(!self.fail_kill, "kill failed");
        self.panes.lock().unwrap().retain(|p| p != id);
        Ok(())
    }
    async fn has_session(&self, id: &str) -> bool {
        self.has(id)
    }
    async fn list_sessions(&self) -> Result<Vec<LiveSession>> {
        Ok(self
            .panes
            .lock()
            .unwrap()
            .iter()
            .map(|id| LiveSession { id: id.clone(), created_ms: 0, pid: None, backend: self.kind })
            .collect())
    }
    async fn session_pid(&self, _: &str) -> Option<u32> {
        None
    }
    async fn attach_args(&self, id: &str) -> Vec<String> {
        vec![format!("{:?}", self.kind), id.to_string()]
    }
    async fn history_size(&self, _: &str) -> i64 {
        0
    }
    async fn capture_history(&self, _: &str, _: i64, _: i64) -> Vec<u8> {
        Vec::new()
    }
    async fn read_screen(&self, id: &str, _: usize, _: bool) -> Result<String> {
        self.record(format!("read {id}"));
        Ok(String::new())
    }
    async fn send_keys(&self, id: &str, _: &str) -> Result<()> {
        self.record(format!("send {id}"));
        Ok(())
    }
    async fn wake_idle_session(&self, id: &str) -> Result<()> {
        self.record(format!("wake {id}"));
        Ok(())
    }
    async fn wait_for_input_prompt(&self, _: &str, _: Duration) -> bool {
        true
    }
    async fn write_input(&self, id: &str, _: &[u8]) -> Result<()> {
        self.record(format!("write {id}"));
        Ok(())
    }
    async fn start_streaming(&self, _: Arc<Engine>, _: SessionId, id: &str) -> Result<()> {
        self.record(format!("stream {id}"));
        Ok(())
    }
    async fn current_pane_identity(&self) -> Result<Option<PaneIdentity>> {
        Ok(self.identity.clone())
    }
    async fn answering(&self) -> bool {
        *self.answering.lock().unwrap()
    }
    async fn ensure_answering(&self, configured: bool) -> Result<()> {
        self.record(format!("ensure configured={configured}"));
        if self.starts_on_ensure {
            *self.answering.lock().unwrap() = true;
            self.panes.lock().unwrap().clear();
        }
        Ok(())
    }
}
