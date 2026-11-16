# Hexagonal Session-Kind Ports — Increment 1 (reactions.rs) Implementation Plan

> **For agentic workers:** Executed natively by the implementing worker in
> this session (no subagent fan-out) — single focused increment, one
> reviewer at the end via `superpowers:requesting-code-review`.

**Goal:** Introduce Port 1's session-kind-behavior trait and migrate
exactly the `lifecycle/reactions.rs` call site to dispatch through it,
without touching the other five call sites, `SessionBackend`, or Port 2.

**Architecture:** A new trait `SessionReactions` lives alongside
`SessionBackend` in `runtime/mod.rs`. Its six methods mirror the six
`reactions.rs` functions that take a `&Session` representing a worker's own
PR lifecycle (CI failing, ready-to-merge, review comments, worker-done,
worker-done-kept-alive, worker-retired). Default method bodies reproduce
today's exact wording, so `TmuxBackend`, `PtydBackend`, and the test-only
`MockBackend` each get a zero-override `impl SessionReactions for X {}`.
`reactions.rs`'s six public functions keep their exact existing signatures
(so `poller.rs`, their only caller, needs zero changes) but their bodies
now resolve a `SessionReactions` implementor via the existing, already-sync
`runtime::configured_backend()` seam and dispatch through the trait.

**Spec deviation (load-bearing — read before executing):** The spec's
premise for this call site does not hold on current `v1`. `reactions.rs`
has no by-kind special-casing today — the evidence commit's "+1" to this
file (`fa23bd5`) was a single test-fixture field addition
(`machine_id: None`), not dispatch logic. The six functions take a
`_session: &Session` parameter that is either unused or used only for
`.id`/`.pr_number` display — there is no kind branch to remove. This
increment therefore introduces the seam (trait + dispatch + default
behavior-preserving impls) without changing any observable output, and the
PR description must say so plainly rather than imply a by-kind match arm
was removed.

**Tech Stack:** Rust (ninox-core crate), existing `async_trait`/`Arc` patterns
already used by `SessionBackend`.

**Spec:** `docs/superpowers/specs/2026-10-21-hexagonal-session-ports-design.md`

## Global Constraints

- Do not modify `SessionBackend` itself (no new supertrait bound, no new
  method on it).
- Do not touch `lifecycle/poller.rs`, `lifecycle/statusline.rs`,
  `worker_status.rs`, `fleet/snapshot.rs`, or `events.rs`.
- Do not touch Port 2 (`store.rs`/`types.rs` visibility/encapsulation).
- `reactions.rs`'s six migrated public function signatures must not change
  (poller.rs calls them synchronously, unchanged).
- Existing `reactions.rs` tests must keep passing with zero modification to
  their assertions.
- No `cargo fmt` wholesale pass (tree is not rustfmt-clean).
- Full workspace build + `cargo test --workspace` + `cargo clippy
  --workspace --all-targets` clean before opening the PR.

## Review Focus

- Any change in output text for the CI-failing/ready-to-merge/review/worker-done/
  worker-done-kept-alive/worker-retired messages, for any existing kind —
  must be byte-for-byte identical (covered by: running the existing
  `reactions.rs` test module unchanged).
- A new trait method silently added to `SessionBackend` instead of the new
  trait (covered by: diff review of `runtime/mod.rs` — `SessionBackend`'s
  own method list must be untouched).
- `reaction_behavior_for` picking `Ptyd` in a test binary where ptyd is
  disallowed, instead of correctly falling back to `Tmux` (covered by: a
  unit test asserting `reaction_behavior_for(Backend::Ptyd)` yields a
  `Tmux`-kind behavior when `ptyd_allowed()` is false, mirroring
  `effective_backend`'s existing contract).
- The new generic-dispatch test accidentally hand-matching on kind instead
  of proving the trait object is opaque (covered by: the stub type's
  overridden methods returning markers unrelated to any real kind name).
- `MockBackend`'s trait impl only compiling under `#[cfg(test)]` and being
  skipped by a non-test build (covered by: `cargo build` — not just
  `cargo test` — as part of verification).

---

## File Structure

- **Modify** `crates/ninox-core/src/runtime/mod.rs` — add `SessionReactions`
  trait (6 default-implemented methods) and
  `pub fn reaction_behavior_for(backend: Backend) -> Arc<dyn SessionReactions>`.
- **Modify** `crates/ninox-core/src/runtime/tmux_backend.rs` — add
  `impl SessionReactions for TmuxBackend {}`.
- **Modify** `crates/ninox-core/src/runtime/ptyd_backend.rs` — add
  `impl SessionReactions for PtydBackend {}`.
- **Modify** `crates/ninox-core/src/runtime/mock.rs` (test-only module) —
  add `impl SessionReactions for MockBackend {}`.
- **Modify** `crates/ninox-core/src/lifecycle/reactions.rs` — six public
  functions become thin dispatchers; add six `pub(crate) *_from` helpers
  taking `&dyn SessionReactions` explicitly (used by both the public
  functions and the new generic-dispatch test); add the generic-dispatch
  test.

## Task 1: `SessionReactions` trait + resolver in `runtime/mod.rs`

**Files:**
- Modify: `crates/ninox-core/src/runtime/mod.rs`

**Interfaces:**
- Produces: `pub trait SessionReactions: Send + Sync` with methods
  `fn ci_failing(&self, session: &Session, ci: &CIStatus, failing_names: &[String]) -> String`,
  `fn ready_to_merge(&self, session: &Session, ci: &CIStatus) -> String`,
  `fn review_comments(&self, session: &Session, comments: &[Comment]) -> String`,
  `fn worker_done(&self, worker: &Session, pr_number: u64) -> String`,
  `fn worker_done_kept_alive(&self, worker: &Session, pr_number: u64) -> String`,
  `fn worker_retired(&self, worker: &Session) -> String`
  — every method has a default body, copied verbatim (same format strings,
  same conditionals) from the current bodies of
  `reactions::format_ci_reaction` / `format_ready_to_merge_reaction` /
  `format_review_reaction` / `format_worker_done_reaction` /
  `format_worker_done_kept_alive_reaction` / `format_worker_retired_reaction`
  respectively (see `crates/ninox-core/src/lifecycle/reactions.rs:1-135` for
  the exact text to preserve, including the `watch_tail`-less wording — the
  watched-PR functions are NOT part of this trait, see Global Constraints).
  Also produces `pub fn reaction_behavior_for(backend: Backend) -> Arc<dyn SessionReactions>`,
  matching on `effective_backend(backend)`: `Backend::Tmux => Arc::new(TmuxBackend)`,
  `Backend::Ptyd => Arc::new(PtydBackend::from_env())`.

- [ ] **Step 1: Add `use crate::types::{CIStatus, Comment, Session};` to `runtime/mod.rs`'s imports**

- [ ] **Step 2: Define the `SessionReactions` trait with its six default-implemented methods, placed directly after the `SessionBackend` trait definition (after line 174)**

Copy each method body from the corresponding function in `reactions.rs`
unchanged (do not alter any format string, pluralization, or ordering).

- [ ] **Step 3: Define `pub fn reaction_behavior_for(backend: Backend) -> Arc<dyn SessionReactions>` near the other free functions at the bottom of the file (after `current_pane_identity`, ~line 448)**

- [ ] **Step 4: Run `cargo build -p ninox-core` and confirm it compiles (trait has no implementors yet, so no call sites exist — this just proves the trait/resolver are well-formed)**

Run: `cargo build -p ninox-core`
Expected: success (unused-trait warning is fine at this point, resolved once implementors land in Task 2)

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-core/src/runtime/mod.rs
git commit -m "feat(runtime): add SessionReactions trait and reaction_behavior_for resolver"
```

## Task 2: Implement `SessionReactions` for every existing `SessionBackend`

**Files:**
- Modify: `crates/ninox-core/src/runtime/tmux_backend.rs`
- Modify: `crates/ninox-core/src/runtime/ptyd_backend.rs`
- Modify: `crates/ninox-core/src/runtime/mock.rs`

**Interfaces:**
- Consumes: `SessionReactions` from Task 1 (`crate::runtime::SessionReactions`, already in scope via `super::SessionReactions` or direct path).
- Produces: zero-override impls proving every existing kind compiles against the new trait.

- [ ] **Step 1: Add `impl SessionReactions for TmuxBackend {}` directly after `impl SessionBackend for TmuxBackend` in `tmux_backend.rs`**

- [ ] **Step 2: Add `impl SessionReactions for PtydBackend {}` directly after `impl SessionBackend for PtydBackend` in `ptyd_backend.rs`**

- [ ] **Step 3: Add `impl SessionReactions for MockBackend {}` in `mock.rs`, inside the existing `#[cfg(test)]`-gated module (no extra cfg attribute needed — the whole module is already test-only)**

- [ ] **Step 4: Run `cargo build --workspace` and `cargo test -p ninox-core --lib runtime::`**

Run: `cargo build --workspace && cargo test -p ninox-core --lib runtime::`
Expected: success, no new warnings about `SessionReactions` being unused (three implementors now exist)

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-core/src/runtime/tmux_backend.rs crates/ninox-core/src/runtime/ptyd_backend.rs crates/ninox-core/src/runtime/mock.rs
git commit -m "feat(runtime): implement SessionReactions for tmux/ptyd/mock backends"
```

## Task 3: Migrate `reactions.rs` to dispatch through the trait

**Files:**
- Modify: `crates/ninox-core/src/lifecycle/reactions.rs`

**Interfaces:**
- Consumes: `crate::runtime::{SessionReactions, Backend, configured_backend, reaction_behavior_for}`.
- Produces: `pub(crate) fn ci_reaction_from(behavior: &dyn SessionReactions, session: &Session, ci: &CIStatus, failing_names: &[String]) -> String`
  and five siblings with the naming pattern `<name>_from` for `ready_to_merge_reaction_from`,
  `review_reaction_from`, `worker_done_reaction_from`, `worker_done_kept_alive_reaction_from`,
  `worker_retired_reaction_from` — each a one-line call into the matching
  trait method. These are consumed by Task 4's test.

- [ ] **Step 1: Add `use crate::runtime::{self, SessionReactions};` to `reactions.rs`'s imports**

- [ ] **Step 2: Replace the body of `format_ci_reaction` with a call to a new `pub(crate) fn ci_reaction_from(behavior: &dyn SessionReactions, session: &Session, ci: &CIStatus, failing_names: &[String]) -> String { behavior.ci_failing(session, ci, failing_names) }`, where `format_ci_reaction` itself becomes:**

```rust
pub fn format_ci_reaction(session: &Session, ci: &CIStatus, failing_names: &[String]) -> String {
    ci_reaction_from(&*runtime::reaction_behavior_for(runtime::configured_backend()), session, ci, failing_names)
}
```

- [ ] **Step 3: Repeat Step 2's pattern for the other five functions** (`format_ready_to_merge_reaction` → `ready_to_merge_reaction_from`, `format_review_reaction` → `review_reaction_from`, `format_worker_done_reaction` → `worker_done_reaction_from`, `format_worker_done_kept_alive_reaction` → `worker_done_kept_alive_reaction_from`, `format_worker_retired_reaction` → `worker_retired_reaction_from`), each delegating to its matching trait method and nothing else. Delete the old inline format-string bodies from `reactions.rs` entirely — they now live only in the trait's default methods (Task 1); do not duplicate them here.

- [ ] **Step 4: Run the existing `reactions.rs` test module unchanged**

Run: `cargo test -p ninox-core --lib lifecycle::reactions::tests`
Expected: all existing tests pass with no assertion changes — this is the behavior-preservation proof.

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-core/src/lifecycle/reactions.rs
git commit -m "refactor(lifecycle): dispatch reactions.rs through SessionReactions"
```

## Task 4: Generic dispatch test (stub kind, no hand-written match arm)

**Files:**
- Modify: `crates/ninox-core/src/lifecycle/reactions.rs` (test module only)

**Interfaces:**
- Consumes: the six `*_from` functions from Task 3, `SessionReactions` from Task 1.

- [ ] **Step 1: Write a failing test in `reactions.rs`'s `#[cfg(test)] mod tests` block:**

```rust
struct StubReactions;
impl SessionReactions for StubReactions {
    fn ci_failing(&self, _: &Session, _: &CIStatus, _: &[String]) -> String { "stub:ci_failing".into() }
    fn ready_to_merge(&self, _: &Session, _: &CIStatus) -> String { "stub:ready_to_merge".into() }
    fn review_comments(&self, _: &Session, _: &[Comment]) -> String { "stub:review_comments".into() }
    fn worker_done(&self, _: &Session, _: u64) -> String { "stub:worker_done".into() }
    fn worker_done_kept_alive(&self, _: &Session, _: u64) -> String { "stub:worker_done_kept_alive".into() }
    fn worker_retired(&self, _: &Session) -> String { "stub:worker_retired".into() }
}

#[test]
fn reactions_dispatch_generically_to_any_session_reactions_impl() {
    let session = mock_session();
    let ci = CIStatus { pr_id: 7, total: 3, failing: 1, passing: 2, pending: 0 };
    let stub = StubReactions;
    assert_eq!(ci_reaction_from(&stub, &session, &ci, &[]), "stub:ci_failing");
    assert_eq!(ready_to_merge_reaction_from(&stub, &session, &ci), "stub:ready_to_merge");
    assert_eq!(review_reaction_from(&stub, &session, &[]), "stub:review_comments");
    assert_eq!(worker_done_reaction_from(&stub, &session, 1), "stub:worker_done");
    assert_eq!(worker_done_kept_alive_reaction_from(&stub, &session, 1), "stub:worker_done_kept_alive");
    assert_eq!(worker_retired_reaction_from(&stub, &session), "stub:worker_retired");
}
```

This is the one generic test standing in for a per-kind match arm: a future
kind only needs its own `impl SessionReactions`, never a change here.

- [ ] **Step 2: Run it to verify it fails before `*_from` exists (skip if Task 3 already landed — then run to verify it passes immediately and note that in the commit message instead)**

- [ ] **Step 3: Run the full test to verify it passes**

Run: `cargo test -p ninox-core --lib lifecycle::reactions::tests`
Expected: PASS, including the new `reactions_dispatch_generically_to_any_session_reactions_impl` test.

- [ ] **Step 4: Commit**

```bash
git add crates/ninox-core/src/lifecycle/reactions.rs
git commit -m "test(lifecycle): prove reactions.rs dispatches generically through SessionReactions"
```

## Task 5: Full verification, follow-on work requests, PR

- [ ] **Step 1:** `cargo build --workspace`
- [ ] **Step 2:** `cargo test --workspace`
- [ ] **Step 3:** `cargo clippy --workspace --all-targets -- -D warnings` (or the project's existing clippy invocation if it differs — check `justfile`/CI config first)
- [ ] **Step 4:** Confirm `git diff v1 --stat` touches only the five files listed in File Structure, plus this plan doc.
- [ ] **Step 5:** File six `ninox request-work` calls (or one combined call) for: migrating `poller.rs`, `statusline.rs`, `worker_status.rs`, `fleet/snapshot.rs`, `events.rs` to `SessionReactions`-style dispatch, and Port 2 encapsulation — each framed as its own small follow-on PR.
- [ ] **Step 6:** Push branch, open PR against `v1` with Why/Summary/Test plan, explaining the trait design and explicitly noting the spec-deviation finding (no real by-kind logic existed yet in `reactions.rs`).
- [ ] **Step 7:** Spawn a reviewer (`superpowers:requesting-code-review` or `code-reviewer:review`) before reporting done.
