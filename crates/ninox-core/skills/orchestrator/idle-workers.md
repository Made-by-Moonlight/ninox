---
name: idle-workers
description: list your idle workers (what each waits on, how long, disk held) and act on a "workers idle" prompt from the user.
---

# Idle Workers

Idle workers hold session slots and worktree disk (`target/`,
`node_modules`). Ninox lists them from stored state, without calling GitHub:

```bash
{{NINOX_BIN}} idle           # grouped by orchestrator: idle time, waiting on, disk
{{NINOX_BIN}} idle --json
```

A worker is idle when its activity has been idle or blocked for the
configured time (`[idle_workers].idle_minutes`, default 30), or its PR merged
and it never finished. Disk numbers are cached and show their age.

## When you get a "workers are idle" message

The message lists each of your idle workers with its idle time, what it is
waiting on and its disk use. For each one:

- PR merged or work done: have the worker run `ninox complete`, or reap it
  (`{{NINOX_BIN}} reap <id>`).
- Waiting on you (review, answer, unblock): act on it now.
- Otherwise: reply saying what it is waiting for.

The user can send this message to every orchestrator at once, or you can run
`{{NINOX_BIN}} idle --prompt --orchestrator <your-id>` to send yourself a
fresh list. The same orchestrator is not prompted about the same set of
workers again within `[idle_workers].prompt_cooldown_minutes` (default 30).
