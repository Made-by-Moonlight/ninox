# GitHub Wiki Brain Backend (Ninox "v1" next-gen line)

**Status:** Design spec — not scheduled, no implementation. Lives on the
long-lived `v1` branch.

**Implementation target:** `ninox-core` (`brain_sync/`). File and line
references below reflect the code as of this writing; treat them as
orientation, not guaranteed locations.

## Context

Item 5 of the next-gen refactor. External brain storage already ships
via S3 (`brain_sync/{s3.rs,manifest.rs,mod.rs}`, per `docs/BRAIN.md`'s
"Remote brains" section and
`docs/superpowers/specs/2026-07-22-remote-brain-design.md`). This spec
covers the only genuinely new part: a second external backend, backed
by a GitHub wiki.

### What already exists — reuse, don't re-litigate

- A clean trait seam already exists: `brain_sync/store.rs` defines
  `RemoteStore`, a key-value, ETag-based conditional-request contract
  (`get(key, if_none_match)`, `put(key, bytes)`,
  `put_if_match(key, bytes, expected_etag)`). `S3RemoteStore` implements
  it; `BrainSync` (`mod.rs`) is built generically over `Arc<dyn
  RemoteStore>`, never against S3 directly. A GitHub-wiki backend plugs
  into this same trait.
- The entire sync model is already settled by the 2026-07-22 spec and
  is **not** revisited here: manifest-as-consistency-anchor
  (`generation`/`entries` map), immutable hash-suffixed entry keys
  (`entries/{rel}@{hash8}`) so concurrent writers can never corrupt
  bytes, a three-way diff (base/local/remote) per path, conflict policy
  (remote wins the canonical path; a local divergent copy is saved as
  `<stem>.conflict-<user>-<ts>.md`, indexed and pushed), push-then-CAS
  ordering (upload objects, then CAS the manifest via `If-Match`, retry
  on conflict with bounded attempts and jitter), and a warn-and-serve-
  local failure policy that never blocks. All of this is reused
  unchanged.
- The config shape (`[[brain.catalogues]]` with `remote = "s3://..."`,
  optional `endpoint`/`region`/`cache_ttl_secs`) extends naturally to
  `remote = "https://github.com/org/repo/wiki"` dispatching to a
  different `RemoteStore` impl, dropping the S3-specific
  `endpoint`/`region` fields.
- No `git2`/`gix` crate dependency exists anywhere in the workspace.
  Ninox already shells out to the `git` CLI extensively elsewhere
  (`events.rs`, `github.rs`, `lifecycle/brain_harvest.rs`,
  `lifecycle/repo_discovery.rs`, `fleet/probe.rs`,
  `lifecycle/poller.rs`) — a wiki backend should follow this established
  pattern rather than add a new git library dependency.
- The brain's file format (plain Markdown+YAML-frontmatter) is already
  git-friendly: `S3RemoteStore::put`/`get` store/retrieve raw bytes
  verbatim with no transformation beyond the hash-suffixed key name, so
  entries would survive byte-identically as literal files committed to
  a wiki repo. The one artifact with no git-native equivalent is
  `manifest.json` — addressed below.

### Why this doesn't reopen the prior rejection of "git as the store"

The 2026-07-22 spec's "Alternatives considered" explicitly rejected
making git the *primary* storage/sync model — replacing the manifest/
three-way-diff/CAS machinery with git's own merge semantics — because
that "drags git plumbing into every lookup." This spec proposes
something materially narrower: implementing the existing, already-
settled `RemoteStore` trait *on top of* git, while keeping the entire
sync engine exactly as designed. The "git plumbing" cost is paid once
per catalogue (via one persistent local clone, fetched on the existing
`cache_ttl_secs`/`pull_if_stale()` freshness cadence), not per lookup.
The prior rejection's reasoning was about a different, bigger proposal
and doesn't block this one.

## Goals

- A new `RemoteStore` implementor (e.g. `GithubWikiRemoteStore`) lets a
  `[[brain.catalogues]]` entry point at a GitHub wiki, reusing the
  entire existing sync engine unchanged.
- Shell out to `git`, following this codebase's established pattern —
  no new crate dependency.
- Resolve the one genuinely new design question the S3 backend didn't
  need to answer: how `put_if_match`'s CAS semantics map onto git's
  commit/push model, specifically for `manifest.json`.

## Non-goals

- Changing the sync engine (manifest format, three-way diff, conflict
  policy) — reused as-is, not redesigned.
- Making git the primary or only remote store model.
- Revisiting the 2026-07-22 spec's rejection of git-as-primary-store —
  that was a different, broader proposal; see above.

## Design

### `get`/`put`: a persistent local clone, not a clone-per-lookup

`GithubWikiRemoteStore` maintains one persistent local clone of the
wiki repo (`<repo>.wiki.git`) per configured catalogue, fetched on the
same freshness cadence `cache_ttl_secs`/`pull_if_stale()` already use
for S3. `get(key, if_none_match)` reads the file at that path from the
local clone's working tree. Since entries are immutable, hash-suffixed,
content-addressed keys, `if_none_match` is nearly always moot for them
— the content for a given key never changes. `put(key, bytes)` writes
the file, `git add`, `git commit`, `git push`. A non-fast-forward push
(a race with another writer) is retried with fetch+rebase — safe
specifically *because* entries are immutable hash-suffixed files, so a
rebase can never produce a real content conflict on an entry file.

### `put_if_match`: git-native CAS, for `manifest.json` specifically

`expected_etag` maps to a sha256 of the manifest file's last-observed
content (symmetric with how entry keys are already content-addressed,
and avoids depending on git commit-sha semantics for this). Before
writing: fetch, and check the current remote content's hash matches
`expected_etag`; if not, return the existing `PutOutcome::PreconditionFailed`
path unchanged — callers already implement the retry-with-backoff loop
for this, untouched by this spec. If it matches: write, commit, push.
A push rejection (a race landing after the pre-check) is treated as the
same conflict outcome, for defense in depth beyond the pre-check alone.

### Auth

Reuse whichever credential/token mechanism this codebase's existing
shell-`git`-against-GitHub call sites already use (see `github.rs`) —
no new auth mechanism invented for this backend specifically.

## Testing

- A trait-level conformance test suite run against `S3RemoteStore` (or
  its existing test double), `InMemoryRemoteStore`, and the new
  `GithubWikiRemoteStore` — all three honoring the identical
  `get`/`put`/`put_if_match` contract, so the sync engine's own tests
  don't need to know or care which backend is under them.
- A local, ninox-controlled bare git repo (not a real GitHub wiki) as
  the test fixture for `GithubWikiRemoteStore` itself, exercising the
  push-race/conflict path by simulating two concurrent writers.

## Open questions

- Exact auth/token source to use — left to the implementation plan,
  pending confirmation of which existing mechanism in `github.rs` fits.
- Local-clone storage location and cleanup/eviction policy.
- Whether a wiki-repo-specific size/history-growth concern needs
  addressing (a wiki's git history grows with every brain entry ever
  written, unlike S3 where old object versions are simply unreferenced)
  — not resolved here.
