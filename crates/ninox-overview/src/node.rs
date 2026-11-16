//! Node content shared by both the TUI and (eventually) GUI renderers.
//! A ghost node is a distinct variant, not a boolean flag, so the type
//! system enforces the spec's invariant: a ghost has no running agent
//! behind it and can never be a communication-edge endpoint (see
//! [`crate::edge::OverviewEdge::new`]).

use serde::Serialize;

pub type NodeId = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum NodeKind {
    Worker,
    Orchestrator,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum RemoteBadge {
    Connected,
    Reconnecting,
    Unreachable,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RealNode {
    pub id: NodeId,
    pub kind: NodeKind,
    pub name: String,
    pub harness_badge: String,
    /// `None` until the worker-types backend ships — see the plan's
    /// citation spot-check.
    pub worker_type_badge: Option<String>,
    pub status: String,
    pub brief: Option<String>,
    pub depth: u32,
    pub breadcrumb: Vec<String>,
    pub activity: Option<String>,
    pub artifacts: Vec<String>,
    pub remote_badge: Option<RemoteBadge>,
    pub depth_capped: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GhostNode {
    pub id: NodeId,
    pub parent_id: NodeId,
    pub worker_type: Option<String>,
    pub brief: String,
    pub dependencies: Vec<NodeId>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum OverviewNode {
    Real(RealNode),
    Ghost(GhostNode),
}

impl OverviewNode {
    pub fn id(&self) -> &str {
        match self {
            Self::Real(n) => &n.id,
            Self::Ghost(n) => &n.id,
        }
    }

    pub fn is_ghost(&self) -> bool {
        matches!(self, Self::Ghost(_))
    }
}
