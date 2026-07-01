# The Brain

The brain is Athene's persistent knowledge store. As orchestrators explore codebases they discover things — where a type is defined, how two repositories relate, why an architectural decision was made, what error surfaces under a particular condition. Without a place to put that knowledge, every new session starts cold.

The brain solves this. Orchestrators write what they find into plain Markdown files. Those files accumulate into a shared second brain that any orchestrator can query before starting work in unfamiliar territory.

## How it works

Knowledge lives as Markdown files organised into sections:

```
brain/
  repos/          where repositories live, their purpose, entry points
  symbols/        where types, functions, and modules are defined
  concepts/       domain terminology and mental models
  patterns/       conventions and recurring implementation shapes
  decisions/      why something was built a certain way (ADRs)
  architecture/   how the system is structured — components, data flows
  relationships/  how repos, services, and teams connect
  errors/         known failure modes and how to resolve them
```

Each file is human-readable Markdown with a small YAML frontmatter header for structured metadata. The files are the source of truth — they can be committed, diffed, and read by anyone without tooling.

A SQLite full-text index sits alongside the files at `{brain_path}/.index.db`. It is derived entirely from the files and rebuilt on demand. This makes the brain importable: point it at any existing Markdown knowledge base (including Obsidian vaults) and run `athene brain index` to make it queryable immediately.

## Configuration

The brain path resolves in order:

1. `brain.path` in the project's `athene.toml` — for project-specific knowledge
2. `brain.path` in `~/.config/athene/config.toml` — the user's default brain
3. `~/.config/athene/brain/` — built-in fallback

Each orchestrator uses exactly one brain. Brains are never merged — this preserves clean boundaries between accounts, clients, and projects.

## CLI

```
athene brain index              rebuild the index from the brain files
athene brain query <text>       full-text search; add --type or --tag to filter
athene brain show <path>        print a single entry
```

## For orchestrators

The intended loop is simple:

1. **Query first.** Before writing a new entry, check whether one already exists. Avoid duplicates.
2. **Write what you find.** Create or update a file in the appropriate section. Keep entries factual and concise. Run `athene brain index` after writing.
3. **Query before unfamiliar work.** At the start of a session in a new area, query the brain for relevant repos, architecture, and patterns. Read the files it surfaces.

The brain grows incrementally. It does not need to be complete to be useful — even a handful of entries about key repositories and their relationships saves the next orchestrator meaningful exploration time.
