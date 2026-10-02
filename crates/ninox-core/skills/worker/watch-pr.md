---
name: watch-pr
description: register extra GitHub PRs for consolidated watching instead of polling gh.
---

# Watch PRs Without Polling

Your own PR is watched automatically by ninox — do not register it
yourself; ninox already delivers merge/CI-failure/changes-requested
notifications into your session for it.

## Never poll GitHub in a loop

Do not run `gh pr checks --watch`, `gh run watch`, or repeated `gh pr view`
calls in a loop. Every one of those burns the shared GitHub API rate limit
for the whole fleet.

## Watching an additional PR

If you need to track a PR that isn't yours — a dependency PR, a teammate's
PR you're blocked on — register a watch instead:

```bash
ninox open --pr <pr-url>
```

ninox delivers merge/CI-failure/changes-requested notifications into your
session for that PR. When you stop caring:

```bash
ninox close --pr <pr-url>
```

Watches also auto-close on their own once the PR merges or closes — closing
early is only needed if you lose interest before that.

To see everything you're currently watching:

```bash
ninox list --prs
```

## If watching isn't available

If `ninox open --pr` warns that `pr_watch` is disabled, the watch is not
active. Fall back to checking `gh pr view` sparingly — single one-off
calls, never a watch loop.
