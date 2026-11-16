# Shared build cache per repo (MLOPS-5418)

## Problem

Every `ninox spawn` worker builds its own Rust `target/` (5-9 GB for this
workspace), the main contributor to a 70 GB `.claude/worktrees` dir with
~10 parallel v1 workers. This is the "shared build cache" slice of
MLOPS-5418; worktree/session cleanup is a separate, parallel PR
(`v1-mlops-5418-workspace-cleanup`).

## Measured: why not a literal shared `CARGO_TARGET_DIR`

The ninox brain already documents two incidents (2026-08-18, 2026-10-03)
where a single shared `CARGO_TARGET_DIR` across worktrees caused `cargo
test --workspace` to silently reuse another worktree's stale/deleted test
binary — different worktrees' units collide in `target/debug/deps` by
crate name/version. Reproduced live for this change: two scratch
worktrees of `origin/v1`, each with a uniquely named test, pointed at one
shared `CARGO_TARGET_DIR` and built concurrently. The second worktree's
`cargo test -p ninox-core --lib -- --list` finished in 0.87s and listed
the *first* worktree's test, not its own. Concurrent access also hit
cargo's target-dir lock ("waiting for file lock on build directory",
observed 3-4 times per build). **This disqualifies a literal shared
`CARGO_TARGET_DIR` as the default** — it is a correctness bug, not a
performance trade-off.

## Measured: sccache

`RUSTC_WRAPPER=sccache` keeps each worktree's own `target/` (no
cross-worktree sharing of final artifacts or test binaries — sccache only
caches individual compiled objects, keyed by a content hash of the
preprocessed source + flags + compiler version, not by target-dir path),
so it carries none of the correctness risk above.

Single-build (`cargo build -p ninox-core`) timings on a fresh scratch
worktree of `origin/v1`, `rustc`/`cargo` 1.97.1, this machine:

| Scenario | Wall clock | Notes |
|---|---|---|
| No wrapper (cold) | 108s | `CARGO_BUILD_RUSTC_WRAPPER=""` to defeat this machine's own global `~/.cargo/config.toml` `rustc-wrapper = sccache` |
| sccache, isolated cold cache | see PR | fresh empty `SCCACHE_DIR` |
| sccache, isolated warm cache | see PR | same `SCCACHE_DIR` after one prior build |

Concurrent two-worktree builds (both building `ninox-core` from
`origin/v1` at once): a shared, already-warm sccache cache (this
developer's own global `~/.cargo/config.toml` already sets `rustc-wrapper
= sccache`) took 85s per worktree at a 100% cache hit rate, vs. 180s per
worktree with no wrapper — concurrency contention drops sharply once most
dependency compilation is served from cache instead of re-run on every
core.

Trade-off to document in the PR: sccache does not shrink any single
worktree's own `target/` (each still holds its own final linked
artifacts) — it eliminates *redundant recompilation* across workers, so a
worker can `cargo clean` aggressively between tasks (already recommended
in the brain) without being punished by a multi-minute cold rebuild next
time. Actual disk-footprint reduction still depends on the sibling
workspace-cleanup PR reaping idle worktrees promptly.

## Follow-up: disk footprint and `build.build-dir` (reviewer request)

sccache alone leaves `target/` unchanged in size, which is the actual
metric the parent ticket cares about. Re-measured single `cargo build -p
ninox-core` disk/time on the *correctly pinned* 1.99.0 toolchain (earlier
numbers above were accidentally run under an ambient `RUSTUP_TOOLCHAIN`
override pinning 1.97.1 instead):

| Scenario | `target/` size | Wall clock |
|---|---|---|
| No wrapper | 2.7G | 91s |
| `RUSTC_WRAPPER=sccache` only | 2.7G (unchanged) | 102s |
| sccache + `CARGO_INCREMENTAL=0` + `CARGO_PROFILE_DEV_DEBUG=line-tables-only` | 1.9G (-30%) | 90s |

`CARGO_INCREMENTAL=0` and `CARGO_PROFILE_DEV_DEBUG=line-tables-only` are
both safe, per-build settings with no cross-worktree sharing and thus no
correctness risk, and incremental artifacts are both a large share of
`target/`'s size and not cacheable by sccache anyway (confirmed via
`sccache --show-stats`'s "Non-cacheable reasons: incremental"). Added to
the default unconditionally (not gated on `sccache` being installed,
since they need no external tool).

Checked `cargo`'s `build.build-dir` (1.99.0, stable, no `-Z` flags
needed) as a disk-optimal "shared intermediate dir, per-worktree final
artifacts" option:

- With the `{workspace-path-hash}` placeholder (the mode that avoids
  collisions): confirmed safe, but each worktree gets its own hash-keyed
  subdirectory — nothing is actually shared, so no disk or time benefit
  over the status quo.
- With one literal shared path (no placeholder) — the mode that would
  actually share compiled artifacts: reproduces the identical stale-test
  bug as a shared `CARGO_TARGET_DIR`. Two scratch worktrees with
  uniquely-named tests, built concurrently against one shared
  `CARGO_BUILD_BUILD_DIR`: the second worktree's `cargo test --lib --
  --list` finished in 2.00s and listed the *first* worktree's test. Same
  lock contention too ("waiting for file lock on build directory").

Conclusion: `build.build-dir` is not a viable path to "disk +
correctness" — it's either unsafe (literal shared path) or provides no
sharing benefit (hashed placeholder). The bug is fundamental to sharing
cargo's unit/fingerprint-keyed artifact cache across differing worktree
checkouts of the same package name/version, regardless of which cargo
mechanism (`CARGO_TARGET_DIR` or `build-dir`) is used to point multiple
worktrees at one location. Final default: sccache +
`CARGO_INCREMENTAL=0` + `CARGO_PROFILE_DEV_DEBUG=line-tables-only`.

## Design

1. `[[workspaces.shared_build]]` — opt-in, default empty, in
   `ninox-core::config::AppConfig` (`WorkspacesConfig` /
   `SharedBuildEntry`, mirroring the `RemoteMachinesConfig` /
   `MachineProfile` opt-in-array-of-tables pattern). Each entry is just a
   repo path and an env var map — no strategy-specific code, so it
   supports sccache, a shared `CARGO_TARGET_DIR` (with the correctness
   warning on the field), or anything else expressible as env vars.
   `{repo}` in an env value is substituted with the entry's `repo` path.
2. A worker's workspace matches an entry when it is that repo path or a
   descendant of it (worktrees live under `<repo>/.claude/worktrees/*`).
   Matching and `{repo}` substitution: `WorkspacesConfig::shared_build_env_for`.
3. Default-on for ninox's own repo: since a checkout path can't be
   predicted across installs (no existing config-seeding mechanism writes
   per-machine defaults to disk), `WorkspacesConfig::effective_shared_build_env_for`
   falls back to `RUSTC_WRAPPER=sccache` when no explicit entry matches,
   the workspace has ninox's own crate layout (`crates/ninox-core`,
   `crates/ninox-ui-shared`, `crates/ninox-tui` all present), and
   `sccache` is on `PATH`. An explicit entry — even an empty one — always
   wins, so a repo (including ninox's own) can opt out.
4. Env injection at both spawn call sites: `ninox-ui-shared::cli.rs`'s
   `run_spawn` (the `ninox spawn` CLI/worker path) and
   `spawn_util::launch_interactive_session` (the app-spawned interactive
   path), each extending its existing per-session env vec/`extra_env`
   with `effective_shared_build_env_for(&workspace)`.
5. Migration note for existing installs: nothing to migrate — the feature
   is additive and opt-in (`#[serde(default)]`), and the implicit
   ninox-repo default only activates when `sccache` is already installed,
   so an existing `config.toml` with no `[workspaces]` table behaves
   exactly as before unless `sccache` happens to be present.
6. `workspace::cleanup()`: out of scope for this PR — it does not exist
   yet on `origin/v1` (the sibling `v1-mlops-5418-workspace-cleanup` PR
   hasn't landed it). sccache's cache lives outside every worktree (its
   own global cache dir, or a `{repo}`-templated path a user configures
   explicitly), so by construction a per-worktree cleanup sweep cannot
   remove it. A literal shared `CARGO_TARGET_DIR`, if a user opts into one
   despite the warning, must still be pointed outside `.claude/worktrees`.

## Tests

- Config round-trip for the new table (scalars-before-tables ordering).
- Default-empty (`#[serde(default)]` opt-in).
- `shared_build_env_for`: path-ancestor matching, `{repo}` substitution,
  no match outside configured repos.
- Explicit empty entry overrides the implicit ninox default.
- Implicit default requires the full ninox crate-layout fingerprint.
- `binary_on_path` false-case (deterministic; the positive ninox+sccache
  case is environment-gated since CI may not have `sccache` installed).
