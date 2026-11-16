# Session/Store Encapsulation (Port 2) Implementation Plan

> **For agentic workers:** This is a scoped, behavior-preserving visibility
> refactor executed inline in this worker session (no subagent fan-out —
> the change is small and the "task" is already the unit of review: one PR).

**Goal:** Shrink the public surface of `ninox-core`'s `store.rs`/`types.rs`
so call sites with no genuine external-crate caller become `pub(crate)`,
turning a missed/incompatible call site into a compile error (per the
`c151750`/`f63f7c8` incident) rather than a runtime surprise — without
changing any observable behavior.

**Architecture:** Grep-verified, not guessed. For every `pub fn` on `Store`
and every `pub` item in `types.rs`, count real call sites outside
`ninox-core` (`ninox-gui`, `ninox-tui`, `ninox-ui-shared`, `ninox-server`;
`ninox-ptyd` doesn't depend on `ninox-core` at all). Zero external callers
→ `pub(crate)`. One or more → stays `pub`, unchanged.

**Tech Stack:** Rust / Cargo workspace.

**Spec:** `docs/superpowers/specs/2026-10-21-hexagonal-session-ports-design.md`
(Port 2 section only — Port 1's trait/fan-out work is separate, untouched
here).

## Investigation findings (current `v1`, re-verified, spec numbers had drifted)

- `store.rs` is now 1,857 lines (spec said 1,681), `types.rs` 702 lines
  (spec said 694) — grown via PR #208/#209/#210, direction unchanged.
- `store.rs` has 37 `pub fn`s, 0 `pub(crate)`. Grepped every method name
  against `ninox-gui/tui/ui-shared/server` for `.method_name(` call sites:
  - **11 have zero external callers** (all confirmed to have ≥1 internal
    caller inside `ninox-core`, i.e. not dead code):
    `update_session_activity`, `set_stacked_deps`,
    `update_statusline_metrics`, `register_migrated_orchestrator_runtime`,
    `record_message_delivered`, `sessions_by_orchestrator`,
    `detach_from_orchestrator`, `upsert_pr`, `upsert_ci_status`,
    `upsert_comment`, `delete_pr_watches_for_pr`.
  - The remaining 26 have real external callers (1–8 each) and stay `pub`.
- `types.rs`: every `pub` item is either (a) directly constructed as a
  struct literal by production code outside `ninox-core` (session spawn
  paths in `ninox-ui-shared/src/{cli,spawn_util,restart}.rs`,
  `ninox-server/src/routes/sessions.rs`, `ninox-gui/src/app.rs`), or
  (b) a field-accessed return type of an already-`pub` `Store` method
  (e.g. `OrchestratorPlan`'s fields are read directly by
  `ninox-gui/src/components/plan_panel.rs:132` and
  `ninox-ui-shared/src/cli.rs:1084` off `get_orchestrator_plan`'s return
  value). **No `types.rs` item has zero external reach** — nothing there
  is a safe `pub(crate)` candidate without first narrowing the `Store`
  methods that expose it, which is out of scope for a zero-behavior-change
  pass.
- `Session` struct split (lean core + per-kind extension data): **not
  attempted in this PR.** Production code outside `ninox-core` builds
  full `Session { .. }` literals directly in no fewer than ~15 call sites
  across `ninox-gui`, `ninox-ui-shared`, and `ninox-server` (spawn
  completion, restart/resume, test fixtures). Splitting the struct would
  require either (a) a builder/constructor API replacing every one of
  those literals, or (b) per-kind extension types threaded through all of
  them — real design work and a much larger, riskier diff than a visibility
  pass. Flagged via `ninox request-work` (Step 6) as its own follow-on,
  consistent with the spec's non-mandate ("the spec describes the target
  shape but doesn't mandate doing the full split in one shot").

## Global Constraints

- Behavior-preserving only: no caller's observable behavior changes, only
  how directly it's allowed to reach in.
- Do not touch the six Port-1 fan-out call sites (`lifecycle/reactions.rs`,
  `lifecycle/poller.rs`, `lifecycle/statusline.rs`, `worker_status.rs`,
  `fleet/snapshot.rs`, `events.rs`) beyond what's already in them.
- Full workspace build (`cargo build --workspace`), `cargo test --workspace`,
  `cargo clippy --workspace --all-targets` must all pass before opening
  the PR.

## Review Focus

- A `pub(crate)` change on a method that's only called via a trait object
  or re-export I didn't grep for (false "zero external callers") — build
  the full workspace, not just `ninox-core`, to catch this as a compile
  error.
- A method called only from `#[cfg(test)]` code in another crate (would
  still count as a real external caller and must stay `pub`) — re-check
  each of the 11 candidates' call-site list for test-only usage in other
  crates before flipping visibility (none found, but worth the explicit
  check since cross-crate test fixtures are common in this repo).
- `ninox-server` specifically, since it's easy to forget as a consumer —
  included in both the grep investigation and the build/test matrix.
- `ninox-ptyd`, confirmed to have zero `ninox_core::` imports — still
  built as part of the workspace check so the "every crate still builds"
  claim is actually verified, not assumed.
- Clippy warnings newly introduced by `pub(crate)` (e.g. "function is
  never used" if a grep miss made something truly dead) — run clippy
  across the workspace, not just `ninox-core`, before opening the PR.

---

### Task 1: Tighten `Store`'s internal-only methods to `pub(crate)`

**Files:**
- Modify: `crates/ninox-core/src/store.rs` (11 `pub fn` → `pub(crate) fn`,
  listed above)

**Interfaces:** No signature changes, only the `pub` → `pub(crate)` keyword
on each of the 11 methods. No consumer of this task's output needs new
names.

- [ ] Change each of the 11 methods' visibility from `pub fn` to
      `pub(crate) fn` in place.
- [ ] Run `cargo build -p ninox-core` — must still succeed (internal
      callers unaffected by `pub(crate)`).
- [ ] Run `cargo build --workspace` — must still succeed. Any failure here
      means a grep miss; add the missed external caller back to the
      investigation findings above and revert that one method to `pub`
      (don't guess a fix — the grep was wrong, not the code).
- [ ] Run `cargo test --workspace` — all existing tests pass unchanged.
- [ ] Run `cargo clippy --workspace --all-targets` — clean.
- [ ] Commit: `refactor(core): tighten Session/Store visibility to catch missed call sites at compile time`

### Task 2: Flag the `Session` struct split as follow-on work

**Files:** none (CLI action only)

- [ ] Run `ninox request-work` describing the deferred full `Session`
      struct split (lean core + per-kind extension data), citing the ~15
      external struct-literal construction sites found in the
      investigation as the reason it didn't fit in this visibility-only
      PR.

### Task 3: Open the PR

- [ ] Push `v1-hexagonal-ports-port2` to `origin`.
- [ ] Open PR against `v1` per the task's PR-format instructions (Why/
      Summary/Test plan, citing `c151750`/`f63f7c8`, no Linear refs, no
      "Generated with Claude Code" line, real test plan naming the build/
      test/clippy commands run). Leave open for review — do not self-merge.
