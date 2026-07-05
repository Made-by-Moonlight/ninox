//! Brain pinboard specimen board — a canvas of wikilink-connected nodes
//! (spec §IV "Pinboard"). Positions are re-derived from the canvas bounds on
//! every draw (deterministic, hash-based) rather than stored in `State`, so
//! resizing the window can never leave stale/clipped node positions behind.

use std::collections::HashMap;

use iced::widget::canvas::{self, Canvas, Frame, Geometry, Path, Stroke};
use iced::{mouse, Color, Element, Length, Point, Rectangle, Renderer, Theme};

use super::brain_panel::{category_color, extract_wikilinks, resolve_link};
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
fn hash01(s: &str, salt: u64) -> f32 {
    let mut h: u64 = 1469598103934665603 ^ salt;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    ((h >> 11) as f64 / (1u64 << 53) as f64) as f32
}

/// The pinboard canvas program. Borrows `App` for the duration of a single
/// `view()` call; its only mutable state is [`PinboardState`]'s hover
/// selection (node positions are still re-derived from bounds every frame,
/// never cached).
pub struct Pinboard<'a> {
    pub app: &'a App,
}

impl<'a> Pinboard<'a> {
    /// Lay out one [`Node`] per brain entry within `bounds`, sized by
    /// outgoing wikilink count and flagged `hit` when the entry matches the
    /// active search filter. Also returns each entry's parsed outgoing
    /// wikilinks (same order as `nodes`/`self.app.brain_view.entries`) so
    /// callers that also need the edges don't re-parse the body a second
    /// time per draw.
    fn nodes_and_links(&self, bounds: Rectangle) -> (Vec<Node>, Vec<Vec<String>>) {
        let s = &self.app.scheme;
        let q = self.app.brain_view.filter.to_lowercase();
        self.app
            .brain_view
            .entries
            .iter()
            .map(|e| {
                let links = extract_wikilinks(&e.body);
                let node = Node {
                    x: bounds.width * (0.05 + 0.90 * hash01(&e.id, 7)),
                    y: bounds.height * (0.06 + 0.88 * hash01(&e.id, 13)),
                    r: node_radius(links.len() as f32),
                    color: category_color(s, &e.entry_type),
                    hit: !q.is_empty()
                        && (e.name.to_lowercase().contains(&q) || e.id.to_lowercase().contains(&q)),
                    id: e.id.clone(),
                };
                (node, links)
            })
            .unzip()
    }

    /// Lay out nodes only, discarding the parsed links (used where only
    /// positions/radii are needed, e.g. click hit-testing).
    fn nodes(&self, bounds: Rectangle) -> Vec<Node> {
        self.nodes_and_links(bounds).0
    }
}

/// The pinboard canvas `Program`'s interaction state: which node (if any)
/// the cursor is currently hovering. Mutated in `update()` and read back in
/// `draw()` (to render the hover ring) and `mouse_interaction()` (to switch
/// to a pointer cursor) — node positions themselves stay re-derived from
/// bounds every frame (see module docs), only the hover *selection* persists.
#[derive(Default)]
pub struct PinboardState {
    hovered: Option<String>,
}

/// Nearest node to `pos`, if within `r + 6` of it — the shared hit-test
/// tolerance for both click-to-select (`update`'s `ButtonPressed`) and
/// hover-preview detection (`update`'s `CursorMoved`), so hovering and
/// clicking always agree on which node is "under" the cursor.
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

/// Specimen dot radius: a 3px floor, growing with wikilink count, clamped to
/// a 9px ceiling so heavily-linked notes don't overwhelm the board.
fn node_radius(link_count: f32) -> f32 {
    3.0 + (link_count * 1.2).min(6.0)
}

impl<'a> canvas::Program<Message> for Pinboard<'a> {
    type State = PinboardState;

    fn draw(
        &self,
        state: &PinboardState,
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
        let (nodes, links) = self.nodes_and_links(local_bounds);

        // Dashed wikilink threads: faint ink by default, lit accent when
        // either endpoint matches the active search. Deduped by undirected
        // node-pair so a mutual link (A -> B and B -> A) is stroked once,
        // not twice — the "lit" state is direction-independent (either
        // endpoint hitting is enough), so dedup can't drop it.
        let ink_edge = Color { a: 0.18, ..s.ink };
        let by_id: HashMap<&str, usize> =
            nodes.iter().enumerate().map(|(i, n)| (n.id.as_str(), i)).collect();
        let mut drawn_edges: std::collections::HashSet<(usize, usize)> = Default::default();
        for (e, entry_links) in self.app.brain_view.entries.iter().zip(&links) {
            let Some(&a) = by_id.get(e.id.as_str()) else { continue };
            for link in entry_links {
                let Some(target) = resolve_link(&self.app.brain_view.entries, link) else { continue };
                let Some(&b) = by_id.get(target.id.as_str()) else { continue };
                if !drawn_edges.insert(edge_key(a, b)) {
                    continue;
                }
                let lit = nodes[a].hit || nodes[b].hit;
                frame.stroke(
                    &Path::line(
                        Point::new(nodes[a].x, nodes[a].y),
                        Point::new(nodes[b].x, nodes[b].y),
                    ),
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
            // Hover ring: full-alpha ink, +3px — deliberately narrower and a
            // different color from the +4px vermilion search-hit ring above
            // so the two states never read as the same thing.
            if state.hovered.as_deref() == Some(n.id.as_str()) {
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
        match event {
            canvas::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                if let Some(pos) = cursor.position_in(bounds) {
                    let local_bounds = Rectangle { x: 0.0, y: 0.0, ..bounds };
                    let nodes = self.nodes(local_bounds);
                    if let Some(id) = hit_test(&nodes, pos) {
                        return (canvas::event::Status::Captured, Some(Message::BrainSelectEntry(id)));
                    }
                }
            }
            // `CursorLeft` fires when the cursor exits the *window*, which
            // `position_in` alone wouldn't catch; folded into the same
            // handling as `CursorMoved` (whose `position_in` already goes
            // `None` once the cursor leaves just this canvas's bounds) so
            // both paths converge on one hover-state update.
            canvas::Event::Mouse(mouse::Event::CursorMoved { .. } | mouse::Event::CursorLeft) => {
                let local_bounds = Rectangle { x: 0.0, y: 0.0, ..bounds };
                let nodes = self.nodes(local_bounds);
                let hovered = cursor.position_in(bounds).and_then(|pos| hit_test(&nodes, pos));
                if hovered != state.hovered {
                    state.hovered = hovered.clone();
                    return (canvas::event::Status::Ignored, Some(Message::BrainHoverEntry(hovered)));
                }
            }
            _ => {}
        }
        (canvas::event::Status::Ignored, None)
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
/// resizes never clip or misalign the board (node positions are re-derived
/// from bounds on every draw, never cached in absolute coordinates).
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

    /// Mirrors the dedup logic in `draw`: two entries that mutually wikilink
    /// each other must collapse to exactly one edge, not two.
    #[test]
    fn mutual_wikilinks_dedup_to_one_edge() {
        use ninox_core::BrainEntry;

        let entries = vec![
            BrainEntry {
                id: "a.md".to_string(),
                entry_type: "concepts".to_string(),
                name: "a".to_string(),
                tags: vec![],
                repos: vec![],
                updated: None,
                body: "sees [[b]]".to_string(),
            },
            BrainEntry {
                id: "b.md".to_string(),
                entry_type: "concepts".to_string(),
                name: "b".to_string(),
                tags: vec![],
                repos: vec![],
                updated: None,
                body: "sees [[a]]".to_string(),
            },
        ];

        let by_id: HashMap<&str, usize> =
            entries.iter().enumerate().map(|(i, e)| (e.id.as_str(), i)).collect();

        let mut edges: std::collections::HashSet<(usize, usize)> = Default::default();
        for e in &entries {
            let Some(&a) = by_id.get(e.id.as_str()) else { continue };
            for link in extract_wikilinks(&e.body) {
                let Some(target) = resolve_link(&entries, &link) else { continue };
                let Some(&b) = by_id.get(target.id.as_str()) else { continue };
                edges.insert(edge_key(a, b));
            }
        }

        assert_eq!(edges.len(), 1, "mutual A<->B links should dedup to a single edge, got {edges:?}");
    }
}
