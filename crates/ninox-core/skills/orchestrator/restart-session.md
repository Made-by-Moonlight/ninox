---
name: restart-session
description: Restart a live, healthy session (a worker, yourself, or the whole fleet) in place so it picks up a tooling-stack update — a newer `ninox` build, harness version, MCP config, or reseeded skills.
---

# Restart a Live Session

`{{NINOX_BIN}} restart` is for a session that is already running and healthy
but needs to pick up something installed since it started — a newer `ninox`
build (after `{{NINOX_BIN}} update`), an updated harness (Claude Code CLI)
version, changed MCP config, or reseeded skills. It kills the pane and
relaunches it under the same session id, same workspace/worktree:

```bash
{{NINOX_BIN}} restart <session-id>     # a worker, or yourself
{{NINOX_BIN}} restart --all            # every live session
```

Its conversation resumes wherever the harness supports `--resume` — it
continues exactly where it left off, just under the new binary/config. Where
the harness can't resume, it restarts fresh and is re-sent its last known
task summary so it doesn't lose track of what it was doing (its
worktree/workspace is untouched either way).

This is **not** for a crashed or interrupted session — that is
`{{NINOX_BIN}} fleet restore` (see the `fleet-recovery` skill), which only
acts on sessions that are already dead. `restart` only ever touches sessions
that are currently live.

## Restarting yourself

Restarting the session this command runs inside is a safe special case:
`{{NINOX_BIN}} restart <your-own-id>` detects it and hands the actual
restart off to a detached background process before returning, since the
in-progress command would otherwise be killed along with the pane before it
could relaunch. Expect the command to return immediately with a
"restarting in background" line, then your pane to drop and come back
within a few seconds — you do not need to do anything else.

## When to use it

Only restart a session when the user asks for it, or when you are told a
tooling update needs picking up — never on your own initiative. A restart
interrupts whatever the session was doing mid-turn (it finishes resuming at
the next turn, not instantly), so treat it the same way you'd treat
`{{NINOX_BIN}} fleet restore`: a deliberate action, not something to run
speculatively.
