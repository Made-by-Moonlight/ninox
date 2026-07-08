use serde::{Deserialize, Serialize};

pub type SessionId      = String;
pub type OrchestratorId = String;
pub type PrId           = i64;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Spawning, Working, PrOpen, CiFailed,
    ReviewPending, Mergeable, Done, Terminated,
    /// Its tmux pane died along with the private tmux server (e.g. a
    /// reboot) rather than exiting on its own. Distinct from `Terminated`
    /// ("gone for good") — an `Interrupted` session has a
    /// `claude_session_id` and a harness capable of `--resume`, so the
    /// user can pick the exact same conversation back up. Never set
    /// silently: only the startup reconciliation in `app.rs` assigns it,
    /// and only a user-triggered Resume action clears it.
    Interrupted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id:             SessionId,
    pub orchestrator_id:Option<OrchestratorId>,
    pub name:           String,
    pub repo:           String,
    pub status:         SessionStatus,
    pub agent_type:     String,
    pub cost_usd:       f64,
    pub started_at:     i64,
    pub pr_number:      Option<u64>,
    pub pr_id:          Option<PrId>,
    pub workspace_path: Option<String>,
    pub pid:            Option<u32>,
    /// Model identifier the session was spawned with (e.g. `"claude-fable-5"`),
    /// mirrors `AgentConfig::model`. `#[serde(default)]` for wire/DB
    /// back-compat with sessions recorded before this field existed.
    #[serde(default)]
    pub model:          Option<String>,
    /// Current context-window occupancy in tokens, as last observed from the
    /// agent's own transcript (see `ninox_core::lifecycle::usage`).
    /// `None` until the usage poller has ingested at least one turn.
    #[serde(default)]
    pub context_tokens: Option<u64>,
    /// Brain catalogue directory this session was spawned with (its
    /// `NINOX_BRAIN`). Recorded so a Re-file can respawn the session
    /// thinking with the same catalogue. `None` for sessions filed before
    /// this field existed (Re-file falls back to the default brain).
    #[serde(default)]
    pub catalogue_path: Option<String>,
    /// Percentage (0-100) of the context window used, as last reported by
    /// Claude Code's own `statusLine` hook (`context_window.used_percentage`
    /// — see `ninox_core::lifecycle::statusline`). More accurate than
    /// `context_tokens` because it accounts for the model's actual window
    /// size and Claude Code's auto-compact buffer. `None` until the
    /// statusline hook has fired at least once for this session.
    #[serde(default)]
    pub context_used_pct: Option<f64>,
    /// Current context-window token count from the same hook payload
    /// (`context_window.total_input_tokens`). `None` until the hook fires.
    #[serde(default)]
    pub context_total_tokens: Option<u64>,
    /// The model's maximum context window size in tokens, from the same
    /// hook payload (`context_window.context_window_size` — 200000 by
    /// default, 1000000 for extended-context models). `None` until the
    /// hook fires.
    #[serde(default)]
    pub context_window_size: Option<u64>,
    /// UUID ninox assigned this session's `claude` CLI process at spawn
    /// time (`--session-id <uuid>`), used to resume the exact same
    /// conversation later (`--resume <uuid>`) if the tmux pane dies
    /// out from under it (see `docs/superpowers/specs/2026-07-06-session-resume-design.md`).
    /// `None` for legacy sessions and for harnesses with no `resume_args`.
    #[serde(default)]
    pub claude_session_id: Option<String>,
    /// One-line human-readable description of what this session is working
    /// on, derived from the first line of its spawn prompt. Shown on the
    /// fleet board card. `None` for sessions spawned before this field
    /// existed, or if the prompt was empty.
    #[serde(default)]
    pub summary: Option<String>,
    /// Unix epoch milliseconds when this session reached a terminal status
    /// (`Done`/`Terminated`) via the automatic lifecycle poller — set by
    /// `poll_pids` on natural process exit and by merge detection in
    /// `poll_github`. Gates the retention sweep
    /// (`Poller::sweep_retired_sessions`) that purges the record from the
    /// store/fleet board after `SessionRetentionConfig::done_retention_days`.
    /// `None` for non-terminal sessions and for terminal sessions produced
    /// by a direct user action (`terminate_session`/`remove_session`),
    /// which the sweep purges on sight rather than holding for the grace
    /// period. `#[serde(default)]` for wire/DB back-compat.
    #[serde(default)]
    pub terminal_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Orchestrator {
    pub id:         OrchestratorId,
    pub name:       String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PR {
    pub id:         PrId,
    pub number:     u64,
    pub title:      String,
    pub url:        String,
    pub body:       String,
    pub session_id: SessionId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CIStatus {
    pub pr_id:   PrId,
    pub total:   u32,
    pub passing: u32,
    pub failing: u32,
    pub pending: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Comment {
    pub id:         i64,
    pub pr_id:      PrId,
    pub author:     String,
    pub body:       String,
    pub path:       Option<String>,
    pub line:       Option<u32>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum NotificationKind {
    CiFailure, AgentStuck, PrNeedsAttention, MergeConflict, WorkerDone,
    /// A worker session was purged by the retention sweep without its PR
    /// ever being detected merged (e.g. its process exited on its own).
    /// Distinct from `WorkerDone`, which implies the merge succeeded.
    WorkerRetired,
    /// A worker asked the orchestrator to schedule additional work it
    /// discovered outside its own task (`ninox request-work`).
    WorkRequested,
    /// A worker opened a PR beyond the one its session tracks — one worker,
    /// one PR is the contract, so this needs orchestrator attention.
    ExtraPr,
    /// GitHub status/CI/review polling for a session's tracked PR failed
    /// against every configured remote (not just a transient error) — status
    /// enrichment has silently stalled for this session until it recovers.
    GithubLookupFailed,
    /// A newer ninox version is published on the registry than the one
    /// currently running — see `lifecycle::update_check`.
    UpdateAvailable,
    /// `cargo install ninox --force --locked` finished successfully; the
    /// running process is still the old binary until restarted.
    UpdateInstalled,
    /// The `cargo install` subprocess triggered by `UpdateAvailable`'s
    /// "Update now" action exited non-zero.
    UpdateFailed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub id:         String,
    pub kind:       NotificationKind,
    pub title:      String,
    pub body:       String,
    pub session_id: Option<SessionId>,
    /// Unix epoch milliseconds — rendered as the mono timestamp on the
    /// notification slip (spec §7).
    ///
    /// `#[serde(default)]` for wire back-compat: older senders/payloads that
    /// predate this field must still deserialize (as `0`) instead of failing.
    #[serde(default)]
    pub created_at: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_deserializes_without_created_at_for_wire_back_compat() {
        // Payload from a sender that predates the `created_at` field — must
        // not fail to deserialize; missing field defaults to 0.
        let json = r#"{
            "id": "n1",
            "kind": "worker_done",
            "title": "Done",
            "body": "…",
            "session_id": null
        }"#;
        let n: Notification = serde_json::from_str(json).expect("missing created_at must not error");
        assert_eq!(n.created_at, 0);
    }

    #[test]
    fn notification_kind_serde_covers_work_requested_and_extra_pr() {
        for (kind, wire) in [
            (NotificationKind::WorkRequested,  "\"work_requested\""),
            (NotificationKind::ExtraPr,        "\"extra_pr\""),
            (NotificationKind::WorkerRetired,  "\"worker_retired\""),
            (NotificationKind::UpdateAvailable, "\"update_available\""),
            (NotificationKind::UpdateInstalled, "\"update_installed\""),
            (NotificationKind::UpdateFailed,    "\"update_failed\""),
        ] {
            assert_eq!(serde_json::to_string(&kind).unwrap(), wire);
            let parsed: NotificationKind = serde_json::from_str(wire).unwrap();
            assert_eq!(parsed, kind);
        }
    }

    #[test]
    fn notification_round_trips_created_at_when_present() {
        let json = r#"{
            "id": "n1",
            "kind": "worker_done",
            "title": "Done",
            "body": "…",
            "session_id": null,
            "created_at": 12345
        }"#;
        let n: Notification = serde_json::from_str(json).expect("valid payload must deserialize");
        assert_eq!(n.created_at, 12345);
    }

    #[test]
    fn interrupted_status_serializes_snake_case() {
        let json = serde_json::to_string(&SessionStatus::Interrupted).unwrap();
        assert_eq!(json, "\"interrupted\"");
        let back: SessionStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back, SessionStatus::Interrupted);
    }
}
