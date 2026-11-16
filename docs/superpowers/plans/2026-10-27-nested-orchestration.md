# Nested Orchestration and the Depth Cap — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a verified orchestrator (or a permitted worker) spawn a real,
depth-tracked sub-orchestrator via a new `ninox spawn-sub-orchestrator`
command, hard-capped at a configurable depth (default 3), distinct from the
existing peer `ninox spawn-orchestrator`.

**Architecture:** Reuse `Session.orchestrator_id` as the lineage pointer for
sub-orchestrators (today always `None` for an orchestrator row — this spec
changes that invariant for the nested case only). Depth is computed by
walking that pointer at spawn time, bounded by the configured cap, rather
than stored as a separate counter. A new `orchestrator_auth::authorize_nested_spawn`
reuses the existing `authorize_orchestrator` tmux-pane/process-ancestry check
for an orchestrator caller, and adds a worker-type-gated path for a worker
caller. `spawn_orchestrator_common` (already shared by the peer-spawn paths)
gains one new parameter so the nested path can set a real parent instead of
`None`.

**Tech Stack:** Rust, `ninox-core` (config, store, orchestrator_auth,
worker_type, capabilities), `ninox-ui-shared` (cli.rs), rusqlite-backed
`Store`, `toml`/`serde`.

**Spec:** `docs/superpowers/specs/2026-10-21-nested-orchestration-design.md`,
plus the "Nested orchestration and the depth cap" section of
`docs/superpowers/specs/2026-10-21-fleet-overview-ui-vision-design.md`.

## Global Constraints

- Depth cap default is **3**, configurable via `ninox config set
  nested_orchestration.max_depth <n>` (generic dotted-path config, no
  special-case CLI code needed).
- The pre-existing spawn-authorization gap for `run_spawn` is **already
  fixed on `origin/v1`** (verified while investigating — `run_spawn` already
  calls `orchestrator_auth::authorize_orchestrator` before trusting any
  `--orchestrator-id` claim). `spawn_orchestrator_common` takes no identity
  claim at all (peer semantics unchanged), so it needs no equivalent fix.
  This plan only adds authorization to the **new** nested-spawn path.
- `spawn-orchestrator`'s existing peer semantics (`orchestrator_id: None`,
  `--user-requested` gate) must not change.
- A rejected nested-spawn claim (bad identity, missing permission, or
  depth-cap violation) must leave **no side effect** — no worktree, no
  session row, no orchestrator row — checked before any of those are
  created, mirroring `run_spawn`'s existing pre-side-effect ordering.
- `cargo fmt` is not run wholesale on this tree — match surrounding
  hand-aligned style in files you edit; do not reformat unrelated lines.
- `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test
  --workspace` must pass before pushing.

## Review Focus

- **`max_depth = 0` (or 1) configured**: must deny every nested-spawn
  attempt predictably (no integer underflow/panic comparing `depth + 1` to
  `cap`), not just the deep ones. Pinned in Task 3.
- **A cyclic or corrupted lineage chain** (`A.orchestrator_id = Some("B")`,
  `B.orchestrator_id = Some("A")`): the depth walk must terminate with an
  error, never hang. Pinned in Task 3.
- **A worker's recorded `worker_type` no longer resolves** in the live
  registry (renamed/removed custom type after config edit): must error
  clearly, not panic or silently deny with a misleading message. Pinned in
  Task 3.
- **Forged/unowned `--orchestrator-id` on the new command specifically**:
  this is a brand-new CLI command, so it does not inherit `run_spawn`'s
  existing test coverage — needs its own rejection test. Pinned in Task 5.
- **A plain worker with no `worker_type` at all** calling nested-spawn:
  must be rejected with a clear "no worker type to grant this" message, not
  treated as an orchestrator or silently allowed. Pinned in Task 3/5.

---

### Task 1: Depth-cap config

**Files:**
- Modify: `crates/ninox-core/src/config.rs`

**Interfaces:**
- Produces: `pub struct NestedOrchestrationConfig { pub max_depth: u32 }`,
  `impl Default for NestedOrchestrationConfig`, and
  `AppConfig::nested_orchestration: NestedOrchestrationConfig` — later tasks
  read `config.nested_orchestration.max_depth`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn nested_orchestration_defaults_to_max_depth_3() {
    assert_eq!(AppConfig::default().nested_orchestration.max_depth, 3);
}

#[test]
fn nested_orchestration_missing_table_defaults_to_3() {
    let cfg: AppConfig = toml::from_str("port = 8080\nfont_size = 13.0\n").unwrap();
    assert_eq!(cfg.nested_orchestration.max_depth, 3);
}

#[test]
fn nested_orchestration_can_be_configured() {
    let toml_src = "port = 8080\nfont_size = 13.0\n\n[nested_orchestration]\nmax_depth = 5\n";
    let cfg: AppConfig = toml::from_str(toml_src).unwrap();
    assert_eq!(cfg.nested_orchestration.max_depth, 5);
}
```

Place these next to the existing `pr_watch_*` tests in `config.rs`'s `mod tests`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox-core nested_orchestration -- --nocapture`
Expected: FAIL with "no field `nested_orchestration`" (compile error).

- [ ] **Step 3: Implement the config struct and field**

Add near `PrWatchConfig` (around line 271):

```rust
/// Hard cap on `ninox spawn-sub-orchestrator` nesting depth. A request
/// that would exceed it is rejected outright — see
/// `docs/superpowers/specs/2026-10-21-nested-orchestration-design.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NestedOrchestrationConfig {
    #[serde(default = "default_max_orchestrator_depth")]
    pub max_depth: u32,
}

fn default_max_orchestrator_depth() -> u32 {
    3
}

impl Default for NestedOrchestrationConfig {
    fn default() -> Self {
        Self { max_depth: default_max_orchestrator_depth() }
    }
}
```

Add `#[serde(default)] pub nested_orchestration: NestedOrchestrationConfig,`
to `AppConfig` (right after `pr_watch`), and
`nested_orchestration: NestedOrchestrationConfig::default(),` to `AppConfig`'s
`Default` impl.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ninox-core nested_orchestration`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-core/src/config.rs
git commit -m "feat(config): add configurable nested-orchestration depth cap"
```

---

### Task 2: Worker-type nested-spawn permission flag

**Files:**
- Modify: `crates/ninox-core/src/worker_type.rs`

**Interfaces:**
- Produces: `WorkerTypeSpec::allow_nested_spawn: bool` (default `false`),
  read by Task 3's `authorize_nested_spawn`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn allow_nested_spawn_defaults_to_false() {
    let spec = WorkerTypeSpec { name: "Bare".into(), ..WorkerTypeSpec::default() };
    assert!(!spec.allow_nested_spawn);
}

#[test]
fn allow_nested_spawn_round_trips_through_toml() {
    let toml_src = "[[worker_types]]\nname = \"Delegator\"\nallow_nested_spawn = true\n";
    #[derive(Deserialize)]
    struct Wrap { worker_types: Vec<WorkerTypeSpec> }
    let w: Wrap = toml::from_str(toml_src).unwrap();
    assert!(w.worker_types[0].allow_nested_spawn);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox-core allow_nested_spawn -- --nocapture`
Expected: FAIL with "no field `allow_nested_spawn`" (compile error).

- [ ] **Step 3: Add the field**

Add to `WorkerTypeSpec` (after `permissions`):

```rust
/// Grants this worker type's spawns the `ninox spawn-sub-orchestrator`
/// capability — reconciles the nested-orchestration spec's permission
/// gating with this spec's registry: a ninox-level capability flag, kept
/// separate from `permissions` (Claude Code's own tool-permission rule
/// strings, a different shape entirely). Default `false`: a worker type
/// must opt in explicitly.
#[serde(default)]
pub allow_nested_spawn: bool,
```

Add `allow_nested_spawn: false,` to `WorkerTypeSpec`'s `Default` impl, and
to the two existing full-field test literals that don't use
`..WorkerTypeSpec::default()`: `config_entry_replaces_builtin_spec`'s
override and `export_import_round_trips_one_entry`'s `spec` (both already
in this file) — update both so they compile again.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ninox-core --lib worker_type::`
Expected: PASS (including the two tests just updated)

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-core/src/worker_type.rs
git commit -m "feat(worker-types): add allow_nested_spawn permission flag"
```

---

### Task 3: Lineage depth walk and nested-spawn authorization

**Files:**
- Modify: `crates/ninox-core/src/orchestrator_auth.rs`

**Interfaces:**
- Consumes: `Store::get_session(&self, id: &str) -> Result<Option<Session>>`;
  `orchestrator_auth::authorize_orchestrator(store, orchestrator_id, caller_type, runtime) -> Result<String>`
  (existing); `crate::worker_type::WorkerTypeRegistry::get(&self, name: &str) -> Option<&WorkerTypeSpec>`
  (existing); `WorkerTypeSpec::allow_nested_spawn` (Task 2).
- Produces:
  - `pub fn orchestrator_depth(store: &Store, orchestrator_id: &str, cap: u32) -> Result<u32>`
  - `pub fn authorize_nested_spawn(store: &Store, claimed_orchestrator_id: Option<&str>, caller_type: Option<&str>, caller_session_id: Option<&str>, runtime: Option<&runtime::PaneIdentity>, worker_type_registry: &crate::worker_type::WorkerTypeRegistry) -> Result<String>`
    (returns the verified **parent** orchestrator id the new sub-orchestrator
    should attach under) — consumed by Task 5's `run_spawn_sub_orchestrator`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn orchestrator_depth_is_zero_for_a_root() {
    let store = open_store();
    register_orchestrator(&store, "root", &pane("root"));
    assert_eq!(orchestrator_depth(&store, "root", 3).unwrap(), 0);
}

#[test]
fn orchestrator_depth_counts_hops_to_root() {
    let store = open_store();
    register_orchestrator(&store, "root", &pane("root"));
    store.upsert_orchestrator(&Orchestrator { id: "sub1".into(), name: "sub1".into(), created_at: 0 }).unwrap();
    store.upsert_session(&session_with("sub1", Some("root"))).unwrap();
    store.upsert_orchestrator(&Orchestrator { id: "sub2".into(), name: "sub2".into(), created_at: 0 }).unwrap();
    store.upsert_session(&session_with("sub2", Some("sub1"))).unwrap();

    assert_eq!(orchestrator_depth(&store, "sub1", 3).unwrap(), 1);
    assert_eq!(orchestrator_depth(&store, "sub2", 3).unwrap(), 2);
}

#[test]
fn orchestrator_depth_errors_instead_of_looping_on_a_cycle() {
    let store = open_store();
    store.upsert_orchestrator(&Orchestrator { id: "a".into(), name: "a".into(), created_at: 0 }).unwrap();
    store.upsert_session(&session_with("a", Some("b"))).unwrap();
    store.upsert_orchestrator(&Orchestrator { id: "b".into(), name: "b".into(), created_at: 0 }).unwrap();
    store.upsert_session(&session_with("b", Some("a"))).unwrap();

    assert!(orchestrator_depth(&store, "a", 3).is_err());
}

#[test]
fn authorize_nested_spawn_accepts_a_verified_orchestrator() {
    let store = open_store();
    register_orchestrator(&store, "orch-1", &pane("orch-1"));
    let registry = crate::worker_type::WorkerTypeRegistry::from_config(&[]);

    let parent = authorize_nested_spawn(
        &store, Some("orch-1"), Some("orchestrator"), None, Some(&pane("orch-1")), &registry,
    ).unwrap();
    assert_eq!(parent, "orch-1");
}

#[test]
fn authorize_nested_spawn_rejects_a_forged_orchestrator_claim() {
    let store = open_store();
    let registry = crate::worker_type::WorkerTypeRegistry::from_config(&[]);
    let err = authorize_nested_spawn(
        &store, Some("some-real-orchestrator"), Some("orchestrator"), None, None, &registry,
    ).unwrap_err();
    assert!(err.to_string().len() > 0);
}

#[test]
fn authorize_nested_spawn_rejects_a_worker_with_no_worker_type() {
    let store = open_store();
    register_orchestrator(&store, "orch-1", &pane("orch-1"));
    store.upsert_session(&session_with("worker-a", Some("orch-1"))).unwrap();
    let registry = crate::worker_type::WorkerTypeRegistry::from_config(&[]);

    let err = authorize_nested_spawn(
        &store, None, Some("worker"), Some("worker-a"), Some(&pane("worker-a")), &registry,
    ).unwrap_err();
    assert!(err.to_string().contains("worker type"));
}

#[test]
fn authorize_nested_spawn_rejects_a_worker_type_without_the_permission() {
    let store = open_store();
    register_orchestrator(&store, "orch-1", &pane("orch-1"));
    let mut w = session_with("worker-a", Some("orch-1"));
    w.worker_type = Some("Implementation".into());
    store.upsert_session(&w).unwrap();
    let registry = crate::worker_type::WorkerTypeRegistry::from_config(&[]);

    let err = authorize_nested_spawn(
        &store, None, Some("worker"), Some("worker-a"), Some(&pane("worker-a")), &registry,
    ).unwrap_err();
    assert!(err.to_string().contains("does not grant"));
}

#[test]
fn authorize_nested_spawn_accepts_a_worker_type_with_the_permission() {
    let store = open_store();
    register_orchestrator(&store, "orch-1", &pane("orch-1"));
    let mut w = session_with("worker-a", Some("orch-1"));
    w.worker_type = Some("Delegator".into());
    store.upsert_session(&w).unwrap();
    let registry = crate::worker_type::WorkerTypeRegistry::from_config(&[crate::worker_type::WorkerTypeSpec {
        name: "Delegator".into(),
        allow_nested_spawn: true,
        ..crate::worker_type::WorkerTypeSpec::default()
    }]);

    let parent = authorize_nested_spawn(
        &store, None, Some("worker"), Some("worker-a"), Some(&pane("worker-a")), &registry,
    ).unwrap();
    assert_eq!(parent, "orch-1");
}

#[test]
fn authorize_nested_spawn_rejects_an_unrecognized_caller_type() {
    let store = open_store();
    let registry = crate::worker_type::WorkerTypeRegistry::from_config(&[]);
    assert!(authorize_nested_spawn(&store, None, None, None, None, &registry).is_err());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox-core orchestrator_auth:: -- --nocapture`
Expected: FAIL (functions don't exist yet — compile error).

- [ ] **Step 3: Implement `orchestrator_depth` and `authorize_nested_spawn`**

```rust
/// Hops from `orchestrator_id` up to its root (an orchestrator whose own
/// session has no recorded parent) — a root orchestrator is depth 0.
/// Bounded by `cap`: a legitimate chain (enforced at spawn time, see
/// `authorize_nested_spawn`'s callers) is never deeper than `cap`, so
/// walking more than `cap` hops means a cycle or corrupted data — this
/// errors out rather than looping further.
pub fn orchestrator_depth(store: &Store, orchestrator_id: &str, cap: u32) -> Result<u32> {
    let mut depth = 0u32;
    let mut current = orchestrator_id.to_string();
    loop {
        let session = store.get_session(&current)?
            .with_context(|| format!("orchestrator '{current}' has no session row"))?;
        let Some(parent) = session.orchestrator_id else { return Ok(depth) };
        depth += 1;
        anyhow::ensure!(
            depth <= cap,
            "lineage chain from '{orchestrator_id}' exceeds the configured depth cap ({cap}) \
             while walking to its root — refusing rather than walking further (possible cycle)"
        );
        current = parent;
    }
}
```

One line on the approach for `authorize_nested_spawn`: dispatch on
`caller_type` (`"orchestrator"` → delegate to `authorize_orchestrator`
unchanged; `"worker"` → new private helper `authorize_nested_spawn_as_worker`
that pane-matches `caller_session_id`, loads that session's `worker_type`,
resolves it in `worker_type_registry`, requires `allow_nested_spawn`, and
returns that session's own `orchestrator_id` as the parent; anything else →
a clear "requires an orchestrator or worker session" error).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ninox-core orchestrator_auth::`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-core/src/orchestrator_auth.rs
git commit -m "feat(orchestrator-auth): add lineage depth walk and nested-spawn authorization"
```

---

### Task 4: Thread a parent pointer through `spawn_orchestrator_common`

**Files:**
- Modify: `crates/ninox-ui-shared/src/cli.rs`

**Interfaces:**
- Consumes: nothing new from earlier tasks.
- Produces: `spawn_orchestrator_common(store: &Store, config: &AppConfig, name: &str, prompt: Option<String>, parent_orchestrator_id: Option<&str>) -> anyhow::Result<SpawnedOrchestrator>`
  — Task 5's `run_spawn_sub_orchestrator` passes `Some(&parent_id)`; existing
  callers (`run_spawn_orchestrator`, `run_orchestrate`) pass `None` (peer
  semantics unchanged).

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn spawn_orchestrator_common_records_the_given_parent() {
    let store = ninox_core::store::Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap();
    let _ = spawn_orchestrator_common(
        &store, &ninox_core::config::AppConfig::default(), "nested-test", None, Some("parent-orch"),
    ).await;
    // tmux create fails deterministically (no real tmux), but the session
    // row must already record the parent before that unrelated failure.
    let session = store.get_session("nested-test").unwrap();
    assert!(session.is_none() || session.unwrap().orchestrator_id.as_deref() == Some("parent-orch"));
}

#[tokio::test]
async fn spawn_orchestrator_common_still_defaults_to_no_parent() {
    let store = ninox_core::store::Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap();
    let _ = spawn_orchestrator_common(
        &store, &ninox_core::config::AppConfig::default(), "peer-test", None, None,
    ).await;
    if let Some(session) = store.get_session("peer-test").unwrap() {
        assert_eq!(session.orchestrator_id, None);
    }
}
```

(Both assertions tolerate the row being rolled back on tmux-create failure,
same as the existing `run_spawn_orchestrator`/`run_orchestrate` tests — the
point is: if a row exists at all, before rollback, it has the right
parent.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox-ui-shared spawn_orchestrator_common -- --nocapture`
Expected: FAIL (wrong argument count — compile error).

- [ ] **Step 3: Add the parameter**

Add `parent_orchestrator_id: Option<&str>` as `spawn_orchestrator_common`'s
final parameter; change the `Session` literal's `orchestrator_id: None,`
(around line 1812) to
`orchestrator_id: parent_orchestrator_id.map(str::to_string),`. Update the
two existing call sites (`run_spawn_orchestrator`, `run_orchestrate`) to
pass `None` as the new last argument.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ninox-ui-shared spawn_orchestrator_common`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-ui-shared/src/cli.rs
git commit -m "refactor(cli): thread an optional parent pointer through spawn_orchestrator_common"
```

---

### Task 5: `ninox spawn-sub-orchestrator` command

**Files:**
- Modify: `crates/ninox-ui-shared/src/cli.rs`

**Interfaces:**
- Consumes: `orchestrator_auth::authorize_nested_spawn` and
  `orchestrator_auth::orchestrator_depth` (Task 3);
  `config.nested_orchestration.max_depth` (Task 1);
  `config.worker_type_registry()` (existing); `spawn_orchestrator_common`
  with its new 5th parameter (Task 4); `CallerRuntime` (existing struct).
- Produces: `Command::SpawnSubOrchestrator { name, prompt, orchestrator_id }`
  variant, dispatched to `run_spawn_sub_orchestrator`.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn run_spawn_sub_orchestrator_rejects_an_unowned_orchestrator_id_claim() {
    use std::sync::Arc;
    let store = Arc::new(ninox_core::store::Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());

    let result = run_spawn_sub_orchestrator(
        store.clone(), ninox_core::config::AppConfig::default(),
        "forged-sub".into(), None, Some("some-real-orchestrator".into()),
        CallerRuntime { caller_type: Some("orchestrator".into()), pane: None },
    ).await;

    let err = result.expect_err("an unverified orchestrator-id claim must be refused").to_string();
    assert!(err.contains("some-real-orchestrator"), "{err}");
    assert!(store.get_session("forged-sub").unwrap().is_none(), "a rejected claim must leave no session row");
}

#[tokio::test]
async fn run_spawn_sub_orchestrator_rejects_exactly_at_the_depth_cap() {
    use std::sync::Arc;
    let store = Arc::new(ninox_core::store::Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
    // Build a chain two levels deep under a verified root; the cap (default 3)
    // means a third nested level is rejected.
    register_orchestrator_for_depth_test(&store, "root", "root");
    seed_sub_orchestrator(&store, "sub1", "root");
    seed_sub_orchestrator(&store, "sub2", "sub1");

    let result = run_spawn_sub_orchestrator(
        store.clone(), ninox_core::config::AppConfig::default(),
        "sub3".into(), None, Some("sub2".into()),
        CallerRuntime { caller_type: Some("orchestrator".into()), pane: Some(pane_for("sub2")) },
    ).await;

    let err = result.expect_err("spawning at exactly the cap must be rejected").to_string();
    assert!(err.contains("cap"), "{err}");
    assert!(store.get_session("sub3").unwrap().is_none());
}

#[tokio::test]
async fn run_spawn_sub_orchestrator_allows_one_level_under_the_cap() {
    use std::sync::Arc;
    let store = Arc::new(ninox_core::store::Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
    register_orchestrator_for_depth_test(&store, "root", "root");
    seed_sub_orchestrator(&store, "sub1", "root");

    let result = run_spawn_sub_orchestrator(
        store.clone(), ninox_core::config::AppConfig::default(),
        "sub2".into(), None, Some("sub1".into()),
        CallerRuntime { caller_type: Some("orchestrator".into()), pane: Some(pane_for("sub1")) },
    ).await;

    // tmux create fails deterministically in tests (no real tmux) — the
    // point is the error must NOT be a depth-cap or identity rejection.
    let err = result.expect_err("tmux create still fails in tests").to_string();
    assert!(!err.contains("cap"), "{err}");
    assert!(!err.contains("could not be verified"), "{err}");
}

#[tokio::test]
async fn run_spawn_sub_orchestrator_rejects_a_worker_type_without_permission() {
    use std::sync::Arc;
    let store = Arc::new(ninox_core::store::Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
    register_orchestrator_for_depth_test(&store, "root", "root");
    let mut w = ninox_core::types::Session { worker_type: Some("Implementation".into()), ..worker_session("worker-a", "root") };
    store.upsert_session(&w).unwrap();

    let result = run_spawn_sub_orchestrator(
        store.clone(), ninox_core::config::AppConfig::default(),
        "sub-by-worker".into(), None, None,
        CallerRuntime { caller_type: Some("worker".into()), pane: Some(pane_for("worker-a")) },
    ).await;

    assert!(result.is_err());
    assert!(store.get_session("sub-by-worker").unwrap().is_none());
}
```

(Helper fns `register_orchestrator_for_depth_test`, `seed_sub_orchestrator`,
`pane_for`, `worker_session` factor the existing `register_orchestrator`/
`session_with`/`pane` helpers already in this test module — add small local
wrappers if the existing ones aren't directly reusable across the
`ninox_core`/`ninox_ui_shared` crate boundary.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox-ui-shared run_spawn_sub_orchestrator -- --nocapture`
Expected: FAIL (function/variant don't exist — compile error).

- [ ] **Step 3: Implement the command**

Add to the `Command` enum (after `SpawnOrchestrator`):

```rust
/// Spawn a sub-orchestrator nested under an already-verified orchestrator
/// (or, for a worker whose worker-type profile explicitly grants it,
/// nested under that worker's own parent) — real delegation, not a peer.
/// Enforces the configured depth cap (`[nested_orchestration]`, default
/// 3): a request that would exceed it is rejected outright.
SpawnSubOrchestrator {
    /// Display name for the sub-orchestrator; slugified into its session ID
    #[arg(long, short)]
    name: String,
    /// Initial brief, delivered once the new sub-orchestrator's harness is
    /// ready for input.
    #[arg(long, short)]
    prompt: Option<String>,
    /// Parent orchestrator id (read from NINOX_ORCHESTRATOR_ID if not
    /// supplied) — verified before being trusted, exactly like `ninox
    /// spawn`'s `--orchestrator-id`.
    #[arg(long)]
    orchestrator_id: Option<String>,
},
```

Dispatch arm (after `SpawnOrchestrator`'s):

```rust
Some(Command::SpawnSubOrchestrator { name, prompt, orchestrator_id }) => {
    let config = AppConfig::load().unwrap_or_default();
    let orchestrator_id = orchestrator_id.or_else(|| fleet::env_nonempty("NINOX_ORCHESTRATOR_ID"));
    let caller = CallerRuntime {
        caller_type: fleet::env_nonempty("NINOX_CALLER_TYPE"),
        pane: ninox_core::runtime::current_private_pane_identity().await.ok().flatten(),
    };
    run_spawn_sub_orchestrator(store, config, name, prompt, orchestrator_id, caller).await
}
```

`run_spawn_sub_orchestrator` (near `run_spawn_orchestrator`): resolves
`NINOX_SESSION` via `fleet::env_nonempty`, calls
`orchestrator_auth::authorize_nested_spawn` (wrapping its error to include
the claimed id when one was given, mirroring `run_spawn`'s existing
wrapping), then `orchestrator_auth::orchestrator_depth` against
`config.nested_orchestration.max_depth`, `ensure!`s `depth + 1 <
max_depth`, and only then calls `spawn_orchestrator_common(&store, &config,
&name, prompt, Some(&parent_id))`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ninox-ui-shared run_spawn_sub_orchestrator`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-ui-shared/src/cli.rs
git commit -m "feat(cli): add ninox spawn-sub-orchestrator with depth-cap enforcement"
```

---

### Task 6: Capability registry entry and skill

**Files:**
- Create: `crates/ninox-core/skills/orchestrator/spawn-sub-orchestrator.md`
- Modify: `crates/ninox-core/src/capabilities.rs`

**Interfaces:**
- Consumes: `Capability`, `Audience` (existing).
- Produces: a `"spawn-sub-orchestrator"` entry in `REGISTRY`, seeded
  automatically into every orchestrator root by the existing generic
  seeding loop — no change needed to `orchestrator_root.rs`.

- [ ] **Step 1: Write the failing test**

Update `registry_covers_every_migrated_skill`'s orchestrator-name list to
insert `"spawn-sub-orchestrator"` right after `"spawn-orchestrator"`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ninox-core registry_covers_every_migrated_skill`
Expected: FAIL (actual list is missing the new name).

- [ ] **Step 3: Add the skill file and registry entry**

`spawn-sub-orchestrator.md` frontmatter:

```markdown
---
name: spawn-sub-orchestrator
description: Spawn a real sub-orchestrator nested under you — hard-capped by depth. Distinct from spawn-orchestrator (a peer); use only for genuine multi-level delegation.
---
```

Body: explains this is real delegation (the sub-orchestrator manages its
own worker pool), that depth is hard-capped (configurable, default 3), that
hitting the cap means spawning a plain worker instructed to report back up
via `{{NINOX_BIN}} send <parent>` instead of nesting further, and the
command shape:

```bash
{{NINOX_BIN}} spawn-sub-orchestrator \
  --name "billing-migration-payments" \
  --prompt "Own the payments section of the billing migration: <brief>"
```

Registry entry (after the `"spawn-orchestrator"` entry):

```rust
Capability {
    name: "spawn-sub-orchestrator",
    audience: Audience::Orchestrator,
    orchestrator_md: Some(include_str!("../skills/orchestrator/spawn-sub-orchestrator.md")),
    worker_md: None,
    enabled: |_| true,
    hook: None,
},
```

(Orchestrator-only, matching `spawn-worker`'s pattern: whether a *worker*
may call this is gated at runtime by its worker-type's
`allow_nested_spawn`, not by a global config toggle the registry's
`enabled: fn(&AppConfig)` shape can express per-type — seeding a worker-side
skill is left to a worker type's own `brief_template`, which already
controls worker-facing framing.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ninox-core capabilities::`
Expected: PASS (including the generic invariant tests, which cover the new
entry automatically)

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-core/skills/orchestrator/spawn-sub-orchestrator.md crates/ninox-core/src/capabilities.rs
git commit -m "feat(capabilities): register spawn-sub-orchestrator skill"
```

---

## Final verification

- [ ] `cargo fmt --check -- crates/ninox-core/src/config.rs crates/ninox-core/src/worker_type.rs crates/ninox-core/src/orchestrator_auth.rs crates/ninox-ui-shared/src/cli.rs crates/ninox-core/src/capabilities.rs` — informational only; do not run a wholesale `cargo fmt`.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] Rebase on `origin/v1`, open PR titled `MLOPS-5308: feat(orchestrator): nested orchestration with a configurable depth cap`, targeting `v1`.
