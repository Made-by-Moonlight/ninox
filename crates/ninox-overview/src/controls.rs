//! The bottom context-controls bar, computed once from the selected
//! node's type/state — see the spec's "context-sensitive controls"
//! section. One list, two renderings (TUI footer hints, GUI button row).

use crate::node::OverviewNode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlAction {
    ViewSubTree,
    Connect,
    Message,
    ApproveSpawnAll,
    ApproveSelectedOnly,
    AddSection,
    DiscardPlan,
}

pub fn controls_for(node: &OverviewNode) -> Vec<ControlAction> {
    use ControlAction::*;
    match node {
        OverviewNode::Ghost(_) => vec![ApproveSpawnAll, ApproveSelectedOnly, AddSection, DiscardPlan],
        OverviewNode::Real(n) => match n.kind {
            crate::node::NodeKind::Orchestrator => vec![ViewSubTree, Connect, Message],
            crate::node::NodeKind::Worker => vec![Connect, Message],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::{GhostNode, NodeKind, RealNode};

    fn real(kind: NodeKind) -> OverviewNode {
        OverviewNode::Real(RealNode {
            id: "n".into(),
            kind,
            name: "n".into(),
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

    fn ghost() -> OverviewNode {
        OverviewNode::Ghost(GhostNode {
            id: "g".into(),
            parent_id: "o".into(),
            worker_type: None,
            brief: "proposed".into(),
            dependencies: vec![],
        })
    }

    #[test]
    fn a_plain_worker_has_no_subtree_or_plan_review_actions() {
        let actions = controls_for(&real(NodeKind::Worker));
        assert_eq!(actions, vec![ControlAction::Connect, ControlAction::Message]);
    }

    #[test]
    fn an_orchestrator_adds_view_subtree_but_no_plan_review_actions() {
        let actions = controls_for(&real(NodeKind::Orchestrator));
        assert_eq!(actions, vec![ControlAction::ViewSubTree, ControlAction::Connect, ControlAction::Message]);
        assert!(!actions.contains(&ControlAction::DiscardPlan));
    }

    #[test]
    fn a_ghost_node_shows_only_plan_review_actions() {
        let actions = controls_for(&ghost());
        assert_eq!(
            actions,
            vec![
                ControlAction::ApproveSpawnAll,
                ControlAction::ApproveSelectedOnly,
                ControlAction::AddSection,
                ControlAction::DiscardPlan,
            ]
        );
    }
}
