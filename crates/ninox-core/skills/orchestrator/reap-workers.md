---
name: reap-workers
description: Use when workers have finished (PR merged, session dead, work abandoned) and you want their sessions and worktrees cleaned up.
---

# Reap Your Workers

Every worker you spawn checks out its own git worktree under
`{repo}/.claude/worktrees/{session-id}`. When the worker finishes, that
worktree stays on disk until it is reaped. Reaping kills the worker's
session, removes its worktree, and lets its card age off the fleet board.
Anything the worker reported but Ninox has not processed yet (a work request,
a freshly opened PR) is left alone for Ninox to pick up.

You can only ever reap **your own** workers — an id belonging to another
orchestrator is refused, not cleaned up.

## Reap everything that has finished

```bash
{{NINOX_BIN}} reap
```

This is the safe default: it touches only workers that are finished for good
(PR merged, process exited, terminated). Two kinds of worker are never
selected by it:

- **Still running** — reaping would destroy work in progress.
- **Interrupted** — its pane died with the machine (a reboot), but its
  conversation and branch survive and the user can resume it. Reaping gives
  that up.

## Reap specific workers

```bash
{{NINOX_BIN}} reap ath-123-auth-fix ath-124-api
```

## Reap a running or interrupted worker

Both need an explicit `--force`, plus `--all` to select them in bulk:

```bash
{{NINOX_BIN}} reap ath-123-auth-fix --force   # this worker whatever state it's in
{{NINOX_BIN}} reap --all --force              # every worker you own
```

Without `--force` those workers are reported as skipped and left completely
alone.

## Merged workers (post-merge validation)

`[worker_completion] mode` in `{{CONFIG_PATH}}` decides what a merge does:

- `self` (default): the merge does not reap. The worker is told to finish
  itself with `ninox complete` once its task, including any post-merge
  steps, is done; Ninox then reaps it. If it sits idle after the merge it
  gets one reminder.
- `reap_on_merge` (legacy `[auto_reap] enabled = true`): a worker whose
  Definition of done is only "PR merged" is reaped the moment it merges.
- `manual` (legacy `[auto_reap] enabled = false`): a merged worker is kept
  alive and you or the user reap it.

A kept-alive merged worker still has its conversation, checkout and
environment, so you can run post-merge validation in it:

```bash
{{NINOX_BIN}} send ath-123-auth-fix "Your PR merged. Verify the deploy succeeded and the feature works on main."
```

It still counts as *running*. Reap it with an explicit `--force` only if it
doesn't finish itself, or you no longer need it:

```bash
{{NINOX_BIN}} reap ath-123-auth-fix --force
```

## When to reap

- A worker's PR merged and it has not finished itself with `ninox complete`
  (after a reminder), or you no longer need it — `--force` reap it.
- A worker died or was terminated and you have read whatever you needed
  from it — reap it.
- You decided a worker's task is no longer wanted — `--force` reap it.

## When NOT to reap

- **Not while a worker is still working.** Message it (`{{NINOX_BIN}} send`) or
  wait. Force-reaping destroys uncommitted work in its worktree.
- **Not an interrupted worker the user may want back.** After a reboot every
  worker is interrupted, not finished. `{{NINOX_BIN}} reap --all --force` at
  that moment throws away every resumable session — ask first.
- **Not to "restart" a worker.** Reap and re-spawn is a fresh session with
  no memory of the old one.
- **Not another orchestrator's workers.** They aren't yours; the command
  will refuse.
