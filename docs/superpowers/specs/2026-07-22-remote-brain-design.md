# Remote Brains: Team-Shared Knowledge over S3

## Context

The brain (`crates/ninox-core/src/brain.rs`, `docs/BRAIN.md`) is Ninox's
persistent knowledge store: Markdown files with YAML frontmatter as the
source of truth, plus a derived SQLite index (`.index.db`) rebuilt on
demand. Today every brain is a purely local directory. A brain can be
moved between machines only via `ninox brain export` / `import` tar.gz
archives — a manual, whole-brain, point-in-time hand-off with no notion
of staying in sync.

This spec adds **remote brains**: a brain shared by a team, canonically
stored in S3 (or any S3-compatible store), mirrored locally on each
machine for fast lookups, kept fresh by a sync check on lookup, and
writable by every team member.

## Goals

- A team member configures one catalogue block and gets the team brain:
  pulled locally, queryable at local speed, updated as teammates push.
- Every lookup (`query` / `show`) verifies the local mirror is up to
  date against S3 before running, by default — because the thing being
  validated is "has someone written knowledge about the project I'm
  looking at right now." A configurable cache TTL lets users trade
  freshness for latency.
- Everyone can write. Local writes are pushed by the existing
  `ninox brain index` step — the habit agents already have ("write
  files, then index") gains a push, with no new commands to remember.
- Concurrent edits to the same entry never lose knowledge: conflicts
  produce a conflict copy alongside the canonical entry.
- Brains without a remote behave byte-for-byte as today: no network, no
  new files, no behavior change.

## Non-goals

- No backends other than the S3-compatible API (a configurable endpoint
  covers AWS S3, Cloudflare R2, MinIO, GCS interop). The `RemoteStore`
  trait keeps the door open; nothing else ships now.
- No syncing of `.index.db` or embeddings — derived state is rebuilt
  locally on each machine, exactly as today.
- No automatic merge of conflicting edits — conflict copies are merged
  by a human or orchestrator at leisure.
- No history / audit log of past generations (an append-only journal
  was considered and rejected as overkill; see Alternatives).
- No background watcher/daemon — sync happens at lookup and at index
  time only, plus an explicit `ninox brain sync`.
- No server-mediated sync: each ninox process talks to S3 directly with
  its own credentials.

## Design

### 1. Configuration & identity

A brain directory becomes remote-backed by a `.sync.toml` file inside
it, next to `.index.db` (and, like it, gitignored and never synced):

```toml
remote = "s3://synthesia-brains/team"
endpoint = "https://<accountid>.r2.cloudflarestorage.com"  # optional
region = "eu-west-1"                                        # optional
cache_ttl_secs = 0   # 0 (default) = freshness-check every lookup
```

The brain directory self-describes. Any process that opens it — the CLI
via `NINOX_BRAIN`, the server, the app — discovers the remote from the
directory itself. No new environment variables, no plumbing through
spawn paths.

`[[brain.catalogues]]` entries (and the `[brain]` table for the default
brain) gain the same optional fields:

```toml
[[brain.catalogues]]
name = "team"
path = "~/.config/ninox/brains/team"
remote = "s3://synthesia-brains/team"
endpoint = "..."       # optional
region = "..."         # optional
cache_ttl_secs = 0     # optional
```

On first open of a catalogue with a `remote`, ninox materializes the
local directory, writes `.sync.toml` from the catalogue fields, and
performs an initial sync. Onboarding a teammate is pasting one config
block. If both the catalogue config and an existing `.sync.toml`
specify a remote, the `.sync.toml` wins and a mismatch logs a warning —
the directory is the source of truth once initialized.

Auth is the standard AWS credential chain (env vars, shared profiles,
SSO) via `aws-config` + `aws-sdk-s3`. The SDK is wrapped in a small
`RemoteStore` trait (conditional get, put, put-if-match, delete) so
unit tests run against an in-memory fake and future backends slot in
behind the same seam.

### 2. S3 layout

```
<prefix>/manifest.json                      # the consistency anchor
<prefix>/entries/repos/ninox.md@a1b2c3d4    # entry bytes, hash-suffixed
```

`manifest.json`:

```json
{
  "format": 1,
  "generation": 42,
  "entries": {
    "repos/ninox.md": {
      "sha256": "a1b2c3d4…",
      "size": 1234,
      "updated_by": "ethan.brodie",
      "updated_at": "2026-07-22T10:00:00Z"
    }
  }
}
```

Entry objects are **immutable**: the key embeds a short prefix of the
content hash, so a new version of an entry is a new object. Concurrent
writers can never corrupt each other's bytes; the manifest alone
decides which version is current. Keys stay path-organized so the
bucket remains human-browsable. An entry absent from the manifest is
deleted. Superseded objects linger until a later `gc` (out of scope for
v1; storage cost of stale markdown files is negligible).

Hashes are SHA-256 of the file content — stable across machines and
Rust versions, unlike the `DefaultHasher` used for the embedding cache
(which stays as-is; it never leaves one machine).

### 3. Sync engine

New module `crates/ninox-core/src/brain_sync.rs`.

A local sync-state file, `.sync-state.json` in the brain dir (derived
state, gitignored, never synced), records:

- the last-pulled manifest generation and its ETag, and
- a per-entry **base hash** — the content hash each local file had when
  it was last in agreement with the remote. Functionally a git index.

**Sync is a three-way diff per relative path** — base (sync state) vs
local (file on disk, hashed) vs remote (manifest):

| local vs base | remote vs base | action |
|---|---|---|
| unchanged | changed | **pull**: download blob, temp-file + rename into place |
| changed | unchanged | **push**: upload blob, then CAS manifest |
| changed | changed, same content | update base only |
| changed | changed, different | **conflict** (below) |
| deleted | unchanged | push deletion (drop from manifest) |
| unchanged | deleted (absent) | delete local file |
| deleted | changed | remote edit wins — file is resurrected, deletion logged |
| new (not in base) | absent | push |
| absent in base+local | new in remote | pull |
| new locally | new remotely, different | **conflict** |

**Conflict rule:** the remote version takes the canonical path. The
diverged local version is preserved as
`<stem>.conflict-<user>-<YYYYMMDD-HHMMSS>.md` in the same section
directory, indexed and queryable like any entry, and **pushed** on the
same sync so the whole team sees the conflict until someone merges and
deletes it. Knowledge is never silently lost. (Same model as Obsidian
Sync / Dropbox conflicted copies.)

**Push protocol:** upload all new entry objects first, then update the
manifest with a compare-and-swap (`PUT` with `If-Match: <etag>`;
supported by AWS S3, R2, and MinIO). A lost CAS (HTTP 412 — someone
else pushed in between) re-pulls the new manifest, re-runs the
three-way diff (which may now surface new conflicts), and retries,
bounded at 5 attempts with jitter. Immutable entry objects make this
retry loop safe: a half-finished loser has only added unreferenced
objects, never overwritten live ones.

`updated_by` / conflict-file user comes from `$USER` (fallback
`whoami`), matching the manifest field.

**Sync modes:**

- `pull_if_stale()` — the lookup path. One conditional GET of the
  manifest (`If-None-Match: <cached etag>`). A 304 costs one small
  round-trip and downloads nothing. A changed manifest triggers the
  pull side of the diff only — no pushes and no conflict copies from a
  read path. Crucially it only touches files whose local content still
  matches base: a locally-diverged file is skipped (served as-is) and
  left for the next full `sync()` to conflict-handle, so a read can
  never clobber unpushed local edits. Rebuilds the index if any files
  changed. Skipped entirely when the last check was within
  `cache_ttl_secs`.
- `sync()` — full pull + push + conflict handling, as above. Used by
  `ninox brain index` (order: pull, resolve, push, rebuild index) and
  `ninox brain sync`.

### 4. Lookup path integration

`BrainIndex` stays purely local and untouched. A thin wrapper —
`SyncedBrain::open(path)` in `ninox-core` — is the new front door for
every read entry point (`run_brain` Query/Show in
`crates/ninox-app/src/main.rs`, the server's brain routes):

1. Read `.sync.toml`. Absent → open `BrainIndex` directly (today's
   behavior, zero network).
2. Present → `pull_if_stale()`, then open `BrainIndex`.

**Failure policy:** any remote error — offline, DNS, auth, throttling —
logs one warning and falls back to the local mirror. A query is never
blocked or failed by S3 being unreachable. Auth misconfiguration is
surfaced distinctly (actionable message naming the credential chain)
but still degrades to local.

### 5. CLI surface

```
ninox brain remote set <s3://bucket/prefix> [--endpoint URL] [--region R] [--ttl SECS]
ninox brain remote status     # remote URL, last sync, pending pushes, live conflicts
ninox brain remote unset      # detach; local copy remains a normal local brain
ninox brain sync              # manual full sync (pull + push)
```

`remote set` writes `.sync.toml` and runs an initial full sync — for a
fresh bucket that publishes the local brain; for an existing remote it
pulls the team's entries (three-way diff handles overlap). `index`,
`query`, and `show` gain the behavior in §3–4 with no interface change.

### 6. Error handling summary

- Pulled files are written temp-file + rename; a crash never leaves a
  half-written entry.
- CAS retry bounded with jitter; exhaustion leaves local files intact
  and reports "push failed, retry with `ninox brain sync`".
- Remote unreachable at lookup → warn once, serve local.
- `.sync.toml`, `.sync-state.json`, `.index.db` are all appended to the
  brain's `.gitignore` (extending `ensure_gitignore`).
- A manifest with an unknown `format` number fails sync loudly (old
  ninox vs newer schema) without touching local files.

## Testing

- `RemoteStore` in-memory fake implementing real conditional-request
  semantics (ETags, `If-None-Match` → 304, `If-Match` → 412).
- Unit tests over the full three-way diff matrix in §3's table.
- CAS race test: two syncers push concurrently; loser retries and
  converges; conflict copy appears on both when edits collide.
- Conflict-copy naming, indexing, and push-back.
- TTL: within-TTL lookup makes zero `RemoteStore` calls.
- Offline fallback: store that errors on every call still serves local
  query results.
- First-open of a remote catalogue materializes dir + `.sync.toml` and
  pulls.
- Non-remote brain: `SyncedBrain::open` on a dir without `.sync.toml`
  makes zero network calls and matches `BrainIndex` behavior.
- Scale: sibling of the existing 500-file rebuild test proving the
  manifest diff stays fast.

## Alternatives considered

- **Snapshot archives** (reuse `brain_archive` tar.gz + version
  pointer): least new code, but whole-brain transfer per change and
  whole-brain last-writer-wins — per-entry conflict copies would
  require unpacking and diffing snapshots anyway.
- **Append-only journal** in S3: free history and natural conflict
  awareness, but needs compaction machinery and a much larger replay
  path. Overkill for a team knowledge base.
- **Git remote as the store**: free merge semantics, but diverges from
  the S3 requirement and drags git plumbing into every lookup.
- **Mutable path-keyed objects** (no hash suffix): simpler keys, but a
  writer that loses the manifest CAS after overwriting an object would
  corrupt the version the winner's manifest references. Immutability
  removes the race entirely.
