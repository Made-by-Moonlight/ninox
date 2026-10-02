---
name: watch-pr
description: Register a GitHub PR for consolidated watching so you get pinged on merge, CI failures, and review activity instead of polling gh yourself.
---

# Watching a PR

NEVER poll GitHub for PR/CI state in a loop (`gh pr checks --watch`,
`gh run watch`, repeated `gh pr view`) — it burns the shared API rate
limit. Register a watch instead:

    ninox open --pr <pr-url>

Ninox's poller then delivers merge, CI-failure, and changes-requested
notifications straight into your session. When you stop caring:

    ninox close --pr <pr-url>

Watches auto-close when the PR merges or closes. `ninox list --prs`
shows active watches. Workers' own PRs are watched automatically — this
is for *additional* PRs (a dependency PR, a teammate's PR, a PR you
opened outside ninox).
