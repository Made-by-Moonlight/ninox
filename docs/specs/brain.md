# Brain — Knowledge Index for Orchestrators

## Overview

The brain is a persistent, file-based knowledge store that orchestrators use to record and retrieve discovered facts about codebases — where things are defined, how repos relate, architectural decisions, known errors, and so on. It grows incrementally as agents explore.

Facts are stored as Markdown files (human-readable, diffable, git-committable). A SQLite index is built from those files on demand to enable fast querying without scanning the whole tree.

---

## Brain Resolution

Each orchestrator uses exactly **one** brain. Brains are never merged — they maintain clean account and project boundaries.

Resolution order:

1. `brain.path` in the project-level `ninox.toml` — project-specific brain
2. `brain.path` in `~/.config/ninox/config.toml` — user-configured global default
3. `~/.config/ninox/brain/` — built-in fallback

---

## Configuration

### Global (`~/.config/ninox/config.toml`)

```toml
[brain]
path = "~/my-second-brain"   # optional — override the default location
```

`BrainConfig` is added as an optional field on `AppConfig`:

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BrainConfig {
    pub path: Option<PathBuf>,
}

pub struct AppConfig {
    // ... existing fields ...
    #[serde(default)]
    pub brain: BrainConfig,
}
```

`AppConfig::resolved_brain_path()` implements the resolution order above, using `dirs::config_dir()` for the fallback.

### Per-project (`ninox.toml`)

```toml
[brain]
path = "./docs/brain"   # optional — use a project-local brain
```

The project config is looked up by walking up from cwd, finding the nearest `ninox.toml`. If absent, the global config is used.

---

## File Structure

```
brain/
  repos/          # Facts about repositories: purpose, entry points, build commands, key deps
  symbols/        # Code definitions: where declared, which repos use them
  concepts/       # Domain knowledge and terminology
  patterns/       # Conventions and recurring implementation approaches
  decisions/      # ADRs — why something was built a certain way
  architecture/   # How things are structured: component maps, data flows, layer diagrams
  relationships/  # Cross-repo and cross-concept links
  errors/         # Known failure modes and resolutions
```

Each file uses YAML frontmatter for structured metadata and Markdown for free-form content:

```markdown
---
type: repo
name: my-crate
tags: [auth, core]
repos: [my-crate]
updated: 2026-07-01
---

# my-crate

Entry point: `src/main.rs`
Build: `cargo build`

Responsible for authentication token issuance. Depends on `crypto-utils`.
```

Wikilinks (`[[other-entry]]`) can reference other brain files.

---

## SQLite Index

The index is **derived** from the files — the files are the source of truth. It is never modified directly.

### Implementation

A `BrainIndex` struct lives in `crates/ninox-core/src/brain.rs`, separate from `Store`. It follows the same patterns as `Store`: `Mutex<Connection>`, WAL mode, `anyhow::Result`.

```rust
pub struct BrainIndex {
    conn: Mutex<Connection>,
    brain_path: PathBuf,
}

impl BrainIndex {
    pub fn open(brain_path: impl AsRef<Path>) -> Result<Self>;
    pub fn rebuild(&self) -> Result<usize>;          // walk files, repopulate; returns entry count
    pub fn query(&self, text: &str, filters: QueryFilters) -> Result<Vec<BrainEntry>>;
    pub fn get(&self, id: &str) -> Result<Option<BrainEntry>>;
}

pub struct QueryFilters {
    pub entry_type: Option<String>,   // repos | symbols | concepts | ...
    pub tag: Option<String>,
}

pub struct BrainEntry {
    pub id: String,          // relative path from brain root
    pub entry_type: String,
    pub name: String,
    pub tags: Vec<String>,
    pub repos: Vec<String>,
    pub updated: Option<String>,
    pub body: String,
}
```

### Schema

```sql
CREATE TABLE IF NOT EXISTS entries (
    id          TEXT PRIMARY KEY,   -- relative path from brain root
    type        TEXT,               -- repos | symbols | concepts | ...
    name        TEXT,
    tags        TEXT,               -- JSON array
    repos       TEXT,               -- JSON array
    updated     TEXT,
    body        TEXT                -- full file content for full-text search
);

CREATE VIRTUAL TABLE IF NOT EXISTS entries_fts
    USING fts5(name, tags, body, content=entries, content_rowid=rowid);
```

### Index Location

```
{brain_path}/.index.db
```

A `.gitignore` entry for `.index.db` is written to the brain root on first `rebuild()` if not already present.

---

## Server Routes

New routes added to `crates/ninox-server/src/routes/brain.rs`:

```
POST /api/brain/index          — rebuild index (triggers BrainIndex::rebuild)
GET  /api/brain/query?q=&type=&tag=  — search entries
GET  /api/brain/entry/*path    — fetch a single entry by relative path
```

The `AppState` in `ninox-server` gains a `brain: Arc<BrainIndex>` field, initialised on server startup using the resolved brain path from `AppConfig`.

---

## CLI Commands

CLI subcommands added to `crates/ninox-app`:

```
ninox brain index              # rebuild index for the active brain
ninox brain index --watch      # rebuild on file change (notify crate)
ninox brain query <text>       # full-text search
ninox brain query --type repo  # filter by section
ninox brain query --tag auth   # filter by tag
ninox brain show <path>        # print a single entry to stdout
```

---

## Orchestrator Skill

The skill at `skills/brain/SKILL.md` (in the TypeScript monorepo's `skills/` directory) teaches orchestrators how to interact with the brain via the server API or CLI. The three operations are:

### 1. Query before writing

Before recording a new fact, check whether it already exists:

```
ninox brain query "<name or concept>"
```

If a relevant entry exists, update it rather than creating a duplicate.

### 2. Write a fact

Create or update a Markdown file in the appropriate section. Follow the frontmatter schema. Keep the body concise — facts over prose. Then rebuild the index:

```
ninox brain index
```

### 3. Read for context

When starting work in an unfamiliar area, query the brain first:

```
ninox brain query --type architecture
ninox brain query --type repo <name>
```

Follow the returned path to read the full file.

---

## Import

Pointing `brain.path` at an existing Obsidian vault or Markdown knowledge base and running `ninox brain index` is sufficient to make it queryable. Entries without Ninox frontmatter are indexed by filename and body text only.

---

## Dependencies

New crate dependencies required:

| Crate | Used for |
|-------|----------|
| `serde_yaml` or `gray_matter` | Frontmatter parsing |
| `walkdir` | Recursive directory traversal during `rebuild()` |
| `notify` | File-watch mode (`--watch` flag) |

Add to `[workspace.dependencies]` in root `Cargo.toml`, then reference with `workspace = true` in the consuming crate's `Cargo.toml`.
