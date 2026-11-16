# Session Self-Identification (`whoami`) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give any ninox session a live, store-backed "what am I" projection (`ninox whoami`), plus `UserPromptSubmit`/`SessionStart` hook reminders that rehydrate the same facts after compaction — Phases 1-3 of the approved spec (Phase 4 is explicitly a future-harness item, not built here).

**Architecture:** One new `ninox_core::identity` module resolves a `SessionIdentity` (or an explicit `Unmanaged`/`Terminated` signal) fresh from the store on every call, reusing `worker_status::resolve_session_id` for caller resolution and `capabilities::for_audience` for the capability slice. `main.rs` gets a new hot-path `Whoami` command (direct use) and `IdentityHook` command (hook entry points). `spawn_util::ensure_statusline_settings` and `orchestrator_root::setup_orchestrator_root` wire the hook commands as sibling entries alongside the existing worker-status/inbox hooks, gated by a new opt-in `[role_rehydration]` config section (default off).

**Tech Stack:** Rust, rusqlite-backed `Store`, existing capability-registry/claude-settings merge machinery.

**Spec:** `docs/superpowers/specs/2026-10-05-session-self-identification-design.md`

## Global Constraints

- Never cache the identity projection — rebuild from the store on every call (spec Goal 1, Risk table).
- Never invent task/persona/workflow text — only identity facts the store already owns (spec Goal 4, Non-goals).
- Unmanaged/terminated sessions get an explicit message, never silence or a guessed identity (spec §5).
- `role_rehydration.enabled` defaults to `false` (spec §7: "default off while hook-wiring rolls out").
- One `Capability` registry entry (`name: "whoami"`, `Audience::Both`) is the sole registration point — markdown + the entry, nothing else (per repo `CLAUDE.md`).
- `ensure_statusline_settings`'s gate must be threaded as a parameter (mirroring `inbox_enabled`), not read from `AppConfig::load()` inside the function — the existing code comment at that call site flags this exact need for a future real-gated hook capability.

## Review Focus

- A worker with no `orchestrator_id` (a standalone session) must not panic or render a bogus owner — `owner` is `Option<String>`, `None` when absent.
- A session id resolvable only via the raw (unvalidated) env var, pointing at a row that is now terminal, must report `Terminated`, not silently fall through to `Unmanaged` (the existing `resolve_session_id` deliberately refuses to resolve terminal rows, so the identity layer needs its own fallback to the raw env var to tell "terminated" apart from "never existed").
- No resolvable id at all (`NINOX_SESSION` unset, cwd matches no workspace) must still produce an explicit, non-panicking message, not an empty string.
- The `UserPromptSubmit`/`SessionStart` hook commands must emit their JSON unconditionally, including the unmanaged/terminated case — unlike the inbox drain hooks, which emit nothing when there's nothing pending.
- `register_orchestrator_runtime`/is_orchestrator vs. worker path must be exercised for both kinds in the capability-slice test, since the filtered `for_audience` set differs by audience.

---

## Task 1: `ninox_core::identity` — resolution and rendering

**Files:**
- Create: `crates/ninox-core/src/identity.rs`
- Modify: `crates/ninox-core/src/lib.rs` (add `pub mod identity;`)

**Interfaces:**
- Produces: `pub enum Resolution { Identity(SessionIdentity), Unmanaged { session_id: Option<String> }, Terminated { session_id: String, at: Option<i64> } }`
- Produces: `pub struct SessionIdentity { session_id: String, is_orchestrator: bool, owner: Option<String>, workspace_path: Option<String>, branch: Option<String>, pr_number: Option<u64>, agent_type: String, model: Option<String>, capabilities: Vec<&'static str> }`
- Produces: `pub fn resolve_for_caller(store: &Store, config: &AppConfig, env_session: Option<&str>, cwd: Option<&Path>) -> anyhow::Result<Resolution>`
- Produces: `pub fn render_text(resolution: &Resolution) -> String`
- Produces: `pub fn render_json(resolution: &Resolution) -> serde_json::Value`
- Produces: `pub const MAX_IDENTITY_REMINDER_CHARS: usize` and `pub fn render_reminder(resolution: &Resolution) -> String` (compact, truncated to the budget, for hook payloads)

- [ ] **Step 1: Write failing tests in `crates/ninox-core/src/identity.rs`** covering: an orchestrator row resolves `is_orchestrator: true`, `owner == Some(self id)`; a worker row resolves `owner == orchestrator_id`; a worker row with no `orchestrator_id` resolves `owner: None`; a missing row with `env_session = Some("ghost")` resolves `Unmanaged { session_id: Some("ghost") }`; a row whose `status` is `Terminated` resolves `Terminated { session_id, at: session.terminal_at }` even though `worker_status::resolve_session_id` itself would return `None` for it; no env var and no cwd match resolves `Unmanaged { session_id: None }`; `render_text`/`render_json`/`render_reminder` each produce the expected shape for all three `Resolution` variants; `render_reminder`'s output never exceeds `MAX_IDENTITY_REMINDER_CHARS`.
- [ ] **Step 2: Run `cargo test -p ninox-core identity:: -- --nocapture`** — expect compile failure (module doesn't exist yet).
- [ ] **Step 3: Implement the module** per the Interfaces block above. `resolve_for_caller` first tries `worker_status::resolve_session_id`; if that yields `None`, falls back to looking up `env_session` directly via `store.get_session` to distinguish `Terminated` from true `Unmanaged`. The capability slice comes from `capabilities::for_audience(audience).filter(|c| (c.enabled)(config)).map(|c| c.name).collect()`. `branch` comes from `store.fleet_record(id)?.and_then(|r| r.branch)`.
- [ ] **Step 4: Run `cargo test -p ninox-core identity::`** — expect PASS.
- [ ] **Step 5: Commit** (`feat(core): add session identity resolution`)

## Task 2: `[role_rehydration]` config + `whoami` capability registry entry

**Files:**
- Modify: `crates/ninox-core/src/config.rs`
- Modify: `crates/ninox-core/src/capabilities.rs`
- Create: `crates/ninox-core/skills/worker/whoami.md`
- Create: `crates/ninox-core/skills/orchestrator/whoami.md`

**Interfaces:**
- Consumes: nothing new from Task 1.
- Produces: `pub struct RoleRehydrationConfig { pub enabled: bool }` (`#[serde(default)]`, `Default` → `enabled: false`), `AppConfig::role_rehydration: RoleRehydrationConfig`, placed after `remote_machines` and before `harnesses` (TOML tables-of-scalars before the table-of-tables, per `CLAUDE.md` convention).
- Produces: one `Capability` entry `{ name: "whoami", audience: Audience::Both, orchestrator_md: Some(include_str!(...)), worker_md: Some(include_str!(...)), enabled: |cfg| cfg.role_rehydration.enabled, hook: None }`, appended to `REGISTRY`.

- [ ] **Step 1: Write failing config tests** in `crates/ninox-core/src/config.rs`, mirroring `pr_watch_defaults_to_disabled`/`pr_watch_can_be_enabled_via_config`: `role_rehydration_defaults_to_disabled`, `role_rehydration_can_be_enabled_via_config` (parse `"[role_rehydration]\nenabled = true\n"`).
- [ ] **Step 2: Run `cargo test -p ninox-core config::tests::role_rehydration`** — expect FAIL (field doesn't exist).
- [ ] **Step 3: Implement `RoleRehydrationConfig` and the `AppConfig` field/Default entry.**
- [ ] **Step 4: Run the config tests** — expect PASS.
- [ ] **Step 5: Write the two skill markdown files** (frontmatter `name: whoami`, one-line `description:`; orchestrator body uses `{{NINOX_BIN}} whoami`, worker body uses plain `ninox whoami`). Explain: resolves live identity from the store, never cached; explicit "not managed"/"terminated" output when unmanaged.
- [ ] **Step 6: Add the `whoami` `Capability` entry to `REGISTRY`** in `capabilities.rs`.
- [ ] **Step 7: Update the two capability-registry tests that hardcode the full name list** (`registry_covers_every_migrated_skill` — append `"whoami"` to both the orchestrator and worker expected vectors) **and `everything_else_is_always_enabled`** (add `"whoami"` to the `name != "watch-pr" && name != "remote-machines"` filter). Add a new test `whoami_capability_is_gated_on_role_rehydration`, mirroring `remote_machines_capability_is_gated_on_its_own_config`.
- [ ] **Step 8: Run `cargo test -p ninox-core capabilities::`** — expect PASS.
- [ ] **Step 9: Commit** (`feat(core): add role_rehydration config and whoami capability`)

## Task 3: `ninox whoami` CLI command

**Files:**
- Modify: `crates/ninox-app/src/main.rs`

**Interfaces:**
- Consumes: `ninox_core::identity::{resolve_for_caller, render_text, render_json}` (Task 1).
- Produces: `Command::Whoami { json: bool }`, dispatched in the hot-path block (alongside `Capabilities`) — reads `Store` + `AppConfig`, resolves `NINOX_SESSION`/cwd, prints text or `--json`.

- [ ] **Step 1: Write a failing integration-style test** in `main.rs`'s test module (or a new `tests/` file if that's this crate's existing convention — check for one before adding) that spawns the binary (or calls the handler function directly, matching how `run_capabilities` is unit-tested) with a seeded `Store` session row and asserts the text and `--json` output both reflect the row; add a second test for an unseeded id asserting the "not managing" text.
- [ ] **Step 2: Run the new test(s)** — expect FAIL (`Whoami` variant doesn't exist).
- [ ] **Step 3: Add the `Whoami` variant to `Command`, the hot-path dispatch arm (same shape as the existing `Capabilities` arm: `Store::open` + `AppConfig::load`, `NINOX_SESSION` env + `current_dir()`), and a `fn run_whoami(store: &Store, config: &AppConfig, env_session: Option<&str>, cwd: Option<&Path>, json: bool) -> String` wrapping `identity::resolve_for_caller` + `render_text`/`render_json`.**
- [ ] **Step 4: Run the test(s)** — expect PASS.
- [ ] **Step 5: Commit** (`feat(app): add ninox whoami command`)

## Task 4: `UserPromptSubmit`/`SessionStart` identity-reminder hooks

**Files:**
- Modify: `crates/ninox-app/src/main.rs`
- Modify: `crates/ninox-core/src/claude_settings.rs` (extend `NINOX_SUBCOMMANDS`)

**Interfaces:**
- Consumes: `identity::{resolve_for_caller, render_reminder}` (Task 1).
- Produces: `Command::IdentityHook { action: IdentityHookAction }` with `IdentityHookAction::Prompt` / `IdentityHookAction::SessionStart`; `fn run_identity_hook(action, db_path)` printing `{"hookSpecificOutput": {"hookEventName": "<event>", "additionalContext": "<reminder>"}}` unconditionally (never silent, unlike inbox drain).

- [ ] **Step 1: Write failing tests** asserting `run_identity_hook` prints valid JSON with the right `hookEventName` for both actions, for a seeded session and for an unmanaged one (never empty output).
- [ ] **Step 2: Run the tests** — expect FAIL.
- [ ] **Step 3: Implement `IdentityHookAction`/`run_identity_hook`, dispatched in the hot-path block next to `Inbox`.** Add `"whoami hook-prompt"` and `"whoami hook-session-start"` to `NINOX_SUBCOMMANDS` in `claude_settings.rs` so settings-merge recognizes them as ninox's own entries.
- [ ] **Step 4: Run the tests** — expect PASS.
- [ ] **Step 5: Commit** (`feat(app): add identity-reminder hook commands`)

## Task 5: Wire the hooks into worker and orchestrator settings

**Files:**
- Modify: `crates/ninox-app/src/spawn_util.rs`
- Modify: `crates/ninox-core/src/orchestrator_root.rs`

**Interfaces:**
- Consumes: Task 4's hook commands.
- Produces: `create_worker_worktree`/`create_worktree_at`/`ensure_statusline_settings` gain a `role_rehydration_enabled: bool` parameter (mirroring `inbox_enabled`); when true, a `whoami hook-prompt` entry is appended to the `UserPromptSubmit` hooks array and a new `"SessionStart": [{ "hooks": [...] }]` key is added. `setup_orchestrator_root` (already takes `config: &AppConfig`) gates the same two entries on `config.role_rehydration.enabled` directly.

- [ ] **Step 1: Write failing tests**: `create_worker_worktree_installs_identity_hooks_when_role_rehydration_enabled` (mirrors `create_worker_worktree_installs_inbox_hooks_when_enabled`) and its negative counterpart; `setup_orchestrator_root_installs_identity_hooks_when_role_rehydration_enabled` and its negative counterpart.
- [ ] **Step 2: Run the tests** — expect FAIL (parameter/behavior doesn't exist).
- [ ] **Step 3: Thread the new parameter through `create_worker_worktree`/`create_worktree_at`/`ensure_statusline_settings`, updating the one caller in `main.rs` to pass `config.role_rehydration.enabled`; add the gated `SessionStart`/`UserPromptSubmit` entries in both `ensure_statusline_settings` and `setup_orchestrator_root`.**
- [ ] **Step 4: Run the full existing test suites for both files** (`cargo test -p ninox-app spawn_util::`, `cargo test -p ninox-core orchestrator_root::`) — expect all PASS, including every pre-existing test (signature change must not break callers).
- [ ] **Step 5: Commit** (`feat: wire identity-reminder hooks into worker and orchestrator settings`)

## Task 6: Workspace-wide verification

- [ ] **Step 1:** `cargo build`
- [ ] **Step 2:** `cargo test --workspace`
- [ ] **Step 3:** `cargo clippy --workspace --all-targets`
- [ ] **Step 4:** Update `CLAUDE.md`'s hot-path command list (`Statusline, Inbox, Open/Close/List..., Connect, Capabilities`) to also name `Whoami`/`IdentityHook`, for documentation accuracy — a one-line edit, folded into this verification commit rather than its own task.
- [ ] **Step 5: Commit** (`docs: note whoami/identity-hook as hot-path commands`) if Step 4 produced a diff.
