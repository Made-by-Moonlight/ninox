# MLOPS-5418 follow-up: `ninox workspaces` CLI + minimal GUI/TUI surfacing

> **For agentic workers:** REQUIRED SUB-SKILL: use superpowers:subagent-driven-development or superpowers:executing-plans. Checkbox (`- [ ]`) syntax tracks steps.

**Goal:** implement items 4 and 6 of `BRIEF-MLOPS-5418-workspace-cleanup.md` on top of the already-merged core (#271, `crates/ninox-core/src/workspace/`): a `ninox workspaces list/prune/show` CLI, a GUI nav-bar chip + compact list/prune modal, and a TUI Remotes-tab panel (`W` key). Items 1-3, 5, 7 are done; this PR does not touch cleanup/orphan safety semantics except one additive, backward-compatible parameter described in Task 1.

**Spec:** `.claude/design-ref/fleet-overview-v1/BRIEF-MLOPS-5418-workspace-cleanup.md` + `BRIEF-common.md` §7 (process).

## Global constraints

- Base: `origin/v1`. PR title: `MLOPS-5418: feat(cli): ninox workspaces command and minimal UI surfacing`.
- Never call `cleanup()` from `list`/`show`, or from `prune --dry-run`.
- `--include-unmerged` must have real effect on what gets pruned (see Task 1) — not just a display difference.
- Dirty/unpushed/live/interrupted rows are never pruned by this command, regardless of flags (enforced by `decide()`/`sweep_repo_orphans_with_policy`, not reimplemented at the CLI layer).
- Never run any test against the real store/`~/.claude/worktrees`/real `AppConfig::load()` — tempdir repos + tempfile `Store`, per `BRIEF-common.md` §7.
- `ninox workspaces` is a new agent-facing capability per the project's `CLAUDE.md` — register it in `capabilities::REGISTRY` with orchestrator-audience skill markdown.
- Do not run `cargo fmt` on the whole tree.

---

### Task 1: `sweep_repo_orphans_with_policy` — give `include_unmerged` real teeth

**Files:** `crates/ninox-core/src/workspace/orphan.rs`

Today `sweep_repo_orphans` cleans every `Orphan(..)` entry `decide()` approves, including a clean/fully-pushed/not-yet-merged branch (worktree removed, branch kept — `decide()`'s `Proceed{delete_branch:false}`). The CLI's `prune` needs a conservative default (merged-or-empty only) with `--include-unmerged` opting into that same already-safe-but-unmerged bucket, without duplicating the module's branch-resolution logic (the recorded-branch-vs-current-HEAD check) outside `orphan.rs`.

- [ ] Add `pub async fn sweep_repo_orphans_with_policy(store: &Store, repo_root: &Path, retention_ms: i64, include_unmerged: bool) -> anyhow::Result<SweepReport>`. Inside the existing `Orphan(ownership)` match arm, compute `unmerged_with_content = !facts.merged && facts.commits_beyond_base > 0` alongside the existing `legacy_qualifies`; when `legacy_qualifies && (include_unmerged || !unmerged_with_content)`, proceed exactly as today; when `!legacy_qualifies`, keep today's needs-attention `CleanupOutcome`; the new third case (`legacy_qualifies && unmerged_with_content && !include_unmerged`) yields `outcome: None` (excluded by policy this run, not a safety skip — distinct from both "cleaned" and "needs attention").
- [ ] `sweep_repo_orphans` becomes `sweep_repo_orphans_with_policy(store, repo_root, retention_ms, true).await` — every existing test must still pass unmodified (this is the poller's call site; its behavior does not change).
- [ ] New tests: `unmerged_clean_pushed_orphan_is_excluded_when_include_unmerged_is_false` (worktree survives, `outcome: None`), `unmerged_clean_pushed_orphan_is_cleaned_when_include_unmerged_is_true` (worktree removed, branch kept — same assertions as today's `unmerged_branch_with_commits_beyond_base_is_kept`-style case but through the new function).
- [ ] Run `cargo test -p ninox-core workspace::orphan -- --nocapture`, then commit.

### Task 2: `WorkspaceRow` / `classify_action` / `dir_size_bytes` — the shared read-only report

**Files:** create `crates/ninox-core/src/workspace/report.rs`; modify `crates/ninox-core/src/workspace/mod.rs` (`mod report; pub use report::*;`). `ninox-core/Cargo.toml` already depends on `walkdir` (used by `brain.rs`).

This is the single data source `ninox workspaces list/show`, the GUI chip/list, and the TUI panel all build on — written once here instead of three times.

- [ ] Interfaces:
  ```rust
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum WorkspaceAction {
      None,
      WouldClean { delete_branch: bool, unmerged_with_content: bool },
      NeedsAttention(SkipReason),
  }
  pub struct WorkspaceRow {
      pub path: PathBuf,
      pub session_id: Option<String>,
      pub status: OrphanStatus,
      pub facts: WorkspaceFacts,
      pub size_bytes: u64,
      pub action: WorkspaceAction,
  }
  /// Pure — mirrors sweep_repo_orphans_with_policy's own gating exactly
  /// (legacy-squeaky-clean check, then decide()), so list/show always agree
  /// with what prune would actually do.
  pub fn classify_action(status: OrphanStatus, facts: &WorkspaceFacts) -> WorkspaceAction
  /// Best-effort recursive file-size sum; unreadable entries are skipped,
  /// a missing path is 0 — never a hard error for one bad worktree.
  pub fn dir_size_bytes(path: &Path) -> u64
  /// Read-only: classify_repo_orphans + classify_action + dir_size_bytes +
  /// session-id lookup, one row per `.claude/worktrees/*` entry. Never
  /// calls cleanup().
  pub async fn build_workspace_rows(store: &Store, repo_root: &Path, retention_ms: i64) -> anyhow::Result<Vec<WorkspaceRow>>
  ```
- [ ] Tests (pure, no IO beyond what `classify_repo_orphans`'s own tempdir idiom already requires): one per `WorkspaceAction` case (Live/NotOrphanYet/NotOurs → `None`; merged orphan → `WouldClean{delete_branch:true, unmerged_with_content:false}`; empty no-upstream orphan → `WouldClean{delete_branch:true, unmerged_with_content:false}`; clean-pushed-unmerged orphan → `WouldClean{delete_branch:false, unmerged_with_content:true}`; dirty orphan → `NeedsAttention(Dirty)`; unclean LegacyMatch → `NeedsAttention(UnpushedCommits)`); `dir_size_bytes` on a tempdir with known file sizes and on a missing path (`0`); `build_workspace_rows` integration test asserting row count/fields for a small fixture and that nothing is deleted.
- [ ] `cargo test -p ninox-core workspace::report -- --nocapture`, then commit.

### Task 3: `ninox workspaces list|prune|show` CLI

**Files:** `crates/ninox-ui-shared/src/cli.rs`

- [ ] Add `Command::Workspaces { #[command(subcommand)] action: WorkspacesAction }` and:
  ```rust
  enum WorkspacesAction {
      List { json: bool, repo: Option<PathBuf> },
      Prune { dry_run: bool, include_unmerged: bool, yes: bool, repo: Option<PathBuf> },
      Show { target: String },
  }
  ```
  `repo` restricts to one repo root (default: every root from `discover_repo_roots`). `Show`'s `target` is a worktree path or a session id (match against `WorkspaceRow::path`/`session_id`).
- [ ] Wire into the main `match command` (same non-hot-path shape as `Reap`, not a short-circuit): `Some(Command::Workspaces { action }) => run_workspaces(store, action).await`.
- [ ] `run_workspaces_list`: for each repo root, `build_workspace_rows`; print a table (path, session id or `—`, status, human size, dirty, unpushed count, upstream/merged state, action) or, with `--json`, serialize `Vec<WorkspaceRow>`-shaped rows (derive `Serialize` on the row/action types, or build an ad hoc `serde_json::json!` per row — match whatever's less friction given `OrphanStatus`/`WorkspaceFacts`/`SkipReason` don't currently derive `Serialize`).
- [ ] `run_workspaces_prune`: build rows, partition by `is_default_prune_candidate`/`is_unmerged_prune_candidate` (two small pure helpers alongside `classify_action` in `report.rs`, or inlined here against `WorkspaceAction::WouldClean{unmerged_with_content, ..}`), print the preview (what would be pruned, with total reclaimed size) always; when `--dry-run`, stop there — **no call to `cleanup()` or `sweep_repo_orphans_with_policy` at all**; otherwise, unless `--yes`, read a confirmation line from stdin (`y`/`yes`, case-insensitive) and abort without touching anything on anything else; then call `sweep_repo_orphans_with_policy(store, repo_root, retention_ms, include_unmerged)` per repo and print `describe()` per acted-on path.
- [ ] `run_workspaces_show`: resolve `target` to one `WorkspaceRow` (search every discovered repo), print its full detail (same fields as a list row, one per line) or `anyhow::bail!` if nothing matches.
- [ ] Register the capability: add a `workspaces.md` orchestrator skill under `crates/ninox-core/skills/orchestrator/` (frontmatter `name: workspaces`, one-line `description:`) documenting `{{NINOX_BIN}} workspaces list/prune/show`, and one `Capability` entry in `REGISTRY` (`audience: Orchestrator`, `enabled: |_| true`).
- [ ] Tests (in `cli.rs`'s existing test module, tempdir repo + tempfile `Store`, matching the core idiom): list output includes expected rows/columns (plain and `--json`); `prune --dry-run` never removes a worktree and the fixture directory still exists afterward; `--include-unmerged` changes which rows a non-dry-run prune actually removes (reuse Task 1/2's fixtures); dirty/unpushed worktrees survive every flag combination; `show` resolves by path and by session id and errors on no match.
- [ ] `cargo test -p ninox-ui-shared -- --nocapture`, `cargo clippy -p ninox-ui-shared --all-targets -- -D warnings`, then commit.

### Task 4: GUI — nav-bar chip + compact list/prune modal

**Files:** `crates/ninox-gui/src/components/shell/nav.rs`, `crates/ninox-gui/src/app.rs`, new `crates/ninox-gui/src/components/workspaces_modal.rs`.

- [ ] A chip on the nav bar's right side (alongside the existing Settings icon button), `"N leftover workspaces · X GB"`, hidden entirely when `N == 0`. Data: every discovered repo's `build_workspace_rows` filtered to `WorkspaceAction::WouldClean{..} | WorkspaceAction::NeedsAttention(..)` (anything the brief's evidence calls "leftover" — not `None` rows), `N` = count, `X` = summed `size_bytes` across those rows converted to GB. Follow the existing async-load pattern already used for another nav/background data source in `app.rs` (`Task::perform`, not a blocking call inside `view()`); refresh on the same cadence/trigger as that existing source rather than inventing a new poll loop.
- [ ] Clicking the chip opens a modal (new `Option<WorkspacesModalState>` field on `App`, same shape as `spawn_modal`/`catalogue_modal` — `None` when closed, rendered as an overlay over the current view when `Some`, Esc/Cancel closes it) showing the same rows as `ninox workspaces list` (path, status, size, action) with a **Prune** button that first shows the preview (reuse the CLI's candidate-selection helpers from `report.rs`) and only calls the real prune (`sweep_repo_orphans_with_policy`, default `include_unmerged: false` — no unmerged checkbox needed for a "minimal" surface) after an explicit second confirm click. No new `View` variant — this never becomes a full screen.
- [ ] Tests: the chip renders nothing (no button) with zero rows, and renders with a non-zero count/size with a small fixture; the modal's row list renders for a zero-entries fixture without panicking (the brief's "render with zero entries" requirement).
- [ ] `cargo build -p ninox-gui`, `cargo test -p ninox-gui`, `cargo clippy -p ninox-gui --all-targets -- -D warnings`, then commit.

### Task 5: TUI — Remotes-tab panel (`W` key)

**Files:** new `crates/ninox-tui/src/tui/workspaces_panel.rs`, modify `crates/ninox-tui/src/tui/remotes.rs` (or wherever the Remotes tab's key handling lives), `crates/ninox-tui/src/tui/mod.rs` (key binding), `crates/ninox-tui/src/tui/view.rs` (render), `crates/ninox-tui/src/tui/state.rs` (panel-open state).

- [ ] Mirror `RemotesView`'s shape: a `WorkspacesPanelState { open: bool, selected: usize, rows: Vec<WorkspaceRowView> }` (a thin display-only projection of `ninox_core::workspace::WorkspaceRow`, since the real struct holds non-`Copy`/non-trivial fields) built via `build_workspace_rows` across every discovered repo root.
- [ ] `W` on the Remotes tab toggles the panel open/closed (same toggle convention as any other existing single-key panel in this app — match whichever already exists rather than inventing a new one); render it as a panel within the Remotes tab, not a new tab.
- [ ] Tests: panel renders with zero entries without panicking; `W` toggles `open`; row projection matches a small fixture's data.
- [ ] `cargo build -p ninox-tui`, `cargo test -p ninox-tui`, `cargo clippy -p ninox-tui --all-targets -- -D warnings`, then commit.

### Task 6: Whole-workspace verification

- [ ] `cargo build --workspace`
- [ ] `cargo test --workspace`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] Capture a `ninox workspaces list` sample run against a scratch fixture for the PR/report.
- [ ] Visual check per `BRIEF-common.md` §7: build, launch against isolated scratch config/db (never the real store), screenshot the GUI chip+modal (window-only) and the TUI panel (kitty window only), compare against the brief's minimal-UI intent (no board reference exists for this feature specifically — compare against the brief's own description instead).
- [ ] Rebase on `origin/v1`, open the PR (`## Why` / `## Summary` / `## Test plan`), do not merge.
