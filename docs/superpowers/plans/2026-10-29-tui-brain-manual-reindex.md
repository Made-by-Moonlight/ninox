# TUI Brain Manual Reindex Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the TUI Brain tab trigger a full, on-demand reindex (`R`) instead of only ever reindexing implicitly after an edit/add/delete.

**Architecture:** Add `Job::ReindexAll` (no entry id) alongside the existing per-entry `Job::Reindex{id}`/`Write`/`Delete`, sharing `run_job`'s sync-toml/remote-sync/rebuild pipeline. A new `Action::ReindexBrain` dispatches to the existing `start_brain_job`, which already runs jobs off the loop and reloads via `brain_job_done` → `load_brain`.

**Tech Stack:** Rust, tokio (`spawn_blocking`), existing `ninox_core::BrainIndex::rebuild`.

**Spec:** `BRIEF-MLOPS-5350-tui-parity.md` (Worker B, item 1: *"Manual reindex: `ctrl+r` inside Brain, or `R`; avoid clashing with global keys"*). Per the orchestrator-approved read-back, bare `R` was chosen (Brain's key dispatch is self-contained and never falls through to the global `R`/reap or `Ctrl+R`/restart-all bindings, so there's no code collision — only a cross-tab naming overlap, now called out in the help modal).

## Global Constraints

- Branch `v1-mlops-5350-tui-parity-brain-3` off `origin/v1`; PR targets `v1`, title `MLOPS-5350: feat(tui): Brain manual reindex key`.
- `cargo fmt` not run wholesale.
- `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` must pass before pushing.

## Review Focus

- **`R` pressed with nothing on disk changed since the last auto-reindex**: must still succeed and report the (unchanged) count, not error — covered by `reindex_all_rebuilds_from_whatever_changed_on_disk_directly`, which only writes to disk directly (bypassing `Job::Write`) precisely to prove `ReindexAll` doesn't depend on an id at all.
- **The notify/indexing-label text for a job with no id**: `Job::id()` returning `""` must not leak into the user-facing message (`"reindexed  · indexed..."` with a stray double space) — covered by `reindex_all_summarizes_without_a_per_entry_id`.
- **Existing per-entry jobs' summaries must not regress** when `summary()` replaces the old inline `format!` — covered by the same test's `Job::Write` case, and the pre-existing `edit_and_reindex_round_trip` round-trip test.

---

## Task 1: `Job::ReindexAll` and its summary text

**Files:** `crates/ninox-tui/src/tui/brain.rs` (`Job` enum/impl, `run_job`, tests)

- [x] Write failing tests: `reindex_all_summarizes_without_a_per_entry_id`, `reindex_all_rebuilds_from_whatever_changed_on_disk_directly`.
- [x] Confirm RED (`Job::ReindexAll`/`summary` don't exist).
- [x] Add the variant, `id()`/`verb()` arms, and `Job::summary(&self, indexed: usize) -> String` (replaces the old inline `format!` in `run_job`, used by every job so existing summaries are unchanged); `run_job`'s per-job-kind match treats `ReindexAll` like `Reindex{..}` (no file I/O, just reindex).
- [x] Confirm GREEN.

## Task 2: Action, key, and dispatch

**Files:** `crates/ninox-tui/src/tui/state.rs` (`Action`, `brain_key`, `HELP`, tests), `crates/ninox-tui/src/tui/mod.rs` (`start_brain_job`, dispatch)

- [x] Write failing test `shift_r_triggers_a_full_reindex`.
- [x] Confirm RED.
- [x] Add `Action::ReindexBrain`; bare `KeyCode::Char('R') => return Action::ReindexBrain` in `brain_key` (confirmed unbound beforehand); dispatch to `start_brain_job(st, lp, brain::Job::ReindexAll)`; give `start_brain_job`'s indexing/notify label a `ReindexAll`-specific case ("reindexing the brain…") instead of the per-id `"indexing {id}…"`. Document `R` in `HELP`, naming the cross-tab overlap with global `R`/reap.
- [x] Confirm GREEN.

## Task 3: Verification, PR

- [ ] `cargo test --workspace` (pre-existing unrelated flakes expected under this machine's load, per the last two PRs).
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Kitty screenshot check per `BRIEF-common.md` §7.
- [ ] Rebase on fresh `origin/v1`, push, open PR, report to the orchestrator, stop pushing.
