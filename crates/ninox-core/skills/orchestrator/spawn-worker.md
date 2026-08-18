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
  --workspace /absolute/path/to/repo \
  --delivery pr
```

`--name` becomes the session ID. Names are slugified automatically (`"ATH-123 auth"` → `"ath-123-auth"`).
Omitting `--name` generates a timestamp ID (`worker-…`).

`NINOX_ORCHESTRATOR_ID` is set in your environment and picked up automatically.
Each spawn prints the session ID (`spawned ath-123-auth-fix`) — use it to send follow-ups.

## Choosing Delivery

Choose the contract explicitly:

- `--delivery pr` — code changes that must be delivered through a branch,
  commit, push, and pull request.
- `--delivery direct` — research, operational tasks, direct-file/artifact
  work, and all non-Git workspaces. The worker validates its artifacts or
  direct changes and reports either a blocker or completion; it does not
  create branches, remotes, commits, or PRs.

When `--delivery` is omitted, Ninox preserves PR delivery for Git repositories
and selects direct delivery for non-Git workspaces. Prefer an explicit choice
so the worker contract reflects the task rather than only the workspace type.

For PR delivery, always pass the primary repository checkout to `--workspace`.
When that checkout is directly under Ninox's configured repositories root,
Ninox leases a warm sibling checkout (`<repo>-w1`, `<repo>-w2`, …). Ninox
chooses and manages the pool slot; never pass a `-wN` path yourself. The
primary checkout remains untouched.

## Messaging Workers (Orchestrator → Worker)

Send instructions or follow-ups to a worker using its session ID:

```bash
{{NINOX_BIN}} send ath-123-auth-fix "Focus on the token refresh path first"
```

## Work Requests (Worker → Orchestrator)

Workers are scoped to one task and one delivery. When a worker discovers
additional work, it runs `{{NINOX_BIN}} request-work "<description>"` and
Ninox forwards the request to you as a
`[Ninox] Worker … requested additional work` message.

When one arrives: decide whether the work is worth doing, and if so
spawn a new worker for it with `{{NINOX_BIN}} spawn`. **Never** tell a worker to widen
its own task or delivery — extra scope always gets its own worker. For PR
delivery, Ninox will also warn you (`[Ninox] Worker … opened N PRs beyond its
tracked PR`) if a worker opens extra PRs anyway; review each extra PR and
either close it or hand it to a dedicated worker.

## Cleaning Up Workers

A finished worker keeps its git worktree checked out until someone clears it.
Reap the ones you are done with:

```bash
{{NINOX_BIN}} reap
```

See the `reap-workers` skill for the full contract.

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
