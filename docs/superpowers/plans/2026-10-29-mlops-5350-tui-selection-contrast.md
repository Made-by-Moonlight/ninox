# MLOPS-5350: TUI selection-highlight contrast audit — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Finish the follow-up #288 asked for: audit every place a selected row/card can render `faint`-coloured content on its own `highlight`-coloured background (the same invisible-text bug class as the modal fix, just not gated by `modal_block`), fix every confirmed instance, and extend the cell-walk regression test to selected rows/cards so the class can't regress.

**Architecture:** #288 split `Palette::card` (modal fills) from a new `Palette::highlight` (every "this row/card is selected" background), both `Color::DarkGray` in the terminal palette alongside `faint`/`rule`. Anything that still styles a span `faint` without checking whether its own row is selected collides with `highlight` the same way `faint`-on-`card` collided before #288's first fix. A peer verifying #288 found one more instance (the Fleet island's status dot, via `member_status_color`'s `WaitingOnPr`/`Stopped`/`Done` → `p.faint`/`p.done` mapping) and asked for a full audit: the Brain list's group headers and search-hit line numbers, the sidebar's fold chevron and terminal-session name, plus the status dot — fixed at the `ink_2`-instead-of-`faint`-when-selected level (the same technique #283 and #288 already used), not by changing the palette further.

**Tech Stack:** Rust, ratatui (`ninox-tui`).

**Spec:** Follow-up from PR #288 review — a peer verified the `highlight` fix but found the Fleet island's status dot still collides, and asked for the same audit to cover Brain's list and the sidebar before merging.

## Global Constraints

- `cargo fmt`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` must pass before pushing.
- Branch from a fresh `origin/v1` (now including #288), PR targets `v1`.

## Review Focus

1. The fix must hold under both `Palette::terminal()` and a themed palette — the cell-walk test checks the palette's actual `highlight` value, not a hardcoded colour.
2. A view not explicitly audited here (e.g. Kanban, Workspaces panel) was already checked during #288 and found to use `ink`/`ink_2`/`accent` exclusively in its selected-row text, so it's not re-covered by name below — only the sites that actually use `faint` get a fix and a test case.
3. The Brain list's search-hit line-number prefix (`{n}:`) needs a fixture entry with real body text to exercise at all (`sample()`'s entries start empty) — added inline in the new test, following the existing `search_is_a_ranked_flat_list_with_word_prefixes_and_smart_case` test's own pattern of mutating one entry's `body` after calling `sample()`, rather than changing the shared fixture for every other test that uses it.

---

### Task 1: Fix every confirmed `faint`-on-`highlight` site, then extend the regression test

**Files:**
- Modify: `crates/ninox-tui/src/tui/island_flow.rs` (`CardContent::render`'s status dot)
- Modify: `crates/ninox-tui/src/tui/view.rs` (`session_line`'s chevron/name; `draw_brain`'s group/entry spans and `highlight_terms`)
- Test: `crates/ninox-tui/src/tui/view.rs` (new test, same file as #288's modal test)

- [x] **Step 1:** Fix the Fleet island status dot: `CardContent::render` computes `dot_color`, swapping to `p.ink_2` only when `ctx.selected && self.status_color == p.highlight` — a value comparison, not naming `faint`/`done` directly, so it still catches a future palette change that reintroduces the collision.
- [x] **Step 2:** Fix the sidebar: give `session_line` a `selected: bool` parameter; its fold-chevron and terminal-status name spans use `p.ink_2` instead of `p.faint` when `selected`. Both call sites updated (`draw_side_item` passes its own `selected`; the Goto modal's match list passes `false`, since that list never fills with `highlight`).
- [x] **Step 3:** Fix the Brain list: the per-row `dim_fg` (`ink_2` when `i == bl.cursor`, else `faint`) replaces every hardcoded `p.faint` in `Item::Group`'s chevron/label/count and `Item::Entry`'s search-hit marker and line-number prefix. `highlight_terms` takes the caller's dim colour as a parameter instead of hardcoding `p.faint`, so its one call site passes `dim_fg` too.
- [x] **Step 4:** Write `no_selected_row_or_card_paints_a_glyph_the_same_colour_as_its_own_highlight_background`: for both palettes, render the Fleet sidebar (a terminal-status selected row), the Fleet island (a `PrOpen` orchestrator, opened and selected — reproduces the status-dot bug), Remotes (a selected row with unknown health), Worker Types (a selected Toggle-off field), Settings (a modal; a selected Toggle-off field, to also guard `highlight` nested inside a `card` fill), Brain (a selected untagged group header), and Brain again (a search hit with a seeded `body` so `hit.line` fires, exercising the `{n}:` prefix) — assert no non-space glyph has `fg == bg` wherever `bg == palette.highlight`.
- [x] **Step 5:** Confirm each fix is load-bearing: revert the island status-dot fix locally, re-run the new test, observe the `"●"` failure; restore. Repeat for the Remotes `dim_fg` fix (the `"○"` failure) and the Brain line-number fix (the `"2"` failure); restore each.
- [x] **Step 6:** `cargo test -p ninox-tui`, `cargo build --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`.
- [x] **Step 7: Commit.**
