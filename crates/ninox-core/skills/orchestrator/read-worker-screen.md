---
name: read-worker-screen
description: Use when you need to see what a worker's terminal currently shows — a stuck prompt, a permission dialog, an error it hasn't reported.
---

# Read a Worker's Screen

Print what a session's terminal is showing right now, as plain text:

```bash
{{NINOX_BIN}} read <session-id>
```

Reach back into scrollback for the last N lines instead of just the visible
screen:

```bash
{{NINOX_BIN}} read <session-id> --lines 200
```

Add `--ansi` to keep colors and styles (escape sequences) — only useful when
piping into something that renders them.

## When to use it

- `worker-status list` shows a worker `blocked` or silent for a long time and
  its note doesn't explain why — read its screen to see what it is waiting on.
- A message you sent seems to have gone nowhere — check whether it is sitting
  unsubmitted in the worker's input box.
- A worker may be stuck on an interactive dialog (permission prompt, trust
  dialog, login) that only a human can answer — tell the user what it shows.

Reading is passive: it never types into or otherwise disturbs the session. To
act on what you see, use `{{NINOX_BIN}} send <session-id> "<message>"`.

Session ids are the ones `{{NINOX_BIN}} list` prints. A session that has
exited has no screen to read; the command fails and says so.
