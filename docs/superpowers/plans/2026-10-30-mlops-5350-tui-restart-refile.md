# MLOPS-5350 item 3: TUI single-session restart and re-file — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Restart (`ctrl+r`) and re-file (`F`) the selected session from the TUI, each behind a y/n confirm, through the same shared functions as the GUI.

**Architecture:** `ninox_ui_shared::restart::refile_request` builds a `relaunch_in_place` request on a freshly minted conversation id (one commit; the GUI's `RefileSession` switches to it from its inlined kill/restore/spawn sequence). Restart is the GUI's Restart/Resume action: `Pending::Restart` confirms into the existing `Action::Resume` (`resume_plan` + `relaunch_in_place`). Re-file is `Pending::Refile` → `Action::Refile`, run through `run_op`.

**Tech Stack:** Rust, ratatui (`ninox-tui`), `ninox-ui-shared::restart`/`spawn_util`.

**Spec:** `.claude/design-ref/fleet-overview-v1/BRIEF-MLOPS-5350-tui-parity.md`, Worker A item 3.

## Global Constraints

- Restart-all moves from `ctrl+r` to `ctrl+shift+r` (the brief's first option). The TUI pushes the kitty keyboard protocol's disambiguate flag, so supporting terminals report it distinctly; in ones that don't, `ctrl+shift+r` arrives as `ctrl+r`, so the single-restart confirm also offers `a` = restart all.
- Re-file is behind a confirm too: it discards the conversation.

## Review Focus

1. `ctrl+r` inside an attached pane still goes to the agent (shell reverse search); only the list and `prefix ctrl+r` restart.
2. The confirm's third button label and its click target come from one `confirm_all_label`, so "Restart all" is never drawn wider than it is clickable.
3. `F` without a recorded workspace explains why instead of opening a confirm that can only fail.

---

### Task 1: Share the re-file request

- [x] **Step 1:** Test `refile_request_relaunches_the_same_id_on_a_fresh_conversation` (fresh id in `base_cmd`, `Terminated` on failure, `None` without a workspace).
- [x] **Step 2:** `restart::refile_request(Session, bool, &AppConfig, i64) -> Option<RelaunchRequest>`; GUI `RefileSession` calls it and `relaunch_in_place`.
- [x] **Step 3:** clippy; commit on its own.

### Task 2: TUI keys

- [x] **Step 1:** Tests: `ctrl_r_restarts_just_the_selected_session_after_a_confirm`, `the_restart_confirm_offers_restart_all_for_terminals_without_ctrl_shift_r`, `ctrl_shift_r_asks_to_restart_every_live_agent`, `shift_f_re_files_the_selection_after_a_confirm_or_says_why_not`; move `restart_all_*` tests to `ctrl+shift+r`; view test `restart_confirm_labels_its_third_button_restart_all`.
- [x] **Step 2:** `Pending::{Restart, Refile}`, `Action::Refile`, `confirm_all_label`, `confirm_rects(.., Option<&str>)`; keys in `command()`; help rows (kept to the same height — the help modal doesn't scroll).
- [x] **Step 3:** `perform`'s `Action::Refile` via `run_op`; detail pane hints `^r restart · F re-file`.
- [x] **Step 4:** `cargo test --workspace`, clippy; commit.
