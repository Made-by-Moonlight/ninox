---
name: brain
description: Read and write Ninox's shared knowledge brain. Use before touching code you haven't seen before, and before finishing your task.
---

# Read and Write the Brain

The brain is Ninox's persistent, shared knowledge store. Your session's
brain is already resolved via `NINOX_BRAIN` — these commands act on it with
no extra configuration.

## Before exploring unfamiliar code

Query first — it blends keyword and semantic matches automatically:

```bash
ninox brain query "<name or concept>"
```

If a relevant entry exists, read it before you start digging through files
yourself. It may save you the exploration entirely.

## Before you finish

Write down anything you discovered that the next session — orchestrator or
worker — would otherwise have to rediscover: where something lives, why
it's built the way it is, a gotcha you hit. Create or update a Markdown
file under the section that fits:

```
repos/          where repositories live, their purpose, entry points
symbols/        where types, functions, and modules are defined
concepts/       domain terminology and mental models
patterns/       conventions and recurring implementation shapes
decisions/      why something was built a certain way (ADRs)
architecture/   how the system is structured — components, data flows
relationships/  how repos, services, and teams connect
errors/         known failure modes and how to resolve them
```

Each file needs YAML frontmatter followed by a Markdown body:

```markdown
---
type: repo
name: my-crate
tags: [auth, core]
repos: [my-crate]
updated: 2026-07-06
---

# my-crate

Entry point: `src/main.rs`
Build: `cargo build`

Facts, not prose. Link related entries with `[[other-entry]]`.
```

Then rebuild the index so the write becomes queryable:

```bash
ninox brain index
```

If this brain is remote-backed (team-shared), `ninox brain index` also
pushes your new entries to the team and pulls theirs — nothing extra to
do. If it reports a conflict copy (`*.conflict-*.md`), merge it into the
canonical entry and delete the copy when you're confident.

## The Rule

**Query before touching unfamiliar code. Write down what you found before
you're done.** A stale or empty brain is no better than no brain at all.
