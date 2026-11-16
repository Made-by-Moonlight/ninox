# `machine_id` Kind-Data Migration (MLOPS-5317) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `Session::kind_data()` the single source of truth for every
remaining local-vs-remote *decision* site in the workspace, migrating each one
off a direct `session.machine_id` read. `Session::machine_id` itself stays the
one stored/serialized field — this is a dispatch-site migration, not a field
removal.

**Architecture:** `Session::kind_data() -> SessionKindData` (`Local` /
`Remote { machine_id: String }`) already exists
(`crates/ninox-core/src/types.rs`, landed in #236/MLOPS-5307). This plan
migrates the five remaining production sites that branch on `machine_id` to
decide local-vs-remote behavior, each to a `match session.kind_data() { ... }`
(or an equivalent `Option<SessionKindData>` derivation for a helper that
returns an option). No signature of any public, cross-crate API changes
except `runtime_cli::remote_machine_id`'s internal body (its own signature —
`Option<String>` — is unchanged, since callers only need Some/None).

**Tech Stack:** Rust / Cargo workspace.

**Spec:** `docs/superpowers/specs/2026-10-21-hexagonal-session-ports-design.md`
(Port 2), narrowed by
`docs/superpowers/plans/2026-10-27-hexagonal-ports-port2-session-extension-data.md`'s
Task 2 follow-on, which is this ticket (MLOPS-5317).

## Investigation findings (this branch, grep-verified against `origin/v1`)

- `grep -rn '\bmachine_id\b' crates/` returns 229 occurrences across 27 files.
  All but five are either: test-fixture `Session { machine_id: None, .. }` /
  `SessionNodeContent { machine_id: ..., .. }` struct literals, the
  `sessions.machine_id` SQL column in `store.rs`, or unconditional field
  carry-through in `spawn_util.rs` (`machine_id: prior.and_then(|s|
  s.machine_id.clone())` — no Local/Remote branch, so nothing to migrate).
  None of these change.
- Two call sites were already migrated in #236:
  `runtime::reaction_behavior_for_session` and `Poller::poll_pids`'s
  pid-liveness branch, both in `crates/ninox-core/src`. Left untouched here.
- `lifecycle/poller.rs`'s `reconcile_remote_sessions` (`existing.machine_id.as_deref()
  != Some(machine_id)`) is a same-id cross-machine *ownership* comparison
  (which specific remote machine owns this row), not a Local-vs-Remote
  decision — `SessionKindData` has no "which remote machine" equality
  semantics beyond what the raw field already gives it. Confirmed out of
  scope by this investigation; left as a direct field read.
- `runtime::Runtime::for_existing`/`backend_for` take `machine_id:
  Option<&str>` as an explicit parameter, never read a `Session` directly,
  and the doc comment confirms nothing calls them with a `Session` in
  production yet. Left untouched — there is no `Session::machine_id` read to
  migrate here.
- Five production decision/construction sites remain:
  1. `ninox-ui-shared/src/connect.rs:24` (`connect_preflight`) — `if let
     Some(machine_id) = session.machine_id.as_deref()` picks SSH-attach vs.
     local-liveness.
  2. `ninox-ui-shared/src/runtime_cli.rs:142` (`remote_machine_id`) — `.and_then(|s|
     s.machine_id)` collapses "no session" and "local session" into the same
     `None`, consumed by `run_pane`'s `if let Some(machine_id) = ...` at
     line 97 to pick SSH-recursion vs. local ptyd attach.
  3. `ninox-server/src/routes/sessions.rs:96` (`diff_for_session`) — `let
     Some(machine_id) = session.machine_id.clone() else { local diff }`.
  4. `ninox-server/src/routes/sessions.rs:134` (`session_diff`) — `if
     session.machine_id.is_some() { load AppConfig } else { default }`,
     deciding whether remote-machines config is needed at all.
  5. `ninox-tui/src/tui/graph.rs:737` — `SessionNodeContent { machine_id:
     row.session.machine_id.clone(), .. }`, the one production construction
     site that reads `Session::machine_id` directly to populate the fleet
     graph's own (separate, UI-only) `machine_id: Option<String>` field.
     `SessionNodeContent` itself is not `Session` and is not touched — only
     this one read is rerouted through `kind_data()`.
- Existing regression coverage already exercises both branches of every one
  of these five sites (local and remote) and must keep passing unchanged:
  `connect_tests::remote_session_skips_local_liveness_and_errors_clearly_with_no_configured_profile`
  (`connect.rs`), `runtime_cli`'s pane-attach tests, `sessions.rs`'s
  `diff_for_a_remote_session_*` / `diff_for_a_local_session_is_unaffected_*`
  tests, and `graph.rs`'s `session_content(&fleet, ...).machine_id` assertions.

## Global Constraints

- Behavior-preserving only: every existing test passes unchanged; no
  caller's observable behavior changes.
- Do not change `Session`'s JSON wire shape or the `sessions.machine_id` SQL
  column.
- Do not touch the ~224 other `machine_id` occurrences (test fixtures,
  `store.rs` persistence, `spawn_util.rs` construction, `SessionNodeContent`'s
  own field, `reconcile_remote_sessions`'s ownership check) — out of scope.
- `cargo build --workspace`, `cargo test --workspace`, and `cargo clippy
  --workspace --all-targets -- -D warnings` must all pass before opening the
  PR.
- Rebase on `origin/v1` immediately before opening the PR (MLOPS-5318 is
  landing parallel work in `remote.rs`/ptyd-upgrade code — this plan touches
  neither file, so conflict risk is low, but rebase to confirm).

## Review Focus

- A sixth kind-dispatch site this investigation missed, left matching the
  raw `machine_id` field instead of `kind_data()` — re-run `grep -rn
  'machine_id\.is_some\|machine_id\.is_none\|Some(machine_id)\|\.machine_id\.as_deref\|\.machine_id\.clone()\|\.machine_id\.as_ref'
  crates/` during review; every remaining hit should be a test fixture, a
  DB/wire read-or-write, `spawn_util.rs`'s carry-through, or
  `reconcile_remote_sessions`'s ownership compare.
- `runtime_cli::remote_machine_id`'s collapsed "unknown session" vs. "local
  session" semantics (both `None`) silently changing meaning during the
  rewrite — pinned by the existing pane-attach tests, which exercise both a
  real local session and an unknown id.
- `graph.rs`'s `SessionNodeContent { machine_id, .. }` construction producing
  a different value than today's direct clone for either kind — pinned by
  the existing `session_content(&fleet, ...).machine_id` assertions in both
  the local and remote graph tests.
- `sessions.rs`'s `session_diff` config-loading branch flipping which kind
  loads `AppConfig` (a local session accidentally loading config, or a remote
  one skipping it) — pinned by `diff_for_a_local_session_is_unaffected_*`
  (uses a panicking `RemoteDiff` to prove the remote path is never touched)
  and the remote diff tests' machine-profile resolution.
- A clippy `-D warnings` failure from an unused `machine_id` binding after a
  match arm drops the inner value it no longer needs (e.g. `SessionKindData::Remote
  { .. }` where the old code bound `machine_id` but the new arm doesn't use
  it) — caught by the full clippy gate in Task 5, not deferred.

---

### Task 1: Migrate `connect.rs`'s SSH-attach-vs-local-liveness branch

**Files:**
- Modify: `crates/ninox-ui-shared/src/connect.rs:24`

**Interfaces:**
- Consumes: `Session::kind_data() -> SessionKindData` (`ninox_core::types`),
  already public.
- No new interfaces produced.

- [ ] **Step 1: Confirm existing coverage already exercises both branches**

Run: `cargo test -p ninox-ui-shared --lib connect:: 2>&1 | tail -40`
Expected: PASS — `remote_session_skips_local_liveness_and_errors_clearly_with_no_configured_profile`
(remote) and `dead_session_is_marked_terminated`/`dead_resumable_session_is_marked_interrupted_not_terminated`
(local) already cover both kinds. No new test needed; this task is a pure
refactor under existing coverage.

- [ ] **Step 2: Replace the direct field read with `kind_data()` dispatch**

In `connect_preflight`, replace:

```rust
if let Some(machine_id) = session.machine_id.as_deref() {
    return Ok(ConnectPlan::Attach(remote_attach_argv(id, machine_id).await?));
}
```

with a `match session.kind_data()` that calls `remote_attach_argv(id,
&machine_id).await?` in the `SessionKindData::Remote { machine_id }` arm and
falls through to the existing liveness code in the `SessionKindData::Local`
arm (same structure as `reaction_behavior_for_session`'s existing `match`
elsewhere in the codebase — mirror that style). Import `SessionKindData`
from `ninox_core::types`.

- [ ] **Step 3: Run the file's tests to confirm unchanged behavior**

Run: `cargo test -p ninox-ui-shared --lib connect:: 2>&1 | tail -40`
Expected: PASS, identical to Step 1.

- [ ] **Step 4: Commit**

```bash
git add crates/ninox-ui-shared/src/connect.rs
git commit -m "refactor(ui-shared): dispatch connect's remote-attach branch off kind_data()"
```

### Task 2: Migrate `runtime_cli.rs`'s `remote_machine_id` helper

**Files:**
- Modify: `crates/ninox-ui-shared/src/runtime_cli.rs:139-143`

**Interfaces:**
- `remote_machine_id`'s signature (`fn remote_machine_id(db_path: &Path,
  pane_id: &str) -> anyhow::Result<Option<String>>`) is unchanged — only its
  body changes. `run_pane`'s call site at line 97 is untouched.

- [ ] **Step 1: Confirm existing coverage already exercises both branches**

Run: `cargo test -p ninox-ui-shared --lib runtime_cli:: 2>&1 | tail -40`
Expected: PASS. The existing test at `runtime_cli.rs:~208-212` stores a
session with `machine_id = Some("prof1")`; confirm (by reading the test
module, if not obvious from the name) there's also local/unknown-id coverage
through `run_pane` or `remote_machine_id` directly — if genuinely absent, add
one `#[test]` asserting `remote_machine_id` returns `None` for a stored
*local* session (distinct from the existing "unknown id" `None` case), since
that's the exact distinction `kind_data()` must preserve.

- [ ] **Step 2: Rewrite the helper body to derive through `kind_data()`**

Replace:

```rust
Ok(store.get_session(pane_id)?.and_then(|s| s.machine_id))
```

with a version that maps the fetched session through `kind_data()`,
returning `Some(machine_id)` only for `SessionKindData::Remote { machine_id
}` and `None` for both "no such session" and `SessionKindData::Local` (same
external `Option<String>` shape as today). Import `SessionKindData` from
`ninox_core::types`.

- [ ] **Step 3: Run the tests to confirm unchanged behavior**

Run: `cargo test -p ninox-ui-shared --lib runtime_cli:: 2>&1 | tail -40`
Expected: PASS, including any test added in Step 1.

- [ ] **Step 4: Commit**

```bash
git add crates/ninox-ui-shared/src/runtime_cli.rs
git commit -m "refactor(ui-shared): derive remote_machine_id through kind_data()"
```

### Task 3: Migrate `ninox-server/src/routes/sessions.rs`'s two diff-routing branches

**Files:**
- Modify: `crates/ninox-server/src/routes/sessions.rs:96-98` (`diff_for_session`)
- Modify: `crates/ninox-server/src/routes/sessions.rs:134-140` (`session_diff`)

**Interfaces:**
- Both functions' signatures are unchanged.

- [ ] **Step 1: Confirm existing coverage already exercises both branches**

Run: `cargo test -p ninox-server --lib routes::sessions:: 2>&1 | tail -60`
Expected: PASS —
`diff_for_a_remote_session_routes_through_ssh_via_the_matching_machine_profile`,
`diff_for_a_local_session_is_unaffected_and_never_touches_remote_transport`,
`diff_for_a_remote_session_surfaces_a_failure_as_a_specific_error_not_an_empty_diff`,
and `diff_for_a_remote_session_with_no_matching_machine_profile_is_a_clear_error`
already cover `diff_for_session`'s branch; `diff_returns_workspace_diff_including_lockfile_only_changes`
covers `session_diff`'s local (no-config-load) path. No new test needed.

- [ ] **Step 2: Migrate `diff_for_session`'s branch**

Replace:

```rust
let Some(machine_id) = session.machine_id.clone() else {
    return Ok(brain_harvest::compute_diff(std::path::Path::new(workspace)).await);
};
```

with a `match session.kind_data() { SessionKindData::Local => return
Ok(brain_harvest::compute_diff(...).await), SessionKindData::Remote {
machine_id } => machine_id }` (or equivalent `let machine_id = match ... `
binding the rest of the function already expects). Import `SessionKindData`
from `ninox_core::types`.

- [ ] **Step 3: Migrate `session_diff`'s config-loading branch**

Replace:

```rust
let remote_machines = if session.machine_id.is_some() {
    AppConfig::load()...remote_machines
} else {
    RemoteMachinesConfig::default()
};
```

with the equivalent `match session.kind_data() { SessionKindData::Remote {
.. } => AppConfig::load()...remote_machines, SessionKindData::Local =>
RemoteMachinesConfig::default() }`.

- [ ] **Step 4: Run the tests to confirm unchanged behavior**

Run: `cargo test -p ninox-server --lib routes::sessions:: 2>&1 | tail -60`
Expected: PASS, identical to Step 1.

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-server/src/routes/sessions.rs
git commit -m "refactor(server): dispatch session diff routing off kind_data()"
```

### Task 4: Migrate `graph.rs`'s `SessionNodeContent` construction

**Files:**
- Modify: `crates/ninox-tui/src/tui/graph.rs:737`

**Interfaces:**
- `SessionNodeContent::machine_id: Option<String>` is unchanged — only how
  its value is derived at this one construction site changes.

- [ ] **Step 1: Confirm existing coverage already exercises both branches**

Run: `cargo test -p ninox-tui --lib tui::graph:: 2>&1 | tail -60`
Expected: PASS — the tests around `graph.rs:1664-1678` already assert
`session_content(&fleet, "o").machine_id == None` for a local session and
`== Some("prof1".to_string())` after setting `row.session.machine_id =
Some("prof1".into())`. No new test needed.

- [ ] **Step 2: Replace the direct clone with a `kind_data()` derivation**

Replace:

```rust
machine_id: row.session.machine_id.clone(),
```

with a `match row.session.kind_data() { SessionKindData::Remote { machine_id
} => Some(machine_id), SessionKindData::Local => None }`. Import
`SessionKindData` from `ninox_core::types`.

- [ ] **Step 3: Run the tests to confirm unchanged behavior**

Run: `cargo test -p ninox-tui --lib tui::graph:: 2>&1 | tail -60`
Expected: PASS, identical to Step 1.

- [ ] **Step 4: Commit**

```bash
git add crates/ninox-tui/src/tui/graph.rs
git commit -m "refactor(tui): derive the fleet graph's machine_id through kind_data()"
```

### Task 5: Full workspace gate, rebase, and PR

**Files:** none (verification and PR only)

- [ ] **Step 1: Re-run the full decision-site grep to confirm nothing was missed**

Run: `grep -rn 'machine_id\.is_some\|machine_id\.is_none\|Some(machine_id)\|\.machine_id\.as_deref\|\.machine_id\.clone()\|\.machine_id\.as_ref' crates/`
Expected: every remaining hit is a test fixture/assertion,
`store.rs`/`spawn_util.rs`, or `reconcile_remote_sessions`'s ownership
compare — per Review Focus's first item.

- [ ] **Step 2: Full workspace gate**

Run: `cargo build --workspace && cargo test --workspace 2>&1 | tail -80 && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -80`
Expected: all three succeed with no errors or warnings.

- [ ] **Step 3: Rebase on `origin/v1`**

```bash
git fetch origin v1
git rebase origin/v1
```

- [ ] **Step 4: Push and open the PR**

Push the branch and open a PR against `v1` titled exactly
`MLOPS-5317: refactor(core): dispatch local-vs-remote decisions through Session::kind_data()`
(Why/Summary/Test plan body, concise, no Linear-history narrative, no
"Generated with Claude Code" line, a real test plan naming the
build/test/clippy commands run). Leave open for review — do not self-merge.
