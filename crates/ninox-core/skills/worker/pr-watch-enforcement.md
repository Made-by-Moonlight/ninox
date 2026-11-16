---
name: pr-watch-enforcement
description: Explains the PreToolUse hook that denies raw gh CI/PR polling loops — use delivered PR events and `ninox pr status` instead.
---

# PR Watch Enforcement

A `PreToolUse` hook denies the exact polling shapes the `watch-pr` skill
tells you never to run:

- `gh pr checks --watch`
- `gh run watch`
- a shell loop (`while`/`until`/`for`) or a `sleep` wrapped around
  `gh pr checks` / `gh pr view` / `gh pr status`

A single one-off `gh pr view` is still allowed; only the looping/watching
shapes are denied.

If a command gets blocked, you don't need it:

- Ninox already delivers every change on your own PR as a `[pr_event]`
  message: CI passed/failed/stuck, review comments (bots included),
  approvals, changes requested, conflicts, behind base, ready for review,
  PR comments and close. The `watch-pr` skill lists them all and what to do
  on each.
- To read the current state, run `ninox pr status`. It answers from Ninox's
  cache and makes no GitHub call.
- For a PR that isn't yours, register a watch and get the same events:

      ninox open --pr <pr-url>
      ninox close --pr <pr-url>

This is enforcement, not just advice: the hook denies the tool call before
it runs, so it catches a polling loop even if you forget the `watch-pr`
guidance mid-session.
