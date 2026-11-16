# Automatic Planning and Sectioning — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a scoped, non-interactive planning subprocess and CLI command that takes a task description, explores a target workspace read-only, and emits a validated ghost-node plan — the backend+CLI half of auto-planning. No UI (the review/approval UI is item 7, already shipped against a `demo_plan()` stub this work replaces/supplements).

**Architecture:** A new `ninox-core::lifecycle::auto_plan` module, architecturally parallel to `lifecycle::brain_harvest`: a `PlanRunner` trait (real impl shells out to `claude -p --tools Read,Grep`, test impl is a fake returning canned JSON), a pure validator (`validate_plan`), and a `generate_plan` function wiring runner → parse → validate. The data model (`GhostNode`/`GhostPlan`) is defined locally in `ninox-core` but field-for-field identical to the already-shipped `ninox_overview::node::GhostNode` / `ninox_overview::plan::GhostPlan` (the item-7 ghost-node contract) — `ninox-core` cannot depend on `ninox-overview` (dependency runs the other way), so this plan mirrors the shape rather than importing it, keeping this task's JSON output a drop-in replacement for `demo_plan()`'s output once something wires the two together (not this task). Validation reuses the **real**, already-shipped worker-type registry (`AppConfig::worker_type_registry()`, `crate::worker_type::WorkerTypeSpec::allow_nested_spawn`) and depth cap (`AppConfig.nested_orchestration.max_depth`) — no stand-ins invented. `ninox plan generate` is a new CLI verb added to the existing `plan` command group, reusing `authorize_orchestrator`.

**Tech Stack:** Rust, `anyhow`, `serde`/`serde_json`, `tokio::process::Command`.

**Spec:** `docs/superpowers/specs/2026-10-22-auto-planning-design.md`

**Supersedes:** an earlier version of this plan (first committed 2026-10-27) invented stand-ins — a `worker_types: Vec<String>` config list and a separate `[planning].depth_cap` — for a worker-type registry and depth cap that did not exist on `origin/v1` at the time. Both now exist for real (merged 2026-10-27–29, PRs #265/#270/#272 and the nested-orchestration work), so this revision reuses them instead.

## Global Constraints

- The real depth cap is `AppConfig.nested_orchestration.max_depth` (default 3, see `docs/superpowers/specs/2026-10-21-nested-orchestration-design.md`). This plan does not add a second cap value.
- The real worker-type registry is `AppConfig::worker_type_registry() -> ninox_core::worker_type::WorkerTypeRegistry` (`.get(name) -> Option<&WorkerTypeSpec>`, resolving aliases). This plan does not add a second registry.
- A ghost node "owning children" (another ghost's `parent_id` pointing at it — i.e. proposing a sub-orchestrator) is valid only when its own `worker_type` resolves to a spec with `allow_nested_spawn == true`. This is this plan's resolution of "a section can itself propose a nested sub-orchestrator": there is no separate orchestrator/worker shape — a sub-orchestrator proposal is just a ghost node whose worker type permits nesting (e.g. the builtin `Orchestrator` type), same as the real system (`authorize_nested_spawn`'s worker branch already gates on this exact field).
- The planning subprocess gets **exactly** `Read,Grep` via `--tools` — no `Bash`/`Write`/`Edit`, ever. Uses `--dangerously-skip-permissions` for the same headless-subprocess reason `brain_harvest.rs` does (no TTY to approve anything).
- `[planning]` config reuses the existing `AgentConfig` type verbatim (`harness`, `model`) — the same shape `[orchestrator]`/`[worker]` already use. Only `harness = "claude-code"` is implemented in this task (mirrors `brain_harvest.rs`'s own hardcoded `claude` invocation, the cited architectural precedent); any other configured harness is a clean `unsupported_harness` error.
- No new UI, no new DB table, no persistence of a generated plan, no auto-execution/auto-approval, no wiring into `ninox-overview`'s `OverviewState`/`restore_plan` (that integration — presumably alongside the ninox-prompt-agent's own plan-request path — is follow-on work, not this task). `ninox plan generate` only prints the validated `GhostPlan` (or a validation/error envelope) to stdout.
- A `GhostPlan`'s `sections` is a **flat list with `parent_id` pointers**, not a nested tree type — matching `ninox_overview::plan::GhostPlan` exactly, not the nested-`children` shape an earlier draft of this plan used.
- Dependency (`dependencies`) references are resolved against every section id in the same `GhostPlan.sections` list (the spec says "a valid DAG," not "per-level"); a dependency may not name the plan's own `parent_id`.

## Review Focus

- **A `parent_id` that names neither the plan's root nor another section's id** (a dangling parent) — a reasonable person expects this rejected, not silently treated as a root-level section. Covered in Task 2.
- **A ghost node that other ghosts point to via `parent_id`, but whose `worker_type` doesn't permit nesting** (`allow_nested_spawn == false`, e.g. a plain `Researcher` ghost with a sub-ghost under it) — this is the real system's actual gate for "can this own children," not a cosmetic shape check; a validator that only checks "does it have a worker_type" would miss this entirely. Covered in Task 2.
- **Model output wrapped in a ```json fenced code block** (near-universal real-model behavior) rather than bare JSON — a validator that only `serde_json::from_str`s the raw trimmed output would reject almost every real response as malformed; that's not the "fails cleanly on genuinely bad input" the spec intends. Covered in Task 3.
- **A section that depends on itself, or on the plan's root `parent_id`** — the trivial one-node cycle, and a reference to an id that exists but isn't a valid dependency target. Covered in Task 2.
- **An unknown/unsupported `worker_type` name that is merely a case or alias mismatch** (e.g. `"researcher"` vs. the canonical `"Researcher"`, or the legacy `"Implementation"` alias) — validation must go through the registry's own alias/case resolution (`WorkerTypeRegistry::get`), not a naive string-equality check, or it will reject valid plans. Covered in Task 2.

---

### Task 1: `[planning]` config section

**Files:**
- Modify: `crates/ninox-core/src/config.rs`

**Interfaces:**
- Produces: `AppConfig.planning: AgentConfig` (`#[serde(default)]`), placed after `worker: AgentConfig` and before `harnesses` (TOML scalars/simple-tables-before-table-of-tables ordering, matching the existing convention). Reuses the already-defined `AgentConfig`/`default_harness()` — no new struct.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn planning_config_defaults_and_round_trips() {
    let toml = "port = 8080\nfont_size = 13.0\n\n[planning]\nharness = \"claude-code\"\nmodel = \"claude-opus-4-5\"\n";
    let cfg: AppConfig = toml::from_str(toml).unwrap();
    assert_eq!(cfg.planning.harness, "claude-code");
    assert_eq!(cfg.planning.model.as_deref(), Some("claude-opus-4-5"));
}

#[test]
fn planning_config_absent_uses_defaults() {
    let cfg: AppConfig = toml::from_str("port = 8080\nfont_size = 13.0\n").unwrap();
    assert_eq!(cfg.planning.harness, "claude-code");
    assert!(cfg.planning.model.is_none());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ninox-core planning_config --lib`
Expected: FAIL to compile — `AppConfig` has no field `planning`.

- [ ] **Step 3: Add `planning: AgentConfig` to `AppConfig`'s struct body and `impl Default` in `crates/ninox-core/src/config.rs`**

Add immediately after `worker: AgentConfig::default()` in both the struct definition and the `Default` impl.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ninox-core planning_config --lib`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-core/src/config.rs
git commit -m "feat(config): add [planning] agent config section"
```

---

### Task 2: Ghost-plan data model and validation

**Files:**
- Create: `crates/ninox-core/src/lifecycle/auto_plan.rs`
- Modify: `crates/ninox-core/src/lifecycle/mod.rs` (add `pub mod auto_plan;`, alphabetically before `binary_update`)

**Interfaces:**
- Consumes: `ninox_core::worker_type::WorkerTypeRegistry` (`.get(name) -> Option<&WorkerTypeSpec>`, `WorkerTypeSpec.allow_nested_spawn: bool`) — already shipped, imported not redefined.
- Produces (used by Task 3):
  - `#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)] pub struct GhostNode { pub id: String, pub parent_id: String, pub worker_type: Option<String>, pub brief: String, #[serde(default)] pub dependencies: Vec<String> }` — field-for-field matching `ninox_overview::node::GhostNode`.
  - `#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)] pub struct GhostPlan { pub parent_id: String, pub sections: Vec<GhostNode> }` — field-for-field matching `ninox_overview::plan::GhostPlan`.
  - `#[derive(Debug, Clone, PartialEq, Eq)] pub enum PlanValidationError` with variants `DanglingParent { id: String, parent_id: String }`, `NestedSpawnNotAllowed { id: String, worker_type: String }`, `MissingWorkerType { id: String }`, `UnknownWorkerType { id: String, worker_type: String }`, `DuplicateSectionId { id: String }`, `UnknownDependency { id: String, target: String }`, `DependencyCycle { id: String }`, `DepthExceeded { depth: u32, cap: u32 }` — plus `impl PlanValidationError { pub fn code(&self) -> &'static str }` (snake_case variant names, e.g. `"nested_spawn_not_allowed"`) and `impl std::fmt::Display`.
  - `pub fn validate_plan(plan: &GhostPlan, registry: &ninox_core::worker_type::WorkerTypeRegistry, depth_cap: u32) -> Vec<PlanValidationError>` (empty = valid).

- [ ] **Step 1: Write the failing tests**

```rust
use crate::worker_type::WorkerTypeRegistry;

fn registry() -> WorkerTypeRegistry { WorkerTypeRegistry::from_config(&[]) } // builtins: Orchestrator, Implementor, Researcher, Evaluator

fn leaf(id: &str, worker_type: &str, deps: &[&str]) -> GhostNode {
    GhostNode {
        id: id.into(), parent_id: "root".into(), worker_type: Some(worker_type.into()),
        brief: "do it".into(), dependencies: deps.iter().map(|s| s.to_string()).collect(),
    }
}

#[test]
fn valid_flat_plan_passes() {
    let plan = GhostPlan { parent_id: "root".into(), sections: vec![leaf("a", "Implementor", &[]), leaf("b", "Researcher", &["a"])] };
    assert!(validate_plan(&plan, &registry(), 3).is_empty());
}

#[test]
fn nested_sub_orchestrator_within_depth_cap_passes() {
    let orch = leaf("sub", "Orchestrator", &[]); // allow_nested_spawn: true builtin
    let mut child = leaf("child", "Implementor", &[]);
    child.parent_id = "sub".into();
    let plan = GhostPlan { parent_id: "root".into(), sections: vec![orch, child] };
    assert!(validate_plan(&plan, &registry(), 3).is_empty());
}

#[test]
fn child_under_a_non_nesting_worker_type_is_rejected() {
    let researcher = leaf("r", "Researcher", &[]); // allow_nested_spawn: false builtin
    let mut child = leaf("child", "Implementor", &[]);
    child.parent_id = "r".into();
    let errs = validate_plan(&GhostPlan { parent_id: "root".into(), sections: vec![researcher, child] }, &registry(), 3);
    assert!(errs.contains(&PlanValidationError::NestedSpawnNotAllowed { id: "r".into(), worker_type: "Researcher".into() }));
}

#[test]
fn dangling_parent_is_rejected() {
    let mut s = leaf("a", "Implementor", &[]);
    s.parent_id = "nowhere".into();
    let errs = validate_plan(&GhostPlan { parent_id: "root".into(), sections: vec![s] }, &registry(), 3);
    assert!(errs.contains(&PlanValidationError::DanglingParent { id: "a".into(), parent_id: "nowhere".into() }));
}

#[test]
fn duplicate_section_id_is_rejected() {
    let plan = GhostPlan { parent_id: "root".into(), sections: vec![leaf("a", "Implementor", &[]), leaf("a", "Researcher", &[])] };
    assert!(validate_plan(&plan, &registry(), 3).contains(&PlanValidationError::DuplicateSectionId { id: "a".into() }));
}

#[test]
fn unknown_worker_type_is_rejected() {
    let errs = validate_plan(&GhostPlan { parent_id: "root".into(), sections: vec![leaf("a", "ghost-type", &[])] }, &registry(), 3);
    assert!(errs.contains(&PlanValidationError::UnknownWorkerType { id: "a".into(), worker_type: "ghost-type".into() }));
}

#[test]
fn worker_type_alias_and_case_resolve_through_the_registry() {
    // "Implementation" is a documented legacy alias for the canonical "Implementor".
    let plan = GhostPlan { parent_id: "root".into(), sections: vec![leaf("a", "Implementation", &[])] };
    assert!(validate_plan(&plan, &registry(), 3).is_empty());
}

#[test]
fn missing_worker_type_is_rejected() {
    let mut s = leaf("a", "Implementor", &[]);
    s.worker_type = None;
    let errs = validate_plan(&GhostPlan { parent_id: "root".into(), sections: vec![s] }, &registry(), 3);
    assert!(errs.contains(&PlanValidationError::MissingWorkerType { id: "a".into() }));
}

#[test]
fn dependency_on_unknown_section_is_rejected() {
    let errs = validate_plan(&GhostPlan { parent_id: "root".into(), sections: vec![leaf("a", "Implementor", &["missing"])] }, &registry(), 3);
    assert!(errs.contains(&PlanValidationError::UnknownDependency { id: "a".into(), target: "missing".into() }));
}

#[test]
fn dependency_on_the_plan_root_is_rejected_as_unknown() {
    let errs = validate_plan(&GhostPlan { parent_id: "root".into(), sections: vec![leaf("a", "Implementor", &["root"])] }, &registry(), 3);
    assert!(errs.contains(&PlanValidationError::UnknownDependency { id: "a".into(), target: "root".into() }));
}

#[test]
fn self_dependency_is_a_cycle() {
    let errs = validate_plan(&GhostPlan { parent_id: "root".into(), sections: vec![leaf("a", "Implementor", &["a"])] }, &registry(), 3);
    assert!(errs.iter().any(|e| matches!(e, PlanValidationError::DependencyCycle { .. })));
}

#[test]
fn two_node_dependency_cycle_is_rejected() {
    let plan = GhostPlan { parent_id: "root".into(), sections: vec![leaf("a", "Implementor", &["b"]), leaf("b", "Implementor", &["a"])] };
    assert!(validate_plan(&plan, &registry(), 3).iter().any(|e| matches!(e, PlanValidationError::DependencyCycle { .. })));
}

#[test]
fn tree_exactly_at_depth_cap_passes_and_one_over_is_rejected() {
    // root(=plan.parent_id, depth 0) -> a(1) -> b(2) -> c(3): depth cap 3 — must pass.
    let mut a = leaf("a", "Orchestrator", &[]);
    let mut b = leaf("b", "Orchestrator", &[]); b.parent_id = "a".into();
    let mut c = leaf("c", "Implementor", &[]); c.parent_id = "b".into();
    let plan = GhostPlan { parent_id: "root".into(), sections: vec![a.clone(), b.clone(), c.clone()] };
    assert!(validate_plan(&plan, &registry(), 3).is_empty());

    // one level deeper: root -> a -> b -> c -> d, depth 4 — must be rejected.
    let mut d = leaf("d", "Implementor", &[]); d.parent_id = "c".into();
    c.worker_type = Some("Orchestrator".into()); // c must now permit nesting too
    let plan2 = GhostPlan { parent_id: "root".into(), sections: vec![a, b, c, d] };
    let errs = validate_plan(&plan2, &registry(), 3);
    assert!(errs.contains(&PlanValidationError::DepthExceeded { depth: 4, cap: 3 }));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox-core auto_plan:: --lib`
Expected: FAIL to compile — module/types don't exist yet.

- [ ] **Step 3: Implement the data model and `validate_plan` in `crates/ninox-core/src/lifecycle/auto_plan.rs`**

For each section: resolve its depth and ancestor chain by walking `parent_id` pointers up to `plan.parent_id` (root, depth 0), bounded to `depth_cap + 1` hops (beyond that, report `DepthExceeded` using the hop count actually reached rather than looping further — mirrors `orchestrator_auth::orchestrator_depth`'s own bounded-walk shape). A `parent_id` that is neither `plan.parent_id` nor another section's `id` is `DanglingParent`. For "does X own children," scan for any section whose `parent_id == X.id`; if found, `registry.get(X.worker_type)` must have `allow_nested_spawn == true` or report `NestedSpawnNotAllowed`. Resolve every `worker_type` through `registry.get(name)` (handles aliases/case). Build one global dependency adjacency map across `sections` (never including `plan.parent_id` as a valid target) for `UnknownDependency` and `DependencyCycle` (standard DFS with a visiting/visited color set, or Kahn's algorithm — implementer's choice). Report every error found, not just the first.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ninox-core auto_plan:: --lib`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-core/src/lifecycle/auto_plan.rs crates/ninox-core/src/lifecycle/mod.rs
git commit -m "feat(planning): add ghost-plan model and validation"
```

---

### Task 3: Planning subprocess runner, prompt, parsing, and `generate_plan`

**Files:**
- Modify: `crates/ninox-core/src/lifecycle/auto_plan.rs`

**Interfaces:**
- Consumes: `GhostPlan`, `GhostNode`, `validate_plan` (Task 2); `ninox_core::worker_type::WorkerTypeRegistry` (shipped).
- Produces (used by Task 4):
  - `pub trait PlanRunner: Send + Sync { fn run(&self, prompt: String, workspace: PathBuf, model: Option<String>) -> Pin<Box<dyn Future<Output = Result<String>> + Send>>; }`
  - `pub struct ClaudePlanRunner;` implementing it (real `claude -p` subprocess).
  - `pub fn build_plan_prompt(task: &str, parent_id: &str, worker_type_names: &[String], depth_cap: u32) -> String`
  - `pub fn parse_plan_output(raw: &str, parent_id: &str) -> Result<GhostPlan, AutoPlanError>`
  - `#[derive(Debug)] pub enum AutoPlanError { UnsupportedHarness(String), SubprocessFailed(String), MalformedOutput(String), ValidationFailed(Vec<PlanValidationError>) }` with `impl AutoPlanError { pub fn code(&self) -> &'static str }` and `impl std::fmt::Display`.
  - `pub async fn generate_plan(runner: &dyn PlanRunner, registry: &ninox_core::worker_type::WorkerTypeRegistry, task: &str, workspace: PathBuf, parent_id: &str, harness: &str, model: Option<String>, depth_cap: u32) -> Result<GhostPlan, AutoPlanError>`

- [ ] **Step 1: Write the failing tests**

```rust
struct FakeRunner(String); // canned stdout
impl PlanRunner for FakeRunner {
    fn run(&self, _: String, _: PathBuf, _: Option<String>) -> Pin<Box<dyn Future<Output = Result<String>> + Send>> {
        let out = self.0.clone();
        Box::pin(async move { Ok(out) })
    }
}

#[test]
fn parse_plan_output_accepts_bare_json() {
    let raw = r#"{"sections":[{"id":"a","parent_id":"root","brief":"b","worker_type":"Implementor","dependencies":[]}]}"#;
    let plan = parse_plan_output(raw, "root").unwrap();
    assert_eq!(plan.sections.len(), 1);
    assert_eq!(plan.parent_id, "root");
}

#[test]
fn parse_plan_output_extracts_fenced_json_block() {
    let raw = "Here is the plan:\n```json\n{\"sections\":[]}\n```\nLet me know if you'd like changes.";
    assert!(parse_plan_output(raw, "root").is_ok());
}

#[test]
fn parse_plan_output_rejects_non_json_cleanly() {
    let err = parse_plan_output("I couldn't find anything to plan.", "root").unwrap_err();
    assert!(matches!(err, AutoPlanError::MalformedOutput(_)));
}

#[test]
fn build_plan_prompt_includes_task_worker_types_and_depth_cap() {
    let prompt = build_plan_prompt("add dark mode", "root", &["Implementor".into(), "Researcher".into()], 3);
    assert!(prompt.contains("add dark mode"));
    assert!(prompt.contains("Implementor"));
    assert!(prompt.contains("Researcher"));
    assert!(prompt.contains('3'));
}

#[tokio::test]
async fn generate_plan_returns_validated_plan_on_good_output() {
    let runner = FakeRunner(r#"{"sections":[{"id":"a","parent_id":"root","brief":"b","worker_type":"Implementor","dependencies":[]}]}"#.into());
    let reg = ninox_core::worker_type::WorkerTypeRegistry::from_config(&[]);
    let plan = generate_plan(&runner, &reg, "task", PathBuf::from("."), "root", "claude-code", None, 3).await.unwrap();
    assert_eq!(plan.sections[0].id, "a");
}

#[tokio::test]
async fn generate_plan_surfaces_validation_errors_without_returning_a_plan() {
    let runner = FakeRunner(r#"{"sections":[{"id":"a","parent_id":"root","brief":"b","worker_type":"ghost-type","dependencies":[]}]}"#.into());
    let reg = ninox_core::worker_type::WorkerTypeRegistry::from_config(&[]);
    let err = generate_plan(&runner, &reg, "task", PathBuf::from("."), "root", "claude-code", None, 3).await.unwrap_err();
    assert!(matches!(err, AutoPlanError::ValidationFailed(errs) if !errs.is_empty()));
}

#[tokio::test]
async fn generate_plan_rejects_unsupported_harness_before_running_anything() {
    let runner = FakeRunner("should never be read".into());
    let reg = ninox_core::worker_type::WorkerTypeRegistry::from_config(&[]);
    let err = generate_plan(&runner, &reg, "task", PathBuf::from("."), "root", "codex", None, 3).await.unwrap_err();
    assert!(matches!(err, AutoPlanError::UnsupportedHarness(_)));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox-core auto_plan:: --lib`
Expected: FAIL to compile — new items don't exist yet.

- [ ] **Step 3: Implement prompt building, parsing, the runner, and `generate_plan`**

`build_plan_prompt`: a system-style prompt telling the model it may only explore with Read/Grep (never modify anything), lists the available worker type names and the exact depth cap, states the nesting rule from Task 2's Global Constraints (a node can only own children if its worker type allows nested spawning — tell it which of the listed types do, via `worker_type_names` paired with a short note rather than exposing `allow_nested_spawn` as a separate parameter), gives it `parent_id` to use as every top-level section's `parent_id`, and instructs it to reply with **only** a single ```json fenced code block containing `{"sections": [...]}` matching `GhostNode`'s shape (`id`, `parent_id`, `worker_type`, `brief`, `dependencies`) — nothing else. Embed `task` plainly (this is the orchestrator's own request text, not third-party content, unlike `brain_harvest`'s diff — no nonce-wrapping needed).

`parse_plan_output`: try `serde_json::from_str::<GhostPlanWire>` (a local `{ sections: Vec<GhostNode> }` shape without `parent_id`, since the model never invents the root) on the trimmed raw text first; on failure, look for a ```json ... ``` fenced block (or a bare ``` ... ``` block) and retry on its contents; if still unparseable, return `AutoPlanError::MalformedOutput` with a short excerpt of the raw output. On success, wrap into `GhostPlan { parent_id: parent_id.to_string(), sections }`.

`ClaudePlanRunner`: mirror `brain_harvest::build_claude_command`/`configure_harvest_env` — a private `const PLAN_TOOLS: &str = "Read,Grep"` and a private env-passthrough const with the same values as `brain_harvest::HARVEST_ENV_PASSTHROUGH` (that one is private to its module, so this is a deliberate small duplication, not a shared refactor). Command: `claude --dangerously-skip-permissions --tools Read,Grep -p <prompt>` plus `--model <model>` when `Some`, `current_dir(workspace)`.

`generate_plan`: if `harness != "claude-code"`, return `UnsupportedHarness` immediately (before calling the runner). Otherwise call `runner.run(build_plan_prompt(task, parent_id, &registry.names(), depth_cap), workspace, model)`, mapping a subprocess error to `SubprocessFailed`; parse the output via `parse_plan_output(&raw, parent_id)`; run `validate_plan` with `registry` and `depth_cap`; return the plan if empty, else `ValidationFailed`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ninox-core auto_plan:: --lib`
Expected: PASS

- [ ] **Step 5: Add the ignored real-subprocess integration test**

```rust
/// Real `claude` subprocess against a tiny fixture repo — NOT part of the
/// suite (`--ignored`), mirrors `spawn_util::persistence_probe`'s pattern:
/// needs a real, authenticated `claude` binary on PATH.
#[tokio::test]
#[ignore]
async fn generate_plan_end_to_end_against_a_real_subprocess() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("README.md"), "# fixture\n").unwrap();
    let reg = ninox_core::worker_type::WorkerTypeRegistry::from_config(&[]);
    let plan = generate_plan(
        &ClaudePlanRunner, &reg, "Add a one-line CHANGELOG entry", dir.path().to_path_buf(), "root",
        "claude-code", None, 3,
    ).await.expect("a well-formed, valid plan");
    assert!(!plan.sections.is_empty());
}
```

- [ ] **Step 6: Run the full (non-ignored) suite once more**

Run: `cargo test -p ninox-core --lib`
Expected: PASS (the new ignored test is skipped by default)

- [ ] **Step 7: Commit**

```bash
git add crates/ninox-core/src/lifecycle/auto_plan.rs
git commit -m "feat(planning): add planning subprocess runner and generate_plan"
```

---

### Task 4: `ninox plan generate` CLI command

**Files:**
- Modify: `crates/ninox-ui-shared/src/cli.rs`

**Interfaces:**
- Consumes: `ninox_core::lifecycle::auto_plan::{generate_plan, ClaudePlanRunner, AutoPlanError}` (Task 3), `AppConfig.planning` (Task 1), `AppConfig::worker_type_registry()` / `AppConfig.nested_orchestration.max_depth` (shipped), existing `ninox_core::orchestrator_auth::authorize_orchestrator`, existing `emit_plan_envelope`/`plan_error` helpers.
- Produces: a new CLI verb `ninox plan generate <task> [--workspace <path>]`.

- [ ] **Step 1: Add the `Generate` variant to `PlanAction`**

```rust
/// Generate a validated ghost plan for `task` via the configured planning
/// subprocess. Never persists or auto-executes the result — prints it (or
/// a validation/error envelope) to stdout for the caller to act on.
Generate {
    /// Natural-language task description to plan.
    task: String,
    /// Workspace the planning subprocess explores read-only.
    /// Defaults to the current directory.
    #[arg(long)]
    workspace: Option<PathBuf>,
},
```

- [ ] **Step 2: Write the failing test for the JSON mapping of `AutoPlanError` to the envelope's `error` field**

```rust
#[test]
fn auto_plan_error_maps_to_envelope_error_json() {
    let err = ninox_core::lifecycle::auto_plan::AutoPlanError::UnsupportedHarness("codex".into());
    let value = plan_error(err.code(), err.to_string());
    assert_eq!(value["code"], "unsupported_harness");
}

#[test]
fn validation_failed_maps_details_array() {
    let errs = vec![ninox_core::lifecycle::auto_plan::PlanValidationError::DuplicateSectionId { id: "a".into() }];
    let err = ninox_core::lifecycle::auto_plan::AutoPlanError::ValidationFailed(errs);
    let value = plan_error_with_details(&err);
    assert_eq!(value["code"], "validation_failed");
    assert_eq!(value["details"][0]["code"], "duplicate_section_id");
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p ninox-ui-shared auto_plan_error --lib`
Expected: FAIL to compile — `plan_error_with_details` doesn't exist yet.

- [ ] **Step 4: Implement the dispatch arm and `plan_error_with_details`**

Add a `PlanAction::Generate { task, workspace }` arm inside the existing `match action` in `run_plan_cli` (after the orchestrator-id auth check already performed there, which resolves `orchestrator_id` — pass that as `generate_plan`'s `parent_id`), alongside `Register`/`Unregister`/`Show`:

```rust
PlanAction::Generate { task, workspace } => {
    let ws = workspace.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let config = AppConfig::load().unwrap_or_default();
    let registry = config.worker_type_registry();
    match generate_plan(
        &ClaudePlanRunner, &registry, &task, ws, &orchestrator_id,
        &config.planning.harness, config.planning.model.clone(), config.nested_orchestration.max_depth,
    ).await {
        Ok(plan) => { emit_plan_envelope(true, serde_json::to_value(&plan).unwrap(), serde_json::Value::Null); 0 }
        Err(err) => { emit_plan_envelope(false, serde_json::Value::Null, plan_error_with_details(&err)); 1 }
    }
}
```

`fn plan_error_with_details(err: &AutoPlanError) -> serde_json::Value` builds on the existing `plan_error(code, message)`: for `ValidationFailed(errs)`, adds a `"details"` array of `{ "code": e.code(), "message": e.to_string() }` per error; for every other variant it's identical to `plan_error(err.code(), err.to_string())`.

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p ninox-ui-shared auto_plan_error --lib`
Expected: PASS

- [ ] **Step 6: Full workspace verification**

Given shared disk pressure across concurrently-building `v1` workers, scope every check to the crates actually touched rather than the whole workspace, and `cargo clean` this worktree's `target/` once the PR is open:
1. `cargo fmt -- --check crates/ninox-core/src/lifecycle/auto_plan.rs crates/ninox-core/src/lifecycle/mod.rs crates/ninox-core/src/config.rs crates/ninox-ui-shared/src/cli.rs` (format only the touched files — do not run `cargo fmt` on the whole tree per project convention)
2. `cargo clippy -p ninox-core -p ninox-ui-shared --all-targets -- -D warnings`
3. `cargo test -p ninox-core -p ninox-ui-shared`
Expected: all three clean/pass.

- [ ] **Step 7: Manual smoke check (not an automated test — needs a real, authenticated `claude` binary)**

Run: `cargo run -p ninox-tui --bin nx -- plan generate "add a one-line CHANGELOG entry" --workspace <some small repo>` from inside a running, authorized orchestrator session.
Expected: a `{"schema_version":...,"command":"plan","ok":true,"data":{"parent_id":...,"sections":[...]}}` envelope, or a clean `ok:false` envelope with a `code`/`message` (and `details` for validation failures) — never a panic or hang.

- [ ] **Step 8: Commit**

```bash
git add crates/ninox-ui-shared/src/cli.rs
git commit -m "feat(cli): add ninox plan generate"
```

---

## Self-Review

1. **Spec coverage:** Planning subprocess (Task 3) ✓. Read-only exploration enforced via `--tools Read,Grep` (Task 3) ✓. Separate `[planning]` config (Task 1) ✓. Plan is always a proposal, never auto-executed (Global Constraints, Task 4 prints only) ✓. Validation: schema (Task 2), worker-type registry — the real one (Task 2), depth cap — the real one (Task 2), DAG/cycles (Task 2) ✓. Testing section's stubbed-runner validation tests (Task 2 + 3) and real-subprocess integration test (Task 3 Step 5) ✓. Output shape matches item 7's already-shipped ghost-node contract so it's a drop-in replacement for `demo_plan()` (Global Constraints, Task 2) ✓.
2. **Step scan:** every code step names exact signatures; only `validate_plan`'s cycle/depth-walk and `ClaudePlanRunner`'s command-building are left to the implementer, both algorithms the tests don't otherwise pin.
3. **Type consistency:** `GhostNode`/`GhostPlan`/`PlanValidationError`/`AutoPlanError`/`PlanRunner` are defined once in Task 2/3 and referenced identically (same names, same field names) in Task 4. Field names match `ninox_overview`'s shipped types exactly (`id`, `parent_id`, `worker_type`, `brief`, `dependencies`).
4. **Review Focus:** all five items have an owning test in Task 2 or 3 (see above).
5. **Proportion:** this plan is longer than the ~155-line spec mainly because the spec explicitly deferred "exact JSON schema / system prompt wording" and "exact mechanism" questions to this plan, and because reconciling with the real, already-shipped worker-type/depth-cap/ghost-node code (not present when the spec was written) needed spelling out precisely.
