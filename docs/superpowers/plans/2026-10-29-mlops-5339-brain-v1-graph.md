# MLOPS-5339 follow-up: GUI Brain tab, PR2 — v1 Graph view

Spec: `BrainGraph.dc.html` under `.claude/design-ref/fleet-overview-v1/`
(parent checkout), brief `BRIEF-MLOPS-5339-brain.md` section 2. Stacked on
PR1 (List, merged as #266); rebased onto `origin/v1` directly once that
merged.

## Scope (PR2)

Replace the Graph placeholder with the real thing: three scopes
(Neighbourhood/Clusters/Everything), a deterministic one-time layout
instead of a continuous physics tick, a spatial index for hit-testing, and
an `iced::widget::canvas::Cache`'d static render layer — the brief's fix
for "the force directed graph... renders badly for me with ~900 nodes."

### Data model (`components/brain_graph.rs`)

- `GraphScope` (Neighbourhood default / Clusters / Everything), `GraphNode`/
  `GraphEdge` (laid-out positions, not raw entry data).
- `neighbourhood()`: BFS from a focus entry, bounded by hops (1-3, default
  2).
- `layout()`: deterministic (hash-seeded, never the clock/an RNG),
  fixed-iteration (120) force simulation — runs once per scope/filter
  change, never per frame ("the simulation stops after it settles").
  Benchmarked at ~105ms for 900 synthetic nodes in release mode
  (`layout_900_nodes_within_time_budget`).
- `SpatialIndex`: uniform grid, O(1)-average hit-testing instead of an
  O(n) scan — `hit_test_at_scale_matches_a_linear_scan` pins it against a
  linear scan at 900 nodes.
- `cluster_by_repo()`: one bubble per repo, sized by count (area, not
  radius, proportional), packed on deterministic shelves (not a grid sized
  to the single largest bubble, which pushed every smaller repo off
  screen in a real, skewed-size brain — see commit history for the
  before/after).
- `fit_transform()`: zoom/pan framing every laid-out item, used by the
  explicit Fit button and automatically on every scope change (not on
  every filter toggle, which would yank the view out from under someone
  mid-exploration).

### Rendering (`GraphCanvas`, `canvas::Program`)

Two-layer draw: `BrainGraphState.cache` holds the expensive static layer
(nodes, edges, labels), redrawn only when the canvas's pixel bounds change
or `cache.clear()` is called explicitly (scope/filter/zoom/pan changes);
a cheap per-frame overlay draws only the hover ring, so hovering never
invalidates the cache. Labels: always on the focus node and its 1-hop
neighbours, dim at 2-hop, none further out; flipped to the node's left
near the canvas's right edge so they can't run into the selected panel.
Cluster bubble labels are suppressed below a legible on-screen radius
instead of overlapping into a second hairball one level up.

### Interaction

Click focuses (re-centres Neighbourhood; just selects in Everything).
Double-click opens in List (hand-rolled, since `canvas::Program` only gets
raw button events). Esc returns to Clusters. Wheel zooms, drag pans,
+/−/fit buttons. A Clusters bubble click scopes Everything down to that
repo.

### Surrounding UI

Filters panel (228px, matching List's Kinds column width): kind
checkboxes (Repos + minor kinds off by default, brief), repo chips, Hide
unlinked (on by default), Freeze layout. Selected panel (340px): kind
badge, date, title, chips, summary (first non-heading paragraph),
Cited by / Links to (queried fresh per click, not cached — Graph selection
is rare compared to List's always-visible reading pane).

## Out of scope for this PR

Arrow-key link walking and the "whole brain" minimap (brief navigation
items) are not implemented — flagged for a follow-up rather than cut
silently. Sources, the TUI Brain tab, and the sync engine's semantics stay
out of scope per the shared brief.

## Test plan

- `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`.
- Manual: isolated instance (scratch db/config/orchestrator root, real
  brain content), window-only screenshots of all three scopes compared
  against `BrainGraph.dc.html`.
