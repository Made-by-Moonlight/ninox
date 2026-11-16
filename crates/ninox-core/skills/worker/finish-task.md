---
name: finish-task
description: how and when to end your own session with `ninox complete` once your whole task, including post-merge steps, is done.
---

# Finishing Your Task

You decide when you are done. A merged PR alone does not end your session:
your task may have steps after the merge (verify a deploy, update a ticket,
report back). When every item in your Definition of done is met **and** any
post-merge steps in your task are finished, run:

```bash
ninox complete --summary "<what you did>" [--evidence <url|path>]...
```

Make it your **last action**. It reports to your orchestrator and **ends
this session**: Ninox removes your worktree shortly after. Do not run it
while you still have work to do.

- It refuses on a dirty or unpushed worktree. Commit and push first; use
  `--force` only if you are deliberately discarding that work.
- If you are blocked or unsure whether you are done, tell your orchestrator
  with `ninox send` instead of completing.
- When your PR merges you get an `[Ninox] Update on your PR` message. If the
  task is complete, finish; if it has steps left, do them first.
- If you stay idle after your PR merged, Ninox sends one reminder to finish
  or to report what is left.
