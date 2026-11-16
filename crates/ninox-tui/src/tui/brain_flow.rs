//! The Brain tab's Neighbourhood graph (`Tab` toggles into it from the
//! List view): the focus entry, its 1-hop links, and up to 15 capped
//! hop-2 links (`ninox_core::brain::neighbourhood_capped`), laid out as
//! three rataflow columns — focus, hop-1, hop-2 — rather than the GUI's
//! pixel-space force simulation, which a terminal grid has no use for.
//! Reuses the opened-island card tree's rataflow plumbing
//! (`island_flow::carry_over_viewport_and_selection`) for the same reason
//! that module already depends on it: a real node/edge graph to lay out
//! and navigate, not the plain `Paragraph` boxes the unopened islands grid
//! uses.

use std::collections::HashSet;

use ratatui::buffer::Buffer;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};
use rataflow::{Edge, Flow, Node, NodeContent, NodeRenderContext, StepEdge};

use ninox_core::brain::{display_title, kind_label, neighbourhood, neighbourhood_capped, normalize_kind};
use ninox_core::BrainEntry;

/// Node id for the "+N more" row — never a real brain entry id (those are
/// relative markdown paths, which never start with a null byte), so a
/// lookup by this id can't collide with an actual entry.
pub const MORE_NODE_ID: &str = "\0more";

const NODE_W: f64 = 26.0;
/// 2 border rows + 3 content rows (title, a blank spacer, kind) — the
/// spacer keeps rataflow's vertically-centered connection handle off the
/// kind-label text; at `NODE_H = 4` the handle dot landed right on top of
/// it (confirmed live, not just in theory — ratatui's own render tests
/// can't see a real rataflow canvas's handle overlay).
const NODE_H: f64 = 5.0;
const COL_GAP: f64 = 6.0;
const ROW_GAP: f64 = 1.0;

#[derive(Debug, Clone)]
pub struct EntryNode {
    pub title: String,
    pub kind: String,
    pub is_focus: bool,
}

impl NodeContent for EntryNode {
    fn render(&self, ctx: &NodeRenderContext, buf: &mut Buffer) {
        let border_color = if ctx.selected {
            ratatui::style::Color::Yellow
        } else if self.is_focus {
            ratatui::style::Color::Cyan
        } else {
            ratatui::style::Color::DarkGray
        };
        let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(border_color));
        let title_style = Style::default().add_modifier(if self.is_focus { Modifier::BOLD } else { Modifier::empty() });
        let lines = vec![
            Line::from(Span::styled(self.title.clone(), title_style)),
            Line::from(""),
            Line::from(Span::styled(self.kind.clone(), Style::default().fg(ratatui::style::Color::DarkGray))),
        ];
        Paragraph::new(lines).block(block).render(ctx.area, buf);
    }
}

#[derive(Debug, Clone)]
pub struct MoreNode {
    pub count: usize,
}

impl NodeContent for MoreNode {
    fn render(&self, ctx: &NodeRenderContext, buf: &mut Buffer) {
        let color = if ctx.selected { ratatui::style::Color::Yellow } else { ratatui::style::Color::DarkGray };
        let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(color));
        Paragraph::new(Span::styled(format!("▸ +{} more", self.count), Style::default().fg(color))).block(block).render(ctx.area, buf);
    }
}

#[derive(Debug, Clone)]
pub enum BrainGraphNode {
    Entry(EntryNode),
    More(MoreNode),
}

impl NodeContent for BrainGraphNode {
    fn render(&self, ctx: &NodeRenderContext, buf: &mut Buffer) {
        match self {
            Self::Entry(e) => e.render(ctx, buf),
            Self::More(m) => m.render(ctx, buf),
        }
    }
}

/// Builds the Neighbourhood graph centred on `entries[focus]`: the focus
/// node, every hop-1 link, and `neighbourhood_capped`'s hop-2 survivors
/// (plus a "+N more" node when any were dropped) — three columns, edges
/// only between nodes actually present (both within the capped set).
/// `None` when `focus` is out of range.
pub fn build_neighbourhood_flow(entries: &[BrainEntry], edges: &[(usize, usize)], focus: usize) -> Option<Flow<BrainGraphNode, StepEdge>> {
    let focus_entry = entries.get(focus)?;
    let (ids, dropped) = neighbourhood_capped(focus, edges, 2);
    let hop1: HashSet<usize> = neighbourhood(focus, edges, 1);
    let mut hop1_ids: Vec<usize> = ids.iter().copied().filter(|&id| id != focus && hop1.contains(&id)).collect();
    hop1_ids.sort_unstable();
    let mut hop2_ids: Vec<usize> = ids.iter().copied().filter(|id| !hop1.contains(id)).collect();
    hop2_ids.sort_unstable();

    let node_for = |idx: usize, is_focus: bool| -> BrainGraphNode {
        let e = &entries[idx];
        BrainGraphNode::Entry(EntryNode { title: display_title(e), kind: kind_label(&normalize_kind(&e.entry_type)), is_focus })
    };

    let mut nodes: Vec<Node<BrainGraphNode>> = Vec::new();
    let focus_y = column_center_y(hop1_ids.len().max(1));
    nodes.push(Node::new(focus_entry.id.clone(), (0.0, focus_y), (NODE_W, NODE_H), node_for(focus, true)).with_draggable(false));

    let col1_x = NODE_W + COL_GAP;
    for (i, &idx) in hop1_ids.iter().enumerate() {
        let y = i as f64 * (NODE_H + ROW_GAP);
        nodes.push(Node::new(entries[idx].id.clone(), (col1_x, y), (NODE_W, NODE_H), node_for(idx, false)).with_draggable(false));
    }

    let col2_x = 2.0 * (NODE_W + COL_GAP);
    for (i, &idx) in hop2_ids.iter().enumerate() {
        let y = i as f64 * (NODE_H + ROW_GAP);
        nodes.push(Node::new(entries[idx].id.clone(), (col2_x, y), (NODE_W, NODE_H), node_for(idx, false)).with_draggable(false));
    }
    if dropped > 0 {
        let y = hop2_ids.len() as f64 * (NODE_H + ROW_GAP);
        nodes.push(Node::new(MORE_NODE_ID.to_string(), (col2_x, y), (NODE_W, NODE_H), BrainGraphNode::More(MoreNode { count: dropped })).with_draggable(false));
    }

    let present: HashSet<usize> = ids.iter().copied().chain(std::iter::once(focus)).collect();
    let edges: Vec<Edge<StepEdge>> = edges
        .iter()
        .filter(|&&(a, b)| present.contains(&a) && present.contains(&b))
        .map(|&(a, b)| Edge::new(format!("{a}-{b}"), entries[a].id.clone(), entries[b].id.clone()))
        .collect();

    Flow::with_graph(nodes, edges).ok().map(|f| f.with_min_zoom(1.0).with_max_zoom(1.0))
}

/// Vertically centers a single node against a column of `rows` entries of
/// the same height — used to center the focus node against its hop-1
/// column, the closest a terminal column layout gets to the brief's "ring".
fn column_center_y(rows: usize) -> f64 {
    let total_h = rows as f64 * NODE_H + (rows.saturating_sub(1)) as f64 * ROW_GAP;
    (total_h - NODE_H) / 2.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str) -> BrainEntry {
        BrainEntry { id: id.into(), entry_type: "concepts".into(), name: id.into(), tags: vec![], repos: vec![], updated: None, body: String::new(), source: String::new() }
    }

    #[test]
    fn build_neighbourhood_flow_includes_the_focus_and_its_hop1_links() {
        let entries = vec![entry("a.md"), entry("b.md"), entry("c.md")];
        let edges = [(0, 1), (1, 2)];
        let flow = build_neighbourhood_flow(&entries, &edges, 0).unwrap();
        assert!(flow.node("a.md").is_some());
        assert!(flow.node("b.md").is_some());
        assert!(flow.node("c.md").is_some(), "hop-2 from a.md via b.md");
    }

    #[test]
    fn build_neighbourhood_flow_marks_only_the_focus_node() {
        let entries = vec![entry("a.md"), entry("b.md")];
        let edges = [(0, 1)];
        let flow = build_neighbourhood_flow(&entries, &edges, 0).unwrap();
        let BrainGraphNode::Entry(focus) = &flow.node("a.md").unwrap().content else { panic!() };
        let BrainGraphNode::Entry(other) = &flow.node("b.md").unwrap().content else { panic!() };
        assert!(focus.is_focus);
        assert!(!other.is_focus);
    }

    #[test]
    fn build_neighbourhood_flow_adds_a_more_node_when_hop2_is_capped() {
        let mut edges: Vec<(usize, usize)> = (1..=60).map(|i| (0, i)).collect();
        for i in 1..=60 {
            edges.push((i, 100 + i));
        }
        let entries: Vec<BrainEntry> = (0..201).map(|i| entry(&format!("e{i}.md"))).collect();
        let flow = build_neighbourhood_flow(&entries, &edges, 0).unwrap();
        let BrainGraphNode::More(more) = &flow.node(MORE_NODE_ID).unwrap().content else { panic!() };
        assert_eq!(more.count, 60 - 15, "every hop-1 kept, only the top 15 hop-2 nodes kept");
    }

    #[test]
    fn build_neighbourhood_flow_omits_the_more_node_below_the_cap() {
        let entries = vec![entry("a.md"), entry("b.md")];
        let edges = [(0, 1)];
        let flow = build_neighbourhood_flow(&entries, &edges, 0).unwrap();
        assert!(flow.node(MORE_NODE_ID).is_none());
    }

    #[test]
    fn build_neighbourhood_flow_only_edges_nodes_actually_present() {
        // b links to c (hop-2 via a straight chain would need a-b-c-d, but
        // with the dense cap unreachable here) — this just pins that an
        // edge whose far endpoint got dropped entirely doesn't appear.
        let entries = vec![entry("a.md"), entry("b.md"), entry("far.md")];
        let edges = [(0, 1), (1, 2)];
        let flow = build_neighbourhood_flow(&entries, &edges, 0).unwrap();
        // hop 2 from a.md (hops capped at 2) includes far.md via b.md.
        assert!(flow.node("far.md").is_some());
        assert_eq!(flow.edges().len(), 2, "a-b and b-far, both endpoints present");
    }

    #[test]
    fn build_neighbourhood_flow_locks_zoom_at_one() {
        let entries = vec![entry("a.md")];
        let flow = build_neighbourhood_flow(&entries, &[], 0).unwrap();
        assert_eq!(flow.viewport.zoom, 1.0);
    }

    #[test]
    fn build_neighbourhood_flow_none_for_an_out_of_range_focus() {
        let entries = vec![entry("a.md")];
        assert!(build_neighbourhood_flow(&entries, &[], 5).is_none());
    }
}
