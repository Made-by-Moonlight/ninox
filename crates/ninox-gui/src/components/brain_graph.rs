//! The v1 Brain tab's Graph view (`BrainGraph.dc.html`): pure layout/scope
//! logic first (this module), the canvas renderer built on top of it. Fixes
//! the brief's core complaint — "the force directed graph... renders badly
//! for me with ~900 nodes" — by never laying out (or drawing) the whole
//! brain unless the user opts into Everything mode, and by making whatever
//! *is* drawn fast to hit-test and cheap to re-render.
//!
//! Three scopes (`GraphScope`):
//! - **Neighbourhood** (default): `focus` plus up to `hops` link-steps away.
//! - **Clusters**: one bubble per repo, sized by entry count.
//! - **Everything**: opt-in, every (filtered) entry — the 900-node case the
//!   brief calls out by name.
//!
//! Layout is a deterministic, fixed-iteration force simulation (seeded from
//! each entry's id, never the clock or an RNG) that runs once per scope/
//! filter change and is then frozen — never a per-frame physics tick like
//! the force-directed pinboard this replaces. `SpatialIndex` turns
//! hit-testing from an O(n) scan into an O(1)-average grid lookup, and the
//! canvas layer (`brain_graph_canvas`, built on this module) caches the
//! static geometry between frames (`iced::widget::canvas::Cache`) so pan/
//! zoom/hover redraw only the cheap dynamic overlay.

use std::collections::{HashMap, HashSet, VecDeque};

use ninox_core::BrainEntry;
pub use ninox_core::brain::{cluster_by_repo, neighbourhood_capped, ClusterBubble};

/// Hops the "Hops" control defaults to (brief: "the Hops control defaults
/// to 2").
pub const DEFAULT_HOPS: u8 = 2;

/// Everything this view needs to remember between renders: the active
/// scope and its controls, the last-computed layout (recomputed only when
/// the scope/focus/filters actually change — see `App::refresh_brain_graph`
/// — never per frame), and the canvas's own geometry cache. Not `Clone`:
/// `iced::widget::canvas::Cache` holds renderer-specific cached geometry
/// behind a `RefCell`, so neither is `BrainViewState` once this is a field
/// of it (nothing in the app actually clones that state — see the removed
/// derive's blame for why it was never load-bearing).
#[derive(Debug)]
pub struct BrainGraphState {
    pub scope: GraphScope,
    /// Neighbourhood's radius, 1-3 (brief default: 2).
    pub hops: u8,
    /// The centred entry in Neighbourhood scope; `None` until something is
    /// selected, in which case the view falls back to Clusters per the
    /// brief ("Esc goes back to Clusters").
    pub focus: Option<usize>,
    /// Entry index under the cursor, for the hover-only label tier.
    pub hovered: Option<usize>,
    /// Cluster-bubble label under the cursor in Clusters scope — drives a
    /// floating hover label for bubbles too small for a static one.
    pub hovered_cluster: Option<String>,
    /// Entry index last clicked/focused in the canvas — drives the 340px
    /// selected panel, independent of the List view's own `selected`.
    pub graph_selected: Option<usize>,
    /// "Hide unlinked entries" — on by default.
    pub hide_unlinked: bool,
    /// Everything scope's repo filter — set when a Clusters bubble is
    /// opened, or cleared to see every (filtered) entry at once.
    pub repo_filter: Option<String>,
    /// `visible_kinds` has been populated from real data at least once —
    /// distinguishes "never initialized yet" from "user unchecked every
    /// kind", since both look like an empty set.
    pub kinds_initialized: bool,
    /// "Freeze layout" — when set, filter/scope changes still recompute
    /// which nodes are shown, but `App::refresh_brain_graph` keeps the
    /// previous positions for any node that was already laid out instead
    /// of reseeding it, so dragging through filters doesn't reshuffle
    /// everything the user hasn't touched.
    pub freeze_layout: bool,
    /// The scope `refresh_brain_graph` last computed a layout for. Freezing
    /// is meant to pin positions against filter changes, not against a
    /// scope switch (the old nodes belong to a different subgraph entirely),
    /// so the freeze guard also checks this against the current `scope`.
    pub last_scope: Option<GraphScope>,
    /// How many beyond-hop-1 nodes `neighbourhood_capped` dropped for the
    /// current focus/hops — drives the "+N more, raise hops or open in
    /// list" hint. Zero outside Neighbourhood or when nothing was dropped.
    pub hidden_neighbourhood_count: usize,
    /// Kind keys (`ninox_core::brain::normalize_kind` values) checked in
    /// Everything mode's filter list. Repos and the minor kinds (hazards,
    /// project, reference, feedback) start unchecked per the brief;
    /// populated once real data is loaded (`App::refresh_brain_graph`)
    /// rather than hardcoded, so a brain with different kinds still gets a
    /// sensible default.
    pub visible_kinds: HashSet<String>,
    /// Camera zoom (1.0 = fit scale at layout time) and pan, in layout
    /// units — applied at draw time, never baked into node positions.
    pub zoom: f32,
    pub pan_x: f32,
    pub pan_y: f32,
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    pub spatial: Option<SpatialIndex>,
    pub clusters: Vec<ClusterBubble>,
    pub cache: iced::widget::canvas::Cache,
    /// Recent layouts by `layout_key`.
    pub layout_cache: Vec<(u64, Vec<GraphNode>)>,
    /// The layout being computed off the UI thread; results for any other
    /// key are stale and dropped.
    pub pending_layout: Option<u64>,
    /// Handed to `App::iced_update` to spawn.
    pub layout_job: Option<LayoutJob>,
    /// A fit was asked for while the layout was pending.
    pub fit_when_laid_out: bool,
    /// The empty state's search text.
    pub search: String,
    /// Every repo cluster, packed — the minimap's map, kept across scopes.
    pub minimap: Vec<ClusterBubble>,
    pub color_by: ColorBy,
}

impl Default for BrainGraphState {
    fn default() -> Self {
        Self {
            scope: GraphScope::default(),
            hops: DEFAULT_HOPS,
            focus: None,
            hovered: None,
            hovered_cluster: None,
            graph_selected: None,
            hide_unlinked: true,
            repo_filter: None,
            kinds_initialized: false,
            freeze_layout: false,
            last_scope: None,
            hidden_neighbourhood_count: 0,
            visible_kinds: HashSet::new(),
            zoom: 1.0,
            pan_x: 0.0,
            pan_y: 0.0,
            nodes: Vec::new(),
            edges: Vec::new(),
            spatial: None,
            clusters: Vec::new(),
            cache: iced::widget::canvas::Cache::new(),
            layout_cache: Vec::new(),
            pending_layout: None,
            layout_job: None,
            fit_when_laid_out: false,
            search: String::new(),
            minimap: Vec::new(),
            color_by: ColorBy::default(),
        }
    }
}

/// What a node's colour means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorBy {
    #[default]
    Kind,
    /// Its repo cluster, so communities stand out.
    Repo,
}

/// Distinct colours for the largest repo clusters, in size order; the
/// rest share `tokens::GRAY` (an "everything else" bucket). No teal: it
/// already means "you are here" and the hovered node's edges.
const REPO_PALETTE: [Color; 9] = [
    tokens::AMBER,
    rgb(0xa3, 0xe6, 0x35), // lime
    tokens::VIOLET,
    tokens::GREEN,
    tokens::RED,
    tokens::BLUE,
    tokens::PINK,
    tokens::CYAN,
    tokens::ORANGE,
];

/// Repo label → colour, from `clusters` sorted largest first (as
/// `cluster_by_repo` returns them). "(no repo)" is always grey.
pub fn repo_colors(clusters: &[ClusterBubble]) -> HashMap<String, Color> {
    clusters
        .iter()
        .filter(|c| c.label != "(no repo)")
        .enumerate()
        .map(|(i, c)| (c.label.clone(), REPO_PALETTE.get(i).copied().unwrap_or(tokens::GRAY)))
        .collect()
}

fn repo_color(colors: &HashMap<String, Color>, label: &str) -> Color {
    colors.get(label).copied().unwrap_or(tokens::GRAY)
}

/// Which slice of the brain the Graph view is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GraphScope {
    #[default]
    Neighbourhood,
    Clusters,
    Everything,
}

/// A laid-out node: `entry` is an index into the same `entries` slice the
/// layout was computed from, `(x, y)` is its position in arbitrary layout
/// units (not pixels — the canvas applies pan/zoom at draw time), `degree`
/// is its link count within the laid-out subgraph (drives node radius and
/// label priority, mirroring the brief's "focus node and its 1-hop
/// neighbours always labelled").
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GraphNode {
    pub entry: usize,
    pub x: f32,
    pub y: f32,
    pub degree: u32,
}

/// A laid-out edge, as positions in `nodes` (not entry indices) — cheap for
/// the canvas to draw without a second lookup per frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GraphEdge {
    pub a: usize,
    pub b: usize,
}

/// Deterministic FNV-1a-derived hash mapped into `[0, 1)` — the same
/// technique the deleted force-directed pinboard used for its hash scatter,
/// kept here as this module's seed: the same entry id (+ salt) always
/// yields the same value, so a layout over the same node set is pixel-
/// identical across renders, window resizes, and app restarts (brief: "a
/// deterministic seed, so layouts are stable across renders").
const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color { r: r as f32 / 255.0, g: g as f32 / 255.0, b: b as f32 / 255.0, a: 1.0 }
}

fn hash01(s: &str, salt: u64) -> f32 {
    let mut h: u64 = 1469598103934665603 ^ salt;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    ((h >> 11) as f64 / (1u64 << 53) as f64) as f32
}

/// Node degree within the subgraph induced by `node_ids` and the subset of
/// `edges` connecting them — not the node's global degree across the whole
/// brain, so a 2-hop neighbour's degree reflects only what's actually drawn.
fn degrees_within(node_ids: &[usize], edges: &[(usize, usize)]) -> HashMap<usize, u32> {
    let set: HashSet<usize> = node_ids.iter().copied().collect();
    let mut degree: HashMap<usize, u32> = node_ids.iter().map(|&n| (n, 0)).collect();
    for &(a, b) in edges {
        if set.contains(&a) && set.contains(&b) {
            *degree.entry(a).or_default() += 1;
            *degree.entry(b).or_default() += 1;
        }
    }
    degree
}

/// Force-directed layout in the ForceAtlas2 style, which separates the
/// communities of a hub-heavy graph like a brain: every pair repels with
/// `k²·(deg+1)(deg+1)/d` (Barnes–Hut approximated), every edge pulls
/// linearly with its length divided by the degree ("dissuade hubs", so a
/// hub doesn't collapse its neighbourhood into a ball), and gravity
/// proportional to degree holds detached pieces near the core. A cooling
/// temperature caps each step so it settles. Fixed iteration count and an
/// id-seeded start, so a layout is deterministic.
const LAYOUT_ITERATIONS: usize = 300;
/// Ideal edge length `k`, in layout units (`LAYOUT_SCALE` px at zoom 1):
/// small enough that a converged 900-node brain fits the canvas with
/// visible nodes, large enough for clusters to read as separate.
const IDEAL_EDGE_LEN: f32 = 0.3;
/// Target median distance between a node and its nearest neighbour, in
/// layout units (~22px at zoom 1: clear of two typical node radii).
const NEIGHBOUR_SPACING: f32 = 0.16;
/// Barnes–Hut opening angle: a cell farther away than `size / THETA` acts
/// as one body. 0.9 is the usual speed/accuracy trade-off.
const BH_THETA: f32 = 0.9;
/// How far out (in core radii) gravity balances the whole graph's repulsion
/// on a detached node.
const GRAVITY_REACH: f32 = 1.4;

/// The median distance from each node to its nearest other node, or `None`
/// for fewer than two nodes.
fn median_nearest_neighbour(pos: &[(f32, f32)]) -> Option<f32> {
    if pos.len() < 2 {
        return None;
    }
    let tree = QuadTree::build(pos, &vec![1.0; pos.len()]);
    let mut nn: Vec<f32> = (0..pos.len()).map(|i| tree.nearest(i as u32, pos[i])).collect();
    nn.sort_by(|a, b| a.total_cmp(b));
    Some(nn[nn.len() / 2])
}

/// A Barnes–Hut quadtree over node positions, rebuilt every iteration.
struct QuadTree {
    cells: Vec<QuadCell>,
}

#[derive(Clone, Copy)]
struct QuadCell {
    cx: f32,
    cy: f32,
    half: f32,
    mass: f32,
    mx: f32,
    my: f32,
    /// First child's index (children are 4 consecutive cells), or 0 for a leaf.
    first_child: u32,
    /// The single body in a leaf, if any (`u32::MAX` when empty).
    body: u32,
}

impl QuadTree {
    const MAX_DEPTH: u32 = 24;

    fn build(pos: &[(f32, f32)], mass: &[f32]) -> Self {
        let (mut min_x, mut min_y, mut max_x, mut max_y) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for &(x, y) in pos {
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
        let half = ((max_x - min_x).max(max_y - min_y) / 2.0).max(1e-3) * 1.001;
        let root = QuadCell { cx: (min_x + max_x) / 2.0, cy: (min_y + max_y) / 2.0, half, mass: 0.0, mx: 0.0, my: 0.0, first_child: 0, body: u32::MAX };
        let mut tree = Self { cells: Vec::with_capacity(pos.len() * 3) };
        tree.cells.push(root);
        for (i, &p) in pos.iter().enumerate() {
            tree.insert(0, i as u32, p, pos, mass, 0);
        }
        tree
    }

    fn quadrant(cell: &QuadCell, p: (f32, f32)) -> u32 {
        (if p.0 >= cell.cx { 1 } else { 0 }) + (if p.1 >= cell.cy { 2 } else { 0 })
    }

    fn insert(&mut self, at: usize, body: u32, p: (f32, f32), pos: &[(f32, f32)], mass: &[f32], depth: u32) {
        let c = self.cells[at];
        let w = mass[body as usize];
        let m = c.mass + w;
        self.cells[at].mx = (c.mx * c.mass + p.0 * w) / m;
        self.cells[at].my = (c.my * c.mass + p.1 * w) / m;
        self.cells[at].mass = m;
        if c.first_child == 0 {
            if c.body == u32::MAX && c.mass == 0.0 {
                self.cells[at].body = body;
                return;
            }
            if depth >= Self::MAX_DEPTH {
                return; // coincident points: they share this leaf's mass
            }
            let first = self.cells.len() as u32;
            let h = c.half / 2.0;
            for q in 0..4u32 {
                let cx = c.cx + if q & 1 == 1 { h } else { -h };
                let cy = c.cy + if q & 2 == 2 { h } else { -h };
                self.cells.push(QuadCell { cx, cy, half: h, mass: 0.0, mx: 0.0, my: 0.0, first_child: 0, body: u32::MAX });
            }
            self.cells[at].first_child = first;
            let old = c.body;
            self.cells[at].body = u32::MAX;
            if old != u32::MAX {
                let op = pos[old as usize];
                let q = Self::quadrant(&self.cells[at], op);
                self.insert((first + q) as usize, old, op, pos, mass, depth + 1);
            }
        }
        let cell = self.cells[at];
        let q = Self::quadrant(&cell, p);
        self.insert((cell.first_child + q) as usize, body, p, pos, mass, depth + 1);
    }

    /// Distance from `p` to the nearest body other than `body`.
    fn nearest(&self, body: u32, p: (f32, f32)) -> f32 {
        let mut best = f32::MAX;
        let mut stack = vec![0u32];
        while let Some(at) = stack.pop() {
            let c = &self.cells[at as usize];
            if c.mass == 0.0 {
                continue;
            }
            // Distance from `p` to this cell's box: skip boxes that can't beat `best`.
            let ox = ((p.0 - c.cx).abs() - c.half).max(0.0);
            let oy = ((p.1 - c.cy).abs() - c.half).max(0.0);
            if ox.hypot(oy) >= best {
                continue;
            }
            if c.first_child == 0 {
                if c.body != body && c.body != u32::MAX {
                    best = best.min((p.0 - c.mx).hypot(p.1 - c.my));
                } else if c.body == u32::MAX || c.mass > 1.0 {
                    best = 0.0; // coincident twins share this leaf
                }
            } else {
                for q in 0..4 {
                    stack.push(c.first_child + q);
                }
            }
        }
        best
    }

    /// The summed repulsion on `body` (weight `own`) at `p`: each other body
    /// of weight `m` contributes `k²·own·m/d`.
    fn repulsion(&self, body: u32, own: f32, p: (f32, f32), k2: f32, stack: &mut Vec<u32>) -> (f32, f32) {
        let (mut fx, mut fy) = (0.0, 0.0);
        stack.clear();
        stack.push(0);
        while let Some(at) = stack.pop() {
            let c = &self.cells[at as usize];
            if c.mass == 0.0 || (c.first_child == 0 && c.body == body && c.mass <= own) {
                continue;
            }
            let (dx, dy) = (p.0 - c.mx, p.1 - c.my);
            let d2 = (dx * dx + dy * dy).max(1e-6);
            let far = (c.half * 2.0) * (c.half * 2.0) < BH_THETA * BH_THETA * d2;
            if c.first_child == 0 || far {
                // A leaf holding `body` plus coincident twins excludes itself.
                let mass = if c.first_child == 0 && c.body == body { c.mass - own } else { c.mass };
                let f = k2 * own * mass / d2;
                fx += dx * f;
                fy += dy * f;
            } else {
                for q in 0..4 {
                    stack.push(c.first_child + q);
                }
            }
        }
        (fx, fy)
    }
}

/// A dense array-backed spatial grid over the current bounding box, used by
/// `layout`'s repulsion and collision passes to find nearby nodes without
/// an all-pairs scan. Rebuilt fresh every iteration (positions move), so it
/// deliberately avoids `HashMap`'s per-insert hashing cost — at 900 nodes
/// across up to 180 rebuilds (`LAYOUT_ITERATIONS` + `COLLISION_PASSES`)
/// that overhead was still showing up directly in wall-clock time despite
/// each lookup itself already being cheap. An empty `Vec` costs no
/// allocation in Rust, so a mostly-empty dense grid over a wide bounding
/// box is still cheap.
struct DenseGrid {
    cell_size: f32,
    min_cx: i32,
    min_cy: i32,
    width: i32,
    height: i32,
    buckets: Vec<Vec<usize>>,
}

impl DenseGrid {
    fn cell_of(cell_size: f32, p: (f32, f32)) -> (i32, i32) {
        ((p.0 / cell_size).floor() as i32, (p.1 / cell_size).floor() as i32)
    }

    fn build(pos: &[(f32, f32)], cell_size: f32) -> Self {
        if pos.is_empty() {
            return Self { cell_size, min_cx: 0, min_cy: 0, width: 0, height: 0, buckets: Vec::new() };
        }
        let (mut min_cx, mut min_cy, mut max_cx, mut max_cy) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
        for &p in pos {
            let (cx, cy) = Self::cell_of(cell_size, p);
            min_cx = min_cx.min(cx);
            min_cy = min_cy.min(cy);
            max_cx = max_cx.max(cx);
            max_cy = max_cy.max(cy);
        }
        let width = max_cx - min_cx + 1;
        let height = max_cy - min_cy + 1;
        let mut buckets = vec![Vec::new(); (width * height) as usize];
        for (i, &p) in pos.iter().enumerate() {
            let (cx, cy) = Self::cell_of(cell_size, p);
            buckets[((cx - min_cx) + (cy - min_cy) * width) as usize].push(i);
        }
        Self { cell_size, min_cx, min_cy, width, height, buckets }
    }

    /// Calls `f` with every node index sharing `p`'s cell or one of its 8
    /// neighbours — a superset of "within `cell_size` of `p`", not an exact
    /// radius query, which is exactly what both callers below want (they
    /// each do their own precise distance check afterward).
    fn for_each_nearby(&self, p: (f32, f32), mut f: impl FnMut(usize)) {
        let (cx, cy) = Self::cell_of(self.cell_size, p);
        for ncy in (cy - 1)..=(cy + 1) {
            let gy = ncy - self.min_cy;
            if gy < 0 || gy >= self.height {
                continue;
            }
            for ncx in (cx - 1)..=(cx + 1) {
                let gx = ncx - self.min_cx;
                if gx < 0 || gx >= self.width {
                    continue;
                }
                for &j in &self.buckets[(gx + gy * self.width) as usize] {
                    f(j);
                }
            }
        }
    }
}

/// Lays out `node_ids` (indices into the full entry set) using only the
/// edges that connect two of them, via a deterministic, fixed-iteration
/// force simulation seeded from each entry's own id — never the clock or
/// an RNG, and never continued frame-to-frame (brief: "the simulation
/// stops after it settles, so nothing jitters"). `entries` supplies the
/// ids used to seed each node's starting position.
pub fn layout(node_ids: &[usize], edges: &[(usize, usize)], entries: &[BrainEntry]) -> Vec<GraphNode> {
    let seeds: Vec<String> = node_ids.iter().map(|&n| entries.get(n).map(|e| e.id.clone()).unwrap_or_default()).collect();
    layout_seeded(node_ids, edges, &seeds)
}

/// [`layout`] with each node's seed id given directly (`seeds[i]` for
/// `node_ids[i]`), so it can run off the UI thread without the entries.
pub fn layout_seeded(node_ids: &[usize], edges: &[(usize, usize)], seeds: &[String]) -> Vec<GraphNode> {
    let degree = degrees_within(node_ids, edges);
    let index_of: HashMap<usize, usize> = node_ids.iter().enumerate().map(|(i, &n)| (n, i)).collect();
    let local_edges: Vec<(usize, usize)> = edges
        .iter()
        .filter_map(|&(a, b)| Some((*index_of.get(&a)?, *index_of.get(&b)?)))
        .collect();

    let n = node_ids.len();
    // Simulated in natural units (repulsion constant 1); rescaled to
    // `IDEAL_EDGE_LEN` afterwards, since the equilibrium spacing depends on
    // degrees and community sizes, not on any one constant.
    let k = 1.0_f32;
    let spread = (n as f32).sqrt() * k;
    let mut pos: Vec<(f32, f32)> = seeds
        .iter()
        .map(|id| ((hash01(id, 7) * 2.0 - 1.0) * spread, (hash01(id, 13) * 2.0 - 1.0) * spread))
        .collect();

    let weight: Vec<f32> = node_ids.iter().map(|id| degree.get(id).copied().unwrap_or(0) as f32 + 1.0).collect();
    // Repulsion scaled to the degree weights so the equilibrium sits near
    // the starting spread and the cooling schedule can reach it (measured:
    // 0.001–0.02 separates 900-node communities, ~0.1 and up freezes them
    // half-expanded; this lands mid-band).
    let mean_weight = weight.iter().sum::<f32>() / n.max(1) as f32;
    let k2 = 0.25 / (mean_weight * mean_weight);
    // Gravity (∝ weight · distance) strong enough that a detached node of
    // weight 1 settles about `GRAVITY_REACH` core radii out, where it
    // balances the whole graph's repulsion — not tens of radii away, which
    // would shrink the fitted core to a dot.
    let total_weight: f32 = weight.iter().sum();
    let core_radius = spread.max(k);
    let reach = core_radius * GRAVITY_REACH;
    let gravity = k2 * total_weight / (reach * reach);
    let mut temperature = spread * 0.2;
    let cooling = (0.002_f32 * k / temperature.max(1e-6)).powf(1.0 / LAYOUT_ITERATIONS as f32);
    let mut disp = vec![(0.0_f32, 0.0_f32); n];
    let mut stack = Vec::with_capacity(64);
    for _ in 0..LAYOUT_ITERATIONS {
        let tree = QuadTree::build(&pos, &weight);
        for i in 0..n {
            disp[i] = tree.repulsion(i as u32, weight[i], pos[i], k2, &mut stack);
        }
        for &(a, b) in &local_edges {
            let (dx, dy) = (pos[a].0 - pos[b].0, pos[a].1 - pos[b].1);
            // Linear pull, shared out by degree so hubs move less.
            disp[a].0 -= dx / weight[a];
            disp[a].1 -= dy / weight[a];
            disp[b].0 += dx / weight[b];
            disp[b].1 += dy / weight[b];
        }
        for i in 0..n {
            disp[i].0 -= pos[i].0 * gravity * weight[i];
            disp[i].1 -= pos[i].1 * gravity * weight[i];
            let len = (disp[i].0 * disp[i].0 + disp[i].1 * disp[i].1).sqrt();
            if len > 0.0 {
                let step = len.min(temperature) / len;
                pos[i].0 += disp[i].0 * step;
                pos[i].1 += disp[i].1 * step;
            }
        }
        temperature *= cooling;
    }

    // Scale by spacing, not edge length: communities pack tightly, so
    // unlinked neighbours sit closer than linked ones. Setting the median
    // nearest-neighbour distance leaves the collision pass only stragglers.
    let scale = match median_nearest_neighbour(&pos) {
        Some(d) if d > 0.0 => NEIGHBOUR_SPACING / d,
        _ => IDEAL_EDGE_LEN,
    };
    for p in &mut pos {
        p.0 *= scale;
        p.1 *= scale;
    }

    // Explicit collision resolution: the force loop above is a generic
    // point-charge repulsion with no awareness of each node's actual drawn
    // radius, so two high-degree (large) nodes can still settle closer
    // together than their circles allow — at the Everything scope's ~900
    // nodes that reads as one solid overlapping blob rather than a graph.
    // This pushes any pair closer than their combined radius (plus a
    // visual gap) directly apart, in layout units (`node_radius` is
    // defined in screen pixels at zoom 1, so divide by `LAYOUT_SCALE` to
    // get the equivalent layout-space size) — independent of whatever
    // zoom the user is currently at.
    //
    // A naive all-pairs scan here is O(n²) per pass — at 900 nodes, this and
    // the repulsion loop above (also originally all-pairs) together pushed
    // `layout_900_nodes_within_time_budget` to 6.9s on CI's debug-build
    // runner. Two nodes can only possibly collide when they're within
    // `collision_cell` of each other — any pair farther apart than the
    // largest possible combined radius plus padding is guaranteed clear —
    // so bucket nodes into a `DenseGrid` sized to that bound and only check
    // each node against its own cell plus the 8 neighbours, the same
    // locality trick `SpatialIndex` uses for hit-testing. This turns the
    // pass from O(n²) into roughly O(n) once nodes are spread beyond a cell
    // width apart, which a 900-node layout's extent easily is.
    const COLLISION_PASSES: usize = 400;
    const COLLISION_PADDING: f32 = 2.0 / LAYOUT_SCALE;
    let max_node_radius_layout = node_radius(u32::MAX) / LAYOUT_SCALE;
    let collision_cell = max_node_radius_layout * 2.0 + COLLISION_PADDING;
    for _ in 0..COLLISION_PASSES {
        let mut moved = false;
        let grid = DenseGrid::build(&pos, collision_cell);
        for i in 0..n {
            let mut hits: Vec<usize> = Vec::new();
            grid.for_each_nearby(pos[i], |j| {
                if j > i {
                    hits.push(j);
                }
            });
            for j in hits {
                let dx = pos[j].0 - pos[i].0;
                let dy = pos[j].1 - pos[i].1;
                let dist = (dx * dx + dy * dy).sqrt().max(0.0001);
                let min_dist = node_radius(degree.get(&node_ids[i]).copied().unwrap_or(0)) / LAYOUT_SCALE
                    + node_radius(degree.get(&node_ids[j]).copied().unwrap_or(0)) / LAYOUT_SCALE
                    + COLLISION_PADDING;
                if dist < min_dist {
                    moved = true;
                    // A hair past touching, so float rounding never leaves the pair just short.
                    let push = (min_dist - dist) / 2.0 + 1e-5;
                    let (ux, uy) = (dx / dist, dy / dist);
                    pos[i].0 -= ux * push;
                    pos[i].1 -= uy * push;
                    pos[j].0 += ux * push;
                    pos[j].1 += uy * push;
                }
            }
        }
        if !moved {
            break;
        }
    }

    node_ids
        .iter()
        .zip(pos)
        .map(|(&entry, (x, y))| GraphNode { entry, x, y, degree: degree.get(&entry).copied().unwrap_or(0) })
        .collect()
}

/// The entry with the most links (lowest index on a tie), or `None` when
/// there are no links at all.
pub fn most_linked(edges: &[(usize, usize)]) -> Option<usize> {
    let mut degree: HashMap<usize, u32> = HashMap::new();
    for &(a, b) in edges {
        *degree.entry(a).or_default() += 1;
        *degree.entry(b).or_default() += 1;
    }
    degree.into_iter().max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0))).map(|(i, _)| i)
}

/// Graphs at least this big are laid out off the UI thread.
pub const ASYNC_LAYOUT_MIN_NODES: usize = 200;
/// Layouts kept for switching back and forth between graphs (arbitrary).
const LAYOUT_CACHE_SIZE: usize = 8;

/// Identifies a layout: the node set and the edges among them.
pub fn layout_key(node_ids: &[usize], edges: &[(usize, usize)]) -> u64 {
    let set: HashSet<usize> = node_ids.iter().copied().collect();
    let mut h: u64 = 1469598103934665603;
    let mut mix = |v: u64| {
        h ^= v;
        h = h.wrapping_mul(1099511628211);
    };
    for &n in node_ids {
        mix(n as u64);
    }
    mix(u64::MAX);
    for &(a, b) in edges {
        if set.contains(&a) && set.contains(&b) {
            mix(((a as u64) << 32) | b as u64);
        }
    }
    h
}

/// A layout to compute off the UI thread.
#[derive(Debug, Clone)]
pub struct LayoutJob {
    pub key: u64,
    pub node_ids: Vec<usize>,
    pub edges: Vec<(usize, usize)>,
    pub seeds: Vec<String>,
}

impl BrainGraphState {
    pub fn cached_layout(&self, key: u64) -> Option<Vec<GraphNode>> {
        self.layout_cache.iter().find(|(k, _)| *k == key).map(|(_, n)| n.clone())
    }

    pub fn remember_layout(&mut self, key: u64, nodes: Vec<GraphNode>) {
        self.layout_cache.retain(|(k, _)| *k != key);
        self.layout_cache.push((key, nodes));
        if self.layout_cache.len() > LAYOUT_CACHE_SIZE {
            self.layout_cache.remove(0);
        }
    }
}

/// Laid-out edges between members of `nodes`, as positions into `nodes`
/// (not entry indices) — resolved once alongside the layout rather than
/// re-filtered by the canvas on every draw.
pub fn layout_edges(nodes: &[GraphNode], edges: &[(usize, usize)]) -> Vec<GraphEdge> {
    let index_of: HashMap<usize, usize> = nodes.iter().enumerate().map(|(i, n)| (n.entry, i)).collect();
    edges
        .iter()
        .filter_map(|&(a, b)| Some(GraphEdge { a: *index_of.get(&a)?, b: *index_of.get(&b)? }))
        .collect()
}

/// A uniform-grid spatial index over laid-out node positions: hit-testing
/// (hover/click) checks only the cell under the cursor and its eight
/// neighbours instead of scanning every node, which is what the brief's
/// "use a spatial index for hit-testing" is guarding against at the
/// 900-node Everything scale.
#[derive(Debug)]
pub struct SpatialIndex {
    cell_size: f32,
    cells: HashMap<(i32, i32), Vec<usize>>,
}

impl SpatialIndex {
    fn cell_of(&self, x: f32, y: f32) -> (i32, i32) {
        ((x / self.cell_size).floor() as i32, (y / self.cell_size).floor() as i32)
    }

    /// `cell_size` should be on the order of the largest node's hit-test
    /// radius — too small and a node can straddle several cells (handled
    /// correctly, just less of a speedup); too large and each cell holds
    /// too many nodes (degrades toward a linear scan).
    pub fn build(nodes: &[GraphNode], cell_size: f32) -> Self {
        let mut index = SpatialIndex { cell_size: cell_size.max(0.0001), cells: HashMap::new() };
        for (i, node) in nodes.iter().enumerate() {
            index.cells.entry(index.cell_of(node.x, node.y)).or_default().push(i);
        }
        index
    }

    /// Node indices (into the `nodes` slice `build` was called with) whose
    /// cell is within one cell of `(x, y)` — a superset of the true
    /// neighbours, cheap enough that the caller just distance-checks each.
    pub fn nearby(&self, x: f32, y: f32) -> Vec<usize> {
        let (cx, cy) = self.cell_of(x, y);
        let mut out = Vec::new();
        for dx in -1..=1 {
            for dy in -1..=1 {
                if let Some(ids) = self.cells.get(&(cx + dx, cy + dy)) {
                    out.extend(ids);
                }
            }
        }
        out
    }
}

/// Nearest node to `(x, y)` within `radius`, using `index` to avoid
/// scanning every node in `nodes`. `None` if nothing is that close.
pub fn hit_test(nodes: &[GraphNode], index: &SpatialIndex, x: f32, y: f32, radius: f32) -> Option<usize> {
    index
        .nearby(x, y)
        .into_iter()
        .map(|i| (i, (nodes[i].x - x).hypot(nodes[i].y - y)))
        .filter(|&(_, d)| d <= radius)
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
        .map(|(i, _)| i)
}

/// The entry id to move focus to when an arrow key is pressed (brief:
/// "the arrow keys walk links") — among `focus_entry`'s direct neighbours
/// in the currently laid-out graph, the one whose direction from the focus
/// most closely matches `dir` (a unit-ish vector; `(1,0)` right, `(0,-1)`
/// up, matching layout space, which isn't y-flipped relative to the
/// screen). `None` if the focus isn't currently laid out, has no
/// neighbours, or none of them are reasonably aligned with `dir` (a
/// neighbour roughly perpendicular to the pressed key, or behind it,
/// shouldn't win just for being the only option).
pub fn walk_direction(nodes: &[GraphNode], edges: &[GraphEdge], focus_entry: usize, dir: (f32, f32)) -> Option<usize> {
    let focus_idx = nodes.iter().position(|n| n.entry == focus_entry)?;
    let focus = nodes[focus_idx];
    let dir_len = (dir.0 * dir.0 + dir.1 * dir.1).sqrt().max(0.0001);

    edges
        .iter()
        .filter_map(|e| if e.a == focus_idx { Some(e.b) } else if e.b == focus_idx { Some(e.a) } else { None })
        .map(|i| {
            let (nx, ny) = (nodes[i].x - focus.x, nodes[i].y - focus.y);
            let len = (nx * nx + ny * ny).sqrt().max(0.0001);
            (i, (nx * dir.0 + ny * dir.1) / (len * dir_len))
        })
        .filter(|&(_, cosine)| cosine > 0.3)
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
        .map(|(i, _)| nodes[i].entry)
}

/// Minor kinds that start unchecked in Everything mode's kind filter
/// (brief: "Repos and the minor kinds off by default").
const MINOR_KINDS: &[&str] = &["repos", "hazards", "project", "reference", "feedback"];

/// The kind filter's starting state once real data is loaded: every kind
/// present in `entries` except `MINOR_KINDS`.
pub fn default_visible_kinds(entries: &[BrainEntry]) -> HashSet<String> {
    entries
        .iter()
        .map(|e| ninox_core::brain::normalize_kind(&e.entry_type))
        .filter(|k| !MINOR_KINDS.contains(&k.as_str()))
        .collect()
}

/// Entry indices to lay out for the current scope and filters — pure
/// selection logic so `App::refresh_brain_graph` can stay a thin
/// "call this, then lay it out" wrapper. `Clusters` has no node selection
/// of its own (`cluster_by_repo` computes bubbles directly from `entries`),
/// so it always returns empty.
/// Bundles `visible_node_ids`'s filter inputs — clippy's `too_many_arguments`
/// is right that six independent positional bools/options is the point
/// where a caller stops being able to tell them apart at the call site.
pub struct GraphFilters<'a> {
    pub scope: GraphScope,
    pub focus: Option<usize>,
    pub hops: u8,
    pub hide_unlinked: bool,
    pub visible_kinds: &'a HashSet<String>,
    pub repo_filter: Option<&'a str>,
}

pub fn visible_node_ids(entries: &[BrainEntry], edges: &[(usize, usize)], filters: GraphFilters) -> Vec<usize> {
    match filters.scope {
        GraphScope::Neighbourhood => {
            let Some(focus) = filters.focus else { return Vec::new() };
            neighbourhood_capped(focus, edges, filters.hops).0
        }
        GraphScope::Clusters => Vec::new(),
        GraphScope::Everything => {
            let linked: HashSet<usize> = edges.iter().flat_map(|&(a, b)| [a, b]).collect();
            (0..entries.len())
                .filter(|i| !filters.hide_unlinked || linked.contains(i))
                .filter(|&i| {
                    entries.get(i).is_some_and(|e| {
                        filters.visible_kinds.contains(&ninox_core::brain::normalize_kind(&e.entry_type))
                            && filters.repo_filter.is_none_or(|r| e.repos.iter().any(|x| x == r))
                    })
                })
                .collect()
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

use iced::widget::canvas::{self, Frame, Geometry, Path, Stroke};
use iced::{mouse, Color, Pixels, Point, Rectangle, Renderer, Theme, Vector};

use super::shell::tokens;
use crate::app::{App, Message};

/// Screen pixels per layout unit at zoom 1.0 — chosen so a Neighbourhood
/// view (nodes roughly `IDEAL_EDGE_LEN` apart) fills a comfortable fraction
/// of the canvas without the user needing to zoom just to see it.
const LAYOUT_SCALE: f32 = 140.0;
/// Screen-pixel hit-test tolerance around a node, independent of zoom.
const HIT_RADIUS_PX: f32 = 10.0;
/// Two clicks on the same node within this window count as a double-click
/// ("double-click opens in List") — `canvas::Program` gets raw button
/// events, not a synthesized double-click, so this is tracked by hand.
const DOUBLE_CLICK_WINDOW: std::time::Duration = std::time::Duration::from_millis(400);

fn node_radius(degree: u32) -> f32 {
    4.0 + (degree as f32 * 1.4).min(10.0)
}

/// Clips a node label to `max_chars`, so a long title doesn't run clear
/// across the canvas and into the selected panel — the canvas has no
/// bounding-box clipping or wrapping of its own the way iced's standard
/// text widgets do.
fn truncate_label(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_chars).collect();
        format!("{}…", truncated.trim_end())
    }
}

/// Approximation of the Graph canvas's own pixel size (total width minus
/// the 228px filters panel and 340px selected panel; height minus the 48px
/// toolbar and 58px prompt bar) — `BrainGraphFit`/scope changes need a pixel
/// size to fit content into, but that's rendering-time information the
/// `App::apply` message handler that computes the fit doesn't have access
/// to. A fixed estimate skews the exact zoom level on an unusually sized
/// window; it doesn't change whether everything ends up visible, which is
/// the fit's actual job.
const ASSUMED_CANVAS_SIZE: (f32, f32) = (450.0, 650.0);

/// Zoom/pan that frames every `(x, y, radius)` item with a margin, for
/// `BrainGraphFit` and the auto-fit on scope changes (brief: "+/−/fit
/// buttons"). `radius` lets a `ClusterBubble`'s own extent count, not just
/// its centre point; pass `0.0` for plain node positions. Falls back to
/// zoom 1 centred on the origin for an empty set (nothing to fit to).
pub fn fit_transform(items: &[(f32, f32, f32)]) -> (f32, f32, f32) {
    fit_transform_reserving(items, FitReserve::default())
}

/// Height (px) the expanded minimap takes from the canvas's bottom edge:
/// its canvas, header, padding and corner margin.
pub const MINIMAP_RESERVE_PX: f32 = MINIMAP_SIZE.1 + 64.0;

/// Height (px) the top-left scope/hops overlay takes from the canvas's top
/// edge: its 14px inset plus the box (`scope_controls_corner`) with its
/// "N of M shown" line; the "+N more" hint adds [`SCOPE_HINT_RESERVE_PX`].
pub const SCOPE_OVERLAY_RESERVE_PX: f32 = 14.0 + 66.0 + 8.0;
pub const SCOPE_HINT_RESERVE_PX: f32 = 20.0;

/// Screen strips (px) the fit keeps content out of: the scope overlay at
/// the top and the minimap at the bottom.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct FitReserve {
    pub top: f32,
    pub bottom: f32,
}

/// [`fit_transform`] into the canvas minus the `reserve` strips, centred in
/// what's left.
pub fn fit_transform_reserving(items: &[(f32, f32, f32)], reserve: FitReserve) -> (f32, f32, f32) {
    if items.is_empty() {
        return (1.0, 0.0, 0.0);
    }
    let (mut min_x, mut max_x, mut min_y, mut max_y) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
    for &(x, y, r) in items {
        min_x = min_x.min(x - r);
        max_x = max_x.max(x + r);
        min_y = min_y.min(y - r);
        max_y = max_y.max(y + r);
    }
    let (width, height) = ((max_x - min_x).max(0.01), (max_y - min_y).max(0.01));
    let (center_x, center_y) = ((min_x + max_x) / 2.0, (min_y + max_y) / 2.0);

    const MARGIN: f32 = 0.85; // fraction of the canvas the content should fill
    let reserved = reserve.top + reserve.bottom;
    let usable_h = (ASSUMED_CANVAS_SIZE.1 - reserved).max(ASSUMED_CANVAS_SIZE.1 * 0.4);
    let zoom = ((ASSUMED_CANVAS_SIZE.0 * MARGIN / (width * LAYOUT_SCALE)).min(usable_h * MARGIN / (height * LAYOUT_SCALE))).clamp(0.1, 2.0);
    // The camera centres on the canvas; shift the content by half the
    // strips' difference so it centres in the free band between them
    // (scaled down with the strips when the 40% floor kicked in).
    let shrink = if reserved > 0.0 { (ASSUMED_CANVAS_SIZE.1 - usable_h) / reserved } else { 0.0 };
    let lift = (reserve.bottom - reserve.top) * shrink / 2.0 / (zoom * LAYOUT_SCALE);
    (zoom, -center_x, -center_y - lift)
}

/// Rough max label pixel width to reserve on either side of a labelled
/// node — not a real text measurement (the canvas has none available
/// outside `Renderer`), just enough to stop a dated-slug-length title on an
/// edge node from running past the canvas the way a bare node-radius fit
/// would allow.
const MAX_LABEL_WIDTH_PX: f32 = 220.0;

/// `fit_transform` for Neighbourhood's node layout specifically: also
/// reserves room for whichever nodes `draw_graph` will actually label (hop
/// 0/1/2 from `focus` — see `hop_distances`), so a scope switch or the Fit
/// button doesn't land a zoom level that clips a label even though the node
/// it belongs to is fully on-screen. Computed in two passes since the label
/// reservation is itself in screen pixels: fit the bare nodes first to get
/// a zoom estimate, convert the pixel reservation to layout units at that
/// zoom, then fit again with labelled nodes' effective radius expanded.
#[cfg(test)]
pub fn fit_transform_for_nodes(nodes: &[GraphNode], edges: &[GraphEdge], focus: Option<usize>) -> (f32, f32, f32) {
    fit_transform_for_nodes_reserving(nodes, edges, focus, FitReserve::default())
}

/// [`fit_transform_for_nodes`] keeping clear of the `reserve` strips.
pub fn fit_transform_for_nodes_reserving(nodes: &[GraphNode], edges: &[GraphEdge], focus: Option<usize>, reserve: FitReserve) -> (f32, f32, f32) {
    let bare: Vec<(f32, f32, f32)> = nodes.iter().map(|n| (n.x, n.y, 0.0)).collect();
    let (zoom1, _, _) = fit_transform_reserving(&bare, reserve);
    if zoom1 <= 0.0 {
        return fit_transform_reserving(&bare, reserve);
    }
    let hops = hop_distances(nodes, edges, focus);
    let label_half_layout = MAX_LABEL_WIDTH_PX / (LAYOUT_SCALE * zoom1);
    let expanded: Vec<(f32, f32, f32)> =
        nodes.iter().zip(hops.iter()).map(|(n, hop)| (n.x, n.y, if hop.is_some() { label_half_layout } else { 0.0 })).collect();
    fit_transform_reserving(&expanded, reserve)
}

/// Layout-units <-> screen-pixels for the current pan/zoom, centred in the
/// canvas's own bounds.
struct Camera {
    center: Point,
    zoom: f32,
    pan: Vector,
}

impl Camera {
    fn to_screen(&self, x: f32, y: f32) -> Point {
        Point::new(
            self.center.x + (x + self.pan.x) * LAYOUT_SCALE * self.zoom,
            self.center.y + (y + self.pan.y) * LAYOUT_SCALE * self.zoom,
        )
    }

    fn to_layout(&self, p: Point) -> (f32, f32) {
        ((p.x - self.center.x) / (LAYOUT_SCALE * self.zoom) - self.pan.x, (p.y - self.center.y) / (LAYOUT_SCALE * self.zoom) - self.pan.y)
    }
}

/// BFS hop distance from the node whose entry is `focus_entry`, indexed by
/// position in `nodes` (not entry id) — drives the label tiers ("always
/// labelled on the focus node and its 1-hop neighbours... 2-hop labels are
/// small and dim... everything else is labelled on hover only"). Computed
/// once per cache rebuild, not per frame.
fn hop_distances(nodes: &[GraphNode], edges: &[GraphEdge], focus_entry: Option<usize>) -> Vec<Option<u8>> {
    let mut dist = vec![None; nodes.len()];
    let Some(start) = focus_entry.and_then(|e| nodes.iter().position(|n| n.entry == e)) else {
        return dist;
    };
    let mut adjacency: HashMap<usize, Vec<usize>> = HashMap::new();
    for e in edges {
        adjacency.entry(e.a).or_default().push(e.b);
        adjacency.entry(e.b).or_default().push(e.a);
    }
    dist[start] = Some(0);
    let mut frontier = VecDeque::from([start]);
    while let Some(node) = frontier.pop_front() {
        let d = dist[node].unwrap();
        if d >= 2 {
            continue;
        }
        if let Some(neighbors) = adjacency.get(&node) {
            for &n in neighbors {
                if dist[n].is_none() {
                    dist[n] = Some(d + 1);
                    frontier.push_back(n);
                }
            }
        }
    }
    dist
}

/// A label not yet drawn — collected while the node circles are drawn, then
/// placed in a second pass (`place_labels`) so colliding labels can be
/// resolved against each other instead of just drawn independently.
struct LabelCandidate {
    text: String,
    anchor: Point,
    node_r: f32,
    hop: u8,
    /// Link count: among neighbours, better-connected ones are labelled first.
    degree: u32,
    /// Under the cursor: always labelled.
    hovered: bool,
    /// Preferred side (left of the node, right-aligned), e.g. near the
    /// canvas's right edge.
    align_right: bool,
}

impl LabelCandidate {
    fn size(&self) -> f32 {
        if self.hop == 0 { 12.0 } else { 10.5 }
    }

    /// The label's box when drawn on the given side, with a little padding.
    fn bbox(&self, align_right: bool) -> (f32, f32, f32, f32) {
        let width = self.text.chars().count() as f32 * self.size() * LABEL_CHAR_WIDTH_EM + LABEL_PAD_PX * 2.0;
        let x0 = if align_right { self.anchor.x - self.node_r - LABEL_GAP_PX - width } else { self.anchor.x + self.node_r + LABEL_GAP_PX };
        (x0, self.anchor.y - LABEL_HEIGHT_PX / 2.0, x0 + width, self.anchor.y + LABEL_HEIGHT_PX / 2.0)
    }
}

/// Approximate glyph metrics for label collisions (the canvas can't
/// measure text): average Inter advance as a fraction of the font size.
const LABEL_CHAR_WIDTH_EM: f32 = 0.62;
const LABEL_HEIGHT_PX: f32 = 14.0;
const LABEL_GAP_PX: f32 = 4.0;
const LABEL_PAD_PX: f32 = 2.0;

fn boxes_overlap(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> bool {
    a.0 < b.2 && a.2 > b.0 && a.1 < b.3 && a.3 > b.1
}

/// Greedy placement: the focus first, then the hovered node, then
/// neighbours by degree. Each label takes its preferred side, else the
/// other side, else it's skipped — never drawn over one already placed or
/// over `obstacles` (the focus node). The focus and the hovered node are
/// always shown. Returns, per candidate, the side it's drawn on
/// (`Some(align_right)`) or `None` when skipped.
fn place_labels(candidates: &[LabelCandidate], obstacles: &[(f32, f32, f32, f32)]) -> Vec<Option<bool>> {
    let mut order: Vec<usize> = (0..candidates.len()).collect();
    order.sort_by_key(|&i| {
        let c = &candidates[i];
        let tier = if c.hop == 0 { 0 } else if c.hovered { 1 } else { 2 };
        (tier, std::cmp::Reverse(c.degree), c.hop)
    });

    let mut placed: Vec<(f32, f32, f32, f32)> = obstacles.to_vec();
    let mut sides: Vec<Option<bool>> = vec![None; candidates.len()];
    for i in order {
        let c = &candidates[i];
        let free = |side: bool| {
            let b = c.bbox(side);
            (!placed.iter().any(|&p| boxes_overlap(b, p))).then_some((side, b))
        };
        let chosen = free(c.align_right).or_else(|| free(!c.align_right));
        let chosen = match chosen {
            Some(hit) => Some(hit),
            None if c.hop == 0 || c.hovered => Some((c.align_right, c.bbox(c.align_right))),
            None => None,
        };
        if let Some((side, b)) = chosen {
            placed.push(b);
            sides[i] = Some(side);
        }
    }
    sides
}

fn draw_graph(frame: &mut Frame, g: &BrainGraphState, entries: &[BrainEntry], camera: &Camera, canvas_width: f32) {
    let hops = hop_distances(&g.nodes, &g.edges, g.focus);
    // Labels: the focus, its neighbours and the hovered node only.
    let max_labelled_hop: u8 = 1;
    let hovered = g.hovered.and_then(|entry| g.nodes.iter().position(|n| n.entry == entry));
    let mut candidates: Vec<LabelCandidate> = Vec::new();
    let repo_colors_now = repo_colors(&g.minimap);

    // `BrainGraph.dc.html`: other edges #2a3245, the focus's #4a5677. A
    // hovered node's edges light up teal and the rest step back.
    let (base, near) = (rgb(0x2a, 0x32, 0x45), rgb(0x4a, 0x56, 0x77));
    let (mut lit, mut rest) = (Vec::new(), Vec::new());
    for edge in &g.edges {
        let touches_hover = hovered.is_some_and(|h| edge.a == h || edge.b == h);
        let touches_focus = hops[edge.a] == Some(0) || hops[edge.b] == Some(0);
        if touches_hover || (hovered.is_none() && touches_focus) {
            lit.push((edge, touches_hover));
        } else {
            rest.push(edge);
        }
    }
    let rest_color = if hovered.is_some() { Color { a: 0.45, ..base } } else { base };
    for edge in rest {
        let (na, nb) = (&g.nodes[edge.a], &g.nodes[edge.b]);
        frame.stroke(
            &Path::line(camera.to_screen(na.x, na.y), camera.to_screen(nb.x, nb.y)),
            Stroke::default().with_color(rest_color).with_width(1.0),
        );
    }
    for (edge, by_hover) in lit {
        let (na, nb) = (&g.nodes[edge.a], &g.nodes[edge.b]);
        frame.stroke(
            &Path::line(camera.to_screen(na.x, na.y), camera.to_screen(nb.x, nb.y)),
            Stroke::default().with_color(if by_hover { Color { a: 0.85, ..tokens::TEAL } } else { near }).with_width(1.6),
        );
    }

    for (i, node) in g.nodes.iter().enumerate() {
        let entry = entries.get(node.entry);
        let kind = entry.map(|e| ninox_core::brain::normalize_kind(&e.entry_type)).unwrap_or_default();
        let color = match g.color_by {
            ColorBy::Kind => super::brain_list::kind_color(&kind),
            ColorBy::Repo => entry.and_then(|e| e.repos.first()).map(|r| repo_color(&repo_colors_now, r)).unwrap_or(tokens::GRAY),
        };
        let hop = hops[i];
        let is_focus = hop == Some(0);
        let p = camera.to_screen(node.x, node.y);
        // `node_radius` is defined in screen pixels at zoom 1 — the layout's
        // collision pass (see `layout`) only guarantees non-overlapping
        // circles under that same assumption, so the drawn radius must
        // scale with `camera.zoom` exactly as the node's position already
        // does via `to_screen`, or the guarantee silently breaks at any
        // other zoom (this is what let Everything's ~900 nodes still
        // overlap into a blob at the zoomed-out fit level).
        let r = node_radius(node.degree) * camera.zoom * if is_focus { 1.4 } else { 1.0 };

        if is_focus {
            frame.stroke(&Path::circle(p, r + 6.0), Stroke::default().with_color(tokens::TEAL).with_width(2.0));
        }
        frame.fill(&Path::circle(p, r), color);

        let label_hop = if hovered == Some(i) { Some(hop.unwrap_or(1).min(1)) } else { hop.filter(|h| *h <= max_labelled_hop) };
        if let (Some(hop), Some(e)) = (label_hop, entry) {
            // Flip the label to the node's left (right-aligned) when it's
            // in the canvas's right quarter — a label drawn to the right
            // there would run past the canvas into whatever sits beside it
            // (the selected panel). The canvas has no bounding-box clipping
            // of its own the way iced's standard text widgets do, so this
            // has to be decided here rather than relied on to just not
            // overflow.
            let near_right_edge = p.x > canvas_width * 0.75;
            candidates.push(LabelCandidate {
                text: truncate_label(&super::brain_panel::display_title(e), 32),
                anchor: p,
                node_r: r,
                hop,
                degree: node.degree,
                hovered: hovered == Some(i),
                align_right: near_right_edge,
            });
        }
    }

    // Labels never cover the focus node itself.
    let obstacles: Vec<(f32, f32, f32, f32)> = candidates
        .iter()
        .filter(|c| c.hop == 0)
        .map(|c| (c.anchor.x - c.node_r, c.anchor.y - c.node_r, c.anchor.x + c.node_r, c.anchor.y + c.node_r))
        .collect();
    let sides = place_labels(&candidates, &obstacles);
    for (candidate, side) in candidates.iter().zip(sides) {
        let Some(align_right) = side else { continue };
        frame.fill_text(canvas::Text {
            content: candidate.text.clone(),
            position: Point::new(
                if align_right { candidate.anchor.x - candidate.node_r - LABEL_GAP_PX } else { candidate.anchor.x + candidate.node_r + LABEL_GAP_PX + LABEL_PAD_PX },
                candidate.anchor.y,
            ),
            color: match candidate.hop {
                0 => tokens::TEXT,
                1 => tokens::DIM,
                _ => tokens::DIM2,
            },
            size: Pixels(if candidate.hop == 0 { 12.0 } else { 10.5 }),
            horizontal_alignment: if align_right { iced::alignment::Horizontal::Right } else { iced::alignment::Horizontal::Left },
            vertical_alignment: iced::alignment::Vertical::Center,
            ..canvas::Text::default()
        });
    }
}

/// Minimum on-screen radius a bubble needs before its label is drawn —
/// below this a repo's name+count just overlaps its neighbours' into an
/// illegible mess (the same complaint this whole view exists to fix,
/// reappearing one level up for a long tail of small repos). The bubble
/// itself — the size comparison the scope is actually for — still draws at
/// any size; only the text is skipped.
const CLUSTER_LABEL_MIN_RADIUS_PX: f32 = 22.0;

/// Rough average glyph width at the cluster label's 12px size, for clipping
/// a label to what its own bubble can actually hold. The canvas has no text
/// measurement available outside `Renderer`, so this is an approximation —
/// good enough to stop a long label running into a neighbouring bubble,
/// which pixel-perfect sizing wouldn't meaningfully improve on.
const CLUSTER_LABEL_CHAR_WIDTH_PX: f32 = 6.5;

/// Clips `label (count)` to fit inside a bubble of `radius_px`, dropping the
/// count first and then truncating the label with an ellipsis — fixes
/// labels colliding into neighbouring bubbles ("rnd-eks-manifests (76)"
/// running into "ninox (71)") instead of just suppressing them outright.
fn cluster_label_text(label: &str, count: usize, radius_px: f32) -> String {
    let usable_px = radius_px * 2.0 * 0.82;
    let full = format!("{label} ({count})");
    if full.chars().count() as f32 * CLUSTER_LABEL_CHAR_WIDTH_PX <= usable_px {
        return full;
    }
    let label_budget_px = (usable_px - CLUSTER_LABEL_CHAR_WIDTH_PX * 2.0).max(CLUSTER_LABEL_CHAR_WIDTH_PX * 3.0);
    let max_chars = (label_budget_px / CLUSTER_LABEL_CHAR_WIDTH_PX).floor().max(1.0) as usize;
    truncate_label(label, max_chars)
}

fn draw_clusters(frame: &mut Frame, clusters: &[ClusterBubble], camera: &Camera, color_by: ColorBy) {
    let colors = repo_colors(clusters);
    for bubble in clusters {
        let p = camera.to_screen(bubble.x, bubble.y);
        let r = bubble.radius * LAYOUT_SCALE * camera.zoom;
        let color = match color_by {
            ColorBy::Kind => super::brain_list::kind_color(&bubble.dominant_kind),
            ColorBy::Repo => repo_color(&colors, &bubble.label),
        };
        frame.fill(&Path::circle(p, r), Color { a: 0.28, ..color });
        frame.stroke(&Path::circle(p, r), Stroke::default().with_color(color).with_width(1.5));
        if r >= CLUSTER_LABEL_MIN_RADIUS_PX {
            frame.fill_text(canvas::Text {
                content: cluster_label_text(&bubble.label, bubble.count, r),
                position: p,
                color: tokens::TEXT,
                size: Pixels(12.0),
                horizontal_alignment: iced::alignment::Horizontal::Center,
                vertical_alignment: iced::alignment::Vertical::Center,
                ..canvas::Text::default()
            });
        }
    }
}

pub struct GraphCanvas<'a> {
    pub app: &'a App,
}

#[derive(Default)]
pub struct GraphCanvasState {
    dragging: bool,
    drag_last: Option<Point>,
    last_click: Option<(usize, std::time::Instant)>,
}

impl<'a> GraphCanvas<'a> {
    fn camera(&self, bounds: Rectangle) -> Camera {
        let g = &self.app.brain_view.graph;
        Camera { center: Point::new(bounds.width / 2.0, bounds.height / 2.0), zoom: g.zoom, pan: Vector::new(g.pan_x, g.pan_y) }
    }

    fn hit_test_node(&self, bounds: Rectangle, cursor: Point) -> Option<usize> {
        let g = &self.app.brain_view.graph;
        let camera = self.camera(bounds);
        let (lx, ly) = camera.to_layout(cursor);
        let spatial = g.spatial.as_ref()?;
        let radius = HIT_RADIUS_PX / (LAYOUT_SCALE * camera.zoom);
        hit_test(&g.nodes, spatial, lx, ly, radius).map(|i| g.nodes[i].entry)
    }

    fn hit_test_cluster(&self, bounds: Rectangle, cursor: Point) -> Option<String> {
        let g = &self.app.brain_view.graph;
        let camera = self.camera(bounds);
        let (lx, ly) = camera.to_layout(cursor);
        g.clusters
            .iter()
            .find(|b| (b.x - lx).hypot(b.y - ly) <= b.radius)
            .map(|b| b.label.clone())
    }
}

impl<'a> canvas::Program<Message> for GraphCanvas<'a> {
    type State = GraphCanvasState;

    fn draw(
        &self,
        state: &GraphCanvasState,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let g = &self.app.brain_view.graph;
        let camera = self.camera(bounds);

        let static_geometry = g.cache.draw(renderer, bounds.size(), |frame| {
            if g.scope == GraphScope::Clusters {
                draw_clusters(frame, &g.clusters, &camera, g.color_by);
            } else {
                draw_graph(frame, g, &self.app.brain_view.entries, &camera, bounds.width);
            }
        });

        // Dynamic overlay — hover ring only, redrawn every call so hovering
        // never invalidates the cached static layer above.
        let mut overlay = Frame::new(renderer, bounds.size());
        if g.scope == GraphScope::Clusters {
            // Bubbles below `CLUSTER_LABEL_MIN_RADIUS_PX` draw no static
            // label (it wouldn't fit), so hovering floats one here instead
            // — on the dynamic layer, same reasoning as the node hover ring,
            // so moving the mouse never invalidates the cached bubbles.
            if let Some(bubble) = g.hovered_cluster.as_deref().and_then(|label| g.clusters.iter().find(|b| b.label == label)) {
                let p = camera.to_screen(bubble.x, bubble.y);
                let r = bubble.radius * LAYOUT_SCALE * camera.zoom;
                if r < CLUSTER_LABEL_MIN_RADIUS_PX {
                    overlay.fill_text(canvas::Text {
                        content: format!("{} ({})", bubble.label, bubble.count),
                        position: Point::new(p.x, p.y - r - 8.0),
                        color: tokens::TEXT,
                        size: Pixels(12.0),
                        horizontal_alignment: iced::alignment::Horizontal::Center,
                        vertical_alignment: iced::alignment::Vertical::Bottom,
                        ..canvas::Text::default()
                    });
                }
            }
        } else if let Some(node) = g.hovered.and_then(|h| g.nodes.iter().find(|n| n.entry == h)) {
            let p = camera.to_screen(node.x, node.y);
            overlay.stroke(
                &Path::circle(p, node_radius(node.degree) * camera.zoom + 5.0),
                Stroke::default().with_color(tokens::TEXT).with_width(1.5),
            );
        }
        let _ = state;

        vec![static_geometry, overlay.into_geometry()]
    }

    fn update(
        &self,
        state: &mut GraphCanvasState,
        event: canvas::Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> (canvas::event::Status, Option<Message>) {
        let canvas::Event::Mouse(mouse_event) = event else {
            return (canvas::event::Status::Ignored, None);
        };
        let Some(pos) = cursor.position_in(bounds) else {
            return (canvas::event::Status::Ignored, None);
        };
        let is_clusters = self.app.brain_view.graph.scope == GraphScope::Clusters;

        match mouse_event {
            mouse::Event::WheelScrolled { delta } if !is_clusters => {
                let amount = match delta {
                    mouse::ScrollDelta::Lines { y, .. } => y,
                    mouse::ScrollDelta::Pixels { y, .. } => y / 40.0,
                };
                (canvas::event::Status::Captured, Some(Message::BrainGraphZoom(amount * 0.1)))
            }
            mouse::Event::ButtonPressed(mouse::Button::Left) if !is_clusters => {
                state.dragging = true;
                state.drag_last = Some(pos);
                (canvas::event::Status::Captured, None)
            }
            mouse::Event::CursorMoved { .. } => {
                if !is_clusters && state.dragging {
                    if let Some(last) = state.drag_last {
                        let camera = self.camera(bounds);
                        let (dx, dy) = ((pos.x - last.x) / (LAYOUT_SCALE * camera.zoom), (pos.y - last.y) / (LAYOUT_SCALE * camera.zoom));
                        state.drag_last = Some(pos);
                        return (canvas::event::Status::Captured, Some(Message::BrainGraphPan(dx, dy)));
                    }
                }
                if is_clusters {
                    let hit = self.hit_test_cluster(bounds, pos);
                    return if hit == self.app.brain_view.graph.hovered_cluster {
                        (canvas::event::Status::Ignored, None)
                    } else {
                        (canvas::event::Status::Ignored, Some(Message::BrainGraphHoverCluster(hit)))
                    };
                }
                let hit = self.hit_test_node(bounds, pos);
                if hit == self.app.brain_view.graph.hovered {
                    (canvas::event::Status::Ignored, None)
                } else {
                    (canvas::event::Status::Ignored, Some(Message::BrainGraphHover(hit)))
                }
            }
            mouse::Event::ButtonReleased(mouse::Button::Left) => {
                state.dragging = false;
                state.drag_last = None;
                if is_clusters {
                    if let Some(repo) = self.hit_test_cluster(bounds, pos) {
                        return (canvas::event::Status::Captured, Some(Message::BrainGraphOpenCluster(repo)));
                    }
                    return (canvas::event::Status::Ignored, None);
                }
                if let Some(entry) = self.hit_test_node(bounds, pos) {
                    let now = std::time::Instant::now();
                    let is_double = state
                        .last_click
                        .as_ref()
                        .is_some_and(|&(id, at)| id == entry && now.duration_since(at) < DOUBLE_CLICK_WINDOW);
                    state.last_click = Some((entry, now));
                    return (
                        canvas::event::Status::Captured,
                        Some(if is_double { Message::BrainGraphOpenInList(entry) } else { Message::BrainGraphFocusNode(entry) }),
                    );
                }
                (canvas::event::Status::Ignored, None)
            }
            _ => (canvas::event::Status::Ignored, None),
        }
    }

    fn mouse_interaction(&self, state: &GraphCanvasState, _bounds: Rectangle, _cursor: mouse::Cursor) -> mouse::Interaction {
        let g = &self.app.brain_view.graph;
        if state.dragging {
            mouse::Interaction::Grabbing
        } else if g.hovered.is_some() || g.hovered_cluster.is_some() {
            mouse::Interaction::Pointer
        } else {
            mouse::Interaction::default()
        }
    }
}

pub fn graph_canvas(app: &App) -> iced::Element<'_, Message> {
    canvas::Canvas::new(GraphCanvas { app }).width(iced::Length::Fill).height(iced::Length::Fill).into()
}

// ---------------------------------------------------------------------------
// Surrounding UI: scope/hops controls, filters panel, selected panel
// ---------------------------------------------------------------------------

use iced::widget::{button, column, container, row, scrollable, text, Space};
use iced::{Alignment, Background, Border, Length};

pub fn scope_seg(app: &App) -> iced::Element<'_, Message> {
    let scope = app.brain_view.graph.scope;
    super::fleet_islands::seg_control(vec![
        ("Neighbourhood".to_string(), scope == GraphScope::Neighbourhood, Message::BrainGraphSetScope(GraphScope::Neighbourhood)),
        ("Clusters".to_string(), scope == GraphScope::Clusters, Message::BrainGraphSetScope(GraphScope::Clusters)),
        ("Everything".to_string(), scope == GraphScope::Everything, Message::BrainGraphSetScope(GraphScope::Everything)),
    ])
}

pub fn hops_seg(app: &App) -> iced::Element<'_, Message> {
    let hops = app.brain_view.graph.hops;
    super::fleet_islands::seg_control(vec![
        ("1".to_string(), hops == 1, Message::BrainGraphSetHops(1)),
        ("2".to_string(), hops == 2, Message::BrainGraphSetHops(2)),
        ("3".to_string(), hops == 3, Message::BrainGraphSetHops(3)),
    ])
}

/// A labelled on/off switch, the segmented style Settings uses.
fn switch_row<'a>(label: &'static str, on: bool, msg: Message) -> iced::Element<'a, Message> {
    row![
        text(label).size(12).font(crate::style::SANS).color(tokens::TEXT),
        Space::new(Length::Fill, 0),
        super::fleet_islands::seg_control(vec![
            ("On".to_string(), on, if on { Message::Noop } else { msg.clone() }),
            ("Off".to_string(), !on, if on { msg } else { Message::Noop }),
        ]),
    ]
    .align_y(Alignment::Center)
    .padding([2, 6])
    .into()
}

/// One kind in "Show kinds": its colour dot, name and count. Clicking the
/// row toggles it; an off kind is dimmed (`BrainGraph.dc.html` `.kind.off`).
fn kind_row<'a>(key: &str, label: String, count: usize, on: bool) -> iced::Element<'a, Message> {
    let dot = super::brain_list::kind_color(key);
    button(
        row![
            text("●").size(9).color(if on { dot } else { Color { a: 0.35, ..dot } }),
            text(label).size(12).font(crate::style::SANS).color(if on { tokens::TEXT } else { tokens::DIM2 }),
            Space::new(Length::Fill, 0),
            text(count.to_string()).size(10.5).font(crate::style::MONO).color(tokens::DIM2),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    )
    .on_press(Message::BrainGraphToggleKind(key.to_string()))
    .padding([4, 6])
    .width(Length::Fill)
    .style(|_t, status| button::Style {
        background: Some(Background::Color(if matches!(status, button::Status::Hovered) { tokens::PANEL2 } else { Color::TRANSPARENT })),
        border: Border { radius: 6.0.into(), ..Default::default() },
        ..Default::default()
    })
    .into()
}

/// A repo filter chip: `.chip-repo`, or filled `.on` with ✕ while active.
fn repo_chip<'a>(name: String, active: bool) -> iced::Element<'a, Message> {
    const ON_BG: Color = rgb(0x1f, 0x3a, 0x6e);
    const ON_FG: Color = rgb(0xcf, 0xe0, 0xff);
    const REPO_BG: Color = rgb(0x14, 0x1c, 0x30);
    const REPO_BORDER: Color = rgb(0x2a, 0x3a, 0x63);
    let label: iced::Element<Message> = if active {
        row![
            text(name.clone()).size(10.5).font(crate::style::SANS).color(ON_FG),
            text("✕").size(9.5).font(crate::style::glyph_font_for("✕")).color(ON_FG),
        ]
        .spacing(5)
        .align_y(Alignment::Center)
        .into()
    } else {
        text(name.clone()).size(10.5).font(crate::style::SANS).color(tokens::BLUE).into()
    };
    button(label)
        .on_press(Message::BrainGraphSetRepoFilter(Some(name)))
        .padding([2, 9])
        .style(move |_t, _s| button::Style {
            background: Some(Background::Color(if active { ON_BG } else { REPO_BG })),
            border: Border { color: if active { ON_BG } else { REPO_BORDER }, width: 1.0, radius: 10.0.into() },
            ..Default::default()
        })
        .into()
}

fn panel_title<'a>(label: &'a str) -> iced::Element<'a, Message> {
    text(label.to_uppercase()).size(10.5).font(crate::style::SANS).color(tokens::DIM2).into()
}

/// Everything mode's kind/repo filters, "Hide unlinked", and "Freeze
/// layout" — 228px, matching the List view's Kinds column width.
pub fn filters_panel(app: &App) -> iced::Element<'_, Message> {
    let g = &app.brain_view.graph;
    let kinds = ninox_core::brain::kinds_sorted_by_count(&app.brain_view.entries);

    let mut rows: Vec<iced::Element<Message>> = vec![panel_title("Show kinds")];
    for (key, label, count) in &kinds {
        rows.push(kind_row(key, label.clone(), *count, g.visible_kinds.contains(key)));
    }

    let (repos, _) = ninox_core::brain::top_values(app.brain_view.entries.iter(), |e| &e.repos, 8);
    if !repos.is_empty() {
        rows.push(Space::new(0, 10).into());
        rows.push(panel_title("Repo"));
        let chips: Vec<iced::Element<Message>> = repos
            .iter()
            .map(|(name, _)| {
                let active = g.repo_filter.as_deref() == Some(name.as_str());
                repo_chip(name.clone(), active)
            })
            .collect();
        rows.push(row(chips).spacing(6).wrap().into());
    }

    rows.push(Space::new(0, 14).into());
    rows.push(panel_title("Graph"));
    rows.push(
        row![
            text("Colour by").size(12).font(crate::style::SANS).color(tokens::TEXT),
            Space::new(Length::Fill, 0),
            super::fleet_islands::seg_control(vec![
                ("Kind".to_string(), g.color_by == ColorBy::Kind, Message::BrainGraphColorBy(ColorBy::Kind)),
                ("Repo".to_string(), g.color_by == ColorBy::Repo, Message::BrainGraphColorBy(ColorBy::Repo)),
            ]),
        ]
        .align_y(Alignment::Center)
        .padding([2, 6])
        .into(),
    );
    rows.push(switch_row("Hide unlinked", g.hide_unlinked, Message::BrainGraphToggleHideUnlinked));
    rows.push(switch_row("Freeze layout", g.freeze_layout, Message::BrainGraphToggleFreezeLayout));

    container(scrollable(column(rows).spacing(4).padding([14, 10])))
        .width(Length::Fixed(228.0))
        .height(Length::Fill)
        .style(|_| container::Style {
            background: Some(Background::Color(tokens::PANEL)),
            border: Border { color: tokens::BORDER, width: 1.0, radius: 0.0.into() },
            ..Default::default()
        })
        .into()
}

fn zoom_controls<'a>() -> iced::Element<'a, Message> {
    let btn = |label: &'static str, msg: Message| {
        button(text(label).size(13).font(crate::style::SANS).color(tokens::TEXT))
            .on_press(msg)
            .padding([4, 10])
            .style(|_t, status| button::Style {
                background: Some(Background::Color(if matches!(status, button::Status::Hovered) { tokens::PANEL2 } else { tokens::PANEL })),
                border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
                text_color: tokens::TEXT,
                ..Default::default()
            })
    };
    container(
        column![
            btn("+", Message::BrainGraphZoom(0.2)),
            btn("−", Message::BrainGraphZoom(-0.2)),
            btn("fit", Message::BrainGraphFit),
        ]
        .spacing(2),
    )
    .padding(10)
    .into()
}

/// Clusters scope has no node selection of its own, so the selected panel
/// would otherwise just read "Click a node to inspect it." even though
/// there's nothing to click a node on — a ranked, clickable list of the
/// same bubbles the canvas is already showing fills that space usefully and
/// gives small, hard-to-click bubbles a reliable way in.
fn cluster_side_list(clusters: &[ClusterBubble]) -> iced::Element<'_, Message> {
    if clusters.is_empty() {
        return text("No repos to show.").size(12.5).font(crate::style::SANS).color(tokens::DIM).into();
    }
    let mut rows: Vec<iced::Element<Message>> = vec![panel_title("Clusters"), Space::new(0, 8).into()];
    for bubble in clusters {
        let color = super::brain_list::kind_color(&bubble.dominant_kind);
        rows.push(
            button(
                row![
                    text("●").size(8).color(color),
                    Space::new(6, 0),
                    text(bubble.label.clone()).size(12).font(crate::style::SANS).color(tokens::TEXT),
                    Space::new(Length::Fill, 0),
                    text(bubble.count.to_string()).size(10.5).font(crate::style::MONO).color(tokens::DIM2),
                ]
                .align_y(Alignment::Center)
                .width(Length::Fill),
            )
            .on_press(Message::BrainGraphOpenCluster(bubble.label.clone()))
            .padding([6, 8])
            .width(Length::Fill)
            .style(|_t, status| button::Style {
                background: Some(Background::Color(if matches!(status, button::Status::Hovered) { tokens::PANEL2 } else { Color::TRANSPARENT })),
                border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
                text_color: tokens::TEXT,
                ..Default::default()
            })
            .into(),
        );
    }
    column(rows).spacing(2).into()
}

/// The 340px selected-entry panel (brief: "kind, date, title, chips, a
/// summary (the first paragraph), cited by / links to, Open in list, Open
/// file").
fn selected_panel(app: &App) -> iced::Element<'_, Message> {
    let g = &app.brain_view.graph;
    // Neighbourhood always has a "current" entry — the focus it's centred
    // on — even when nothing has been explicitly clicked in the canvas yet
    // (e.g. arriving via "Show in graph"), so fall back to it rather than
    // showing the empty-state copy while a node is plainly focused.
    let implied = if g.scope == GraphScope::Neighbourhood { g.focus } else { None };
    let selected_index = g.graph_selected.or(implied);
    let selected = selected_index.and_then(|i| app.brain_view.entries.get(i));

    let content: iced::Element<Message> = match selected {
        None if g.scope == GraphScope::Clusters => cluster_side_list(&g.clusters),
        None => text("Click a node to inspect it.").size(12.5).font(crate::style::SANS).color(tokens::DIM).into(),
        Some(e) => {
            let key = ninox_core::brain::normalize_kind(&e.entry_type);
            let mut sections: Vec<iced::Element<Message>> = vec![
                row![
                    super::fleet_focus::badge(ninox_core::brain::kind_label(&key).to_uppercase(), Color { a: 0.18, ..super::brain_list::kind_color(&key) }, super::brain_list::kind_color(&key)),
                    Space::new(8, 0),
                    text(e.updated.clone().unwrap_or_default()).size(10.5).font(crate::style::MONO).color(tokens::DIM2),
                ]
                .align_y(Alignment::Center)
                .into(),
                Space::new(0, 8).into(),
                text(super::brain_panel::display_title(e)).size(15).font(crate::style::SANS).color(tokens::TEXT).into(),
                Space::new(0, 8).into(),
            ];

            let mut chips: Vec<iced::Element<Message>> = Vec::new();
            for repo in &e.repos {
                chips.push(text(repo.clone()).size(10.5).font(crate::style::SANS).color(tokens::BLUE).into());
            }
            for t in &e.tags {
                chips.push(text(t.clone()).size(10.5).font(crate::style::SANS).color(tokens::DIM).into());
            }
            if !chips.is_empty() {
                sections.push(row(chips).spacing(8).wrap().into());
                sections.push(Space::new(0, 10).into());
            }

            sections.push(panel_title("Summary"));
            let summary = e.body.split("\n\n").find(|p| !p.trim().is_empty() && !p.trim_start().starts_with('#')).unwrap_or("").trim();
            sections.push(text(summary.to_string()).size(11.5).font(crate::style::SANS).color(tokens::DIM).into());

            // Queried fresh per render rather than cached on an app-state
            // field: the panel only re-renders on an actual click (Graph
            // selection is cheap/rare compared to the List view's own
            // continuously-visible reading pane), so a cache would save a
            // small, already-indexed SQLite lookup at the cost of another
            // field to keep in sync.
            let backs = app.brain.backlinks(&e.id).unwrap_or_default();
            if !backs.is_empty() {
                sections.push(Space::new(0, 12).into());
                sections.push(text(format!("Cited by · {}", backs.len())).size(10).font(crate::style::SANS).color(tokens::DIM2).into());
                sections.push(row(backs.iter().map(|b| text(super::brain_panel::display_title(b)).size(11).font(crate::style::SANS).color(tokens::DIM).into()).collect::<Vec<_>>()).spacing(8).wrap().into());
            }
            let outs = app.brain.outlinks(&e.id).unwrap_or_default();
            if !outs.is_empty() {
                sections.push(Space::new(0, 12).into());
                sections.push(text(format!("Links to · {}", outs.len())).size(10).font(crate::style::SANS).color(tokens::DIM2).into());
                sections.push(row(outs.iter().map(|o| text(super::brain_panel::display_title(o)).size(11).font(crate::style::SANS).color(tokens::DIM).into()).collect::<Vec<_>>()).spacing(8).wrap().into());
            }

            sections.push(Space::new(0, 14).into());
            sections.push(
                row![
                    button(text("Open in list").size(11).font(crate::style::SANS).color(tokens::TEXT))
                        .on_press(Message::BrainGraphOpenInList(selected_index.expect("Some(e) above came from selected_index")))
                        .padding([5, 10])
                        .style(|_t, _s| button::Style { background: Some(Background::Color(tokens::PANEL2)), border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() }, text_color: tokens::TEXT, ..Default::default() }),
                    button(text("Open file").size(11).font(crate::style::SANS).color(tokens::TEXT))
                        .on_press(Message::OpenInEditor(app.brain.file_path(&e.id).to_string_lossy().to_string()))
                        .padding([5, 10])
                        .style(|_t, _s| button::Style { background: Some(Background::Color(tokens::PANEL2)), border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() }, text_color: tokens::TEXT, ..Default::default() }),
                ]
                .spacing(8)
                .into(),
            );

            column(sections).into()
        }
    };

    container(scrollable(container(content).padding([16, 18])))
        .width(Length::Fixed(340.0))
        .height(Length::Fill)
        .style(|_| container::Style {
            background: Some(Background::Color(tokens::PANEL)),
            border: Border { color: tokens::BORDER, width: 1.0, radius: 0.0.into() },
            ..Default::default()
        })
        .into()
}

/// Entries matching the empty state's search, best first (arbitrary cap).
pub fn search_entries(entries: &[BrainEntry], query: &str) -> Vec<usize> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<(bool, usize)> = entries
        .iter()
        .enumerate()
        .filter_map(|(i, e)| {
            let title = super::brain_panel::display_title(e).to_lowercase();
            (title.contains(&q) || e.id.to_lowercase().contains(&q)).then_some((!title.starts_with(&q), i))
        })
        .collect();
    hits.sort();
    hits.into_iter().take(8).map(|(_, i)| i).collect()
}

/// Neighbourhood with no centre (a brain without links): never a blank
/// canvas, but a way to pick one.
fn empty_neighbourhood(app: &App) -> iced::Element<'_, Message> {
    let g = &app.brain_view.graph;
    let mut col = column![
        text("Pick an entry in List or search").size(13).font(crate::style::SANS).color(tokens::TEXT),
        text("Neighbourhood centres on one entry and its links.").size(11).font(crate::style::SANS).color(tokens::DIM2),
        iced::widget::text_input("Search names", &g.search)
            .on_input(Message::BrainGraphSearch)
            .size(12)
            .font(crate::style::SANS)
            .padding([6, 8])
            .width(Length::Fixed(280.0))
            .style(|_t, status| iced::widget::text_input::Style {
                background: Background::Color(tokens::PANEL2),
                border: Border { color: if matches!(status, iced::widget::text_input::Status::Focused) { tokens::TEAL } else { tokens::BORDER }, width: 1.0, radius: 6.0.into() },
                icon: tokens::DIM,
                placeholder: tokens::DIM,
                value: tokens::TEXT,
                selection: tokens::TEAL,
            }),
    ]
    .spacing(8)
    .align_x(Alignment::Center);
    for i in search_entries(&app.brain_view.entries, &g.search) {
        let title = super::brain_panel::display_title(&app.brain_view.entries[i]);
        col = col.push(
            button(text(truncate_label(&title, 40)).size(12).font(crate::style::SANS).color(tokens::TEXT))
                .on_press(Message::BrainGraphFocusNode(i))
                .padding([4, 10])
                .width(Length::Fixed(280.0))
                .style(|_t, status| button::Style {
                    background: matches!(status, button::Status::Hovered).then_some(Background::Color(tokens::PANEL2)),
                    border: Border { radius: 5.0.into(), ..Default::default() },
                    text_color: tokens::TEXT,
                    ..Default::default()
                }),
        );
    }
    container(col).width(Length::Fill).height(Length::Fill).center_x(Length::Fill).center_y(Length::Fill).into()
}

fn graph_body(app: &App) -> iced::Element<'_, Message> {
    let g = &app.brain_view.graph;
    let canvas: iced::Element<Message> = if g.scope == GraphScope::Neighbourhood && g.focus.is_none() {
        empty_neighbourhood(app)
    } else {
        graph_canvas(app)
    };
    let canvas_area = container(iced::widget::stack![
        canvas,
        scope_controls_corner(app),
        zoom_controls_corner(),
        minimap_corner(app),
    ])
    .width(Length::Fill)
    .height(Length::Fill);
    row![filters_panel(app), canvas_area, selected_panel(app)].width(Length::Fill).height(Length::Fill).into()
}

fn zoom_controls_corner<'a>() -> iced::Element<'a, Message> {
    container(zoom_controls())
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(iced::alignment::Horizontal::Left)
        .align_y(iced::alignment::Vertical::Bottom)
        .into()
}

const MINIMAP_SIZE: (f32, f32) = (180.0, 110.0);

/// The "Whole brain" minimap: every repo as a small bubble, the one the
/// current scope is centred in highlighted and labelled "you are here",
/// click-to-jump to any other (brief: navigation). Suppressed in Clusters
/// scope itself, where the main canvas already shows the same thing full
/// size.
struct MinimapCanvas<'a> {
    app: &'a App,
}

impl<'a> MinimapCanvas<'a> {
    /// The repo this minimap should highlight as "you are here": the
    /// active Everything repo filter, else the focused entry's first repo.
    fn here(&self) -> Option<String> {
        let g = &self.app.brain_view.graph;
        if let Some(r) = &g.repo_filter {
            return Some(r.clone());
        }
        g.focus.and_then(|i| self.app.brain_view.entries.get(i)).and_then(|e| e.repos.first().cloned())
    }

    /// Repos with at least one entry in the current view.
    fn in_view(&self) -> HashSet<String> {
        let entries = &self.app.brain_view.entries;
        self.app.brain_view.graph.nodes.iter().filter_map(|n| entries.get(n.entry)).flat_map(|e| e.repos.iter().cloned()).collect()
    }

    fn fitted_bubbles(&self) -> Vec<ClusterBubble> {
        let bubbles = self.app.brain_view.graph.minimap.clone();
        let items: Vec<(f32, f32, f32)> = bubbles.iter().map(|b| (b.x, b.y, b.radius)).collect();
        let (zoom, pan_x, pan_y) = fit_transform(&items);
        // `fit_transform`'s zoom is calibrated for `Camera::to_screen`, which
        // multiplies by `LAYOUT_SCALE` as well as zoom — omitting that factor
        // here (as a previous version did) collapses every bubble to within
        // a pixel of the center, which read as "draws almost nothing".
        // `fit_transform` sizes for the full-size canvas (`ASSUMED_CANVAS_SIZE`)
        // — rescale its zoom down to this minimap's own much smaller area.
        let rescale = (MINIMAP_SIZE.0 / ASSUMED_CANVAS_SIZE.0).min(MINIMAP_SIZE.1 / ASSUMED_CANVAS_SIZE.1);
        let scale = zoom * LAYOUT_SCALE * rescale;
        bubbles
            .into_iter()
            .map(|b| ClusterBubble { x: (b.x + pan_x) * scale, y: (b.y + pan_y) * scale, radius: b.radius * scale, ..b })
            .collect()
    }
}

impl<'a> canvas::Program<Message> for MinimapCanvas<'a> {
    type State = ();

    fn draw(&self, _state: &(), renderer: &Renderer, _theme: &Theme, bounds: Rectangle, _cursor: mouse::Cursor) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let center = Point::new(bounds.width / 2.0, bounds.height / 2.0);
        let here = self.here();
        let in_view = self.in_view();
        let colors = repo_colors(&self.app.brain_view.graph.minimap);
        for bubble in self.fitted_bubbles() {
            let p = Point::new(center.x + bubble.x, center.y + bubble.y);
            let is_here = here.as_deref() == Some(bubble.label.as_str());
            let fill = if is_here {
                tokens::TEAL
            } else if in_view.contains(&bubble.label) {
                let c = match self.app.brain_view.graph.color_by {
                    ColorBy::Kind => super::brain_list::kind_color(&bubble.dominant_kind),
                    ColorBy::Repo => repo_color(&colors, &bubble.label),
                };
                Color { a: 0.85, ..c }
            } else {
                Color { a: 0.3, ..tokens::DIM }
            };
            frame.fill(&Path::circle(p, bubble.radius.max(2.0)), fill);
            if is_here {
                frame.stroke(&Path::circle(p, bubble.radius.max(2.0) + 2.0), Stroke::default().with_color(tokens::TEAL).with_width(1.5));
                frame.fill_text(canvas::Text {
                    content: "you are here".to_string(),
                    position: Point::new(p.x, p.y + bubble.radius.max(2.0) + 10.0),
                    color: tokens::DIM,
                    size: Pixels(8.5),
                    horizontal_alignment: iced::alignment::Horizontal::Center,
                    ..canvas::Text::default()
                });
            }
        }
        vec![frame.into_geometry()]
    }

    fn update(&self, _state: &mut (), event: canvas::Event, bounds: Rectangle, cursor: mouse::Cursor) -> (canvas::event::Status, Option<Message>) {
        let canvas::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) = event else {
            return (canvas::event::Status::Ignored, None);
        };
        let Some(pos) = cursor.position_in(bounds) else {
            return (canvas::event::Status::Ignored, None);
        };
        let center = Point::new(bounds.width / 2.0, bounds.height / 2.0);
        let (lx, ly) = (pos.x - center.x, pos.y - center.y);
        if let Some(bubble) = self.fitted_bubbles().into_iter().find(|b| (b.x - lx).hypot(b.y - ly) <= b.radius.max(4.0)) {
            return (canvas::event::Status::Captured, Some(Message::BrainGraphOpenCluster(bubble.label)));
        }
        (canvas::event::Status::Ignored, None)
    }

    fn mouse_interaction(&self, _state: &(), bounds: Rectangle, cursor: mouse::Cursor) -> mouse::Interaction {
        let Some(pos) = cursor.position_in(bounds) else { return mouse::Interaction::default() };
        let center = Point::new(bounds.width / 2.0, bounds.height / 2.0);
        let (lx, ly) = (pos.x - center.x, pos.y - center.y);
        if self.fitted_bubbles().into_iter().any(|b| (b.x - lx).hypot(b.y - ly) <= b.radius.max(4.0)) {
            mouse::Interaction::Pointer
        } else {
            mouse::Interaction::default()
        }
    }
}

fn minimap_toggle<'a>(label: &'static str) -> iced::Element<'a, Message> {
    button(text(label).size(9).font(crate::style::glyph_font_for(label)).color(tokens::DIM))
        .on_press(Message::BrainGraphToggleMinimap)
        .padding([1, 5])
        .style(|_t, status| button::Style {
            background: matches!(status, button::Status::Hovered).then_some(Background::Color(tokens::PANEL2)),
            border: Border { color: tokens::BORDER, width: 1.0, radius: 4.0.into() },
            ..Default::default()
        })
        .into()
}

fn corner(inner: iced::Element<'_, Message>) -> iced::Element<'_, Message> {
    container(inner)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(14)
        .align_x(iced::alignment::Horizontal::Right)
        .align_y(iced::alignment::Vertical::Bottom)
        .into()
}

fn minimap_corner(app: &App) -> iced::Element<'_, Message> {
    if app.brain_view.graph.scope == GraphScope::Clusters {
        return Space::new(0, 0).into();
    }
    if app.config.brain_minimap_collapsed {
        let pill = container(
            row![text("WHOLE BRAIN").size(9).font(crate::style::SANS).color(tokens::DIM2), minimap_toggle("▴")]
                .spacing(8)
                .align_y(Alignment::Center),
        )
        .padding([5, 8])
        .style(|_| container::Style {
            background: Some(Background::Color(Color { a: 0.94, ..tokens::PANEL })),
            border: Border { color: tokens::BORDER, width: 1.0, radius: 8.0.into() },
            ..Default::default()
        });
        return corner(pill.into());
    }
    let inner = container(
        column![
            row![
                text("WHOLE BRAIN").size(9).font(crate::style::SANS).color(tokens::DIM2),
                Space::new(Length::Fill, 0),
                text("click a cluster").size(8.5).font(crate::style::SANS).color(tokens::DIM2),
                Space::new(6, 0),
                minimap_toggle("▾"),
            ]
            // Without a fixed width here, the header row's `Fill` space
            // propagates up through the otherwise-`Shrink` container below
            // and stretches the whole minimap box across most of the
            // canvas instead of staying a compact corner overlay — which
            // also pushed it into the zoom buttons' bottom-left corner.
            .width(Length::Fixed(MINIMAP_SIZE.0))
            .align_y(Alignment::Center),
            canvas::Canvas::new(MinimapCanvas { app }).width(Length::Fixed(MINIMAP_SIZE.0)).height(Length::Fixed(MINIMAP_SIZE.1)),
        ]
        .spacing(6)
        .padding(10),
    )
    .style(|_| container::Style {
        background: Some(Background::Color(Color { a: 0.94, ..tokens::PANEL })),
        border: Border { color: tokens::BORDER, width: 1.0, radius: 8.0.into() },
        ..Default::default()
    });
    corner(inner.into())
}

/// Scope/Hops controls plus the "N of M shown" count, as an overlay on the
/// canvas itself (top-left) — `BrainGraph.dc.html`'s own layout, and the
/// fix for the toolbar overflowing at 1024px when these lived there
/// instead.
fn scope_controls_corner(app: &App) -> iced::Element<'_, Message> {
    let g = &app.brain_view.graph;
    let shown = if g.scope == GraphScope::Clusters { g.clusters.len() } else { g.nodes.len() };
    let total = app.brain_view.entries.len();

    let mut controls_row = row![scope_seg(app)].spacing(10).align_y(Alignment::Center);
    if g.scope != GraphScope::Clusters {
        controls_row = controls_row.push(text("Hops").size(11).font(crate::style::SANS).color(tokens::DIM));
        controls_row = controls_row.push(hops_seg(app));
    }

    // Each status line gets its own row rather than sharing one with
    // `controls_row`: packed into a single `row!`, the segmented controls'
    // width left so little room for the trailing text that it wrapped
    // word-by-word into a multi-line column instead of staying on one line.
    let mut content = column![controls_row].spacing(6);
    content = content.push(text(format!("{shown} of {total} shown")).size(10.5).font(crate::style::SANS).color(tokens::DIM2));
    if g.hidden_neighbourhood_count > 0 {
        content = content.push(
            text(format!("+{} more — raise hops or open in list", g.hidden_neighbourhood_count))
                .size(10.5)
                .font(crate::style::SANS)
                .color(tokens::DIM2),
        );
    }

    let overlay = container(content.padding([8, 10]))
        .style(|_| container::Style {
            background: Some(Background::Color(Color { a: 0.94, ..tokens::PANEL })),
            border: Border { color: tokens::BORDER, width: 1.0, radius: 8.0.into() },
            ..Default::default()
        });
    container(overlay)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(14)
        .align_x(iced::alignment::Horizontal::Left)
        .align_y(iced::alignment::Vertical::Top)
        .into()
}

pub fn graph_view(app: &App) -> iced::Element<'_, Message> {
    super::brain_list::toolbar_wrap(app, graph_body(app))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn entry(id: &str) -> BrainEntry {
        BrainEntry { id: id.into(), entry_type: "concepts".into(), name: id.into(), tags: vec![], repos: vec![], updated: None, body: String::new(), source: String::new() }
    }

    // `neighbourhood`/`neighbourhood_capped`/`cluster_by_repo` are
    // unit-tested in `ninox_core::brain` — this module only tests the
    // rendering-side logic built on top of them.

    #[test]
    fn layout_is_deterministic_across_runs() {
        let entries = vec![entry("a.md"), entry("b.md"), entry("c.md")];
        let edges = [(0, 1), (1, 2)];
        let ids = [0, 1, 2];
        let first = layout(&ids, &edges, &entries);
        let second = layout(&ids, &edges, &entries);
        assert_eq!(first, second);
    }

    #[test]
    fn layout_separates_unconnected_nodes() {
        let entries = vec![entry("a.md"), entry("b.md")];
        let ids = [0, 1];
        let nodes = layout(&ids, &[], &entries);
        let dist = (nodes[0].x - nodes[1].x).hypot(nodes[0].y - nodes[1].y);
        assert!(dist > 0.1, "unconnected nodes should repel apart, got distance {dist}");
    }

    #[test]
    fn layout_never_overlaps_nodes_even_when_densely_connected() {
        // A small clique: every node linked to every other, which pulls
        // them as close together as the attraction force can manage —
        // exactly the case that used to settle into an overlapping blob
        // before the explicit collision pass.
        let entries: Vec<BrainEntry> = (0..12).map(|i| entry(&format!("e{i}.md"))).collect();
        let ids: Vec<usize> = (0..12).collect();
        let mut edges = Vec::new();
        for i in 0..12 {
            for j in (i + 1)..12 {
                edges.push((i, j));
            }
        }
        let nodes = layout(&ids, &edges, &entries);
        for i in 0..nodes.len() {
            for j in (i + 1)..nodes.len() {
                let dist = (nodes[i].x - nodes[j].x).hypot(nodes[i].y - nodes[j].y);
                let min_dist = (node_radius(nodes[i].degree) + node_radius(nodes[j].degree)) / LAYOUT_SCALE;
                assert!(
                    dist >= min_dist - 0.001,
                    "nodes {i} and {j} overlap: dist {dist}, min {min_dist}"
                );
            }
        }
    }

    #[test]
    fn layout_degree_counts_only_edges_within_the_subgraph() {
        let entries = vec![entry("a.md"), entry("b.md"), entry("c.md")];
        // c is connected to a node NOT in the laid-out subset (99) — that
        // edge must not inflate c's degree.
        let edges = [(0, 1), (2, 99)];
        let ids = [0, 1, 2];
        let nodes = layout(&ids, &edges, &entries);
        let degree_of = |entry: usize| nodes.iter().find(|n| n.entry == entry).unwrap().degree;
        assert_eq!(degree_of(0), 1);
        assert_eq!(degree_of(1), 1);
        assert_eq!(degree_of(2), 0);
    }

    #[test]
    fn layout_900_nodes_within_time_budget() {
        let entries: Vec<BrainEntry> = (0..900).map(|i| entry(&format!("entries/{i}.md"))).collect();
        let ids: Vec<usize> = (0..900).collect();
        // A sparse, realistic-ish edge set: each node links to the next
        // two, so there's real attraction work without it being trivially
        // edge-free.
        let edges: Vec<(usize, usize)> = (0..898).map(|i| (i, i + 1)).chain((0..897).map(|i| (i, i + 2))).collect();

        let start = Instant::now();
        let nodes = layout(&ids, &edges, &entries);
        let elapsed = start.elapsed();

        assert_eq!(nodes.len(), 900);
        // Before the grid-based repulsion/collision passes, this was an
        // all-pairs O(n²) scan per iteration/pass — 6.9s on CI's debug-build
        // runner (comfortably under 5s locally, which is why CI, not local
        // dev, caught the regression). The grid rewrite brought local debug
        // timing to ~0.6s and release to ~80ms; 3s leaves real margin for a
        // slower CI runner while still catching a real O(n²) regression.
        assert!(
            elapsed.as_secs_f32() < 3.0,
            "laying out 900 nodes took {elapsed:?}, expected well under 3s for a one-time, non-interactive layout pass"
        );
    }

    /// 900 nodes, ~2k edges in nine communities with a few bridges — the
    /// shape of a real brain. The layout must finish in budget, converge
    /// (edges near their ideal length) and visibly separate communities.
    #[test]
    fn layout_900_nodes_2k_edges_converges_and_separates_clusters() {
        const CLUSTERS: usize = 9;
        const PER: usize = 100;
        let entries: Vec<BrainEntry> = (0..CLUSTERS * PER).map(|i| entry(&format!("entries/{i}.md"))).collect();
        let ids: Vec<usize> = (0..CLUSTERS * PER).collect();
        // Like a real brain: random links within each community, a hub per
        // community linked from every fourth member, one bridge onward.
        let mut edges = Vec::new();
        for c in 0..CLUSTERS {
            let base = c * PER;
            for i in 0..PER {
                for salt in [1, 2] {
                    let j = (hash01(&format!("{c}-{i}"), salt) * PER as f32) as usize % PER;
                    if j != i {
                        edges.push((base + i, base + j));
                    }
                }
                if i % 4 == 0 && i != 0 {
                    edges.push((base + i, base));
                }
            }
            edges.push((base, ((c + 1) % CLUSTERS) * PER));
        }
        assert!(edges.len() >= 1950, "{}", edges.len());

        let start = Instant::now();
        let nodes = layout(&ids, &edges, &entries);
        let elapsed = start.elapsed();
        assert!(elapsed.as_secs_f32() < 3.0, "900 nodes / {} edges took {elapsed:?}", edges.len());

        let pos = |i: usize| (nodes[i].x, nodes[i].y);
        let mean_edge = edges.iter().map(|&(a, b)| (pos(a).0 - pos(b).0).hypot(pos(a).1 - pos(b).1)).sum::<f32>() / edges.len() as f32;
        let (min_x, max_x) = nodes.iter().fold((f32::MAX, f32::MIN), |(lo, hi), n| (lo.min(n.x), hi.max(n.x)));
        let (min_y, max_y) = nodes.iter().fold((f32::MAX, f32::MIN), |(lo, hi), n| (lo.min(n.y), hi.max(n.y)));
        let extent = (max_x - min_x).max(max_y - min_y);
        assert!(mean_edge < extent / 4.0, "converged edges are local: mean {mean_edge} vs layout extent {extent}");

        let centroid = |c: usize| {
            let (sx, sy) = (c * PER..(c + 1) * PER).fold((0.0, 0.0), |(x, y), i| (x + pos(i).0, y + pos(i).1));
            (sx / PER as f32, sy / PER as f32)
        };
        let centroids: Vec<(f32, f32)> = (0..CLUSTERS).map(centroid).collect();
        let spread = |c: usize| {
            (c * PER..(c + 1) * PER).map(|i| (pos(i).0 - centroids[c].0).hypot(pos(i).1 - centroids[c].1)).sum::<f32>() / PER as f32
        };
        let mean_spread = (0..CLUSTERS).map(spread).sum::<f32>() / CLUSTERS as f32;
        let mut nearest_gap = f32::MAX;
        for a in 0..CLUSTERS {
            for b in (a + 1)..CLUSTERS {
                nearest_gap = nearest_gap.min((centroids[a].0 - centroids[b].0).hypot(centroids[a].1 - centroids[b].1));
            }
        }
        assert!(
            nearest_gap > mean_spread * 1.5,
            "communities should separate: nearest centroid gap {nearest_gap} vs mean in-community spread {mean_spread}"
        );
    }

    #[test]
    fn layout_900_nodes_never_overlap_at_any_zoom() {
        // The orchestrator's own repro: Everything's collision pass only
        // protects a guarantee that holds if the drawn radius scales with
        // zoom exactly as position does (`draw_graph` now does this) — this
        // pins the guarantee at the layout level, independent of drawing.
        let entries: Vec<BrainEntry> = (0..900).map(|i| entry(&format!("entries/{i}.md"))).collect();
        let ids: Vec<usize> = (0..900).collect();
        let edges: Vec<(usize, usize)> = (0..898).map(|i| (i, i + 1)).chain((0..897).map(|i| (i, i + 2))).collect();
        let nodes = layout(&ids, &edges, &entries);

        for i in 0..nodes.len() {
            for j in (i + 1)..nodes.len() {
                let dist = (nodes[i].x - nodes[j].x).hypot(nodes[i].y - nodes[j].y);
                let min_dist = node_radius(nodes[i].degree) / LAYOUT_SCALE + node_radius(nodes[j].degree) / LAYOUT_SCALE + 2.0 / LAYOUT_SCALE;
                assert!(
                    dist >= min_dist - f32::EPSILON,
                    "nodes {i} and {j} overlap: centre distance {dist} < required {min_dist}"
                );
            }
        }
    }

    #[test]
    fn hit_test_finds_the_nearest_node_within_radius() {
        let nodes = vec![
            GraphNode { entry: 0, x: 0.0, y: 0.0, degree: 0 },
            GraphNode { entry: 1, x: 5.0, y: 5.0, degree: 0 },
        ];
        let index = SpatialIndex::build(&nodes, 1.0);
        assert_eq!(hit_test(&nodes, &index, 0.1, 0.1, 0.5), Some(0));
        assert_eq!(hit_test(&nodes, &index, 5.1, 5.1, 0.5), Some(1));
    }

    #[test]
    fn hit_test_returns_none_outside_radius() {
        let nodes = vec![GraphNode { entry: 0, x: 0.0, y: 0.0, degree: 0 }];
        let index = SpatialIndex::build(&nodes, 1.0);
        assert_eq!(hit_test(&nodes, &index, 10.0, 10.0, 0.5), None);
    }

    #[test]
    fn hit_test_at_scale_matches_a_linear_scan() {
        let entries: Vec<BrainEntry> = (0..900).map(|i| entry(&format!("entries/{i}.md"))).collect();
        let ids: Vec<usize> = (0..900).collect();
        let nodes = layout(&ids, &[], &entries);
        let index = SpatialIndex::build(&nodes, 0.2);

        for &(x, y) in &[(0.0, 0.0), (0.5, -0.3), (-0.8, 0.9)] {
            let via_index = hit_test(&nodes, &index, x, y, 0.1);
            let via_scan = nodes
                .iter()
                .enumerate()
                .map(|(i, n)| (i, (n.x - x).hypot(n.y - y)))
                .filter(|&(_, d)| d <= 0.1)
                .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
                .map(|(i, _)| i);
            assert_eq!(via_index, via_scan);
        }
    }

    #[test]
    fn fit_transform_centers_a_single_item() {
        let (zoom, pan_x, pan_y) = fit_transform(&[(3.0, -2.0, 0.1)]);
        assert!(zoom > 0.0);
        assert_eq!(pan_x, -3.0);
        assert_eq!(pan_y, 2.0);
    }

    #[test]
    fn fit_transform_shrinks_zoom_for_a_wide_spread() {
        let tight = fit_transform(&[(0.0, 0.0, 0.1), (1.0, 0.0, 0.1)]);
        let wide = fit_transform(&[(0.0, 0.0, 0.1), (20.0, 0.0, 0.1)]);
        assert!(wide.0 < tight.0, "a much wider spread should zoom out further: {wide:?} vs {tight:?}");
    }

    #[test]
    fn fit_transform_handles_an_empty_set() {
        assert_eq!(fit_transform(&[]), (1.0, 0.0, 0.0));
    }

    #[test]
    fn fit_transform_for_nodes_zooms_out_further_than_bare_node_fit_when_a_node_is_labelled() {
        let nodes = vec![
            GraphNode { entry: 0, x: 0.0, y: 0.0, degree: 1 },
            GraphNode { entry: 1, x: 3.0, y: 0.0, degree: 1 },
        ];
        let edges = vec![GraphEdge { a: 0, b: 1 }];
        let bare = fit_transform(&nodes.iter().map(|n| (n.x, n.y, 0.0)).collect::<Vec<_>>());
        let with_labels = fit_transform_for_nodes(&nodes, &edges, Some(0));
        assert!(
            with_labels.0 <= bare.0,
            "reserving label width should never zoom in further than the bare-node fit: {with_labels:?} vs {bare:?}"
        );
    }

    #[test]
    fn fit_transform_for_nodes_matches_bare_fit_with_no_focus() {
        let nodes = vec![GraphNode { entry: 0, x: 0.0, y: 0.0, degree: 1 }];
        assert_eq!(fit_transform_for_nodes(&nodes, &[], None), fit_transform(&[(0.0, 0.0, 0.0)]));
    }

    fn label(text: &str, x: f32, y: f32, hop: u8, degree: u32) -> LabelCandidate {
        LabelCandidate { text: text.into(), anchor: Point::new(x, y), node_r: 6.0, hop, degree, hovered: false, align_right: false }
    }

    #[test]
    fn place_labels_keeps_a_single_label_on_its_preferred_side() {
        assert_eq!(place_labels(&[label("solo", 0.0, 0.0, 0, 1)], &[]), vec![Some(false)]);
    }

    #[test]
    fn place_labels_never_overlaps_and_prefers_better_connected_neighbours() {
        // Three neighbours stacked on one spot: only one fits per side.
        let c = vec![
            label("focus-node", -200.0, 0.0, 0, 9),
            label("leaf-one", 0.0, 0.0, 1, 1),
            label("busy-hub", 0.0, 2.0, 1, 7),
            label("leaf-two", 0.0, 4.0, 1, 2),
        ];
        let sides = place_labels(&c, &[]);
        assert_eq!(sides[0], Some(false), "the focus is always labelled");
        assert_eq!(sides[2], Some(false), "the best-connected neighbour gets its preferred side");
        assert_eq!(sides[3], Some(true), "the next takes the other side");
        assert_eq!(sides[1], None, "no room left: skipped, not drawn on top");
        let boxes: Vec<_> = c.iter().zip(&sides).filter_map(|(c, s)| s.map(|s| c.bbox(s))).collect();
        for i in 0..boxes.len() {
            for j in (i + 1)..boxes.len() {
                assert!(!boxes_overlap(boxes[i], boxes[j]), "labels {i} and {j} overlap");
            }
        }
    }

    #[test]
    fn a_hovered_label_always_shows_and_labels_avoid_the_focus_node() {
        let mut c = vec![label("crowded", 0.0, 0.0, 1, 9), label("hovered", 0.0, 1.0, 2, 0)];
        c[1].hovered = true;
        let blocked = [c[0].bbox(false), c[0].bbox(true)];
        let sides = place_labels(&c, &blocked);
        assert!(sides[1].is_some(), "hover always shows its label");
        assert_eq!(sides[0], None, "a neighbour whose sides are both taken (here by the focus node) is skipped");
    }

    #[test]
    fn truncate_label_leaves_short_text_untouched() {
        assert_eq!(truncate_label("short", 32), "short");
    }

    #[test]
    fn truncate_label_clips_long_text_with_an_ellipsis() {
        let long = "a".repeat(40);
        let out = truncate_label(&long, 32);
        assert_eq!(out.chars().count(), 33);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn default_visible_kinds_excludes_minor_kinds() {
        let mut repo_entry = entry("repos/x.md");
        repo_entry.entry_type = "repos".into();
        let mut decision_entry = entry("decisions/x.md");
        decision_entry.entry_type = "decision".into();
        let visible = default_visible_kinds(&[repo_entry, decision_entry]);
        assert_eq!(visible, HashSet::from(["decisions".to_string()]));
    }

    fn filters<'a>(scope: GraphScope, focus: Option<usize>, hops: u8, hide_unlinked: bool, visible_kinds: &'a HashSet<String>, repo_filter: Option<&'a str>) -> GraphFilters<'a> {
        GraphFilters { scope, focus, hops, hide_unlinked, visible_kinds, repo_filter }
    }

    #[test]
    fn visible_node_ids_neighbourhood_is_empty_without_a_focus() {
        let entries = vec![entry("a.md")];
        let kinds = HashSet::new();
        let ids = visible_node_ids(&entries, &[], filters(GraphScope::Neighbourhood, None, 2, true, &kinds, None));
        assert!(ids.is_empty());
    }

    #[test]
    fn visible_node_ids_neighbourhood_matches_the_bfs() {
        let entries = vec![entry("a.md"), entry("b.md"), entry("c.md")];
        let edges = [(0, 1), (1, 2)];
        let kinds = HashSet::new();
        let ids = visible_node_ids(&entries, &edges, filters(GraphScope::Neighbourhood, Some(0), 1, true, &kinds, None));
        assert_eq!(ids, vec![0, 1]);
    }

    #[test]
    fn visible_node_ids_everything_hides_unlinked_by_default() {
        let mut a = entry("a.md");
        a.entry_type = "decisions".into();
        let mut b = entry("b.md");
        b.entry_type = "decisions".into();
        let kinds = HashSet::from(["decisions".to_string()]);
        let ids = visible_node_ids(&[a, b], &[(0, 1)], filters(GraphScope::Everything, None, 0, true, &kinds, None));
        assert_eq!(ids, vec![0, 1]);

        let mut c = entry("c.md");
        c.entry_type = "decisions".into();
        let ids_with_unlinked =
            visible_node_ids(&[entry("a.md"), c], &[], filters(GraphScope::Everything, None, 0, true, &kinds, None));
        assert!(ids_with_unlinked.is_empty(), "the only entry is unlinked and hide_unlinked is set");
    }

    #[test]
    fn visible_node_ids_everything_respects_kind_and_repo_filters() {
        let mut a = entry("a.md");
        a.entry_type = "decisions".into();
        a.repos = vec!["ninox".to_string()];
        let mut b = entry("b.md");
        b.entry_type = "errors".into();
        b.repos = vec!["ninox".to_string()];
        let entries = vec![a, b];
        let kinds = HashSet::from(["decisions".to_string()]);
        let ids = visible_node_ids(&entries, &[], filters(GraphScope::Everything, None, 0, false, &kinds, Some("ninox")));
        assert_eq!(ids, vec![0]);
    }

    #[test]
    fn visible_node_ids_clusters_scope_is_always_empty() {
        let entries = vec![entry("a.md")];
        let kinds = HashSet::new();
        let ids = visible_node_ids(&entries, &[], filters(GraphScope::Clusters, None, 0, false, &kinds, None));
        assert!(ids.is_empty());
    }

    #[test]
    fn cluster_label_text_fits_short_labels_unclipped() {
        assert_eq!(cluster_label_text("ninox", 71, 60.0), "ninox (71)");
    }

    #[test]
    fn cluster_label_text_clips_long_labels_to_fit_the_bubble() {
        let out = cluster_label_text("rnd-eks-manifests", 76, CLUSTER_LABEL_MIN_RADIUS_PX);
        let usable_px = CLUSTER_LABEL_MIN_RADIUS_PX * 2.0 * 0.82;
        assert!((out.chars().count() as f32 * CLUSTER_LABEL_CHAR_WIDTH_PX) <= usable_px, "label must fit the bubble it's drawn on: {out:?}");
        assert!(out.contains('…'), "a clipped label should be marked as clipped: {out:?}");
    }

    #[test]
    fn most_linked_picks_the_hub_and_none_without_links() {
        assert_eq!(most_linked(&[(0, 1), (0, 2), (3, 0), (1, 2)]), Some(0));
        assert_eq!(most_linked(&[]), None);
    }

    #[test]
    fn layout_keys_differ_by_node_set_and_edges_but_not_by_unrelated_edges() {
        let k = layout_key(&[0, 1, 2], &[(0, 1)]);
        assert_eq!(k, layout_key(&[0, 1, 2], &[(0, 1), (5, 6)]));
        assert_ne!(k, layout_key(&[0, 1, 2], &[(0, 1), (1, 2)]));
        assert_ne!(k, layout_key(&[0, 1, 3], &[(0, 1)]));
    }

    #[test]
    fn repo_colours_are_distinct_for_the_largest_clusters_and_grey_after() {
        let bubble = |label: &str| ClusterBubble { label: label.into(), count: 1, x: 0.0, y: 0.0, radius: 0.1, dominant_kind: String::new() };
        let mut clusters: Vec<ClusterBubble> = (0..12).map(|i| bubble(&format!("r{i}"))).collect();
        clusters.insert(3, bubble("(no repo)"));
        let colors = repo_colors(&clusters);
        let top: HashSet<String> = (0..9).map(|i| format!("{:?}", colors[&format!("r{i}")])).collect();
        assert_eq!(top.len(), 9, "the nine largest repos each get their own colour");
        assert_eq!(colors[&"r10".to_string()], tokens::GRAY);
        assert_eq!(repo_color(&colors, "(no repo)"), tokens::GRAY);
    }

    #[test]
    fn fitting_around_the_minimap_lifts_the_content_and_never_zooms_in() {
        let items = [(-1.0, -1.0, 0.0), (1.0, 1.0, 0.0)];
        let (z0, _, y0) = fit_transform_reserving(&items, FitReserve::default());
        let (z1, _, y1) = fit_transform_reserving(&items, FitReserve { top: 0.0, bottom: MINIMAP_RESERVE_PX });
        assert!(z1 <= z0, "less room never means a bigger zoom: {z1} vs {z0}");
        assert!(y1 < y0, "content moves up, out from behind the minimap: {y1} vs {y0}");
    }

    #[test]
    fn fitting_below_the_scope_overlay_lowers_the_content_and_never_zooms_in() {
        let items = [(-1.0, -1.0, 0.0), (1.0, 1.0, 0.0)];
        let (z0, _, y0) = fit_transform_reserving(&items, FitReserve::default());
        let (z1, _, y1) = fit_transform_reserving(&items, FitReserve { top: SCOPE_OVERLAY_RESERVE_PX, bottom: 0.0 });
        assert!(z1 <= z0, "less room never means a bigger zoom: {z1} vs {z0}");
        assert!(y1 > y0, "content moves down, out from under the overlay: {y1} vs {y0}");

        let both = FitReserve { top: SCOPE_OVERLAY_RESERVE_PX, bottom: MINIMAP_RESERVE_PX };
        let (z2, _, _) = fit_transform_reserving(&items, both);
        assert!(z2 <= z1, "both strips leave the least room: {z2} vs {z1}");
    }
}
