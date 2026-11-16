# MLOPS-5418: never retire live workers or delete dirty worktrees

On 31 Oct the daemon's retention sweep removed a live worker exactly 24h after
its first PR merged, and lost the worktree's uncommitted changes. Four other
live workers went the same way that evening. Each fix below lands with a test
that fails before the fix and passes after it.

## 1. Retention never retires a live worker
- `Poller::sweep_retired_sessions` only considers `Done`/`Terminated` rows,
  timed by `terminal_at`. The merged-but-live fallback (timed by `merged_at`)
  is deleted.
- `merged_at` stays as an informational stamp. Nothing that removes things
  reads it for a live row any more. Cleanup's own merge bypass already
  requires the merged branch to still be checked out with nothing committed
  since. Clearing it when the branch changes would re-trigger merge
  detection for the old PR (review finding).
- `remove_session(id, force: false)` refuses a live row outright, before it
  kills anything.
- Tests: a live merged worker past the window keeps its row, session and
  worktree; a non-forced `remove_session` refuses a live row.

## 2. A row whose worktree was kept is never deleted
- `remove_session` returns a `RemoveOutcome`. When a non-forced cleanup skips
  the worktree (dirty, unpushed, stashed) or fails to remove it, the row stays:
  `Terminated` if it wasn't terminal already, the cleanup result recorded, and
  `fleet_sessions.retained_at` stamped.
- The sweep retries kept rows every 10 minutes, so a worktree that has since
  been pushed or cleaned is reclaimed. It sends the retired notice once (it
  says the worktree was kept), not on every retry. A row whose worktree is
  already gone from disk is removed: a skipped cleanup never deletes the
  branch.
- Test: a dirty `Terminated` row past the window survives the sweep with its
  worktree.

## 3. Orchestrator removal keeps dirty and Interrupted workers
- `remove_orchestrator` and `remove_orchestrator_keeping_live` clean workers
  up with `force: false`. A worker whose worktree is skipped is detached from
  the orchestrator, marked `Terminated` (it was killed), and returned as kept.
  `Interrupted` workers are never killed or cleaned; they are detached and kept.
- The GUI and TUI report kept workers.
- Tests: removing an orchestrator leaves a dirty worker's worktree and row, and
  an `Interrupted` worker's.

## 4. Stashes and commits on no remote
- Gitignored files stay out of the dirty check. A stash made on the
  worktree's branch (or its detached HEAD) counts as dirty, because `git
  status` can't see stashes.
- Commits that no remote has are already refused by #271's facts:
  unpushed-vs-upstream, `base..branch`, and unreachable-from-remotes for a
  detached HEAD. Commits reachable only from the worker's branch are never
  lost either, since a skipped cleanup keeps the branch. An extra
  off-remote count was tried and dropped: it over-counted commits still on
  local `main`.
- Tests: a stash-only worktree counts as dirty and another branch's stash
  doesn't; gitignored build output doesn't.

## 5. Scratch instances stay out of the real data dir
- `AppConfig::default_db_path()` (now used by the CLI and `server_restart`)
  follows a scratch `NINOX_CONFIG`'s directory. Real config keeps the real
  data dir.
- The ptyd socket follows a scratch config the same way, so a scratch instance
  never starts or reaches the user's ptyd host.
- `daemon.log` already sits next to the db (#319).
- Test: with a temp HOME and a scratch `NINOX_CONFIG`, nothing resolves under
  HOME's data dir.

## 6. Stale-daemon warning
- #319 shows the GUI/TUI banner with Restart server. This adds
  `server_restart::stale_daemon_warning` ("daemon is running an older build:
  restart it"), printed to stderr by the CLI when it finds a daemon already
  running, and logged once by the GUI and TUI. Nothing restarts on its own.
- Test: the warning fires on a version or build mismatch and stays quiet on a
  match or an unknown build.

## Also covered
- #317 "Clean up finished" (`ReapSelection::Finished`): test over a live merged
  worker, a dirty `Terminated` one and an `Interrupted` one.
- #320 dead-pane reconcile only writes statuses; queued sessions never reach
  the sessions table. Both are covered by the sweep's terminal-only rule.
