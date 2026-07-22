# Brain: Pinboard Decluttering, Unified Sidebar, Category Color Fixes

## Context

At real data volume (137+ specimens in the screenshot that prompted this),
the brain panel's pinboard (`crates/ninox-app/src/components/brain_pinboard.rs`)
and its taxonomy rail (`pinboard_body` in
`crates/ninox-app/src/components/brain_panel.rs:447`) show three compounding
problems:

1. **No clustering.** `Pinboard::nodes()` (`brain_pinboard.rs:53`) places every
   node at `hash01(&e.id, salt)` — a deterministic-but-arbitrary scatter with
   no relationship to category or link structure. Every edge is always drawn.
   At scale this reads as a solid mesh, not a graph.
2. **Sidebar doesn't match catalogue.** `pinboard_body` renders a flat list of
   `(category, count)` rows (`brain_panel.rs:451-469`); `catalogue_body`
   (`brain_panel.rs:514`) renders `drawers_rail`, an expandable per-category
   list of actual entries. These are two different components doing
   adjacent jobs, and the pinboard rail is strictly less useful.
3. **Categories fall through to grey.** `category_color`
   (`brain_panel.rs:25`) hard-codes 8 taxonomy categories. But
   `process_file` (`crates/ninox-core/src/brain.rs:581`) derives
   `entry_type` from either an explicit frontmatter `type:` field or the
   parent directory name, with no reconciliation between the two — so a
   file physically filed under `errors/` with `type: error` in its
   frontmatter yields `entry_type: "error"` (singular), which doesn't match
   the taxonomy's `"errors"` and falls to `category_color`'s `_ => s.faint`
   branch. The screenshot shows this concretely: `errors` (12) and `error`
   (13) as two separate sidebar rows, one of them washed-out grey.

## Goal

- The pinboard visually clusters related specimens instead of scattering
  them uniformly, so relationships and neighborhoods are legible at 100+
  nodes.
- The pinboard's sidebar is the same drawer/expand component the catalogue
  view uses — one rail implementation, not two.
- Every entry gets a real, distinct color: known taxonomy aliases collapse
  to one canonical category, and anything still unrecognized gets a stable
  procedural color instead of grey.

## Non-goals

- No pan/zoom camera system for the pinboard canvas — every node stays
  within the canvas bounds today (normalized `[0,1]` coordinates scaled to
  `bounds` at draw time) and continues to; nothing here changes that.
- No manual drag-to-reposition of nodes.
- No rewriting existing `.md` frontmatter files in any brain store —
  normalization happens at read time in `ninox-core`, per the earlier
  decision that a bulk migration across every catalogue's files is more
  risk than it's worth for a cosmetic/display concern.
- No continuous per-frame physics — the simulation runs once per data
  change (reindex, catalogue switch, initial navigation) and caches its
  result, matching the existing "recompute on data change, not per draw"
  architecture note already on `BrainViewState::edges`
  (`app.rs:102-107`).
- No changes to `ninox-server` or the CLI beyond the shared `ninox-core`
  normalization fix (which benefits them incidentally, same as any other
  `BrainIndex` consumer) — no new CLI flags or server routes.

## Approach

### 1. Category alias normalization + procedural fallback color

`process_file` (`ninox-core/src/brain.rs:601-607`) gains a normalization
step immediately after deriving `entry_type` (whether from frontmatter or
parent dir), before it's stored in `FileRecord`:

```rust
fn normalize_entry_type(raw: &str) -> String {
    match raw.trim().to_lowercase().as_str() {
        "repo" => "repos",
        "symbol" => "symbols",
        "concept" => "concepts",
        "pattern" => "patterns",
        "decision" => "decisions",
        "relationship" => "relationships",
        "error" => "errors",
        other => other,
    }.to_string()
}
```

This lives in `ninox-core` (not `ninox-app`) because `process_file` is
shared by the CLI, `ninox-server`, and the GUI app — normalizing at the one
place all three consume fixes the data for everyone, not just the pinboard.
It runs once per file during `rebuild()`, not per query.

This is a pure alias table, not a fuzzy match — an entirely new/misspelled
category (e.g. `"decisons"`, typo) is intentionally left alone and falls
through to the procedural color below rather than being silently guessed
at.

`category_color` (`ninox-app/src/components/brain_panel.rs:25`) changes its
fallback arm from `_ => s.faint` to a deterministic hash-based hue:

```rust
_ => procedural_category_color(ty),
```

`procedural_category_color` reuses the existing `hash01` (currently private
to `brain_pinboard.rs`; becomes `pub(crate)` so `brain_panel.rs` can call
it) to pick a hue in `[0, 360)` from the category string, fixed
saturation/lightness matched to the existing palette's category-color
family (roughly S 55–65%, L 55–65%, tuned by eye against the 8 hand-picked
colors so procedural ones don't look out of place next to them), converted
to `iced::Color` via a small `hsl_to_rgb` helper. Same category string always
yields the same color (pure function of the string); different unrecognized
categories get visibly different hues instead of all collapsing to the same
grey.

Net effect on the screenhot's own data: `errors`/`error` merge into one
`errors: 25` category with the real error color; `decision`/`pattern`/
`relationship`/`repo` singulars similarly merge into their plural
canonical counterparts. Any category that's still genuinely unmapped gets
its own stable hue rather than grey.

### 2. Force-directed pinboard layout, cached per data change

`BrainViewState` (`app.rs:78-108`) gains:

```rust
pub layout: HashMap<String, (f32, f32)>, // entry id -> normalized (x, y) in [0,1]
```

A new pure function, `force_layout(entries: &[BrainEntry], edges: &[(usize,
usize)]) -> HashMap<String, (f32, f32)>`, added to `brain_pinboard.rs`
alongside `resolve_edges`:

- **Seed**: initial position for each entry from the existing
  `hash01(&e.id, 7)` / `hash01(&e.id, 13)` pair — identical to today's
  scatter. This makes the simulation's starting condition deterministic and
  a pure function of entry ids, so re-running on unchanged data reproduces
  the same converged layout bit-for-bit.
- **Forces**, applied for a fixed 400 iterations (no wall-clock or
  convergence-threshold termination — a fixed count keeps this a pure,
  deterministic function of its inputs, and 400 × ~150 nodes is a trivial
  one-time cost on data change, not a per-frame cost):
  - **Repulsion**: inverse-square push between every pair of nodes
    (O(n²), ~11k pairs at 150 nodes — negligible for a one-shot compute).
  - **Spring attraction**: along each edge in `edges`, pulling toward a
    fixed ideal distance.
  - **Temperature-capped displacement**: standard Fruchterman-Reingold —
    each iteration's per-node displacement is capped by a linearly-cooling
    "temperature" (starting at a fixed value, reaching ~0 by the final
    iteration) so the system settles instead of oscillating indefinitely.
    Combined with the hard `[0.05, 0.95]` clamp below, this keeps the graph
    on-canvas without a separate centering force.
- **Output**: final positions clamped into `[0.05, 0.95]` on both axes
  (same margin the current hash-based placement uses) and returned keyed
  by entry id.

`App::refresh_brain_edges` (`app.rs:762`, called from `NavigateBrain`'s
initial load, `BrainReindex`, and `BrainSwitchCatalogue` —
`app.rs:1860,1979,2064`) is extended to also call `force_layout` after
resolving edges and store the result in `state.brain_view.layout`. No new
call sites — this rides the same three existing triggers edges already
use.

`Pinboard::nodes()` (`brain_pinboard.rs:53`) reads `x`/`y` from
`self.app.brain_view.layout.get(&e.id)` instead of computing `hash01`
inline, scaling the normalized coordinate to `bounds` exactly as before. If
an entry is somehow missing from `layout` (defensive case — e.g. a reindex
race), it falls back to the current hash-based position rather than
panicking or hiding the node, consistent with this module's existing
defensive-resolution style (see `resolve_edges`'s doc comment on skipped
link endpoints).

### 3. Unified sidebar + in-place selection highlight

`pinboard_body` (`brain_panel.rs:447`) drops its bespoke flat category-count
`rail` and instead renders `drawers_rail(app)` — the same component
`catalogue_body` already uses — at the same width (272px, up from the
current 215px). This is a straight reuse, not a fork: one rail
implementation for both modes.

`dentry_row` clicks already send `Message::BrainSelectEntry(id)`
(`brain_panel.rs:649`), which already auto-opens the matching drawer
(`open_drawers.insert(e.entry_type.clone())`, `app.rs:1948`) — no change
needed there. The behavior change is in what `BrainSelectEntry` does to
`mode`:

Today (`app.rs:1943-1954`), `BrainSelectEntry` unconditionally sets
`state.brain_view.mode = BrainMode::Catalogue`. Every *current* call site
except the pinboard canvas's own click handler
(`brain_pinboard.rs:241`) already fires while `mode` is Catalogue (drawer
rows and backlink/related chips only render in catalogue mode today), so
that line is presently a no-op everywhere except canvas clicks — where it
silently bounces the user out of the pinboard the moment they click a
node. That's the opposite of what selecting a specimen from the *new*
pinboard-mode drawer should do (per the "highlight in place" decision), and
arguably undesirable for canvas clicks too, since it defeats the point of
being able to inspect a node without losing your place in the graph.

**Change**: remove the unconditional `mode = BrainMode::Catalogue` line
from `BrainSelectEntry` entirely. Selecting an entry updates `selected`,
markdown, and `open_drawers` exactly as today, but never forces a mode
switch — whichever mode you were in when you clicked (canvas node, pinboard
drawer, or catalogue drawer/backlink chip) is the mode you stay in.

The pinboard canvas then needs to visibly reflect `selected`, independent
of `hovered`:

- `Pinboard::draw` (`brain_pinboard.rs:201-223`) gains a persistent ring for
  the node matching `self.app.brain_view.selected`, styled distinctly from
  both existing rings (search-hit: solid `+4px` accent; hover: solid
  `+3px` full-alpha ink) — a dashed `+6px` accent ring, so all three states
  (search hit, hover, selected) remain visually distinguishable when they
  overlap (e.g. hovering the currently-selected node).
- `pinboard_body`'s hover-preview slip (`brain_panel.rs:494-502`) currently
  keys only on `app.brain_view.hovered`. It changes to prefer `hovered`,
  falling back to `selected` when nothing is hovered — so clicking a
  drawer entry shows the same preview slip a hover would, and moving the
  mouse away doesn't hide it if the entry is still the active selection.

## Error handling

- `force_layout` on an empty `entries` slice returns an empty map — no
  special-casing needed, the loops simply don't run.
- A single node with no edges stays at its `hash01`-seeded starting
  position (no repulsion/spring terms apply without another node or an
  edge) — no divide-by-zero.
- `Pinboard::nodes()`'s fallback to hash-based position for a `layout`
  cache miss (described above) is the same defensive pattern
  `resolve_edges` already uses for a link endpoint absent from `entries` —
  skip/degrade gracefully, never panic.
- `procedural_category_color` and `normalize_entry_type` are both total
  functions over `&str` — empty string, unicode, arbitrary garbage all
  produce *some* deterministic output, never a panic (mirrors the existing
  `hash01` tests already covering an empty-string input).

## Testing

`ninox-core/src/brain.rs`:
- `normalize_entry_type` unit tests: each of the 6 known singular→plural
  aliases maps correctly; an unrecognized string passes through unchanged;
  case-insensitivity (`"Error"` → `"errors"`).
- A `process_file`-level test (or adjustment to an existing one) confirming
  a file under `errors/` with `type: error` in frontmatter now yields
  `entry_type == "errors"`, not `"error"`.

`ninox-app/src/components/brain_pinboard.rs`:
- `force_layout` determinism: calling it twice with identical
  `entries`/`edges` yields bit-identical output.
- All returned coordinates fall within `[0.05, 0.95]` on both axes.
- Clustering sanity: a small crafted graph (a tightly-linked triangle of
  three entries, plus one unlinked entry) ends with the triangle's pairwise
  distances smaller than any triangle-to-outlier distance.
- Empty-entries and single-node-no-edges cases don't panic and return
  expected shapes.
- `procedural_category_color`-style hash reuse: confirm two distinct
  unrecognized category strings produce different colors (extending the
  existing `hash01` test module's determinism/range coverage).

`ninox-app/src/components/brain_panel.rs`:
- `category_color` fallback: an unrecognized type returns a non-grey,
  deterministic color; a known taxonomy type is unaffected (still returns
  its hard-coded color, procedural path not hit).
- Existing `categories()` test (`brain_panel.rs:925`) continues to pass
  unmodified — normalization now happens upstream in `ninox-core`, so by
  the time `categories()` runs there are no more singular/plural
  duplicates to bucket separately.

`ninox-app/src/app.rs`:
- `BrainSelectEntry` no longer changes `mode`: a test selecting an entry
  while `mode == Pinboard` asserts `mode` stays `Pinboard` afterward
  (extending the existing selection tests around `app.rs:3673-3920`, which
  today implicitly rely on already being in Catalogue mode).
- Existing hover tests (`app.rs:3716-3737`) are unaffected — `hovered`
  handling doesn't change.

## Wiring

- New `BrainViewState` field: `layout: HashMap<String, (f32, f32)>`,
  default empty (`app.rs:78-108`).
- `hash01` in `brain_pinboard.rs` changes from private to `pub(crate)` so
  `brain_panel.rs`'s `procedural_category_color` can reuse it.
- No new `Message` variants — this reuses `BrainSelectEntry`,
  `BrainHoverEntry`, `BrainReindex`, `BrainSwitchCatalogue`, and
  `NavigateBrain` exactly as they exist today.
- No new dependencies — the force simulation is plain arithmetic over
  existing `f32` positions, no physics crate needed at this node count.
