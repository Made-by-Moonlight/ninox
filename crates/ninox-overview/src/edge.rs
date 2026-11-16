//! Two edge kinds: hierarchy (who-spawned-whom, stable) and communication
//! (transient, drawn for a message). A ghost node can never anchor a
//! communication edge — see [`OverviewEdge::new`].

use crate::node::{NodeId, OverviewNode};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum EdgeKind {
    Hierarchy,
    Communication,
}

// Fields are private so the ghost invariant below can only be bypassed by
// code in this module — a struct literal elsewhere would skip `new`'s check
// entirely. Construct via `new`; read via `kind`/`from`/`to`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OverviewEdge {
    kind: EdgeKind,
    from: NodeId,
    to: NodeId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhostEdgeError {
    pub ghost_id: NodeId,
}

impl OverviewEdge {
    /// Rejects a `Communication` edge when either endpoint is a ghost node
    /// (looked up in `nodes`) — the spec's invariant that a proposed,
    /// not-yet-running node can never be a communication-edge endpoint.
    /// An endpoint id absent from `nodes` is treated as non-ghost (callers
    /// that already filtered dangling ids, e.g. the snapshot adapter, are
    /// the ones responsible for that check).
    pub fn new(kind: EdgeKind, from: NodeId, to: NodeId, nodes: &[OverviewNode]) -> Result<Self, GhostEdgeError> {
        if kind == EdgeKind::Communication {
            for id in [&from, &to] {
                if nodes.iter().any(|n| n.id() == id && n.is_ghost()) {
                    return Err(GhostEdgeError { ghost_id: id.clone() });
                }
            }
        }
        Ok(Self { kind, from, to })
    }

    pub fn kind(&self) -> EdgeKind {
        self.kind
    }

    pub fn from(&self) -> &str {
        &self.from
    }

    pub fn to(&self) -> &str {
        &self.to
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::{GhostNode, NodeKind, RealNode};

    fn real(id: &str) -> OverviewNode {
        OverviewNode::Real(RealNode {
            id: id.into(),
            kind: NodeKind::Worker,
            name: id.into(),
            harness_badge: "claude-code".into(),
            worker_type_badge: None,
            status: "working".into(),
            brief: None,
            depth: 0,
            breadcrumb: vec![],
            activity: None,
            artifacts: vec![],
            remote_badge: None,
            depth_capped: false,
        })
    }

    fn ghost(id: &str, parent: &str) -> OverviewNode {
        OverviewNode::Ghost(GhostNode {
            id: id.into(),
            parent_id: parent.into(),
            worker_type: None,
            brief: "proposed work".into(),
            dependencies: vec![],
        })
    }

    #[test]
    fn ghost_node_cannot_be_a_communication_edge_source() {
        let nodes = [ghost("g1", "o"), real("r1")];
        let err = OverviewEdge::new(EdgeKind::Communication, "g1".into(), "r1".into(), &nodes).unwrap_err();
        assert_eq!(err.ghost_id, "g1");
    }

    #[test]
    fn ghost_node_cannot_be_a_communication_edge_target() {
        let nodes = [real("r1"), ghost("g1", "o")];
        let err = OverviewEdge::new(EdgeKind::Communication, "r1".into(), "g1".into(), &nodes).unwrap_err();
        assert_eq!(err.ghost_id, "g1");
    }

    #[test]
    fn hierarchy_edge_to_a_ghost_is_allowed() {
        let nodes = [real("o"), ghost("g1", "o")];
        assert!(OverviewEdge::new(EdgeKind::Hierarchy, "o".into(), "g1".into(), &nodes).is_ok());
    }

    #[test]
    fn real_to_real_communication_edge_is_allowed() {
        let nodes = [real("r1"), real("r2")];
        assert!(OverviewEdge::new(EdgeKind::Communication, "r1".into(), "r2".into(), &nodes).is_ok());
    }
}
