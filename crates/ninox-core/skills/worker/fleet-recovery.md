---
name: fleet-recovery
description: what to do when Ninox resumes or restarts you after a reboot or crash.
---

# Fleet Recovery

If your session was interrupted (machine reboot, terminal host crash), Ninox
restores it and sends you a note starting with `[Ninox recovery note]`.

- **Resumed** (conversation intact): continue your task from where you left
  off. The note states your worktree, branch, uncommitted changes and PR/CI
  state as Ninox recorded them.
- **Restarted fresh** (your harness could not resume the conversation): the
  briefing repeats your original task brief. Your worktree still holds the
  work done so far — inspect it (`git status`, `git log`, your open PR) and
  continue from its current state. Do not start over.

If the note does not match what you find in the worktree, tell your
orchestrator before doing anything else:

```bash
ninox send <orchestrator-id> "<what looks wrong>"
```

`ninox fleet status` shows what Ninox knows about the fleet.
