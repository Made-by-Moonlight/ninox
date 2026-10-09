//! [`SessionBackend`] over the private tmux server — a thin delegation to
//! `crate::tmux`, whose behaviour is unchanged by the seam.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use super::{Backend, LiveSession, PaneIdentity, SessionBackend};
use crate::events::Engine;
use crate::tmux;
use crate::types::SessionId;

pub struct TmuxBackend;

#[async_trait]
impl SessionBackend for TmuxBackend {
    fn kind(&self) -> Backend {
        Backend::Tmux
    }

    async fn knows(&self, id: &str) -> bool {
        tmux::has_session(id).await
    }

    async fn create_session(&self, id: &str, workspace: &str, cmd: &str, env: &[(&str, &str)]) -> Result<()> {
        tmux::create_session(id, workspace, cmd, env).await
    }

    async fn kill_session(&self, id: &str) -> Result<()> {
        tmux::kill_session(id).await
    }

    async fn has_session(&self, id: &str) -> bool {
        tmux::has_session(id).await
    }

    async fn list_sessions(&self) -> Result<Vec<LiveSession>> {
        Ok(tmux::list_sessions()
            .await?
            .into_iter()
            .map(|s| LiveSession { id: s.id, created_ms: s.created_ms, pid: s.pid, backend: Backend::Tmux })
            .collect())
    }

    async fn session_pid(&self, id: &str) -> Option<u32> {
        tmux::list_sessions().await.ok()?.into_iter().find(|s| s.id == id)?.pid
    }

    async fn attach_args(&self, id: &str) -> Vec<String> {
        tmux::attach_args(id).await
    }

    async fn history_size(&self, id: &str) -> i64 {
        tmux::history_size(id).await
    }

    async fn capture_history(&self, id: &str, start: i64, end: i64) -> Vec<u8> {
        tmux::capture_history(id, start, end).await
    }

    async fn read_screen(&self, id: &str, scrollback: usize, ansi: bool) -> Result<String> {
        tmux::capture_screen(id, scrollback, ansi).await
    }

    async fn send_keys(&self, id: &str, text: &str) -> Result<()> {
        tmux::send_keys(id, text).await
    }

    async fn wake_idle_session(&self, id: &str) -> Result<()> {
        tmux::wake_idle_session(id).await
    }

    async fn wait_for_input_prompt(&self, id: &str, timeout: Duration) -> bool {
        tmux::wait_for_input_prompt(id, timeout).await
    }

    async fn write_input(&self, id: &str, bytes: &[u8]) -> Result<()> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let buf = format!("ninox-in-{id}-{pid}-{n}");
        let tmp = std::env::temp_dir().join(format!("{buf}.tmp"));
        tmux::paste_buffer(id, &buf, &tmp.to_string_lossy(), bytes).await
    }

    async fn start_streaming(&self, engine: Arc<Engine>, session_id: SessionId, id: &str) -> Result<()> {
        crate::pty::start_tmux_streaming(engine, session_id, id).await
    }

    async fn current_pane_identity(&self) -> Result<Option<PaneIdentity>> {
        tmux::current_private_pane_identity().await
    }
}
