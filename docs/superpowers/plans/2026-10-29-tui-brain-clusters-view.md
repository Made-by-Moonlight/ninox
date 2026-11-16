# TUI Brain Clusters View Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The Clusters scope of the Brain graph view: a ranked table of repos with counts, coloured/labelled by dominant kind.

**Architecture:** `BrainView` gains a `GraphScope` (Neighbourhood/Clusters, `s` cycles) plus a `clusters_cursor` row index. Clusters has no rataflow canvas at all — `ninox_core::brain::cluster_by_repo` (moved to core in an earlier PR) already returns a ranked `Vec<ClusterBubble>`, rendered as a plain table. `Enter` on a row sets `repo_filter` (the same field the List view's own `r` key already cycles) and leaves graph mode, landing in the List view already filtered to that repo — reusing existing filtering machinery rather than inventing "a graph scoped to one repo."

**Tech Stack:** Rust, ratatui. No new dependency (no rataflow needed for this scope).

**Spec:** `BRIEF-MLOPS-5350-tui-parity.md` (Worker B, item 3: *"Clusters: a ranked table of repos/kinds with counts, coloured by dominant kind. Enter filters the graph to that cluster."* / *"Everything: not in the TUI... Say 'Everything is GUI-only; use Clusters' in the scope bar."*).

**Scope decision**: "Enter filters the graph to that cluster" is read as "filters the Brain view to that repo" — switching to the already-built List view with `repo_filter` set, rather than a second graph mode scoped to one repo (which the brief doesn't otherwise describe and `cluster_by_repo`'s own data model has no node/edge information for anyway — it returns bubbles, not a subgraph). The TUI has no literal "scope bar" widget to grey out `Everything` on, so its disclaimer is folded into the graph breadcrumb instead, always visible regardless of which scope is active.

## Global Constraints

- Branch `v1-mlops-5350-tui-parity-brain-7`, stacked on the not-yet-merged `v1-mlops-5350-tui-parity-brain-6` (item 6, the Neighbourhood graph this scope toggle lives inside) — rebase onto `origin/v1` once that PR merges.
- `cargo fmt` not run wholesale.
- `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` must pass before pushing.
- The help modal has no scroll (see the item 6 plan's own note on this) — the new `tab` entry's description was edited in place rather than adding a new line, to stay within the unscrolled height budget confirmed by `help_matches_the_six_tab_set_not_the_old_seven`.

## Review Focus

- **Selecting the "(no repo)" bucket and pressing Enter**: sets `repo_filter` to the literal string `"(no repo)"`, which matches zero real entries (`visible_entries`'s repo filter has no "entries with an empty repos list" concept) — a known, documented limitation rather than a crash; noted in the PR rather than engineered around, since `cluster_by_repo`'s `NO_REPO` sentinel is private to `ninox-core` and duplicating it as a shared constant for this one edge case isn't worth the coupling.
- **An empty brain (no entries at all)**: `clusters()` returns an empty `Vec`, and the render path shows the same "Nothing to show" empty state Neighbourhood uses for no-focus, not a panic — covered by `draw_brain_clusters`'s `is_empty()` guard (not separately unit-tested beyond that guard being read directly, since `brain_clusters_table_renders_ranked_repos_with_the_selected_row_marked` already exercises the non-empty path and the guard mirrors Neighbourhood's own tested empty-state pattern).
- **The cursor drifting past the end** (e.g. entries change between frames, shrinking the cluster count) — `selected_cluster_repo` returns `None` rather than panicking on an out-of-bounds index — covered by `selected_cluster_repo_returns_the_label_at_the_cursor`'s explicit out-of-range case.
- **Moving the cursor below zero or past the last row** — covered by `move_clusters_cursor_clamps_within_bounds`.

---

## Task 1: `GraphScope` and Clusters state on `BrainView`

**Files:** `crates/ninox-tui/src/tui/brain.rs`

- [x] Write failing tests (`cycle_graph_scope_toggles_neighbourhood_and_clusters`, `clusters_ranks_by_repo_count`, `move_clusters_cursor_clamps_within_bounds`, `selected_cluster_repo_returns_the_label_at_the_cursor`).
- [x] Confirm RED.
- [x] Add `GraphScope` enum (`Neighbourhood`/`Clusters`, `next()`/`label()`), `graph_scope`/`clusters_cursor` fields, `cycle_graph_scope`/`clusters`/`move_clusters_cursor`/`selected_cluster_repo` methods.
- [x] Confirm GREEN (one test's fixture assumption was wrong on the first pass — forgot two `sample()` entries default into the "(no repo)" bucket, outranking a single-entry "ninox" cluster — fixed the test, not the implementation).

## Task 2: Key routing

**Files:** `crates/ninox-tui/src/tui/state.rs`

- [x] Write failing tests (`s_cycles_graph_scope_to_clusters_and_back`, `clusters_scope_moves_the_cursor_and_enter_filters_the_list_by_repo`).
- [x] Confirm RED.
- [x] Split `brain_graph_key` into a scope dispatcher (`Esc`/`s` handled first, since they apply in either scope) plus `brain_neighbourhood_key` (the existing arrows/Enter/`o` logic, unchanged) and `brain_clusters_key` (`j`/`k`/arrows move the row cursor, `Enter` sets `repo_filter` and leaves graph mode).
- [x] Confirm GREEN. Edited the existing `tab` help-modal line in place (see Global Constraints) rather than adding a new one.

## Task 3: Rendering

**Files:** `crates/ninox-tui/src/tui/view.rs`

- [x] Write failing test (`brain_clusters_table_renders_ranked_repos_with_the_selected_row_marked`).
- [x] Confirm RED (temporarily dropped the `▸`/selection marker, watched the assertion fail, restored it).
- [x] `draw_brain_graph` now branches on `graph_scope` for both the breadcrumb (always shows the "Everything is GUI-only" hint) and the body; `draw_brain_neighbourhood` is the pre-existing logic unchanged, `draw_brain_clusters` renders the ranked table with the selected row marked.
- [x] Confirm GREEN; full `cargo test -p ninox-tui tui::` sweep clean (401 passed).
- [x] `cargo clippy --workspace --all-targets -- -D warnings` clean.

## Task 4: Verification, PR

- [ ] `cargo test --workspace`.
- [ ] Kitty screenshot check per `BRIEF-common.md` §7: cycle into Clusters with `s`, confirm the ranked table and breadcrumb hint render, move the cursor, confirm `Enter` lands in the List view filtered to the selected repo.
- [ ] Rebase onto `origin/v1` once item 6's PR (#289) merges (this branch stacks on it meanwhile); push; open PR; report to the orchestrator. This closes out all 7 items of the approved PR sequence (item 4, the catalogue picker, remains held pending the All-sources index per the earlier coordination with the orchestrator and `v1-mlops-5339-brain-all-sources`).
