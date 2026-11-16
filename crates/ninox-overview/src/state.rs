//! Aggregate state for the fleet-overview screen: nodes/edges, the
//! computed tree, the current selection, and (phase 1) a single active
//! demo ghost plan. One core, consumed identically by any renderer.

use crate::controls::{controls_for, ControlAction};
use crate::edge::{EdgeKind, OverviewEdge};
use crate::node::{GhostNode, NodeId, OverviewNode};
use crate::plan::{demo_plan, GhostPlan};
use crate::tree::OverviewTree;
use ninox_core::fleet::snapshot::FleetSnapshot;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct OverviewState {
    pub nodes: Vec<OverviewNode>,
    pub edges: Vec<OverviewEdge>,
    pub tree: OverviewTree,
    pub selected: Option<NodeId>,
    pub active_plan: Option<GhostPlan>,
}

impl OverviewState {
    pub fn new(nodes: Vec<OverviewNode>, edges: Vec<OverviewEdge>) -> Self {
        let tree = OverviewTree::build(&nodes, &edges);
        Self { nodes, edges, tree, selected: None, active_plan: None }
    }

    pub fn from_snapshot(snap: &FleetSnapshot) -> Self {
        let (nodes, edges) = crate::snapshot_adapter::from_snapshot(snap);
        Self::new(nodes, edges)
    }

    pub fn select(&mut self, id: impl Into<NodeId>) {
        self.selected = Some(id.into());
    }

    pub fn selected_node(&self) -> Option<&OverviewNode> {
        let id = self.selected.as_deref()?;
        self.nodes.iter().find(|n| n.id() == id)
    }

    pub fn controls(&self) -> Vec<ControlAction> {
        self.selected_node().map(controls_for).unwrap_or_default()
    }

    /// Adds `demo_plan(orchestrator_id)`'s ghost nodes/edges if no plan is
    /// active for that parent, or removes the active one if it is —
    /// idempotent toggle. Only one plan is ever active at a time: starting
    /// a plan for a different orchestrator first removes the existing
    /// one's ghosts, rather than leaving them orphaned in `nodes`/`edges`
    /// with nothing left tracking them.
    pub fn toggle_demo_plan(&mut self, orchestrator_id: &str) {
        let switching_away = self.active_plan.as_ref().is_some_and(|p| p.parent_id != orchestrator_id);
        let turning_off = self.active_plan.as_ref().is_some_and(|p| p.parent_id == orchestrator_id);
        if switching_away || turning_off {
            self.remove_active_plan();
            if turning_off {
                return;
            }
        }
        self.insert_plan(demo_plan(orchestrator_id));
    }

    /// Removes the active plan's ghost nodes/edges, if any. Clears
    /// `selected` if it pointed at a ghost the removal just dropped.
    fn remove_active_plan(&mut self) {
        let Some(plan) = self.active_plan.take() else { return };
        let ghost_ids: Vec<NodeId> = plan.sections.iter().map(|g| g.id.clone()).collect();
        self.nodes.retain(|n| !ghost_ids.contains(&n.id().to_string()));
        self.edges.retain(|e| !ghost_ids.iter().any(|id| id == e.to() || id == e.from()));
        if self.selected.as_ref().is_some_and(|id| ghost_ids.contains(id)) {
            self.selected = None;
        }
        self.tree = OverviewTree::build(&self.nodes, &self.edges);
    }

    /// Re-inserts a previously-active plan's ghost nodes/edges — e.g. after
    /// a periodic rebuild of `nodes`/`edges` from fresh fleet data replaced
    /// `self` with a ghost-free `OverviewState` (see `ninox-tui`'s
    /// `GraphState::sync_fleet`). Unlike `toggle_demo_plan`, this takes the
    /// caller's exact plan rather than regenerating it from `demo_plan`, so
    /// a section added via `add_plan_section` survives the rebuild too.
    pub fn restore_plan(&mut self, plan: GhostPlan) {
        self.insert_plan(plan);
    }

    fn insert_plan(&mut self, plan: GhostPlan) {
        for section in &plan.sections {
            self.nodes.push(OverviewNode::Ghost(section.clone()));
        }
        for section in &plan.sections {
            if let Ok(edge) = OverviewEdge::new(EdgeKind::Hierarchy, section.parent_id.clone(), section.id.clone(), &self.nodes) {
                self.edges.push(edge);
            }
        }
        self.active_plan = Some(plan);
        self.tree = OverviewTree::build(&self.nodes, &self.edges);
    }

    /// Appends one more hardcoded ghost section to the active plan ("Add
    /// section" control) — phase 1 has no real planner to draft one from,
    /// so this is a fixed placeholder brief the user can edit afterward.
    pub fn add_plan_section(&mut self) {
        let Some(plan) = &mut self.active_plan else { return };
        let n = plan.sections.len();
        let section = GhostNode {
            id: format!("ghost:{}:section-{n}", plan.parent_id),
            parent_id: plan.parent_id.clone(),
            worker_type: None,
            brief: "new section".into(),
            dependencies: vec![],
        };
        plan.sections.push(section.clone());
        self.nodes.push(OverviewNode::Ghost(section.clone()));
        if let Ok(edge) = OverviewEdge::new(EdgeKind::Hierarchy, section.parent_id.clone(), section.id.clone(), &self.nodes) {
            self.edges.push(edge);
        }
        self.tree = OverviewTree::build(&self.nodes, &self.edges);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::{NodeKind, RealNode};

    fn orchestrator(id: &str) -> OverviewNode {
        OverviewNode::Real(RealNode {
            id: id.into(),
            kind: NodeKind::Orchestrator,
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

    #[test]
    fn toggle_demo_plan_is_idempotent_add_then_remove() {
        let mut st = OverviewState::new(vec![orchestrator("o")], vec![]);

        st.toggle_demo_plan("o");
        assert_eq!(st.nodes.len(), 3, "orchestrator + 2 demo ghost sections");
        assert!(st.active_plan.is_some());

        st.toggle_demo_plan("o");
        assert_eq!(st.nodes.len(), 1, "back to just the orchestrator");
        assert!(st.active_plan.is_none());
    }

    #[test]
    fn starting_a_plan_on_a_different_orchestrator_removes_the_first_plans_ghosts() {
        let mut st = OverviewState::new(vec![orchestrator("a"), orchestrator("b")], vec![]);

        st.toggle_demo_plan("a");
        assert_eq!(st.nodes.len(), 4, "a + b + 2 demo ghost sections for a");

        st.toggle_demo_plan("b");

        assert_eq!(st.nodes.len(), 4, "a + b + 2 demo ghost sections for b — a's orphaned, not left over");
        assert!(st.nodes.iter().all(|n| !n.id().starts_with("ghost:a:")), "a's ghosts must be gone: {:?}", st.nodes);
        assert_eq!(st.active_plan.as_ref().map(|p| p.parent_id.as_str()), Some("b"));
    }

    #[test]
    fn selecting_a_discarded_ghost_clears_selection() {
        let mut st = OverviewState::new(vec![orchestrator("o")], vec![]);
        st.toggle_demo_plan("o");
        let ghost_id = st.active_plan.as_ref().unwrap().sections[0].id.clone();
        st.select(ghost_id);
        assert!(st.selected_node().is_some());

        st.toggle_demo_plan("o");

        assert!(st.selected.is_none());
        assert!(st.selected_node().is_none());
    }

    #[test]
    fn restore_plan_reinserts_a_section_added_after_the_demo_plan() {
        let mut original = OverviewState::new(vec![orchestrator("o")], vec![]);
        original.toggle_demo_plan("o");
        original.add_plan_section();
        let plan = original.active_plan.clone().unwrap();
        assert_eq!(plan.sections.len(), 3, "2 demo sections + 1 added");

        // Simulate a periodic rebuild that dropped ghosts, then restore.
        let mut rebuilt = OverviewState::new(vec![orchestrator("o")], vec![]);
        rebuilt.restore_plan(plan.clone());

        assert_eq!(rebuilt.nodes.len(), 4, "orchestrator + 3 sections");
        assert_eq!(rebuilt.active_plan, Some(plan));
    }
}
