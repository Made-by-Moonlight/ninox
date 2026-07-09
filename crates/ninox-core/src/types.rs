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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum GateCheck {
    Passing,
    Failing,
    Pending,
    Unknown,
}

/// Structured snapshot of the three raw signals `derive_session_status`
/// already collapses into one `SessionStatus` — kept separately so the UI
/// can explain *which* check is blocking and *since when*, not just the
/// single derived enum value. Current-state only: no transition history.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GateStatus {
    pub ci:        GateCheck,
    pub review:    GateCheck,
    pub mergeable: GateCheck,
    /// Epoch ms this exact (ci, review, mergeable) combination was first
    /// observed — reset whenever any of the three values changes.
    pub since: i64,
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
    /// Structured CI/review/mergeable breakdown behind the current
    /// `status`. `None` until the first GitHub enrichment tick for a
    /// session with an open PR (`Spawning`/`Working` sessions have no PR
    /// yet, so no gate to report). `#[serde(default)]` for wire/DB
    /// back-compat with sessions recorded before this field existed.
    #[serde(default)]
    pub gate_status: Option<GateStatus>,
}

/// Which fields of a `Session` a particular `Event::SessionUpdated` carries
/// fresh, authoritative values for. Every producer of that event is read
/// from a DB snapshot taken at the start of its own tick — a snapshot that
/// can be stale for fields *other* actors are concurrently writing. Flagging
/// exactly the fields a given tick just persisted, and merging field-by-field
/// on the receiving end (`Session::merge_from`), means a stale snapshot can
/// never stomp a fresher value for a field it isn't authoritative for —
/// closing the class of bug fixed one field at a time in PR #57.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionFields(u16);

impl SessionFields {
    pub const NONE:        Self = Self(0);
    pub const STATUS:      Self = Self(1 << 0);
    pub const GATE:        Self = Self(1 << 1);
    /// `pr_number`, `pr_id`, and `repo` travel together — they're only ever
    /// self-healed/adopted as a unit (see `poller.rs`'s repo/PR self-heal).
    pub const PR_LINK:     Self = Self(1 << 2);
    pub const COST:        Self = Self(1 << 3);
    /// `context_tokens`, `context_used_pct`, `context_total_tokens`,
    /// `context_window_size` — all sourced from the same usage/statusline
    /// snapshot, so they travel together too.
    pub const CONTEXT:     Self = Self(1 << 4);
    pub const TERMINAL_AT: Self = Self(1 << 5);
    pub const PID:         Self = Self(1 << 6);
    pub const WORKSPACE:   Self = Self(1 << 7);
    pub const MODEL:       Self = Self(1 << 8);
    /// Full-struct replace — only for the spawn-completion event, where the
    /// row is transitioning from an optimistic placeholder to its first real
    /// snapshot and every field is being established for the first time.
    pub const ALL:         Self = Self(0xFFFF);

    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl std::ops::BitOr for SessionFields {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl Session {
    /// Copy only the fields flagged in `fields` from `incoming` onto `self`.
    /// See `SessionFields`'s doc comment for why this must never be a
    /// wholesale replace except when `fields == SessionFields::ALL`.
    pub fn merge_from(&mut self, incoming: &Session, fields: SessionFields) {
        if fields == SessionFields::ALL {
            *self = incoming.clone();
            return;
        }
        if fields.contains(SessionFields::STATUS) {
            self.status = incoming.status.clone();
        }
        if fields.contains(SessionFields::GATE) {
            self.gate_status = incoming.gate_status.clone();
        }
        if fields.contains(SessionFields::PR_LINK) {
            self.pr_number = incoming.pr_number;
            self.pr_id = incoming.pr_id;
            self.repo = incoming.repo.clone();
        }
        if fields.contains(SessionFields::COST) {
            self.cost_usd = incoming.cost_usd;
        }
        if fields.contains(SessionFields::CONTEXT) {
            self.context_tokens = incoming.context_tokens;
            self.context_used_pct = incoming.context_used_pct;
            self.context_total_tokens = incoming.context_total_tokens;
            self.context_window_size = incoming.context_window_size;
        }
        if fields.contains(SessionFields::TERMINAL_AT) {
            self.terminal_at = incoming.terminal_at;
        }
        if fields.contains(SessionFields::PID) {
            self.pid = incoming.pid;
        }
        if fields.contains(SessionFields::WORKSPACE) {
            self.workspace_path = incoming.workspace_path.clone();
        }
        if fields.contains(SessionFields::MODEL) {
            self.model = incoming.model.clone();
        }
    }
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

    fn base_session() -> Session {
        Session {
            id: "s1".into(), orchestrator_id: None, name: "w".into(),
            repo: "r1".into(), status: SessionStatus::Working,
            agent_type: "claude-code".into(), cost_usd: 1.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: Some("/ws".into()),
            pid: Some(111), model: Some("m1".into()), context_tokens: Some(10),
            catalogue_path: None, context_used_pct: Some(1.0),
            context_total_tokens: Some(10), context_window_size: Some(200_000),
            claude_session_id: None, summary: None, terminal_at: None,
            gate_status: None,
        }
    }

    #[test]
    fn merge_from_gate_copies_only_when_flagged() {
        let mut existing = base_session();
        let mut incoming = base_session();
        incoming.gate_status = Some(GateStatus {
            ci: GateCheck::Failing, review: GateCheck::Passing,
            mergeable: GateCheck::Unknown, since: 42,
        });

        existing.merge_from(&incoming, SessionFields::COST); // GATE not flagged
        assert_eq!(existing.gate_status, None, "unflagged GATE must not be copied");

        existing.merge_from(&incoming, SessionFields::GATE);
        assert_eq!(existing.gate_status, incoming.gate_status, "flagged GATE must be copied");
    }

    #[test]
    fn merge_from_only_copies_flagged_fields() {
        let mut existing = base_session();
        let mut incoming = base_session();
        // Incoming carries a *stale* repo/pr fields (as if read before another
        // actor's write landed) but a fresh cost_usd.
        incoming.repo = "stale-repo".into();
        incoming.cost_usd = 42.0;

        existing.merge_from(&incoming, SessionFields::COST);

        assert_eq!(existing.cost_usd, 42.0, "flagged field must be copied");
        assert_eq!(existing.repo, "r1", "unflagged field must survive untouched");
    }

    #[test]
    fn merge_from_disjoint_updates_do_not_stomp_each_other() {
        // Simulates two out-of-order Event::SessionUpdated arrivals touching
        // disjoint fields — this is the regression test PR #57's fix lacked
        // at the general level.
        let mut state = base_session();

        let mut a = base_session();
        a.status = SessionStatus::PrOpen;
        a.pr_number = Some(7);
        state.merge_from(&a, SessionFields::STATUS | SessionFields::PR_LINK);

        let mut b = base_session(); // stale snapshot: still pr_number None
        b.cost_usd = 9.99;
        state.merge_from(&b, SessionFields::COST);

        assert!(matches!(state.status, SessionStatus::PrOpen), "A's status must survive B's arrival");
        assert_eq!(state.pr_number, Some(7), "A's pr_number must survive B's arrival");
        assert_eq!(state.cost_usd, 9.99, "B's cost_usd must still apply");
    }

    #[test]
    fn merge_from_all_replaces_the_whole_struct() {
        let mut existing = base_session();
        let mut incoming = base_session();
        incoming.name = "brand-new-name".into();
        incoming.pid = Some(999);

        existing.merge_from(&incoming, SessionFields::ALL);

        assert_eq!(existing.name, "brand-new-name");
        assert_eq!(existing.pid, Some(999));
    }

    #[test]
    fn session_fields_bitor_combines_flags() {
        let combined = SessionFields::STATUS | SessionFields::PR_LINK;
        assert!(combined.contains(SessionFields::STATUS));
        assert!(combined.contains(SessionFields::PR_LINK));
        assert!(!combined.contains(SessionFields::COST));
    }
}
