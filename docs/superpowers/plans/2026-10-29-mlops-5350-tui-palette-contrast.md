# MLOPS-5350: TUI modal palette-contrast fix — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the root cause behind PR #283's invisible cost/hint text — `Palette::terminal()` giving `faint` (and `rule`) the same colour as `card` — at the palette level, and add a regression test covering every `modal_block`-rendered modal, not just Spawn's.

**Architecture:** `Palette::terminal()` is the TUI's default (used whenever `[tui] colors` isn't set to `"field-notes"`). Every modal (`Confirm`/`Spawn`/`AddRemote`/`Goto`/`Help`/`Restore`/`Settings`) renders through the shared `modal_block` helper, which fills its whole area with `card`. Any span styled `faint` or `rule` on that background was invisible (`fg == bg`, both `Color::DarkGray`). Generalize the cell-walk regression test PR #283 added for Spawn alone to every modal variant under both the terminal and a themed palette. First fix attempt changed `faint`/`rule` themselves to `Color::Gray` — an orchestrator review caught that this dims the *main* screens too (footer hints, toolbar labels, folded rows all read `faint`/`rule` against `paper`, where `DarkGray` already contrasts fine), since those tokens are shared app-wide, not modal-specific. Final fix: leave `faint`/`rule` untouched and change `card` (used only by modal/selected-card surfaces) to `Color::Black` instead — distinct from both without touching anything outside modals/selected-cards.

**Tech Stack:** Rust, ratatui (`ninox-tui`).

**Spec:** Follow-up from PR #283 (MLOPS-5350 item 1) review — a peer found the root cause was a palette-level colour collision, not a per-modal bug, and asked for the fix to be applied there so new modals can't reintroduce it.

## Global Constraints

- `cargo fmt`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` must pass before pushing.
- Branch from a fresh `origin/v1`, PR targets `v1`.

## Review Focus

1. A modal added after this PR that styles a span `faint` or `rule` must still be caught if a *future* palette change reintroduces a collision — the test walks the palette's actual `card` value, not a hardcoded colour, so it holds regardless of what `card`/`faint` are set to.
2. The fix must not just mask Spawn's own spans (already fixed in #283) — it must hold for every other modal that never got that treatment.
3. Changing `Palette::terminal()` must not change the *named* (non-RGB) nature of its colours — `default_palette_uses_only_the_terminals_own_colours` already pins this.

---

### Task 1: Generalize the contrast regression test, then fix the palette

**Files:**
- Modify: `crates/ninox-tui/src/tui/view.rs` (generalize the existing Spawn-only test)
- Modify: `crates/ninox-tui/src/tui/palette.rs` (`Palette::terminal()`)

- [x] **Step 1:** Generalize `spawn_modal_text_is_never_the_same_colour_as_its_own_card_background` into `no_modal_paints_a_glyph_the_same_colour_as_its_own_card_background`, walking all seven `Modal` variants under both `Palette::terminal()` and a themed palette.
- [x] **Step 2:** Run it — fails on `Confirm` (`faint` on `card`).
- [x] **Step 3 (superseded):** First attempt fixed `Palette::terminal()` by changing `faint`/`rule` from `Color::DarkGray` to `Color::Gray`. Orchestrator review rejected this: `faint`/`rule` are read on the main screens (footer hints, toolbar labels, folded rows) against `paper`, where `DarkGray` already contrasts fine, so dimming them app-wide to fix a modal-only bug regressed the main-screen hierarchy.
- [x] **Step 4 (final):** Fix `Palette::terminal()` at `card` instead, leaving `faint`/`rule` as `Color::DarkGray`: `card: Color::DarkGray` → `Color::Black`. `card` is used only by `modal_block` fills and the Fleet island's selected-card highlight, so this is scoped to exactly the surfaces that had the collision.
- [x] **Step 5:** Re-run the cell-walk test — passes for all seven modals under both palettes.
- [x] **Step 6:** Verify in kitty: before/after screenshots of Fleet (empty-state footer/toolbar, pixel-identical before and after, confirming the main screen is unchanged), Goto (divider/filter-chips/hint text invisible before, all readable after), and Confirm via the Worker Types delete-type dialog (detail line invisible before, readable after).
- [x] **Step 7:** `cargo test -p ninox-tui`, `cargo build --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`.
- [x] **Step 8: Commit.**
