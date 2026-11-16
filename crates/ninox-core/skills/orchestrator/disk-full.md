---
name: disk-full
description: Handle a full or nearly full disk without destroying live workers' builds — never run machine-wide cleanup tools.
---

# When the Disk Is Full

Other orchestrators' workers share this disk and may be compiling right
now. A machine-wide cleanup deletes their builds mid-compile. This is what
happened when an agent ran `cleardisk --yes` at 98% full.

**Never run, and never tell a worker to run, any of these:**

- `cleardisk` (with or without `--yes`)
- `rm -rf` across repos or across worktrees' `target/` folders
- `docker system prune`
- `go clean -cache` / `-modcache`, `cargo` cache wipes, or any other shared-cache wipe

**What Ninox already does:**

- When a session finishes (Done/Terminated, self-completed, or merged and
  idle), Ninox deletes its worktree's gitignored build-output dirs
  (`[workspaces] build_output_dirs`, by default `target` and
  `node_modules/.cache`). It does this even when the worktree is kept for
  uncommitted work. Source is never deleted.
- When free space drops below `[workspaces] disk_low_percent` (default
  10%), Ninox frees finished sessions' builds oldest first, then runs the
  safe orphan sweep. If space is still low, it raises a "Disk is N% full"
  notification.

**What you can do:**

1. Check what's taking space: `{{NINOX_BIN}} workspaces list`.
2. Prune safe leftovers: `{{NINOX_BIN}} workspaces prune`. This never
   touches dirty or unpushed worktrees.
3. Reap your own finished workers (see the reap-workers skill).
4. If space is still short, ask the user. Never delete another
   orchestrator's worktrees, and never delete a live worker's build output.

Tell any worker that hits "no space left on device" to stop and report to
you, and to clean only its own worktree.
