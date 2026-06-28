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
}
