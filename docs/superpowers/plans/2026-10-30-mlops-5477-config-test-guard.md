# MLOPS-5477: forbid tests from resolving the real config path

**Goal:** no test, in any crate, can read or write the developer's real
`config.toml`, brain, sessions dir or store.

## Mechanism

- `ninox_core::test_isolation` owns the default per-user roots:
  `config_root()` (`<config_dir>/ninox`) and `data_root()`
  (`<data_local_dir>/ninox`). Every default in `AppConfig`
  (`config_path`, `sessions_dir`, `ninox_bin_dir`,
  `worker_type_exports_dir`, `prompt_agent_workspace_dir`,
  `resolved_brain_path`) and ui-shared's `default_db_path` resolve through them.
- Enforcement is on when: `cfg(test)` (ninox-core's own unit tests), or the
  `test-isolation` feature is on *and* the process is a test harness binary
  (`target/*/deps/`), or `NINOX_TEST_ISOLATION=1`. Under enforcement:
  - resolving a default root panics unless `isolate()` is active on the thread;
  - an env override (`NINOX_CONFIG`/`NINOX_BRAIN`/`NINOX_DATA_DIR`/
    `NINOX_WORKER_TYPE_EXPORTS`) pointing inside the real roots panics too:
    ninox sessions export those at the real files, so `cargo test` run from
    inside a session would otherwise inherit them.
- Override: `isolate()` / `isolate_with_config(path)` return a `!Send` guard
  that sets a thread-local root (a tempdir) and wins over env overrides.
  `propagate(f)` carries it into `spawn_blocking`/thread work.
- Cross-crate reach: `cfg(test)` doesn't apply to dependents' tests, so every
  crate depending on ninox-core enables `ninox-core/test-isolation` in its
  `[dev-dependencies]`; a ninox-core test parses the workspace manifests and
  fails if any dependent crate is missing it.

## Steps

1. Regression tests first (`test_isolation::tests`): `update` without the
   helper panics and leaves the real file's mtime unchanged; env pointing at
   the real file panics; every default path is guarded; `isolate` redirects
   all of them; per-thread scoping; `propagate`; the manifest invariant.
2. Route the path helpers through `test_isolation`; collapse ui-shared's
   duplicated `NINOX_DATA_DIR`/`NINOX_BRAIN` reads onto `sessions_dir()` /
   `resolved_brain_path()` (same precedence) so nothing bypasses the guard.
3. Add the feature + dev-dependency edges.
4. Migrate tests: every test the guard trips gets `isolate()`; every
   `with_env_override("NINOX_CONFIG", p, ..)` / manual `set_var("NINOX_CONFIG")`
   becomes `isolate_with_config(p)` (tests of the env override itself stay).
   Rendering paths that only need catalogue *names* use the new
   `AppConfig::catalogue_names()` instead of resolving the brain path.
5. ninox-gui test proving the guard fires there.
6. Full suite parallel and `--test-threads=1` (no env-race masking), with
   the session's real-pointing `NINOX_*` vars set; stat the real
   `config.toml` before/after.
