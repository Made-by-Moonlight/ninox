//! The Fleet tab's opened-island view (MLOPS-5339 slice 3) — one
//! orchestrator's own tree as full cards, matching `FleetFocused.dc.html`.
//! Reuses `ninox_overview::islands::Island` membership (never re-derives
//! which sessions belong to this island) and `fleet_islands`'s status/badge
//! classification; the only thing built here is the hierarchy (parent/child
//! edges scoped to this island's members) needed to draw cards hanging from
//! a trunk, mirroring the TUI's `graph::overview_from_rows`: `brief` is
//! `session.summary`, not the DB-only `FleetRecord.task_brief`, which isn't
//! available at this layer without per-card store I/O.
//!
//! Folding (finished workers into one stacked card) reuses slice 2's
//! `fleet_expanded_folds`/`Message::FleetToggleFold` — one shared toggle for
//! the whole island, not a second per-node mechanism — so a level with no
//! finished members just never shows a fold card, and the common case (one
//! orchestrator, no sub-orchestrators) behaves exactly like the design.

use std::collections::{HashMap, HashSet};

use iced::{
    widget::{button, column, container, mouse_area, row, scrollable, text, Space},
    Alignment, Background, Border, Color, Element, Length, Padding,
};

use ninox_core::types::Session;

use ninox_overview::edge::{EdgeKind, OverviewEdge};
use ninox_overview::islands::{Island, IslandKey};
use ninox_overview::node::{NodeId, NodeKind, OverviewNode, RealNode};
use ninox_overview::status::MemberStatus;

use super::fleet_islands::{chip_pill, fold_expanded, member_chip, member_status_color, seg_control};
use super::shell::tokens;
use crate::app::{App, Message};

const CARD_WIDTH: f32 = 204.0;
const CARD_GAP: f32 = 14.0;
const ROW_GAP: f32 = 22.0;
const CONNECTOR_HEIGHT: f32 = 18.0;

/// `Legend.dc.html`'s `.badge-type`/`.badge-harness` — pulled directly from
/// its CSS, same convention as `shell::tokens`.
const BADGE_TYPE_BG: Color = Color { r: 0x1e as f32 / 255.0, g: 0x27 as f32 / 255.0, b: 0x40 as f32 / 255.0, a: 1.0 };
const BADGE_TYPE_FG: Color = Color { r: 0x9d as f32 / 255.0, g: 0xb4 as f32 / 255.0, b: 0xff as f32 / 255.0, a: 1.0 };
const BADGE_HARNESS_BG: Color = Color { r: 0x15 as f32 / 255.0, g: 0x23 as f32 / 255.0, b: 0x1d as f32 / 255.0, a: 1.0 };
const BADGE_HARNESS_FG: Color = Color { r: 0x7f as f32 / 255.0, g: 0xd8 as f32 / 255.0, b: 0xa8 as f32 / 255.0, a: 1.0 };
const BADGE_FINISHED_BG: Color = Color { r: 0x15 as f32 / 255.0, g: 0x29 as f32 / 255.0, b: 0x1f as f32 / 255.0, a: 1.0 };
const BADGE_FINISHED_FG: Color = Color { r: 0x7f as f32 / 255.0, g: 0xd8 as f32 / 255.0, b: 0xa8 as f32 / 255.0, a: 1.0 };
/// `FleetFocused.dc.html`'s `.card-sel` — a distinct gold, not the island
/// grid's teal `ISLAND_SELECTED_BORDER`, since the two selections (a grid
/// card vs. a card inside the opened view) are different things.
const CARD_SELECTED_BORDER: Color = Color { r: 0xc9 as f32 / 255.0, g: 0x8a as f32 / 255.0, b: 0x2e as f32 / 255.0, a: 1.0 };

/// Clips `s` to at most `max_chars`, whole, with an ellipsis — same rule as
/// `fleet_islands::truncate_chars` and `shell::tree::truncate_chars`
/// (duplicated rather than shared: a trivial leaf utility, not
/// classification logic that must agree across call sites).
fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let clipped: String = s.chars().take(max_chars).collect();
        format!("{clipped}…")
    }
}

/// Builds real nodes + hierarchy edges for everyone in `island.members` —
/// not a second derivation of island membership, just the parent/child
/// shape needed to lay the island's own members out as a tree.
pub(crate) fn nodes_for_island(app: &App, island: &Island) -> (Vec<OverviewNode>, Vec<OverviewEdge>) {
    let orchestrator_ids: HashSet<&str> = app.orchestrators.iter().map(|o| o.id.as_str()).collect();
    let member_ids: HashSet<&str> = island.members.iter().map(|m| m.id.as_str()).collect();

    let nodes: Vec<OverviewNode> = island.members.iter().filter_map(|m| {
        let session = app.sessions.get(&m.id)?;
        Some(OverviewNode::Real(RealNode {
            id: m.id.clone(),
            kind: if orchestrator_ids.contains(m.id.as_str()) { NodeKind::Orchestrator } else { NodeKind::Worker },
            name: session.name.clone(),
            harness_badge: session.agent_type.clone(),
            worker_type_badge: session.worker_type.clone(),
            status: format!("{:?}", session.status),
            brief: session.summary.clone(),
            depth: 0,
            breadcrumb: Vec::new(),
            activity: session.activity_note.clone(),
            artifacts: Vec::new(),
            remote_badge: None,
            depth_capped: false,
        }))
    }).collect();

    let edges: Vec<OverviewEdge> = island.members.iter().filter_map(|m| {
        let session = app.sessions.get(&m.id)?;
        let parent = session.orchestrator_id.clone()?;
        if !member_ids.contains(parent.as_str()) {
            return None;
        }
        OverviewEdge::new(EdgeKind::Hierarchy, parent, m.id.clone(), &nodes).ok()
    }).collect();

    (nodes, edges)
}

fn children_of(edges: &[OverviewEdge], parent: &str) -> Vec<NodeId> {
    edges.iter()
        .filter(|e| e.kind() == EdgeKind::Hierarchy && e.from() == parent)
        .map(|e| e.to().to_string())
        .collect()
}

/// How many `CARD_WIDTH`-wide cards fit in `available_width` before
/// wrapping — never scrolls horizontally (brief's "Must not"),
/// approximating the same available-width wrap the TUI graph does (#230)
/// rather than a pixel-exact `column-count`.
fn per_row_for(available_width: f32) -> usize {
    let available = available_width.max(CARD_WIDTH);
    ((available + CARD_GAP) / (CARD_WIDTH + CARD_GAP)).floor().max(1.0) as usize
}

fn cards_per_row(app: &App, islands: &[Island]) -> usize {
    per_row_for(canvas_width(app, islands) - 40.0)
}

/// The Fleet canvas's width: the window less the tree and, when it's
/// showing, the detail panel.
pub(crate) fn canvas_width(app: &App, islands: &[Island]) -> f32 {
    use super::shell::tree::{EXPANDED_WIDTH, RAIL_WIDTH};
    let tree_w = if app.fleet_nav.tree_collapsed { RAIL_WIDTH } else { EXPANDED_WIDTH };
    // The detail panel (plus its 1px divider) takes real width out of the
    // canvas whenever it's actually showing — without this, cards wrap at a
    // per-row count sized for the panel-less width, and the last one or two
    // in each row render past the panel's left edge instead of wrapping
    // (found while screenshotting slice 4: an orchestrator with 8 children
    // showed only 5, the rest invisible behind the panel). Deferring to
    // `fleet_detail::context` — the shell's own source of truth for whether
    // the panel renders — rather than re-deriving "is it showing" here from
    // `fleet_nav.selected.is_some()` alone keeps the two from drifting if
    // that condition ever gains another gate (code-review finding).
    let panel_w = if super::fleet_detail::context(&app.view, &app.fleet_nav, Some(islands)).is_some() {
        super::fleet_detail::PANEL_WIDTH + 1.0
    } else {
        0.0
    };
    app.window_width - tree_w - panel_w
}

pub(crate) fn badge<'a>(label: String, bg: Color, fg: Color) -> Element<'a, Message> {
    container(text(label).size(9.5).font(crate::style::SANS).color(fg).wrapping(iced::widget::text::Wrapping::None))
        .padding([2, 7])
        .style(move |_| container::Style {
            background: Some(Background::Color(bg)),
            border: Border { radius: 4.0.into(), ..Default::default() },
            ..Default::default()
        })
        .into()
}

pub(crate) fn key_hint<'a>(label: &'static str) -> Element<'a, Message> {
    container(text(label).size(9.5).font(super::help_overlay::chip_font(label)).color(tokens::DIM))
        .padding([1, 5])
        .style(|_| container::Style {
            border: Border { color: tokens::BORDER, width: 1.0, radius: 4.0.into() },
            ..Default::default()
        })
        .into()
}

/// The activity/meta line a card shows below its brief: the live activity
/// note while working, otherwise the same chip `fleet_islands::member_chip`
/// gives the grid cards (must agree with it — see that function's own doc
/// comment), `None` for a plain idle/finished member (no extra line).
fn meta_line<'a>(session: &Session, status: MemberStatus, queue_position: Option<usize>) -> Option<Element<'a, Message>> {
    if status == MemberStatus::Working {
        if let Some(note) = session.activity_note.as_deref().filter(|n| !n.trim().is_empty()) {
            // Advanced: Inter has no "⚒", and Basic shaping doesn't fall back.
            return Some(text(format!("⚒ {note}")).size(10.5).font(crate::style::SANS).shaping(iced::widget::text::Shaping::Advanced).color(tokens::TEAL).into());
        }
        return None;
    }
    member_chip(status, session, queue_position).map(|(label, bg, fg)| chip_pill(label, bg, fg))
}

fn real_card<'a>(app: &'a App, id: &str, status: MemberStatus, is_orchestrator: bool, selected: bool) -> Element<'a, Message> {
    let session = app.sessions.get(id);
    let name = session.map(|s| s.name.as_str()).unwrap_or(id);
    let harness = session.map(|s| s.agent_type.clone()).unwrap_or_default();
    let type_label = if is_orchestrator {
        "Orchestrator".to_string()
    } else {
        session.and_then(|s| s.worker_type.clone()).unwrap_or_else(|| "Worker".to_string())
    };

    let mut lines: Vec<Element<Message>> = vec![
        row![badge(type_label, BADGE_TYPE_BG, BADGE_TYPE_FG), badge(harness, BADGE_HARNESS_BG, BADGE_HARNESS_FG)]
            .spacing(6)
            .into(),
        container(
            text(truncate_chars(name, 34))
                .size(12.5)
                .font(crate::style::SANS)
                .color(tokens::TEXT)
                .wrapping(iced::widget::text::Wrapping::None),
        )
        .width(Length::Fixed(CARD_WIDTH - 22.0))
        .clip(true)
        .into(),
    ];
    if let Some(brief) = session.and_then(|s| s.summary.as_deref()).filter(|b| !b.trim().is_empty()) {
        let brief = ninox_overview::display::display_brief(brief);
        if !brief.is_empty() {
            lines.push(
                container(text(brief).size(10.5).font(crate::style::SANS).color(tokens::DIM))
                    .width(Length::Fixed(CARD_WIDTH - 22.0))
                    .into(),
            );
        }
    }
    if let Some(session) = session {
        if let Some(meta) = meta_line(session, status, app.queue_positions.get(id).copied()) {
            lines.push(meta);
        }
    }

    let bar_color = member_status_color(status);
    let border_color = if selected { CARD_SELECTED_BORDER } else { tokens::BORDER };
    let border_width = if selected { 2.0 } else { 1.0 };

    let card = container(column![
        container(Space::new(0, 0)).width(Length::Fill).height(Length::Fixed(4.0)).style(move |_| {
            container::Style { background: Some(Background::Color(bar_color)), ..Default::default() }
        }),
        container(column(lines).spacing(5)).padding(Padding { top: 8.0, right: 11.0, bottom: 10.0, left: 11.0 }),
    ])
    .width(Length::Fixed(CARD_WIDTH))
    .style(move |_| container::Style {
        background: Some(Background::Color(tokens::PANEL)),
        border: Border { color: border_color, width: border_width, radius: 8.0.into() },
        ..Default::default()
    });

    mouse_area(card).on_press(Message::FleetNodeCardPressed(id.to_string())).into()
}

fn finished_fold_card<'a>(done: usize, stopped: usize, key: IslandKey) -> Element<'a, Message> {
    let mut parts = Vec::new();
    if done > 0 {
        parts.push(format!("{done} done"));
    }
    if stopped > 0 {
        parts.push(format!("{stopped} stopped"));
    }
    let front = container(column![
        container(Space::new(0, 0)).width(Length::Fill).height(Length::Fixed(4.0)).style(|_| {
            container::Style { background: Some(Background::Color(tokens::GREEN)), ..Default::default() }
        }),
        container(
            column![
                badge(format!("{} FINISHED", done + stopped), BADGE_FINISHED_BG, BADGE_FINISHED_FG),
                text(parts.join(" · ")).size(12.5).font(crate::style::SANS).color(tokens::TEXT),
                text("folded — click to unfold").size(9.5).font(crate::style::SANS_ITALIC).color(tokens::DIM2),
            ]
            .spacing(5),
        )
        .padding(Padding { top: 8.0, right: 11.0, bottom: 10.0, left: 11.0 }),
    ])
    .width(Length::Fixed(CARD_WIDTH))
    .style(|_| container::Style {
        background: Some(Background::Color(tokens::PANEL)),
        border: Border { color: tokens::BORDER, width: 1.0, radius: 8.0.into() },
        ..Default::default()
    });

    button(front)
        .on_press(Message::FleetToggleFold(key))
        .padding(0)
        .style(|_t, _status| button::Style { background: None, ..Default::default() })
        .into()
}

/// Chunks `cells` into rows of at most `per_row`, keeping each row's own
/// `Vec` (rather than immediately building a `row!` widget) so a caller that
/// needs to know how many cards are in each wrapped row — e.g. to size that
/// row's own hierarchy-trunk segment — can see it.
fn group_rows<'a>(cells: Vec<Element<'a, Message>>, per_row: usize) -> Vec<Vec<Element<'a, Message>>> {
    let per_row = per_row.max(1);
    let mut rows = Vec::new();
    let mut current: Vec<Element<Message>> = Vec::new();
    for cell in cells {
        current.push(cell);
        if current.len() >= per_row {
            rows.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        rows.push(current);
    }
    rows
}

fn wrap_rows<'a>(cells: Vec<Element<'a, Message>>, per_row: usize) -> Vec<Element<'a, Message>> {
    group_rows(cells, per_row)
        .into_iter()
        .map(|group| row(group).spacing(CARD_GAP).align_y(Alignment::Start).into())
        .collect()
}

/// The center-to-center span a row's hierarchy-trunk horizontal bar must
/// cover for `n` child cards — `n - 1` hops of one card width plus one gap
/// each, not the full row width (which would overshoot past the last
/// child's own center tick into its far edge). Zero for 0 or 1 children: a
/// single child gets a plain vertical tick, no horizontal bar.
fn trunk_span(n: usize) -> f32 {
    if n <= 1 {
        0.0
    } else {
        (n - 1) as f32 * (CARD_WIDTH + CARD_GAP)
    }
}

/// Whether a node's children hang from one continuous connector (exactly
/// one child — brief item 3a: a single child must not render as two short
/// dashed segments with a gap) or from trunk segments, one per wrapped row
/// (brief item 3b: this now applies to a nested sub-orchestrator's own
/// children too, not just the island's root).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildTrunk {
    Single,
    Fanned,
}

fn child_trunk_shape(n_children: usize) -> ChildTrunk {
    if n_children == 1 {
        ChildTrunk::Single
    } else {
        ChildTrunk::Fanned
    }
}

/// The continuous root → child connector for `ChildTrunk::Single` — one
/// solid bar the same total height the two-segment (tick / gap / tick)
/// layout used to occupy, so fixing the "broken dashed stub" doesn't change
/// the island's overall vertical rhythm.
fn single_child_connector<'a>() -> Element<'a, Message> {
    container(Space::new(0, 0))
        .width(Length::Fixed(2.0))
        .height(Length::Fixed(CONNECTOR_HEIGHT * 2.0 + ROW_GAP))
        .style(|_| container::Style { background: Some(Background::Color(tokens::DIM2)), ..Default::default() })
        .into()
}

/// One row-trunk segment (horizontal bar + ticks) per wrapped row of
/// `cells` — shared by the island root and, since brief item 3b, a nested
/// sub-orchestrator's own children (previously only the root drew this).
/// `top_level` must be `false` for a nested sub-orchestrator: a
/// `center_x(Length::Fill)` child forces its own enclosing `Column` to
/// `Length::Fill` too (`Column::push` encloses its width to the widest
/// child), and that column sits inside the parent's `row(group)` cell —
/// `Fill` there would swallow the row's remaining width from its sibling
/// cards and misalign the parent's own per-child ticks (code-review
/// finding on this function's first version, which used `Fill` either way).
fn children_rows_block<'a>(cells: Vec<Element<'a, Message>>, per_row: usize, top_level: bool) -> Element<'a, Message> {
    let mut body: Vec<Element<Message>> = Vec::new();
    for group in group_rows(cells, per_row) {
        let n = group.len();
        body.push(row_trunk(n, top_level));
        let row_el = row(group).spacing(CARD_GAP).align_y(Alignment::Start);
        body.push(if top_level { container(row_el).center_x(Length::Fill).into() } else { row_el.into() });
    }
    let col = column(body).spacing(ROW_GAP).padding(Padding { top: ROW_GAP, right: 0.0, bottom: 0.0, left: 0.0 });
    if top_level { col.into() } else { col.align_x(Alignment::Center).into() }
}

/// One wrapped row's hierarchy-trunk segment: a horizontal bar spanning
/// `trunk_span(n)`, centered the same way the cards row below it is
/// centered, over a row of `n` `CARD_WIDTH`-wide cells each holding a short
/// vertical drop tick — because both this row and the cards row use
/// identical cell widths/gaps and the same centering, the ticks land over
/// each card without needing absolute positioning (#254 review carry-over:
/// the trunk previously spanned the full canvas width instead of just this
/// row's own children, with no per-child tick).
fn row_trunk<'a>(n: usize, top_level: bool) -> Element<'a, Message> {
    let span = trunk_span(n);
    let bar: Element<Message> = if span > 0.0 {
        container(Space::new(0, 0)).width(Length::Fixed(span)).height(Length::Fixed(2.0)).style(|_| {
            container::Style { background: Some(Background::Color(tokens::DIM2)), ..Default::default() }
        }).into()
    } else {
        Space::new(0, 2).into()
    };

    let tick = || {
        container(container(Space::new(0, 0)).width(Length::Fixed(2.0)).height(Length::Fixed(CONNECTOR_HEIGHT)).style(|_| {
            container::Style { background: Some(Background::Color(tokens::DIM2)), ..Default::default() }
        }))
        .width(Length::Fixed(CARD_WIDTH))
        .center_x(Length::Fixed(CARD_WIDTH))
        .into()
    };
    let ticks: Vec<Element<Message>> = (0..n.max(1)).map(|_| tick()).collect();
    let ticks_row = row(ticks).spacing(CARD_GAP);

    // Same `Fill`-forces-the-enclosing-column concern as
    // `children_rows_block` — a nested sub-orchestrator's trunk must center
    // via plain `align_x`, never `center_x(Length::Fill)`.
    if top_level {
        column![container(bar).center_x(Length::Fill), container(ticks_row).center_x(Length::Fill)].into()
    } else {
        column![bar, ticks_row].align_x(Alignment::Center).into()
    }
}

/// Renders `id`'s own card plus (recursively) its children hanging from a
/// trunk below it — "Child cards hang from a solid hierarchy trunk, and
/// sub-orchestrators nest with their own trunk" (brief item 3). `top_level`
/// only affects whether the trunk spans the full canvas width (the root) or
/// is a short centered connector (a nested sub-orchestrator) — iced has no
/// absolute positioning, so a nested `Length::Fill` trunk inside a `row`
/// would fight its siblings for width.
#[allow(clippy::too_many_arguments)]
fn subtree<'a>(
    app: &'a App,
    id: &str,
    edges: &[OverviewEdge],
    orchestrator_ids: &HashSet<&str>,
    status_by_id: &HashMap<&str, MemberStatus>,
    island_key: &IslandKey,
    expanded: bool,
    selected: Option<&str>,
    per_row: usize,
    top_level: bool,
) -> Element<'a, Message> {
    let status = status_by_id.get(id).copied().unwrap_or(MemberStatus::Working);
    let is_orchestrator = orchestrator_ids.contains(id);
    let card = real_card(app, id, status, is_orchestrator, selected == Some(id));

    let mut kids = children_of(edges, id);
    kids.sort_by_key(|cid| app.sessions.get(cid).map(|s| s.name.clone()).unwrap_or_else(|| cid.clone()));
    if kids.is_empty() {
        return card;
    }

    let (finished, active): (Vec<NodeId>, Vec<NodeId>) = kids.into_iter()
        .partition(|cid| matches!(status_by_id.get(cid.as_str()), Some(MemberStatus::Done | MemberStatus::Stopped)));

    let mut cells: Vec<Element<Message>> = active.iter()
        .map(|cid| subtree(app, cid, edges, orchestrator_ids, status_by_id, island_key, expanded, selected, per_row, false))
        .collect();
    if !finished.is_empty() {
        if expanded {
            cells.extend(finished.iter().map(|cid| {
                subtree(app, cid, edges, orchestrator_ids, status_by_id, island_key, expanded, selected, per_row, false)
            }));
        } else {
            let done = finished.iter().filter(|cid| status_by_id.get(cid.as_str()) == Some(&MemberStatus::Done)).count();
            cells.push(finished_fold_card(done, finished.len() - done, island_key.clone()));
        }
    }

    if child_trunk_shape(cells.len()) == ChildTrunk::Single {
        let only = cells.into_iter().next().expect("ChildTrunk::Single implies exactly one cell");
        let connector = single_child_connector();
        return if top_level {
            column![
                container(card).center_x(Length::Fill),
                container(connector).center_x(Length::Fill),
                container(only).center_x(Length::Fill),
            ]
            .width(Length::Fill)
            .into()
        } else {
            column![card, connector, only].align_x(Alignment::Center).into()
        };
    }

    let connector = container(Space::new(0, 0))
        .width(Length::Fixed(2.0))
        .height(Length::Fixed(CONNECTOR_HEIGHT))
        .style(|_| container::Style { background: Some(Background::Color(tokens::DIM2)), ..Default::default() });
    let children = children_rows_block(cells, per_row, top_level);

    if top_level {
        column![
            container(card).center_x(Length::Fill),
            container(connector).center_x(Length::Fill),
            children,
        ]
        .width(Length::Fill)
        .into()
    } else {
        column![card, connector, children].align_x(Alignment::Center).into()
    }
}

/// The Unparented group has no single root orchestrator to hang a tree
/// from — every member renders as a flat wrapped row of cards instead.
fn flat_members<'a>(app: &'a App, island: &Island, selected: Option<&str>, per_row: usize) -> Element<'a, Message> {
    let cells: Vec<Element<Message>> = island.members.iter()
        .map(|m| real_card(app, &m.id, m.status, false, selected == Some(m.id.as_str())))
        .collect();
    container(column(wrap_rows(cells, per_row)).spacing(ROW_GAP)).center_x(Length::Fill).into()
}

fn breadcrumb<'a>(app: &'a App, island: &Island) -> Element<'a, Message> {
    let active = island.summary.active();
    let done = island.summary.done;
    let stopped = island.summary.stopped;

    row![
        button(text("Fleet").size(12).font(crate::style::SANS).color(tokens::VIOLET_BADGE_FG))
            .on_press(Message::FleetCloseIsland)
            .padding(0)
            .style(|_t, _status| button::Style {
                background: None,
                text_color: tokens::VIOLET_BADGE_FG,
                border: Border::default(),
                ..Default::default()
            }),
        text("›").size(12).color(tokens::DIM2),
        text(truncate_chars(&island.name, 40)).size(12).font(crate::style::SANS).color(tokens::TEXT),
        text(format!("· {active} active · {done} done · {stopped} stopped")).size(11).color(tokens::DIM2),
        Space::new(10, 0),
        key_hint("esc"),
        text("back to fleet").size(10.5).color(tokens::DIM2),
        Space::new(Length::Fill, 0),
        seg_control(vec![
            ("Full cards".to_string(), !app.fleet_compact, Message::FleetSetCompact(false)),
            ("Compact".to_string(), app.fleet_compact, Message::FleetSetCompact(true)),
        ]),
    ]
    .align_y(Alignment::Center)
    .spacing(8)
    .padding(Padding { top: 9.0, right: 18.0, bottom: 9.0, left: 18.0 })
    .into()
}

/// `edges`: this island's hierarchy edges, from `nodes_for_island` —
/// accepted as a param (computed once by the caller) rather than called
/// again here, since `shell::shell` needs the exact same edges for the
/// detail panel's breadcrumb and would otherwise redo the same per-member
/// work a second time every frame (code-review finding).
pub fn fleet_focus<'a>(app: &'a App, islands: &[Island], key: &IslandKey, edges: &[OverviewEdge]) -> Element<'a, Message> {
    let Some(island) = islands.iter().find(|i| &i.key == key) else {
        // The island vanished between render and this frame (e.g. its last
        // session just ended) — fall back to the grid rather than panic or
        // render an empty canvas.
        return super::fleet_islands::fleet_islands(app, islands);
    };

    let orchestrator_ids: HashSet<&str> = app.orchestrators.iter().map(|o| o.id.as_str()).collect();
    let status_by_id: HashMap<&str, MemberStatus> = island.members.iter().map(|m| (m.id.as_str(), m.status)).collect();
    let expanded = fold_expanded(app, key);
    let per_row = cards_per_row(app, islands);
    let selected = app.fleet_nav.selected.as_deref();

    let graph: Element<Message> = match &island.root {
        Some(root_id) => subtree(app, root_id, edges, &orchestrator_ids, &status_by_id, key, expanded, selected, per_row, true),
        None => flat_members(app, island, selected, per_row),
    };

    let island_orchestrators: Vec<&str> = island.members.iter().map(|m| m.id.as_str()).filter(|id| orchestrator_ids.contains(id)).collect();
    let graph: Element<Message> = match super::ghost_plans::ghosts_for_island(app, &island_orchestrators) {
        Some(ghosts) => column![graph, ghosts].spacing(28).width(Length::Fill).into(),
        None => graph,
    };

    let canvas = scrollable(container(graph).width(Length::Fill).padding(Padding { top: 24.0, right: 18.0, bottom: 24.0, left: 18.0 }))
        .direction(scrollable::Direction::Vertical(scrollable::Scrollbar::new().width(4).scroller_width(4)))
        .width(Length::Fill)
        .height(Length::Fill);

    container(
        column![
            container(breadcrumb(app, island)).width(Length::Fill).style(|_| container::Style {
                background: Some(Background::Color(tokens::BG)),
                border: Border { color: tokens::BORDER, width: 0.0, radius: 0.0.into() },
                ..Default::default()
            }),
            container(canvas).width(Length::Fill).height(Length::Fill).style(|_| container::Style {
                background: Some(Background::Color(tokens::BG)),
                border: Border { color: tokens::BORDER, width: 0.0, radius: 0.0.into() },
                ..Default::default()
            }),
        ]
        .width(Length::Fill)
        .height(Length::Fill),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_chars_clips_long_names_whole() {
        let name = "bottlerocket-selinux-mount-propagation-test";
        let clipped = truncate_chars(name, 10);
        assert_eq!(clipped.chars().count(), 11);
        assert!(clipped.ends_with('…'));
    }

    #[test]
    fn truncate_chars_leaves_short_names_alone() {
        assert_eq!(truncate_chars("short", 10), "short");
    }

    #[test]
    fn per_row_for_fits_at_least_one_card_even_when_narrower_than_one() {
        assert_eq!(per_row_for(10.0), 1);
    }

    #[test]
    fn per_row_for_fits_exactly_as_many_as_the_width_allows() {
        // Four cards plus their inter-card gaps, no more.
        let width = 4.0 * CARD_WIDTH + 3.0 * CARD_GAP;
        assert_eq!(per_row_for(width), 4);
        assert_eq!(per_row_for(width - 1.0), 3);
    }

    fn placeholder_cells(n: usize) -> Vec<Element<'static, Message>> {
        (0..n).map(|_| Space::new(0, 0).into()).collect()
    }

    #[test]
    fn wrap_rows_chunks_into_full_rows_plus_a_partial_remainder() {
        let rows = wrap_rows(placeholder_cells(7), 3);
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn wrap_rows_fits_everything_on_one_row_when_per_row_is_not_exceeded() {
        let rows = wrap_rows(placeholder_cells(3), 5);
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn wrap_rows_treats_a_zero_per_row_as_one() {
        let rows = wrap_rows(placeholder_cells(2), 0);
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn trunk_span_is_zero_for_zero_or_one_child() {
        assert_eq!(trunk_span(0), 0.0);
        assert_eq!(trunk_span(1), 0.0);
    }

    #[test]
    fn trunk_span_covers_center_to_center_of_the_first_and_last_child_only() {
        // Three children: the span is two center-to-center hops, each one
        // card width plus one gap — not the full row width (which would
        // overshoot past the last child's own center tick).
        assert_eq!(trunk_span(3), 2.0 * (CARD_WIDTH + CARD_GAP));
    }

    #[test]
    fn group_rows_matches_wrap_rows_row_count() {
        let cells = placeholder_cells(7);
        let groups = group_rows(cells, 3);
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].len(), 3);
        assert_eq!(groups[2].len(), 1);
    }

    #[test]
    fn nested_children_rows_block_does_not_force_fill_width() {
        // A nested sub-orchestrator's own fanned children must stay
        // natural-width (Shrink-enclosed, not Fill) — otherwise, sitting
        // inside the parent's `row(group)` cell, it swallows its siblings'
        // space and the parent row_trunk's per-child ticks no longer land
        // over their cards (code-review finding on #3b's first version).
        let block = children_rows_block(placeholder_cells(2), 3, false);
        assert_ne!(block.as_widget().size_hint().width, Length::Fill);
    }

    #[test]
    fn top_level_children_rows_block_is_fill_width() {
        // The island root's own trunk still spans/centers across the full
        // canvas width, same as before this fix.
        let block = children_rows_block(placeholder_cells(2), 3, true);
        assert_eq!(block.as_widget().size_hint().width, Length::Fill);
    }

    #[test]
    fn child_trunk_shape_is_single_only_for_exactly_one_child() {
        assert_eq!(child_trunk_shape(1), ChildTrunk::Single);
        assert_eq!(child_trunk_shape(2), ChildTrunk::Fanned);
        assert_eq!(child_trunk_shape(5), ChildTrunk::Fanned);
    }

    #[test]
    fn children_of_only_follows_hierarchy_edges_from_the_given_parent() {
        let nodes = vec![];
        let edges = vec![
            OverviewEdge::new(EdgeKind::Hierarchy, "root".into(), "a".into(), &nodes).unwrap(),
            OverviewEdge::new(EdgeKind::Hierarchy, "root".into(), "b".into(), &nodes).unwrap(),
            OverviewEdge::new(EdgeKind::Hierarchy, "a".into(), "c".into(), &nodes).unwrap(),
        ];
        let mut kids = children_of(&edges, "root");
        kids.sort();
        assert_eq!(kids, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(children_of(&edges, "a"), vec!["c".to_string()]);
        assert_eq!(children_of(&edges, "c"), Vec::<String>::new());
    }
}
