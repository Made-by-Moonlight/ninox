# TUI Brain Neighbourhood Graph Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A terminal-native Neighbourhood graph view for the Brain tab (`Tab` toggles into it from the List view): the focus entry, its 1-hop links, and the capped hop-2 set, navigable with arrow keys.

**Architecture:** A new `tui::brain_flow` module (mirroring `island_flow.rs`'s rataflow plumbing) builds a `Flow<BrainGraphNode, StepEdge>` with three columns — focus, hop-1, hop-2(+"+N more") — from `ninox_core::brain::neighbourhood_capped` (moved to core in the prior PR). `BrainView` holds the built `Flow` in a `RefCell` (rendering only ever gets `&TuiState`), rebuilt every render the same way `FleetState::opened_flow` is; arrow keys call rataflow's own `FlowAction::SelectUp/Down/Left/Right` via `Flow::apply`, reusing `island_flow::carry_over_viewport_and_selection` to keep the user's position across rebuilds when the focus hasn't changed.

**Tech Stack:** Rust, ratatui, rataflow (already a workspace dependency via the opened-island card tree).

**Spec:** `BRIEF-MLOPS-5350-tui-parity.md` (Worker B, item 3: *"Neighbourhood (default): the focus entry centred as a box, with 1-hop entries in a ring or columns of boxes linked by box-drawing edges via rataflow... Reuse the GUI's neighbourhood cap... Arrows walk links, Enter refocuses, o opens in the list"*).

**Scope decisions** (brief doesn't specify these, so the most rataflow-consistent option was picked and is noted here per the ticket's "pick the most spec-consistent option and note it" rule):
- **Columns, not a ring**: a literal ring has no natural terminal-grid analogue; three columns (focus/hop-1/hop-2) is what `island_flow.rs` already establishes as this codebase's terminal-native graph idiom, and rataflow's `StepEdge` routing (built for exactly this kind of banded layout) makes it the path of least surprise.
- **Mouse is out of scope**: the brief's interaction list for this view is keyboard-only (arrows, Enter, `o`); `island_flow.rs`'s mouse click/drag handling isn't ported. A follow-up can add it if wanted.
- **No animation**: the opened island's marching-ants ticking is a `Working`-status cue with no Brain-tab equivalent; skipped entirely.
- **Hops fixed at 2** (no 1–3 Hops control like the GUI's): the brief's own description of the cap ("all hop-1 entries, plus at most 15 hop-2 entries") only makes sense at hops=2; a variable control isn't mentioned for the TUI.

## Global Constraints

- Branch `v1-mlops-5350-tui-parity-brain-6`, stacked on the not-yet-merged `v1-mlops-5350-tui-parity-brain-5` (item 5, moving `neighbourhood_capped`/`cluster_by_repo` to core) — a real dependency, not a sequencing convenience; rebase onto `origin/v1` once that PR merges. PR targets `v1`, title `MLOPS-5350: feat(tui): Brain Neighbourhood graph view`.
- `cargo fmt` not run wholesale.
- `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` must pass before pushing.
- `Tab` was confirmed unbound anywhere in `brain_key`'s dispatch (and `tab_key()` in `state.rs` only intercepts digit keys despite its name, never the literal `Tab`/`BackTab` key codes) before choosing it for the List/Graph toggle.
- The help modal has no scroll — adding lines to `HELP` risks pushing later sections off-screen on common terminal sizes (caught live: a first draft with a dedicated "Brain graph" section broke `help_matches_the_six_tab_set_not_the_old_seven`'s "Settings (not a tab" visibility assertion at 120×60; fixed by folding the graph keys into one line of the existing Brain section instead of a new section).

## Review Focus

- **A focus entry no longer in `entries`** (deleted, or filtered out some other way) — `ensure_graph_flow` must clear the flow, not panic on a stale index — covered by `ensure_graph_flow_clears_when_there_is_no_focus` and the out-of-range-focus case in `brain_flow`'s own `build_neighbourhood_flow_none_for_an_out_of_range_focus`.
- **Enter on the "+N more" sentinel node** must not treat it as a real entry to refocus on — covered by `refocus_graph_moves_the_focus_but_ignores_the_more_sentinel`.
- **`o` on the "+N more" node** likewise must not try to select it in the list — covered directly in `brain_graph_key`'s `id != MORE_NODE_ID` guard (the sentinel's own id, `"\0more"`, can never collide with a real entry id, which is always a relative markdown path).
- **Re-entering graph mode after already having a focus** must not clobber it with whatever's currently selected in the list — covered by `toggle_graph_mode_keeps_an_existing_focus_instead_of_resetting_it`.
- **An entry with no links at all** — `build_neighbourhood_flow` still returns a one-node flow (just the focus), and the empty-state branch only fires when there's no focus *at all*, not merely no links; covered by `brain_flow`'s own `neighbourhood_always_includes_the_focus` (in `ninox_core`) plus `brain_graph_shows_an_empty_state_with_nothing_selected` pinning the no-focus case specifically.

---

## Task 1: `brain_flow` module — pure graph construction

**Files:** `crates/ninox-tui/src/tui/brain_flow.rs` (new), `crates/ninox-tui/src/tui/mod.rs` (module declaration)

- [x] Write failing tests (6 cases: hop-1 inclusion, focus marking, the more-node cap, no more-node below the cap, edges only between present nodes, zoom locked at 1, `None` for an out-of-range focus).
- [x] Confirm RED (temporarily stubbed `build_neighbourhood_flow` to always return `None`, watched 6 of 7 fail, restored).
- [x] Implement `EntryNode`/`MoreNode`/`BrainGraphNode` (`NodeContent`), `build_neighbourhood_flow` (three-column layout, edges filtered to the capped node set), `MORE_NODE_ID` sentinel, `column_center_y`.
- [x] Confirm GREEN.

## Task 2: `BrainView` graph state

**Files:** `crates/ninox-tui/src/tui/brain.rs`

- [x] Write failing tests (`toggle_graph_mode_*` ×2, `ensure_graph_flow_*` ×2, `refocus_graph_moves_the_focus_but_ignores_the_more_sentinel`).
- [x] Confirm RED.
- [x] Add `graph_mode`/`graph_focus`/`graph_flow`/`graph_flow_focus` fields and `toggle_graph_mode`/`ensure_graph_flow`/`graph_flow_mut`/`graph_selected_id`/`apply_graph_action`/`refocus_graph`/`select_entry` methods.
- [x] Confirm GREEN.

## Task 3: Key routing

**Files:** `crates/ninox-tui/src/tui/state.rs`

- [x] Write failing tests (`tab_toggles_brain_graph_mode_and_seeds_focus_from_selection`, `graph_mode_enter_refocuses_and_o_opens_in_list`, `esc_exits_graph_mode`).
- [x] Confirm RED.
- [x] `Tab` toggles graph mode (checked before the `editing_query`/list-mode dispatch); `brain_graph_key` handles arrows (→ `FlowAction::Select*` via `apply_graph_action`), `Enter` (→ `refocus_graph`), `o` (→ `select_entry` + leave graph mode), `Esc` (→ leave graph mode). Added one line to the existing "Brain tab" `HELP` entry rather than a new section (see Global Constraints).
- [x] Confirm GREEN.

## Task 4: Layout and rendering

**Files:** `crates/ninox-tui/src/tui/layout.rs`, `crates/ninox-tui/src/tui/view.rs`

- [x] Add `BrainLayout.graph: Rect` (the body area below the search-bar-turned-breadcrumb row, zero outside graph mode).
- [x] Write failing render tests (`brain_graph_renders_the_focus_and_its_links`, `brain_graph_shows_an_empty_state_with_nothing_selected`).
- [x] Confirm RED (temporarily no-op'd the `draw_brain_graph` call, watched both fail, restored).
- [x] `draw_brain_graph`: breadcrumb header in `bl.search`, the rataflow canvas in `bl.graph` (or an empty state for "nothing selected" vs. "no links").
- [x] Confirm GREEN; full `cargo test -p ninox-tui` sweep clean (367 passed).
- [x] `cargo clippy --workspace --all-targets -- -D warnings` clean.

## Task 5: Verification, PR

- [ ] `cargo test --workspace`.
- [ ] Kitty screenshot check per `BRIEF-common.md` §7: toggle into graph mode, confirm the focus/hop-1/hop-2 columns and edges render, walk with arrow keys, refocus with Enter, open in list with `o`, back out with Esc.
- [ ] Rebase onto `origin/v1` once item 5's PR merges (this branch stacks on it meanwhile); push; open PR; report to the orchestrator.
