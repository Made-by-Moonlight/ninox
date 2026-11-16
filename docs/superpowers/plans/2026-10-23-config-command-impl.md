# `ninox config` Command Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the `set-agent-config` skill's Edit-tool workaround with a real `ninox config get/set/show` command, gated orchestrator-only, with atomic writes and harness-name validation.

**Architecture:** A new `ninox-core::config_cli` module holds pure, auth-free dotted-path get/set/show logic over `toml::Value` (generic across `AppConfig`'s current and future fields), plus a harness-specific validation hook for `orchestrator.harness`/`worker.harness`. `ninox-ui-shared::cli` adds a `Command::Config` variant that short-circuits in `dispatch()` (like `Plan`), gated by the existing `orchestrator_auth::authorize_orchestrator` pattern, and calls into `config_cli` for the actual work.

**Tech Stack:** Rust, `toml` 0.8 (`toml::Value`, `Table::entry`), `clap` (`Subcommand`), existing `ninox_core::{orchestrator_auth, runtime, store::Store, harness::HarnessRegistry, config::AppConfig}`.

**Spec:** `docs/superpowers/specs/2026-10-22-config-command-design.md`

## Open questions — resolved

**1. Full set of keys worth exposing beyond `orchestrator.*`/`worker.*`:**
`show` dumps the entire resolved `AppConfig` (every field, via its existing `Serialize` impl — same shape `AppConfig::save()` already persists). For `get`/`set`, the mechanism is a fully generic dotted-path walk over the config's TOML tree rather than a hand-maintained allowlist: any current or future `AppConfig` field is reachable by its dotted path (e.g. `pr_watch.enabled`, `auto_reap.enabled`, `runtime.backend`, `theme`, `tui.prefix`, `harnesses.codex.enabled`) with no per-field code to add when a new top-level section ships — satisfying the spec's "doesn't need re-designing" goal. The only hand-written validation is the harness-specific check required for `orchestrator.harness`/`worker.harness`.
`get` reads the literal `config.toml` first, falling back to the fully-resolved (defaults-applied) config when the key isn't physically present in the file (e.g. a field hidden by `skip_serializing_if` while at its default) — so `get` always reflects the effective value, matching the "print current value" wording in the spec. `set` reads/writes the literal file's `toml::Value` tree directly (creating intermediate tables as needed) and validates the result by requiring it to deserialize into `AppConfig`; this accepts a typo'd unknown key (e.g. `ninox config set foo.bar 1`) as inert dead weight in the file rather than silently discarding it, which is an accepted tradeoff given the spec's explicit non-goal of a general validation framework — `AppConfig::load()` ignores unknown keys exactly as it does today for any other stray TOML.

**2. Stronger concurrency guarantee (file lock):** No. The atomic temp-file-plus-rename (Task 1) is the only guarantee — it matches `AppConfig::update()`'s existing read-modify-write, which has never used a cross-process lock either. A real race between two concurrent `ninox config set` calls is last-writer-wins at the semantic level (one process's change can be silently superseded), but never torn/corrupted at the file level, which is the actual integrity risk the spec names. Config edits are rare, human/agent-initiated, one-at-a-time in practice; a `flock`-based lock would add real complexity (cross-platform semantics, stale-lock recovery after a crash) to close a race that's already benign enough that `AppConfig::update()` accepted it.

## Global Constraints

- `set` must never leave `config.toml` torn/partially-written: write to a temp file in the same directory, then `fs::rename` over the original (spec "Concurrency safety").
- `orchestrator.harness`/`worker.harness` writes must be rejected with a clear error when the name isn't an enabled harness in `HarnessRegistry` (and, for `worker.harness` specifically, when it has no `worker_args`) — never written and left to fail at spawn time (spec "Validation").
- `ninox config` is gated orchestrator-only via `orchestrator_auth::authorize_orchestrator`, the same pattern `ninox plan` (`run_plan_cli` in `cli.rs:979`) already uses — no claim-only `NINOX_ORCHESTRATOR_ID` trust (spec "Authorization").
- No other dotted key gets semantic validation beyond "deserializes into `AppConfig`" (spec non-goal).
- `crates/ninox-core/skills/orchestrator/set-agent-config.md` must be updated to describe the real command, not the Edit-tool workaround (capabilities.rs:223 serves this file verbatim via `include_str!`).

## Review Focus

- **Empty/whitespace dotted key** (`ninox config get ""` or `set ".." x`): must error clearly, not panic on an empty `split('.')` segment or silently no-op.
- **Setting a non-leaf path to a scalar** (`ninox config set orchestrator abc`, where `orchestrator` is a table, not a leaf): must error (can't overwrite a table with a scalar) rather than corrupt the schema in a way that breaks every subsequent `AppConfig::load()`.
- **Harness name validation for `worker.harness` set to a harness that exists but isn't worker-capable** (e.g. `freebuff`, which ships with no `worker_args`): must be rejected with the same "no verified worker mode" reasoning `run_spawn` already bails with, not accepted and left to fail at the next `ninox spawn`.
- **Missing config.toml file entirely** (fresh install, no file on disk yet): `get`/`show` must fall back to `AppConfig::default()` rather than erroring "file not found", and `set` must create the file (and its parent directory) rather than requiring it to pre-exist.
- **Non-orchestrator caller** (no `NINOX_ORCHESTRATOR_ID`/`NINOX_CALLER_TYPE`, or a worker's): must be rejected by `authorize_orchestrator` before any read or write, exactly as `ninox plan`/`ninox spawn --orchestrator-id` already enforce — covered by reusing `authorize_orchestrator`'s existing test suite rather than re-testing the primitive, per Task 2.

---

## Task 1: `config_cli` core module — dotted-path get/set/show + harness validation

**Files:**
- Create: `crates/ninox-core/src/config_cli.rs`
- Modify: `crates/ninox-core/src/lib.rs` (add `pub mod config_cli;`)

**Interfaces:**
- Produces (consumed by Task 2):
  - `pub fn show() -> anyhow::Result<String>`
  - `pub fn get(key: &str) -> anyhow::Result<String>`
  - `pub fn set(key: &str, raw_value: &str) -> anyhow::Result<AppConfig>` (returns the resulting config so the caller can log/print if desired; the write has already happened atomically by the time this returns `Ok`)
  - All three take no store/auth params — this module is pure config-file mechanics; Task 2 performs `orchestrator_auth` gating before calling any of them.

- [ ] **Step 1: Write failing tests for the dotted-path primitives**

```rust
#[test]
fn get_path_returns_nested_value() {
    let doc: toml::Value = toml::from_str("[orchestrator]\nharness = \"codex\"\n").unwrap();
    assert_eq!(get_path(&doc, "orchestrator.harness").unwrap().as_str(), Some("codex"));
}

#[test]
fn get_path_none_for_missing_key() {
    let doc: toml::Value = toml::from_str("port = 8080\n").unwrap();
    assert!(get_path(&doc, "nonexistent.key").is_none());
}

#[test]
fn set_path_creates_intermediate_tables() {
    let mut doc = toml::Value::Table(Default::default());
    set_path(&mut doc, "a.b.c", toml::Value::Boolean(true)).unwrap();
    assert_eq!(get_path(&doc, "a.b.c").unwrap().as_bool(), Some(true));
}

#[test]
fn set_path_errors_when_overwriting_a_table_with_a_leaf() {
    let mut doc: toml::Value = toml::from_str("[orchestrator]\nharness = \"codex\"\n").unwrap();
    assert!(set_path(&mut doc, "orchestrator", toml::Value::String("x".into())).is_err());
}

#[test]
fn set_path_errors_on_empty_or_blank_key() {
    let mut doc = toml::Value::Table(Default::default());
    assert!(set_path(&mut doc, "", toml::Value::Boolean(true)).is_err());
    assert!(set_path(&mut doc, "a..b", toml::Value::Boolean(true)).is_err());
}

#[test]
fn parse_scalar_recognizes_bool_int_float_string() {
    assert_eq!(parse_scalar("true"), toml::Value::Boolean(true));
    assert_eq!(parse_scalar("42"), toml::Value::Integer(42));
    assert_eq!(parse_scalar("1.5"), toml::Value::Float(1.5));
    assert_eq!(parse_scalar("codex"), toml::Value::String("codex".into()));
}

#[test]
fn format_value_prints_strings_unquoted() {
    assert_eq!(format_value(&toml::Value::String("codex".into())), "codex");
    assert_eq!(format_value(&toml::Value::Boolean(true)), "true");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox-core config_cli:: -- --list` then `cargo test -p ninox-core config_cli::tests::get_path_returns_nested_value`
Expected: FAIL to compile (`get_path`/`set_path`/`parse_scalar`/`format_value` not defined)

- [ ] **Step 3: Implement the primitives in `config_cli.rs`**

```rust
fn get_path<'a>(root: &'a toml::Value, path: &str) -> Option<&'a toml::Value>
fn set_path(root: &mut toml::Value, path: &str, value: toml::Value) -> anyhow::Result<()>
fn parse_scalar(raw: &str) -> toml::Value
fn format_value(v: &toml::Value) -> String
```

- `get_path`: split `path` on `.`, descend via `Value::as_table()` + `Table::get`, `None` on any miss.
- `set_path`: split into `(parents, last)`; walk `parents` using `Table::entry(seg).or_insert_with(|| Value::Table(Default::default()))`, erroring with `anyhow::bail!` if an existing entry isn't a table (can't descend into a leaf); error the same way if the final segment's parent isn't a table (covers `set orchestrator <scalar>` — `orchestrator` is itself the last segment and its *current* value is a table, so insert would silently replace it: explicitly check `cur.as_table().and_then(|t| t.get(last))` isn't `Some(Value::Table(_))` before inserting a non-table, or simpler: if the key already exists as a `Table` and `value` is not a `Table`, error). Empty `path` or any empty segment (`"".split('.')` yields `[""]`; `"a..b"` yields an empty middle segment) is a `bail!` before any traversal.
- `parse_scalar`: try `bool`, then `i64`, then `f64`, else `String` — in that order (matches the test above; `"true"`/`"false"` must not fall through to string).
- `format_value`: `Value::String(s) => s.clone()`, everything else `other.to_string()` (the `toml` crate's `Display` impl for `Value`, enabled by its default `display` feature, renders scalars/arrays/tables as inline TOML).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ninox-core config_cli::`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-core/src/config_cli.rs
git commit -m "feat(config): add dotted-path TOML get/set primitives"
```

---

- [ ] **Step 6: Write failing tests for `get`/`set`/`show` against a real file (env-overridden `NINOX_CONFIG`, mirroring `config.rs`'s `with_env_override` pattern)**

```rust
#[test]
fn set_then_get_round_trips_a_known_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    crate::config::with_env_override("NINOX_CONFIG", &path, || {
        set("worker.harness", "codex").unwrap();
        assert_eq!(get("worker.harness").unwrap(), "codex");
    });
}

#[test]
fn two_sequential_sets_to_different_keys_both_land() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    crate::config::with_env_override("NINOX_CONFIG", &path, || {
        set("orchestrator.harness", "codex").unwrap();
        set("worker.harness", "codex").unwrap();
        assert_eq!(get("orchestrator.harness").unwrap(), "codex");
        assert_eq!(get("worker.harness").unwrap(), "codex");
    });
}

#[test]
fn set_rejects_unknown_orchestrator_harness_name() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    crate::config::with_env_override("NINOX_CONFIG", &path, || {
        let err = set("orchestrator.harness", "not-a-real-harness").unwrap_err();
        assert!(err.to_string().contains("unknown or disabled harness"), "{err}");
    });
}

#[test]
fn set_rejects_worker_harness_with_no_worker_mode() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    crate::config::with_env_override("NINOX_CONFIG", &path, || {
        // freebuff ships enabled:false in the builtin registry AND has no
        // worker_args — enable it first so the failure is specifically the
        // worker-mode check, not the enabled-harness check.
        set("harnesses.freebuff.enabled", "true").unwrap();
        let err = set("worker.harness", "freebuff").unwrap_err();
        assert!(err.to_string().contains("no verified worker mode"), "{err}");
    });
}

#[test]
fn get_falls_back_to_resolved_default_when_key_absent_from_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    crate::config::with_env_override("NINOX_CONFIG", &path, || {
        assert_eq!(get("auto_reap.enabled").unwrap(), "true"); // default, file doesn't exist yet
    });
}

#[test]
fn set_creates_a_missing_config_file_and_its_parent_dir() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested").join("config.toml"); // parent dir doesn't exist yet
    crate::config::with_env_override("NINOX_CONFIG", &path, || {
        assert!(!path.exists());
        set("worker.harness", "codex").unwrap();
        assert!(path.exists());
        assert_eq!(get("worker.harness").unwrap(), "codex");
    });
}

#[test]
fn show_includes_every_top_level_section() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    crate::config::with_env_override("NINOX_CONFIG", &path, || {
        let dump = show().unwrap();
        assert!(dump.contains("[orchestrator]"));
        assert!(dump.contains("[worker]"));
    });
}
```

- [ ] **Step 7: Run tests to verify they fail**

Run: `cargo test -p ninox-core config_cli::`
Expected: FAIL to compile (`get`/`set`/`show` not defined)

- [ ] **Step 8: Implement `get`/`set`/`show` plus the harness-validation and atomic-write helpers**

```rust
pub fn show() -> anyhow::Result<String>
pub fn get(key: &str) -> anyhow::Result<String>
pub fn set(key: &str, raw_value: &str) -> anyhow::Result<AppConfig>

fn read_raw_doc() -> anyhow::Result<toml::Value>   // empty Table if config.toml doesn't exist
fn write_atomic(doc: &toml::Value) -> anyhow::Result<()>  // temp file (same dir, pid-suffixed name) + fs::rename
fn validate_harness_keys(key: &str, cfg: &AppConfig) -> anyhow::Result<()>  // matches on "orchestrator.harness" / "worker.harness" only
```

- `show`: `toml::to_string_pretty(&AppConfig::load()?)`.
- `get`: `read_raw_doc()`, try `get_path` on it; on miss, `toml::Value::try_from(&AppConfig::load()?)?` and try `get_path` on that; on a second miss, `anyhow::bail!("unknown config key '{key}'")`.
- `set`: `read_raw_doc()` → `set_path(&mut doc, key, parse_scalar(raw_value))?` → `let candidate: AppConfig = doc.clone().try_into().with_context(...)?` (surfaces `toml`'s own deserialize error, e.g. a type mismatch) → `validate_harness_keys(key, &candidate)?` → `write_atomic(&doc)?` → `Ok(candidate)`.
- `validate_harness_keys`: for `"orchestrator.harness"`/`"worker.harness"`, check `candidate.registry().enabled_names().contains(name)`, and for `worker.harness` additionally `candidate.registry().spec(name).worker_args.is_some()` — same checks `run_spawn` (`cli.rs:1100`) already performs, reused here as pre-write validation instead of a spawn-time failure. Error messages list the enabled harness names (`enabled_names().join(", ")`) so the agent can self-correct without a second round trip.
- `write_atomic`: serialize with plain `toml::to_string` (matches `AppConfig::save()`'s existing format, not `_pretty`, to avoid an unrelated reformatting diff on every `set`), write to `AppConfig::config_path().with_extension(format!("toml.tmp.{}", std::process::id()))`, then `std::fs::rename` over the real path. Create the parent directory first (mirrors `AppConfig::save()`).

- [ ] **Step 9: Run tests to verify they pass**

Run: `cargo test -p ninox-core config_cli::`
Expected: PASS

- [ ] **Step 10: Commit**

```bash
git add crates/ninox-core/src/config_cli.rs crates/ninox-core/src/lib.rs
git commit -m "feat(config): add ninox-core config_cli get/set/show with harness validation"
```

---

## Task 2: `ninox config get/set/show` CLI command

**Files:**
- Modify: `crates/ninox-ui-shared/src/cli.rs`

**Interfaces:**
- Consumes: `ninox_core::config_cli::{get, set, show}` (Task 1); `ninox_core::orchestrator_auth::authorize_orchestrator` (existing, `orchestrator_auth.rs:20`); `ninox_core::runtime::current_private_pane_identity` (existing); `ninox_core::store::Store::open` (existing).
- Produces: `Command::Config { action: ConfigAction }` variant; nothing downstream depends on this (leaf command).

- [ ] **Step 1: Add the `Command::Config` variant and `ConfigAction` enum**

Add to `enum Command` (near `Plan`, `cli.rs:181-186`):

```rust
/// Read or write `config.toml` by dotted key, or dump the full resolved
/// config (orchestrator-only — see `ninox_core::config_cli`).
Config {
    #[command(subcommand)]
    action: ConfigAction,
},
```

Add alongside `enum PlanAction` (`cli.rs:368-381`):

```rust
#[derive(Subcommand)]
enum ConfigAction {
    /// Print a config value by dotted key, e.g. `orchestrator.harness`.
    Get {
        key: String,
    },
    /// Set a config value by dotted key, e.g. `worker.harness codex`.
    /// `orchestrator.harness`/`worker.harness` are validated against the
    /// harness registry before writing.
    Set {
        key: String,
        value: String,
    },
    /// Print the full resolved config as TOML.
    Show,
}
```

- [ ] **Step 2: Add the dispatch short-circuit, mirroring `Plan`'s gating (`cli.rs:674-677`)**

```rust
if let Some(Command::Config { action }) = command {
    let db_path = args.db.unwrap_or_else(default_db_path);
    return run_config_cli(action, db_path).await.map(ControlFlow::Continue);
}
```

Place it directly after the existing `Command::Plan` branch.

- [ ] **Step 3: Implement `run_config_cli`, reusing `run_plan_cli`'s auth sequence (`cli.rs:979-1012`) but returning a plain `anyhow::Result<()>` (matching `run_spawn`'s error style, `cli.rs:1118-1129`) instead of `run_plan_cli`'s bespoke JSON-envelope/exit-code protocol — `config` is a direct human/agent CLI surface, not a machine-parsed one like `plan`**

```rust
async fn run_config_cli(action: ConfigAction, db_path: PathBuf) -> anyhow::Result<()>
```

Body:
1. `std::fs::create_dir_all` the db path's parent (same as `run_plan_cli`).
2. `Store::open(db_path)?`.
3. `let runtime = ninox_core::runtime::current_private_pane_identity().await?;` (propagate the error directly — matches `run_plan_cli`'s `authorization_failed` treatment of this call's `Err` case).
4. `let _orchestrator_id = ninox_core::orchestrator_auth::authorize_orchestrator(&store, env NINOX_ORCHESTRATOR_ID, env NINOX_CALLER_TYPE, runtime.as_ref()).context("not authorized to run `ninox config`")?;` — same three env/arg reads `run_plan_cli` uses.
5. Match `action`:
   - `Get { key } => println!("{}", ninox_core::config_cli::get(&key)?);`
   - `Set { key, value } => { ninox_core::config_cli::set(&key, &value)?; println!("{key} = {value}"); }` (print the literal assignment, not the parsed/round-tripped value — keeps worker-facing output predictable even though `set` internally re-parses it)
   - `Show => println!("{}", ninox_core::config_cli::show()?);`
6. `Ok(())`

- [ ] **Step 4: Add the `unreachable!` arm for `Command::Config` in the big `match command` (alongside the `Command::Plan` arm at `cli.rs:941-943`)**

```rust
Some(Command::Config { .. }) => {
    unreachable!("Config short-circuits and returns earlier in dispatch()")
}
```

- [ ] **Step 5: Build and manually verify gating (no automated cli.rs test exists for any orchestrator-gated command today — `run_spawn`'s and `run_plan_cli`'s claim checks rely entirely on `orchestrator_auth`'s own test suite, which this command reuses unchanged)**

Run: `cargo build -p ninox-tui`
Then, outside any ninox-managed session (no `NINOX_ORCHESTRATOR_ID`/`NINOX_CALLER_TYPE` set):
Run: `./target/debug/nx config get orchestrator.harness`
Expected: a clear authorization error (`NINOX_ORCHESTRATOR_ID is not set`, surfaced via the `.context(...)` wrapper), not a panic and not a silent success.

- [ ] **Step 6: Commit**

```bash
git add crates/ninox-ui-shared/src/cli.rs
git commit -m "feat(cli): add ninox config get/set/show command"
```

---

## Task 3: Replace the `set-agent-config` skill doc

**Files:**
- Modify: `crates/ninox-core/skills/orchestrator/set-agent-config.md`

**Interfaces:**
- Consumes: nothing new — `capabilities.rs:223` already pulls this file in via `include_str!`; no Rust change needed here, and the registry's own tests (`every_markdown_has_frontmatter_with_a_parseable_description`, `rendering_every_orchestrator_md_leaves_no_placeholders`) already guard this file's shape.

- [ ] **Step 1: Rewrite the body to document the real command**

Keep the existing frontmatter (`name: set-agent-config`, same `description:`). Replace the "Use the Edit tool..." instruction with the actual commands, e.g.:

```markdown
## Change the harness or model

    ninox config set orchestrator.harness codex
    ninox config set orchestrator.model <model-name>
    ninox config set worker.harness codex
    ninox config set worker.model <model-name>

`harness` is validated against the known agent harnesses — an unknown or
disabled name is rejected with an error instead of being written. Omit
`model` (or set it to the harness's default) by not setting it at all.
Changes take effect on the next spawn.

## Inspect current config

    ninox config get orchestrator.harness
    ninox config show          # full resolved config
```

Drop the `{{CONFIG_PATH}}`/raw-TOML-format block entirely — the command surface replaces the need to know the file's path or shape. If the `{{NINOX_BIN}}`/`{{CONFIG_PATH}}` placeholders are no longer referenced anywhere in the rewritten body, no further placeholder-substitution change is needed (the substitution function and its tests are generic over whichever placeholders a given skill happens to use).

- [ ] **Step 2: Run the capability-registry invariant tests**

Run: `cargo test -p ninox-core capabilities::`
Expected: PASS (frontmatter/description checks still hold; no placeholder assertions broken since this file no longer uses any)

- [ ] **Step 3: Commit**

```bash
git add crates/ninox-core/skills/orchestrator/set-agent-config.md
git commit -m "docs(skills): point set-agent-config at the real ninox config command"
```

---

## Task 4: Full workspace verification

**Files:** none (verification only)

- [ ] **Step 1: Run the full test suite**

Run: `cargo test --workspace`
Expected: PASS, including every new `config_cli::` test and the untouched `config::tests::*`/`capabilities::tests::*` suites (confirms `AppConfig::load()`/`AppConfig::update()` and the capability registry are unaffected — spec Step 5 regression check).

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --workspace --all-targets`
Expected: clean (no new warnings)

- [ ] **Step 3: Full build**

Run: `cargo build --workspace`
Expected: PASS

- [ ] **Step 4: Commit (if any fixups were needed)**

```bash
git add -A
git commit -m "chore: clippy/test fixups for ninox config command"
```
