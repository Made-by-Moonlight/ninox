# TUI Brain Backlinks/Related/Outlinks Panel Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the TUI Brain tab's entry view the same backlinks/outlinks data the GUI already fetches (`BrainIndex::backlinks`/`related`/`outlinks`), rendered as three navigable panes under the reading pane, with `Enter` jumping to a focused link entry.

**Architecture:** `BrainView` gains `backlinks`/`related`/`outlinks: Vec<BrainEntry>` and a `link_focus: Option<usize>` flattened-row cursor, plus `set_links` (applies a fetch result only if the selection hasn't moved on — the same staleness guard `semantic_done` already uses for search) and the focus/jump methods. A new `Action::LoadBrainLinks(id)` fires whenever `brain_key`'s post-dispatch check sees the selection change (list navigation, a filter narrowing it, or a jump), handled by a new async `load_brain_links` in `mod.rs` mirroring `load_brain`'s `spawn_blocking` pattern. `layout.rs` carves a `links: Rect` out of `doc_body`'s bottom when the selected entry has any links and there's room; `view.rs` renders it via a new `draw_brain_links` helper.

**Tech Stack:** Rust, ratatui/crossterm, tokio (`spawn_blocking` for the sqlite-backed fetch), existing `ninox_core::BrainIndex::{backlinks,related,outlinks}`.

**Spec:** `/Users/ethan.brodie/proj/ninox/.claude/design-ref/fleet-overview-v1/BRIEF-MLOPS-5350-tui-parity.md` (Worker B, item 1 "Brain list depth": *"Backlinks, related and outlinks panes in the entry view, with Enter to jump"*). The GUI (`crates/ninox-gui/src/components/brain_list.rs::reading_pane`) only currently *renders* two of the three ("Cited by" / "Links to"; `related` is fetched and tested but not yet shown anywhere in the GUI) — the brief's own text, not the GUI's current pixels, is authoritative here, so the TUI shows all three.

## Global Constraints

- Branch `v1-mlops-5350-tui-parity-brain-2` off `origin/v1` (merged PR #280 at tip); PR targets `v1`, title `MLOPS-5350: feat(tui): Brain backlinks, related, and outlinks panel`.
- `cargo fmt` not run wholesale.
- `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` must pass before pushing.
- Never copy GUI-only (iced-coupled) rendering code into the TUI; only the already-shared `ninox_core::brain`/`BrainIndex` data layer is reused.
- No new global-key collisions: `L` is unbound anywhere in `brain_key` today (confirmed by grep before choosing it).

## Review Focus

- **A stale async fetch arriving after the user already moved on**: `set_links` must drop a result for an id that is no longer selected — covered by `set_links_is_dropped_when_the_selection_moved_on`.
- **`L`/jump on an entry with zero links**: must no-op, not panic or focus an empty row — covered by `enter_link_focus_is_a_noop_with_no_links`.
- **Jumping to a link entry currently hidden behind an active kind/repo/tag filter**: must still land there (filters clear on jump) rather than silently failing — covered by `jump_to_focused_link_selects_that_entry_and_clears_filters`.
- **Esc while link-focused must not also close the whole reading pane** (two nested "back" meanings on the same key) — covered by `esc_exits_link_focus_without_closing_the_entry_or_changing_selection`.
- **Narrow terminal / entry with no links at all**: the links `Rect` must collapse to zero height and give the space back to the body, not panic on an empty layout — covered by the existing `every_view_renders_at_tiny_and_standard_sizes` sweep plus `link_rows_needed == 0` short-circuiting in `layout::brain()`.

---

## Task 1: `BrainView` link state and navigation methods

**Files:**
- Modify: `crates/ninox-tui/src/tui/brain.rs` (struct fields, `impl BrainView` methods, tests)

**Interfaces:**
- Produces: `pub backlinks/related/outlinks: Vec<BrainEntry>`, `pub link_focus: Option<usize>`; `pub fn set_links(&mut self, id: &str, backlinks, related, outlinks)`, `pub fn has_links(&self) -> bool`, `pub(crate) fn link_rows(&self) -> Vec<(&'static str, &BrainEntry)>`, `pub fn enter_link_focus/exit_link_focus/move_link_focus(delta: i64)/jump_to_focused_link(&mut self)`.

- [x] Step 1: Write failing tests (`set_links_is_dropped_when_the_selection_moved_on`, `enter_link_focus_is_a_noop_with_no_links`, `link_focus_cycles_across_all_three_sections_in_order`, `jump_to_focused_link_selects_that_entry_and_clears_filters`, `exit_link_focus_clears_it_without_changing_selection`).
- [x] Step 2: Confirm RED (`cargo test -p ninox-tui brain::` — method-not-found compile errors).
- [x] Step 3: Implement the fields/methods.
- [x] Step 4: Confirm GREEN — caught 3 test-fixture bugs (cursor left on a Group header, not an Entry) on the first pass; fixed the fixtures, not the implementation.
- [x] Step 5: Commit.

## Task 2: Key routing in `brain_key`

**Files:** `crates/ninox-tui/src/tui/state.rs` (`brain_key`, `Action` enum, `HELP`, tests)

- [x] Step 1: Write failing tests (`l_enters_link_focus_and_enter_jumps_to_the_focused_link`, `esc_exits_link_focus_without_closing_the_entry_or_changing_selection`).
- [x] Step 2: Confirm RED.
- [x] Step 3: Add `Action::LoadBrainLinks(String)`; insert link-focus-guarded arms (`j/k`/`Enter`/`Esc`+`h`) ahead of the existing `if b.open` arms (match order matters — first satisfying guard wins); add bare `L`; compute `before_id`/`after_id` around the whole match and return `LoadBrainLinks` on any selection change, superseding a single `Action::None` return. Document `L` and the link-focus keys in `HELP`.
- [x] Step 4: Confirm GREEN (required dropping `L`'s initial `if b.open` guard — the reading pane already always previews the selection regardless of `open`, so link focus shouldn't require it either).
- [x] Step 5: Commit.

## Task 3: `load_brain_links` and wiring `load_brain`'s initial fetch

**Files:** `crates/ninox-tui/src/tui/mod.rs`

- [x] Step 1: Add `load_brain_links(st, id)` (spawn_blocking `BrainIndex::open` + `backlinks`/`related(_,6)`/`outlinks`, applied via `set_links`; any error clears to empty for that id).
- [x] Step 2: Dispatch `Action::LoadBrainLinks(id) => load_brain_links(st, id).await`.
- [x] Step 3: Have `load_brain` call it once for the post-load selection, so the first entry shown on opening the tab already has its links instead of waiting for a cursor move.
- [x] Step 4: `cargo build -p ninox-tui` clean.

## Task 4: Layout and rendering

**Files:** `crates/ninox-tui/src/tui/layout.rs` (`BrainLayout.links`, `brain()`), `crates/ninox-tui/src/tui/view.rs` (`draw_brain_links`, call site, test)

- [x] Step 1: Add `links: Rect` to `BrainLayout`; compute it in `brain()` as a capped (`LINKS_MAX_H = 8`, leaving `doc_body` at least `DOC_BODY_MIN_H = 4`) bottom slice, sized from `link_rows().len()` plus one header row per distinct section, zero when the selection has no links.
- [x] Step 2: Write failing render test `brain_renders_the_backlinks_related_and_outlinks_panel_with_the_focused_row_marked`.
- [x] Step 3: Confirm RED (temporarily no-op'd the render call, watched the assertion fail, restored it — a lighter-weight substitute for pre-writing the render function, since the function itself came from the same design pass as the already-TDD'd state layer).
- [x] Step 4: Implement `draw_brain_links`: one header line per non-empty section (`"Cited by · N"` etc.), each entry as `display_title`, the focused row prefixed `▸` in the accent color, scrolled so the focused line stays roughly centered.
- [x] Step 5: Confirm GREEN; `cargo test -p ninox-tui tui::` full sweep clean (346 passed).
- [x] Step 6: `cargo clippy --workspace --all-targets -- -D warnings` clean.

## Task 5: Verification, PR

- [ ] Step 1: `cargo test --workspace` (expect only pre-existing, unrelated environment flakes under this machine's current parallel-worker load, per the earlier two PR cycles).
- [ ] Step 2: Kitty screenshot check per `BRIEF-common.md` §7 (isolated instance, synthetic brain with cross-linked entries).
- [ ] Step 3: Rebase on fresh `origin/v1`, push, open PR, report SHA + screenshots + deviations to the orchestrator, then stop pushing.
