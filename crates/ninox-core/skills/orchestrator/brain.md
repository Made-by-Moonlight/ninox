---
name: brain
description: Read and write Ninox's shared knowledge brain. Use before exploring unfamiliar code (query first) and as soon as you learn something worth keeping — write it down, don't wait until the end.
---

# Read and Write the Brain

The brain is Ninox's persistent, shared knowledge store. As you explore
codebases you discover things — where a type is defined, how two repos
relate, why a decision was made. Without a place to put that, every new
session starts cold. Write it down so the next orchestrator doesn't have to
rediscover it.

Your session's brain is already resolved — these commands act on it with no
extra configuration.

## 1. Query first

`brain query` blends keyword and semantic matches automatically — no new
syntax needed. Before writing a new entry, check whether one already exists:

```bash
{{NINOX_BIN}} brain query "<name or concept>"
```

Narrow with filters:

```bash
{{NINOX_BIN}} brain query "<text>" --entry-type repo
{{NINOX_BIN}} brain query "<text>" --tag auth
```

If a relevant entry exists, update it instead of creating a duplicate.

## 2. Write a fact

Create or update a Markdown file under the section that fits, then rebuild
the index:

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

Each file needs YAML frontmatter followed by Markdown body:

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
{{NINOX_BIN}} brain index
```

## 3. Read for context

At the start of work in unfamiliar territory, query before exploring:

```bash
{{NINOX_BIN}} brain query "" --entry-type architecture
{{NINOX_BIN}} brain query "" --entry-type repo
{{NINOX_BIN}} brain show <path-from-a-query-result>
```

## The Rule

**Before exploring anything unfamiliar, query first.** As soon as you learn
something a future session would want to know — don't wait until the end
of your session — write it down and index it. A stale or empty brain is no
better than no brain at all.
