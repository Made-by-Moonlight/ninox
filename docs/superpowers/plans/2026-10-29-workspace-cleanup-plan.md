# Unified Workspace Cleanup + Orphan Sweeper Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** One safety-checked `workspace::cleanup()` function backs every worktree teardown path (manual reap, auto-reap-on-merge, retention sweep), and a new orphan sweeper finds and safely cleans abandoned `.claude/worktrees/*` dirs across every repo ninox has spawned into.

**Architecture:** A new `ninox-core::workspace` module splits pure decision logic (`WorkspaceFacts` + `decide()`, fully unit-testable with synthetic facts) from IO (`gather_facts()`, `cleanup()`, both exercised with real git in tempdirs, matching the codebase's existing no-mocking test style). `Engine::cleanup_session`, `Engine::reap_workers`, and `Engine::remove_session` are refactored to gather facts and call the shared `cleanup()` instead of their own ad hoc tmux-kill + `remove_worker_worktree` sequences. A new orphan module reuses the same `WorkspaceFacts`/`decide()` pair, adding an ownership classifier that trusts a per-worktree marker file over a path/branch-name heuristic for worktrees that predate this ticket. The poller gains one more timer tick calling the same sweep function the future CLI slice will call on demand.

**Tech Stack:** Rust, rusqlite, tokio, existing `git` CLI shell-outs (no new dependencies).

**Spec:** `.claude/design-ref/fleet-overview-v1/BRIEF-MLOPS-5418-workspace-cleanup.md` (read `BRIEF-common.md` §7 first — process/PR rules only, no design content for this ticket).

## Global Constraints

- Base branch: `origin/v1`. PR targets `v1`, title exactly `MLOPS-5418: feat(core): unified workspace cleanup and orphan sweeper`.
- PR body: `## Why` / `## Summary` / `## Test plan`, concise.
- Never delete a worktree with uncommitted changes or unpushed commits unless the caller passes `CleanupPolicy { force: true }`. The user's `reap --force` is the only path that ever sets it; every automatic path (auto-reap-on-merge, retention sweep, orphan sweep) always passes `force: false`.
- Never touch an `Interrupted` session's worktree, and never touch a worktree with a live (non-terminal) session row — both gated by the same `force` flag as the dirty/unpushed rule (`reap --force` already overrides exactly this pair today for `ReapOutcome::ReapedLive`/`ReapedResumable`; this ticket must not regress that).
- `cleanup()` must be idempotent: calling it twice on an already-torn-down worktree/branch must not error.
- Only ever act on a path matching the exact `{repo}/.claude/worktrees/{name}` suffix (reuse the same strict suffix check `remove_worker_worktree` already uses — never a substring/`contains` check).
- The orphan sweeper must never run against the real store, the real `~/.claude/worktrees` tree, or real `AppConfig::load()` in tests — every orphan test builds its own tempdir repo(s) and in-memory/tempfile `Store`, per `BRIEF-common.md` §7's isolation rule.
- Do not implement `ninox complete`/`reap_requested` (MLOPS-5417) — that worker hadn't started as of this plan (confirmed: its branch is identical to `origin/v1`'s tip). Expose `workspace::cleanup()` as a public, stable entry point for it to call later; do not block on it.
- Do not run `cargo fmt` on the whole tree — only format files this plan touches, if at all.
- Branch deletion only ever targets the one branch explicitly tied to the worktree being cleaned (the session's recorded branch, or the ownership marker's recorded branch for an orphan) — never a repo's shared/default branch, and never an arbitrary `git branch -D` by name.
- This PR covers brief items 1, 2, 3 (minus CLI) and 7. CLI (`ninox workspaces`), shared build cache config, and GUI/TUI surfacing are out of scope — hand off via `ninox request-work` after this PR opens.

## Review Focus

- **Idempotent cleanup on a half-torn-down worktree** — worktree dir already removed by a previous crashed run but branch still present (or vice versa) — `cleanup()` must finish the remaining work without erroring. (Task 3)
- **A worktree `cleanup()`/the orphan sweeper must never touch** — a directory under `.claude/worktrees/` that isn't a ninox worktree at all (no marker, branch name doesn't match the directory name) — must come back `Ownership::NotOurs` and be skipped outright, never even fact-gathered for deletion. (Task 5)
- **No-upstream-but-never-committed-to branch** (the brief's `no-upstream-empty` safety-matrix case) must be treated as *safe* to delete, while **no-upstream-with-local-commits** must be treated as *unsafe* — these are easy to conflate into one "no upstream → unsafe" rule, which would wrongly block the `pr-227-review-r3`-style clean-but-unpushed-to-nowhere worktrees the brief's evidence calls out. (Task 1)
- **Branch deletion must never fire when there is no branch to delete** (detached HEAD, or the worktree's branch happens to equal the repo's own default branch) — `decide()` must require `has_branch` before ever considering deletion, and callers must only ever pass the session- or marker-recorded branch, never a derived "current branch of HEAD". (Task 1, Task 3)
- **One unreadable/missing repo must not abort the whole orphan sweep** — a repo root that's been deleted since its last session spawned, or a `.claude/worktrees` dir with a permission error, must be logged and skipped, leaving every other repo's sweep unaffected. (Task 6)

---

### Task 1: Pure safety/branch-deletion decision logic

**Files:**
- Create: `crates/ninox-core/src/workspace/mod.rs`
- Create: `crates/ninox-core/src/workspace/policy.rs`
- Modify: `crates/ninox-core/src/lib.rs` (add `pub mod workspace;` alongside the existing module list)

**Interfaces:**
- Produces (used by every later task):
  ```rust
  pub struct WorkspaceFacts {
      pub dirty: bool,
      pub has_branch: bool,
      pub has_upstream: bool,
      pub unpushed_commit_count: u32,   // commits on branch not in its upstream; meaningless if !has_upstream
      pub commits_beyond_base: u32,     // commits on branch not in the repo's base/default ref
      pub merged: bool,                 // true only when known-merged (e.g. Session::merged_at.is_some())
      pub interrupted: bool,
      pub live_session_row: bool,
  }
  pub struct CleanupPolicy { pub force: bool }
  #[derive(Debug, Clone, Copy, PartialEq)]
  pub enum SkipReason { LiveSessionRow, Interrupted, Dirty, UnpushedCommits }
  #[derive(Debug, Clone, Copy, PartialEq)]
  pub enum CleanupDecision { Proceed { delete_branch: bool }, Skip(SkipReason) }
  pub fn decide(facts: &WorkspaceFacts, policy: CleanupPolicy) -> CleanupDecision
  ```

- [ ] **Step 1: Write the failing tests** in `crates/ninox-core/src/workspace/policy.rs` under `#[cfg(test)] mod tests`, one per safety-matrix row plus the branch-deletion rules and the Review Focus `no-upstream-empty` case:
  ```rust
  fn facts(overrides: impl FnOnce(&mut WorkspaceFacts)) -> WorkspaceFacts { /* all-false/zero base, apply overrides */ }

  #[test] fn dirty_worktree_is_skipped_without_force() { /* facts(|f| f.dirty = true); assert_eq!(decide(&f, CleanupPolicy{force:false}), CleanupDecision::Skip(SkipReason::Dirty)); */ }
  #[test] fn dirty_worktree_proceeds_with_force() { /* same facts, force:true => Proceed */ }
  #[test] fn unpushed_commits_with_upstream_is_skipped_without_force() { /* has_upstream:true, unpushed_commit_count:1 */ }
  #[test] fn no_upstream_with_commits_beyond_base_is_skipped_without_force() { /* has_upstream:false, commits_beyond_base:3 */ }
  #[test] fn no_upstream_and_empty_is_safe_to_proceed() { /* has_upstream:false, commits_beyond_base:0, dirty:false => Proceed{delete_branch:true} since has_branch:true, merged:false, commits_beyond_base:0 */ }
  #[test] fn interrupted_is_skipped_without_force() {}
  #[test] fn interrupted_proceeds_with_force() {}
  #[test] fn live_session_row_is_skipped_without_force() {}
  #[test] fn live_session_row_proceeds_with_force() {}
  #[test] fn merged_branch_is_deleted() { /* merged:true, commits_beyond_base: 5 (squash-merge case) => Proceed{delete_branch:true} */ }
  #[test] fn unmerged_branch_with_commits_beyond_base_is_kept() { /* merged:false, commits_beyond_base:2, otherwise clean => Proceed{delete_branch:false} */ }
  #[test] fn no_branch_never_deletes() { /* has_branch:false => Proceed{delete_branch:false} regardless of merged/commits_beyond_base */ }
  #[test] fn idempotent_decision_is_pure() { /* calling decide() twice on identical facts gives identical results — guards against any hidden mutable state creeping in later */ }
  ```

- [ ] **Step 2: Run tests to verify they fail**

  Run: `cargo test -p ninox-core workspace::policy -- --nocapture`
  Expected: FAIL (module/function not defined).

- [ ] **Step 3: Implement `WorkspaceFacts`, `CleanupPolicy`, `SkipReason`, `CleanupDecision`, `decide()`** in `crates/ninox-core/src/workspace/policy.rs`.

  `decide()`'s logic: if `!policy.force`, return `Skip(LiveSessionRow)` if `facts.live_session_row`; else `Skip(Interrupted)` if `facts.interrupted`; else `Skip(Dirty)` if `facts.dirty`; else compute `unsafe_unpushed = if facts.has_upstream { facts.unpushed_commit_count > 0 } else { facts.commits_beyond_base > 0 }` and return `Skip(UnpushedCommits)` if that's true. Otherwise (force, or all four checks passed), return `Proceed { delete_branch: facts.has_branch && (facts.merged || facts.commits_beyond_base == 0) }`.

  Create `crates/ninox-core/src/workspace/mod.rs` with `mod policy; pub use policy::*;`. Add `pub mod workspace;` to `crates/ninox-core/src/lib.rs`'s module list.

- [ ] **Step 4: Run tests to verify they pass**

  Run: `cargo test -p ninox-core workspace::policy -- --nocapture`
  Expected: PASS, all cases above green.

- [ ] **Step 5: Commit**

  ```bash
  git add crates/ninox-core/src/workspace/mod.rs crates/ninox-core/src/workspace/policy.rs crates/ninox-core/src/lib.rs
  git commit -m "feat(core): add pure workspace cleanup safety-decision logic"
  ```

---

### Task 2: Git fact-gathering and the ownership marker file

**Files:**
- Create: `crates/ninox-core/src/workspace/facts.rs`
- Create: `crates/ninox-core/src/workspace/ownership.rs`
- Modify: `crates/ninox-core/src/workspace/mod.rs` (re-export)

**Interfaces:**
- Consumes: `WorkspaceFacts` (Task 1).
- Produces:
  ```rust
  // facts.rs
  /// Shells out to `git -C repo_root` to fill in the git-derived fields of
  /// `WorkspaceFacts` (dirty, has_branch, has_upstream, unpushed_commit_count,
  /// commits_beyond_base). `base_ref` is the ref to diff against for
  /// `commits_beyond_base` — callers resolve it once per repo (e.g.
  /// `origin/HEAD`) and pass it in, so this function never discovers it
  /// itself. The session/store-derived fields (merged, interrupted,
  /// live_session_row) are left at their input defaults — callers set those
  /// separately from what they already know.
  pub fn gather_git_facts(repo_root: &Path, worktree_path: &Path, branch: Option<&str>, base_ref: Option<&str>) -> WorkspaceFacts

  /// `git -C repo_root rev-parse --abbrev-ref origin/HEAD`, trimmed; `None`
  /// if unresolvable (no `origin` remote, or `origin/HEAD` never set).
  pub fn resolve_default_base_ref(repo_root: &Path) -> Option<String>

  // ownership.rs
  #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
  pub struct WorkspaceMarker { pub session_id: String, pub repo: String, pub branch: String, pub created_at: i64 }
  pub const MARKER_FILENAME: &str = ".ninox-workspace.json";

  /// Writes the marker JSON at `worktree_path/.ninox-workspace.json` and
  /// git-excludes it via `spawn_util`'s shared `git_exclude_add` — called
  /// from `create_worktree_at` in Task 4. Lives in `ninox-core` (not
  /// `ninox-ui-shared`) so the orphan sweeper, also in `ninox-core`, can read
  /// it without a crate-dependency inversion; takes the exclude step as an
  /// injected closure so this module has no dependency on `ninox-ui-shared`.
  pub fn write_marker(worktree_path: &Path, marker: &WorkspaceMarker) -> anyhow::Result<()>
  pub fn read_marker(worktree_path: &Path) -> Option<WorkspaceMarker>
  ```

- [ ] **Step 1: Write the failing tests**, real git + tempdir, matching the existing `events.rs:748-791` idiom (local `run_git` closure, `tempdir()`, no mocking):
  ```rust
  // facts.rs tests
  #[test] fn clean_pushed_branch_has_no_unsafe_facts() { /* init repo, commit, branch, push to a bare remote tempdir, assert dirty=false, has_upstream=true, unpushed_commit_count=0 */ }
  #[test] fn dirty_worktree_is_detected() { /* write an untracked file, assert dirty=true */ }
  #[test] fn ninox_managed_files_do_not_count_as_dirty() { /* write .claude/settings.json only, assert dirty=false — reuses the same NINOX_MANAGED_PREFIXES convention fleet::probe already applies; this test documents that gather_git_facts must not regress it */ }
  #[test] fn unpushed_commit_is_detected() { /* commit after push, assert has_upstream=true, unpushed_commit_count=1 */ }
  #[test] fn no_upstream_branch_reports_commits_beyond_base() { /* branch off base with 2 commits, never pushed, assert has_upstream=false, commits_beyond_base=2 */ }
  #[test] fn resolve_default_base_ref_reads_origin_head() { /* bare remote, `git remote set-head origin -a`, assert Some("origin/<branch>") */ }
  #[test] fn resolve_default_base_ref_none_without_origin() { /* no remote configured, assert None */ }

  // ownership.rs tests
  #[test] fn marker_round_trips() { /* write_marker then read_marker, assert equal */ }
  #[test] fn missing_marker_reads_as_none() {}
  #[test] fn corrupt_marker_reads_as_none() { /* write garbage JSON, read_marker must not panic, returns None */ }
  ```

- [ ] **Step 2: Run tests to verify they fail**

  Run: `cargo test -p ninox-core workspace::facts workspace::ownership -- --nocapture`
  Expected: FAIL.

- [ ] **Step 3: Implement `gather_git_facts`, `resolve_default_base_ref`** in `facts.rs` using `git -C <repo_root> status --porcelain --untracked-files=all` (filtered through the same `has_user_changes`/`NINOX_MANAGED_PREFIXES` approach as `fleet::probe` — import and reuse `ninox_core::fleet::probe`'s existing filter rather than duplicating it, exposing it as `pub(crate)` from `probe.rs` if it isn't already), `git -C <worktree_path> rev-parse --abbrev-ref --symbolic-full-name <branch>@{u}` (presence → `has_upstream`), `git rev-list --count <branch>@{u}..<branch>` (→ `unpushed_commit_count`), and `git rev-list --count <base_ref>..<branch>` (→ `commits_beyond_base`, `0` when `base_ref` is `None`). All git calls are best-effort: a failed/missing command leaves the conservative default (e.g. `has_upstream: false`, counts `0` only when that's actually safe per Task 1's logic — reread Task 1's `no_upstream_and_empty_is_safe` case before picking defaults, and prefer the *unsafe* default, e.g. treat a failed "count commits beyond base" as a large nonzero sentinel, when a git call errors rather than cleanly reporting zero).

  Implement `write_marker`/`read_marker` in `ownership.rs` with `serde_json` — run `cargo tree -p ninox-core -i serde_json` first; if it's not already a dependency (only `serde`'s derive macros are confirmed in use by `fleet/probe.rs`), add `serde_json = { workspace = true }` (or match whatever other crates in this workspace use) to `crates/ninox-core/Cargo.toml` before implementing. `write_marker` does not itself call `git_exclude_add` (that's `ninox-ui-shared`'s job in Task 4); it only writes the file.

- [ ] **Step 4: Run tests to verify they pass**

  Run: `cargo test -p ninox-core workspace::facts workspace::ownership -- --nocapture`
  Expected: PASS.

- [ ] **Step 5: Commit**

  ```bash
  git add crates/ninox-core/src/workspace/facts.rs crates/ninox-core/src/workspace/ownership.rs crates/ninox-core/src/workspace/mod.rs
  git commit -m "feat(core): gather git-derived workspace facts and read/write ownership markers"
  ```

---

### Task 3: The unified `cleanup()` function and its call sites

**Files:**
- Create: `crates/ninox-core/src/workspace/cleanup.rs`
- Modify: `crates/ninox-core/src/workspace/mod.rs` (re-export)
- Modify: `crates/ninox-core/src/store.rs` (new `cleanup_result`/`cleanup_at` columns + narrow setter)
- Modify: `crates/ninox-core/src/types.rs` (`Session` gains `cleanup_result: Option<String>`, `cleanup_at: Option<i64>`)
- Modify: every existing `Session { .. }` struct-literal site workspace-wide (mechanical — grep `Session {` and add the two new fields as `None`)
- Modify: `crates/ninox-core/src/events.rs` (`cleanup_session_in`, `reap_workers`, `remove_session` call the new `cleanup()`)

**Interfaces:**
- Consumes: `WorkspaceFacts`, `CleanupPolicy`, `CleanupDecision`, `decide()` (Task 1); `gather_git_facts` (Task 2, used by callers, not by `cleanup()` itself — `cleanup()` takes facts as an input so it stays IO-decision-separated and so orphan cleanup, which gathers facts differently, can reuse it unchanged).
- Produces:
  ```rust
  pub struct CleanupOutcome {
      pub worktree_removed: bool,
      pub branch_deleted: Option<String>,
      pub artifacts_removed: bool,
      pub skipped: Option<SkipReason>,
  }
  /// The one cleanup function. Best-effort tmux kill, conditional
  /// `git worktree remove --force` (only reached once `decide()` says
  /// Proceed — the `--force` on the git command is unconditional because by
  /// the time we're removing at all, the policy-level safety check has
  /// already run; it mirrors today's `remove_worker_worktree`), conditional
  /// `git branch -D`, `git worktree prune`, `hooks::remove_session_artifacts`.
  /// Idempotent: a missing worktree dir / already-deleted branch are treated
  /// as already-done, not errors.
  pub async fn cleanup(
      session_id: &str,
      workspace_path: &str,
      branch: Option<&str>,
      facts: WorkspaceFacts,
      policy: CleanupPolicy,
      sessions_dir: &Path,
  ) -> anyhow::Result<CleanupOutcome>
  ```
  - New `Store` method: `pub fn set_cleanup_result(&self, session_id: &str, outcome: &CleanupOutcome) -> anyhow::Result<()>` — narrow `UPDATE sessions SET cleanup_result=?, cleanup_at=? WHERE id=?` (never a full-row upsert, so it can't race `update_live_session_row`'s read-apply-write elsewhere). `cleanup_result` is a short human string, e.g. `"worktree removed, branch deleted"` / `"skipped: dirty"`.

- [ ] **Step 1: Write the failing tests** in `crates/ninox-core/src/workspace/cleanup.rs`, real git + tempdir (same idiom as Task 2):
  ```rust
  #[tokio::test] async fn proceed_removes_worktree_and_deletes_merged_branch() { /* real repo+worktree+branch, facts with merged:true, clean; assert worktree gone, branch gone, outcome.worktree_removed && outcome.branch_deleted == Some(branch) */ }
  #[tokio::test] async fn proceed_keeps_unmerged_branch_with_commits() { /* commits_beyond_base:1, merged:false; assert worktree gone but branch still exists, outcome.branch_deleted == None */ }
  #[tokio::test] async fn skip_dirty_without_force_leaves_everything() { /* facts.dirty=true, policy.force=false; assert worktree still exists, outcome.skipped == Some(SkipReason::Dirty) */ }
  #[tokio::test] async fn force_removes_dirty_worktree() { /* same facts, policy.force=true; assert worktree removed */ }
  #[tokio::test] async fn idempotent_second_call_on_already_removed_worktree_is_a_noop_ok() { /* call cleanup() twice with the same clean-mergeable facts; second call must return Ok with worktree_removed:false (nothing left to remove) rather than erroring */ }
  #[tokio::test] async fn removes_artifacts_alongside_worktree() { /* sessions_dir fixture with a {session_id}.json file, assert it's gone after cleanup() and outcome.artifacts_removed */ }
  #[tokio::test] async fn non_ninox_path_is_never_touched() { /* workspace_path not matching the `.claude/worktrees/{id}` suffix; assert cleanup() returns worktree_removed:false without running any git command against it — reuse remove_worker_worktree's exact suffix check */ }

  // store.rs test
  #[test] fn set_cleanup_result_is_a_narrow_update() { /* write a session row, call set_cleanup_result, assert only cleanup_result/cleanup_at changed and every other column (e.g. status) is untouched — proves it's not a full upsert */ }
  ```

- [ ] **Step 2: Run tests to verify they fail**

  Run: `cargo test -p ninox-core workspace::cleanup -- --nocapture` and `cargo test -p ninox-core store:: -- --nocapture`
  Expected: FAIL.

- [ ] **Step 3a: Implement `cleanup()`** in `workspace/cleanup.rs`. Reuse (don't duplicate) the existing strict-suffix / repo-root derivation and the `git worktree remove --force`, `hooks::remove_session_artifacts` calls currently inline in `events.rs`'s `remove_worker_worktree`/`remove_worktree_and_artifacts` — move that logic here (or have `events.rs` call into this module; pick whichever leaves `events.rs` smaller, since it's already a large file per the project's "files that change together" guidance) so there is exactly one implementation. Add `git -C <repo_root> branch -D <branch>` only when `decide()` returned `Proceed { delete_branch: true }`; swallow a "branch not found" failure as already-done (idempotency). Add `git -C <repo_root> worktree prune` after the remove.

- [ ] **Step 3b: Add `cleanup_result`/`cleanup_at` columns** to `store.rs`'s migration list (same `ALTER TABLE sessions ADD COLUMN ...` pattern as the existing entries at store.rs:86-106) and implement `Store::set_cleanup_result`.

- [ ] **Step 3c: Add `cleanup_result: Option<String>`, `cleanup_at: Option<i64>`** to `Session` in `types.rs`, and mechanically update every existing `Session { .. }` literal in the workspace to set both to `None` (run `cargo build --workspace` and `cargo test --workspace --no-run` repeatedly to find every remaining site via compile errors — faster and more reliable than grepping, since the compiler can't miss one).

- [ ] **Step 3d: Rewire the three call sites** in `events.rs`:
  - `cleanup_session_in`: after the existing `get_session` lookup, build `WorkspaceFacts` from that `Session` (`merged: false` — `cleanup_session` always runs on the auto-reap-on-merge path per `handle_merge_detection`'s `else` branch, which only reaches here when the merge was just detected, i.e. this *is* the merge event, so set `merged: true`; `interrupted`/`live_session_row: false`, since this path is only reached for the session whose merge was just observed, not a general kill), gather git facts via Task 2's `gather_git_facts` (resolve `base_ref` via `resolve_default_base_ref` once), call `cleanup()` with `CleanupPolicy { force: false }`, then call `store.set_cleanup_result`.
  - `reap_workers`: replace the `remove_worker_worktree(wp, &session.id).await` call with a `cleanup()` call carrying `CleanupPolicy { force }` (the same `force` the caller already threaded through) and facts built from `interrupted`/`was_live` (→ `live_session_row: was_live`) already computed a few lines above — do not duplicate that classification, just read it into `WorkspaceFacts`. Keep every existing status-transition/`ReapOutcome` line untouched; only the worktree-teardown call changes.
  - `remove_session`: same shape as `cleanup_session_in`, with `merged: session.merged_at.is_some()`, `CleanupPolicy { force: true }` (this path is reached by direct user deletion or the retention sweep's fallback purge of an already-terminal/already-notified record — both are intentional, final cleanups, so always force; the retention sweep itself already gates *which* sessions reach this call via its own terminal/expired checks in `sweep_retired_sessions`, unchanged by this task).

- [ ] **Step 4: Run tests to verify they pass**

  Run: `cargo test -p ninox-core -- --nocapture`
  Expected: PASS, including the three rewired `events.rs` tests from the existing suite (`cleanup_session_sets_done_status`, `cleanup_session_removes_worktree_and_artifacts`, and the `reap_workers`/`removing_an_orchestrator_...` tests) still passing unmodified — if any need updating because their fixture `Session` literals now need the two new fields, that's expected churn from Step 3c, not a logic change.

- [ ] **Step 5: Commit**

  ```bash
  git add crates/ninox-core/src/workspace/cleanup.rs crates/ninox-core/src/workspace/mod.rs crates/ninox-core/src/store.rs crates/ninox-core/src/types.rs crates/ninox-core/src/events.rs
  git commit -m "feat(core): route cleanup_session, reap_workers and remove_session through workspace::cleanup"
  ```

  (If Step 3c's mechanical literal updates touch files outside `ninox-core` — e.g. `ninox-ui-shared`, `ninox-tui`, `ninox-gui` test fixtures — commit those in the same commit; they're inseparable from the field addition.)

---

### Task 4: Write the ownership marker at worktree-creation time

**Files:**
- Modify: `crates/ninox-ui-shared/src/spawn_util.rs` (`create_worktree_at`)

**Interfaces:**
- Consumes: `ninox_core::workspace::{WorkspaceMarker, write_marker}` (Task 2); `git_exclude_add` (existing, `spawn_util.rs:742`).

- [ ] **Step 1: Write the failing test** in `spawn_util.rs`'s existing test module, following its established real-git-tempdir pattern (e.g. near its other `create_worktree_at` tests):
  ```rust
  #[test] fn create_worktree_at_writes_and_excludes_ownership_marker() {
      // real bare-ish repo + create_worktree_at(...), then:
      // assert the marker file exists at target.join(".ninox-workspace.json")
      // assert its session_id/repo/branch match what was passed in
      // assert git_exclude_add was applied: read repo_root/.git/info/exclude and assert it contains ".ninox-workspace.json"
  }
  ```

- [ ] **Step 2: Run test to verify it fails**

  Run: `cargo test -p ninox-ui-shared create_worktree_at_writes_and_excludes_ownership_marker -- --nocapture`
  Expected: FAIL.

- [ ] **Step 3: Implement** — in `create_worktree_at` (`spawn_util.rs:525-575`), right after each successful `ensure_statusline_settings` call (both the fresh-branch and existing-branch success paths, mirroring how that function is already called twice), build a `WorkspaceMarker { session_id: branch.to_string(), repo: repo_str.clone(), branch: branch.to_string(), created_at: now_millis() }` (branch name == session id by the existing convention this module already relies on), call `ninox_core::workspace::write_marker(target, &marker)`, then `git_exclude_add(target, ninox_core::workspace::MARKER_FILENAME)`. Both are best-effort (log via `tracing::warn!` and continue) — a marker write failure must never fail the spawn itself.

- [ ] **Step 4: Run test to verify it passes**

  Run: `cargo test -p ninox-ui-shared create_worktree_at_writes_and_excludes_ownership_marker -- --nocapture`
  Expected: PASS.

- [ ] **Step 5: Commit**

  ```bash
  git add crates/ninox-ui-shared/src/spawn_util.rs
  git commit -m "feat(spawn): record an ownership marker in every worker worktree at creation"
  ```

---

### Task 5: Orphan classification and discovery

**Files:**
- Create: `crates/ninox-core/src/workspace/orphan.rs`
- Modify: `crates/ninox-core/src/workspace/mod.rs` (re-export)
- Modify: `crates/ninox-core/src/config.rs` (`OrphanSweepConfig`, wired into `AppConfig`)

**Interfaces:**
- Consumes: `WorkspaceFacts`, `CleanupPolicy`, `decide()`, `cleanup()` (Tasks 1/3); `WorkspaceMarker`/`read_marker` (Task 2); `Store` (`list_sessions`, `sessions_by_orchestrator` or equivalent — read-only).
- Produces:
  ```rust
  #[derive(Debug, Clone, Default, Serialize, Deserialize)]
  pub struct OrphanSweepConfig {
      #[serde(default = "default_orphan_sweep_enabled")] pub enabled: bool,
      #[serde(default = "default_orphan_sweep_interval_hours")] pub interval_hours: u64,
  }
  // default_orphan_sweep_enabled() -> true, default_orphan_sweep_interval_hours() -> 1,
  // following the exact AutoReapConfig pattern at config.rs:386-400 (free fn defaults + manual Default impl).

  #[derive(Debug, Clone, Copy, PartialEq)]
  pub enum Ownership { Tracked, LegacyMatch, NotOurs }
  #[derive(Debug, Clone, Copy, PartialEq)]
  pub enum OrphanStatus { Live, NotOrphanYet, NotOurs, Orphan(Ownership) }

  /// Pure: classifies one `.claude/worktrees/<dir_name>` entry given what's
  /// already known about it. `session_row` is `Some` only if a session row's
  /// `workspace_path` still points at this exact directory (live or
  /// terminal-but-not-yet-expired); `now`/`retention_ms` gate the
  /// terminal-but-not-expired case.
  pub fn classify_worktree(
      dir_name: &str, git_branch: Option<&str>, marker: Option<&WorkspaceMarker>,
      session_row: Option<&Session>, now: i64, retention_ms: i64,
  ) -> OrphanStatus

  pub struct SweepAction { pub path: PathBuf, pub status: OrphanStatus, pub outcome: Option<CleanupOutcome> /* None when not touched */ }
  pub struct SweepReport { pub actions: Vec<SweepAction> }

  /// Read-only: discovers every `.claude/worktrees/*` entry under
  /// `repo_root`, classifies each (`classify_worktree`) and gathers its
  /// `WorkspaceFacts`, and returns the full picture. Never calls `cleanup()`
  /// and never deletes anything — a sweeper whose only entry point can
  /// delete is a safety gap, not a feature, so this is the one function the
  /// future CLI `list`/`show` and any dry-run path call directly.
  pub async fn classify_repo_orphans(
      store: &Store, repo_root: &Path, retention_ms: i64,
  ) -> anyhow::Result<Vec<(PathBuf, OrphanStatus, WorkspaceFacts)>>

  /// One repo's worth of orphan discovery + action: calls
  /// `classify_repo_orphans` first, then for each `Orphan(ownership)` entry
  /// (applying the extra `LegacyMatch` conservatism — see Step 3d) calls
  /// `cleanup()`. Used by both the poller (Task 6) and, later, the CLI
  /// slice's on-demand prune trigger — this is the single "scan + act"
  /// entry point the brief's "periodically ... and on demand" requirement
  /// refers to.
  pub async fn sweep_repo_orphans(store: &Store, repo_root: &Path, retention_ms: i64) -> anyhow::Result<SweepReport>

  /// Every distinct repo root implied by `{workspace_path}`s ending in
  /// `/.claude/worktrees/<name>` across all known session rows.
  pub fn discover_repo_roots(store: &Store) -> anyhow::Result<Vec<PathBuf>>
  ```

- [ ] **Step 1: Write the failing tests** in `orphan.rs`:
  ```rust
  // classify_worktree — pure, no IO
  #[test] fn live_session_row_is_live() { /* session_row Some with non-terminal status => OrphanStatus::Live */ }
  #[test] fn terminal_row_within_retention_is_not_orphan_yet() { /* terminal_at recent => NotOrphanYet */ }
  #[test] fn terminal_row_past_retention_is_tracked_orphan() { /* terminal_at old, no marker needed since row still exists => Orphan(Tracked) */ }
  #[test] fn no_row_with_marker_is_tracked_orphan() { /* session_row None, marker Some matching dir_name => Orphan(Tracked) */ }
  #[test] fn no_row_no_marker_branch_matches_dirname_is_legacy_orphan() { /* git_branch == Some(dir_name) => Orphan(LegacyMatch) */ }
  #[test] fn no_row_no_marker_branch_mismatch_is_not_ours() { /* git_branch == Some("some-human-branch") => NotOurs */ }
  #[test] fn no_row_no_marker_no_branch_is_not_ours() { /* git_branch: None (detached/not git) => NotOurs */ }

  // classify_repo_orphans — read-only; real git + tempdir repo(s) + tempfile Store
  #[tokio::test] async fn classify_never_deletes_a_safe_orphan() { /* a clean, pushed, zero-commits-beyond-base Tracked orphan — the kind sweep_repo_orphans WOULD clean — still exists on disk after classify_repo_orphans returns, and the returned Vec reports it as Orphan(Tracked) with clean facts */ }
  #[tokio::test] async fn classify_reports_facts_for_every_entry_including_live_and_not_ours() { /* one live, one orphan, one not-ours dir => 3 entries back, each with its OrphanStatus and WorkspaceFacts, nothing deleted */ }

  // sweep_repo_orphans — real git + tempdir repo(s) + tempfile Store, per BRIEF-common.md isolation rule
  #[tokio::test] async fn clean_pushed_tracked_orphan_is_auto_cleaned() {}
  #[tokio::test] async fn dirty_tracked_orphan_is_needs_attention_not_deleted() { /* assert worktree still exists, outcome.skipped == Some(Dirty) recorded in the report */ }
  #[tokio::test] async fn legacy_orphan_with_commits_beyond_base_is_needs_attention_even_if_pushed() { /* LegacyMatch requires squeaky-clean: commits_beyond_base must be 0, not just "pushed" */ }
  #[tokio::test] async fn legacy_orphan_squeaky_clean_is_auto_cleaned() {}
  #[tokio::test] async fn not_ours_directory_is_never_touched() { /* a worktree with an unrelated branch name; assert it still exists and no SweepAction targeted it for cleanup */ }
  #[tokio::test] async fn interrupted_session_row_is_never_swept() { /* session_row Some, status Interrupted, terminal_at irrelevant (Interrupted isn't a terminal status) => Live, never reached as Orphan */ }
  #[tokio::test] async fn sweep_never_runs_against_a_real_default_appconfig_or_home_dir() { /* sanity: sweep_repo_orphans takes explicit repo_root/Store params, so this test just asserts the signature has no hidden global-state reads — documents the isolation constraint for reviewers */ }

  // discover_repo_roots
  #[test] fn discovers_distinct_repo_roots_from_workspace_paths() { /* two sessions in repo A, one in repo B, one non-worktree workspace => returns {A, B} exactly once each */ }
  ```

- [ ] **Step 2: Run tests to verify they fail**

  Run: `cargo test -p ninox-core workspace::orphan -- --nocapture`
  Expected: FAIL.

- [ ] **Step 3a: Add `OrphanSweepConfig`** to `config.rs`, following `AutoReapConfig`'s exact shape (config.rs:386-400) and wiring it into `AppConfig` (struct field + `Default::default()`), placed before `harnesses` per the project CLAUDE.md's "scalars before tables" rule.

- [ ] **Step 3b: Implement `classify_worktree`** (pure, per the Interfaces block's exact precedence: `Live` → `NotOrphanYet` → marker/branch → `NotOurs`).

- [ ] **Step 3c: Implement `discover_repo_roots`** — for each session's `workspace_path`, strip the `/.claude/worktrees/{id}` suffix (reuse the same strict-suffix helper as Task 3/`remove_worker_worktree`); collect into a `BTreeSet<PathBuf>` for determinism; skip paths that don't match.

- [ ] **Step 3d: Implement `classify_repo_orphans`** (read-only) — list `repo_root/.claude/worktrees/*` dirs; for each, look up a matching session row (by `workspace_path`), read the marker (Task 2), read the git branch (reuse `fleet::probe::current_branch`), call `classify_worktree`, then gather `WorkspaceFacts` via Task 2's `gather_git_facts` for every entry (not just orphans — `classify_repo_orphans` is purely observational, so `Live`/`NotOrphanYet`/`NotOurs` entries get facts too, cheap since it's one `git status`/`rev-list` pass per directory). Return the full `Vec<(PathBuf, OrphanStatus, WorkspaceFacts)>`; this function must contain no call to `cleanup()` anywhere in its body.

- [ ] **Step 3e: Implement `sweep_repo_orphans`** — call `classify_repo_orphans` first; for each `Orphan(ownership)` entry in its result, additionally require, for `LegacyMatch` only, `facts.commits_beyond_base == 0` before calling `cleanup()` at all (when that extra check fails, record the action as skipped/needs-attention without calling `cleanup()`, so a `LegacyMatch` branch is never even offered the "merged" leniency `Tracked` gets); call `cleanup()` with `CleanupPolicy { force: false }` always for the entries that do qualify. Collect every entry (Live/NotOrphanYet/NotOurs/Orphan, touched or not) into the returned `SweepReport` so callers can log or display the full picture, not just the entries that were cleaned.

- [ ] **Step 4: Run tests to verify they pass**

  Run: `cargo test -p ninox-core workspace::orphan -- --nocapture` and `cargo test -p ninox-core config:: -- --nocapture`
  Expected: PASS.

- [ ] **Step 5: Commit**

  ```bash
  git add crates/ninox-core/src/workspace/orphan.rs crates/ninox-core/src/workspace/mod.rs crates/ninox-core/src/config.rs
  git commit -m "feat(core): classify and sweep orphaned worktrees"
  ```

---

### Task 6: Wire the orphan sweep into the poller, with logging

**Files:**
- Modify: `crates/ninox-core/src/lifecycle/poller.rs` (`start()`'s timer loop)

**Interfaces:**
- Consumes: `workspace::{discover_repo_roots, sweep_repo_orphans}`, `AppConfig::orphan_sweep` (Task 5).

- [ ] **Step 1: Write the failing test** in `poller.rs`'s existing test module, following its established fixture style (e.g. near `sweep_retired_sessions`'s own tests):
  ```rust
  #[tokio::test]
  async fn orphan_sweep_tick_logs_and_does_not_touch_other_repos_on_one_repo_error() {
      // Two repo roots registered via session rows: one real tempdir repo
      // with an orphaned worktree, one repo root that's been deleted from
      // disk. Assert the real repo's orphan still gets swept/classified
      // despite the missing repo not erroring the whole call — i.e. the
      // function this test calls per repo must isolate failures (Review
      // Focus item 5).
  }
  ```
  (This is a function/integration-level test, not a full `Poller::start()` loop test — add a small `async fn run_orphan_sweep(&self, cfg: &OrphanSweepConfig)` method on `Poller` that Step 3 wires into the `select!` loop, and test that method directly, the same way `sweep_retired_sessions` itself is a directly-callable, directly-tested method rather than only reachable through `start()`.)

- [ ] **Step 2: Run test to verify it fails**

  Run: `cargo test -p ninox-core poller::tests::orphan_sweep_tick -- --nocapture`
  Expected: FAIL (method doesn't exist).

- [ ] **Step 3: Implement** `async fn run_orphan_sweep(&self, cfg: &OrphanSweepConfig)` on `Poller`: early-return if `!cfg.enabled`; call `workspace::discover_repo_roots(&self.engine.store)`, logging+continuing on error rather than propagating (matches `sweep_retired_sessions`'s `let Ok(..) = .. else { return }` style, but per-repo here rather than whole-sweep, per Review Focus item 5 — wrap each repo's `sweep_repo_orphans` call so one repo's failure doesn't stop the loop over the rest); for each repo, call `workspace::sweep_repo_orphans(&self.engine.store, &repo_root, retention_ms)` (retention from `AppConfig::load().session_retention`, matching `sweep_retired_sessions`'s own source of truth) and `tracing::info!`/`tracing::warn!` one line per `SweepAction` (cleaned vs. needs-attention vs. skipped), satisfying brief item 7's "sweeper logs each action" and "cleanup failures ... surface as a needs-attention entry, not silent leftovers" (a `cleanup()` call that returns `Err` or an outcome with `skipped.is_some()` logs at `warn!`, everything else at `info!`).

  Add a new `tokio::time::interval(Duration::from_secs(cfg.interval_hours * 3600))` tick to `start()`'s `tokio::select!` (`poller.rs:448-477`), reloading `AppConfig::load().unwrap_or_default().orphan_sweep` fresh each tick (matching the existing `github_interval`/`pid_interval` pattern of reloading config per-tick rather than once at `start()`), calling `self.run_orphan_sweep(&cfg).await`.

- [ ] **Step 4: Run tests to verify they pass**

  Run: `cargo test -p ninox-core -- --nocapture`
  Expected: PASS, full `ninox-core` suite green.

- [ ] **Step 5: Commit**

  ```bash
  git add crates/ninox-core/src/lifecycle/poller.rs
  git commit -m "feat(core): run the orphan sweep on a configurable poller tick"
  ```

---

### Task 7: Whole-workspace verification

**Files:**
- Create: `crates/ninox-core/src/workspace/orphan.rs` — one additional `#[test]` (no new production code).

- [ ] **Step 0: Produce a `workspaces list --dry-run`-style sample run for the final report.** The brief's Report section asks for this even though the real `ninox workspaces` CLI is out of scope for this PR — satisfy it with a test, not new CLI surface. Add a test to `orphan.rs`:
  ```rust
  #[tokio::test]
  async fn sample_run_prints_a_list_style_report() {
      // Build a scratch fixture: 2 repos, each with a tempdir Store containing
      // a mix of a live session, a clean pushed Tracked orphan, a dirty
      // Tracked orphan, and a NotOurs directory. Call sweep_repo_orphans on
      // each, then println! one line per SweepAction in roughly
      // `<path>  <status>  <action-taken>` form (status/action from
      // OrphanStatus/Option<CleanupOutcome> — exact column format is this
      // test's own choice, it's a report aid, not a stable CLI contract).
  }
  ```
  Run it with `cargo test -p ninox-core sample_run_prints_a_list_style_report -- --nocapture` and keep the captured stdout — paste it into the PR/report as the brief's requested sample run.

- [ ] **Step 1:** `cargo build --workspace`
  Expected: builds clean.
- [ ] **Step 2:** `cargo test --workspace`
  Expected: all green, including every pre-existing test whose `Session` literal was mechanically updated in Task 3.
- [ ] **Step 3:** `cargo clippy --workspace --all-targets -- -D warnings`
  Expected: clean.
- [ ] **Step 4:** Query the brain for anything prior art/gotchas on this area before writing back (`ninox brain query "workspace cleanup"`, `ninox brain query "worktree reap"`), then `ninox brain add` a short note on the `cleanup()` seam MLOPS-5417 should call and the marker-file ownership-tracking decision, so the next worker touching this area (including 5417's) finds it.
- [ ] **Step 5:** No commit (verification-only task) — proceed to rebase-on-`origin/v1` + PR per the workflow.

## Out of scope for this PR (hand off via `ninox request-work` after opening the PR)

- `ninox workspaces list/prune/show` CLI (brief item 4) — can call `workspace::sweep_repo_orphans`/`discover_repo_roots` directly once they exist.
- Shared build cache opt-in config (brief item 5).
- GUI/TUI surfacing (brief item 6).
