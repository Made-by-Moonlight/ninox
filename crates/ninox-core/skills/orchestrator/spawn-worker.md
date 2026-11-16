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

The worker inherits your brain set (every brain you read, writing to your
write brain). To give it less, add `--brain <name>` (repeatable) and
`--brain-write <name>`; both must be brains you read yourself, and the write
brain can't be a read-only one.

## Worker Types

Add `--type <name>` to spawn under a named worker type. The type's harness
and permission profile apply to that spawn, and if the type has a brief
template, it's composed with `--prompt` automatically:

```bash
{{NINOX_BIN}} spawn \
  --type Researcher \
  --prompt "Investigate why the flaky test fails intermittently" \
  --workspace /absolute/path/to/repo
```

### Picking the type

Before spawning, list the types you may spawn and when to use each:

```bash
{{NINOX_BIN}} worker-type list          # add --json for machine-readable output
```

Each line gives a type's name, its "use when" description, the model it
runs and what it may spawn in turn. The list includes the user's custom
types, which the built-ins below don't cover. Pick the type whose
description matches the task. If none clearly fits, use `Implementor` (also
what omitting `--type` gives you; `Implementation` and `Worker` still
resolve to it). An unrecognized `--type` name is a spawn-time error; it never
silently falls back to your default config.

## Model

A worker's model is, first match wins: `--model <id>` on the spawn, the
worker type's own model, the user's `[worker].model` setting (when it's for
the same harness), ninox's built-in default for that harness (Sonnet on
claude-code), else the harness's own default. Only pass `--model` when the
user asked for a specific model. `{{NINOX_BIN}} models show` prints what
each type resolves to.

Each type also has a `can_spawn` list controlling what types IT may pass to
`--type` in turn (e.g. `Implementor` may spawn `Researcher`, but not
`Evaluator`) — `ninox spawn` resolves this from the calling session's own
type, so a worker's sub-spawns are checked the same way yours are. See the
`worker-type` skill for defining custom types (including their own
`can_spawn`) and inspecting any type's full profile — don't hand-edit
`[[worker_types]]` in config.toml directly.

## Success Criteria & Self-Completion

Each worker type has default success criteria (e.g. `Implementor` defaults to
"PR merged"; `Researcher`/`Evaluator` default to "final report sent"). Add
`--success "<text>"` (repeatable) to require additional checklist items the
worker must attest to before it can finish, or `--success-only` to replace
the type's defaults outright with just the `--success` items you give:

```bash
{{NINOX_BIN}} spawn \
  --type Implementor \
  --prompt "Fix the auth regression" \
  --workspace /absolute/path/to/repo \
  --success "docs updated"
```

A worker decides when it is done: once every item in its Definition of
done is met and any post-merge steps in its task are finished, it runs
`ninox complete --summary "<text>" [--evidence <url|path>]…` as its last
action. `complete` verifies what it can, refuses on a dirty or unpushed
worktree (unless `--force`), stores the outcome, reports back to you, and
**ends the session**: the worker is reaped shortly after, independently of
`[worker_completion]`. You don't need to run `{{NINOX_BIN}} reap` after a
merge; it's still there for stragglers, a worker that never opens a PR, and
review-mode types (which pause `AwaitingReview` for you to finalize with
`{{NINOX_BIN}} reap <id>` or send feedback to put the worker back to work).

## Messaging Workers (Orchestrator → Worker)

Send instructions or follow-ups to a worker using its session ID:

```bash
{{NINOX_BIN}} send ath-123-auth-fix "Focus on the token refresh path first"
```

Add `--kind <kind>` (`completion`, `request_for_work`, `status`, `question`,
`system`, or the default `freeform`) to label what the message is — it shows
up as a prefix tag for the recipient and feeds Ninox's own duplicate-
notification suppression. Plain instructions and follow-ups like the one
above are fine left as `freeform`; reach for a specific kind when you're
sending something Ninox itself also reasons about, e.g. `status` for a
progress update.

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

## Cleaning Up Workers

A finished worker keeps its git worktree checked out until someone clears it.
Reap the ones you are done with:

```bash
{{NINOX_BIN}} reap
```

See the `reap-workers` skill for the full contract.

## The Rule

**Never use the Agent tool for implementation work.** All implementation goes
through `{{NINOX_BIN}} spawn`. Read-only Explore/Plan agents are permitted.

| Thought | Reality |
|---|---|
| "The task is small" | Size doesn't matter. Workers handle small tasks fine. |
| "I'm already mid-context" | Offload work to preserve orchestrator context. |
| "It's just a push/PR" | Pushes need auth wiring subagents don't have. |
| "The Agent tool is easier" | It's always easier. That's why this rule exists. |
