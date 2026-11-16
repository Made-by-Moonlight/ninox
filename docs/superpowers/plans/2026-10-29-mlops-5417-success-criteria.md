# MLOPS-5417: Worker Success Criteria & Self-Completion — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Workers finish by meeting explicit, typed success criteria and remove themselves via `ninox complete`, replacing "reaped only when the PR merges" (which strands workers that never open a PR). Orchestrator↔worker messages carry a `kind`, and Ninox suppresses its own redundant done/merged/retired notices once a worker's own completion/request-for-work message already reached the recipient.

**Architecture:** A new `SuccessCriterion` enum (`PrMerged`/`PrOpened`/`ReportSent`/`Check(String)`) lives on `WorkerTypeSpec` as typed defaults and is resolved per spawn (type defaults + `--success`/`--success-only`) into a JSON column on `Session`. `ninox complete` (worker-only, operates on `$NINOX_SESSION`) verifies what it can, refuses on an unsafe worktree, stores an outcome, sends a `completion`-kind message to the live parent, and either flags the session `Done` + a `self_completed_at` marker (poller performs the actual tmux-kill/worktree-remove on its next tick, reusing `Engine::cleanup_session` — `complete` itself must never kill its own pane mid-run) or, under `completion_review = "orchestrator"`, sets `AwaitingReview` for the orchestrator to finalize. Auto-reap-on-merge narrows to sessions whose resolved criteria are exactly `PrMerged`-and-otherwise-verifiable; everything else gets a nudge. A second slice adds `MessageKind` to the messaging layer and a `message_events`-backed dedup check so Ninox's own system notices are suppressed once a worker's own completion/request-for-work already landed.

**Tech Stack:** Rust workspace (`ninox-core`, `ninox-ui-shared`, `ninox-gui`, `ninox-tui`, `ninox-overview`), `rusqlite`, `serde`/`toml`/`serde_json`, `clap`, `reqwest` (GitHub REST), `iced` (GUI), `ratatui` (TUI).

**Spec:** `.claude/design-ref/fleet-overview-v1/BRIEF-MLOPS-5417-success-criteria.md` (read `BRIEF-common.md` §7 first — both live outside this worktree, in the main ninox checkout's `.claude/design-ref/`). The spec is authoritative; where it leaves a genuine gap, this plan's choice is called out inline and must be noted again in the PR description.

## Global Constraints

- Base branch: `origin/v1`; PR targets `v1`, never `main`.
- PR title: `MLOPS-5417: feat(core): worker success criteria and ninox complete` (first PR of this plan — tasks 1–12). A second, stacked PR for tasks 13–16 gets its own ticket-prefixed title at that time.
- `cargo fmt` is **not** run wholesale (the tree isn't rustfmt-clean) — format only touched lines by hand if needed.
- `cargo clippy --workspace --all-targets` and `cargo test --workspace` must pass before every push.
- `WorkerTypeSpec` changes land in their own small commit, separate from everything else, so a rebase onto MLOPS-5416 (`v1-mlops-5416-worker-types`, renaming builtins to Orchestrator/Implementor/Researcher/Evaluator) is a narrow, mechanical conflict.
- Never let `ninox complete` accept a target session id — it always resolves `$NINOX_SESSION` itself, which is also how "a worker can't complete another session" is enforced by construction.
- Never let `ninox complete`'s own process call `cleanup_session`/kill its own pane — it is a child of the pane it would be killing. It writes the outcome, sends the message, flags the session, prints its result, and exits; the poller's next tick does the physical reap.
- Orchestrators are never reaped by this mechanism, under any resolved criteria.
- Don't duplicate MLOPS-5416's `ninox worker-type` CLI — if `--success`/`--success-only`/`completion_review` eventually need a `worker-type create/edit` surface, that's a `ninox request-work`, not this ticket.

## Review Focus

- A session spawned before this change has `success_criteria`/`completion_outcome`/`self_completed_at` all NULL/empty. `ninox complete` on such a session must default sensibly (`[ReportSent]`) rather than failing with "no criteria" or panicking on an empty JSON parse.
- The poller's self-reap sweep races the orchestrator's own `ninox reap <id> --force` on the same session. The sweep must tolerate `store.get_session` returning `None` (already reaped) without erroring the whole tick, mirroring `sweep_retired_sessions`'s existing `Ok(sessions) else return` style.
- `ninox complete` run with `$NINOX_SESSION` unset (e.g., a human pasting the command in a shell) must fail before any side effect — no outcome write, no message sent, no status change — not partially complete.
- The worktree is gone or inaccessible (manually deleted, moved) when `ninox complete` shells out to `git status`: must surface a clear error, never panic on a bad exit code or missing directory.
- `ninox send --kind completion` from an arbitrary/unverified caller must not be able to fabricate a dedup-suppressing completion for a session it doesn't own — the dedup lookup keys off `attributed_from` (the attested sender, post `orchestrator_auth::attest_sender`), never the raw unverified `from` claim.

---

## File Structure

New:
- `crates/ninox-core/src/lifecycle/completion.rs` — criterion verification, worktree safety check, live-parent resolution, outcome type. Kept separate from `poller.rs` (already 7500+ lines) and from `cli.rs` (5000+ lines) so this logic has one obvious home and is unit-testable without a CLI or a running poller.

Modified:
- `crates/ninox-core/src/worker_type.rs` — `SuccessCriterion`, `CompletionReview`, `WorkerTypeSpec::success`/`completion_review`, builtin defaults.
- `crates/ninox-core/src/types.rs` — `SessionStatus::AwaitingReview`, `Session::success_criteria`/`completion_outcome`/`self_completed_at`, `SessionFields` bits, `MessageKind`, `MessageEvent::kind`.
- `crates/ninox-core/src/store.rs` — idempotent `ALTER TABLE` migrations for the new `sessions` and `message_events` columns; read/write + new query methods.
- `crates/ninox-core/src/messaging.rs` — thread `MessageKind` through delivery and event recording; AwaitingReview→Working flip on inbound delivery.
- `crates/ninox-core/src/lifecycle/poller.rs` — self-reap sweep; auto-reap-on-merge narrowing + nudge; dedup checks before the worker-done/kept-alive/retired reactions; tag the work-request send with its kind.
- `crates/ninox-core/src/lifecycle/reactions.rs` — new formatters: definition-of-done brief block, completion-to-parent wrapper, merged-needs-complete nudge.
- `crates/ninox-ui-shared/src/cli.rs` — `Spawn`'s `--success`/`--success-only`; new `Complete` command + `run_complete`; `Send`'s `--kind`; brief-injection wiring in `run_spawn`.
- `crates/ninox-overview/src/status.rs` — `AwaitingReview` classifies as `NeedsYou`.
- `crates/ninox-gui/src/components/fleet_detail.rs` and the TUI's equivalent worker-detail renderer (`crates/ninox-tui/src/tui/island_flow.rs`, mirroring the GUI's `worker_fields`) — "Definition of done" + "Outcome" fields.
- `crates/ninox-core/skills/orchestrator/spawn-worker.md` — mentions `--success`, self-completion, `ninox send --kind`.

---

## Task 1: `SuccessCriterion` / `CompletionReview` and `WorkerTypeSpec` fields

**Files:**
- Modify: `crates/ninox-core/src/worker_type.rs`

**Interfaces:**
- Produces: `pub enum SuccessCriterion { PrMerged, PrOpened, ReportSent, Check(String) }` (`#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]`, `#[serde(rename_all = "snake_case")]` — unit variants serialize as bare strings, `Check` as `{ "check": "..." }`/`{ check = "..." }`, which TOML's array-of-mixed-types support (TOML ≥0.5) and `serde_json` both accept).
- Produces: `pub enum CompletionReview { None, Orchestrator }` (`Serialize`/`Deserialize`, `#[serde(rename_all = "snake_case")]`, `Default` = `None`).
- Produces: `WorkerTypeSpec::success: Vec<SuccessCriterion>` (`#[serde(default = "default_success")]` where `default_success() -> Vec<SuccessCriterion> { vec![SuccessCriterion::ReportSent] }` — this is also `WorkerTypeSpec::default()`'s value, satisfying the spec's "Custom types: `ReportSent` unless set" for both a genuinely-custom type and a config override of a builtin that omits `success`).
- Produces: `WorkerTypeSpec::completion_review: CompletionReview` (`#[serde(default)]`).

- [ ] **Step 1: Write failing tests** in `worker_type.rs`'s `#[cfg(test)]` module:
  - `implementation_builtin_defaults_to_pr_merged` — `reg().get("Implementation").unwrap().success == vec![SuccessCriterion::PrMerged]`.
  - `researcher_and_evaluator_builtins_default_to_report_sent` — both equal `vec![SuccessCriterion::ReportSent]`.
  - `worker_type_spec_default_success_is_report_sent` — `WorkerTypeSpec::default().success == vec![SuccessCriterion::ReportSent]`.
  - `completion_review_defaults_to_none` — `WorkerTypeSpec::default().completion_review == CompletionReview::None`.
  - `success_round_trips_through_toml_with_a_check_item` — a `[[worker_types]]` TOML snippet with `success = ["pr_opened", { check = "docs updated" }]` parses to `vec![SuccessCriterion::PrOpened, SuccessCriterion::Check("docs updated".into())]`.
  - `completion_review_round_trips_through_toml` — `completion_review = "orchestrator"` parses to `CompletionReview::Orchestrator`.

- [ ] **Step 2: Run tests, confirm they fail to compile** (`cargo test -p ninox-core worker_type:: 2>&1 | tail -30`) — expect "no field/variant" errors.

- [ ] **Step 3: Implement** the two enums and the two new `WorkerTypeSpec` fields (add them to the struct, its manual `Default` impl, `builtin_specs()` — set `Implementation`'s `success` explicitly to `vec![SuccessCriterion::PrMerged]`, leave `Researcher`/`Evaluator` on the default — and every existing test-fixture struct literal the compiler flags, e.g. `config_entry_replaces_builtin_spec`'s and `export_import_round_trips_one_entry`'s literals).

- [ ] **Step 4: Run tests** (`cargo test -p ninox-core worker_type::`) — expect PASS.

- [ ] **Step 5: Commit** as its own commit (per Global Constraints — isolates the MLOPS-5416 rebase surface):
```bash
git add crates/ninox-core/src/worker_type.rs
git commit -m "feat(worker-type): add success criteria and completion-review fields"
```

---

## Task 2: Session schema — `AwaitingReview`, criteria/outcome fields, store migrations

**Files:**
- Modify: `crates/ninox-core/src/types.rs`
- Modify: `crates/ninox-core/src/store.rs`

**Interfaces:**
- Consumes: `SuccessCriterion` (Task 1).
- Produces: `SessionStatus::AwaitingReview` variant.
- Produces: `pub struct CompletionOutcome { pub summary: String, pub evidence: Vec<String>, pub completed_at: i64 }` (`Serialize`/`Deserialize`/`PartialEq`) in `types.rs`.
- Produces: `Session::success_criteria: Vec<SuccessCriterion>` (`#[serde(default)]`), `Session::completion_outcome: Option<CompletionOutcome>` (`#[serde(default)]`), `Session::self_completed_at: Option<i64>` (`#[serde(default)]`) — mirrors `merged_at`'s doc-comment style: what it means, when it's set, when it's cleared.
- Produces: `SessionFields::SUCCESS_CRITERIA`, `SessionFields::COMPLETION_OUTCOME`, `SessionFields::SELF_COMPLETED_AT` (next free bits after `ACTIVITY` at `1 << 10`, i.e. `1 << 11`, `1 << 12`, `1 << 13`).
- Produces: `Store` persists the three new `sessions` columns (`success_criteria TEXT`, `completion_outcome TEXT`, `self_completed_at INTEGER`) as JSON-in-TEXT (empty vec → `"[]"`, `None` outcome → `NULL`), via the existing `column_exists`-gated `ALTER TABLE` list plus `upsert_session`'s column list / `ON CONFLICT` clause / row-mapping closure.

- [ ] **Step 1: Write failing tests**:
  - `types.rs`: `session_success_criteria_defaults_to_empty_for_backcompat_json` — deserializing a `Session` JSON blob missing the three new fields succeeds with `success_criteria == vec![]`, `completion_outcome == None`, `self_completed_at == None`.
  - `store.rs`: `upsert_session_round_trips_success_criteria_and_outcome` — build a `Session` with `success_criteria: vec![SuccessCriterion::PrMerged, SuccessCriterion::Check("x".into())]` and `completion_outcome: Some(CompletionOutcome { summary: "done".into(), evidence: vec!["https://x".into()], completed_at: 123 })` and `self_completed_at: Some(456)`; `upsert_session` then `get_session` must round-trip all three exactly.
  - `store.rs`: `pre_existing_row_with_null_new_columns_reads_back_with_defaults` — insert a row via a raw `INSERT` that doesn't set the three new columns (simulating a pre-migration DB), then `get_session` must return `success_criteria: vec![]`, `completion_outcome: None`, `self_completed_at: None` without erroring.

- [ ] **Step 2: Run tests, confirm failure** (`cargo test -p ninox-core -- types:: store::`).

- [ ] **Step 3: Implement.** In `types.rs`: add the `AwaitingReview` variant (placed after `Interrupted` with a doc comment explaining it's set by `ninox complete` under `completion_review = "orchestrator"` and cleared only by the orchestrator's `ninox reap`/`ninox send`); add `CompletionOutcome`; add the three `Session` fields with `#[serde(default)]`; update every existing `Session { .. }` test-fixture literal the compiler flags (`events.rs`, `reactions.rs`, `status.rs`, `messaging.rs`, etc. — there are several `mock_session()`-style helpers; the compiler enumerates all of them). In `store.rs`: add the three columns to the `ALTER TABLE` migration list; extend `upsert_session`'s column list, placeholders, and `ON CONFLICT DO UPDATE SET`; extend the row-mapping closure(s) used by `get_session`/`list_sessions` to parse the JSON columns, treating `NULL`/empty string as the default. `is_terminal()` is **not** changed — `AwaitingReview` has a live agent behind it (or at least a session worth reviewing), unlike `Interrupted`.

- [ ] **Step 4: Run tests** (`cargo test -p ninox-core`) — expect PASS, including every previously-passing test the new variant/fields didn't break.

- [ ] **Step 5: Commit**
```bash
git add crates/ninox-core/src/types.rs crates/ninox-core/src/store.rs
git commit -m "feat(core): add AwaitingReview status and success-criteria/outcome session fields"
```

---

## Task 3: Resolve and store success criteria at spawn time

**Files:**
- Modify: `crates/ninox-ui-shared/src/cli.rs` (the `Spawn` command variant, `run_spawn`)

**Interfaces:**
- Consumes: `SuccessCriterion` (Task 1), `Session::success_criteria` (Task 2), `WorkerTypeSpec::success` (Task 1).
- Produces: `fn resolve_success_criteria(type_defaults: &[SuccessCriterion], success_args: &[String], success_only: bool) -> Vec<SuccessCriterion>` — `success_only` replaces the type defaults outright with `success_args` (each mapped to `SuccessCriterion::Check`); otherwise the type defaults are kept and `success_args` are appended as `Check` items.

- [ ] **Step 1: Write failing tests** (new `#[cfg(test)]` block near `run_spawn`, or alongside existing `cli.rs` unit tests):
  - `resolve_success_criteria_appends_checks_to_type_defaults` — defaults `[PrMerged]`, args `["smoke test passes"]`, not `success_only` → `[PrMerged, Check("smoke test passes")]`.
  - `resolve_success_criteria_success_only_replaces_defaults` — defaults `[PrMerged]`, args `["x", "y"]`, `success_only: true` → `[Check("x"), Check("y")]`.
  - `resolve_success_criteria_with_no_args_keeps_type_defaults` — args `[]`, not `success_only` → unchanged defaults.

- [ ] **Step 2: Run tests, confirm failure.**

- [ ] **Step 3: Implement** `resolve_success_criteria`. Add `#[arg(long)] success: Vec<String>` (repeatable) and `#[arg(long)] success_only: bool` to `Command::Spawn`; thread both through the `Command::Spawn => run_spawn(...)` call and `run_spawn`'s signature. Inside `run_spawn`, after resolving `worker_type` (the existing `match worker_type.as_deref() { None => ..., Some(name) => ... }` block that produces `agent`/`worker_type_name`/`prompt`), also resolve `let type_defaults = worker_type.as_deref().and_then(|n| config.worker_type_registry().get(n)).map(|s| s.success.clone()).unwrap_or_else(|| WorkerTypeSpec::default().success);` and `let resolved_criteria = resolve_success_criteria(&type_defaults, &success, success_only);`. Set `session.success_criteria = resolved_criteria.clone()` in the `Session { .. }` literal built later in `run_spawn`.

- [ ] **Step 4: Run tests** (`cargo test -p ninox-ui-shared`) — expect PASS.

- [ ] **Step 5: Commit**
```bash
git add crates/ninox-ui-shared/src/cli.rs
git commit -m "feat(spawn): resolve and store per-session success criteria"
```

---

## Task 4: "Definition of done" brief injection

**Files:**
- Modify: `crates/ninox-core/src/lifecycle/reactions.rs`
- Modify: `crates/ninox-ui-shared/src/cli.rs` (`run_spawn`)

**Interfaces:**
- Consumes: `Vec<SuccessCriterion>` (Task 1/3).
- Produces: `pub fn format_definition_of_done(criteria: &[SuccessCriterion]) -> String` in `reactions.rs` — one line per criterion (`PrMerged` → "Your pull request is merged.", `PrOpened` → "You have opened a pull request.", `ReportSent` → "You have sent your final report (`ninox complete` *is* that report).", `Check(text)` → `text` verbatim), followed by an instruction to run `ninox complete --summary "<text>" [--evidence <url|path>]…` once every item is met.

- [ ] **Step 1: Write failing tests** in `reactions.rs`:
  - `definition_of_done_lists_every_criterion` — mixed `[PrMerged, Check("docs updated")]` → output contains both "merged" wording and the literal `"docs updated"`.
  - `definition_of_done_tells_the_worker_to_run_complete` — output contains `"ninox complete"`.
  - `definition_of_done_handles_empty_criteria` — `[]` → doesn't panic, still returns a non-empty, sensible string (e.g. "No explicit success criteria — run `ninox complete --summary ...` when the task is done.").

- [ ] **Step 2: Run tests, confirm failure.**

- [ ] **Step 3: Implement** `format_definition_of_done`.

- [ ] **Step 4: Run tests** (`cargo test -p ninox-core reactions::`) — expect PASS.

- [ ] **Step 5: Wire into `run_spawn`.** Append `reactions::format_definition_of_done(&resolved_criteria)` to `effective_prompt` (after `worker_context_footer`, same place/order as that footer is appended — both are post-prompt context, not part of the task brief itself). Add a `cli.rs` test (or extend an existing `run_spawn`-adjacent test, matching however `effective_prompt`/`worker_context_footer` is already tested, e.g. by inspecting the prompt a fake spawn would send) asserting the Definition-of-done block is present in the spawned prompt.

- [ ] **Step 6: Run tests, then commit**
```bash
git add crates/ninox-core/src/lifecycle/reactions.rs crates/ninox-ui-shared/src/cli.rs
git commit -m "feat(spawn): inject a Definition of done block into the worker brief"
```

---

## Task 5: Worktree safety check (dirty / unpushed)

**Files:**
- Create: `crates/ninox-core/src/lifecycle/completion.rs`
- Modify: `crates/ninox-core/src/lifecycle/mod.rs` (register the new module)

**Interfaces:**
- Produces: `pub async fn worktree_unsafe_reason(workspace: &std::path::Path) -> anyhow::Result<Option<String>>` — `Ok(None)` means safe to reap; `Ok(Some(reason))` names what's unsafe (dirty / N unpushed commits); `Err` for a git-invocation failure (missing/inaccessible worktree), which the caller must surface, never swallow into "safe".
  - Dirty: `git -C <workspace> status --porcelain` is non-empty.
  - Upstream configured (`git -C <workspace> rev-parse --abbrev-ref --symbolic-full-name @{u}` succeeds): unsafe if `git -C <workspace> rev-list --count <upstream>..HEAD` > 0.
  - No upstream: resolve the remote's default branch via `git -C <workspace> symbolic-ref refs/remotes/origin/HEAD` (e.g. `refs/remotes/origin/main`); if that resolves, unsafe if `git -C <workspace> rev-list --count $(git -C <workspace> merge-base HEAD <default>)..HEAD` > 0 (commits beyond the fork point with nothing tracking them). If `origin/HEAD` itself can't be resolved, treat as safe (documented limitation — matches the spec's own example: "no upstream, no commits beyond base" is clean; a repo with no resolvable default branch and no upstream has, by definition, nothing provably unpushed to flag).

- [ ] **Step 1: Write failing tests**, using a real temp git repo + a `git worktree add` or plain `git init` (mirror whatever pattern `cli.rs`'s existing `create_worker_worktree` tests already use for throwaway git repos — reuse that helper if one is exported, otherwise write a small local one in this module's test block):
  - `clean_pushed_branch_is_safe` — branch tracks a remote, HEAD == upstream, clean tree → `Ok(None)`.
  - `dirty_tree_is_unsafe` — an uncommitted modified file → `Ok(Some(_))` mentioning uncommitted/dirty.
  - `unpushed_commits_with_upstream_is_unsafe` — one commit ahead of `@{u}` → `Ok(Some(_))` mentioning unpushed.
  - `no_upstream_no_commits_beyond_base_is_safe` — a fresh branch off `origin/HEAD`'s default with zero commits of its own, no `@{u}` set → `Ok(None)` (the brief's "researcher that never committed" case).
  - `no_upstream_with_commits_beyond_base_is_unsafe` — same setup, one local commit beyond the fork point, no `@{u}` → `Ok(Some(_))`.
  - `missing_worktree_directory_errors` — a nonexistent path → `Err(_)`, not `Ok(None)`.

- [ ] **Step 2: Run tests, confirm failure** (module doesn't exist yet).

- [ ] **Step 3: Implement** `worktree_unsafe_reason`, shelling out via `tokio::process::Command` (matching the async style already used for git operations elsewhere in this crate, e.g. `create_worker_worktree`/`remove_worker_worktree`).

- [ ] **Step 4: Run tests** (`cargo test -p ninox-core lifecycle::completion::`) — expect PASS.

- [ ] **Step 5: Commit**
```bash
git add crates/ninox-core/src/lifecycle/completion.rs crates/ninox-core/src/lifecycle/mod.rs
git commit -m "feat(core): detect a dirty or unpushed worktree before self-completion"
```

---

## Task 6: Criterion verification

**Files:**
- Modify: `crates/ninox-core/src/lifecycle/completion.rs`

**Interfaces:**
- Consumes: `SuccessCriterion`, `Session`, `github::GithubApi` (existing trait — mockable, same pattern the poller's own tests already use for a fake `GithubApi`).
- Produces: `pub async fn verify_criteria(session: &Session, criteria: &[SuccessCriterion], github: &dyn github::GithubApi) -> Vec<(SuccessCriterion, Result<(), String>)>` — one verification result per criterion, in input order. `PrOpened`: `Ok(())` iff `session.pr_number.is_some()` (a tracked PR row already implies GitHub confirmed its existence via `poll_pr_reconciliation`; no extra network call). `PrMerged`: requires `session.pr_number`/`session.repo`; calls `github.get_pr_status(owner, repo, pr_number)` (split `session.repo`, which is `"owner/repo"`, on `/`) and checks `.merged`; `Err` if no PR is tracked at all, or if the live check says not merged. `ReportSent`: always `Ok(())` — satisfied by `complete` itself, per spec ("the summary *is* the report"). `Check(_)`: always `Ok(())` — attested, not independently checkable.

- [ ] **Step 1: Write failing tests**, using a fake `GithubApi` (construct one inline implementing the trait, or reuse an existing test fake from `poller.rs`'s test module if it's `pub(crate)`/importable; otherwise a small local fake is fine — don't go fetch and generalize the poller's fake into a shared location unless it already is one):
  - `pr_merged_passes_when_github_reports_merged`.
  - `pr_merged_fails_when_github_reports_not_merged`.
  - `pr_merged_fails_with_no_tracked_pr`.
  - `pr_opened_passes_with_a_tracked_pr_number_and_no_network_call` (assert the fake's merged/open query was never invoked for this criterion — e.g. a call-counting fake).
  - `report_sent_and_check_always_pass`.
  - `verify_criteria_preserves_input_order_and_reports_every_item`.

- [ ] **Step 2: Run tests, confirm failure.**

- [ ] **Step 3: Implement** `verify_criteria`.

- [ ] **Step 4: Run tests** (`cargo test -p ninox-core lifecycle::completion::`) — expect PASS.

- [ ] **Step 5: Commit**
```bash
git add crates/ninox-core/src/lifecycle/completion.rs
git commit -m "feat(core): verify success criteria against GitHub and session state"
```

---

## Task 7: Live-parent resolution

**Files:**
- Modify: `crates/ninox-core/src/lifecycle/completion.rs`

**Interfaces:**
- Produces: `pub fn resolve_live_parent(store: &Store, session: &Session) -> Option<String>` — starts at `session.orchestrator_id`; if that id has no session row, or its row's status `.is_terminal()`, climbs to *that* row's own `orchestrator_id` and repeats; returns the first live id found, or `None` if the chain is empty or every ancestor is gone. Bounded to a small fixed number of hops (reuse the same cap idea as `orchestrator_depth`'s cycle guard, e.g. 64) so a corrupt/cyclic chain can't loop forever.

- [ ] **Step 1: Write failing tests**:
  - `resolve_live_parent_returns_the_direct_parent_when_live`.
  - `resolve_live_parent_climbs_past_a_terminated_parent_to_a_live_grandparent`.
  - `resolve_live_parent_returns_none_when_the_whole_chain_is_gone`.
  - `resolve_live_parent_returns_none_for_a_root_with_no_orchestrator_id`.

- [ ] **Step 2: Run tests, confirm failure.**

- [ ] **Step 3: Implement** `resolve_live_parent`, mirroring `orchestrator_auth::orchestrator_depth`'s walk-and-cap style.

- [ ] **Step 4: Run tests** — expect PASS.

- [ ] **Step 5: Commit**
```bash
git add crates/ninox-core/src/lifecycle/completion.rs
git commit -m "feat(core): resolve the nearest live ancestor for completion routing"
```

---

## Task 8: `ninox complete` command

**Files:**
- Modify: `crates/ninox-ui-shared/src/cli.rs` (new `Command::Complete`, `run_complete`, dispatch wiring)
- Modify: `crates/ninox-core/src/lifecycle/reactions.rs` (completion-message wrapper)

**Interfaces:**
- Consumes: `completion::{worktree_unsafe_reason, verify_criteria}` (Tasks 5–6; `resolve_live_parent` lands in Task 14 — until then, route to `session.orchestrator_id` directly), `CompletionOutcome` (Task 2), `messaging::deliver_message`/`deliver_message_with_acp` (existing — kind-less call for now; Task 13 adds `kind`).
- Produces: `Command::Complete { summary: String, evidence: Vec<String>, force: bool }` (no session-id argument — see Global Constraints).
- Produces: `async fn run_complete(store: Arc<Store>, config: AppConfig, port: u16, summary: String, evidence: Vec<String>, force: bool) -> anyhow::Result<()>`.
- Produces: `reactions::format_completion_reaction(worker: &Session, summary: &str, evidence: &[String]) -> String` — wraps the worker's own summary/evidence for the parent, same convention as `format_work_request_reaction`.

- [ ] **Step 1: Write failing tests** for `run_complete` (integration-style, in `cli.rs`'s test module, using the same tmux-backed test scaffolding `run_spawn`/`run_reap` tests already use):
  - `complete_requires_ninox_session_env` — unset `NINOX_SESSION` → `Err` containing "NINOX_SESSION".
  - `complete_refuses_for_an_orchestrator_session` — session id is in `store.list_orchestrators()` → `Err` mentioning orchestrators.
  - `complete_refuses_on_a_dirty_worktree_without_force` — dirty temp worktree → `Err`, session status unchanged, no message sent, no outcome stored.
  - `complete_succeeds_on_a_dirty_worktree_with_force`.
  - `complete_refuses_when_a_verifiable_criterion_fails` — `PrMerged` criterion, fake GitHub reports not merged → `Err` naming the failing criterion; no side effects.
  - `complete_success_stores_outcome_notifies_parent_marks_done_and_flags_self_completed` — clean worktree, satisfiable criteria → session row has `completion_outcome` set, `status == Done`, `self_completed_at` is `Some`, and the orchestrator received a message containing the summary.
  - `complete_in_review_mode_sets_awaiting_review_instead_of_done` — type's `completion_review == Orchestrator` → status `AwaitingReview`, `self_completed_at` stays `None` (no self-reap; the orchestrator finalizes).
  - `complete_prints_its_result_before_any_reap_happens` — assert the command's own stdout/return carries the success report, independent of whether the session got physically reaped (it doesn't, in this test — reaping is the poller's job per Task 9).

- [ ] **Step 2: Run tests, confirm failure.**

- [ ] **Step 3: Implement.**
  1. Resolve `session_id` from `NINOX_SESSION` (bail with the same message style as `run_request_work`'s missing-env error if unset).
  2. Load the session; bail if missing, or if `store.list_orchestrators()` contains this id.
  3. Resolve the worker type's `completion_review` (`config.worker_type_registry().get(session.worker_type.as_deref().unwrap_or_default())`, defaulting to `CompletionReview::None` for an unknown/absent type).
  4. `worktree_unsafe_reason` on `session.workspace_path`; if `Some(reason)` and not `force`, bail naming the reason and that `--force` overrides it.
  5. `verify_criteria` against `session.success_criteria` (empty → treat as `[ReportSent]`, matching Task 1's "unset defaults to ReportSent"); bail listing every failing criterion if any verifiable one failed.
  6. Build `CompletionOutcome { summary, evidence, completed_at: now }`; read-apply-write it onto the session (mirroring `handle_merge_detection`'s `update_live_session_row`-style pattern — re-read before writing).
  7. Send `reactions::format_completion_reaction(&session, &summary, &evidence)` to `session.orchestrator_id` (if present) via `messaging::deliver_message_with_acp`, same construction as the existing `Command::Send` dispatch arm (same `from`/`sender_pane`/`HttpAcpRouter` resolution) so the message is attributed to this session itself.
  8. If `completion_review == Orchestrator`: set status `AwaitingReview`, leave `self_completed_at` `None`.
     Else: set status `Done`, set `self_completed_at = Some(now)`.
  9. Print the outcome (summary + whether it's awaiting review or will be reaped) to stdout, then return `Ok(())` — no cleanup call here.

- [ ] **Step 4: Run tests** (`cargo test -p ninox-ui-shared`) — expect PASS.

- [ ] **Step 5: Commit**
```bash
git add crates/ninox-ui-shared/src/cli.rs crates/ninox-core/src/lifecycle/reactions.rs
git commit -m "feat(cli): add ninox complete for worker self-completion"
```

---

## Task 9: Poller self-reap sweep + review-mode reap/send handling

**Files:**
- Modify: `crates/ninox-core/src/lifecycle/poller.rs`
- Modify: `crates/ninox-core/src/events.rs` (`reap_workers`'s force-gate for `AwaitingReview`)
- Modify: `crates/ninox-core/src/messaging.rs` (AwaitingReview→Working flip on delivery)

**Interfaces:**
- Consumes: `Session::self_completed_at` (Task 2), `Engine::cleanup_session` (existing).
- Produces: `async fn reap_self_completed_sessions(&self)` method on the poller (same shape as `sweep_retired_sessions`: `let Ok(sessions) = self.engine.store.list_sessions() else { return };`, filter `self_completed_at.is_some()`), called from `start()`'s tick loop alongside the existing `sweep_retired_sessions` call. For each: call `self.engine.cleanup_session(&session.id)` (idempotent — tolerates an already-gone tmux/worktree; this already emits `SessionFields::STATUS | SessionFields::TERMINAL_AT`), then clear `self_completed_at` back to `None` via a read-apply-write (mirroring `handle_merge_detection`'s `update_live_session_row` + `Event::SessionUpdated(row, SessionFields::SELF_COMPLETED_AT)` pattern) so the marker is never retried once handled; a `None` result from the read (session already deleted — e.g. a human beat the poller to `ninox reap --force`) is a silent no-op, not an error. **Note for a future PR (MLOPS-5418):** this is the one call site to the shared reap/cleanup path — when `v1-mlops-5418-workspace-cleanup`'s unified `workspace::cleanup` lands, swap it in here only.
- Produces: `reap_workers` (in `events.rs`) treats an **explicitly-named** (`ReapSelection::Ids`) session whose status is `AwaitingReview` as reapable without `force` (new local, e.g. `let review_bypass = matches!(selection, ReapSelection::Ids(_)) && session.status == SessionStatus::AwaitingReview;` folded into the existing `if !force && (was_live || resumable)` guard as `if !force && !review_bypass && (was_live || resumable)`). Bulk `ReapSelection::All`/`Finished` are unaffected — `AwaitingReview` is not `is_finished`, so it's still skipped by the default bulk reap, matching "the orchestrator finalises with `ninox reap <id>`" (an explicit id), not a bulk sweep.
- Produces: in `messaging::deliver_message_with_acp`, after a successful delivery, if the target session's current status is `AwaitingReview`, flip it to `Working` via read-apply-write (same event-less side-effect convention as `record_message_delivered`/`record_message_event` in that same function).

- [ ] **Step 1: Write failing tests**:
  - `poller.rs`: `reap_self_completed_sessions_cleans_up_a_marked_session` — a `Done` session with `self_completed_at: Some(_)` and a real-but-empty worktree dir → after the sweep, the worktree dir is gone and `self_completed_at` is `None` in the store.
  - `poller.rs`: `reap_self_completed_sessions_tolerates_an_already_deleted_session` — call the sweep on a session id that no longer exists in the store → no panic, no error propagated.
  - `events.rs`: `reap_workers_reaps_an_explicitly_named_awaiting_review_session_without_force` — `ReapSelection::Ids(&[id])`, `force: false`, session status `AwaitingReview` → `ReapOutcome` is a reaped variant, not `SkippedLive`.
  - `events.rs`: `reap_workers_finished_selection_still_excludes_awaiting_review` — `ReapSelection::Finished` never includes an `AwaitingReview` session.
  - `messaging.rs`: `delivering_a_message_to_an_awaiting_review_session_flips_it_to_working`.

- [ ] **Step 2: Run tests, confirm failure.**

- [ ] **Step 3: Implement** the three pieces above.

- [ ] **Step 4: Run tests** (`cargo test -p ninox-core`) — expect PASS.

- [ ] **Step 5: Commit**
```bash
git add crates/ninox-core/src/lifecycle/poller.rs crates/ninox-core/src/events.rs crates/ninox-core/src/messaging.rs
git commit -m "feat(core): poller-driven self-reap and review-mode reap/send handling"
```

---

## Task 10: Narrow auto-reap-on-merge to `PrMerged`-only-and-verifiable sessions

**Files:**
- Modify: `crates/ninox-core/src/lifecycle/poller.rs` (`handle_merge_detection`)
- Modify: `crates/ninox-core/src/lifecycle/reactions.rs` (new nudge formatter)

**Interfaces:**
- Consumes: `Session::success_criteria`, `SuccessCriterion` (Tasks 1–3).
- Produces: `pub fn format_merged_needs_complete_reaction(worker: &Session, pr_number: u64) -> String` in `reactions.rs` — "your PR merged; run `ninox complete` when the remaining criteria are met" wording, naming the worker and PR, matching the existing reaction functions' style/tests.
- Produces: in `handle_merge_detection`, a new local `let auto_reapable = session.success_criteria.iter().any(|c| matches!(c, SuccessCriterion::PrMerged)) && session.success_criteria.iter().all(|c| !matches!(c, SuccessCriterion::Check(_)));` (a `Check` item is never independently verifiable, so its presence always disqualifies the merge path, regardless of `PrMerged` — `PrOpened`/`ReportSent` alongside `PrMerged` don't disqualify it, since both are trivially true once the PR merged and the worker is being cleaned up). When `pr_merged && !auto_reapable`: send `format_merged_needs_complete_reaction` instead of the existing done/kept-alive reaction, and **do not** clean up or stamp `merged_at` — the worker stays exactly as it is until it calls `ninox complete` itself. When `session.success_criteria` is empty (pre-existing/untyped session, back-compat): keep today's unconditional behavior (treat as auto-reapable) — this is a deliberate back-compat carve-out, noted in the PR.

- [ ] **Step 1: Write failing tests** in `poller.rs`'s existing `handle_merge_detection` test block:
  - `merge_detection_auto_reaps_when_criteria_are_pr_merged_only`.
  - `merge_detection_nudges_instead_of_reaping_when_criteria_include_a_check_item` — assert the orchestrator got the nudge wording, the session was NOT cleaned up, and `merged_at` stayed `None`.
  - `merge_detection_still_auto_reaps_when_criteria_are_empty` (back-compat).
  - `merge_detection_auto_reaps_when_criteria_are_pr_merged_and_report_sent`.

- [ ] **Step 2: Run tests, confirm failure.**

- [ ] **Step 3: Implement.**

- [ ] **Step 4: Run tests** (`cargo test -p ninox-core lifecycle::poller::`) — expect PASS.

- [ ] **Step 5: Commit**
```bash
git add crates/ninox-core/src/lifecycle/poller.rs crates/ninox-core/src/lifecycle/reactions.rs
git commit -m "fix(poller): auto-reap on merge only when PrMerged is the whole story"
```

---

## Task 11: UI — Definition of done / Outcome fields, `AwaitingReview` classification

**Files:**
- Modify: `crates/ninox-overview/src/status.rs`
- Modify: `crates/ninox-gui/src/components/fleet_detail.rs` (`worker_fields`)
- Modify: `crates/ninox-tui/src/tui/island_flow.rs` (the worker-detail-pane renderer — mirror `worker_fields`'s structure/placement)

**Interfaces:**
- Consumes: `Session::success_criteria`/`completion_outcome` (Task 2).
- Produces: `classify_member` returns `MemberStatus::NeedsYou` for `SessionStatus::AwaitingReview` (checked before the existing `Blocked`/`CiFailed`/pending-request check, alongside it).
- Produces: GUI `worker_fields` gains, after the existing "Worker type · harness · model" field: a "Definition of done" field listing each `session.success_criteria` entry with `✓`/`○` (✓ for `PrOpened`/`PrMerged` when `session.pr_number`/`.merged_at`-or-`Done` already confirms it, `ReportSent`/`Check` shown `✓` only once `session.completion_outcome.is_some()`, `○` otherwise); when `session.completion_outcome` is `Some`, an additional "Outcome" field below it with the summary and each evidence entry (as a link via `Message::OpenUrl` when it parses as a URL, plain text otherwise — mirror the existing PR-link `rich_text!`/`span(...).link(...)` pattern in `pr_section`).
- Produces: the TUI detail pane renders the same two fields, in whatever text/line style its existing worker-detail function already uses for comparable fields (match its established label/value convention rather than inventing a new one).

- [ ] **Step 1: Write failing tests**:
  - `status.rs`: `awaiting_review_is_needs_you`.
  - `fleet_detail.rs`: a test constructing a `Session` with `success_criteria: vec![PrMerged, Check("x")]` and no outcome yet, asserting `worker_fields` output contains both criterion labels and at least one `○`; a second test with `completion_outcome: Some(_)` asserting the Outcome field's summary text appears.
  - TUI equivalent test(s), matching however that module already tests detail-pane content (grep for its existing field-rendering tests and follow the same assertion style — e.g. asserting rendered buffer cell contents, if that's the established pattern there).

- [ ] **Step 2: Run tests, confirm failure.**

- [ ] **Step 3: Implement** the three pieces. Keep the GUI/TUI changes scoped to exactly these two fields — per the spec, "the try-out workers own everything else in the detail panes."

- [ ] **Step 4: Run tests** (`cargo test -p ninox-overview -p ninox-gui -p ninox-tui`) — expect PASS.

- [ ] **Step 5: Commit**
```bash
git add crates/ninox-overview/src/status.rs crates/ninox-gui/src/components/fleet_detail.rs crates/ninox-tui/src/tui/island_flow.rs
git commit -m "feat(ui): show Definition of done and Outcome in the fleet detail panel"
```

---

## Task 12: Orchestrator skill text (first pass) + end-to-end sanity

**Files:**
- Modify: `crates/ninox-core/skills/orchestrator/spawn-worker.md`

**Interfaces:**
- None new — documentation only.

- [ ] **Step 1:** Add a "Success Criteria & Self-Completion" section after "Worker Types": mention `--success "<text>"` (repeatable) and `--success-only`, that workers self-complete via `ninox complete --summary "..."` once their Definition of done is met, and that a finished worker reaps itself (no more manual `ninox reap` for the common case — `ninox reap` is still there for stragglers/review-mode). Leave the `ninox send --kind` mention for Task 16 (it doesn't exist yet).

- [ ] **Step 2:** Run the full workspace test/lint gate before considering this PR's slice done:
```bash
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
Fix anything that surfaces (in particular: every exhaustive `match` on `SessionStatus` the compiler flags for the new `AwaitingReview` variant that earlier tasks' own builds didn't already force — there may be a few in UI/status-badge/icon code outside the files already touched; fix each with the behavior that reads most consistently with `NeedsYou`/live-but-unusual statuses nearby in that same file, since `classify_member` already establishes the semantic).

- [ ] **Step 3: Commit**
```bash
git add crates/ninox-core/skills/orchestrator/spawn-worker.md
git commit -m "docs(skill): document --success, self-completion in spawn-worker"
```

**→ This is the natural end of PR 1** (`MLOPS-5417: feat(core): worker success criteria and ninox complete`). Rebase on `origin/v1`, run the gate once more, push, open the PR. Continue to Task 13 only after that PR is open (as its own stacked branch/PR per the orchestrator's go-ahead), or continue in the same branch if the orchestrator later says to fold it in — check before assuming either way if time has passed since planning.

---

## Task 13: `MessageKind` and the `message_events.kind` column

**Files:**
- Modify: `crates/ninox-core/src/types.rs`
- Modify: `crates/ninox-core/src/store.rs`
- Modify: `crates/ninox-core/src/messaging.rs`
- Modify: `crates/ninox-ui-shared/src/cli.rs` (`Send`'s `--kind`)
- Modify: `crates/ninox-core/src/events.rs` (`Engine::send_to_session` — poller reactions pass `System`)

**Interfaces:**
- Produces: `pub enum MessageKind { Completion, RequestForWork, Status, Question, System, Freeform }` in `types.rs` (`Serialize`/`Deserialize`/`Copy`/`PartialEq`/`Eq`, `#[serde(rename_all = "snake_case")]`, `Default` = `Freeform`); `impl MessageKind { pub fn tag(self) -> &'static str }` returning e.g. `"[completion]"`/`"[request_for_work]"`/.../`"[freeform]"` for the delivered-text prefix (spec: "shown as a short prefix tag in the delivered text").
- Produces: `MessageEvent::kind: MessageKind` (`#[serde(default)]`).
- Produces: `message_events.kind TEXT` column (idempotent `ALTER TABLE`, default `'freeform'` for pre-existing rows via `COALESCE`/a Rust-side default on read, matching the `sessions` migration convention); `Store::record_message_event` gains a `kind: MessageKind` parameter; `Store::recent_message_events` reads it back.
- Produces: `messaging::deliver_message`/`deliver_message_with_acp` gain a `kind: MessageKind` parameter, prefixing `kind.tag()` onto the delivered text (before the attestation line, so the receiving agent sees the tag first) and passing `kind` through to `record_message_event`. Every existing call site updates: `Command::Send` (new `--kind` flag, default `Freeform`), `Engine::send_to_session` (`System`, since every current caller of it is a poller reaction), `run_request_work`'s delivery path if it goes through `deliver_message` (it currently delivers via `hooks::append_work_request` + the poller's own `deliver_work_requests`, which calls `engine.send_to_session` — tag that call `RequestForWork` instead of the blanket `System` by giving `send_to_session` an optional/explicit kind parameter rather than hardcoding `System` inside it; every other `send_to_session` call site keeps passing `System`).

- [ ] **Step 1: Write failing tests**:
  - `types.rs`: `message_kind_defaults_to_freeform_for_backcompat_json`.
  - `store.rs`: `record_and_read_back_a_message_events_kind`; `pre_existing_message_event_row_with_null_kind_reads_as_freeform`.
  - `messaging.rs`: `delivered_text_is_prefixed_with_the_kind_tag`; `kind_round_trips_through_record_message_event` (extend the existing `recent_message_events_round_trips_and_orders_by_id`-style test).
  - `cli.rs`: `send_default_kind_is_freeform`; `send_kind_flag_overrides_the_default`.
  - `poller.rs`: `work_request_delivery_is_tagged_request_for_work`.

- [ ] **Step 2: Run tests, confirm failure.**

- [ ] **Step 3: Implement**, updating every call site the compiler flags for the new required parameter (there will be several across `cli.rs`, `events.rs`, `poller.rs`, and existing tests in `messaging.rs`/`reactions.rs`) — pass `MessageKind::System` for every existing internally-generated reaction (CI/review/watched-PR/worker-done/worker-retired/nudge), `MessageKind::RequestForWork` for `deliver_work_requests`'s send, and thread `--kind` (default `Freeform`) through `Command::Send`.

- [ ] **Step 4: Run tests** (`cargo test --workspace`) — expect PASS.

- [ ] **Step 5: Commit**
```bash
git add crates/ninox-core/src/types.rs crates/ninox-core/src/store.rs crates/ninox-core/src/messaging.rs crates/ninox-core/src/events.rs crates/ninox-ui-shared/src/cli.rs
git commit -m "feat(messaging): add a kind to every delivered message"
```

---

## Task 14: Route `ninox complete`'s message through `resolve_live_parent`, tag it `Completion`

**Files:**
- Modify: `crates/ninox-ui-shared/src/cli.rs` (`run_complete`)

**Interfaces:**
- Consumes: `completion::resolve_live_parent` (Task 7), `MessageKind::Completion` (Task 13).

- [ ] **Step 1: Write failing tests** (extend Task 8's `run_complete` tests):
  - `complete_sends_exactly_one_completion_message_to_the_live_parent`.
  - `complete_routes_to_the_nearest_live_ancestor_when_the_direct_parent_is_gone`.

- [ ] **Step 2: Run tests, confirm failure.**

- [ ] **Step 3: Implement** — swap `run_complete`'s direct `session.orchestrator_id` target for `completion::resolve_live_parent(&store, &session)`, and pass `MessageKind::Completion` to the delivery call.

- [ ] **Step 4: Run tests** — expect PASS.

- [ ] **Step 5: Commit**
```bash
git add crates/ninox-ui-shared/src/cli.rs
git commit -m "feat(complete): route the completion message to the nearest live ancestor"
```

---

## Task 15: Dedup system notices against a delivered completion

**Files:**
- Modify: `crates/ninox-core/src/store.rs`
- Modify: `crates/ninox-core/src/lifecycle/poller.rs` (`handle_merge_detection`, `sweep_retired_sessions`)

**Interfaces:**
- Produces: `pub fn has_completion_since(&self, from: &str, to: &str, since_ms: i64) -> anyhow::Result<bool>` on `Store` — `true` iff `message_events` has a row with `kind = 'completion'`, `from_id = from`, `to_id = to`, `delivered_at >= since_ms`. `since_ms` is always the session's own `started_at` in call sites below — the "same unit of work" boundary this plan uses, since one worker tracks exactly one PR/task for its whole lifetime (an existing invariant elsewhere in this codebase), making "since this session started" equivalent to "for this PR" without needing a separate per-PR key. **Deviation note for the PR:** the spec's "or since the session's last spawn/resume" distinguishes spawn from resume; there's no separate resume timestamp on `Session` today, so a resumed session's dedup window is its original `started_at` — the practical effect (a worker's own completion, once sent, suppresses the system notice for the rest of that session's life) matches the spec's intent even if the literal resume boundary isn't tracked.
- Produces: in `handle_merge_detection`, before building/sending `format_worker_done_reaction`/`format_worker_done_kept_alive_reaction`: if `session.orchestrator_id` is `Some(to)` and `store.has_completion_since(&session.id, to, session.started_at)` is `true`, skip the `send_to_session` call (log at `tracing::debug!` instead) but still perform the status/`merged_at`/cleanup side effects unchanged.
- Produces: in `sweep_retired_sessions`, the same check before `format_worker_retired_reaction`'s send.
- Produces: `deliver_work_requests`'s dedup is a no-op by design for this ticket (see Task 13's note — there is no second "system" announcement of a work request today to suppress); this task does not add behavior there beyond the `RequestForWork` tag already landed in Task 13.

- [ ] **Step 1: Write failing tests**:
  - `store.rs`: `has_completion_since_finds_a_matching_recent_completion`; `has_completion_since_ignores_a_completion_before_the_since_bound`; `has_completion_since_ignores_a_different_recipient`; `has_completion_since_ignores_a_non_completion_kind`.
  - `poller.rs`: `merge_detection_suppresses_the_done_reaction_after_a_completion_was_sent` (still cleans up/stamps, but asserts no `format_worker_done_reaction` text was sent — e.g. via a spy/fake `send_to_session` or by asserting the message log); `merge_detection_sends_the_done_reaction_with_no_prior_completion` (fallback case, unchanged behavior); `sweep_retired_sessions_suppresses_the_retired_notice_after_a_completion_was_sent`; `sweep_retired_sessions_sends_the_retired_notice_with_no_prior_completion`.
  - `messaging.rs` or `cli.rs` (wherever `Command::Send`'s dispatch is already tested): `an_unattested_send_claiming_kind_completion_is_not_recorded_as_that_sessions_completion` — deliver a `--kind completion` message where the claimed `from` does NOT verify against `sender_pane` (same setup `orchestrator_auth::attest_sender`'s own existing unverified-claim tests already use), then assert `store.has_completion_since(<claimed-from>, to, 0)` is `false` — the unattested send must record `from_id = NULL` (today's existing `annotate_with_attestation` behavior), never the claimed id, so a forged `--kind completion` can never suppress another session's real done/retired notice.

- [ ] **Step 2: Run tests, confirm failure.**

- [ ] **Step 3: Implement.**

- [ ] **Step 4: Run tests** (`cargo test --workspace`) — expect PASS, including the four scenarios the spec's own test list names (complete → exactly one completion to the parent; merge after a completion → no second notice; merge with no completion → the system notice is sent; a dead parent → the ancestor receives it — the last two already covered by Tasks 8/14's tests) plus the `--kind` round trip (Task 13).

- [ ] **Step 5: Commit**
```bash
git add crates/ninox-core/src/store.rs crates/ninox-core/src/lifecycle/poller.rs
git commit -m "fix(poller): suppress done/retired notices once a worker's own completion arrived"
```

---

## Task 16: Finish the skill text

**Files:**
- Modify: `crates/ninox-core/skills/orchestrator/spawn-worker.md`

- [ ] **Step 1:** Add a one-line mention of `ninox send --kind <kind>` to the "Messaging Workers" section (what kinds exist, when to use one over the default `freeform`).

- [ ] **Step 2:** Full gate once more:
```bash
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

- [ ] **Step 3: Commit**
```bash
git add crates/ninox-core/skills/orchestrator/spawn-worker.md
git commit -m "docs(skill): mention ninox send --kind"
```

**→ End of PR 2** (stacked on PR 1, or a second commit range in the same branch — confirm with the orchestrator which, per its go-ahead message, before opening it).
