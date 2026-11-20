---
name: brain
description: Read and write Ninox's shared knowledge brain. Use before touching code you haven't seen before, and before finishing your task.
---

# Read and Write the Brain

The brain is Ninox's persistent, shared knowledge store. By default you read
every enabled brain and can write to every enabled read-write one, unless your
orchestrator limited you to a subset (you inherit its limit). These commands
already act on whichever applies, with no extra configuration.

- `ninox brain query` searches every brain you read. With more than one,
  each result is tagged with its source (`source:id`, e.g.
  `team-wiki:repos/api.md`); `ninox brain show <id>` and
  `ninox brain related <id>` take that id. `--catalogue <name>` searches
  just one.
- `ninox brain add` writes to your write brain and prints back any other brain you could also copy the entry to; read what it prints.
- `ninox brain rules` looks up the write rules of the brains you can write to.

## Before exploring unfamiliar code

Query first — it blends keyword and semantic matches automatically:

```bash
ninox brain query "<name or concept>"
```

If a relevant entry exists, read it before you start digging through files
yourself. It may save you the exploration entirely. **Check the other
brains before concluding something isn't known** — if you narrowed with
`--catalogue`, re-run the query without it before exploring.

## Before you finish

Write down anything you discovered that the next session — orchestrator or
worker — would otherwise have to rediscover: where something lives, why
it's built the way it is, a gotcha you hit. Write it as a Markdown file
under the section that fits:

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

Each entry needs YAML frontmatter followed by a Markdown body. Use
`ninox brain add <path>` to write it — this indexes the entry immediately,
so it's queryable right away with no separate reindex step:

```bash
ninox brain add repos/my-crate.md <<'EOF'
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
EOF
```

### Rules

Write with `ninox brain add <path>` and read what it prints back. Copy an entry
to another brain only when that brain's rules fit it, never just because it has
no rules. Run
`ninox brain rules set` / `set-override` only when the user explicitly asks to
change a brain's rules; `brain add` refuses `_rules.md`.

If your write brain is remote-backed (team-shared), `ninox brain index` also
pushes your new entries to the team and pulls theirs — nothing extra to
do. If it reports a conflict copy (`*.conflict-*.md`), merge it into the
canonical entry and delete the copy when you're confident.

## The Rule

**Query before touching unfamiliar code. Write down what you found before
you're done.** A stale or empty brain is no better than no brain at all.
