# TUI Brain Catalogue Picker Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The Brain tab stops being pinned to `AppConfig::resolved_brain_path()` — a `c` key cycles the active catalogue through `AppConfig::catalogue_options()` plus "All sources" (via the now-merged `MergedBrainIndex`, MLOPS-5339/#287), the same set the GUI's catalogue picker offers.

**Architecture:** The GUI's `BrainScope`/`BrainScopeId` enums (`crates/ninox-gui/src/app.rs:76-216`) already do exactly the delegation this needs — wrapping either a single `Arc<BrainIndex>` or an `Arc<MergedBrainIndex>` behind one API (`query`/`backlinks`/`outlinks`/`related`/`links_all`/`rebuild`/`file_path`/`includes`/`identity`) — and have zero GUI-specific dependencies (only `ninox_core` types). Per this ticket's "must not copy GUI-only logic into the TUI; move it to core and share it" rule, they move into `ninox_core::brain` first (mirroring the `neighbourhood_capped`/`cluster_by_repo` move from an earlier PR in this sequence), and the GUI re-exports them exactly as it already does for `BrainSort`. The TUI's `BrainView` then gets its own `catalogue_options: Vec<CatalogueRef>` snapshot and `active_catalogue: Option<usize>` (`None` = All sources), and `load_brain`/`load_brain_links` build a `BrainScope` from that instead of unconditionally resolving the default path.

**Tech Stack:** Rust, ratatui. No new dependency.

**Spec:** `BRIEF-MLOPS-5350-tui-parity.md` (Worker B, item 2: *"Catalogue picker: `c` cycles or opens a picker over `AppConfig::catalogue_options()`, plus 'All sources' once the GUI Sources PR lands its merged index. The TUI must stop being pinned to `resolved_brain_path()`."*).

**Scope decision (picker vs. cycle)**: every other Brain-tab selector added in this PR sequence (kind/repo/tag filter, sort, graph scope) is a single-key cycle, not a modal — the TUI has no picker/dropdown widget anywhere, and building one just for this would be new UI vocabulary for one feature. `c` cycles through "All sources" then each `catalogue_options()` entry in order (matching the GUI's own pick-list order: All sources first), wrapping around — consistent with the brief's "cycles **or** opens a picker" either/or wording.

**Scope decision (writes under "All sources")**: `run_job` (`crates/ninox-tui/src/tui/brain.rs:747`) always targets one concrete catalogue root — a `MergedBrainIndex` has no single writable root. For an existing entry (edit-in-place reindex, delete), the target catalogue is resolved from the entry's own qualified id (`BrainEntry.source` / `MergedBrainIndex::split_qualified`) via a new `BrainScope::catalogue_root_for(id)`, so editing/deleting an "All sources" entry always writes back to the catalogue it actually came from. A brand-new entry (`a`) has no source yet to resolve from; while "All sources" is active it's written to `catalogue_options()[0]` (the implicit "default" catalogue) — the spec is silent here, and "default" is the least surprising target. `R` (reindex) under "All sources" rebuilds every enabled catalogue (`BrainScope::rebuild` already dispatches to `MergedBrainIndex::rebuild_all`).

## Global Constraints

- Branch `v1-mlops-5350-tui-parity-brain-4`, off fresh `origin/v1` (no code dependency on item 7/PR #290).
- `cargo fmt` not run wholesale.
- `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` must pass before pushing.
- The help modal has no scroll — new key documentation goes into an existing line's description rather than a new line/section, verified against `help_matches_the_six_tab_set_not_the_old_seven`.
- Never show secrets (brief's Sources-view rule) — N/A here, the picker only ever shows catalogue `name`s, never `remote`/`endpoint` connection strings.

## Review Focus

- **Only one catalogue configured** (the common case today): `catalogue_options()` returns a single "default" entry, so cycling still has two states (All sources, default) even though they query the identical data — not a bug, since "All sources" of one catalogue is a trivial merge, but worth a test so it isn't mistaken for a no-op bug later.
- **Cycling away from the Brain tab and back**: the active catalogue must persist on `BrainView` across tab switches (it's not reset on `load_brain`), the same way `kind_filter`/`repo_filter` already survive — covered by a test that cycles, switches `View`, switches back, and asserts the catalogue is unchanged.
- **Deleting/reindexing an entry while "All sources" is active**: must resolve to the entry's *own* source catalogue, not silently fall back to "default" and corrupt or no-op against the wrong directory — covered by `catalogue_root_for_resolves_an_all_sources_entry_to_its_own_source`.
- **A catalogue that fails to open** (bad path, corrupt index): `load_brain` must surface `st.brain.error` exactly as it does today for the single-catalogue case, not panic — covered by reusing the existing `load_brain` error path, now routed through `BrainScope`'s `Result`-returning methods.
- **The footer/help budget**: adding `c`'s key doc must not push the help modal's "Settings (not a tab" line off-screen at 120×60 — covered by re-running `help_matches_the_six_tab_set_not_the_old_seven` after the edit.

---

## Task 1: Move `BrainScope`/`BrainScopeId` to `ninox_core::brain`

**Files:** `crates/ninox-core/src/brain.rs`, `crates/ninox-gui/src/app.rs`, `crates/ninox-gui/src/components/brain_list.rs`, `crates/ninox-gui/src/components/global_search.rs`

- [ ] Write failing tests in `ninox-core/src/brain.rs` porting the existing `BrainScope`/`BrainScopeId` behavior (construct a `Single` and an `All` scope over small fixture `BrainIndex`/`MergedBrainIndex`s; assert `identity()`, `is_all()`, `includes()`, `file_path()` for both variants) — these are new tests in core, since the enum has none of its own today (only exercised indirectly via GUI's `app.rs` tests).
- [ ] Confirm RED (types don't exist in core yet).
- [ ] Cut `BrainScope`/`BrainScopeId` and their `impl` block verbatim from `ninox-gui/src/app.rs:76-216` into `ninox-core/src/brain.rs`; add `pub use brain::{BrainScope, BrainScopeId};` to `ninox-core/src/lib.rs` alongside the existing `BrainEntry, BrainIndex, MergedBrainIndex, MergedSource, QueryFilters` re-export.
- [ ] In `ninox-gui/src/app.rs`, replace the removed definitions with `pub use ninox_core::brain::{BrainScope, BrainScopeId};` (same pattern as the existing `pub use ninox_core::brain::BrainSort;` one line above it); fix any now-redundant imports (`Arc<BrainIndex>` etc. are still used elsewhere in the file, so those imports stay).
- [ ] Confirm GREEN; run `cargo test -p ninox-core -p ninox-gui` to confirm the GUI's own existing `BrainScope`-dependent tests (`active_catalogue`, `brain_freshened_for_the_active_catalogue_swaps_the_index_and_reloads`, etc.) are unaffected by the move.
- [ ] Commit: `refactor(brain): move BrainScope/BrainScopeId to core`.

## Task 2: Catalogue state on `BrainView`

**Files:** `crates/ninox-tui/src/tui/brain.rs`

**Interfaces:**
- Consumes: `ninox_core::config::{AppConfig, CatalogueRef}` (already a `ninox-core` dependency via `ninox_ui_shared`/existing TUI config loads); `ninox_core::brain::{BrainScope, BrainScopeId}` (Task 1).
- Produces: `BrainView::catalogue_options: Vec<CatalogueRef>`, `BrainView::active_catalogue: Option<usize>`, `BrainView::cycle_catalogue()`, `BrainView::set_catalogues(options: Vec<CatalogueRef>)`, `BrainView::active_catalogue_label(&self) -> &str` ("All sources" or the `CatalogueRef.name`) — all consumed by Task 3 (key routing) and Task 5 (rendering).

- [ ] Write failing tests: `cycle_catalogue_wraps_through_all_sources_and_each_configured_catalogue`, `cycle_catalogue_is_a_two_state_toggle_with_only_the_default_catalogue`, `active_catalogue_label_names_all_sources_or_the_catalogues_own_name`.
- [ ] Confirm RED.
- [ ] Add the fields/methods above. `cycle_catalogue`: `None -> Some(0) -> Some(1) -> ... -> Some(len-1) -> None`; a no-op (stays `None`) when `catalogue_options` is empty (shouldn't happen — `catalogue_options()` always includes "default" — but avoids a panic on an unexpected empty config).
- [ ] Confirm GREEN.
- [ ] Commit: `feat(tui): Brain catalogue state`.

## Task 3: `c` key routing

**Files:** `crates/ninox-tui/src/tui/state.rs`

**Interfaces:**
- Consumes: `BrainView::cycle_catalogue()` (Task 2).
- Produces: `Action::CycleBrainCatalogue` (consumed by Task 4's `mod.rs` dispatch).

- [ ] Write failing test: `c_cycles_the_brain_catalogue_and_reloads`.
- [ ] Confirm RED.
- [ ] Add `Action::CycleBrainCatalogue` to the `Action` enum; in `brain_key` (not in graph mode, not editing the query — same guard tier as `R`/`t`/`s`), `KeyCode::Char('c') => { st.brain.cycle_catalogue(); return Action::CycleBrainCatalogue; }`. Add `c` to the `HELP` modal's existing Brain-tab line (consolidated, not a new line — see Global Constraints) and to the footer's `fit_hints` candidate list (lowest priority, since the filter/sort keys it already shows are more commonly used).
- [ ] Confirm GREEN; re-run `help_matches_the_six_tab_set_not_the_old_seven`.
- [ ] Commit: `feat(tui): c cycles the Brain catalogue`.

## Task 4: Load the active catalogue, not just the default

**Files:** `crates/ninox-tui/src/tui/mod.rs`

**Interfaces:**
- Consumes: `BrainScope` (Task 1), `BrainView::{catalogue_options, active_catalogue}` (Task 2), `Action::CycleBrainCatalogue` (Task 3).
- Produces: `fn open_brain_scope(options: &[CatalogueRef], active: Option<usize>) -> anyhow::Result<BrainScope>` (also used by Task 5's write-path resolution).

- [ ] Write failing integration-style test (in `mod.rs`'s own test module, or a new one alongside — check whichever existing pattern `load_brain`'s tests already use) asserting that after `set_catalogues` + `cycle_catalogue` to a second fixture catalogue and calling `load_brain`, `st.brain.entries` reflects that catalogue's own entries, not the default's.
- [ ] Confirm RED.
- [ ] Add `open_brain_scope`: `Some(idx)` → `BrainScope::Single(Arc::new(BrainIndex::open(&options[idx].path)?))`; `None` → `BrainScope::All(Arc::new(MergedBrainIndex::open(options)?))`. Populate `st.brain.catalogue_options`/`active_catalogue` once, at first `load_brain` (via `AppConfig::load_or_default().catalogue_options()`), preserving whatever `active_catalogue` already is on subsequent reloads so cycling survives a tab switch (Review Focus). Rewrite `load_brain`/`load_brain_links` to build a `BrainScope` via `open_brain_scope` and call its `query`/`links_all`/`backlinks`/`related`/`outlinks` methods instead of opening a `BrainIndex` directly. Add the `Action::CycleBrainCatalogue` arm to the dispatcher (`=> load_brain(st).await`, reusing the existing handler since it already re-derives the scope from current state).
- [ ] Confirm GREEN.
- [ ] Commit: `feat(tui): Brain list follows the active catalogue`.

## Task 5: Write-path resolution for the active catalogue

**Files:** `crates/ninox-core/src/brain.rs` (new `BrainScope` method), `crates/ninox-tui/src/tui/mod.rs` (`start_brain_job`)

**Interfaces:**
- Consumes: `BrainScope` (Task 1), `open_brain_scope` (Task 4).
- Produces: `BrainScope::catalogue_root_for(&self, id: &str, default: &Path) -> PathBuf`.

- [ ] Write failing tests in `ninox-core/src/brain.rs`: `catalogue_root_for_a_single_scope_is_always_its_own_root`, `catalogue_root_for_resolves_an_all_sources_entry_to_its_own_source`, `catalogue_root_for_falls_back_to_default_for_an_unqualified_new_entry_id`.
- [ ] Confirm RED.
- [ ] Implement `catalogue_root_for`: `Single(b)` → `b.path()`; `All(m)` → `MergedBrainIndex::split_qualified(id)` then look up that source's root in `m.sources()`, falling back to `default` if the id isn't qualified or the source isn't found. In `start_brain_job`, resolve the job's path via `open_brain_scope(...).catalogue_root_for(job.id(), &options[0].path)` instead of `config.resolved_brain_path()` (Review Focus: existing-entry jobs resolve correctly; a brand-new `Job::Write` id has no qualifier yet, so it takes the documented default-catalogue fallback).
- [ ] Confirm GREEN.
- [ ] Commit: `feat(tui): resolve brain writes to their own catalogue`.

## Task 6: Rendering — show the active catalogue

**Files:** `crates/ninox-tui/src/tui/view.rs`

- [ ] Write failing test: `brain_list_shows_the_active_catalogue_name_when_more_than_one_is_configured` (and a companion asserting it's hidden with only the implicit default — Review Focus).
- [ ] Confirm RED.
- [ ] In `draw_brain`'s info line (`view.rs:~1915`, where `kind:`/`repo:`/`tag:`/`sort:` are appended), append `" · {name}"` using `BrainView::active_catalogue_label()` whenever `b.catalogue_options.len() > 1`, styled like the other filter badges (`p.accent`).
- [ ] Confirm GREEN.
- [ ] Commit: `feat(tui): show the active Brain catalogue`.

## Task 7: Verification, PR

- [x] `cargo test --workspace` (the one failure, `prompt_agent::tests::concurrent_first_asks_do_not_race_into_a_double_spawn`, is a known pre-existing flake unrelated to this branch — passes in isolation, and a clean re-run of the full suite showed it green).
- [x] `cargo clippy --workspace --all-targets -- -D warnings`.
- [x] Kitty screenshot check with a scratch config with 2 `[[brain.catalogues]]` entries: `c` cycles All sources (2 entries) → default (1) → team (1) → back to All sources; `R` under All sources fans out and reindexes both catalogues; deleting an "All sources" entry removes it from its own catalogue's directory, not the default's — this caught a real bug (`run_job`'s `source_file` joining a still-qualified id onto the resolved root), fixed and regression-tested (see the `fix(tui)` commit).
- [ ] Rebase onto `origin/v1` if anything new merged; push; open PR titled `MLOPS-5350: feat(tui): Brain catalogue picker`; report to the orchestrator.
