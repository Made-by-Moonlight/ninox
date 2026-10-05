use serde::{Deserialize, Serialize};

pub type SessionId      = String;
pub type OrchestratorId = String;
pub type PrId           = i64;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PooledCheckoutState {
    Provisioning,
    Leased,
    Free,
    Quarantined,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PooledCheckoutKind {
    Sibling,
    Managed,
    Explicit,
    UnsafeLegacy,
}

/// Durable registry entry for a reusable linked Git worktree.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PooledCheckoutRecord {
    pub path: std::path::PathBuf,
    pub source_repo: std::path::PathBuf,
    pub common_git_dir: std::path::PathBuf,
    pub slot: u32,
    pub kind: PooledCheckoutKind,
    pub worktree_git_dir: Option<std::path::PathBuf>,
    pub worktree_identity: Option<String>,
    pub state: PooledCheckoutState,
    pub session_id: Option<SessionId>,
    pub owner_incarnation_id: Option<String>,
    pub lease_id: Option<String>,
    pub branch: Option<String>,
    pub quarantine_reason: Option<String>,
}

/// Capability returned while a checkout is reserved for one session.
///
/// Mutating registry operations require both IDs so stale session cleanup
/// cannot release or quarantine a checkout that has since been re-leased.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PooledCheckoutLease {
    pub path: std::path::PathBuf,
    pub source_repo: std::path::PathBuf,
    pub common_git_dir: std::path::PathBuf,
    pub slot: u32,
    pub kind: PooledCheckoutKind,
    pub worktree_git_dir: Option<std::path::PathBuf>,
    pub worktree_identity: Option<String>,
    pub session_id: SessionId,
    pub owner_incarnation_id: String,
    pub lease_id: String,
    pub branch: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkerIncarnationState {
    Allocating,
    Active,
    Retained,
    CleanupClaimed,
    ReleaseClaimed,
    Released,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkerIncarnation {
    pub session_id: SessionId,
    pub incarnation_id: String,
    pub orchestrator_id: Option<OrchestratorId>,
    pub started_at: i64,
    pub source_workspace: String,
    pub workspace_path: String,
    /// Stable canonical Git identity shared by every checkout of one repository.
    pub repository_key: Option<String>,
    pub lease_id: Option<String>,
    pub checkout_backed: bool,
    pub state: WorkerIncarnationState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyWorkerRuntimeCapability {
    pub session_id: SessionId,
    pub incarnation_id: String,
    pub physical_tmux_name: String,
    pub pane_id: String,
    pub pane_pid: u32,
}

#[derive(Debug)]
pub struct WorkerRuntimeClaim {
    pub worker: WorkerIncarnation,
    pub claim_id: String,
}

/// An orchestrator's immutable runtime identity — the private tmux pane it
/// was first authorized from. `authorize_orchestrator` cross-checks every
/// subsequent orchestrator-facing CLI call against this so a spoofed
/// `NINOX_ORCHESTRATOR_ID` env var alone can't impersonate it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OrchestratorRuntimeIdentity {
    pub orchestrator_id: String,
    pub runtime_id: String,
    pub server_epoch: String,
    pub physical_tmux_name: String,
    pub pane_id: String,
    pub root_pid: u32,
    pub root_created_at: i64,
    pub registered_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkerFinalization {
    pub session_id: String,
    pub incarnation_id: String,
    pub orchestrator_id: String,
    pub claimed_at: i64,
    pub finalized_at: Option<i64>,
}

#[derive(Debug)]
pub enum WorkerFinalizationIntent {
    Apply(WorkerIncarnation),
    AlreadyFinalized(WorkerIncarnation),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkerCompletion {
    pub completion_id: String,
    pub session_id: String,
    pub incarnation_id: String,
    pub orchestrator_id: String,
    pub summary: String,
    pub completed_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerCompletionIntent {
    Completed(WorkerCompletion),
    AlreadyCompleted(WorkerCompletion),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerCompletionDelivery {
    pub completion: WorkerCompletion,
    pub attempt_id: String,
    pub attempt: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerCompletionReceipt {
    Delivered(WorkerCompletion),
    AlreadyAcknowledged,
}

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
    /// silently: only the poller's startup reconciliation assigns it,
    /// and only a user-triggered Resume action clears it.
    Interrupted,
}

impl SessionStatus {
    /// No live agent process behind this status: the session has finished,
    /// been killed, or lost its pane. The canonical definition — several
    /// places need "is this session still live?" and they must agree, since
    /// a row that ends up non-terminal without a live process is a permanent
    /// ghost (`sweep_retired_sessions` only purges `Done`/`Terminated`, and
    /// `poll_pids` needs a `pid`, which a CLI-spawned worker never has).
    ///
    /// Note this is broader than `events`' reap-local `is_finished`, which
    /// deliberately excludes the resumable `Interrupted`.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done | Self::Terminated | Self::Interrupted)
    }
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

/// Moment-to-moment agent activity, orthogonal to the PR-lifecycle
/// `SessionStatus`: a session can be `PrOpen` (lifecycle) while `Idle`
/// (activity). Written by the worker's own Claude Code hooks
/// (`UserPromptSubmit`/`Stop` → `ninox worker-status hook-*`) and by the
/// agent's explicit `ninox worker-status set`; see
/// `ninox_core::worker_status` for the transition rules.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ActivityState {
    Working,
    Idle,
    Blocked,
    /// No activity signal available: the session predates the status hooks,
    /// runs a harness without hook support, or hasn't reported yet. Distinct
    /// from `Idle` — "we don't know" vs "we know it's between turns".
    #[default]
    Unknown,
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
    /// Unix epoch milliseconds when merge detection first saw this
    /// session's PR merged while `[auto_reap]` was disabled — i.e. the
    /// session was deliberately left alive (tmux + worktree intact) for
    /// post-merge validation instead of being cleaned up on the spot.
    /// Once set, the merged notification/worker-done reaction never fire
    /// again and GitHub enrichment skips the session entirely (its PR is
    /// merged — there is nothing left to poll), even though its `status`
    /// stays live until it is reaped. With `[auto_reap]` enabled the
    /// session goes straight to `Done` instead and this stays `None`.
    /// `#[serde(default)]` for wire/DB back-compat.
    #[serde(default)]
    pub merged_at: Option<i64>,
    /// Structured CI/review/mergeable breakdown behind the current
    /// `status`. `None` until the first GitHub enrichment tick for a
    /// session with an open PR (`Spawning`/`Working` sessions have no PR
    /// yet, so no gate to report). `#[serde(default)]` for wire/DB
    /// back-compat with sessions recorded before this field existed.
    #[serde(default)]
    pub gate_status: Option<GateStatus>,
    /// See `ActivityState`. `#[serde(default)]` (→ `Unknown`) for wire/DB
    /// back-compat with sessions recorded before this field existed.
    #[serde(default)]
    pub activity: ActivityState,
    /// Free-text context for a self-reported state (`ninox worker-status set
    /// blocked --note "…"`). Cleared whenever `activity` changes without a
    /// new note.
    #[serde(default)]
    pub activity_note: Option<String>,
    /// Epoch ms the current `activity` value was first observed — reset on
    /// every state change, mirroring `GateStatus::since`. `None` until the
    /// first report.
    #[serde(default)]
    pub activity_since: Option<i64>,
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
    /// A merged-but-kept-alive worker just had `merged_at` stamped (see
    /// `Session::merged_at`) — so the in-memory copy learns the session is
    /// merged even though its live `status` is unchanged.
    pub const MERGED_AT:   Self = Self(1 << 9);
    /// `activity`, `activity_note`, `activity_since` — all sourced from the
    /// same `worker_status::apply_activity` write, so they travel together.
    pub const ACTIVITY:    Self = Self(1 << 10);
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
    /// True once this session's PR merge has been fully handled by the
    /// poller — either it reached `Done` (auto-reap cleaned it up) or it was
    /// kept alive for post-merge validation with `merged_at` stamped (see
    /// `merged_at`). Both mean GitHub enrichment has nothing left to do for
    /// it and merge detection must not fire again. The canonical predicate
    /// for the poller's "skip this session" checks, which must all agree.
    pub fn merge_handled(&self) -> bool {
        matches!(self.status, SessionStatus::Done) || self.merged_at.is_some()
    }

    /// Copy only the fields flagged in `fields` from `incoming` onto `self`.
    /// See `SessionFields`'s doc comment for why this must never be a
    /// wholesale replace except when `fields == SessionFields::ALL`.
    ///
    /// `name`, `summary`, `catalogue_path`, `claude_session_id`,
    /// `agent_type`, `started_at`, `orchestrator_id`, and `id` have no flag
    /// of their own — they're only ever set via the `ALL` path at
    /// spawn-completion. A new emitter that needs to update one of these
    /// must use `SessionFields::ALL` or add a new flag for it; reusing an
    /// existing flag would silently drop the update.
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
        if fields.contains(SessionFields::MERGED_AT) {
            self.merged_at = incoming.merged_at;
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
        if fields.contains(SessionFields::ACTIVITY) {
            self.activity = incoming.activity;
            self.activity_note = incoming.activity_note.clone();
            self.activity_since = incoming.activity_since;
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Orchestrator {
    pub id:         OrchestratorId,
    pub name:       String,
    pub created_at: i64,
}

/// An orchestrator's registered goals/plan markdown doc — a pointer
/// (`file_path`), not the content itself. The desktop app polls the file
/// on disk and re-reads it on mtime change; see
/// `docs/superpowers/specs/2026-08-26-orchestrator-plan-tracking-design.md`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OrchestratorPlan {
    pub orchestrator_id: String,
    pub file_path: String,
    pub registered_at: i64,
    pub updated_at: i64,
}

/// How a worker→worker dependency edge came to exist. Stored as its
/// `as_str()` form in the `session_deps` table.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum DepKind {
    /// Registered explicitly via `ninox worker-status depend <session>`.
    Declared,
    /// Inferred by the poller from PR branch stacking (this session's PR
    /// base ref is the dependency's PR head ref) — re-derived every GitHub
    /// tick, so it appears and disappears with the branch relationship.
    Stacked,
}

impl DepKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Declared => "declared",
            Self::Stacked  => "stacked",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "declared" => Some(Self::Declared),
            "stacked"  => Some(Self::Stacked),
            _          => None,
        }
    }
}

/// A worker→worker dependency edge: `session_id` depends on `depends_on`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionDep {
    pub session_id: SessionId,
    pub depends_on: SessionId,
    pub kind:       DepKind,
    pub note:       Option<String>,
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

/// An explicit PR watch registered via `ninox open --pr` — additive to the
/// implicit watching of session-attached PRs. `opener_session_id = None`
/// means the watch was registered outside any ninox session (state/UI
/// events only, no tmux delivery target). Auto-removed when the PR merges
/// or closes; otherwise lives until `ninox close --pr`.
#[derive(Debug, Clone, PartialEq)]
pub struct PrWatch {
    pub repo:              String,
    pub pr_number:         u64,
    pub pr_url:            String,
    pub opener_session_id: Option<String>,
    pub created_at:        i64,
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
    /// A worker checkout could not be safely allocated or restored.
    CheckoutUnavailable,
    /// A "restart all agents" batch (desktop or TUI) finished with every
    /// targeted session restarted successfully.
    RestartAllCompleted,
    /// A "restart all agents" batch finished with at least one session
    /// failing to restart.
    RestartAllFailed,
    /// A tracked PR's status-check rollup transitioned into all-passing +
    /// mergeable — the symmetric counterpart to `CiFailure`'s
    /// newly-failing transition. Applies to both a session's own PR and an
    /// explicit `ninox open --pr` watch.
    PrReadyToMerge,
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
    fn notification_kind_serde_covers_added_variants() {
        for (kind, wire) in [
            (NotificationKind::WorkRequested,  "\"work_requested\""),
            (NotificationKind::ExtraPr,        "\"extra_pr\""),
            (NotificationKind::WorkerRetired,  "\"worker_retired\""),
            (NotificationKind::UpdateAvailable, "\"update_available\""),
            (NotificationKind::UpdateInstalled, "\"update_installed\""),
            (NotificationKind::UpdateFailed,    "\"update_failed\""),
            (
                NotificationKind::CheckoutUnavailable,
                "\"checkout_unavailable\"",
            ),
            (NotificationKind::PrReadyToMerge,  "\"pr_ready_to_merge\""),
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
            gate_status: None, merged_at: None,
            activity: ActivityState::Unknown,
            activity_note: None, activity_since: None,
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

    #[test]
    fn activity_state_serde_round_trips_snake_case() {
        for (state, wire) in [
            (ActivityState::Working, "\"working\""),
            (ActivityState::Idle,    "\"idle\""),
            (ActivityState::Blocked, "\"blocked\""),
            (ActivityState::Unknown, "\"unknown\""),
        ] {
            assert_eq!(serde_json::to_string(&state).unwrap(), wire);
            let parsed: ActivityState = serde_json::from_str(wire).unwrap();
            assert_eq!(parsed, state);
        }
    }

    #[test]
    fn session_deserializes_without_activity_fields_for_back_compat() {
        // A row serialized before the activity fields existed must load with
        // Unknown / empty defaults rather than failing.
        let mut v = serde_json::to_value(base_session()).unwrap();
        let obj = v.as_object_mut().unwrap();
        obj.remove("activity");
        obj.remove("activity_note");
        obj.remove("activity_since");
        let s: Session = serde_json::from_value(v).expect("pre-activity payload must deserialize");
        assert_eq!(s.activity, ActivityState::Unknown);
        assert_eq!(s.activity_note, None);
        assert_eq!(s.activity_since, None);
    }

    #[test]
    fn merge_from_activity_copies_only_when_flagged() {
        let mut existing = base_session();
        let mut incoming = base_session();
        incoming.activity = ActivityState::Blocked;
        incoming.activity_note = Some("waiting on migration".into());
        incoming.activity_since = Some(42);

        existing.merge_from(&incoming, SessionFields::COST); // ACTIVITY not flagged
        assert_eq!(existing.activity, ActivityState::Unknown, "unflagged activity must not be copied");
        assert_eq!(existing.activity_note, None);
        assert_eq!(existing.activity_since, None);

        existing.merge_from(&incoming, SessionFields::ACTIVITY);
        assert_eq!(existing.activity, ActivityState::Blocked, "flagged activity must be copied");
        assert_eq!(existing.activity_note.as_deref(), Some("waiting on migration"));
        assert_eq!(existing.activity_since, Some(42));
    }

    #[test]
    fn dep_kind_serde_and_db_string_round_trip() {
        for (kind, wire) in [(DepKind::Declared, "declared"), (DepKind::Stacked, "stacked")] {
            assert_eq!(serde_json::to_string(&kind).unwrap(), format!("\"{wire}\""));
            assert_eq!(kind.as_str(), wire);
            assert_eq!(DepKind::parse(wire), Some(kind));
        }
        assert_eq!(DepKind::parse("garbage"), None);
    }
}
