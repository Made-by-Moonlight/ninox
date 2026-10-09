---
name: restart-session
description: Restart your own live session in place to pick up a tooling-stack update — a newer `ninox` build, harness version, MCP config, or reseeded skills.
---

# Restart Yourself

If you need to pick up something installed since your session started — a
newer `ninox` build (after `ninox update`), an updated harness (Claude Code
CLI) version, changed MCP config, or reseeded skills — restart yourself:

```bash
ninox restart $NINOX_SESSION
```

This is detected as a self-restart: the command hands the actual restart off
to a detached background process and returns immediately, because killing
your own pane would otherwise kill this very command before it could
relaunch you. Expect the command to return right away with a "restarting in
background" line, then your pane to drop and come back within a few
seconds. Your worktree and workspace are untouched.

Your conversation resumes where it left off if your harness supports it —
you'll get a `[Ninox restart note]` telling you so. If it can't, you start
fresh and the note repeats your last known task summary; your worktree still
holds whatever work was done, so check `git status`/`git log`/your open PR
and continue from there instead of starting over.

Only restart yourself when the user asks for it, or your orchestrator tells
you a tooling update needs picking up — never on your own initiative.

This is **not** for recovering from a crash — if your session was
interrupted (reboot, terminal host crash), Ninox restores it on its own; see
the `fleet-recovery` skill.
