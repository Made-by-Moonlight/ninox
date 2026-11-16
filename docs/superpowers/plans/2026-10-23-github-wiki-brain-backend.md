# GitHub Wiki Brain Backend Implementation Plan

> **For agentic workers:** This plan is executed natively, in-session, by
> the same worker that wrote it (single ninox worker, single PR, no
> subagent fan-out) — see `superpowers:executing-plans`.

**Goal:** Add `GithubWikiRemoteStore`, a second `RemoteStore` implementor
backed by a persistent local clone of a GitHub wiki repo (shelling out to
`git`), so a `[[brain.catalogues]]` entry can point at a GitHub wiki
instead of S3, reusing the entire sync engine unchanged.

**Architecture:** One new file, `crates/ninox-core/src/brain_sync/github_wiki.rs`,
implementing `RemoteStore` against a persistent local clone. Reads use
`git fetch` + `git show origin/<branch>:<key>` (never touches the working
tree). Writes use `git fetch` + `git checkout -B <branch> origin/<branch>`
(or an orphan branch if the wiki has no commits yet) + write + `git add` +
`git commit` + `git push`, with the two CAS paths from the spec: `put`
(entries) retries a rejected push with fetch+rebase; `put_if_match`
(manifest) does a content-hash pre-check and treats any push rejection as
`PreconditionFailed` with no internal retry (the engine's own `sync()`
loop already retries from scratch). `BrainSync::for_brain` (mod.rs) picks
`S3RemoteStore` vs `GithubWikiRemoteStore` by `cfg.remote`'s scheme.

**Tech Stack:** Rust, `tokio::process::Command` shelling out to the `git`
CLI (no `git2`/`gix`), `sha2` (already a dep) for content-hash ETags,
`dirs::cache_dir()` for the local clone location.

**Spec:** `docs/superpowers/specs/2026-10-21-github-wiki-brain-backend-design.md`

## Resolution of the spec's three open questions

**1. Auth/token source.** Investigated every shell-`git` call site the
task description named (`events.rs`, `github.rs`, `lifecycle/brain_harvest.rs`,
`lifecycle/repo_discovery.rs`, `fleet/probe.rs`, `lifecycle/poller.rs`,
plus `ninox-ui-shared/src/spawn_util.rs`). Finding: **none of them ever
run `git clone`, `git fetch`, or `git push` against a GitHub remote.**
Every shelled-out `git` invocation in the codebase today is purely local
(`worktree add`, `commit`, `add`, `branch`, `status`, `checkout`,
`rev-parse`, `diff`, `config`, `init`, one `merge-base`) against an
already-cloned, already-authenticated checkout a human set up. `github.rs`
is a REST client (Bearer token via `resolve_token`: config → `GITHUB_TOKEN`
env → `gh auth token`) used only for the GitHub API, never for git
network operations. So the spec's assumption that an existing
shell-`git`-against-GitHub convention exists to copy doesn't hold — this
is the first time ninox itself needs to authenticate an outbound git
network call. Resolution: **rely on the ambient git credential
configuration already on the host** (SSH agent, or `gh`'s git credential
helper from `gh auth login`, or a stored HTTPS credential) — i.e. pass no
explicit token and let `git`/`libcurl` resolve credentials exactly as it
would for a human running `git push` from the same machine. This is
consistent with the fact that nothing else in ninox has ever needed to
inject a token into a git subprocess. No new auth mechanism is invented.

**2. Local-clone storage location and eviction.** The only existing
precedent for "persistent, re-derivable local cache state" is
`fastembed_cache_dir()` (`crates/ninox-core/src/embeddings.rs:182-187`):
`dirs::cache_dir()/ninox/fastembed`. This backend follows the same
convention: `dirs::cache_dir()/ninox/brain-wiki/<owner>-<repo>`, one
directory per distinct wiki repo (so multiple catalogues pointing at
different wikis don't collide). No eviction policy is needed now: the
clone is fully re-derivable (re-clone on next use if missing), small
(markdown-only wiki), and `put`/`put_if_match` complete their
write-commit-push sequence synchronously within one call, so there's no
window where deleting the cache mid-operation loses unpushed work beyond
what's already true of any process being killed mid-git-command. A
follow-up could teach `cleardisk` about `ninox/brain-wiki/*`; out of scope
here.

**3. Wiki history growth.** Deferred, per the spec's explicit allowance.
Brain entries are small markdown facts, not binaries; a wiki's git history
growing by one commit per write is not a practical size concern at brain
scale, and GitHub doesn't meter wiki repos against a history-size quota
the way it does for Git LFS. If this becomes a real problem, a future
fix (e.g. periodic history squash) is a separate, independently
shippable change — not blocking this one.

## Global Constraints

- No new crate dependency (`git2`/`gix`) — shell out to `git` via
  `tokio::process::Command`, matching the codebase's existing pattern.
- Do not touch the manifest format, three-way diff, or conflict policy —
  `brain_sync/{manifest,diff}.rs` and the engine logic in `mod.rs` are
  reused byte-for-byte unchanged.
- `RemoteStore`'s `get`/`put`/`put_if_match` contract (`store.rs:31-40`)
  must be honored exactly as `InMemoryRemoteStore` and `S3RemoteStore`
  already honor it — same `GetResponse`/`PutOutcome` semantics.
- ETags are sha256 hex of file content (spec: "a sha256 of the manifest
  file's last-observed content... symmetric with how entry keys are
  already content-addressed") — reuse `manifest::sha256_hex`, don't
  invent a second hash.
- Entry keys are immutable and content-addressed
  (`entries/{rel}@{hash8}`) — this is what makes fetch+rebase retries for
  `put` safe; don't add any logic that assumes otherwise.

## Review Focus

- **Empty/uninitialized wiki** (brand-new catalogue, GitHub wiki with zero
  pages ever created): clone fails ("repository not found"-style error).
  Must fall back to `git init` + `remote add origin` and treat it as an
  unborn branch, so the first `put`/`put_if_match` initializes the wiki on
  push rather than crashing.
- **Concurrent writers racing on the manifest** (two processes/catalogue
  clones both call `put_if_match` against the same wiki): the loser must
  get `PreconditionFailed` — either from the pre-check or from a rejected
  push — never silently overwrite or crash.
- **Concurrent entry pushes** (two writers `put` different new entries at
  the same time): both must land — a non-fast-forward push must be
  resolved via fetch+rebase and retried, not surfaced as an error.
- **`get` of a key that was never written** (manifest absent on first
  ever sync, or a specific entry key that doesn't exist): must return
  `GetResponse::NotFound`, not an error — `git show` on a missing path
  must be distinguished from a real git failure.
- **Conditional get with a matching etag** (`pull_if_stale`'s fast path):
  must return `NotModified` without writing anything, exactly like
  `InMemoryRemoteStore`/`S3RemoteStore`.

---

## File Structure

- Create: `crates/ninox-core/src/brain_sync/github_wiki.rs` —
  `GithubWikiRemoteStore` (the new `RemoteStore` impl) plus its own unit
  tests using a local bare git repo fixture.
- Create: `crates/ninox-core/src/brain_sync/conformance.rs` — the
  trait-level test suite (`#[cfg(test)]`, generic over `Arc<dyn
  RemoteStore>`) run against all three implementors.
- Modify: `crates/ninox-core/src/brain_sync/mod.rs` — `pub mod
  github_wiki;`, and `BrainSync::for_brain` dispatches on `cfg.remote`'s
  scheme instead of hardcoding `s3::S3RemoteStore::from_config`.
- Modify: `crates/ninox-core/src/brain_sync/config.rs` — no field changes
  needed (`SyncToml` is already backend-agnostic: `remote`/`endpoint`/
  `region`/`cache_ttl_secs`); add a small `SyncToml::is_github_wiki(&self)
  -> bool` (or equivalent scheme check) that `mod.rs`'s dispatch calls, so
  the "what kind of remote is this" logic lives next to `bucket_and_prefix`
  rather than being duplicated ad hoc in `mod.rs`.

## Task 1: `SyncToml` scheme dispatch

**Files:**
- Modify: `crates/ninox-core/src/brain_sync/config.rs`

**Interfaces:**
- Produces: `SyncToml::is_github_wiki(&self) -> bool` — true iff `remote`
  starts with `https://github.com/` and ends with `/wiki` (optionally
  trailing `/` or `.git`... normalize to the one form the spec names:
  `https://github.com/org/repo/wiki`).

- [ ] **Step 1: Write the failing test** in `config.rs`'s test module:
  ```rust
  #[test]
  fn is_github_wiki_detects_wiki_remote() {
      let s3 = SyncToml { remote: "s3://bucket/prefix".into(), ..sample() };
      assert!(!s3.is_github_wiki());
      let wiki = SyncToml { remote: "https://github.com/org/repo/wiki".into(), ..sample() };
      assert!(wiki.is_github_wiki());
  }
  ```
- [ ] **Step 2: Run** `cargo test -p ninox-core brain_sync::config:: -- --nocapture`;
  expect FAIL (method doesn't exist).
- [ ] **Step 3: Implement** `is_github_wiki` on `SyncToml`.
- [ ] **Step 4: Run the same test; expect PASS.**
- [ ] **Step 5: Commit** (`git add`, local commit only — final squash/commit
  policy is Task 6).

## Task 2: `GithubWikiRemoteStore` — clone/init, path layout, branch detection

**Files:**
- Create: `crates/ninox-core/src/brain_sync/github_wiki.rs`

**Interfaces:**
- Produces:
  - `pub struct GithubWikiRemoteStore { .. }`
  - `pub async fn GithubWikiRemoteStore::from_config(cfg: &SyncToml) -> Result<Self>`
  - private `fn parse_wiki_remote(remote: &str) -> Result<(String, String)>`
    (owner, repo)
  - private `fn clone_dir_for(owner: &str, repo: &str) -> PathBuf` —
    `dirs::cache_dir()/ninox/brain-wiki/<owner>-<repo>`
  - private `async fn ensure_cloned(&self) -> Result<()>` — clones
    `https://github.com/{owner}/{repo}.wiki.git` into the clone dir if
    `.git` is absent; on clone failure, `git init` + `remote add origin`
    (unborn-wiki fallback) instead of erroring.
  - private `async fn branch(&self) -> Result<String>` —
    `git symbolic-ref --short HEAD` in the clone dir.

- [ ] **Step 1: Write the failing tests** (in a `#[cfg(test)] mod tests`
  at the bottom of the new file), using a local bare repo as the "GitHub"
  fixture (no network):
  ```rust
  fn init_bare_wiki_fixture(dir: &Path) { /* git init --bare -b master <dir> */ }

  #[tokio::test]
  async fn from_config_clones_existing_bare_repo() { .. }

  #[tokio::test]
  async fn from_config_initializes_when_bare_repo_is_empty_and_unreachable_by_clone() { .. }
  ```
  (`from_config` here takes the bare repo's local `file://` path as
  `cfg.remote` is normally a `github.com` URL — add a second, test-only
  constructor `GithubWikiRemoteStore::for_clone_url(clone_url: &str,
  clone_dir: PathBuf)` that `from_config` delegates to after resolving
  the real GitHub URL, so tests can point at a local fixture directly.)
- [ ] **Step 2: Run; expect FAIL** (types/fns don't exist).
- [ ] **Step 3: Implement** `parse_wiki_remote`, `clone_dir_for`,
  `for_clone_url`, `from_config`, `ensure_cloned`, `branch`.
- [ ] **Step 4: Run; expect PASS.**
- [ ] **Step 5: Commit.**

## Task 3: `get` — fetch + `git show`, ETag semantics

**Files:**
- Modify: `crates/ninox-core/src/brain_sync/github_wiki.rs`

**Interfaces:**
- Consumes: `ensure_cloned`, `branch` (Task 2); `manifest::sha256_hex`
  (existing, `pub fn sha256_hex(bytes: &[u8]) -> String`).
- Produces: `impl RemoteStore for GithubWikiRemoteStore { async fn
  get(&self, key: &str, if_none_match: Option<&str>) -> Result<GetResponse> }`
  plus the private `async fn fetch(&self) -> Result<()>` (debounced:
  skip if the last fetch was under 1s ago, to avoid refetching once per
  entry during a multi-entry `pull_if_stale`/`sync` pass) and `async fn
  show_remote_file(&self, key: &str) -> Result<Option<Vec<u8>>>` (`git
  show origin/<branch>:<key>`; a missing path or unborn-branch error from
  `git show`'s stderr maps to `Ok(None)`, any other nonzero exit bails).

- [ ] **Step 1: Write failing tests**: `get` of a missing key is
  `NotFound`; `get` after a direct push to the bare fixture (simulating a
  teammate) is `Found` with the right bytes; a second `get` passing the
  first etag as `if_none_match` is `NotModified`.
- [ ] **Step 2: Run; expect FAIL.**
- [ ] **Step 3: Implement** `fetch`, `show_remote_file`, `get`.
- [ ] **Step 4: Run; expect PASS.**
- [ ] **Step 5: Commit.**

## Task 4: `put` — write/add/commit/push, fetch+rebase retry on rejection

**Files:**
- Modify: `crates/ninox-core/src/brain_sync/github_wiki.rs`

**Interfaces:**
- Consumes: `ensure_cloned`, `fetch`, `branch`.
- Produces: `impl RemoteStore for GithubWikiRemoteStore { async fn
  put(&self, key: &str, bytes: Vec<u8>) -> Result<String> }` plus a
  private `enum PushOutcome { Pushed, Rejected }` and `async fn
  write_commit_push(&self, key: &str, bytes: &[u8]) -> Result<PushOutcome>`
  (checkout-or-create `<branch>` tracking `origin/<branch>` — or an
  orphan branch when `origin/<branch>` doesn't exist yet — write the
  file, `git add`, `git commit` treating "nothing to commit" as an
  already-converged no-op success, `git push`; a non-fast-forward
  rejection (stderr matched) returns `Rejected` rather than erroring).
  `put` retries `Rejected` up to 5 times with `git fetch` + `git rebase
  origin/<branch>` between attempts (bounded, matching the engine's own
  `MAX_ATTEMPTS = 5` convention in `mod.rs::BrainSync::sync`), then bails.

- [ ] **Step 1: Write failing tests**: a single `put` round-trips through
  a fresh `get`; two `GithubWikiRemoteStore`s cloned from the same bare
  fixture each `put` a different key concurrently (one after the other,
  simulating the race — see Task 6 for the fuller concurrent scenario) —
  both entries must be retrievable afterward with no data loss.
- [ ] **Step 2: Run; expect FAIL.**
- [ ] **Step 3: Implement** `write_commit_push`, `put`.
- [ ] **Step 4: Run; expect PASS.**
- [ ] **Step 5: Commit.**

## Task 5: `put_if_match` — content-hash CAS for the manifest

**Files:**
- Modify: `crates/ninox-core/src/brain_sync/github_wiki.rs`

**Interfaces:**
- Consumes: `fetch`, `show_remote_file`, `write_commit_push`,
  `manifest::sha256_hex`.
- Produces: `impl RemoteStore for GithubWikiRemoteStore { async fn
  put_if_match(&self, key: &str, bytes: Vec<u8>, expected_etag:
  Option<&str>) -> Result<PutOutcome> }` — fetch, hash-compare current
  remote content against `expected_etag` (`None` = create-only, matching
  `InMemoryRemoteStore`'s semantics exactly), `PreconditionFailed` on
  mismatch without writing; on match, one `write_commit_push` attempt —
  `Rejected` also maps to `PreconditionFailed` (no internal retry, per
  the spec).

- [ ] **Step 1: Write failing tests** mirroring
  `store.rs`'s `put_if_match_enforces_cas` test one-for-one against
  `GithubWikiRemoteStore` (create-only succeeds-then-fails, matching-etag
  succeeds, stale-etag fails) plus one new test: a push rejected after
  the pre-check passed (seed a race by pushing directly to the bare
  fixture between the pre-check and the write — feasible by pausing
  inside a test-only hook, or more simply by writing the test at the
  conformance-suite level in Task 6 using two real store instances) is
  `PreconditionFailed`, not an error.
- [ ] **Step 2: Run; expect FAIL.**
- [ ] **Step 3: Implement** `put_if_match`.
- [ ] **Step 4: Run; expect PASS.**
- [ ] **Step 5: Commit.**

## Task 6: Trait-level conformance suite + two-writer race test

**Files:**
- Create: `crates/ninox-core/src/brain_sync/conformance.rs`
- Modify: `crates/ninox-core/src/brain_sync/mod.rs` (`mod conformance;`
  under `#[cfg(test)]`)

**Interfaces:**
- Consumes: `RemoteStore`, `InMemoryRemoteStore` (`store.rs`);
  `S3RemoteStore` has no local-only test double reachable without real
  AWS creds, so the conformance suite runs against `InMemoryRemoteStore`
  and `GithubWikiRemoteStore` (backed by a fresh bare-repo fixture per
  test) — document in the file header why `S3RemoteStore` itself isn't
  instantiated here (no credential-free local S3 double exists in this
  codebase).
- Produces: a parametrized set of `async fn` conformance checks (e.g.
  `async fn get_missing_is_not_found(store: &dyn RemoteStore)`, `async fn
  put_then_get_round_trips(store: &dyn RemoteStore)`, `async fn
  put_if_match_enforces_cas(store: &dyn RemoteStore)`) each called once
  per backend from a `#[tokio::test]` per backend, so a failure names
  which backend broke the shared contract.

- [ ] **Step 1: Write the conformance functions** against
  `InMemoryRemoteStore` first (trivially passes — it's the reference
  implementation already tested in `store.rs`), then call the same
  functions from `GithubWikiRemoteStore`-backed tests.
- [ ] **Step 2: Add the two-concurrent-writers test**: two
  `GithubWikiRemoteStore`s, each with their own clone directory, both
  cloned from one shared bare-repo fixture; writer A `put_if_match`s the
  manifest first and succeeds; writer B, still holding the old etag,
  `put_if_match`s and must get `PreconditionFailed`; writer B then
  `put`s two new *entries* (not the manifest) concurrently-in-spirit with
  anything A does and both must be retrievable afterward (exercises the
  `put` fetch+rebase retry path from Task 4 for real, against actual
  divergent history, not just sequential calls).
- [ ] **Step 3: Run `cargo test -p ninox-core brain_sync::`; expect PASS.**
- [ ] **Step 4: Commit.**

## Task 7: Wire the dispatch into `BrainSync::for_brain`

**Files:**
- Modify: `crates/ninox-core/src/brain_sync/mod.rs:57-63`

**Interfaces:**
- Consumes: `SyncToml::is_github_wiki` (Task 1),
  `GithubWikiRemoteStore::from_config` (Task 2).
- Produces: `BrainSync::for_brain` unchanged in signature; internally
  branches: `if cfg.is_github_wiki() { github_wiki::GithubWikiRemoteStore::from_config(&cfg).await? as Arc<dyn RemoteStore> } else { s3::S3RemoteStore::from_config(&cfg).await? }`.

- [ ] **Step 1: Write failing test** in `mod.rs`'s existing test module:
  `for_brain_rejects_non_s3_non_wiki_remote` (rename/extend the existing
  `for_brain_rejects_non_s3_remote` test — an `ftp://` remote must still
  error) plus confirm (via a local bare-repo fixture, same pattern as
  Task 2's tests) that a `github.com/.../wiki` remote routes to
  `GithubWikiRemoteStore` and not `S3RemoteStore` — simplest check: it
  must *not* error the way an `s3://`-only `bucket_and_prefix()` parse
  would, and a `pull_if_stale()` against the empty bare fixture must
  succeed and report `checked: true`.
- [ ] **Step 2: Run; expect FAIL.**
- [ ] **Step 3: Implement the dispatch** in `for_brain`.
- [ ] **Step 4: Run `cargo test -p ninox-core brain_sync::`; expect PASS.**
- [ ] **Step 5: Run full workspace build/test/clippy**:
  `cargo build --workspace`, `cargo test --workspace`, `cargo clippy
  --workspace --all-targets` — all clean.
- [ ] **Step 6: Commit.**

## Task 8: PR

- [ ] Push `v1-github-wiki-brain-impl` to `origin`.
- [ ] Open PR base `v1`, title `feat(brain-sync): add GitHub wiki RemoteStore backend`,
  body per the repo's PR format (Why/Summary/Test plan), explaining the
  three open-question resolutions above. No Linear reference, no
  "Generated with Claude Code" line beyond the org-mandated trailer.
- [ ] Spawn a reviewer (per the task's Step 6 / global workflow) before
  leaving it open for human review. Do not self-merge.

## Self-review notes

- Spec coverage: `get`/`put`/`put_if_match` semantics (Tasks 3-5),
  persistent clone + `cache_ttl_secs` reuse (Task 2/3's debounce note),
  config dispatch (Tasks 1/7), conformance + concurrent-writer testing
  (Task 6), all three open questions (resolved above) — all covered.
  Non-goals (manifest format, diff, conflict policy) are explicitly not
  touched by any task.
- The "wiki init on first push" fallback (empty/unreachable clone) is
  covered in Task 2 and exercised again implicitly by Task 7's dispatch
  test, which starts from an empty bare fixture.
