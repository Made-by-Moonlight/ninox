//! File-based reply channel from an orchestrator back to a running TUI's
//! "ninox prompt" (see `docs/superpowers/specs/
//! 2026-10-21-fleet-overview-ui-vision-design.md`, "The ninox prompt").
//!
//! One short-lived JSON file per outstanding prompt at
//! `{sessions_dir}/prompt_replies/{reply_to}.json`, written by `ninox
//! prompt-answer` and consumed (read, then deleted) by the TUI's own tick
//! — see `ninox-tui`'s `event_loop` and `state::apply_prompt_reply`.
//! Mirrors `inbox::write_message`'s atomic tmp-then-rename write for the
//! same reason: the writer (a short-lived CLI invocation) and the reader
//! (the long-running TUI process) are different processes that never
//! coordinate directly.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PromptReply {
    pub answer: Option<String>,
    #[serde(default)]
    pub select: Vec<String>,
}

impl PromptReply {
    /// Nothing useful to show — the CLI rejects writing one of these.
    pub fn is_empty(&self) -> bool {
        self.answer.is_none() && self.select.is_empty()
    }
}

/// `reply_to` reaches here as a raw CLI argument (`ninox prompt-answer
/// <reply_to> ...`), so it must be validated before ever touching a path —
/// without this, a `reply_to` like `../../etc/cron.d/evil` would let
/// `write_reply` create/overwrite an arbitrary file the calling process can
/// write to. Every legitimate `reply_to` is produced by `new_reply_id`
/// (a UUID), so requiring that shape costs nothing real callers notice.
fn reply_path(sessions_dir: &Path, reply_to: &str) -> Result<PathBuf> {
    anyhow::ensure!(
        uuid::Uuid::parse_str(reply_to).is_ok(),
        "reply_to must be the id ninox sent you (a UUID), got {reply_to:?}"
    );
    Ok(sessions_dir.join("prompt_replies").join(format!("{reply_to}.json")))
}

/// Writes `reply` for `reply_to`, atomically (tmp-then-rename).
pub fn write_reply(sessions_dir: &Path, reply_to: &str, reply: &PromptReply) -> Result<()> {
    let path = reply_path(sessions_dir, reply_to)?;
    std::fs::create_dir_all(path.parent().expect("reply_path always has a parent"))?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_string(reply)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Reads and removes the reply for `reply_to`, if one has arrived yet.
/// `Ok(None)` covers both "not written yet" (the common case on every
/// tick while the orchestrator is still thinking) and a malformed file —
/// this never surfaces a parse error to the polling TUI, the same
/// best-effort posture `inbox::read_pending_messages` takes toward a
/// corrupt entry.
pub fn take_reply(sessions_dir: &Path, reply_to: &str) -> Result<Option<PromptReply>> {
    let path = reply_path(sessions_dir, reply_to)?;
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let _ = std::fs::remove_file(&path);
    Ok(serde_json::from_slice(&bytes).ok())
}

/// A fresh id correlating an outgoing ninox-prompt message with the
/// `ninox prompt-answer <id> ...` reply it expects back. Same
/// `Uuid::new_v4()` convention already used in `config.rs`/`harness.rs`/
/// `lifecycle::brain_harvest`.
pub fn new_reply_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const ID: &str = "11111111-1111-1111-1111-111111111111";

    #[test]
    fn take_reply_is_none_before_any_write() {
        let dir = tempdir().unwrap();
        assert_eq!(take_reply(dir.path(), ID).unwrap(), None);
    }

    #[test]
    fn write_then_take_round_trips_and_consumes() {
        let dir = tempdir().unwrap();
        let reply = PromptReply { answer: Some("nothing's blocking it".into()), select: vec!["worker-1".into()] };
        write_reply(dir.path(), ID, &reply).unwrap();

        assert_eq!(take_reply(dir.path(), ID).unwrap(), Some(reply));
        assert_eq!(
            take_reply(dir.path(), ID).unwrap(),
            None,
            "a second read must not see a stale reply"
        );
    }

    #[test]
    fn take_reply_ignores_a_malformed_file_rather_than_erroring() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("prompt_replies").join(format!("{ID}.json"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not json").unwrap();

        assert_eq!(take_reply(dir.path(), ID).unwrap(), None);
    }

    #[test]
    fn write_reply_rejects_a_reply_to_that_is_not_a_uuid() {
        let dir = tempdir().unwrap();
        let reply = PromptReply { answer: Some("x".into()), select: vec![] };

        assert!(write_reply(dir.path(), "../../etc/cron.d/evil", &reply).is_err());
        assert!(
            !dir.path().parent().unwrap().join("etc").exists(),
            "a non-UUID reply_to must never be allowed to escape sessions_dir"
        );
    }

    #[test]
    fn take_reply_rejects_a_reply_to_that_is_not_a_uuid() {
        let dir = tempdir().unwrap();
        assert!(take_reply(dir.path(), "../../etc/passwd").is_err());
    }

    #[test]
    fn is_empty_true_only_with_neither_field_set() {
        assert!(PromptReply::default().is_empty());
        assert!(!PromptReply { answer: Some("x".into()), select: vec![] }.is_empty());
        assert!(!PromptReply { answer: None, select: vec!["n".into()] }.is_empty());
    }

    #[test]
    fn new_reply_id_is_fresh_each_call() {
        assert_ne!(new_reply_id(), new_reply_id());
    }
}
