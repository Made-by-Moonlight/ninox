# Orchestrator Identity Restart & Re-registration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a legitimate orchestrator whose pane/process was restarted by
`relaunch_in_place` clear its stale `OrchestratorRuntimeIdentity` row once
the old root process is confirmed dead, so the existing lazy
self-registration path re-fires instead of failing the identity check
forever.

**Architecture:** One new, narrowly-scoped store method
(`delete_dead_orchestrator_runtime`, TOCTOU-guarded on the exact pid) plus a
four-step check/poll/clear sequence inserted into `relaunch_in_place` itself,
gated on `req.is_orchestrator`. No new eager-write path; no new command or
capability.

**Tech Stack:** Rust, rusqlite (`ninox-core::store`), tokio (bounded async
poll loop), existing `lifecycle::probe::is_pid_alive`.

**Spec:** `docs/superpowers/specs/2026-10-22-orchestrator-identity-restart-design.md`

## Global Constraints

- `delete_dead_orchestrator_runtime(orchestrator_id: &str, expected_root_pid: u32) -> Result<bool>` —
  deletes iff a row exists AND its `root_pid` exactly equals
  `expected_root_pid`; never a wildcard delete; returns whether a row was
  actually cleared.
- The clear/poll sequence lives **only** inside `relaunch_in_place`, gated
  on `req.is_orchestrator` — never exposed as a `ninox` subcommand, never
  added to `capabilities::REGISTRY`, never invoked from
  `authorize_orchestrator`/`attest_sender` on a mere mismatch.
- `register_orchestrator_runtime` stays production-uncalled (no wiring
  changes). `register_migrated_orchestrator_runtime`'s
  `ON CONFLICT DO NOTHING` semantics (PR #200) are untouched.
- Liveness is confirmed via `lifecycle::probe::is_pid_alive`, polled with a
  bounded retry mirroring `tmux.rs::wait_for_input_prompt`'s shape: a loop
  with a fixed poll delay against a deadline, returning a bool. Poll delay:
  **200ms**; timeout: **3s** (see rationale below). If the wait times out,
  do nothing — the row stays stale and the relaunch proceeds anyway
  (fail-closed).
- Store access inside `relaunch_in_place` goes through `engine.store`
  (`Arc<Store>`, already a field on `Engine`) — no new parameter needed on
  `relaunch_in_place`'s signature.

### Open-question resolutions

1. **Bounded-wait duration:** `tmux.rs::wait_for_input_prompt` polls every
   `PROMPT_POLL_DELAY_MS` (500ms) with a caller-supplied timeout (`fleet.rs`
   uses `DEFAULT_PROMPT_TIMEOUT = 90s` for a harness to draw its prompt — a
   slow, user-facing event). Confirming process death is a much faster,
   kernel-level event (`kill_session`'s `SIGTERM`/`SIGKILL` plus the OS
   reaping the zombie) — not comparable to waiting on an agent harness to
   boot and render. 200ms poll / 3s total deadline (15 attempts) gives
   `kill_session` generous room to finish reaping on a loaded machine while
   keeping a failed-wait case from stalling every relaunch for 90s. Documented
   here and in the code comment at the call site; bump if CI/real restarts
   show it's too tight.
2. **Messaging for `ninox restart` vs `ninox fleet restore`:** no new
   user-facing messaging in either path. Both already log via `tracing`
   (`restart.rs`/`fleet.rs` use `tracing::warn!` for soft failures the user
   never sees synchronously); add a `tracing::info!`/`tracing::debug!` line
   in `relaunch_in_place` itself (shared by both) when a row is actually
   cleared, and a `tracing::debug!` when the wait times out. This is
   consistent with how `relaunch_in_place`/`ensure_session_workspace`
   already log their own soft failures inline rather than each of the four
   callers adding bespoke messaging — keeping the "one shared mechanism, no
   per-caller duplication" goal intact. No new CLI output field.
3. **ptyd liveness signal:** resolved, not left open. Both backends'
   `current_pane_identity` resolve `PaneIdentity.pane_pid` (→ persisted
   `root_pid`) to a real, same-machine OS pid: `tmux_backend` delegates to
   `tmux.rs::current_private_pane_identity` (a real tmux pane's root pid),
   and `ptyd_backend::identity_for` sets `pane_pid: info.pid` from
   `PaneInfo` — the ptyd host's own record of that pane's root process pid
   on the same machine the host runs on (confirmed via `reaches_before`
   process-ancestry walk, same as tmux). Since this spec is explicitly
   same-machine only (Non-goals), `is_pid_alive` (a `kill(pid, 0)` probe)
   is a valid liveness check against `root_pid` for both backends
   identically — no backend-specific branch needed.

## Review Focus

- A caller presenting a live, mismatched `PaneIdentity` *without* going
  through `relaunch_in_place` must never trigger a clear — pin this with a
  regression test on `authorize_orchestrator`/`attest_sender` directly.
- `delete_dead_orchestrator_runtime` must be a true no-op (not just
  "returns false" but literally leaves the row's bytes unchanged) when
  `expected_root_pid` doesn't match — test by re-reading the row after.
- A non-orchestrator (`req.is_orchestrator == false`) relaunch must never
  touch `orchestrator_runtimes` at all — worker restarts have no equivalent
  persisted row and must not even attempt the read.
- An orchestrator with *no* persisted identity yet (brand new, never
  migrated) hitting `relaunch_in_place` must take the "nothing to clear"
  branch cleanly and still let ordinary first-use migration fire afterward
  — not error, not skip the relaunch.
- The liveness-wait timeout path must leave the row **exactly** as it was
  (not partially cleared, not retried past the deadline) — test with a
  still-alive pid (e.g. the test process's own pid) and assert the row is
  byte-identical after.

---

## Task 1: `delete_dead_orchestrator_runtime` store method

**Files:**
- Modify: `crates/ninox-core/src/store.rs` (new method near
  `register_migrated_orchestrator_runtime` at line ~568)
- Test: same file's `#[cfg(test)] mod tests`

**Interfaces:**
- Produces: `Store::delete_dead_orchestrator_runtime(&self, orchestrator_id: &str, expected_root_pid: u32) -> Result<bool>`

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn delete_dead_orchestrator_runtime_clears_matching_row() {
    let store = test_store();
    store.upsert_orchestrator(&Orchestrator { id: "orch".into(), name: "orch".into(), created_at: 0 }).unwrap();
    let runtime = OrchestratorRuntimeIdentity {
        orchestrator_id: "orch".into(), runtime_id: "r1".into(), server_epoch: "1:1".into(),
        physical_tmux_name: "orch".into(), pane_id: "%1".into(), root_pid: 100,
        root_created_at: 0, registered_at: 0,
    };
    store.register_orchestrator_runtime(&runtime).unwrap();

    assert!(store.delete_dead_orchestrator_runtime("orch", 100).unwrap());
    assert!(store.orchestrator_runtime_identity("orch").unwrap().is_none());
}

#[test]
fn delete_dead_orchestrator_runtime_is_noop_on_pid_mismatch() {
    let store = test_store();
    store.upsert_orchestrator(&Orchestrator { id: "orch".into(), name: "orch".into(), created_at: 0 }).unwrap();
    let runtime = OrchestratorRuntimeIdentity {
        orchestrator_id: "orch".into(), runtime_id: "r1".into(), server_epoch: "1:1".into(),
        physical_tmux_name: "orch".into(), pane_id: "%1".into(), root_pid: 100,
        root_created_at: 0, registered_at: 0,
    };
    store.register_orchestrator_runtime(&runtime).unwrap();

    assert!(!store.delete_dead_orchestrator_runtime("orch", 999).unwrap());
    assert_eq!(store.orchestrator_runtime_identity("orch").unwrap().unwrap(), runtime);
}

#[test]
fn delete_dead_orchestrator_runtime_is_noop_when_no_row_exists() {
    let store = test_store();
    assert!(!store.delete_dead_orchestrator_runtime("ghost", 100).unwrap());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox-core delete_dead_orchestrator_runtime`
Expected: FAIL with "no method named `delete_dead_orchestrator_runtime`"

- [ ] **Step 3: Implement `delete_dead_orchestrator_runtime` in `store.rs`**

Doc comment per the spec's sketch (the method's own rustdoc, not a
restated inline comment). SQL: `DELETE FROM orchestrator_runtimes WHERE
orchestrator_id=?1 AND root_pid=?2`, return `Ok(changed == 1)`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ninox-core delete_dead_orchestrator_runtime`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-core/src/store.rs
git commit -m "feat(store): add delete_dead_orchestrator_runtime, TOCTOU-guarded on root_pid"
```

## Task 2: wire the clear sequence into `relaunch_in_place`

**Files:**
- Modify: `crates/ninox-ui-shared/src/spawn_util.rs:227` (`relaunch_in_place`)
- Test: same file's `#[cfg(test)] mod tests`, plus
  `crates/ninox-core/src/orchestrator_auth.rs`'s test module for the
  regression-pin tests (Review Focus items 1 and 2 don't need
  `relaunch_in_place` at all — they test `orchestrator_auth`/`store`
  directly, which is cheaper and more direct than spinning up real tmux
  sessions).

**Interfaces:**
- Consumes: `Task 1`'s `store.delete_dead_orchestrator_runtime(id, pid) -> Result<bool>`;
  `ninox_core::lifecycle::probe::is_pid_alive(pid: u32) -> bool`;
  `engine.store` (`Arc<Store>`, already public on `Engine`);
  `store.orchestrator_runtime_identity(id) -> Result<Option<OrchestratorRuntimeIdentity>>`
  (existing).
- Produces: no new public interface — `relaunch_in_place`'s signature is
  unchanged (`engine: Arc<Engine>, req: RelaunchRequest, config: &AppConfig, stream_pty: bool`).

- [ ] **Step 1: Write the failing tests in `orchestrator_auth.rs`** (pin: no route to clearing state)

```rust
#[test]
fn authorize_orchestrator_mismatch_never_clears_the_persisted_row() {
    let store = open_store();
    register_orchestrator(&store, "orch-1", &pane("orch-1"));
    let before = store.orchestrator_runtime_identity("orch-1").unwrap().unwrap();

    // Caller's live pane doesn't match the persisted row at all.
    let mismatched = pane("someone-else");
    let result = authorize_orchestrator(&store, Some("orch-1"), Some("orchestrator"), Some(&mismatched));

    assert!(result.is_err());
    assert_eq!(store.orchestrator_runtime_identity("orch-1").unwrap().unwrap(), before);
}

#[test]
fn attest_sender_mismatch_never_clears_the_persisted_row() {
    let store = open_store();
    register_orchestrator(&store, "orch-1", &pane("orch-1"));
    store.upsert_session(&session_with("worker-a", Some("orch-1"))).unwrap();
    let before = store.orchestrator_runtime_identity("orch-1").unwrap().unwrap();

    let a = attest_sender(&store, Some("orch-1"), Some(&pane("someone-else")), "worker-a");

    assert!(!a.verified);
    assert_eq!(store.orchestrator_runtime_identity("orch-1").unwrap().unwrap(), before);
}
```

(These pass against today's code unchanged — they're a regression pin, not
a TDD-red step; run them to confirm green both before and after Task 2's
implementation, since the thing being guarded against is a regression
*introduced* by this task, not a bug it fixes.)

- [ ] **Step 2: Write the failing tests in `spawn_util.rs`** (the three
  liveness-sequence behaviors, exercised directly against a helper rather
  than a full tmux `relaunch_in_place` run — see Step 3 for the helper's
  signature)

```rust
#[tokio::test]
async fn clears_identity_when_old_root_pid_is_confirmed_dead() {
    let store = test_store_with_orchestrator("orch-1");
    // A pid that is certainly dead: spawn a short-lived child and wait on it.
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let dead_pid = child.id();
    child.wait().unwrap();
    store.register_orchestrator_runtime(&runtime_for("orch-1", dead_pid)).unwrap();

    maybe_clear_dead_orchestrator_identity(&store, "orch-1").await;

    assert!(store.orchestrator_runtime_identity("orch-1").unwrap().is_none());
}

#[tokio::test]
async fn leaves_identity_stale_when_old_root_pid_is_still_alive() {
    let store = test_store_with_orchestrator("orch-1");
    let live_pid = std::process::id(); // this test process: definitely alive
    let before = runtime_for("orch-1", live_pid);
    store.register_orchestrator_runtime(&before).unwrap();

    maybe_clear_dead_orchestrator_identity(&store, "orch-1").await;

    assert_eq!(store.orchestrator_runtime_identity("orch-1").unwrap().unwrap(), before);
}

#[tokio::test]
async fn no_persisted_identity_is_a_clean_noop() {
    let store = test_store_with_orchestrator("orch-2"); // never registered a runtime row
    maybe_clear_dead_orchestrator_identity(&store, "orch-2").await; // must not panic/error
    assert!(store.orchestrator_runtime_identity("orch-2").unwrap().is_none());
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p ninox-ui-shared maybe_clear_dead_orchestrator_identity`
Expected: FAIL with "cannot find function `maybe_clear_dead_orchestrator_identity`"
(and the two `orchestrator_auth` pin tests already pass — confirm with
`cargo test -p ninox-core never_clears_the_persisted_row`, expected PASS,
proving the guard holds before this task's code exists at all.)

- [ ] **Step 4: Implement `maybe_clear_dead_orchestrator_identity` and wire it into `relaunch_in_place`**

```rust
const DEAD_PID_POLL: Duration = Duration::from_millis(200);
const DEAD_PID_TIMEOUT: Duration = Duration::from_secs(3);

/// Orchestrator-only: if `orchestrator_id` has a persisted runtime identity
/// whose root pid is confirmed dead within a short bounded wait, clear it so
/// the next authorized action re-migrates a fresh identity. A no-op when
/// there is no persisted row, or when the wait times out without confirming
/// death (fail-closed — see the design spec's "Interim behavior").
async fn maybe_clear_dead_orchestrator_identity(store: &ninox_core::store::Store, orchestrator_id: &str) {
    let Ok(Some(persisted)) = store.orchestrator_runtime_identity(orchestrator_id) else { return };
    let deadline = tokio::time::Instant::now() + DEAD_PID_TIMEOUT;
    loop {
        if !ninox_core::lifecycle::probe::is_pid_alive(persisted.root_pid) {
            match store.delete_dead_orchestrator_runtime(orchestrator_id, persisted.root_pid) {
                Ok(true) => tracing::info!(
                    "relaunch {orchestrator_id}: cleared stale runtime identity (old root pid {} confirmed dead)",
                    persisted.root_pid
                ),
                Ok(false) => {} // lost the TOCTOU race to a concurrent re-registration; nothing to do
                Err(e) => tracing::warn!("relaunch {orchestrator_id}: failed to clear stale identity: {e}"),
            }
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::debug!(
                "relaunch {orchestrator_id}: old root pid {} still alive after {DEAD_PID_TIMEOUT:?}; leaving identity as-is",
                persisted.root_pid
            );
            return;
        }
        tokio::time::sleep(DEAD_PID_POLL).await;
    }
}
```

In `relaunch_in_place`, immediately after the existing
`ninox_core::runtime::kill_session(&id).await` call:

```rust
if req.is_orchestrator {
    maybe_clear_dead_orchestrator_identity(&engine.store, &id).await;
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p ninox-ui-shared maybe_clear_dead_orchestrator_identity`
Expected: PASS
Run: `cargo test -p ninox-core never_clears_the_persisted_row`
Expected: PASS (unchanged — proves Task 2 adds no new route to clearing state)

- [ ] **Step 6: Commit**

```bash
git add crates/ninox-ui-shared/src/spawn_util.rs crates/ninox-core/src/orchestrator_auth.rs
git commit -m "feat(orchestrator-auth): re-register runtime identity after a confirmed-dead restart"
```

## Task 3: end-to-end re-registration test + non-orchestrator/no-duplication regression coverage

**Files:**
- Test: `crates/ninox-ui-shared/src/spawn_util.rs` (extend the `mod tests`
  from Task 2)

**Interfaces:**
- Consumes: `maybe_clear_dead_orchestrator_identity` (Task 2),
  `orchestrator_auth::persisted_or_migrated_identity`/`authorize_orchestrator`
  (existing, `pub(crate)` or `pub` as already declared in `ninox-core`).

- [ ] **Step 1: Write the failing end-to-end test**

```rust
#[test]
fn legitimate_restart_re_registers_after_confirmed_dead_root_pid() {
    let store = test_store_with_orchestrator("orch-1");
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let dead_pid = child.id();
    child.wait().unwrap();
    store.register_orchestrator_runtime(&runtime_for("orch-1", dead_pid)).unwrap();

    assert!(store.delete_dead_orchestrator_runtime("orch-1", dead_pid).unwrap());

    // The orchestrator's next authorized action, from a new pane whose
    // physical_tmux_name still matches its id and whose pid is the current
    // process, must re-migrate exactly as a brand-new orchestrator's first
    // use does today.
    let new_pane = ninox_core::runtime::PaneIdentity {
        physical_tmux_name: "orch-1".to_string(),
        pane_id: "%new".to_string(),
        pane_pid: std::process::id(),
        pane_created_at: 0,
        server_epoch: "2:1".to_string(),
    };
    let result = ninox_core::orchestrator_auth::authorize_orchestrator(
        &store, Some("orch-1"), Some("orchestrator"), Some(&new_pane),
    );
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(
        store.orchestrator_runtime_identity("orch-1").unwrap().unwrap().pane_id,
        "%new"
    );
}

#[tokio::test]
async fn non_orchestrator_relaunch_never_touches_orchestrator_runtimes() {
    let store = test_store_with_orchestrator("orch-1");
    let runtime = runtime_for("orch-1", std::process::id());
    store.register_orchestrator_runtime(&runtime).unwrap();

    // Simulate the worker path: is_orchestrator is false, so the gate in
    // relaunch_in_place must short-circuit before any read/poll/clear.
    // (Directly assert the helper is never reached for a worker by relying
    // on relaunch_in_place's own `if req.is_orchestrator` gate — covered by
    // inspection in Task 2's wiring; this test instead pins that calling
    // maybe_clear_dead_orchestrator_identity for a *different*, non-existent
    // orchestrator id is the documented no-op, and that orch-1's own row is
    // untouched by anything in this test run.)
    maybe_clear_dead_orchestrator_identity(&store, "some-worker-id").await;

    assert_eq!(store.orchestrator_runtime_identity("orch-1").unwrap().unwrap(), runtime);
}
```

Note on the second test: asserting the `is_orchestrator` gate itself
requires either a real tmux-backed `relaunch_in_place` run (slow, flaky in
CI sandboxes — see existing `#[ignore]`-style patterns for tmux-dependent
tests elsewhere in this crate) or a source read of `relaunch_in_place`
confirming the `if req.is_orchestrator` check wraps the only call site. Do
the source read as part of Step 4 below and note it in the PR body per the
spec's testing section ("confirm... via a regression test or at minimum
explicit verification in your PR body").

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox-ui-shared legitimate_restart_re_registers`
Expected: FAIL (helpers not yet defined — see Step 3)

- [ ] **Step 3: Add the shared test helpers `test_store_with_orchestrator` and `runtime_for`**

```rust
fn test_store_with_orchestrator(id: &str) -> ninox_core::store::Store {
    let store = ninox_core::store::Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap();
    store.upsert_orchestrator(&ninox_core::types::Orchestrator {
        id: id.to_string(), name: id.to_string(), created_at: 0,
    }).unwrap();
    store
}

fn runtime_for(id: &str, root_pid: u32) -> ninox_core::types::OrchestratorRuntimeIdentity {
    ninox_core::types::OrchestratorRuntimeIdentity {
        orchestrator_id: id.to_string(), runtime_id: format!("test:{id}"),
        server_epoch: "1:1".to_string(), physical_tmux_name: id.to_string(),
        pane_id: "%1".to_string(), root_pid, root_created_at: 0, registered_at: 0,
    }
}
```

- [ ] **Step 4: Run tests to verify they pass; read `relaunch_in_place` to confirm the `is_orchestrator` gate**

Run: `cargo test -p ninox-ui-shared legitimate_restart_re_registers non_orchestrator_relaunch`
Expected: PASS
Re-read the edited `relaunch_in_place` body and confirm the
`maybe_clear_dead_orchestrator_identity` call is the only call site and is
wrapped in `if req.is_orchestrator`, and that none of the four callers
(`fleet.rs:511`, `restart.rs:255`, `ninox-gui/src/app.rs:2063`,
`ninox-tui/src/tui/mod.rs:1065`) calls `kill_session`/`launch_interactive_session`
directly instead of going through `relaunch_in_place` — grep confirms this
(see plan's own citation spot-check). Note this confirmation in the PR body.

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-ui-shared/src/spawn_util.rs
git commit -m "test(orchestrator-auth): cover legitimate restart re-registration and non-orchestrator no-op"
```

## Task 4: full workspace verification and PR

**Files:** none (verification only)

- [ ] **Step 1: Full workspace build**

Run: `cargo build --workspace`
Expected: clean build, no errors

- [ ] **Step 2: Full workspace test**

Run: `cargo test --workspace`
Expected: all tests pass, including the pre-existing
`orchestrator_auth.rs`/`store.rs`/PR #200 spawn-auth tests, unchanged

- [ ] **Step 3: Clippy**

Run: `cargo clippy --workspace --all-targets`
Expected: clean (no new warnings)

- [ ] **Step 4: Push branch and open PR**

Base `v1`, no self-merge (trust-boundary change — leave open for review).
