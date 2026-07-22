# The Brain

The brain is Ninox's persistent knowledge store. As orchestrators explore codebases they discover things — where a type is defined, how two repositories relate, why an architectural decision was made, what error surfaces under a particular condition. Without a place to put that knowledge, every new session starts cold.

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

A SQLite full-text index sits alongside the files at `{brain_path}/.index.db`. It is derived entirely from the files and rebuilt on demand. This makes the brain importable: point it at any existing Markdown knowledge base (including Obsidian vaults) and run `ninox brain index` to make it queryable immediately.

## Configuration

By default, all orchestrators share a single global brain. This is intentional — knowledge discovered in one project is available to every other orchestrator without any configuration.

To use a different brain for a specific project, set `brain.path` in that project's `ninox.toml`:

```toml
[brain]
path = "./docs/brain"
```

The full resolution order is:

1. `brain.path` in the project's `ninox.toml` — overrides the shared brain for this project only
2. `brain.path` in `~/.config/ninox/config.toml` — changes the default for all orchestrators
3. `~/.config/ninox/brain/` — the shared brain used when nothing is configured

Each orchestrator uses exactly one brain. Brains are never merged — when a project specifies its own brain, it is fully isolated from the global one. This preserves clean boundaries between accounts, clients, and projects where knowledge should not cross over.

## Remote brains

A brain can be shared by a team. The canonical copy lives in S3 (or any
S3-compatible store — R2, MinIO); every machine keeps a full local mirror,
so queries stay local-speed. A brain directory is remote-backed when it
contains a `.sync.toml` (written by `ninox brain remote set`, or
materialized automatically from a `[[brain.catalogues]]` entry with a
`remote` field). Auth uses the standard AWS credential chain.

    [[brain.catalogues]]
    name = "team"
    path = "~/.config/ninox/brains/team"
    remote = "s3://team-brains/main"
    # endpoint = "https://<account>.r2.cloudflarestorage.com"
    # region = "eu-west-1"
    # cache_ttl_secs = 0   # 0 = freshness-check every lookup

How it stays in sync:

- **Lookups** (`query`, `show`, and the server's brain routes) first make
  one conditional GET of the remote's `manifest.json`. Unchanged manifest
  = one cheap 304 and zero downloads. Changed = only the changed entries
  are pulled, and only ones you haven't edited locally — a read never
  clobbers unpushed work. Raise `cache_ttl_secs` to trade freshness for
  latency. If the remote is unreachable, the local mirror answers with a
  warning — a query is never blocked by S3.
- **Writes** ride the habit you already have: `ninox brain index` pulls,
  resolves, pushes, then rebuilds. `ninox brain sync` does the same
  without a rebuild being the goal.
- **Conflicts** (two people edited the same entry since they last agreed)
  never lose knowledge: the remote version takes the entry's path and
  your version is kept — and shared — as `<entry>.conflict-<user>-<ts>.md`
  until someone merges the two and deletes it.

`.index.db`, `.sync.toml`, and `.sync-state.json` never leave the
machine; each mirror rebuilds its own index. Entry objects in the bucket
are immutable (`entries/<path>@<hash>`); `manifest.json` alone decides
what's current, and concurrent pushes are serialized by compare-and-swap
on it. Full design: `docs/superpowers/specs/2026-07-22-remote-brain-design.md`.

## CLI

```
ninox brain index                    rebuild the index from the brain files
ninox brain query <text>             full-text search; add --type or --tag to filter
ninox brain show <path>              print a single entry
ninox brain discover-repos [paths]   scan repos and write repos/ + relationships/ entries
ninox brain sync                     pull + push all changes to the brain's remote
ninox brain remote set <url>         attach an S3-compatible remote (--endpoint, --region, --ttl)
ninox brain remote status            remote URL, last sync, pending pushes, live conflicts
ninox brain remote unset             detach; the local copy stays a normal brain
```

`discover-repos` mechanically populates `repos/` and `relationships/` instead
of relying on an orchestrator to notice and write them by hand: given one or
more workspace paths (or, if none are given, every `workspace_path` the
session store has ever recorded — i.e. every repo a worker has been spawned
into), it derives each repo's canonical on-disk location, remote, purpose
(from its README/Cargo.toml/package.json), and entry points, then records any
mechanically detectable relationships — repos that are git worktrees of the
same underlying repository, and repos sharing a remote owner/org. It queries
the brain before writing, so re-running it updates existing entries rather
than duplicating them, and reindexes when done. It's a one-shot command today
— running it automatically (e.g. once per newly-seen `workspace_path` at
spawn time, the way [[brain-harvest]] triggers on PR-open) is a natural
follow-up, not yet implemented.

When defaulting to every recorded `workspace_path`, sessions can span more
than one catalogue: a session spawned against a non-default brain (its own
`NINOX_BRAIN`) has that path recorded as its `catalogue_path`, and discovery
writes that session's repo into its own catalogue rather than into this
invocation's default brain — the same per-session catalogue rule
[[brain-harvest]] follows. Passing explicit paths on the CLI always targets
this invocation's own resolved brain, matching `index`/`query`/`show`.

## For orchestrators

The intended loop is simple:

1. **Query first.** Before writing a new entry, check whether one already exists. Avoid duplicates.
2. **Write what you find.** Create or update a file in the appropriate section. Keep entries factual and concise. Run `ninox brain index` after writing.
3. **Query before unfamiliar work.** At the start of a session in a new area, query the brain for relevant repos, architecture, and patterns. Read the files it surfaces.

The brain grows incrementally. It does not need to be complete to be useful — even a handful of entries about key repositories and their relationships saves the next orchestrator meaningful exploration time.
