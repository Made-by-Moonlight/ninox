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
/// `view()` call; holds no mutable state of its own (`State = ()`).
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
    type State = ();

    fn draw(
        &self,
        _state: &(),
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
        }

        vec![frame.into_geometry()]
    }

    fn update(
        &self,
        _state: &mut (),
        event: canvas::Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> (canvas::event::Status, Option<Message>) {
        if let canvas::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) = event {
            if let Some(pos) = cursor.position_in(bounds) {
                let local_bounds = Rectangle { x: 0.0, y: 0.0, ..bounds };
                let nodes = self.nodes(local_bounds);
                if let Some(n) = nodes.iter().min_by(|a, b| {
                    let da = (a.x - pos.x).hypot(a.y - pos.y);
                    let db = (b.x - pos.x).hypot(b.y - pos.y);
                    da.partial_cmp(&db).unwrap()
                }) {
                    if (n.x - pos.x).hypot(n.y - pos.y) < n.r + 6.0 {
                        return (
                            canvas::event::Status::Captured,
                            Some(Message::BrainSelectEntry(n.id.clone())),
                        );
                    }
                }
            }
        }
        (canvas::event::Status::Ignored, None)
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
