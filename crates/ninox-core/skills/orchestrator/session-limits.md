---
name: session-limits
description: Use when a spawn says "queued", when you need to know how many sessions are running or waiting on a machine, or when you want to cancel a spawn that hasn't started.
---

# Session Limits and the Spawn Queue

Each machine has a cap on how many **worker** sessions run at once (default 6
on this machine). Orchestrators and the prompt agent don't count toward it and
are never queued. A worker spawn over the cap is **queued, not refused**:

```
$ {{NINOX_BIN}} spawn --prompt "…" --workspace /repo --name fix-auth
queued fix-auth: 2 ahead of you on this machine (session limit reached; it starts automatically when a slot frees)
```

The session id is valid straight away:

- `{{NINOX_BIN}} send fix-auth "…"` is held and delivered, in order, once it
  starts.
- It starts on its own, first in first out, as running sessions finish.
  You don't need to retry or poll.
- `{{NINOX_BIN}} list` shows it as `queued #N` and prints a
  `4/6 workers running locally · 3 orchestrators · 2 queued` line at the top.

## See the limits

```bash
{{NINOX_BIN}} limits show
```

## Cancel a queued spawn

```bash
{{NINOX_BIN}} reap fix-auth
```

A queued session never started, so reaping it just removes it from the
queue, along with any messages held for it.

## Scripts that must not wait

Pass `--no-queue` to `spawn`. When the machine is at its worker limit, the
command fails straight away instead of queueing. Orchestrator spawns are never
subject to the limit.

## Changing the limit

Only change it when the user asks. Use
`{{NINOX_BIN}} limits set local <n|unlimited>`. The new value applies within
a few seconds. Lowering it never stops running sessions.
