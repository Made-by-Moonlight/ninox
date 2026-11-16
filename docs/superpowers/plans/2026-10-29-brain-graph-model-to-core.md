# Brain Graph Model: Move to Core Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the (upcoming) TUI Brain graph view the same neighbourhood-capping and repo-clustering logic the GUI's Graph view already uses, instead of a second copy.

**Architecture:** Move exactly the two functions (plus their minimal dependencies/return types) the brief names for TUI reuse — `neighbourhood_capped` (+ its `neighbourhood` BFS helper) and `cluster_by_repo` (+ its `ClusterBubble` return type) — from `ninox-gui/src/components/brain_graph.rs` into `ninox_core::brain`, alongside the already-shared list/filter functions. `ninox-gui` re-exports them (`pub use ninox_core::brain::{...}`) so its own call sites (`App::refresh_brain_graph`, canvas rendering) are unchanged.

**Tech Stack:** Rust, no new dependencies.

**Spec:** `BRIEF-MLOPS-5350-tui-parity.md` (Worker B, item 3: *"Reuse the GUI's graph model functions, but move the pure ones (neighbourhood capping, cluster counts) into core first"*).

**Scope note:** the GUI's force-directed `layout`/`SpatialIndex`/`hit_test`/`walk_direction`/`fit_transform` and the `Camera`/canvas rendering stay in `ninox-gui` — they're specific to a pixel-space force simulation the TUI's rataflow-based rendering (item 6) won't use at all, the same way the Fleet tab's `island_flow.rs` computes its own depth-banded positions rather than reusing a GUI layout algorithm. Moving them would be dead weight in `ninox-core` with no second caller, which is exactly the YAGNI the brief's "move the pure ones" (not "move everything pure") is drawing the line around.

## Global Constraints

- Branch `v1-mlops-5350-tui-parity-brain-5` off `origin/v1`; PR targets `v1`, title `MLOPS-5350: refactor(brain): move neighbourhood/cluster graph model to core`.
- `cargo fmt` not run wholesale.
- `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` must pass before pushing.
- No GUI behavior change — this PR is a pure move, verified by the GUI's own test suite staying green with the same assertions (not just "compiles").

## Review Focus

- **The GUI's Graph view must render identically after the move** — no test currently screenshots it, so the safeguard is the full existing `cluster_by_repo`/`neighbourhood_capped` test suite running unchanged against the moved code (ported verbatim, not rewritten) plus `cargo test -p ninox-gui` staying at the same pass count minus exactly the 12 moved tests.
- **A caller importing the old GUI-local path** (`crate::components::brain_graph::cluster_by_repo` et al.) **must keep compiling** — covered by the `pub use` re-export rather than requiring every call site to switch to `ninox_core::brain::...` directly.
- **Dead code left behind by the move** (an unused `neighbourhood` re-export, stray now-unused imports) — covered by `cargo clippy -D warnings`, which already caught one on the first pass (an unused `neighbourhood` re-export nothing in the GUI calls directly) and was fixed before committing.

---

## Task 1: Move `neighbourhood`/`neighbourhood_capped`/`ClusterBubble`/`cluster_by_repo` to `ninox-core`

**Files:**
- Modify: `crates/ninox-core/src/brain.rs` (new "Graph model" section + `graph_tests` module, inserted after the existing `filter_tests` module)
- Modify: `crates/ninox-gui/src/components/brain_graph.rs` (delete the four moved items and their 12 tests; add the re-export)

- [x] Step 1: Copy the four items and their exact doc comments into `ninox-core/src/brain.rs`, plus all 12 of their existing tests verbatim (5 `neighbourhood_*`, 2 `neighbourhood_capped_*`, 4 `cluster_by_repo_*`, 1 `cluster_bubbles_never_overlap`) into a new `#[cfg(test)] mod graph_tests`.
- [x] Step 2: `cargo test -p ninox-core brain::graph_tests::` — all 12 pass in the new location.
- [x] Step 3: Delete the four items from `brain_graph.rs`; add `pub use ninox_core::brain::{cluster_by_repo, neighbourhood_capped, ClusterBubble};` (not `neighbourhood` — nothing in the GUI calls it directly, only through `neighbourhood_capped`); delete the 12 now-duplicated tests from `brain_graph.rs`'s own test module, keeping the GUI-only `cluster_label_text_*` tests (that function stays, since it does pixel-bubble-fit text clipping, not graph modeling).
- [x] Step 4: `cargo build -p ninox-gui` clean; `cargo test -p ninox-gui` — same failures as before (none), count down by exactly 12.
- [x] Step 5: `cargo clippy --workspace --all-targets -- -D warnings` clean.
- [x] Step 6: Commit.

## Task 2: Verification, PR

- [ ] `cargo test --workspace`.
- [ ] Rebase on fresh `origin/v1`, push, open PR, report to the orchestrator.
- No kitty screenshot: this PR changes no user-visible behavior in either app (pure internal move, zero UI touched) — the TUI side that will actually display something lands in item 6.
