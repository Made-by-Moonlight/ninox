# Fleet Overview Phase 1 (TUI + shared core) Implementation Plan

> **For agentic workers:** this plan is executed natively (no human reviewer
> available in this session — an autonomous worker task). Implemented
> directly, task by task, committing after each.

**Goal:** Ship the shared graph/tree/state data model plus a TUI-only
renderer of the four-region fleet-overview screen described in
`docs/superpowers/specs/2026-10-21-fleet-overview-ui-vision-design.md`,
extending (not replacing) the existing `rataflow`-based Graph tab.

**Spec:** `docs/superpowers/specs/2026-10-21-fleet-overview-ui-vision-design.md`
(primary) and `docs/superpowers/specs/2026-10-19-rataflow-fleet-graph-design.md`
(the graph foundation this extends).

## Citation spot-check against current `v1` (required before coding)

The fleet-overview spec and the task brief both cite things that do not
match current `v1`. Documenting rather than blocking, per the task's own
instruction:

- **No `worker_type` field exists.** `Session` in `crates/ninox-core/src/
  types.rs` has no `worker_type`, and there is no `worker_type.rs`/
  `WorkerTypeRegistry` anywhere. Commit `aab6322` ("Merge PR #203") only
  merged the worker-types-backend **design spec** doc, not an
  implementation; PR #208 does not exist on this branch. **Call:** the
  node-content model still has a `worker_type_badge: Option<String>` field
  (per the spec's "Node content" list), sourced as `None` for every real
  node today. It's real schema, just unpopulated until the backend ships —
  a smaller follow-on than redesigning the type later.
- **No live-flash mechanism exists in `graph.rs` on `v1`.** The sibling
  spec and the task brief describe it as "already shipping"
  (`graph-view-live-comms` branch). That branch exists but is **not
  merged into `v1`** (`git merge-base --is-ancestor graph-view-live-comms
  v1` fails; it has its own unmerged `message_events` table and flash
  rendering commits). **Call:** phase 1 cannot "reuse" a mechanism that
  isn't there. It implements the new transient-dashed-edge renderer (the
  genuinely in-scope part — the "no existing hierarchy edge" case) as a
  generic `flash` primitive on `GraphState`, demonstrated via an explicit
  demo trigger (see Task 4) rather than real message-event data (no
  message-event stream exists on `v1` yet). Flagged as follow-on: land
  `graph-view-live-comms` (or redo its `message_events` work against the
  current `ninox-tui`/`ninox-ui-shared` split) so parent↔child flashes and
  this new cross-tree edge type both have real triggers.
- **`fleet/snapshot.rs`'s `Role` enum** is `Orchestrator | Worker |
  Standalone` (not just worker/orchestrator) — the new node model's
  `NodeKind` maps `Standalone` to `Worker` for rendering purposes (a
  standalone session has no parent to show in the tree, but is otherwise a
  plain worker card).

## Architecture

**New crate `crates/ninox-overview`**, not a module, because: (a) the
spec's Goal #1 explicitly wants "one core drives both renderers" — a
future `ninox-gui` consumer needs to depend on this without pulling in all
of `ninox-tui` (ratatui/crossterm) or vice versa; (b) it has its own
non-trivial invariant (ghost nodes can't be communication-edge endpoints)
worth unit-testing in isolation from any rendering concern. It depends on
`ninox-core` (for `fleet::snapshot::FleetSnapshot`/`FleetMember`/`Role`)
and `serde`, nothing ratatui-specific.

It wraps `FleetSnapshot` rather than re-deriving fleet state: a pure
`from_snapshot(&FleetSnapshot) -> (Vec<OverviewNode>, Vec<OverviewEdge>)`
builds real nodes from `FleetMember` and hierarchy edges from
`session.orchestrator_id`. Ghost nodes/edges are added separately (plan
review, see below) — never derived from the snapshot, since nothing in
the snapshot represents a pending plan yet (auto-planning, item 9, is a
future consumer, not phase 1).

**TUI integration extends `crates/ninox-tui/src/tui/graph.rs`/`layout.rs`/
`view.rs`/`state.rs` in place** — the existing numbered "6 Graph" tab
becomes the four-region screen, rather than adding a new `View` variant.
The spec's skeleton (tree / canvas / prompt / detail / bottom controls) is
a superset of what `View::Graph` already has (canvas + a 2-line detail
strip); this plan grows that strip into a real right-side detail panel,
adds a left tree column and a prompt line, and turns `graph.rs`'s node
type into an enum so one `Flow` can hold both real and ghost nodes side
by side (so existing hierarchy/selection/viewport-carry-over code keeps
working unchanged).

## File structure

- `crates/ninox-overview/Cargo.toml`, `src/lib.rs` — new crate.
- `crates/ninox-overview/src/node.rs` — `NodeId`, `NodeKind`, `RealNode`,
  `GhostNode`, `OverviewNode` enum.
- `crates/ninox-overview/src/edge.rs` — `EdgeKind`, `OverviewEdge`
  (validating constructor).
- `crates/ninox-overview/src/tree.rs` — `OverviewTree`, breadcrumb/depth.
- `crates/ninox-overview/src/controls.rs` — `ControlAction`,
  `controls_for`.
- `crates/ninox-overview/src/plan.rs` — `GhostPlan`, `demo_plan`.
- `crates/ninox-overview/src/snapshot_adapter.rs` — `from_snapshot`.
- `crates/ninox-overview/src/state.rs` — `OverviewState` (aggregate).
- Modify `crates/ninox-tui/src/tui/graph.rs` — `OverviewNodeContent`
  enum, ghost rendering, `flash` transient-edge primitive, demo-plan
  wiring.
- Modify `crates/ninox-tui/src/tui/layout.rs` — `GraphLayout` grows
  `tree`, `prompt`, and a real `detail` panel region.
- Modify `crates/ninox-tui/src/tui/view.rs` — `draw_graph` renders the
  four regions; footer hints computed from `controls_for`.
- Modify `crates/ninox-tui/src/tui/state.rs` — `graph_key` handles
  ghost-plan-review keys and the prompt-input stub.
- Modify root `Cargo.toml` (workspace members), `crates/ninox-tui/
  Cargo.toml` (new dependency).

## Review focus

- A session with `orchestrator_id` pointing at a session not present in
  the snapshot (torn-down orchestrator) must not panic building hierarchy
  edges — skip it, matching `graph.rs::sync_fleet`'s existing
  dependency-edge skip pattern.
- Selecting a ghost node and then the plan being discarded must clear
  detail-panel/controls-bar state, not point at a dangling id.
- A `Standalone`-role member (no orchestrator) must still render in the
  tree as a root, not be dropped.
- Two ghost nodes in the same demo plan must get distinct ids even though
  they share a parent and worker type (id collision would make
  `Flow::with_graph` reject the whole graph, per the existing dependency-
  edge test precedent in `graph.rs`).
- `controls_for` on a plain worker must not include "view sub-tree" or
  any plan-review action; on an orchestrator with no pending plan it must
  not include plan-review actions either (only a node that is *itself* a
  ghost shows them, per the spec's "Plan review (ghost nodes)" section).

---

## Tasks

### Task 1: `ninox-overview` node/edge data model

**Files:** Create `crates/ninox-overview/{Cargo.toml,src/lib.rs,src/node.rs,src/edge.rs}`.

- `NodeId = String`.
- `NodeKind { Worker, Orchestrator }`.
- `RealNode { id: NodeId, kind: NodeKind, name: String, harness_badge: String, worker_type_badge: Option<String>, status: String, brief: Option<String>, depth: u32, breadcrumb: Vec<String>, activity: Option<String>, artifacts: Vec<String>, remote_badge: Option<RemoteBadge>, depth_capped: bool }`.
- `RemoteBadge { Connected, Reconnecting, Unreachable }`.
- `GhostNode { id: NodeId, parent_id: NodeId, worker_type: Option<String>, brief: String, dependencies: Vec<NodeId> }`.
- `OverviewNode { Real(RealNode), Ghost(GhostNode) }` with `id()`, `is_ghost()`.
- `EdgeKind { Hierarchy, Communication }`.
- `OverviewEdge::new(kind, from: NodeId, to: NodeId, nodes: &[OverviewNode]) -> Result<Self, GhostEdgeError>` — returns `Err` if `kind == Communication` and either endpoint resolves to a `Ghost` node in `nodes`.
- Unit tests: `ghost_node_cannot_be_a_communication_edge_endpoint` (both directions), `hierarchy_edge_to_a_ghost_is_allowed`, `real_to_real_communication_edge_is_allowed`.
- Commit: `feat(ninox-overview): node/edge data model with ghost invariant`.

### Task 2: Tree (breadcrumb/depth) and controls bar

**Files:** Create `src/tree.rs`, `src/controls.rs`.

- `OverviewTree::build(nodes: &[OverviewNode], edges: &[OverviewEdge]) -> OverviewTree` computes, per node id, `depth` (root = 0) and `breadcrumb` (root-to-node name chain) by walking `Hierarchy` edges; a node with no incoming hierarchy edge is a root.
- `ControlAction` enum: `ViewSubTree, Connect, Message, ApproveSpawnAll, ApproveSelectedOnly, AddSection, DiscardPlan`.
- `controls_for(node: &OverviewNode) -> Vec<ControlAction>`: `Ghost` → `[ApproveSpawnAll, ApproveSelectedOnly, AddSection, DiscardPlan]`; `Real` orchestrator → `[ViewSubTree, Connect, Message]`; `Real` worker → `[Connect, Message]`.
- Tests: depth-2 breadcrumb for `root -> mid -> leaf`; standalone (no incoming hierarchy edge) is depth 0; `controls_for` differs for worker vs orchestrator vs ghost (the "Review focus" item above).
- Commit: `feat(ninox-overview): tree depth/breadcrumb and context-controls`.

### Task 3: `FleetSnapshot` adapter and demo ghost plan

**Files:** Create `src/snapshot_adapter.rs`, `src/plan.rs`, `src/state.rs`.

**Interfaces consumed:** `ninox_core::fleet::snapshot::{FleetSnapshot, FleetMember, Role}` (existing); Task 1's `OverviewNode`/`RealNode`/`OverviewEdge`; Task 2's `OverviewTree`.

- `from_snapshot(snap: &FleetSnapshot) -> (Vec<OverviewNode>, Vec<OverviewEdge>)`: one `RealNode` per `FleetMember` (`NodeKind::Orchestrator` for `Role::Orchestrator`, else `Worker`; `worker_type_badge: None` per the spot-check above; `harness_badge: member.session.agent_type.clone()`); one `Hierarchy` edge per member whose `orchestrator_id` names another member present in `snap` (skip otherwise, per Review Focus).
- `GhostPlan { parent_id: NodeId, sections: Vec<GhostNode> }`.
- `demo_plan(parent_id: &str) -> GhostPlan`: two hardcoded `GhostNode`s (e.g. "research the rewrite", "implement the rewrite", second depending on the first), ids namespaced `format!("ghost:{parent_id}:{n}")` so two calls for different parents never collide.
- `OverviewState { nodes: Vec<OverviewNode>, edges: Vec<OverviewEdge>, tree: OverviewTree, selected: Option<NodeId>, active_plan: Option<GhostPlan> }` with `from_snapshot`, `select(id)`, `toggle_demo_plan(orchestrator_id)` (adds/removes the demo plan's ghost nodes + hierarchy edges to/from `nodes`/`edges`, clearing `selected` if it pointed at a now-removed ghost), `selected_node() -> Option<&OverviewNode>`, `controls() -> Vec<ControlAction>` (empty when nothing selected).
- Tests: `from_snapshot_skips_a_hierarchy_edge_to_a_missing_orchestrator` (Review Focus item); `toggle_demo_plan_is_idempotent_add_then_remove`; `selecting_a_discarded_ghost_clears_selection`.
- Commit: `feat(ninox-overview): FleetSnapshot adapter, OverviewState, demo ghost plan`.

### Task 4: Wire into the workspace; `graph.rs` gets ghost nodes + a transient flash edge

**Files:** Modify root `Cargo.toml`, `crates/ninox-tui/Cargo.toml`, `crates/ninox-tui/src/tui/graph.rs`.

**Interfaces consumed:** Task 3's `OverviewState`, `ninox_overview::node::OverviewNode`.

- Add `"crates/ninox-overview"` to workspace `members`; add `ninox-overview = { path = "../ninox-overview", version = "0.37.0" }` to `ninox-tui`'s deps.
- `graph.rs`: new `GhostNodeContent { label: String, worker_type: Option<String>, palette: Palette }` implementing `NodeContent::render` with a dashed border (`BorderType::Rounded` isn't dashed in ratatui — build the border manually with `Block::default().borders(Borders::ALL).border_style(Style::default().add_modifier(Modifier::DIM))` plus a literal `"╌"`-drawn top/bottom rule via a custom `Paragraph` line, and a `"GHOST"` badge span) — document the exact ratatui construction inline only if it's non-obvious from the code (this is the "your call on exact terminal rendering" the spec allows).
- `OverviewNodeContent { Session(SessionNodeContent), Ghost(GhostNodeContent) }` implementing `NodeContent` by dispatching `render`.
- Change `GraphState.fleet` to `RefCell<Flow<OverviewNodeContent, StepEdge>>`; `sync_fleet` wraps each node in `OverviewNodeContent::Session(...)` (existing behavior, now wrapped — the "must not regress" case from Step 5 of the task).
- `GraphState` gains `overview: ninox_overview::state::OverviewState`, kept in sync with `fleet`'s nodes each `sync_fleet` call (rebuilt via `OverviewState::from_snapshot`-equivalent built straight from `rows`/`deps` the same way `sync_fleet` already does, since `graph.rs` doesn't have a `FleetSnapshot` in scope — a `ninox_overview::snapshot_adapter::from_rows`-style second entry point, or simplest: construct `RealNode`s directly from `rows` inline in `sync_fleet` and hand them to `OverviewState::new(nodes, edges)`). Pick whichever keeps `sync_fleet`'s existing node/edge-building code the single source of truth — do not duplicate it.
- `GraphState::toggle_demo_plan()`: calls `self.overview.toggle_demo_plan(parent)` for the currently-focused orchestrator node, then re-inserts/removes the resulting ghost nodes into `self.fleet`'s `Flow` (as `OverviewNodeContent::Ghost` nodes plus `Hierarchy` `StepEdge`s from the parent) without rebuilding the whole graph (preserve viewport/selection via the existing `carry_over_viewport_and_selection` helper).
- `GraphState::flash(from: &str, to: &str)`: pushes a `(from, to, inserted_at: Instant)` into a `Vec` on `GraphState`; a `tick_flashes(now)` method (called from the TUI's existing 1s tick, same as `tick_drilldown`) drops entries older than 1s and (re)adds/removes their dashed `StepEdge`s in `self.fleet` accordingly — only for pairs with **no existing hierarchy edge** between them (check `rows`' `group`/id relationship before flashing; same-parent-child pairs are a documented no-op per the spot-check above, since there is no trigger to flash them from yet).
- Unit tests (extending the existing `#[cfg(test)] mod tests`): ghost nodes from `toggle_demo_plan` render with the `GHOST` badge (structural check, not full snapshot); `sync_fleet` still produces exactly the prior node/edge count for a plain fleet with no ghosts (the no-regression test from Step 5); `flash` between two nodes with an existing hierarchy edge is a no-op; `flash` between unrelated nodes adds then, after 1s, removes a dashed edge.
- Run `cargo test -p ninox-tui -p ninox-overview --lib`.
- Commit: `feat(ninox-tui): ghost-node rendering and transient cross-tree flash edges in the Graph tab`.

### Task 5: Four-region layout — tree, prompt, detail panel, bottom controls

**Files:** Modify `crates/ninox-tui/src/tui/layout.rs`, `view.rs`, `state.rs`.

**Interfaces consumed:** Task 3/4's `GraphState.overview: OverviewState`, `ControlAction`.

- `layout.rs`: `GraphLayout` gains `tree: Rect` (left, full body height, width matching `SIDEBAR_MIN..SIDEBAR_MAX` like the existing sidebar), `prompt: Rect` (one line, under `canvas`, above the old `detail` strip's position), `detail: Rect` becomes a right-side column (width `INSPECTOR_WIDTH`) instead of a bottom strip. `graph(body)` is rewritten accordingly; below `MIN_SPLIT_W` the tree takes priority (full width) and canvas/detail stack below it when focus is on the tree — mirror the existing `Board` view's narrow-width fallback idiom rather than inventing a new one.
- `state.rs`: `TuiState` (or `GraphState`) gains `prompt_input: String` and `prompt_editing: bool`; `graph_key` gains: a key to focus/edit the prompt (reuse whatever key `Brain`'s query-editing uses, e.g. `/`), `Enter` while editing appends `"not yet wired: <input>"` as a one-shot notice via `st.notify(Level::Info, ...)` and clears the input (the spec-mandated stub); ghost-plan-review keys only reachable when a ghost node is selected — `a` Approve & spawn all → `st.notify(Level::Info, "not yet wired to a planner")` (real spawning needs item 9, out of scope), `s` Approve selected only → same, `+` Add section → appends one more hardcoded `GhostNode` to the active plan via `OverviewState`, `d` Discard plan → `GraphState::toggle_demo_plan` again (removes it).
- `view.rs`: `draw_graph` renders `gl.tree` (walks `st.graph.overview.tree`, indenting by depth, a hollow marker for ghost rows per the spec's "tree shows proposed nodes with a hollow marker"), `gl.prompt` (the input line, cursor when editing), and the expanded `gl.detail` (all of `RealNode`'s/`GhostNode`'s fields, not just the 2-line summary). The footer hints arm for `(_, View::Graph, _)` is replaced with one computed from `st.graph.overview.controls()` (falling back to the existing nav hints when nothing is selected — the spec's open question on "no selection" default, resolved here as "keep today's nav hints").
- Tests: a structural TUI test (buffer-cell assertions, matching the existing `render_blocked`-style tests in `graph.rs`) confirming: all four regions have non-zero area for a normal body size; a ghost row in the tree renders its hollow marker; the footer hints differ when a plain worker vs. an orchestrator vs. a ghost node is selected.
- Commit: `feat(ninox-tui): four-region fleet-overview skeleton (tree/canvas/prompt/detail/controls)`.

### Task 6: Workspace-wide verification and follow-on requests

- `cargo build`, `cargo test --workspace`, `cargo clippy --workspace --all-targets` — all clean.
- Manually trace through `sync_fleet` call sites to confirm a fleet with zero ghosts/zero cross-tree messages renders byte-identical node/edge content to before (Task 4's regression test covers this mechanically; this step is the human-readable confirmation for the PR description).
- `ninox request-work` for each of: GUI (Iced) renderer of this screen; worker-type management screen UI; depth-cap UX polish beyond the basic marker; real ninox-prompt NLU wiring; compact/minimal node-card toggle; landing `graph-view-live-comms`'s `message_events`/flash work against the current crate split (the dependency this plan's spot-check surfaced).
- Commit (if anything changed, e.g. clippy fixes): `fix(fleet-overview): clippy/test cleanup`.
