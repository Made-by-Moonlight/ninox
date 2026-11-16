# MLOPS-5350 item 4: TUI session info panel and diff — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The inspector (`i`) beside an attached or zoomed pane gains an Info tab (PR, CI checks, full review comments, Definition of done, Outcome and evidence) and a Diff tab (file list with `+/-` counts; Enter opens a scrollable unified diff), for live sessions as well as ended ones.

**Architecture:** Shared logic first, one commit each: `ninox_overview::completion` (the Definition-of-done evaluation and outcome lines both apps had inline) and `ninox_overview::diff::files` (a unified diff split into per-file sections with hunk-only counts). The TUI's new `inspector` module builds each tab's lines pre-wrapped to the panel width, so `layout` clamps scrolling to their exact count. Diffs come from the GUI's own `brain_harvest::compute_diff`, fetched off the loop on selection and every 10s while the Diff tab shows.

**Tech Stack:** Rust, ratatui (`ninox-tui`), `ninox-overview`, `ninox-core::lifecycle::brain_harvest`.

**Spec:** `.claude/design-ref/fleet-overview-v1/BRIEF-MLOPS-5350-tui-parity.md`, Worker A item 4.

## Global Constraints

- Comment bodies go through `comment_markdown::plain_text` and are wrapped in full — never cut to 60 chars.
- No horizontal scroll: prose word-wraps, diff lines hard-wrap by display width.
- The inspector now also shows beside a zoomed pane (the brief: "next to an attached or zoomed pane"); zoom still hides the sidebar.

## Review Focus

1. A diff fetch that lands after the selection moved must not overwrite the new session's (dropped by `DiffView::finish`).
2. A refetch with fewer files keeps the cursor and any open file in range.
3. Remote sessions, sessions with no workspace, and an empty diff each say why there's no list (and that uncommitted edits aren't included — `compute_diff` is `default...HEAD`, same as the GUI).
4. Bare keys stay with the agent while the pane has focus; the inspector takes focus only via prefix-Tab, and Esc peels back (open file → list → pane).
5. Selected rows stay legible: the Diff list's cursor row and the sidebar's selected row never paint a glyph in their own `highlight` colour.

---

### Task 1: Shared Definition of done
- [x] Move `criterion_done`/`criterion_label`/`definition_of_done`/`outcome_lines` and their tests from the GUI's `fleet_detail.rs` to `ninox_overview::completion`; GUI and TUI detail pane call it. Commit.

### Task 2: Shared diff split
- [x] `ninox_overview::diff::files(&str) -> Vec<FileDiff>` with tests for modified/added/deleted/renamed/binary files and a removed line starting `---`. Commit.

### Task 3: TUI inspector tabs
- [x] Tests: layout (`zoom_hides_the_sidebar_but_keeps_an_opened_inspector`, Diff tab wider, inspector focus keeps the pane layout); keys (prefix-Tab focus cycle, tab switch and bounded Info scroll, Diff list/open/scroll/esc/`r`, `i` hands focus back); view (Info content incl. an over-60-char comment in full, Diff list and open file, the unavailable messages, highlight contrast on the cursor row and the sidebar's cost); `DiffView` stale-result and shrink cases.
- [x] `Focus::Inspector`, `inspector::{Tab, DiffView}`, `inspector_key`, `sync_inspector`, `draw_inspector`; the selected sidebar row lifts any `highlight`-coloured metadata to `ink_2`.
- [x] `cargo test --workspace`, clippy; commit.
