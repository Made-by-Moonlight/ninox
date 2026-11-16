# Session-Kind Extension Data (Port 2, first slice) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. This is a scoped, behavior-preserving refactor executed inline in this worker session (no subagent fan-out — small change, one PR).

**Goal:** Introduce `SessionKindData`, a per-kind extension-data enum colocated
with `Session` in `types.rs`, as the typed home for session-kind-specific
data (today just `machine_id`), and migrate the two existing dispatch points
that pattern-match `machine_id` for *kind-behavior* decisions
(`runtime::reaction_behavior_for_session`, `Poller::poll_pids`'s pid-liveness
check) to dispatch off it instead. This is the smallest coherent first slice
of the Session struct's hexagonal Port 2 encapsulation — the full retirement
of the `machine_id: Option<String>` field itself (and its ~180 remaining
read/construction call sites across 7+ crates) is flagged as follow-on work,
not attempted here.

**Architecture:** Add `SessionKindData` (`Local` default / `Remote {
machine_id: String }`) and a `Session::kind_data(&self) -> SessionKindData`
method that *derives* it from the existing `machine_id` field — purely
additive. No field is removed, no DB schema changes, no JSON wire-format
changes. This is a hard constraint, not a simplification: `Session` is
serialized directly as HTTP JSON between a local orchestrator and a remote
machine's own `ninox-server` (`routes::list_sessions -> Json<Vec<Session>>`,
consumed by `Poller::reconcile_remote_sessions` across possibly different
`ninox` versions), and `runtime::remote_backend`'s own tests embed
`"machine_id":null` as a raw JSON string literal — changing the wire shape
here would be an actual behavior change to a cross-process, cross-version
protocol, not a safe refactor. Only the two call sites that branch on
`machine_id` for kind-dispatch purposes move to the new accessor; the ~180
other sites (test-fixture struct literals, `store.rs` persistence, real
construction in `spawn_util.rs`, UI read sites in `ninox-gui`/`ninox-tui`)
are untouched.

**Tech Stack:** Rust / Cargo workspace.

**Spec:** `docs/superpowers/specs/2026-10-21-hexagonal-session-ports-design.md`
(Port 2 section only — Port 1's trait/fan-out work is separate and already
substantially merged: `SessionReactions`, `reaction_behavior_for_session`).

## Investigation findings (current `v1`, grep-verified)

- `grep -rn '\bmachine_id\b'` across the workspace returns ~190 occurrences
  in 27 files. The overwhelming majority are `machine_id: None,` /
  `machine_id: Some(x),` struct-literal boilerplate in per-file test fixture
  helpers (one line each, in `events.rs`, `lifecycle/reactions.rs`,
  `lifecycle/statusline.rs`, `fleet/snapshot.rs`, `worker_status.rs`,
  `identity.rs`, `messaging.rs`, `orchestrator_auth.rs`, `remote_backend.rs`,
  and more) — not kind-dispatch logic. Changing `machine_id`'s type would
  force editing every one of these in the same PR (Rust can't compile a
  partial rename), which is why this slice does **not** change the field's
  type.
- Two places branch on `machine_id` to decide *kind behavior* through Port
  1's `SessionReactions` trait — its own resolution points, so Port 2 only
  needs to change what they key off, not introduce new dispatch:
  1. `runtime::reaction_behavior_for_session` (`crates/ninox-core/src/runtime/mod.rs:386-391`)
     — `match &session.machine_id { Some(_) => RemoteBackend::unresolved(), None => reaction_behavior_for(configured_backend()) }`.
  2. `Poller::poll_pids` (`crates/ninox-core/src/lifecycle/poller.rs:742-745`)
     — `match session.machine_id { Some(_) => reaction_behavior_for_session(&session).pid_is_local(), None => local_pid_is_local }`.
  **Correction (found during final review, not by the original
  investigation):** these are not the *only* `machine_id` branches that pick
  kind-specific behavior — they are only the ones that go through Port 1's
  trait. At least three more decide local-vs-remote behavior directly,
  outside that trait, and were missed by this plan's "exactly two" framing:
  `ninox-ui-shared/src/connect.rs:24` (SSH attach vs. local liveness),
  `ninox-server/src/routes/sessions.rs:96` (remote vs. local diff), and
  `ninox-server/src/routes/sessions.rs:134` (whether to load remote-machines
  config). None of these are migrated in this slice — migrating them means
  crossing into `ninox-ui-shared`/`ninox-server`, which this slice
  deliberately kept out of scope (see Global Constraints) — but the
  follow-on work (Task 2) names them explicitly so they aren't lost.
- `Session` is confirmed serialized as literal wire JSON: `routes::sessions::list_sessions`
  returns `Json<Vec<Session>>` directly (`crates/ninox-server/src/routes/sessions.rs:29`),
  and `Poller::reconcile_remote_sessions`'s doc comment
  (`crates/ninox-core/src/lifecycle/poller.rs:809-810`) confirms this crosses
  the SSH tunnel between a local orchestrator and a remote machine's own
  `ninox-server`, which may run a different `ninox` version. `runtime::remote_backend`'s
  own tests hardcode `"machine_id":null` in a raw JSON string
  (`crates/ninox-core/src/runtime/remote_backend.rs:405,415`), confirming the
  exact on-wire key name is already depended upon by name elsewhere in the
  codebase, not just conceptually.
- Existing regression coverage for both dispatch points already exists and
  must keep passing unchanged: `reaction_behavior_for_session_resolves_by_the_session_s_own_machine_id`
  (`crates/ninox-core/src/runtime/tests.rs`) and
  `poll_pids_skips_the_local_pid_check_for_a_remote_session`
  (`crates/ninox-core/src/lifecycle/poller.rs`).

## Global Constraints

- Behavior-preserving only: every existing test passes unchanged; no
  caller's observable behavior changes.
- Do not change `Session`'s JSON wire shape or the `sessions.machine_id` SQL
  column — `machine_id: Option<String>` stays the one stored/serialized
  field; `SessionKindData` is a derived view over it in this slice.
- Do not touch the ~180 other `machine_id` sites (test fixtures, `store.rs`
  persistence, `spawn_util.rs` construction, `ninox-gui`/`ninox-tui` reads) —
  explicitly out of scope, flagged as follow-on via Task 2.
- Full workspace build (`cargo build --workspace`), `cargo test --workspace`,
  `cargo clippy --workspace --all-targets -- -D warnings` must all pass
  before opening the PR.

## Review Focus

- A third kind-dispatch call site this investigation missed, left matching
  the raw `machine_id` field instead of the new accessor — re-grep
  `session.machine_id`/`\.machine_id` during review; every remaining hit
  should be a test fixture, a DB/wire read-or-write, or a pass-through
  parameter, never a `match .. { Some/None }` deciding kind behavior.
- `SessionKindData::Remote`'s `machine_id` silently diverging from
  `Session::machine_id` after the derive (e.g. a future edit to one without
  the other) — the only defense is keeping `kind_data()` a thin, obviously
  correct one-line derivation rather than duplicated state; pinned by a
  round-trip unit test in Task 1.
- The wire/DB shape of `Session` unintentionally changing as a side effect
  of adding the new type — guarded directly by a test asserting
  `serde_json::to_value(&session)` still has a bare `"machine_id"` key (null
  or string) for both a local and a remote session.
- `SessionKindData` missing `Debug`/`Clone`/`PartialEq`/`Eq` would block the
  follow-on migration's own tests from day one — derive the full set now.
- The follow-on `ninox request-work` (Task 2) landing with enough detail
  (the ~180 call sites, which 7+ crates, why this slice stopped here) that
  whoever picks it up doesn't have to redo this investigation.

---

### Task 1: `SessionKindData` + migrate the two kind-dispatch call sites

**Files:**
- Modify: `crates/ninox-core/src/types.rs` (add `SessionKindData` enum +
  `Session::kind_data()`, plus tests)
- Modify: `crates/ninox-core/src/runtime/mod.rs:386-391`
  (`reaction_behavior_for_session`)
- Modify: `crates/ninox-core/src/lifecycle/poller.rs:742-745` (`poll_pids`)

**Interfaces:**
- Produces: `pub enum SessionKindData { Local, Remote { machine_id: String } }`
  (in `crate::types`, derives `Debug, Clone, PartialEq, Eq, Default` with
  `#[default] Local`) and `impl Session { pub fn kind_data(&self) ->
  SessionKindData }`. No other task in this plan consumes these, but the
  Task 2 follow-on ticket's description should name them exactly so the next
  worker doesn't have to rediscover them.

- [ ] **Step 1: Write the failing tests in `types.rs`**

```rust
#[test]
fn kind_data_is_local_when_machine_id_is_none() {
    let s = base_session(); // existing helper; machine_id: None
    assert_eq!(s.kind_data(), SessionKindData::Local);
}

#[test]
fn kind_data_is_remote_with_machine_id_when_set() {
    let mut s = base_session();
    s.machine_id = Some("m1".into());
    assert_eq!(s.kind_data(), SessionKindData::Remote { machine_id: "m1".into() });
}

#[test]
fn kind_data_does_not_change_sessions_wire_shape() {
    let local = serde_json::to_value(&base_session()).unwrap();
    assert_eq!(local["machine_id"], serde_json::Value::Null);

    let mut remote_session = base_session();
    remote_session.machine_id = Some("m1".into());
    let remote = serde_json::to_value(&remote_session).unwrap();
    assert_eq!(remote["machine_id"], serde_json::json!("m1"));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p ninox-core --lib types:: 2>&1 | tail -40`
Expected: FAIL to compile — `SessionKindData` and `Session::kind_data` not
defined (the third test should already pass once it compiles, since it
doesn't touch the new API — but all three fail together at the compile
step).

- [ ] **Step 3: Implement `SessionKindData` and `Session::kind_data` in `types.rs`**

Add the enum near `Session` (after the `Session` impl block is a reasonable
spot) and the method inside `impl Session`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SessionKindData {
    #[default]
    Local,
    Remote { machine_id: String },
}
```

`kind_data` derives from `self.machine_id` only — one line, `match
&self.machine_id { Some(id) => SessionKindData::Remote { machine_id:
id.clone() }, None => SessionKindData::Local }`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p ninox-core --lib types:: 2>&1 | tail -40`
Expected: PASS, 3/3 (plus all pre-existing `types::tests` still pass).

- [ ] **Step 5: Migrate `reaction_behavior_for_session` to dispatch off `kind_data()`**

Replace the `match &session.machine_id { Some(_) => .., None => .. }` body
with a `match session.kind_data() { SessionKindData::Remote { .. } =>
Arc::new(RemoteBackend::unresolved()), SessionKindData::Local =>
reaction_behavior_for(configured_backend()) }`. Import `SessionKindData`
from `crate::types`.

- [ ] **Step 6: Migrate `Poller::poll_pids`'s pid-liveness branch to `kind_data()`**

Replace `match session.machine_id { Some(_) => .., None => .. }` with
`match session.kind_data() { SessionKindData::Remote { .. } =>
runtime::reaction_behavior_for_session(&session).pid_is_local(),
SessionKindData::Local => local_pid_is_local }`. Import `SessionKindData`
from `crate::types`.

- [ ] **Step 7: Run the full existing regression tests for both migrated sites**

Run: `cargo test -p ninox-core --lib runtime::tests::reaction_behavior_for_session_resolves_by_the_session_s_own_machine_id lifecycle::poller::tests::poll_pids_skips_the_local_pid_check_for_a_remote_session -- --exact 2>&1 | tail -40`
Expected: both PASS, unchanged from before the migration (proves
behavior-preservation).

- [ ] **Step 8: Run the full workspace build, test, and clippy gate**

Run: `cargo build --workspace && cargo test --workspace 2>&1 | tail -60 && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -60`
Expected: all three succeed with no errors or warnings.

- [ ] **Step 9: Commit**

```bash
git add crates/ninox-core/src/types.rs crates/ninox-core/src/runtime/mod.rs crates/ninox-core/src/lifecycle/poller.rs
git commit -m "refactor(core): add SessionKindData extension type, dispatch kind-behavior off it"
```

### Task 2: Flag the full `machine_id` retirement as follow-on work

**Files:** none (CLI action only)

- [ ] Run `ninox request-work` describing the deferred full migration:
      retiring `Session::machine_id: Option<String>` as the stored field in
      favor of `SessionKindData` as the sole source of truth, migrating the
      ~180 remaining read/construction call sites across `ninox-core`,
      `ninox-gui`, `ninox-tui`, `ninox-server`, `ninox-ui-shared`, and
      `ninox-overview` — name the two already-migrated dispatch sites, the
      wire/DB-compatibility constraint this slice preserved (and that any
      future migration must also preserve or do as a deliberate, versioned
      wire-format change), and that the broader "narrow accessor for all of
      `Session`'s ~20 fields" goal from the Port 2 spec is separate, larger,
      and not part of this follow-on.

### Task 3: Open the PR

- [ ] Rebase the branch on `origin/v1`.
- [ ] Push `v1-mlops-5307-hexports-port2` to `origin`.
- [ ] Open a PR against `v1` titled exactly
      `MLOPS-5307: refactor(core): add SessionKindData extension type for session-kind dispatch`
      (Why/Summary/Test plan body, no Linear-history narrative, no
      "Generated with Claude Code" line, a real test plan naming the
      build/test/clippy commands run). Leave open for review — do not
      self-merge.
