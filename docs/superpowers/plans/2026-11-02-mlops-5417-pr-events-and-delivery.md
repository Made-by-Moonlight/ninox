# MLOPS-5417: deliver every PR event workers wait on, and make `ninox send` reliable

Two PRs off `origin/v1`.

## PR 1: complete GitHub event delivery

### Problem
- Workers are told not to poll `gh`, but Ninox only delivers CI failure,
  changes-requested, ready-to-merge and merge. A worker waiting for green
  CI on a repo with skipped jobs, a bot review, an approval, a conflict or a
  comment waits forever.
- Every dedup flag lives in the poller's in-memory `enrichment_cache`, so a
  daemon restart replays CI and review reactions.
- `summarize_checks` counts `skipped`/`neutral` as pending, so "all green"
  never fires on repos with skipped jobs.

### Design
- `ninox_core::pr_events`: a pure detector
  `detect(&mut PrCursor, &PrSnapshot, now, &PrEventsConfig) -> Vec<PrEvent>`.
  - Events: `ci_passed`, `ci_failed`, `ci_pending_stuck`, `review_comment`,
    `review_approved`, `changes_requested`, `review_requested`,
    `review_dismissed`, `mergeable_conflict`, `behind_base`,
    `ready_for_review`, `pr_comment`, `merged`, `closed`.
  - The cursor holds head SHA, last CI verdict for that SHA, first-seen
    pending time per check, seen review ids and their last state, seen
    inline-comment ids, seen issue-comment ids, requested reviewers,
    conflict/behind/draft flags and the terminal flag. A head SHA change
    resets the CI fields.
  - Seen-id sets are pruned to ids still present in the latest snapshot, so
    the cursor stays bounded (GitHub's windows only drop old items).
  - First observation is a baseline: review/comment history is recorded
    silently. Current CI verdict, conflict, behind and an outstanding
    changes-requested are still reported once, because a worker that just
    started waiting needs them.
- Cursors persist in a new `pr_event_cursors` table keyed by
  `(subscriber, repo, number)`, so nothing replays after a restart. A
  worker that watches its own PR shares the key with its session PR, so it
  gets one copy.
- The poller runs one event pass per tick over the unique
  `(subscriber, PR)` pairs: sessions' own PRs plus watches with an opener.
  It runs on both the batched GraphQL sweep and the legacy REST path, using
  data those paths already fetched. Each subscriber gets one message per
  tick, `MessageKind::PrEvent` (`[pr_event]`), and an `Event::PrEvents` on
  the bus. The old per-path worker sends for CI failure, ready-to-merge and
  review are removed (UI notifications stay), so nothing is delivered twice.
- GraphQL query additions (same single request per 50 PRs): `url isDraft
  mergeStateStatus`, `reviewRequests`, `reviewThreads.isResolved`,
  `viewer { login }`. GraphQL POSTs have no ETag support; the sweep is
  already one request per chunk.
- `pr_status_cache` table: the event pass writes a `PrStatusView` per PR;
  `ninox pr status [<pr>] [--json]` reads only that (no GitHub client is
  constructed), short-circuited in `dispatch()`.
- Hang nudge, `[pr_events]` config (on by default):
  `hang_nudge_after_mins = 30` and `ci_stuck_after_mins = 45` (0 disables
  either). Trigger: own PR open, the worker is idle for N minutes, and
  there has been no PR event for N minutes. The worker gets one nudge per
  idle episode, and nudges are at least N minutes apart; the nudge includes
  the PR status. The orchestrator gets one `status` message per quiet spell,
  reset by the next PR event.
- Skills: worker and orchestrator `watch-pr` plus worker
  `pr-watch-enforcement` list every event, say to use `ninox pr status`,
  and say what to do on each.

### Tests
- Detector unit tests per event: detect, dedupe, reset on push, baseline.
- Store round-trip: a cursor persisted then reloaded produces no events
  (no replay after restart).
- Poller: 20 workers across 3 PRs give one `fetch_batch` with 3 keys
  (O(PRs)); the hang check makes no fetch; nudge rate limit.
- `pr status` reads the cache with no network.

## PR 2: reliable message delivery

### Plan
1. Reproduce in a scratch fleet (`isolate()`, random port, scratch
   `TMUX_TMPDIR`, no `NINOX_*` env): session_socket, keystrokes, and an
   orchestrator that isn't a Ninox-managed pane. Cover the installed 0.37
   shim (diagnose only) and v1. Check that the v1 CLI and daemon resolve
   the same store path.
2. `ninox send` prints a receipt (message id, mechanism,
   delivered|queued) or exits non-zero with the reason.
3. Unreachable targets go to `held_messages` and the poller retries them;
   the sender sees "queued, target unreachable".
4. `message_events` gains an `outcome` column, and every attempt is
   recorded.
5. `ninox messages --to-me [--since]`.
6. GUI/TUI: held/undelivered count on the target's card; the sender's
   activity shows "message queued (target unreachable)".
