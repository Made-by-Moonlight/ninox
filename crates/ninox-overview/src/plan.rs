//! A stub/hardcoded "plan review" data source for phase 1 — the real
//! auto-planning algorithm (spec item 9) is a separate, not-yet-implemented
//! piece. `demo_plan` stands in for it so ghost-node rendering and the
//! plan-review controls have something real to drive against.

use crate::node::{GhostNode, NodeId};

#[derive(Debug, Clone, PartialEq)]
pub struct GhostPlan {
    pub parent_id: NodeId,
    pub sections: Vec<GhostNode>,
}

/// Ids are namespaced by `parent_id` so demo plans for two different
/// orchestrators never collide if both are active at once.
pub fn demo_plan(parent_id: &str) -> GhostPlan {
    let research_id = format!("ghost:{parent_id}:research");
    let implement_id = format!("ghost:{parent_id}:implement");
    GhostPlan {
        parent_id: parent_id.to_string(),
        sections: vec![
            GhostNode {
                id: research_id.clone(),
                parent_id: parent_id.to_string(),
                worker_type: Some("Researcher".into()),
                brief: "research the rewrite".into(),
                dependencies: vec![],
            },
            GhostNode {
                id: implement_id,
                parent_id: parent_id.to_string(),
                worker_type: Some("Implementation".into()),
                brief: "implement the rewrite".into(),
                dependencies: vec![research_id],
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_plans_for_different_parents_never_collide() {
        let a = demo_plan("orch-a");
        let b = demo_plan("orch-b");
        let a_ids: Vec<&str> = a.sections.iter().map(|g| g.id.as_str()).collect();
        let b_ids: Vec<&str> = b.sections.iter().map(|g| g.id.as_str()).collect();
        assert!(a_ids.iter().all(|id| !b_ids.contains(id)));
    }
}
