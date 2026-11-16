# MLOPS-5339 follow-up: Brain "All sources" merged index

## Decision (sent as the pre-build read-back, approved)

`MergedBrainIndex` (ninox-core/src/brain.rs): one `BrainIndex` per enabled
`CatalogueRef`, unchanged per-catalogue API, fanned out and merged. No SQLite
schema change — `BrainEntry.source` is stamped in memory by `MergedBrainIndex`
from the `CatalogueRef` that owns each child index (approved simplification:
avoids a migration and a window where existing rows read `source == ""`).
Cross-catalogue id collisions are avoided by qualifying ids as `"<source>:<id>"`
only on entries returned through the merged index — single-catalogue
`BrainIndex` usage (CLI, TUI, server, GUI single-source mode) is completely
unchanged.

## Core (ninox-core/src/brain.rs)

- `BrainEntry` gains `pub source: String` (empty for direct single-catalogue
  use; stamped by `MergedBrainIndex` otherwise). Every existing struct-literal
  construction site (tests in brain.rs, brain_list.rs, brain_graph.rs,
  tui/brain.rs, tui/view.rs) gets `source: String::new()`.
- `link_target_matches(target, entry)` — pure-Rust port of the wikilink
  resolution rule already duplicated as SQL in `backlinks`/`outlinks`/
  `links_all` (`target == entry.name || target == entry.id ||
  stem(target) == stem(entry.id) || stem(target) == entry.name`), reusable
  for merge-time cross-source resolution.
- `BrainIndex::raw_links_all() -> Vec<(String, String)>` — new accessor,
  `SELECT from_id, target FROM links` with no join, needed so
  `MergedBrainIndex` can tell which raw link targets did *not* resolve
  within their own catalogue (same-source-first requires seeing the raw,
  unresolved target list, which the existing resolved `outlinks`/`backlinks`
  don't expose).
- `MergedSource { name: String, index: Arc<BrainIndex> }`,
  `MergedBrainIndex { sources: Vec<MergedSource> }`:
  - `open(&[CatalogueRef]) -> Result<Self>` — opens one `BrainIndex` per
    `enabled` catalogue; a single catalogue failing to open is logged
    (`tracing::warn!`) and skipped rather than failing the whole merge, so
    one broken source doesn't blank the GUI's All-sources view.
  - `query`/`get`/`backlinks`/`outlinks`/`related`/`links_all`/`rebuild_all`
    fan out to the per-source `BrainIndex`, qualify ids as `"source:id"`,
    stamp `source`, and merge:
    - `query`: no-text-filter-only path concatenates + re-sorts by name
      (matches each child's own "no constraints" ordering); text-search path
      runs each child's own hybrid query independently (preserving its
      internal keyword/semantic ranking) then merges the per-child ranked id
      lists with the existing `reciprocal_rank_fusion` helper, so a result
      near the top of a small catalogue doesn't automatically out-rank a
      true top hit in a larger one.
    - `backlinks`/`outlinks`/`links_all`: same-source edges come straight
      from each child's own (already-correct) resolved method. Cross-source
      edges are found by taking each child's unresolved raw targets
      (`raw_links_all()` minus whatever its own `outlinks` already
      accounts for) and testing them with `link_target_matches` against
      every *other* source's entries — implementing "same source first,
      then across sources" directly from the two existing primitives.
    - `related`: same tiering as `BrainIndex::related` (direct links, then
      co-citation, then shared tags) but built on the merged
      `backlinks`/`outlinks` above, so tiers 1-2 are already cross-source
      aware; tier 3 (tag overlap) scans the merged entry set.
  - `qualify(source, id)` / `split_qualified(id)` helpers, split on the
    first `:` — catalogue names are short config-chosen identifiers and are
    not expected to contain `:`.

## GUI (ninox-gui)

- `App.brain: Arc<BrainIndex>` becomes `App.brain: BrainScope`, an enum of
  `Single(Arc<BrainIndex>)` / `All(Arc<MergedBrainIndex>)` with delegating
  methods (`query`, `get`, `backlinks`, `outlinks`, `related`, `links_all`,
  `rebuild`, and an `identity()` used in place of the old `path()` race-guard
  comparison in `BrainFreshened`) — this keeps the ~25 existing
  `state.brain.*` call sites in `app.rs`/`brain_panel.rs` unchanged other
  than the field's type name.
- `Message::BrainSwitchCatalogue(usize)` becomes `BrainSwitchCatalogue(Option<usize>)`
  (`None` = All sources). `catalogue_select` (`brain_list.rs`) prepends an
  "All sources" option, selected by default whenever more than one catalogue
  is `enabled`.
- List rows and the reading pane show a small source chip when
  `app.brain` is `BrainScope::All` (hidden in single-source mode, where it's
  redundant with the toolbar's catalogue select).
- `resolve_link` (`brain_panel.rs`) takes the clicked-from entry's `source`
  and searches same-source entries before falling back to the full merged
  list, so wikilink clicks respect "same source first" at the UI layer too.

## Out of scope (per ticket)

`brain_sync` semantics, the TUI Brain tab/picker (MLOPS-5350 TUI-parity
worker is holding a small follow-up PR for the TUI catalogue picker until
this lands), the Sources view (shipped in #278), the ninox prompt.

## Tests

- `brain.rs`: source tagging on merge, id qualification round-trip,
  disabled-source exclusion, merged query ordering (RRF across catalogues),
  cross-source link resolution precedence (same-source wins over a
  same-named entry in another source), `related` tiers across sources.
- `app.rs`: `BrainSwitchCatalogue(None)` loads a merged view with source
  chips, `BrainFreshened` race-guard still ignores a stale switched-away
  scope.
