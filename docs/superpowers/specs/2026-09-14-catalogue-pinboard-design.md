# Catalogue/Pinboard: shared sidebar, cross-highlighting, force-directed drag

**Date:** 2026-09-14
**Status:** Approved design, pre-implementation

## Problem

The brain view (`crates/ninox-app/src/components/brain_panel.rs`) has two
modes, Catalogue and Pinboard, that should feel like two views onto the same
specimen set but currently don't:

1. **Duplicated sidebars.** `catalogue_body()` uses `drawers_rail()` — a
   272px rail of collapsible per-category drawers. `pinboard_body()` builds
   its own separate 215px flat "category counts" rail inline. Same data,
   two different components, two different visual styles.
2. **No cross-highlighting.** `BrainViewState.selected`/`.hovered`
   (`app.rs:107-138`) already exist and are already the single source of
   truth for both modes, but they're only wired one-directionally: pinboard
   hover writes `.hovered` (`brain_pinboard.rs`'s `update()` emits
   `Message::BrainHoverEntry`), but the catalogue drawer's `dentry_row`
   never reads it, and the pinboard canvas never draws anything for
   `.selected` at all.
3. **The pinboard isn't actually a graph layout.** `Pinboard::nodes()`
   (`brain_pinboard.rs:53`) re-derives every node's position from a
   deterministic hash of its id, from scratch, on every single frame — by
   design, per that file's own doc comment, so window resizes can never
   leave stale/clipped positions. That property is good, but it also means
   there is no persistent position to move, so nothing can be dragged and
   there is no real force-directed layout, just a fixed scatter.
4. Catalogue rows carry no visual link to their pinboard-node identity
   (category color appears on drawer headers and pinboard nodes, but not on
   individual `dentry_row`s).

## Decisions (scope for this pass)

- **Unify on the drawer-style sidebar.** `pinboard_body()`'s inline flat
  rail is deleted; both modes call the same `drawers_rail(app)`. One
  component, one width (272px), one style (`heavy_frame`).
- **Layout stays resize-safe by staying normalized.** Rather than storing
  node positions in canvas pixels (which would need re-scaling logic on
  every resize), the new persistent layout stores positions/velocities in
  `[0,1]×[0,1]` unit-square coordinates, multiplied by `bounds` only at
  draw time — the same invariant the current hash-scatter already
  guarantees, just now backed by a stateful simulation instead of a
  stateless hash.
- **Live simulation, not settle-once.** Springs (along wikilink edges),
  inverse-square repulsion (all node pairs), a mild centering pull, and
  velocity damping run every tick, continuously, while pinboard mode is on
  screen. A quiescent graph asymptotically stops moving on its own (damping
  drives velocity to ~0); dragging or new entries perturb it and it
  re-settles live rather than needing a manual "re-layout" trigger.
- **Session-only drag persistence.** Dragging directly sets a node's
  position each frame; releasing hands it back to the simulation. Nothing
  is written to disk/config — a reindex, catalogue switch, or app restart
  reseeds from the same deterministic hash scatter used today, so a stale
  drag never survives past the session that made it.
- **Click vs. drag is threshold-based, not modifier-based.** A left-button
  press only becomes a drag once the cursor has moved past a small pixel
  tolerance while held; a press-and-release under that tolerance still
  fires `BrainSelectEntry`, preserving today's click-to-select behavior
  unchanged.
- **Physics only ticks while it's on screen.** A dedicated subscription
  drives `Message::BrainPhysicsTick`, gated on `view == View::Brain &&
  brain_view.mode == BrainMode::Pinboard`, so there's no background
  CPU/battery cost when the pinboard isn't visible.
- **Cross-highlight reuses the existing message pair, doesn't add new
  ones.** Sidebar rows gain `mouse_area` `on_enter`/`on_exit` emitting the
  same `Message::BrainHoverEntry` the canvas already emits; both `dentry_row`
  and the pinboard canvas read `app.brain_view.hovered`/`.selected` as their
  single shared source of truth for what to highlight, instead of each
  view only knowing about its own mouse.

## 1. Shared sidebar — `brain_panel.rs`

Delete the inline `rail` construction inside `pinboard_body()` (currently
`category_color` dots + counts, `brain_panel.rs:451-484`). Replace the call
site with `drawers_rail(app)`, identical to `catalogue_body()`. No new
component needed — `drawers_rail`, `drawer`, and `dentry_row` already take
`&App` and don't assume catalogue mode.

`dentry_row` gains a category-color dot (mirroring the pinboard node's
`category_color`) ahead of the entry name, so a row visually matches its
pinboard specimen — the concrete piece of "catalogue polish" from the
brainstorm.

## 2. Cross-highlighting — `brain_panel.rs` + `brain_pinboard.rs`

**Sidebar → pinboard (new):** wrap each `dentry_row`'s button in
`mouse_area(...).on_enter(Message::BrainHoverEntry(Some(id))).on_exit(Message::BrainHoverEntry(None))`.
This is the same message the canvas already sends on `CursorMoved`, so no
new state or handler is needed in `app.rs` — only a new emitter.

**Pinboard → sidebar (new):** `dentry_row` currently only styles on
`is_selected` and its own button-hover `Status`. Add a third check,
`is_cross_hovered = app.brain_view.hovered.as_deref() == Some(&entry.id)`,
rendered as a lighter tint distinct from both the selected accent bar and
the local-hover `paper_2` background, so "hovered via pinboard" reads
differently from "hovered via mouse right here" and from "selected."

**Selection ring on canvas (new):** `Pinboard::draw()` currently draws a
hover ring from the canvas's own local `PinboardState.hovered` and has no
rendering for `app.brain_view.selected` at all. Change the ring source to
`self.app.brain_view.hovered` (still updated by both the canvas's own mouse
movement and the new sidebar `mouse_area`s) and add a second, visually
distinct ring (thicker, accent-colored, always-on rather than hover-only)
for `self.app.brain_view.selected` — matching the sidebar's existing
selected-row accent bar. `PinboardState.hovered` (the canvas-local field)
stays, but purely as a dedupe guard against re-emitting `BrainHoverEntry`
on every sub-pixel `CursorMoved` while hovering the same node — it stops
being read for rendering.

## 3. Force-directed layout — new `force_layout.rs`

A small, pure module (no `iced` dependency, fully unit-testable in
isolation):

```rust
pub struct ForceLayout {
    pos: HashMap<String, (f32, f32)>,   // normalized [0,1]^2
    vel: HashMap<String, (f32, f32)>,
}

impl ForceLayout {
    // Seeds any id present in `entries` but missing from `pos` via the
    // existing `hash01` scatter (imported from brain_pinboard); prunes any
    // id no longer present. Called once per data change.
    pub fn sync_entries(&mut self, entries: &[BrainEntry]);

    // One simulation step: Hooke's-law springs along `edges`, inverse-
    // square repulsion between all pairs, a mild pull toward (0.5, 0.5),
    // velocity damping, position integration, and hard clamping back into
    // [0,1]^2. `pinned`, if set, is excluded from force integration (its
    // position is being driven externally by a drag) but still exerts
    // repulsion/spring forces on every other node.
    pub fn step(&mut self, entries: &[BrainEntry], edges: &[(usize, usize)], pinned: Option<&str>, dt: f32);

    pub fn position(&self, id: &str) -> Option<(f32, f32)>;
    pub fn set_position(&mut self, id: &str, pos: (f32, f32));
}
```

Constants (rest length, repulsion strength, centering strength, damping
factor) are tuned empirically during implementation; no config surface for
this pass.

`BrainViewState` gains:
```rust
pub layout: force_layout::ForceLayout,
pub dragging: Option<String>,
```

New `Message` variants in `app.rs`:
- `BrainPhysicsTick` — `layout.sync_entries(&entries); layout.step(&entries, &edges, dragging.as_deref(), DT)`.
- `BrainDragStart(String)` — sets `dragging = Some(id)`.
- `BrainDragMove(String, f32, f32)` — `layout.set_position(&id, (x, y))` while `dragging == Some(id)`.
- `BrainDragEnd` — clears `dragging`, releasing the node back to the simulation with whatever velocity it last had (i.e. none injected — it resumes from rest, no throw/fling momentum).

`Pinboard::nodes()` (`brain_pinboard.rs:53`) reads `x, y` from
`self.app.brain_view.layout.position(&e.id)` (falling back to the existing
`hash01` scatter for the one frame between an entry appearing and its next
`sync_entries` — practically unreachable since `sync_entries` runs on every
tick, but keeps the function total rather than panicking on a lookup miss).

**Subscription** (`App::subscription`, `app.rs:3268`): add a fourth branch,
```rust
let physics_sub = if matches!(state.view, View::Brain) && state.brain_view.mode == BrainMode::Pinboard {
    iced::time::every(Duration::from_millis(16)).map(|_| Message::BrainPhysicsTick)
} else {
    Subscription::none()
};
```
folded into the existing `Subscription::batch([...])`. The `view ==
View::Brain && mode == Pinboard` predicate is pulled into a standalone `fn
wants_physics_tick(view: &View, mode: BrainMode) -> bool` so it's unit
tested directly rather than only indirectly through `Subscription` (which
isn't practically assertable in a test).

**Canvas interaction** (`Pinboard::update()`, `brain_pinboard.rs:228`):
`PinboardState` gains `press: Option<(String, Point)>` (candidate node id +
press-origin, in local canvas coordinates) alongside the existing
`hovered`.
- `ButtonPressed`: if `hit_test` finds a node, record `press = Some((id,
  pos))`. Do **not** emit `BrainSelectEntry` yet (deferred to release,
  unlike today's press-triggers-select).
- `CursorMoved` while `press.is_some()` and not yet `dragging`: if the
  cursor has moved past a small tolerance (a handful of pixels) from the
  press origin, transition — set a `dragging: Option<String>` field on
  `PinboardState` to the pressed id, emit `BrainDragStart`. On every
  subsequent `CursorMoved` while `dragging.is_some()`, emit
  `BrainDragMove(id, x / bounds.width, y / bounds.height)`.
- `ButtonReleased`: if `dragging.is_some()`, emit `BrainDragEnd` and clear
  both `dragging` and `press`. Else if `press.is_some()` (a plain click that
  never crossed the drag threshold), emit `BrainSelectEntry(id)`.
- Existing hover handling (`CursorMoved`/`CursorLeft` → `BrainHoverEntry`)
  is unchanged and continues to run alongside the press/drag state machine.

## 4. Catalogue polish

Covered in §1 (category-color dot on `dentry_row`). No broader visual
system change — the sidebar unification itself is the bulk of what makes
catalogue mode feel consistent with pinboard mode rather than "stale."

## Testing

- `force_layout.rs`: two nodes joined by an edge, started far apart,
  converge toward the spring rest length over repeated `step()` calls; two
  coincident, unconnected nodes are pushed apart by repulsion; an isolated
  node doesn't drift outside `[0,1]^2`; `sync_entries` seeds newly-added ids
  deterministically (via the existing `hash01`) and prunes removed ones.
- `app.rs`-style `Message`/`update` tests (matching the existing
  `BrainSelectEntry`/`BrainHoverEntry` test style around `app.rs:4481+`):
  `BrainDragStart`/`BrainDragMove`/`BrainDragEnd` set/clear `dragging` and
  move the right entry's position; `BrainPhysicsTick` advances positions
  without panicking on an empty entry set; sidebar-hover (once wired) sets
  `brain_view.hovered` the same way canvas-hover already does.
- `wants_physics_tick`: unit tested directly against the `(View, BrainMode)`
  matrix.
- Existing `brain_pinboard.rs` tests (`hash01`, `node_radius`, `hit_test`,
  `edge_key`, `resolve_edges`) are unaffected and stay as-is; add a
  click-vs-drag disambiguation test driving `Pinboard::update()` with a
  press, a sub-threshold move, and a release (expect `BrainSelectEntry`),
  and a press/over-threshold-move/release sequence (expect
  `BrainDragStart`/`BrainDragMove`/`BrainDragEnd`, no `BrainSelectEntry`).

## Out of scope (deliberate)

- **No spatial partitioning / Barnes-Hut for repulsion.** Plain O(n²)
  all-pairs repulsion, fine for the entry counts this brain view actually
  sees; revisit only if a real catalogue makes it visibly janky.
- **No throw/fling momentum on drag release.** Released nodes resume from
  rest, not from last-observed drag velocity — simpler, and not something
  the brainstorm asked for.
- **No persisted layout across restarts/reindex.** Explicitly session-only
  per the approved design.
- **No broader catalogue visual redesign** (typography, new iconography,
  animation) beyond the category-color dot — nothing more specific than
  that was requested.
