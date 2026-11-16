---
name: worker-status
description: read workers' live activity states and manage dependency edges between them.
---

# Worker Activity and Dependencies

Every worker's moment-to-moment activity is tracked: `working` (mid-turn),
`idle` (between turns), `blocked` (self-declared, with a note saying on
what), or `unknown` (the worker can't report — pre-existing worktree or a
harness without hooks).

## Read the fleet

```bash
{{NINOX_BIN}} worker-status list          # activity + dependency edges
{{NINOX_BIN}} worker-status list --json   # machine-readable
```

Use this instead of polling workers' terminals to see who is stuck. A
`blocked` worker's note says what it's waiting for; act on it (nudge the
blocking worker, merge the blocking PR, or re-scope).

## Declare dependencies between workers

When you know worker B's task depends on worker A's, register the edge so
the fleet view shows it (`--for` names the depending worker):

```bash
{{NINOX_BIN}} worker-status depend <worker-a> --for <worker-b> --note "B builds on A's API"
{{NINOX_BIN}} worker-status undepend <worker-a> --for <worker-b>
```

Sessions are referenced by id or name. PR branch stacking (B's PR based on
A's branch) is detected automatically and needs no declaration.

## Sequencing work

Prefer spawning dependent workers only after their dependency merges. When
they must run concurrently, declare the edge at spawn time — the fleet
view then explains *why* a worker sits blocked instead of looking stalled.

## Messages that didn't arrive

`ninox send` prints a receipt and exits non-zero when a message was not
delivered. A live but unreachable target gets `queued, target unreachable`
(exit 75), and Ninox retries for 24h. To see everything addressed to you,
including messages a delivery mechanism failed to inject:

    ninox messages --to-me --since 2h
    ninox messages --to <worker> --since 1d     # what a worker was sent

If a worker sits idle without messaging anyone since its last incoming
message (usually a read-back it wrote only in its own pane), Ninox nudges
it once and sends you a `[status]` note.
