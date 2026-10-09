//! Pure force-directed layout simulation for the brain pinboard — no
//! `iced` dependency, so the physics is testable in isolation from
//! rendering. Positions live in normalized `[0,1] x [0,1]` space, not
//! canvas pixels, so the layout survives window resizes the same way the
//! old per-frame hash-scatter did — multiply by canvas bounds only at
//! draw time (see `brain_pinboard::Pinboard::nodes`).

use std::collections::HashMap;

use ninox_core::BrainEntry;

use super::brain_pinboard::hash01;

/// Spring rest length, as a fraction of the unit square's diagonal.
const REST_LENGTH: f32 = 0.22;
/// Hooke's-law spring constant along edges.
const SPRING_K: f32 = 3.0;
/// Coulomb-like repulsion constant between all node pairs.
const REPULSION_K: f32 = 0.0026;
/// Minimum pairwise distance used in repulsion/spring math, avoiding a
/// division singularity when two nodes land exactly on top of each other.
const MIN_DIST: f32 = 0.02;
/// Pull-to-center strength — keeps disconnected/isolated nodes on-canvas.
const CENTER_K: f32 = 0.03;
/// Velocity damping applied every step (1.0 = no damping, drifts forever).
const DAMPING: f32 = 0.82;

/// `pos`/`vel` are parallel to the `entries` slice last passed to
/// [`ForceLayout::sync_entries`], and `index` maps an entry id to its
/// offset in them. The hot `step()` loops therefore index by `usize`
/// instead of hashing a path-like `String` key six times per node pair —
/// the same O(n²) algorithm, but 46ms/tick down to 1.6ms/tick at n=1000,
/// which is what brings a 1000-entry board back inside the 16.6ms frame
/// budget the 60Hz physics subscription expects.
#[derive(Debug, Clone, Default)]
pub struct ForceLayout {
    index: HashMap<String, usize>,
    pos: Vec<(f32, f32)>,
    vel: Vec<(f32, f32)>,
}

impl ForceLayout {
    /// Re-key the layout onto `entries`'s current order: every surviving id
    /// carries its existing position/velocity over, every id new to this
    /// call is seeded via the same deterministic hash scatter the pinboard
    /// always used, and every id no longer present is dropped. Called every
    /// physics tick, so a reindex/catalogue switch can't leak stale
    /// positions or leave a removed entry's node stuck on screen forever.
    pub fn sync_entries(&mut self, entries: &[BrainEntry]) {
        let mut index = HashMap::with_capacity(entries.len());
        let mut pos = Vec::with_capacity(entries.len());
        let mut vel = Vec::with_capacity(entries.len());
        for (i, e) in entries.iter().enumerate() {
            match self.index.get(e.id.as_str()) {
                Some(&old) => {
                    pos.push(self.pos[old]);
                    vel.push(self.vel[old]);
                }
                None => {
                    pos.push((0.05 + 0.90 * hash01(&e.id, 7), 0.06 + 0.88 * hash01(&e.id, 13)));
                    vel.push((0.0, 0.0));
                }
            }
            index.insert(e.id.clone(), i);
        }
        self.index = index;
        self.pos = pos;
        self.vel = vel;
    }

    pub fn position(&self, id: &str) -> Option<(f32, f32)> {
        self.index.get(id).map(|&i| self.pos[i])
    }

    /// No-op if `id` hasn't been seeded yet (via `sync_entries`) — a drag
    /// message racing ahead of the next physics tick simply has nothing to
    /// move.
    pub fn set_position(&mut self, id: &str, new_pos: (f32, f32)) {
        if let Some(&i) = self.index.get(id) {
            self.pos[i] = (new_pos.0.clamp(0.0, 1.0), new_pos.1.clamp(0.0, 1.0));
        }
    }

    /// Advance the simulation by one step of `dt` seconds: inverse-square
    /// repulsion between every pair, Hooke's-law springs along `edges`
    /// pulling toward `REST_LENGTH`, a mild centering pull, velocity
    /// damping, then integration. `pinned`, if set, is excluded from force
    /// *integration* (a drag is driving its position directly via
    /// `set_position`) but still exerts repulsion/spring forces on every
    /// other node, exactly like any other node.
    ///
    /// `entries` must be in the same order [`Self::sync_entries`] last saw;
    /// any tail this layout hasn't been synced to yet simply sits out this
    /// step rather than panicking.
    pub fn step(&mut self, entries: &[BrainEntry], edges: &[(usize, usize)], pinned: Option<&str>, dt: f32) {
        let n = entries.len().min(self.pos.len());
        let mut force = vec![(0.0f32, 0.0f32); n];

        for i in 0..n {
            for j in (i + 1)..n {
                let (pi, pj) = (self.pos[i], self.pos[j]);
                let (dx, dy) = (pi.0 - pj.0, pi.1 - pj.1);
                let dist = dx.hypot(dy).max(MIN_DIST);
                let f = REPULSION_K / (dist * dist);
                let (fx, fy) = (dx / dist * f, dy / dist * f);
                force[i].0 += fx;
                force[i].1 += fy;
                force[j].0 -= fx;
                force[j].1 -= fy;
            }
        }

        for &(a, b) in edges {
            if a >= n || b >= n {
                continue;
            }
            let (pa, pb) = (self.pos[a], self.pos[b]);
            let (dx, dy) = (pb.0 - pa.0, pb.1 - pa.1);
            let dist = dx.hypot(dy).max(MIN_DIST);
            let f = SPRING_K * (dist - REST_LENGTH);
            let (fx, fy) = (dx / dist * f, dy / dist * f);
            force[a].0 += fx;
            force[a].1 += fy;
            force[b].0 -= fx;
            force[b].1 -= fy;
        }

        for (i, f) in force.iter_mut().enumerate() {
            let p = self.pos[i];
            f.0 += (0.5 - p.0) * CENTER_K;
            f.1 += (0.5 - p.1) * CENTER_K;
        }

        let pinned = pinned.and_then(|id| self.index.get(id).copied());
        for (i, &(fx, fy)) in force.iter().enumerate() {
            if pinned == Some(i) {
                continue;
            }
            let v = &mut self.vel[i];
            v.0 = (v.0 + fx * dt) * DAMPING;
            v.1 = (v.1 + fy * dt) * DAMPING;
            let (vx, vy) = (v.0, v.1);
            let p = &mut self.pos[i];
            p.0 = (p.0 + vx * dt).clamp(0.0, 1.0);
            p.1 = (p.1 + vy * dt).clamp(0.0, 1.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn sync_entries_seeds_new_ids_deterministically() {
        let mut a = ForceLayout::default();
        let mut b = ForceLayout::default();
        let entries = vec![entry("concepts/x.md")];
        a.sync_entries(&entries);
        b.sync_entries(&entries);
        assert_eq!(a.position("concepts/x.md"), b.position("concepts/x.md"));
        assert!(a.position("concepts/x.md").is_some());
    }

    #[test]
    fn sync_entries_prunes_removed_ids() {
        let mut layout = ForceLayout::default();
        layout.sync_entries(&[entry("a.md"), entry("b.md")]);
        assert!(layout.position("b.md").is_some());
        layout.sync_entries(&[entry("a.md")]);
        assert!(layout.position("b.md").is_none());
    }

    /// Re-syncing an unchanged entry set must not reseed anything — a node
    /// the user dragged keeps the position they dropped it at, rather than
    /// snapping back to its hash-scatter seed on the very next tick.
    #[test]
    fn sync_entries_preserves_positions_for_surviving_ids() {
        let mut layout = ForceLayout::default();
        let entries = vec![entry("a.md"), entry("b.md")];
        layout.sync_entries(&entries);
        let b_before = layout.position("b.md").unwrap();
        layout.set_position("a.md", (0.25, 0.75));

        layout.sync_entries(&entries);

        assert_eq!(layout.position("a.md"), Some((0.25, 0.75)));
        assert_eq!(layout.position("b.md"), Some(b_before));
    }

    #[test]
    fn set_position_clamps_into_unit_square() {
        let mut layout = ForceLayout::default();
        layout.sync_entries(&[entry("a.md")]);
        layout.set_position("a.md", (1.5, -0.5));
        assert_eq!(layout.position("a.md"), Some((1.0, 0.0)));
    }

    #[test]
    fn set_position_is_a_noop_for_an_unseeded_id() {
        let mut layout = ForceLayout::default();
        layout.set_position("ghost.md", (0.5, 0.5));
        assert_eq!(layout.position("ghost.md"), None);
    }

    #[test]
    fn connected_nodes_converge_toward_rest_length() {
        let mut layout = ForceLayout::default();
        let entries = vec![entry("a.md"), entry("b.md")];
        layout.sync_entries(&entries);
        layout.set_position("a.md", (0.1, 0.5));
        layout.set_position("b.md", (0.9, 0.5));
        let edges = vec![(0, 1)];
        for _ in 0..500 {
            layout.step(&entries, &edges, None, 0.05);
        }
        let (ax, _) = layout.position("a.md").unwrap();
        let (bx, _) = layout.position("b.md").unwrap();
        let dist = (bx - ax).abs();
        assert!((dist - REST_LENGTH).abs() < 0.05, "expected ~{REST_LENGTH}, got {dist}");
    }

    #[test]
    fn unconnected_nodes_are_pushed_apart_by_repulsion() {
        let mut layout = ForceLayout::default();
        let entries = vec![entry("a.md"), entry("b.md")];
        layout.sync_entries(&entries);
        layout.set_position("a.md", (0.5, 0.5));
        layout.set_position("b.md", (0.5001, 0.5));
        for _ in 0..50 {
            layout.step(&entries, &[], None, 0.05);
        }
        let (ax, ay) = layout.position("a.md").unwrap();
        let (bx, by) = layout.position("b.md").unwrap();
        assert!((ax - bx).hypot(ay - by) > 0.01);
    }

    #[test]
    fn isolated_node_stays_within_the_unit_square() {
        let mut layout = ForceLayout::default();
        let entries = vec![entry("a.md")];
        layout.sync_entries(&entries);
        layout.set_position("a.md", (0.99, 0.01));
        for _ in 0..200 {
            layout.step(&entries, &[], None, 0.05);
        }
        let (x, y) = layout.position("a.md").unwrap();
        assert!((0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y));
    }

    #[test]
    fn pinned_node_is_excluded_from_integration() {
        let mut layout = ForceLayout::default();
        let entries = vec![entry("a.md"), entry("b.md")];
        layout.sync_entries(&entries);
        layout.set_position("a.md", (0.5, 0.5));
        layout.set_position("b.md", (0.5001, 0.5));
        layout.step(&entries, &[], Some("a.md"), 0.05);
        assert_eq!(layout.position("a.md"), Some((0.5, 0.5)));
    }
}
