---
name: disk-full
description: What to do when a build fails with "no space left on device" or the disk is nearly full — never run machine-wide cleanup.
---

# When the Disk Is Full

Many sessions share this machine's disk, and other workers may be
compiling right now. A machine-wide cleanup deletes their builds
mid-compile.

**Never run any of these:**

- `cleardisk` (with or without `--yes`)
- `rm -rf` across repos or across other worktrees' `target/` folders
- `docker system prune`
- `go clean -cache` / `-modcache`, `cargo` cache wipes, or any other shared-cache wipe

**What to do instead:**

1. Clean only your own worktree's build output, e.g. `cargo clean` from
   your worktree root. Never touch a path outside your worktree.
2. If that isn't enough, stop and tell your orchestrator:

   ```bash
   ninox send <orchestrator> "Disk full: <what failed>. Freed my own target/; need more space."
   ```

   Wait for it to free space. Don't try to clean up yourself.

Ninox frees finished sessions' build output on its own, and runs its own
safe cleanup when free space drops below `[workspaces] disk_low_percent`.
