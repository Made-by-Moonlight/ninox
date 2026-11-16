//! Builds the shared data model from `ninox_core::fleet::snapshot::
//! FleetSnapshot` rather than re-deriving fleet state — the snapshot is
//! already the pass that reads sessions, PR/CI and inbox state from the
//! store.

use crate::edge::{EdgeKind, OverviewEdge};
use crate::node::{NodeKind, OverviewNode, RealNode};
use ninox_core::fleet::snapshot::{FleetMember, FleetSnapshot, Role};

pub fn from_snapshot(snap: &FleetSnapshot) -> (Vec<OverviewNode>, Vec<OverviewEdge>) {
    let nodes: Vec<OverviewNode> = snap.members.iter().map(real_node).collect();
    let known_ids: std::collections::HashSet<&str> = snap.members.iter().map(|m| m.id()).collect();
    let mut edges = Vec::new();
    for m in &snap.members {
        if let Some(parent) = &m.session.orchestrator_id {
            if known_ids.contains(parent.as_str()) {
                if let Ok(edge) = OverviewEdge::new(EdgeKind::Hierarchy, parent.clone(), m.id().to_string(), &nodes) {
                    edges.push(edge);
                }
            }
        }
    }
    (nodes, edges)
}

fn real_node(m: &FleetMember) -> OverviewNode {
    let kind = match m.role {
        Role::Orchestrator => NodeKind::Orchestrator,
        Role::Worker | Role::Standalone => NodeKind::Worker,
    };
    OverviewNode::Real(RealNode {
        id: m.id().to_string(),
        kind,
        name: m.session.name.clone(),
        harness_badge: m.session.agent_type.clone(),
        worker_type_badge: m.session.worker_type.clone(),
        status: format!("{:?}", m.session.status),
        brief: m.record.task_brief.clone(),
        depth: 0,
        breadcrumb: vec![],
        activity: m.session.activity_note.clone(),
        artifacts: Vec::new(),
        remote_badge: None,
        depth_capped: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninox_core::fleet::probe::WorkspaceObservation;
    use ninox_core::store::FleetRecord;
    use ninox_core::types::{ActivityState, Session, SessionStatus};

    // `fleet::snapshot::test_support` is `pub(crate)` to ninox-core, so this
    // crate's tests build fixtures the same way it does, from the same
    // public fields.
    fn session(id: &str, orch: Option<&str>) -> Session {
        Session {
            id: id.into(), orchestrator_id: orch.map(str::to_string), name: format!("{id}-name"),
            repo: "o/r".into(), status: SessionStatus::Working, agent_type: "claude-code".into(), cost_usd: 0.0,
            started_at: 0, pr_number: None, pr_id: None,
            workspace_path: Some(format!("/ws/{id}")), pid: None, model: None,
            context_tokens: None, brain_reads: Vec::new(), brain_write: None, context_used_pct: None,
            context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: ActivityState::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        }
    }

    fn member(session: Session, role: Role) -> FleetMember {
        FleetMember {
            session, role, record: FleetRecord::default(), pr: None, ci: None,
            pending_inbox: 0, can_resume: true,
            workspace: WorkspaceObservation { exists: true, is_git: true, branch: None, dirty: Some(false) },
            anomalies: Vec::new(),
        }
    }

    #[test]
    fn builds_a_real_node_and_hierarchy_edge_per_member() {
        let o = member(session("o", None), Role::Orchestrator);
        let w = member(session("w", Some("o")), Role::Worker);
        let snap = FleetSnapshot::assemble(vec![o, w], vec![], vec![], 0);

        let (nodes, edges) = from_snapshot(&snap);

        assert_eq!(nodes.len(), 2);
        assert!(nodes.iter().any(|n| n.id() == "o" && !n.is_ghost()));
        assert_eq!(edges.len(), 1);
        assert_eq!((edges[0].kind(), edges[0].from(), edges[0].to()), (EdgeKind::Hierarchy, "o", "w"));
    }

    #[test]
    fn a_sessions_worker_type_becomes_the_nodes_worker_type_badge() {
        let mut s = session("w", None);
        s.worker_type = Some("Researcher".into());
        let (nodes, _) = from_snapshot(&FleetSnapshot::assemble(vec![member(s, Role::Worker)], vec![], vec![], 0));
        let OverviewNode::Real(n) = &nodes[0] else { panic!("expected a real node") };
        assert_eq!(n.worker_type_badge.as_deref(), Some("Researcher"));
    }

    #[test]
    fn skips_a_hierarchy_edge_to_an_orchestrator_missing_from_the_snapshot() {
        // Worker reports to "gone", which isn't in this snapshot (a torn-down
        // orchestrator) — must not panic, and still yields the worker node.
        let w = member(session("w", Some("gone")), Role::Worker);
        let snap = FleetSnapshot::assemble(vec![w], vec![], vec![], 0);

        let (nodes, edges) = from_snapshot(&snap);

        assert_eq!(nodes.len(), 1);
        assert!(edges.is_empty());
    }
}
