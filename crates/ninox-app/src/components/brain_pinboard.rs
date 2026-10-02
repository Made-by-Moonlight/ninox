//! Brain pinboard specimen board — a canvas of wikilink-connected nodes
//! (spec §IV "Pinboard"). Node positions live in `app.brain_view.layout`
//! (the shared `ForceLayout`, ticked by physics and pinned while dragging)
//! as normalized `[0,1]^2` coordinates, scaled against the canvas bounds on
//! every draw rather than stored in canvas-space — so resizing the window
//! can never leave stale/clipped node positions behind.

use std::collections::HashMap;

use iced::widget::canvas::{self, Canvas, Frame, Geometry, Path, Stroke};
use iced::{mouse, Color, Element, Length, Point, Rectangle, Renderer, Theme};
use ninox_core::BrainEntry;

use super::brain_panel::category_color;
use crate::app::{App, Message};

/// A drawn specimen dot: canvas-space position, radius, category color, and
/// whether it matches the active search filter.
struct Node {
    x: f32,
    y: f32,
    r: f32,
    color: Color,
    hit: bool,
    id: String,
}

/// Deterministic FNV-1a-derived hash mapped into `[0, 1)`.
///
/// No RNG/clock involved: the same entry id (+ salt) always yields the same
/// value, so the same set of brain entries always lays out identically
/// across frames, window resizes, and app restarts.
pub(crate) fn hash01(s: &str, salt: u64) -> f32 {
    let mut h: u64 = 1469598103934665603 ^ salt;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    ((h >> 11) as f64 / (1u64 << 53) as f64) as f32
}

/// The pinboard canvas program. Borrows `App` for the duration of a single
/// `view()` call; node positions live in `app.brain_view.layout` (a
/// `ForceLayout` advanced by `Message::BrainPhysicsTick`) as normalized
/// coordinates, scaled to `bounds` at draw time. Its own mutable state is
/// [`PinboardState`] — the hover selection plus the press/drag machine.
pub struct Pinboard<'a> {
    pub app: &'a App,
}

impl<'a> Pinboard<'a> {
    /// Lay out one [`Node`] per brain entry within `bounds`, sized by node
    /// degree (in `self.app.brain_view.edges`, resolved once per data change
    /// — see `App::refresh_brain_edges` — never re-derived here) and flagged
    /// `hit` when the entry matches the active search filter.
    fn nodes(&self, bounds: Rectangle) -> Vec<Node> {
        let s = &self.app.scheme;
        let q = self.app.brain_view.filter.to_lowercase();

        let mut degree = vec![0u32; self.app.brain_view.entries.len()];
        for &(a, b) in &self.app.brain_view.edges {
            if let Some(d) = degree.get_mut(a) {
                *d += 1;
            }
            if let Some(d) = degree.get_mut(b) {
                *d += 1;
            }
        }

        self.app
            .brain_view
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let (nx, ny) = self
                    .app
                    .brain_view
                    .layout
                    .position(&e.id)
                    .unwrap_or((0.05 + 0.90 * hash01(&e.id, 7), 0.06 + 0.88 * hash01(&e.id, 13)));
                Node {
                    x: bounds.width * nx,
                    y: bounds.height * ny,
                    r: node_radius(degree.get(i).copied().unwrap_or(0) as f32),
                    color: category_color(s, &e.entry_type),
                    hit: !q.is_empty()
                        && (e.name.to_lowercase().contains(&q) || e.id.to_lowercase().contains(&q)),
                    id: e.id.clone(),
                }
            })
            .collect()
    }
}

/// The pinboard canvas `Program`'s interaction state. `draw()` does not
/// read any of it: the hover and selection rings render from the shared
/// `app.brain_view.hovered`/`.selected` so the sidebar and the canvas
/// cross-highlight each other.
#[derive(Default)]
pub struct PinboardState {
    /// Last hover this canvas emitted a `BrainHoverEntry` for — read by
    /// `mouse_interaction()` (pointer cursor) and by `handle_mouse_event`'s
    /// dedupe guard, so a hover that moves within the same node doesn't
    /// re-emit.
    hovered: Option<String>,
    /// Candidate node + press-origin (local canvas coords), recorded on
    /// `ButtonPressed`, before it's known whether this press resolves to a
    /// click (select) or a drag.
    press: Option<(String, Point)>,
    /// Set once a press has moved past `DRAG_THRESHOLD` — the id currently
    /// being dragged. Mirrored into `App.brain_view.dragging` via
    /// `BrainDragStart`/`BrainDragEnd` so the physics tick knows to pin it.
    dragging: Option<String>,
}

/// Minimum cursor movement (local canvas pixels) from the press origin
/// before a press upgrades from "candidate click" to "drag".
const DRAG_THRESHOLD: f32 = 4.0;

/// Nearest node to `pos`, if within `r + 6` of it — the shared hit-test
/// tolerance for the press/click-to-select path (`ButtonPressed` records
/// the candidate, `ButtonReleased` resolves it to a selection) and for
/// hover-preview detection (`CursorMoved`), so hovering and clicking
/// always agree on which node is "under" the cursor.
fn hit_test(nodes: &[Node], pos: Point) -> Option<String> {
    nodes
        .iter()
        .min_by(|a, b| {
            let da = (a.x - pos.x).hypot(a.y - pos.y);
            let db = (b.x - pos.x).hypot(b.y - pos.y);
            da.partial_cmp(&db).unwrap()
        })
        .filter(|n| (n.x - pos.x).hypot(n.y - pos.y) < n.r + 6.0)
        .map(|n| n.id.clone())
}

/// Undirected dedup key for the edge between node indices `a` and `b`:
/// order-independent, so a mutual pair of wikilinks (A -> B and B -> A)
/// collapses to a single key instead of being stroked twice.
fn edge_key(a: usize, b: usize) -> (usize, usize) {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

/// Resolve `BrainIndex::links_all()`'s `(from_id, to_id)` string pairs into
/// deduplicated, undirected node-index pairs against `entries`'s current
/// order — the form the pinboard canvas draws from. Called once per data
/// change (`App::refresh_brain_edges`, on `NavigateBrain` / `BrainReindex` /
/// `BrainSwitchCatalogue`), never per draw.
///
/// A link endpoint that isn't in `entries` (index/entries drift, or a link
/// to an id that no longer exists) is skipped rather than panicking; a
/// mutual pair (A -> B and B -> A) collapses to one edge via [`edge_key`].
pub(crate) fn resolve_edges(entries: &[BrainEntry], links: &[(String, String)]) -> Vec<(usize, usize)> {
    let index: HashMap<&str, usize> =
        entries.iter().enumerate().map(|(i, e)| (e.id.as_str(), i)).collect();
    let mut seen: std::collections::HashSet<(usize, usize)> = Default::default();
    let mut edges = Vec::new();
    for (from, to) in links {
        let (Some(&a), Some(&b)) = (index.get(from.as_str()), index.get(to.as_str())) else {
            continue;
        };
        if a == b {
            continue;
        }
        let key = edge_key(a, b);
        if seen.insert(key) {
            edges.push(key);
        }
    }
    edges
}

/// Specimen dot radius: a 3px floor, growing with wikilink count, clamped to
/// a 9px ceiling so heavily-linked notes don't overwhelm the board.
fn node_radius(link_count: f32) -> f32 {
    3.0 + (link_count * 1.2).min(6.0)
}

/// The press/drag/hover state machine, factored out of `Program::update` so
/// it's testable without a real `App`. `hit` is whichever node (if any) is
/// under the cursor for this event, already resolved via `hit_test`; `pos`
/// is the cursor's local-canvas position (`None` once it's left this
/// canvas's own bounds, matching `cursor.position_in`); `bounds` is used
/// only to normalize a drag's emitted position into `[0,1]^2`.
fn handle_mouse_event(
    state: &mut PinboardState,
    event: mouse::Event,
    pos: Option<Point>,
    hit: Option<String>,
    bounds: Rectangle,
) -> (canvas::event::Status, Option<Message>) {
    match event {
        mouse::Event::ButtonPressed(mouse::Button::Left) => {
            if let (Some(pos), Some(id)) = (pos, hit) {
                state.press = Some((id, pos));
            }
            (canvas::event::Status::Ignored, None)
        }
        mouse::Event::ButtonReleased(mouse::Button::Left) => {
            if state.dragging.take().is_some() {
                return (canvas::event::Status::Captured, Some(Message::BrainDragEnd));
            }
            if let Some((id, _)) = state.press.take() {
                return (canvas::event::Status::Captured, Some(Message::BrainSelectEntry(id)));
            }
            (canvas::event::Status::Ignored, None)
        }
        mouse::Event::CursorMoved { .. } | mouse::Event::CursorLeft => {
            if let Some(pos) = pos {
                if let Some(dragging) = state.dragging.clone() {
                    let nx = (pos.x / bounds.width).clamp(0.0, 1.0);
                    let ny = (pos.y / bounds.height).clamp(0.0, 1.0);
                    return (canvas::event::Status::Captured, Some(Message::BrainDragMove(dragging, nx, ny)));
                }
                if let Some((id, origin)) = state.press.clone() {
                    if (pos.x - origin.x).hypot(pos.y - origin.y) > DRAG_THRESHOLD {
                        state.press = None;
                        state.dragging = Some(id.clone());
                        return (canvas::event::Status::Captured, Some(Message::BrainDragStart(id)));
                    }
                }
            }

            // Written only on the path that also emits the matching
            // message: the branches above return without one, so recording
            // the hover there would leave `app.brain_view.hovered` behind
            // forever — the dedupe guard would never see it change back.
            if hit != state.hovered {
                state.hovered = hit.clone();
                return (canvas::event::Status::Ignored, Some(Message::BrainHoverEntry(hit)));
            }
            (canvas::event::Status::Ignored, None)
        }
        _ => (canvas::event::Status::Ignored, None),
    }
}

impl<'a> canvas::Program<Message> for Pinboard<'a> {
    type State = PinboardState;

    fn draw(
        &self,
        _state: &PinboardState,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let s = &self.app.scheme;
        let mut frame = Frame::new(renderer, bounds.size());
        // Node coordinates are frame-local (origin at 0,0), matching the
        // Frame's own coordinate space — not the window-relative `bounds`.
        let local_bounds = Rectangle { x: 0.0, y: 0.0, ..bounds };
        let nodes = self.nodes(local_bounds);

        // Dashed link threads: faint ink by default, lit accent when either
        // endpoint matches the active search. Edges are precomputed
        // node-index pairs (`App::refresh_brain_edges` /
        // `resolve_edges`) — already deduped by undirected pair, so a
        // mutual link (A -> B and B -> A) is stroked once, not twice.
        let ink_edge = Color { a: 0.18, ..s.ink };
        for &(a, b) in &self.app.brain_view.edges {
            let (Some(na), Some(nb)) = (nodes.get(a), nodes.get(b)) else { continue };
            let lit = na.hit || nb.hit;
            frame.stroke(
                &Path::line(Point::new(na.x, na.y), Point::new(nb.x, nb.y)),
                Stroke {
                    style: canvas::Style::Solid(if lit {
                        Color { a: 0.55, ..s.accent }
                    } else {
                        ink_edge
                    }),
                    width: 1.0,
                    line_dash: canvas::LineDash { segments: &[3.0, 3.0], offset: 0 },
                    ..Stroke::default()
                },
            );
        }

        // Ink-outlined, category-colored specimen dots; search hits get a
        // +4px accent ring.
        for n in &nodes {
            let dot = Path::circle(Point::new(n.x, n.y), n.r);
            frame.fill(&dot, n.color);
            frame.stroke(
                &dot,
                Stroke::default().with_color(Color { a: 0.75, ..s.ink }).with_width(1.2),
            );
            if n.hit {
                frame.stroke(
                    &Path::circle(Point::new(n.x, n.y), n.r + 4.0),
                    Stroke::default().with_color(s.accent).with_width(1.2),
                );
            }
            // Selection ring: thicker, always-on accent ring, matching the
            // sidebar's selected-row accent bar (`dentry_row`) — shared
            // source of truth (`app.brain_view.selected`), not local mouse
            // state.
            if self.app.brain_view.selected.as_deref() == Some(n.id.as_str()) {
                frame.stroke(
                    &Path::circle(Point::new(n.x, n.y), n.r + 5.0),
                    Stroke::default().with_color(s.accent).with_width(2.0),
                );
            }
            // Hover ring: sourced from `app.brain_view.hovered`, shared with
            // the sidebar's cross-highlight (`dentry_row`'s `is_cross_hovered`)
            // — a sidebar-row hover rings the matching node here too, not
            // just this canvas's own mouse movement.
            if self.app.brain_view.hovered.as_deref() == Some(n.id.as_str()) {
                frame.stroke(
                    &Path::circle(Point::new(n.x, n.y), n.r + 3.0),
                    Stroke::default().with_color(Color { a: 1.0, ..s.ink }).with_width(1.4),
                );
            }
        }

        vec![frame.into_geometry()]
    }

    fn update(
        &self,
        state: &mut PinboardState,
        event: canvas::Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> (canvas::event::Status, Option<Message>) {
        let canvas::Event::Mouse(mouse_event) = event else {
            return (canvas::event::Status::Ignored, None);
        };
        let local_bounds = Rectangle { x: 0.0, y: 0.0, ..bounds };
        let nodes = self.nodes(local_bounds);
        let pos = cursor.position_in(bounds);
        let hit = pos.and_then(|p| hit_test(&nodes, p));
        handle_mouse_event(state, mouse_event, pos, hit, bounds)
    }

    fn mouse_interaction(
        &self,
        state: &PinboardState,
        _bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        if state.hovered.is_some() {
            mouse::Interaction::Pointer
        } else {
            mouse::Interaction::default()
        }
    }
}

/// The pinboard canvas widget — fills its container on both axes so window
/// resizes never clip or misalign the board (`brain_view.layout` stores
/// normalized positions, scaled against bounds on every draw, never cached
/// in absolute coordinates).
pub fn pinboard_canvas(app: &App) -> Element<'_, Message> {
    Canvas::new(Pinboard { app }).width(Length::Fill).height(Length::Fill).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash01_is_in_unit_range() {
        for s in ["a", "concepts/frame-alignment.md", "", "errors/x"] {
            for salt in [7, 13] {
                let v = hash01(s, salt);
                assert!((0.0..1.0).contains(&v), "hash01({s:?}, {salt}) = {v} out of range");
            }
        }
    }

    #[test]
    fn hash01_is_stable_across_calls() {
        assert_eq!(hash01("symbols/scrollback-buffer.md", 7), hash01("symbols/scrollback-buffer.md", 7));
    }

    #[test]
    fn hash01_differs_by_salt_for_typical_inputs() {
        // Not a strict guarantee for all inputs, but should hold for a real id.
        assert_ne!(hash01("concepts/frame-alignment.md", 7), hash01("concepts/frame-alignment.md", 13));
    }

    #[test]
    fn node_radius_has_floor_and_ceiling() {
        assert_eq!(node_radius(0.0), 3.0);
        assert_eq!(node_radius(100.0), 9.0);
        assert!(node_radius(2.0) > 3.0 && node_radius(2.0) < 9.0);
    }

    #[test]
    fn hit_test_picks_nearest_node_within_tolerance() {
        let nodes = vec![
            Node { x: 10.0, y: 10.0, r: 4.0, color: Color::BLACK, hit: false, id: "a".into() },
            Node { x: 100.0, y: 100.0, r: 4.0, color: Color::BLACK, hit: false, id: "b".into() },
        ];
        assert_eq!(hit_test(&nodes, Point::new(11.0, 11.0)), Some("a".to_string()));
        assert_eq!(hit_test(&nodes, Point::new(99.0, 101.0)), Some("b".to_string()));
    }

    #[test]
    fn hit_test_returns_none_outside_tolerance() {
        let nodes =
            vec![Node { x: 10.0, y: 10.0, r: 4.0, color: Color::BLACK, hit: false, id: "a".into() }];
        // r + 6 == 10, so a point 11px away misses.
        assert_eq!(hit_test(&nodes, Point::new(21.0, 10.0)), None);
    }

    #[test]
    fn edge_key_is_order_independent() {
        assert_eq!(edge_key(2, 5), edge_key(5, 2));
        assert_eq!(edge_key(2, 5), (2, 5));
        assert_eq!(edge_key(0, 0), (0, 0));
    }

    fn bounds_100() -> Rectangle {
        Rectangle { x: 0.0, y: 0.0, width: 100.0, height: 100.0 }
    }

    #[test]
    fn click_without_movement_selects() {
        let mut state = PinboardState::default();
        let bounds = bounds_100();
        let (_, msg) = handle_mouse_event(
            &mut state,
            mouse::Event::ButtonPressed(mouse::Button::Left),
            Some(Point::new(10.0, 10.0)),
            Some("a.md".to_string()),
            bounds,
        );
        assert!(msg.is_none());
        assert!(matches!(&state.press, Some((id, _)) if id == "a.md"));

        let (_, msg) = handle_mouse_event(
            &mut state,
            mouse::Event::ButtonReleased(mouse::Button::Left),
            Some(Point::new(11.0, 10.0)),
            Some("a.md".to_string()),
            bounds,
        );
        assert!(matches!(msg, Some(Message::BrainSelectEntry(id)) if id == "a.md"));
        assert!(state.press.is_none());
        assert!(state.dragging.is_none());
    }

    #[test]
    fn press_move_past_threshold_starts_a_drag_not_a_select() {
        let mut state = PinboardState::default();
        let bounds = bounds_100();
        handle_mouse_event(
            &mut state,
            mouse::Event::ButtonPressed(mouse::Button::Left),
            Some(Point::new(10.0, 10.0)),
            Some("a.md".to_string()),
            bounds,
        );

        let (_, msg) = handle_mouse_event(
            &mut state,
            mouse::Event::CursorMoved { position: Point::new(30.0, 10.0) },
            Some(Point::new(30.0, 10.0)),
            Some("a.md".to_string()),
            bounds,
        );
        assert!(matches!(msg, Some(Message::BrainDragStart(id)) if id == "a.md"));
        assert_eq!(state.dragging.as_deref(), Some("a.md"));
        assert!(state.press.is_none());

        let (_, msg) = handle_mouse_event(
            &mut state,
            mouse::Event::ButtonReleased(mouse::Button::Left),
            Some(Point::new(30.0, 10.0)),
            Some("a.md".to_string()),
            bounds,
        );
        assert!(matches!(msg, Some(Message::BrainDragEnd)));
        assert!(state.dragging.is_none());
    }

    #[test]
    fn dragging_emits_normalized_position() {
        let mut state = PinboardState { dragging: Some("a.md".to_string()), ..Default::default() };
        let bounds = bounds_100();
        let (_, msg) = handle_mouse_event(
            &mut state,
            mouse::Event::CursorMoved { position: Point::new(25.0, 75.0) },
            Some(Point::new(25.0, 75.0)),
            None,
            bounds,
        );
        assert!(matches!(
            msg,
            Some(Message::BrainDragMove(id, x, y))
                if id == "a.md" && (x - 0.25).abs() < 1e-6 && (y - 0.75).abs() < 1e-6
        ));
    }

    /// Mid-drag the dragged node's rendered position lags the cursor, so
    /// the hit test can land on some *other* node. That must not quietly
    /// advance `state.hovered`: no `BrainHoverEntry` goes out on this path,
    /// and a silently-advanced guard would swallow the next real hover
    /// change, desyncing `app.brain_view.hovered` for good.
    #[test]
    fn dragging_does_not_swallow_a_hover_change() {
        let mut state = PinboardState {
            dragging: Some("a.md".to_string()),
            hovered: Some("a.md".to_string()),
            ..Default::default()
        };
        let bounds = bounds_100();
        let (_, msg) = handle_mouse_event(
            &mut state,
            mouse::Event::CursorMoved { position: Point::new(25.0, 75.0) },
            Some(Point::new(25.0, 75.0)),
            Some("b.md".to_string()),
            bounds,
        );
        assert!(matches!(msg, Some(Message::BrainDragMove(id, _, _)) if id == "a.md"));
        assert_eq!(state.hovered.as_deref(), Some("a.md"));

        // Drag over: the still-pending change is detected and emitted now.
        state.dragging = None;
        let (_, msg) = handle_mouse_event(
            &mut state,
            mouse::Event::CursorMoved { position: Point::new(25.0, 75.0) },
            Some(Point::new(25.0, 75.0)),
            Some("b.md".to_string()),
            bounds,
        );
        assert!(matches!(msg, Some(Message::BrainHoverEntry(Some(id))) if id == "b.md"));
        assert_eq!(state.hovered.as_deref(), Some("b.md"));
    }

    #[test]
    fn hover_still_fires_when_not_pressing() {
        let mut state = PinboardState::default();
        let bounds = bounds_100();
        let (_, msg) = handle_mouse_event(
            &mut state,
            mouse::Event::CursorMoved { position: Point::new(10.0, 10.0) },
            Some(Point::new(10.0, 10.0)),
            Some("a.md".to_string()),
            bounds,
        );
        assert!(matches!(msg, Some(Message::BrainHoverEntry(Some(id))) if id == "a.md"));
    }

    fn entry(id: &str) -> BrainEntry {
        BrainEntry {
            id: id.to_string(),
            entry_type: "concepts".to_string(),
            name: id.to_string(),
            tags: vec![],
            repos: vec![],
            updated: None,
            body: String::new(),
        }
    }

    /// A mutual pair of resolved edges (A -> B and B -> A, as `links_all()`
    /// would return for two entries that wikilink each other) must collapse
    /// to exactly one undirected node-index pair, not two.
    #[test]
    fn resolve_edges_dedups_mutual_links() {
        let entries = vec![entry("a.md"), entry("b.md")];
        let links = vec![
            ("a.md".to_string(), "b.md".to_string()),
            ("b.md".to_string(), "a.md".to_string()),
        ];
        let edges = resolve_edges(&entries, &links);
        assert_eq!(edges, vec![(0, 1)]);
    }

    /// A link endpoint that isn't in `entries` (stale id, index/entries
    /// drift) is skipped, not a panic.
    #[test]
    fn resolve_edges_skips_unresolvable_ids() {
        let entries = vec![entry("a.md"), entry("b.md")];
        let links = vec![
            ("a.md".to_string(), "b.md".to_string()),
            ("a.md".to_string(), "missing.md".to_string()),
            ("missing.md".to_string(), "b.md".to_string()),
        ];
        let edges = resolve_edges(&entries, &links);
        assert_eq!(edges, vec![(0, 1)]);
    }

    /// A (degenerate) self-link is dropped rather than producing a (n, n)
    /// self-edge.
    #[test]
    fn resolve_edges_drops_self_links() {
        let entries = vec![entry("a.md")];
        let links = vec![("a.md".to_string(), "a.md".to_string())];
        assert!(resolve_edges(&entries, &links).is_empty());
    }
}
