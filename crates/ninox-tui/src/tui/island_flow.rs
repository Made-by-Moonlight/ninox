//! The opened island's rataflow-rendered card tree — reuses the fleet
//! canvas's rataflow plumbing (`Flow`/`Node`/`StepEdge`, the crate's own
//! spatial key/mouse handling) that the old whole-fleet force-directed
//! canvas (#230) used, scoped to just one orchestrator's own island instead
//! of the whole fleet. Positions are computed by hand (top-down, wrapped
//! into rows that fit the available width) rather than via `Sugiyama`:
//! the design calls for cards that stay readable at a fixed 1.0 zoom and
//! wrap rather than shrink, which a generic force-directed/layered layout
//! doesn't guarantee. The islands grid (unopened) stays plain `Paragraph`
//! boxes — islands never connect to each other, so rataflow buys nothing
//! there (that view's own spatial keyboard navigation is
//! `fleet::nearest_island_in_direction`, a plain pure function, not this
//! module).

use std::collections::{HashMap, HashSet};

use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};
use rataflow::{Edge, EdgeContent, EdgeMarker, EdgeStyle, Flow, Node, NodeContent, NodeRenderContext, StepEdge};

use ninox_overview::islands::Island;
use ninox_overview::status::MemberStatus;

use super::fleet::{island_children, member_status_color, member_tag};
use super::palette::Palette;
use super::state::Row;

/// Card size in world/terminal units — `Flow`'s viewport is locked to
/// exactly 1.0 zoom (see `build_flow`), so these are literal column/row
/// counts. Wide and tall enough for the brief's four content lines without
/// truncating a typical name.
const CARD_W: f64 = 32.0;
/// 2 border rows + the 4 content lines `CardContent` always renders (status
/// dot/type/harness, name, brief, activity-or-tag) — a card shorter than
/// this silently clips the last line, since `NodeRenderContext::area` is
/// the card's full area and `Paragraph`'s own `Block` inset leaves exactly
/// `CARD_H - 2` rows for content.
const CARD_H: f64 = 6.0;
const COL_GAP: f64 = 2.0;
const ROW_GAP: f64 = 1.0;
/// Vertical gap between a parent row and its children's row — wider than
/// `ROW_GAP` so the trunk edge reads as a distinct connector, not more
/// sibling spacing.
const TRUNK_GAP: f64 = 2.0;

#[derive(Debug, Clone)]
pub struct CardContent {
    /// Line 1: status dot + type + harness, e.g. "IMPL · claude-code".
    pub kind_harness: String,
    /// Line 2: the bold name, already ellipsized to fit the card.
    pub name: String,
    /// Line 3: the brief (`ninox_overview::display::display_brief`),
    /// blank when there's nothing worth showing.
    pub brief: String,
    /// Line 4: the live activity note, or the PR/status tag text.
    pub activity_or_tag: String,
    pub status_color: Color,
    pub palette: Palette,
}

impl NodeContent for CardContent {
    fn render(&self, ctx: &NodeRenderContext, buf: &mut Buffer) {
        let p = &self.palette;
        let (border_color, bg) = if ctx.selected { (p.working, Some(p.highlight)) } else { (self.status_color, None) };
        let mut block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(border_color));
        if let Some(bg) = bg {
            block = block.style(Style::default().bg(bg));
        }
        // `p.ink_2` rather than `p.faint` while selected: `p.highlight`
        // (the selected background) is chosen distinctly from both in
        // every palette, but `p.ink_2` reads better than dimmed `p.faint`
        // against a highlighted row regardless.
        let dim_fg = if ctx.selected { p.ink_2 } else { p.faint };
        // The status dot's colour comes from `member_status_color`, which
        // for `WaitingOnPr`/`Stopped`/`Done` is `p.faint`/`p.done` — both
        // `p.highlight` in the terminal palette. Swap to `p.ink_2` whenever
        // that would otherwise paint the dot the exact colour of the
        // selected card's own background, rather than hardcoding the two
        // tokens that currently collide.
        let dot_color = if ctx.selected && self.status_color == p.highlight { p.ink_2 } else { self.status_color };
        let lines = vec![
            Line::from(vec![
                Span::styled("● ", Style::default().fg(dot_color)),
                Span::styled(self.kind_harness.clone(), Style::default().fg(dim_fg)),
            ]),
            Line::from(Span::styled(self.name.clone(), Style::default().fg(p.ink).add_modifier(Modifier::BOLD))),
            Line::from(Span::styled(self.brief.clone(), Style::default().fg(p.ink_2))),
            Line::from(Span::styled(self.activity_or_tag.clone(), Style::default().fg(dim_fg))),
        ];
        Paragraph::new(lines).block(block).render(ctx.area, buf);
    }
}

#[derive(Debug, Clone)]
pub struct FoldContent {
    pub label: String,
    pub palette: Palette,
}

impl NodeContent for FoldContent {
    fn render(&self, ctx: &NodeRenderContext, buf: &mut Buffer) {
        let p = &self.palette;
        let mut style = Style::default().fg(p.faint);
        if ctx.selected {
            style = Style::default().fg(p.working).bg(p.highlight);
        }
        let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(if ctx.selected { p.working } else { p.faint }));
        let inner_style = if ctx.selected { style } else { Style::default().fg(p.faint) };
        Paragraph::new(Span::styled(format!("▸ {}", self.label), inner_style)).block(block).render(ctx.area, buf);
    }
}

#[derive(Debug, Clone)]
pub enum OpenedNode {
    Card(CardContent),
    Fold(FoldContent),
}

impl NodeContent for OpenedNode {
    fn render(&self, ctx: &NodeRenderContext, buf: &mut Buffer) {
        match self {
            Self::Card(c) => c.render(ctx, buf),
            Self::Fold(f) => f.render(ctx, buf),
        }
    }
}

/// The style a working-member edge gets: the fixed hierarchy line
/// animates (marching ants) only when its target is actively `Working` —
/// a static line otherwise, matching the brief's "marching-ants only on
/// edges to working nodes".
fn working_edge_style(p: &Palette) -> EdgeStyle {
    EdgeStyle::default().with_stroke_style(Style::default().fg(p.working)).with_marker_end(EdgeMarker::ArrowClosed)
}

/// Clips `s` to at most `max_chars`, whole, with an ellipsis.
fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let clipped: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{clipped}…")
    }
}

/// One node's worth of data before positioning — built once per member (or
/// fold group), independent of layout.
struct Entry {
    id: String,
    parent: Option<String>,
    content: OpenedNode,
}

fn card_entry(rows: &[Row], island: &Island, id: &str, palette: &Palette, parent: Option<String>) -> Entry {
    let status = island.members.iter().find(|m| m.id == id).map(|m| m.status).unwrap_or(MemberStatus::Working);
    let row = rows.iter().find(|r| r.id() == id);
    let name = row.map(|r| r.session.name.clone()).unwrap_or_else(|| id.to_string());
    let kind = row
        .map(|r| if r.is_orchestrator { "ORCHESTRATOR".to_string() } else { r.session.worker_type.clone().unwrap_or_else(|| "WORKER".into()).to_uppercase() })
        .unwrap_or_default();
    let harness = row.map(|r| r.session.agent_type.clone()).unwrap_or_default();
    let brief = row
        .and_then(|r| r.session.summary.as_deref())
        .map(ninox_overview::display::display_brief)
        .filter(|b| !b.is_empty())
        .unwrap_or_default();
    let activity_or_tag = row
        .filter(|_| status == MemberStatus::Working)
        .and_then(|r| r.session.activity_note.as_deref())
        .filter(|n| !n.trim().is_empty())
        .map(|n| format!("▸ {n}"))
        .or_else(|| row.and_then(|r| member_tag(status, r, palette)).map(|(t, _)| t.trim().to_string()))
        .unwrap_or_else(|| match status {
            MemberStatus::NeedsYou => "!you".to_string(),
            MemberStatus::WaitingOnPr => "waiting on PR".to_string(),
            _ => "idle".to_string(),
        });
    Entry {
        id: id.to_string(),
        parent,
        content: OpenedNode::Card(CardContent {
            kind_harness: format!("{kind} · {harness}"),
            name: truncate_chars(&name, (CARD_W as usize).saturating_sub(2)),
            brief: truncate_chars(&brief, (CARD_W as usize).saturating_sub(2)),
            activity_or_tag: truncate_chars(&activity_or_tag, (CARD_W as usize).saturating_sub(2)),
            status_color: member_status_color(status, palette),
            palette: *palette,
        }),
    }
}

/// Walks the island's tree from `root` (or lists every member flat for the
/// Unparented group), producing one `Entry` per visible card plus one
/// `Entry` per fold group — in the exact order `assign_positions` will lay
/// them out (parent immediately before its children).
fn collect_entries(rows: &[Row], island: &Island, fold_expanded: bool, palette: &Palette) -> Vec<Entry> {
    let children = island_children(rows, island);
    let status_of = |id: &str| island.members.iter().find(|m| m.id == id).map(|m| m.status).unwrap_or(MemberStatus::Working);
    let name_of = |id: &str| rows.iter().find(|r| r.id() == id).map(|r| r.session.name.clone()).unwrap_or_else(|| id.to_string());

    let mut entries = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    if island.root.is_none() {
        for m in &island.members {
            if seen.insert(m.id.clone()) {
                entries.push(card_entry(rows, island, &m.id, palette, None));
            }
        }
        return entries;
    }

    let mut stack: Vec<(String, Option<String>)> = vec![(island.root.clone().unwrap(), None)];
    while let Some((id, parent)) = stack.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        entries.push(card_entry(rows, island, &id, palette, parent.clone()));

        let mut kids = children.get(&id).cloned().unwrap_or_default();
        kids.sort_by_key(|cid| name_of(cid));
        let (finished, active): (Vec<String>, Vec<String>) =
            kids.into_iter().partition(|cid| matches!(status_of(cid), MemberStatus::Done | MemberStatus::Stopped));

        for cid in active.into_iter().rev() {
            stack.push((cid, Some(id.clone())));
        }
        if finished.is_empty() {
            continue;
        }
        if fold_expanded {
            for cid in finished.into_iter().rev() {
                stack.push((cid, Some(id.clone())));
            }
        } else {
            let done = finished.iter().filter(|cid| status_of(cid) == MemberStatus::Done).count();
            let stopped = finished.len() - done;
            let mut parts = Vec::new();
            if done > 0 {
                parts.push(format!("{done} done"));
            }
            if stopped > 0 {
                parts.push(format!("{stopped} stopped"));
            }
            let fold_id = format!("fold:{id}");
            seen.insert(fold_id.clone());
            entries.push(Entry {
                id: fold_id,
                parent: Some(id.clone()),
                content: OpenedNode::Fold(FoldContent { label: parts.join(" · "), palette: *palette }),
            });
        }
    }
    entries
}

/// Assigns each entry a top-down, wrapped `(x, y)` position: one row-band
/// per tree depth (root at the top), with a band's own entries wrapped
/// into as many sub-rows as `available_width` forces, each sub-row
/// centered — never narrower than one card, never scrolling horizontally.
/// Returns the positions plus the total content height (world units), so
/// the caller can size the `Flow`'s scrollable area.
fn assign_positions(entries: &[Entry], available_width: f64) -> (HashMap<String, (f64, f64)>, f64) {
    let cols_per_row = ((available_width + COL_GAP) / (CARD_W + COL_GAP)).floor().max(1.0) as usize;

    // Depth via BFS from whichever entries have no parent (roots) — the
    // Unparented group's flat list has every entry at depth 0.
    let mut depth: HashMap<&str, usize> = HashMap::new();
    for e in entries {
        if e.parent.is_none() {
            depth.insert(&e.id, 0);
        }
    }
    // Entries are already parent-before-child (see `collect_entries`), so a
    // single forward pass resolves every depth.
    for e in entries {
        if let Some(parent) = &e.parent {
            let d = depth.get(parent.as_str()).copied().unwrap_or(0) + 1;
            depth.entry(&e.id).or_insert(d);
        }
    }

    let max_depth = depth.values().copied().max().unwrap_or(0);
    let mut by_depth: Vec<Vec<&Entry>> = vec![Vec::new(); max_depth + 1];
    for e in entries {
        let d = depth.get(e.id.as_str()).copied().unwrap_or(0);
        by_depth[d].push(e);
    }
    // Within a depth band, group by parent (keeping entry order, already
    // name-sorted per parent from `collect_entries`) so siblings stay
    // adjacent and a wrap boundary never lands inside one parent's group.
    for band in &mut by_depth {
        band.sort_by_key(|e| e.parent.clone().unwrap_or_default());
    }

    let mut positions = HashMap::new();
    let mut y = 0.0;
    for band in &by_depth {
        let mut row_start = 0;
        while row_start < band.len() {
            let row_end = (row_start + cols_per_row).min(band.len());
            let row = &band[row_start..row_end];
            let row_w = row.len() as f64 * CARD_W + (row.len().saturating_sub(1)) as f64 * COL_GAP;
            let start_x = ((available_width - row_w) / 2.0).max(0.0);
            for (i, e) in row.iter().enumerate() {
                let x = start_x + i as f64 * (CARD_W + COL_GAP);
                positions.insert(e.id.clone(), (x, y));
            }
            y += CARD_H + ROW_GAP;
            row_start = row_end;
        }
        // Replace the last sub-row's `ROW_GAP` with `TRUNK_GAP` before the
        // next (deeper) band starts, so the step down to children reads as
        // a trunk, not just more sibling spacing.
        y += TRUNK_GAP - ROW_GAP;
    }
    (positions, y)
}

/// Builds the island's own card tree at a fixed 1.0 zoom: real cards for
/// every active member (or every member flat, for the Unparented group),
/// one folded summary card per parent with finished (done/stopped)
/// children when `fold_expanded` is false, hierarchy edges throughout.
/// `available_width` is the render area's width in columns — children wrap
/// into additional rows rather than shrinking or scrolling horizontally.
pub fn build_flow(rows: &[Row], island: &Island, fold_expanded: bool, palette: &Palette, available_width: f64) -> Option<Flow<OpenedNode, StepEdge>> {
    let entries = collect_entries(rows, island, fold_expanded, palette);
    if entries.is_empty() {
        return None;
    }
    let (positions, _total_height) = assign_positions(&entries, available_width.max(CARD_W));

    // A synthetic fold node (bundling only Done/Stopped members, never a
    // `Working` one by construction — see `collect_entries`) has no entry
    // in `island.members` at all; defaulting a missing lookup to `Working`
    // here would make its edge animate as if it were actively running, so
    // "not found" must mean "don't animate," not "assume working."
    let status_of = |id: &str| island.members.iter().find(|m| m.id == id).map(|m| m.status);
    // rataflow's own default (`Right`/`Left`, built for left-to-right flow
    // diagrams) is exactly the "chained" look the design rejects — a
    // sibling row's edges routing out one card's side and into the next's
    // reads as a chain, not a hierarchy. `Bottom`/`Top` routes every edge
    // straight down from a parent's bottom-centre into a child's
    // top-centre instead, which is what makes same-row siblings sharing a
    // parent look like a trunk (`StepEdge`'s own right-angle routing),
    // never like them pointing at each other.
    let nodes: Vec<Node<OpenedNode>> = entries
        .iter()
        .map(|e| {
            let pos = positions.get(&e.id).copied().unwrap_or((0.0, 0.0));
            Node::new(e.id.clone(), pos, (CARD_W, CARD_H), e.content.clone())
                .with_draggable(false)
                .with_source_position(rataflow::HandlePosition::Bottom)
                .with_target_position(rataflow::HandlePosition::Top)
        })
        .collect();
    let edges: Vec<Edge<StepEdge>> = entries
        .iter()
        .filter_map(|e| {
            let parent = e.parent.as_ref()?;
            let mut edge = Edge::new(format!("{parent}->{}", e.id), parent.clone(), e.id.clone());
            if status_of(&e.id) == Some(MemberStatus::Working) {
                edge.animated = true;
                edge.content = StepEdge::default().with_style(working_edge_style(palette));
            }
            Some(edge)
        })
        .collect();

    let mut flow = Flow::with_graph(nodes, edges).ok()?.with_min_zoom(1.0).with_max_zoom(1.0);
    flow.viewport = rataflow::Viewport::default_view();
    Some(flow)
}

/// Carries a rebuilt `Flow`'s viewport and node selection over from the one
/// it replaces — without this, the user's own scroll position (or node
/// selection) is undone every time `build_flow` runs again (every render,
/// since content can change live), since a fresh `Flow` always starts at
/// the default viewport with nothing selected.
pub fn carry_over_viewport_and_selection<N: NodeContent, E: EdgeContent>(old: &Flow<N, E>, new: &mut Flow<N, E>) {
    new.viewport = old.viewport;
    if let Some(id) = old.first_selected_node_id() {
        if new.node(&id).is_some() {
            new.select_node(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninox_core::types::SessionStatus;
    use ninox_overview::islands::build_islands;
    use std::collections::HashMap as StdHashMap;

    fn row(id: &str, group: Option<&str>, is_orchestrator: bool, status: SessionStatus) -> Row {
        let mut row = Row {
            session: ninox_ui_shared::cli::test_fixtures::session(id, group, status),
            is_orchestrator,
            group: group.map(String::from),
            machine_health: None, queue_position: None,
        };
        if is_orchestrator {
            row.group = Some(id.to_string());
            row.session.orchestrator_id = None;
        }
        row
    }

    fn island_for(rows: &[Row], root: &str) -> Island {
        let snapshot = crate::tui::fleet::fleet_snapshot(rows);
        let islands = build_islands(&snapshot, &StdHashMap::new());
        islands.into_iter().find(|i| i.root.as_deref() == Some(root)).expect("island built")
    }

    #[test]
    fn build_flow_includes_a_card_per_member() {
        let rows = vec![row("acme", None, true, SessionStatus::Working), row("w1", Some("acme"), false, SessionStatus::Working)];
        let island = island_for(&rows, "acme");
        let flow = build_flow(&rows, &island, false, &Palette::default(), 120.0).unwrap();
        assert!(flow.node("acme").is_some());
        assert!(flow.node("w1").is_some());
    }

    fn rendered_lines(content: &CardContent, selected: bool) -> Vec<String> {
        let area = ratatui::layout::Rect::new(0, 0, CARD_W as u16, CARD_H as u16);
        let mut buf = Buffer::empty(area);
        let ctx = NodeRenderContext {
            id: "n",
            area,
            selected,
            dragging: false,
            position_absolute: rataflow::Position::new(0.0, 0.0),
            theme: rataflow::Theme::default(),
            animation_phase: 0,
        };
        content.render(&ctx, &mut buf);
        (0..area.height).map(|y| (0..area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>()).collect()
    }

    #[test]
    fn card_renders_all_four_content_lines_selected_or_not() {
        // Regression: CARD_H only left room for 3 of 4 content rows (the
        // activity/tag line silently clipped), and the selected card's dim
        // lines (`p.faint`) were invisible against the selected background
        // (`p.card`) in the default terminal palette, where both are
        // `DarkGray`.
        let content = CardContent {
            kind_harness: "IMPL · claude-code".into(),
            name: "mlops-9999-example".into(),
            brief: "a brief worth keeping".into(),
            activity_or_tag: "✓ PR #42 · 3/3".into(),
            status_color: Color::Green,
            palette: Palette::default(),
        };
        for selected in [false, true] {
            let lines = rendered_lines(&content, selected);
            let joined = lines.join("\n");
            assert!(joined.contains("IMPL"), "line 1 (type · harness) missing, selected={selected}:\n{joined}");
            assert!(joined.contains("mlops-9999-example"), "line 2 (name) missing, selected={selected}:\n{joined}");
            assert!(joined.contains("a brief worth keeping"), "line 3 (brief) missing, selected={selected}:\n{joined}");
            assert!(joined.contains("PR #42"), "line 4 (activity/tag) missing, selected={selected}:\n{joined}");
        }
    }

    #[test]
    fn build_flow_routes_edges_top_down_not_side_to_side() {
        // Regression: rataflow's own default handle positions (Right
        // source / Left target) are built for left-to-right flow diagrams
        // — on our visually top-down layout that reads as a chain through
        // the card flanks, exactly the look the design rejected. Every
        // node must explicitly route Bottom (source) -> Top (target).
        let rows = vec![row("acme", None, true, SessionStatus::Working), row("w1", Some("acme"), false, SessionStatus::Working)];
        let island = island_for(&rows, "acme");
        let flow = build_flow(&rows, &island, false, &Palette::default(), 120.0).unwrap();
        for id in ["acme", "w1"] {
            let node = flow.node(id).unwrap();
            assert_eq!(node.source_position, rataflow::HandlePosition::Bottom, "{id}'s source handle must be on the bottom");
            assert_eq!(node.target_position, rataflow::HandlePosition::Top, "{id}'s target handle must be on the top");
        }
    }

    #[test]
    fn build_flow_locks_zoom_at_one() {
        let rows = vec![row("acme", None, true, SessionStatus::Working)];
        let island = island_for(&rows, "acme");
        let flow = build_flow(&rows, &island, false, &Palette::default(), 120.0).unwrap();
        assert_eq!(flow.viewport.zoom, 1.0);
    }

    #[test]
    fn build_flow_animates_the_edge_to_a_working_member_only() {
        let rows = vec![
            row("acme", None, true, SessionStatus::Working),
            row("working", Some("acme"), false, SessionStatus::Working),
            row("ci-failed", Some("acme"), false, SessionStatus::CiFailed),
        ];
        let island = island_for(&rows, "acme");
        let flow = build_flow(&rows, &island, false, &Palette::default(), 120.0).unwrap();
        let animated: Vec<bool> = flow.edges().iter().map(|e| e.animated).collect();
        assert!(animated.contains(&true), "the edge to the working member must animate");
        assert!(animated.contains(&false), "the edge to the non-working member must not animate");
    }

    #[test]
    fn build_flow_folds_finished_members_into_one_node_when_not_expanded() {
        let rows = vec![
            row("acme", None, true, SessionStatus::Working),
            row("done1", Some("acme"), false, SessionStatus::Done),
            row("stopped1", Some("acme"), false, SessionStatus::Terminated),
        ];
        let island = island_for(&rows, "acme");
        let flow = build_flow(&rows, &island, false, &Palette::default(), 120.0).unwrap();
        assert!(flow.node("done1").is_none(), "a folded member has no card of its own");
        assert!(flow.node("fold:acme").is_some(), "one fold node summarizes both finished members");
    }

    #[test]
    fn build_flow_never_animates_the_edge_to_a_folded_group() {
        // Regression: a fold node has no entry in `island.members` (it's
        // synthetic), so a naive lookup defaulting to "working" on a miss
        // would make a fully-finished group's edge animate as if it were
        // still running.
        let rows = vec![
            row("acme", None, true, SessionStatus::Working),
            row("done1", Some("acme"), false, SessionStatus::Done),
            row("stopped1", Some("acme"), false, SessionStatus::Terminated),
        ];
        let island = island_for(&rows, "acme");
        let flow = build_flow(&rows, &island, false, &Palette::default(), 120.0).unwrap();
        let fold_edge = flow.edges().iter().find(|e| e.target == "fold:acme").expect("an edge into the fold node");
        assert!(!fold_edge.animated, "a fold node bundling only done/stopped members must never show marching ants");
    }

    #[test]
    fn build_flow_shows_every_card_when_fold_expanded() {
        let rows = vec![row("acme", None, true, SessionStatus::Working), row("done1", Some("acme"), false, SessionStatus::Done)];
        let island = island_for(&rows, "acme");
        let flow = build_flow(&rows, &island, true, &Palette::default(), 120.0).unwrap();
        assert!(flow.node("done1").is_some());
        assert!(flow.node("fold:acme").is_none());
    }

    #[test]
    fn build_flow_lists_unparented_members_flat_with_no_edges() {
        let a = row("solo-a", None, false, SessionStatus::Working);
        let b = row("solo-b", None, false, SessionStatus::Working);
        let rows = vec![a, b];
        let snapshot = crate::tui::fleet::fleet_snapshot(&rows);
        let islands = build_islands(&snapshot, &StdHashMap::new());
        let island = islands.into_iter().find(|i| i.root.is_none()).expect("unparented island built");
        let flow = build_flow(&rows, &island, false, &Palette::default(), 120.0).unwrap();
        assert!(flow.node("solo-a").is_some() && flow.node("solo-b").is_some());
        assert!(flow.edges().is_empty(), "the Unparented group has no single root to hang edges from");
    }

    #[test]
    fn assign_positions_wraps_children_instead_of_shrinking() {
        let root = Entry { id: "root".into(), parent: None, content: OpenedNode::Fold(FoldContent { label: String::new(), palette: Palette::default() }) };
        let children: Vec<Entry> = (0..5)
            .map(|i| Entry { id: format!("c{i}"), parent: Some("root".into()), content: OpenedNode::Fold(FoldContent { label: String::new(), palette: Palette::default() }) })
            .collect();
        let mut entries = vec![root];
        entries.extend(children);
        // Only 2 cards fit per row at this width.
        let width = 2.0 * CARD_W + COL_GAP;
        let (positions, _) = assign_positions(&entries, width);
        let ys: HashSet<i64> = (0..5).map(|i| positions[&format!("c{i}")].1 as i64).collect();
        assert!(ys.len() >= 3, "5 children at 2-per-row must wrap across at least 3 distinct y bands: {ys:?}");
        for (x, _) in positions.values() {
            assert!(*x >= -0.01 && *x + CARD_W <= width + 0.01, "no card may extend past the available width: x={x}");
        }
    }

    #[test]
    fn assign_positions_puts_the_root_above_its_children() {
        let root = Entry { id: "root".into(), parent: None, content: OpenedNode::Fold(FoldContent { label: String::new(), palette: Palette::default() }) };
        let child = Entry { id: "child".into(), parent: Some("root".into()), content: OpenedNode::Fold(FoldContent { label: String::new(), palette: Palette::default() }) };
        let (positions, _) = assign_positions(&[root, child], 200.0);
        assert!(positions["root"].1 < positions["child"].1, "the root must sit above its child");
    }
}
