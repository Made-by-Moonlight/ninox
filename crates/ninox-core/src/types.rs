use serde::{Deserialize, Serialize};

pub type SessionId      = String;
pub type OrchestratorId = String;
pub type PrId           = i64;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Spawning, Working, PrOpen, CiFailed,
    ReviewPending, Mergeable, Done, Terminated,
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
}
