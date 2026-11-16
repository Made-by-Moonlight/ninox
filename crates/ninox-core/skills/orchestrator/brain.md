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

## Your brains: read many, write one

Your session has a **brain set**: one or more brains it *reads*, and exactly
one it *writes* to. It was chosen when you were spawned and is shown in your
session header ("Brains: reads A, B · writes A"); the commands below already
act on it with no extra configuration.

- `brain query`, `show`, `related` and `backlinks` search **every** brain
  you read. With more than one, results are tagged `source:id` (e.g.
  `team-wiki:repos/api.md`) — pass that id straight to `show`/`related`.
  Add `--catalogue <name>` to search just one.
- `brain add` writes to your write brain. `--catalogue <name>` targets
  another writable brain you read; read-only brains and brains outside your
  set are refused.
- `brain index` / `brain sync` act on your write brain; `--all` covers your
  whole set (read-only brains are pulled, never pushed).
- Workers and sub-orchestrators you spawn inherit your set. Narrow it for a
  child with `--brain <name>` (repeatable) and `--brain-write <name>` — only
  ever to brains you read yourself.

**Check the other brains before concluding something isn't known.** If a
query of one brain comes up empty, the answer may well be in another one
you read — query without `--catalogue` before you go exploring.

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

Write a Markdown entry under the section that fits with `brain add` (it
indexes immediately), or edit the file under your write brain and rebuild
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

Write it with:

```bash
{{NINOX_BIN}} brain add repos/my-crate.md --content "$(cat my-crate.md)"
```

or, after editing the file by hand, rebuild the index so it becomes
queryable:

```bash
{{NINOX_BIN}} brain index
```

## 3. Read for context

At the start of work in unfamiliar territory, query before exploring:

```bash
{{NINOX_BIN}} brain query "" --entry-type architecture
{{NINOX_BIN}} brain query "" --entry-type repo
{{NINOX_BIN}} brain show <id-from-a-query-result>
{{NINOX_BIN}} brain related <id-from-a-query-result>
```

## The Rule

**Before exploring anything unfamiliar, query first.** As soon as you learn
something a future session would want to know — don't wait until the end
of your session — write it down and index it. A stale or empty brain is no
better than no brain at all.
