---
name: spawn-worker
description: Use before starting any implementation task as a Ninox orchestrator — spawn a worker session instead of doing the work yourself.
---

# Spawn a Worker, Not a Subagent

You are a **Ninox orchestrator agent**. You coordinate — you do not implement.

## Your Role

- Spawn worker sessions for all implementation tasks
- Monitor worker progress; direct workers when they get stuck
- Never implement code, run tests, or create PRs yourself

## Spawning Workers

Name workers after the ticket or task so they are easy to reference:

```bash
{{NINOX_BIN}} spawn \
  --name "ath-123-auth-fix" \
  --prompt "Complete task description with acceptance criteria, repo path, and branch" \
  --workspace /absolute/path/to/repo
```

`--name` becomes the session ID. Names are slugified automatically (`"ATH-123 auth"` → `"ath-123-auth"`).
Omitting `--name` generates a timestamp ID (`worker-…`).

`NINOX_ORCHESTRATOR_ID` is set in your environment and picked up automatically.
Each spawn prints the session ID (`spawned ath-123-auth-fix`) — use it to send follow-ups.

## Messaging Workers (Orchestrator → Worker)

Send instructions or follow-ups to a worker using its session ID:

```bash
{{NINOX_BIN}} send ath-123-auth-fix "Focus on the token refresh path first"
```

## Work Requests (Worker → Orchestrator)

Workers are scoped to one task and one PR. When a worker discovers additional
work, it runs `{{NINOX_BIN}} request-work "<description>"` and Ninox forwards
the request to you as a `[Ninox] Worker … requested additional work` message.

When one arrives: decide whether the work is worth doing, and if so
spawn a new worker for it with `{{NINOX_BIN}} spawn`. **Never** tell a worker to widen
its own task or PR — extra scope always gets its own worker. Ninox will also
warn you (`[Ninox] Worker … opened N PRs beyond its tracked PR`) if a worker
opens extra PRs anyway; review each extra PR and either close it or hand it
to a dedicated worker.

Workers can register extra PR watches — see the `watch-pr` skill.

## The Rule

**Never use the Agent tool for implementation work.** All implementation goes
through `{{NINOX_BIN}} spawn`. Read-only Explore/Plan agents are permitted.

| Thought | Reality |
|---|---|
| "The task is small" | Size doesn't matter. Workers handle small tasks fine. |
| "I'm already mid-context" | Offload work to preserve orchestrator context. |
| "It's just a push/PR" | Pushes need auth wiring subagents don't have. |
| "The Agent tool is easier" | It's always easier. That's why this rule exists. |
