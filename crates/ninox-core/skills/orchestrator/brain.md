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

## Your brains

By default your session reads every enabled brain and can write to every
enabled read-write one; your local brain gets every entry. A session can be
**limited** to a subset when it is spawned, shown in its header as
"Brains: all" or "Brains: limited to A, B · writes A". The commands below
already act on whichever applies, with no extra configuration.

- `brain query`, `show`, `related` and `backlinks` search **every** brain
  you read. With more than one, results are tagged `source:id` (e.g.
  `team-wiki:repos/api.md`) — pass that id straight to `show`/`related`.
  Add `--catalogue <name>` to search just one.
- `brain add` writes to your write brain and prints back any other brain you could also copy the entry to; read what it prints.
- `brain rules` looks up the write rules of the brains you can write to.
- `brain index` / `brain sync` act on your write brain; `--all` covers your
  whole set (read-only brains are pulled, never pushed).
- Workers and sub-orchestrators you spawn inherit your limit (or have none).
  Narrow it for a child with `--brain <name>` (repeatable) and
  `--brain-write <name>` — only ever to brains you read yourself. Change a
  running worker's limit with `ninox brain limit <session> --set a,b` or
  `--clear`.

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

### Rules

Write with `{{NINOX_BIN}} brain add <path>` and read what it prints back. Copy an entry
to another brain only when that brain's rules fit it, never just because it has
no rules. Run
`{{NINOX_BIN}} brain rules set` / `set-override` only when the user explicitly asks to
change a brain's rules; `brain add` refuses `_rules.md`.

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
