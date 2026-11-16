# TUI Brain List Filters & Sort Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the TUI Brain tab the same kind/repo/tag filtering and Recent/A-Z/Most-linked sort the GUI's Brain List view already has, sharing the filter/sort logic with the GUI instead of re-deriving it.

**Architecture:** Move the GUI's pure (non-iced) filter/sort functions — `kinds_sorted_by_count`, `count_for_kind`, `top_values`, `resolve_edges`, `visible_entries`, and the `BrainSort` enum — from `ninox-gui/src/components/brain_list.rs` into `ninox-core/src/brain.rs`, alongside the already-shared `normalize_kind`/`kind_label`. Update the GUI to call the moved versions (no GUI behavior change). Then add `kind_filter`/`repo_filter`/`tag_filter`/`sort`/`edges` state to the TUI's `BrainView` (`crates/ninox-tui/src/tui/brain.rs`), a Kinds column + filter chips + sort indicator to the Brain layout/draw code, and new Brain-local keys to cycle/clear them.

**Tech Stack:** Rust, ratatui/crossterm (TUI), iced (GUI, unaffected behaviorally), existing `ninox-core` brain module.

**Spec:** `/Users/ethan.brodie/proj/ninox/.claude/design-ref/fleet-overview-v1/BRIEF-MLOPS-5350-tui-parity.md` (Worker B, item 1 "Brain list depth") and `BRIEF-common.md` §7. No `docs/superpowers/specs` design doc covers Brain (`2026-10-21-fleet-overview-ui-vision-design.md` line 123: "Brain — unchanged" at the fleet-shell level); the ticket brief is authoritative for Brain's own content, translating `Brain.dc.html`'s GUI behavior into the TUI's box-drawing language per the brief.

## Global Constraints

- Branch `v1-mlops-5350-tui-parity-brain-1` off `origin/v1` (already done, at `f0f393b7`); PR targets `v1`, title `MLOPS-5350: feat(tui): Brain list filters and sort`.
- `cargo fmt` is **not** run wholesale (tree isn't rustfmt-clean) — format only touched lines/blocks by hand to match surrounding style.
- `cargo clippy --workspace --all-targets -D warnings` and `cargo test --workspace` must pass before pushing.
- No horizontal scroll; no new global key collisions (verified in the read-back: Brain's `brain_key` dispatch never falls through to Fleet-global `command()`, so this task's new keys are free regardless).
- Never copy GUI-only (iced-coupled) code into the TUI — only the pure functions move, and they move to `ninox-core`, not to `ninox-tui` directly.

## Review Focus

- **Empty catalogue / zero entries**: `kinds_sorted_by_count(&[])` and `visible_entries` on an empty `entries` slice must return empty, not panic — covered by porting the existing GUI unit tests verbatim (they already exercise small fixed entry sets; add one explicit empty-slice case in Task 1).
- **Filter combination yields zero results**: kind+repo+tag all set, nothing matches — the TUI list must render an empty state, not panic or show stale rows (Task 3 test).
- **Clearing a filter after the underlying entry disappears** (e.g. active `repo_filter` for a repo no longer present after a reindex) — must fall back to showing "no entries match" rather than silently reverting the filter without telling the user (Task 3 test: filter on a repo, remove all its entries, confirm list is empty and filter badge still shows the (now zero-count) selection, matching GUI's behavior of not auto-clearing filters).
- **`MostLinked` sort with no edges yet computed** (first load, before `resolve_edges` has run) — must degrade to all-zero degrees, stable order, not panic (already covered by the ported `visible_entries_most_linked_sorts_by_global_degree`-style test plus a new zero-edges case in Task 1).
- **Cursor identity across a filter change**: changing `kind_filter`/`repo_filter`/`tag_filter`/`sort` while an entry is selected must not crash `cursor_index`/`set_cursor` when that entry drops out of the now-filtered view — `BrainView` already handles this generally via `Sel`/`last_index` fallback (`brain.rs:241-261`); Task 3 adds a test that filtering out the selected entry leaves the cursor on a valid remaining row (or empty, not out-of-bounds).

---

## Task 1: Move pure filter/sort logic from `ninox-gui` to `ninox-core`

**Files:**
- Modify: `crates/ninox-core/src/brain.rs` (insert after `strip_leading_h1` / its `title_tests` module, i.e. after line 234 in the current file, before `RebuildStats`)
- Modify: `crates/ninox-gui/src/components/brain_list.rs:34-217` (remove the moved functions and their `mod tests` cases at `:843-983`; keep `kind_color`, `distinct_kind_count`, `matches_search`-caller-only bits that stay GUI-side — note `matches_search` itself moves too since `visible_entries` depends on it; `format_ago` stays GUI-only)
- Modify: `crates/ninox-gui/src/app.rs:61-69` (remove the local `BrainSort` enum; re-export or directly reference `ninox_core::brain::BrainSort`)
- No test file changes beyond what's described above — tests move with the code.

**Interfaces:**
- Produces (for Task 3 and for the GUI's existing call sites):
  - `pub enum BrainSort { Recent, Az, MostLinked }` (derives `Debug, Clone, Copy, PartialEq, Eq, Default`, `Recent` is `#[default]`) in `ninox_core::brain`
  - `pub fn kinds_sorted_by_count(entries: &[BrainEntry]) -> Vec<(String, String, usize)>`
  - `pub fn count_for_kind(entries: &[BrainEntry], kind: Option<&str>) -> usize`
  - `pub fn top_values<'a>(entries: impl Iterator<Item = &'a BrainEntry>, extract: impl Fn(&'a BrainEntry) -> &'a [String], limit: usize) -> (Vec<(String, usize)>, usize)`
  - `pub fn resolve_edges(entries: &[BrainEntry], links: &[(String, String)]) -> Vec<(usize, usize)>`
  - `pub fn visible_entries<'a>(entries: &'a [BrainEntry], edges: &[(usize, usize)], kind: Option<&str>, repo: Option<&str>, tag: Option<&str>, query: &str, sort: BrainSort) -> Vec<&'a BrainEntry>`
  - (private helpers `edge_key`, `link_degrees`, `matches_search` move alongside, unchanged)

- [ ] **Step 1: Confirm the moved tests still describe the target behavior**

The functions and their 13 existing tests (`kinds_sorted_by_count_merges_duplicate_rows`, `kinds_sorted_by_count_ties_break_alphabetically`, `visible_entries_filters_by_kind`, `visible_entries_filters_by_repo_and_tag`, `visible_entries_filters_by_search_across_name_tags_body`, `visible_entries_az_sorts_by_name`, `visible_entries_most_linked_sorts_by_global_degree`, `count_for_kind_ignores_repo_tag_search`, `top_values_orders_by_count_then_alphabetically_and_truncates`, `resolve_edges_dedups_mutual_links`, `resolve_edges_skips_unresolvable_ids`, `resolve_edges_drops_self_links`) already exist verbatim in `crates/ninox-gui/src/components/brain_list.rs:843-983`. `distinct_kind_count_merges_singular_and_plural_like_kinds_sorted_by_count` and `format_ago_buckets_by_magnitude` test functions that are **not** moving (they stay GUI-only) — leave those two tests in `brain_list.rs`.

Add two new tests for the Review Focus gaps, written against the *current* GUI location first (so they fail/pass there before the move, keeping TDD honest):
```rust
#[test]
fn kinds_sorted_by_count_on_empty_entries_is_empty() {
    assert!(kinds_sorted_by_count(&[]).is_empty());
}

#[test]
fn visible_entries_most_linked_with_no_edges_keeps_a_stable_order() {
    let entries = vec![entry("a.md", "x", "Zebra"), entry("b.md", "x", "Apple")];
    let out = visible_entries(&entries, &[], None, None, None, "", BrainSort::MostLinked);
    assert_eq!(out.len(), 2); // degree 0/0 tie; no panic, no particular order asserted beyond "both present"
}
```

- [ ] **Step 2: Run the new tests to verify they pass against the current GUI location**

Run: `cargo test -p ninox-gui brain_list:: -- --nocapture`
Expected: all pass (these are additive, not yet moved).

- [ ] **Step 3: Move the functions, enum, and all 15 tests (13 existing + 2 new) into `crates/ninox-core/src/brain.rs`**

Insert the "Pure logic — filtering, sorting" section (functions listed in Interfaces, plus private `edge_key`/`link_degrees`/`matches_search`) immediately after the `title_tests` module closes (current line 234), before `RebuildStats`. Add a `#[cfg(test)] mod filter_tests { use super::*; ... }` directly after it with the 15 tests, using a locally-scoped test helper named `entry` (shadowing is fine inside its own module, matching the existing `title_tests`/`tests` pattern of each test module defining its own `entry` helper) built as: `BrainEntry { id: id.into(), entry_type: ty.into(), name: name.into(), tags: vec![], repos: vec![], updated: None, body: String::new() }`.

- [ ] **Step 4: Run the moved tests in their new location**

Run: `cargo test -p ninox-core brain::filter_tests:: -- --nocapture`
Expected: all 15 pass.

- [ ] **Step 5: Update `ninox-gui` to call the moved code**

In `crates/ninox-gui/src/app.rs:61-69`, delete the local `BrainSort` enum definition; add `pub use ninox_core::brain::BrainSort;` (or import it directly at each use site — prefer the re-export so `crate::app::BrainSort` keeps working for existing callers like `brain_list.rs:28`'s `use crate::app::{App, BrainSort, Message};` and `app.rs:6919-6920`'s test, with zero other call-site edits).

In `crates/ninox-gui/src/components/brain_list.rs`, delete the moved function bodies and their 13 moved tests (keep `kind_color`, `distinct_kind_count` + its test, `format_ago` + its test, `edge_key`/`link_degrees`/`matches_search` are gone with the move — remove them here); add `use ninox_core::brain::{count_for_kind, kinds_sorted_by_count, resolve_edges, top_values, visible_entries};` alongside the existing `use ninox_core::brain::{kind_label, normalize_kind};`. Leave every call site (`App::refresh_brain_edges` at `app.rs:1415`, the toolbar/kinds-column/entry-list rendering in `brain_list.rs`) unchanged — same function names, same signatures, just a different `use`.

- [ ] **Step 6: Run the full GUI test suite to confirm no behavior change**

Run: `cargo test -p ninox-gui`
Expected: all pass, same count as before the move (13 tests relocated, not deleted; 2 new ones added at the core side only).

- [ ] **Step 7: Run core + clippy**

Run: `cargo test -p ninox-core && cargo clippy -p ninox-core -p ninox-gui --all-targets -- -D warnings`
Expected: pass, no warnings.

- [ ] **Step 8: Commit**

```bash
git add crates/ninox-core/src/brain.rs crates/ninox-gui/src/app.rs crates/ninox-gui/src/components/brain_list.rs
git commit -m "refactor(brain): move brain list filter/sort logic to ninox-core"
```

---

## Task 2: Add filter/sort state to the TUI's `BrainView`

**Files:**
- Modify: `crates/ninox-tui/src/tui/brain.rs` (struct `BrainView` at `:85-106`, `items()` at `:206-229`, `search()` at `:146-202`, test module at `:482+`)

**Interfaces:**
- Consumes: `ninox_core::brain::{BrainSort, resolve_edges, visible_entries}` (Task 1).
- Produces (for Task 3):
  - New `BrainView` fields: `pub kind_filter: Option<String>`, `pub repo_filter: Option<String>`, `pub tag_filter: Option<String>`, `pub sort: ninox_core::brain::BrainSort`, `pub edges: Vec<(usize, usize)>`.
  - `pub fn set_edges(&mut self, links: &[(String, String)])` — wraps `resolve_edges(&self.entries, links)` and stores into `self.edges`; called once per data load, mirroring the GUI's `refresh_brain_edges`.
  - `pub fn cycle_kind_filter(&mut self, forward: bool)` — cycles `kind_filter` through `None` + the kinds returned by `kinds_sorted_by_count(&self.entries)` (by key), wrapping.
  - `pub fn cycle_sort(&mut self)` — `Recent -> Az -> MostLinked -> Recent`.
  - `pub fn clear_filters(&mut self)` — sets `kind_filter`/`repo_filter`/`tag_filter` to `None`.
  - `items()` changes signature-compatibly (same `Vec<Item>` return) but now filters its source rows through `visible_entries` before grouping/searching, when any of `kind_filter`/`repo_filter`/`tag_filter` is `Some` or `sort != Recent`.

- [ ] **Step 1: Write failing tests for filtered `items()` and cycling**

```rust
#[test]
fn kind_filter_narrows_the_flat_list() {
    let mut b = sample(); // existing fixture: concepts/ptyd, concepts/loose, errors/socket, patterns/Alpha
    b.grouping = Grouping::Flat;
    b.kind_filter = Some("errors".to_string());
    assert_eq!(lines(&b), ["  socket"]);
}

#[test]
fn repo_and_tag_filters_combine_with_kind() {
    let mut b = sample();
    b.grouping = Grouping::Flat;
    b.entries[0].repos = vec!["ninox".to_string()]; // ptyd
    b.kind_filter = Some("concepts".to_string());
    b.repo_filter = Some("ninox".to_string());
    assert_eq!(lines(&b), ["  ptyd"]);
}

#[test]
fn filtering_out_the_selected_entry_leaves_a_valid_cursor() {
    let mut b = sample();
    b.grouping = Grouping::Flat;
    b.move_cursor(0); // ptyd
    b.kind_filter = Some("errors".to_string()); // ptyd is "concepts", drops out
    let items = b.items();
    assert!(!items.is_empty());
    assert!(b.cursor_index(&items) < items.len());
}

#[test]
fn cycle_sort_wraps_recent_az_mostlinked() {
    let mut b = BrainView::default();
    assert_eq!(b.sort, BrainSort::Recent);
    b.cycle_sort();
    assert_eq!(b.sort, BrainSort::Az);
    b.cycle_sort();
    assert_eq!(b.sort, BrainSort::MostLinked);
    b.cycle_sort();
    assert_eq!(b.sort, BrainSort::Recent);
}

#[test]
fn cycle_kind_filter_wraps_through_none_and_every_kind() {
    let mut b = sample(); // kinds: concepts, errors, patterns
    assert_eq!(b.kind_filter, None);
    b.cycle_kind_filter(true);
    assert_eq!(b.kind_filter.as_deref(), Some("concepts"));
    b.cycle_kind_filter(true);
    assert_eq!(b.kind_filter.as_deref(), Some("errors"));
    b.cycle_kind_filter(true);
    assert_eq!(b.kind_filter.as_deref(), Some("patterns"));
    b.cycle_kind_filter(true);
    assert_eq!(b.kind_filter, None, "wraps back to All");
}

#[test]
fn clear_filters_resets_all_three() {
    let mut b = sample();
    b.kind_filter = Some("errors".into());
    b.repo_filter = Some("x".into());
    b.tag_filter = Some("y".into());
    b.clear_filters();
    assert_eq!((b.kind_filter, b.repo_filter, b.tag_filter), (None, None, None));
}
```

- [ ] **Step 2: Run tests, confirm they fail**

Run: `cargo test -p ninox-tui brain:: -- --nocapture`
Expected: FAIL (fields/methods not defined).

- [ ] **Step 3: Implement the fields and methods in `crates/ninox-tui/src/tui/brain.rs`**

Add the five fields to `BrainView` (all `Default`-able, matching the struct's existing `#[derive(Debug, Default)]`). Implement `cycle_kind_filter`/`cycle_sort`/`clear_filters`/`set_edges` as described. Change `items()`'s first lines so that when `kind_filter.is_some() || repo_filter.is_some() || tag_filter.is_some() || sort != BrainSort::Recent`, it builds its working entry set via `ninox_core::brain::visible_entries(&self.entries, &self.edges, self.kind_filter.as_deref(), self.repo_filter.as_deref(), self.tag_filter.as_deref(), "", self.sort)` and maps back to indices into `self.entries` before feeding the existing group/search logic — the query-search path (`!self.query.trim().is_empty()`) stays exactly as-is (search and column-filters are independent facets per the GUI's own `visible_entries` call, which takes `query` as one more filter — but the TUI's `search()` already has its own ranked-hit logic; **do not** pass `self.query` into `visible_entries` here, since that would run two different search behaviors — restrict `visible_entries`'s role to kind/repo/tag/sort only, and apply it to the entry set `search()`/grouping already operate over by filtering `self.entries` by index membership before grouping, leaving `search()` itself untouched for its own query path).

- [ ] **Step 4: Run tests, confirm they pass**

Run: `cargo test -p ninox-tui brain:: -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Run clippy**

Run: `cargo clippy -p ninox-tui --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/ninox-tui/src/tui/brain.rs
git commit -m "feat(tui): kind/repo/tag filters and sort state for the Brain tab"
```

---

## Task 3: Wire the filter bar, Kinds column, sort indicator, and keys

**Files:**
- Modify: `crates/ninox-tui/src/tui/layout.rs:86-100,353-387` (`BrainLayout` struct and its layout function — add a `kinds: Rect` column and a `filters: Rect`/status line for active repo/tag + sort)
- Modify: `crates/ninox-tui/src/tui/view.rs` (`draw_brain`, wherever the brain draw function is) — render the Kinds column (using `ninox_core::brain::kinds_sorted_by_count`) with the active `kind_filter` highlighted, a one-line active-filters/sort indicator above the list
- Modify: `crates/ninox-tui/src/tui/state.rs` (`brain_key` at `:2028-2114`) — new key handling
- Modify: `crates/ninox-tui/src/tui/state.rs` `HELP` const (`:531-590`) — document the new Brain-local keys

**Interfaces:**
- Consumes: `BrainView::{cycle_kind_filter, cycle_sort, clear_filters}` (Task 2), `ninox_core::brain::kinds_sorted_by_count` (Task 1).
- Produces: new key bindings, scoped entirely inside `brain_key` (never reachable from Fleet, per the read-back's collision analysis):
  - `[` / `]` — cycle `kind_filter` back/forward (mirrors the Kinds column's up/down feel without colliding with `j`/`k`'s existing list-cursor meaning)
  - `s` — cycle `sort` (Recent → A-Z → Most-linked → Recent)
  - `x` — clear all three filters (kind/repo/tag) back to "All"
  - (Repo/tag filters themselves are set by selecting a chip in the Kinds column with Enter in a future small follow-up only if the brief requires standalone repo/tag chip rows beyond what the Kinds column covers — **for this task, scope repo/tag filtering to the state/logic existing in Task 2 and expose only the kind cycle + clear-all key; wire the repo/tag fields to the Kinds-column UI if time allows, else leave them settable only via `BrainView` for GUI-parity logic reuse and note in the PR description that repo/tag chip UI is deferred** — resolve this open question by implementing the kind cycle + clear-all now, since that is what the brief's "Kind column filter" + "Repo and tag filters" items minimally require for the TUI to stop being list-only, and note the chip-row UI simplification explicitly in the PR body per the ticket's "pick the most spec-consistent option and note it" rule.)

- [ ] **Step 1: Write a failing key-route test**

```rust
#[test]
fn bracket_keys_cycle_kind_filter_and_s_cycles_sort() {
    let mut st = test_state_with_brain_entries(/* concepts, errors, patterns fixture */);
    st.view = View::Brain;
    brain_key(&mut st, KeyEvent::from(KeyCode::Char(']'))).unwrap();
    assert_eq!(st.brain.kind_filter.as_deref(), Some("concepts"));
    brain_key(&mut st, KeyEvent::from(KeyCode::Char('s'))).unwrap();
    assert_eq!(st.brain.sort, BrainSort::Az);
    brain_key(&mut st, KeyEvent::from(KeyCode::Char('x'))).unwrap();
    assert_eq!(st.brain.kind_filter, None);
}
```
(Match the exact helper names/signatures already used by neighboring tests in `state.rs`'s test module — read the nearest existing `brain_key` test for the real fixture constructor name before writing this, since `test_state_with_brain_entries` is illustrative, not a confirmed existing helper.)

- [ ] **Step 2: Run, confirm failure**

Run: `cargo test -p ninox-tui state:: -- --nocapture`
Expected: FAIL.

- [ ] **Step 3: Implement the three key arms in `brain_key`**

Add `KeyCode::Char('[') => st.brain.cycle_kind_filter(false)`, `KeyCode::Char(']') => st.brain.cycle_kind_filter(true)`, `KeyCode::Char('s') => st.brain.cycle_sort()`, `KeyCode::Char('x') => st.brain.clear_filters()` to the match in `brain_key`, each followed by whatever redraw/cursor-reclamp the existing `t` (cycle_grouping) arm does (read that arm first and mirror it, since `cycle_grouping` already resets cursor state the same new filter-changing actions will need).

- [ ] **Step 4: Run, confirm pass**

Run: `cargo test -p ninox-tui state:: -- --nocapture`

- [ ] **Step 5: Add the Kinds column and filter/sort indicator to layout + draw**

In `layout.rs`, extend `BrainLayout` with a `kinds: Rect` column (narrow, left of `list`, mirroring the GUI's `KINDS_COLUMN_WIDTH` proportionally) and shrink `list` to fit; add a one-line `status: Rect` above `list` for "Sort: A–Z · Kind: errors · Repo: — · Tag: —". In `view.rs`'s brain draw function, render the kinds column via `ninox_core::brain::kinds_sorted_by_count(&st.brain.entries)`, highlighting the active `kind_filter` row, and render the status line from `st.brain.{kind_filter,repo_filter,tag_filter,sort}`.

- [ ] **Step 6: Update `HELP` in `state.rs`**

Add to the Brain-tab section of the `HELP` const: `"[ / ]" => "cycle Kind filter"`, `"s" => "cycle sort (Recent/A-Z/Most-linked)"`, `"x" => "clear Brain filters"`.

- [ ] **Step 7: Run full TUI test suite and clippy**

Run: `cargo test -p ninox-tui && cargo clippy -p ninox-tui --all-targets -- -D warnings`
Expected: pass, no warnings.

- [ ] **Step 8: Manual kitty screenshot check (per BRIEF-common.md §7)**

Build release, launch an isolated test instance (`env -u` every `NINOX_*`, fresh `--db`, scratch `config.toml`/`TMUX_TMPDIR`, seeded synthetic brain entries spanning 3+ kinds/repos/tags) in kitty at 200x55 and 120x40, capture the Brain tab window-only via JXA `CGWindowListCopyWindowInfo` + `screencapture -x -o -l <id>`, compare against `Brain.dc.html`/`TUI.dc.html`, close the test instance.

- [ ] **Step 9: Commit**

```bash
git add crates/ninox-tui/src/tui/layout.rs crates/ninox-tui/src/tui/view.rs crates/ninox-tui/src/tui/state.rs
git commit -m "feat(tui): Brain Kinds column, filter/sort keys, and help entries"
```

---

## Task 4: Rebase, final verification, PR

- [ ] **Step 1:** `git fetch origin v1 && git rebase origin/v1`
- [ ] **Step 2:** `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings`
- [ ] **Step 3:** Push branch, open PR titled `MLOPS-5350: feat(tui): Brain list filters and sort` against `v1`, body `## Why` / `## Summary` / `## Test plan`, noting the repo/tag-chip-UI deferral decision from Task 3 and the two Brain-local keys reusing globally-overloaded chords is **not** the case here (`[`/`]`/`s`/`x` are all previously-unbound in Brain) — no help-modal cross-tab note needed for *this* PR (that note applies to the later reindex/catalogue-picker PRs per the approved read-back).
- [ ] **Step 4:** Report final SHA + screenshots + deviation list per `BRIEF-common.md` §7.4, then stop pushing.
