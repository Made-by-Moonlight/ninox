---
name: watch-pr
description: Register a GitHub PR for consolidated watching so you get every CI, review and mergeability event instead of polling gh yourself; read cached state with `ninox pr status`.
---

# Watching a PR

NEVER poll GitHub for PR/CI state in a loop (`gh pr checks --watch`,
`gh run watch`, repeated `gh pr view`): it burns the shared API rate
limit. Register a watch instead:

    ninox open --pr <pr-url>
    ninox close --pr <pr-url>

Watches auto-close when the PR merges or closes. `ninox list --prs` shows
active watches. Workers' own PRs are watched automatically: this is for
*additional* PRs (a dependency PR, a teammate's PR, a PR you opened outside
ninox).

## What gets delivered

Each event arrives once as a `[pr_event]` message, to workers for their own
PR and to whoever registered a watch:

- `ci_passed`, `ci_failed`, `ci_pending_stuck` (a check queued or running
  longer than `[pr_events] ci_stuck_after_mins`, default 45)
- `review_comment` (human or bot, with author, file:line and excerpt),
  `changes_requested`, `review_approved`, `review_requested`,
  `review_dismissed`
- `mergeable_conflict`, `behind_base`, `ready_for_review`, `pr_comment`
- `merged` / `closed`

A new push resets CI state; a restart never replays events.

## Reading state without GitHub calls

    ninox pr status <pr>           # URL, owner/repo#N or #N
    ninox pr status <pr> --json

It answers from the last consolidated sweep (checks, reviews per reviewer,
unresolved threads, mergeable state, last update) and never calls GitHub.
Prefer it to `gh pr view` when deciding whether a worker's PR is mergeable.

## Quiet workers

If a worker sits idle with an open PR and no PR event for
`[pr_events] hang_nudge_after_mins` (default 30), Ninox nudges it with its
PR status and tells you once with a `[status]` message. It usually means
the PR is waiting on a human review or on a decision from you.
