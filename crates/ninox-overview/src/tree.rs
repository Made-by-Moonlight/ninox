//! Depth and root-to-node breadcrumb, walked from `Hierarchy` edges. A node
//! with no incoming hierarchy edge is a root (depth 0) — this covers both a
//! real root orchestrator and a `Role::Standalone` session with no parent.

use crate::edge::{EdgeKind, OverviewEdge};
use crate::node::{NodeId, OverviewNode};
use std::collections::HashMap;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct OverviewTree {
    pub depth: HashMap<NodeId, u32>,
    pub breadcrumb: HashMap<NodeId, Vec<String>>,
    /// Id of the root ancestor for every node (a root's own entry maps to
    /// itself) — the island model's grouping key (see `crate::islands`).
    pub roots: HashMap<NodeId, NodeId>,
}

impl OverviewTree {
    pub fn build(nodes: &[OverviewNode], edges: &[OverviewEdge]) -> Self {
        let parent_of: HashMap<&str, &str> = edges
            .iter()
            .filter(|e| e.kind() == EdgeKind::Hierarchy)
            .map(|e| (e.to(), e.from()))
            .collect();
        let name_of: HashMap<&str, &str> = nodes
            .iter()
            .map(|n| {
                let name = match n {
                    OverviewNode::Real(r) => r.name.as_str(),
                    OverviewNode::Ghost(g) => g.brief.as_str(),
                };
                (n.id(), name)
            })
            .collect();

        let mut tree = Self::default();
        for node in nodes {
            let mut chain = vec![node.id()];
            let mut cur = node.id();
            // Hierarchy edges form a tree in practice; a guard against a
            // malformed cycle keeps this from looping forever.
            while let Some(parent) = parent_of.get(cur) {
                if chain.contains(parent) {
                    break;
                }
                chain.push(parent);
                cur = parent;
            }
            chain.reverse();
            tree.roots.insert(node.id().to_string(), chain[0].to_string());
            let breadcrumb: Vec<String> = chain.iter().map(|id| name_of.get(id).copied().unwrap_or(*id).to_string()).collect();
            let depth = (chain.len() - 1) as u32;
            tree.depth.insert(node.id().to_string(), depth);
            tree.breadcrumb.insert(node.id().to_string(), breadcrumb);
        }
        tree
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::{NodeKind, RealNode};

    fn worker(id: &str) -> OverviewNode {
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

    fn hierarchy(from: &str, to: &str) -> OverviewEdge {
        OverviewEdge::new(EdgeKind::Hierarchy, from.into(), to.into(), &[]).unwrap()
    }

    #[test]
    fn a_depth_two_chain_gets_the_full_breadcrumb() {
        let nodes = [worker("root"), worker("mid"), worker("leaf")];
        let edges = [hierarchy("root", "mid"), hierarchy("mid", "leaf")];
        let tree = OverviewTree::build(&nodes, &edges);
        assert_eq!(tree.depth["leaf"], 2);
        assert_eq!(tree.breadcrumb["leaf"], vec!["root", "mid", "leaf"]);
    }

    #[test]
    fn a_standalone_node_with_no_incoming_hierarchy_edge_is_depth_zero() {
        let nodes = [worker("standalone")];
        let tree = OverviewTree::build(&nodes, &[]);
        assert_eq!(tree.depth["standalone"], 0);
        assert_eq!(tree.breadcrumb["standalone"], vec!["standalone"]);
    }

    #[test]
    fn a_depth_two_chains_root_is_the_top_ancestor_for_every_node_in_it() {
        let nodes = [worker("root"), worker("mid"), worker("leaf")];
        let edges = [hierarchy("root", "mid"), hierarchy("mid", "leaf")];
        let tree = OverviewTree::build(&nodes, &edges);
        assert_eq!(tree.roots["root"], "root");
        assert_eq!(tree.roots["mid"], "root");
        assert_eq!(tree.roots["leaf"], "root");
    }

    #[test]
    fn a_standalone_nodes_root_is_itself() {
        let nodes = [worker("standalone")];
        let tree = OverviewTree::build(&nodes, &[]);
        assert_eq!(tree.roots["standalone"], "standalone");
    }
}
