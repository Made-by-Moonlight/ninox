# MLOPS-5350 item 7: TUI Board and Fleet parity — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:**
- **Board:** a filter field (`/`), orchestrator scope chips (`o` cycles), and Resume all behind a confirm.
- **Fleet:** a Machine filter (All / Local / each remote; `M` cycles) and per-island fold (`f` on the island in focus, `ctrl+f` for all, since `F` is re-file).
- **Help fix:** the help modal's overflowing key cell, folded in from the #305 review.

**Architecture:** Shared logic first, in its own commits:
- `ninox_overview::board`: `column_absorbs`, `on_board` (scope and name/repo query; never an orchestrator's own session or the prompt agent) and `resume_all_targets`. All three were inline in the GUI's `fleet_board.rs`, with a partial copy in the TUI's `kanban.rs`.
- `ninox_overview::islands::MachineFilter`: `matches`, `next` and `machine_ids`. These were the GUI's `FleetMachineFilter`/`matches_filter`; `FleetMachineFilter` is now a re-export.

On the TUI side:
- `KanbanView` gains `query`/`editing_query`/`scope`, and `column_rows` filters through `on_board`. A Board header row shows the filter, the chips (collapsing to `+n` when narrow), `A resume all (n)` and `shown/total`.
- `FleetState` keeps `all_islands` and serves `islands` narrowed to `machine_filter`. If the filter hides the opened island, it closes.
- Per-island fold mirrors the GUI: `fold_expanded` is the default and `fold_flipped` the islands flipped from it. `island_grid_heights`, `island_rows`, `tree_rows` (per orchestrator group) and the opened island's flow all read `island_fold_expanded`.

**Tech Stack:** Rust, ratatui (`ninox-tui`), `ninox-overview`.

**Spec:** `.claude/design-ref/fleet-overview-v1/BRIEF-MLOPS-5350-tui-parity.md`, Worker A item 7.

## Global Constraints

- Key changes:
  - `f` on Fleet now folds one island; `ctrl+f` folds all (the old `f`).
  - New Fleet key: `M`.
  - New Board keys: `/`, `o`, `A`. Esc on the Board clears an active filter before leaving.
- The machine field and `M` hint only appear once a remote machine exists.

## Review Focus

1. Typing a digit into the Board filter must not switch tabs.
2. Resume all never includes the prompt agent, and skips sessions already resuming.
3. The tree's folded summary row follows each orchestrator group's own fold.
4. No help key cell is wider than its 34-column key column.

---

### Task 1: Shared logic
- [x] `ninox_overview::board` with tests; the GUI's board calls it. Commit.
- [x] `islands::MachineFilter` with tests; the GUI uses the re-export. Commit.

### Task 2: TUI
- [x] Board: `KanbanView` fields, `column_rows`/`shown`/`scopes`/`cycle_scope`, keys, `Pending::ResumeAll`/`Action::ResumeAll`, header row and its layout.
- [x] Fleet: `machine_filter`/`machines`/`all_islands`, `fold_flipped`, `focused_island`, keys, toolbar machine field/hint.
- [x] Help: new Board/Fleet rows, a shortened `→ · ←` key cell, `HELP_KEY_W`.
- [x] Tests: kanban scope/query/cycle; Board keys (typing, Esc, `o`, `A`); Board header; `f`/`ctrl+f`; `M` (and closing a filtered-away open island); help key width; the full help text. `cargo test --workspace`, clippy; commit.
