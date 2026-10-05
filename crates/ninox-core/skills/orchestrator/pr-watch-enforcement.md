---
name: pr-watch-enforcement
description: Explains the PreToolUse hook that denies raw gh CI/PR polling loops — register a ninox PR watch instead.
---

# PR Watch Enforcement

A `PreToolUse` hook denies the exact polling shapes the `watch-pr` skill
tells you never to run:

- `gh pr checks --watch`
- `gh run watch`
- a shell loop (`while`/`until`/`for`) or a `sleep` wrapped around
  `gh pr checks` / `gh pr view` / `gh pr status`

A single one-off `gh pr view` is still allowed — only the looping/watching
shapes are denied.

If a command gets blocked, the fix is the same one `watch-pr` already
describes: register a watch and let ninox deliver the notification instead
of polling for it.

    ninox open --pr <pr-url>
    ninox close --pr <pr-url>

This is enforcement, not just advice — the hook denies the tool call before
it runs, so it catches a polling loop even if you forget the `watch-pr`
guidance mid-session.
