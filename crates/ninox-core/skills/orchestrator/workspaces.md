---
name: workspaces
description: Inspect and clean up leftover agent worktrees across every repo you've spawned into, when disk fills up or before reaping a batch of finished workers.
---

# Inspecting and Pruning Leftover Workspaces

Every worker checks out its own git worktree under
`{repo}/.claude/worktrees/{session-id}`. Reaping a worker (see the
reap-workers skill) cleans up its own worktree, but leftovers can still pile
up — a sweep that skipped a dirty worktree, a worker killed outside Ninox, a
repo swept before this feature existed. A periodic sweep already runs in the
background (see `[orphan_sweep]`); this is the on-demand view and lever.

## See what's there

```bash
{{NINOX_BIN}} workspaces list
```

Lists every `.claude/worktrees/*` entry across every repo you've spawned
into: path, the session it's (or was) tied to, status, on-disk size, dirty/
unpushed state, and the action the sweeper would take on it. Add `--json`
for a machine-readable form, or `--repo <path>` to restrict to one repo.
This never deletes anything.

## Clean up leftovers

```bash
{{NINOX_BIN}} workspaces prune
```

Removes exactly what the periodic sweep would: merged or genuinely empty
orphaned worktrees only. It always previews what it's about to do before
touching anything, and asks for confirmation — pass `--yes` to skip that,
or `--dry-run` to only ever see the preview. A worktree with uncommitted
changes or unpushed commits is **never** touched by this command, no matter
what flags you pass — those need a human to look at them first (see
`ninox workspaces list`'s "needs attention" rows).

`--include-unmerged` additionally prunes clean, fully-pushed worktrees whose
branch just hasn't been merged yet (the branch itself is kept; only the
worktree is removed, so the work isn't lost).

## Inspect one worktree

```bash
{{NINOX_BIN}} workspaces show <path-or-session-id>
```

## Build output is freed automatically

When a session finishes, Ninox deletes its worktree's gitignored build
output (`[workspaces] build_output_dirs`). Low disk triggers the same
freeing plus the orphan sweep (`[workspaces] disk_low_percent`). If the
disk is full, see the disk-full skill. Never run machine-wide cleanup
tools.
