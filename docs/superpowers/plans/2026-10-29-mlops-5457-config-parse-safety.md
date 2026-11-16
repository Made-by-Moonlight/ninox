# MLOPS-5457: config.toml parse-error safety — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** a `config.toml` that fails to parse never gets silently replaced by
defaults-on-disk, and the user always finds out about it (CLI/TUI/GUI).

**Architecture:** `AppConfig::load()` already returns `Err` on a present-but-
invalid file and `Ok(default)` when the file is absent — the bug is almost
entirely at the ~55 call sites across `ninox-core`/`ninox-ui-shared`/
`ninox-tui`/`ninox-gui` that collapse both cases with `.unwrap_or_default()`,
plus two real gaps: `port`/`font_size` lack `#[serde(default)]` (so an
old-but-valid config without them currently *looks* invalid), and `save()`
has no guard against clobbering a file it can't parse. Fix: (1) add the two
missing defaults, (2) give `load()` a structured `ConfigParseError`
(message + line/column) instead of an opaque `anyhow::Error`, (3) add one
`load_or_default()` convenience that logs on invalid-but-present and
mechanically replace every `.unwrap_or_default()` call site with it, (4)
harden `save()` (shared by `AppConfig::save` and `ninox config set`) to
refuse overwriting an unparseable on-disk file, back it up to
`config.toml.bak`, and write atomically, (5) surface the same
`ConfigParseError` message at the three real entry points: CLI stderr, a
persistent TUI footer line, a persistent GUI banner row.

**Tech Stack:** Rust, `toml` 0.8 (`toml::de::Error::span()`/`message()`),
`serde`, existing `anyhow::Result` conventions.

**Spec:** the ticket body (MLOPS-5457) — no separate design doc; this plan
is the spec of record. Related, read-only context: `docs/superpowers/specs/
2026-10-22-config-command-design.md` (the `ninox config` command this PR's
`config_cli.rs` already implements) and its own atomic-write code, which
Task 2 generalizes and reuses rather than duplicating.

## Global Constraints

- Never touch the real `~/Library/Application Support/ninox/config.toml` —
  tests and manual verification use scratch files/dirs only.
- `cargo fmt` is **not** run wholesale (tree isn't rustfmt-clean); format
  only the lines this PR touches if `cargo fmt --check` on them fails.
- `cargo clippy --workspace --all-targets -- -D warnings` and
  `cargo test --workspace` must pass before pushing.
- Branch off a fresh `origin/v1` (done — fast-forwarded with zero local
  divergence), PR targets `v1`, title `MLOPS-5457: fix(config): surface
  config.toml parse errors and never overwrite an unparseable config`.
- One task, one PR — no unrelated cleanup (e.g. the stale doc-comment in
  `config_cli.rs::read_raw_doc` that explains the port/font_size workaround
  gets corrected because Task 1 makes it literally false, not as drive-by
  polish).
- No manual screenshot capture is required for this PR (no existing
  `BRIEF-*` design artifact covers this feature); the "visual check" in
  BRIEF-common.md §7 only governs the fleet-overview-v1 UI project. Verify
  the GUI banner and TUI footer warning by launching an isolated instance
  per the ticket's capture rules (scratch `--db`/`NINOX_CONFIG`, `env -u`
  every `NINOX_*`), screenshot/read the window, but do not commit images.

## Review Focus

- A config file with a syntax error that also happens to be a prefix of a
  file that would otherwise be fine (e.g. `not = [valid`) must produce a
  readable line/column, not panic on an out-of-bounds byte offset when
  computing line/col from the TOML span.
- A `save()`/`update()` call while the on-disk file is invalid must leave
  the broken file completely untouched (not even a `.bak` write), since
  there is nothing valid to back up — only a *successful* overwrite backs
  up the *previous* (valid) content.
- The CLI's hot-path subcommands (`Statusline`, `Inbox`, `IdentityHook`,
  `Whoami`, `Open`/`Close`/`List`, `Connect`, `Capabilities`,
  `PromptAnswer`) must keep returning before the new stderr check — they
  already short-circuit before the tmux-config/wrapper setup block this
  check lives beside, so this is a placement constraint, not new code, but
  the task must not accidentally move the check earlier.
- `config_cli::set()` already refuses a schema-invalid pre-existing file
  (the whole doc round-trips through `try_into::<AppConfig>()`), so Task 2
  must not special-case "invalid" twice in a way that changes its existing
  error wording/behavior — only add the backup step.
- A `None` line/column (e.g. an io error, or a future toml error variant
  with no span) must still produce a sensible banner message, not
  `"on line None"` or a panic on `.unwrap()`.

---

## File structure

- `crates/ninox-core/src/config.rs` — `ConfigParseError`, line/col helper,
  `#[serde(default)]` for `port`/`font_size`, `load()`/`load_or_default()`,
  the new `write_file_atomic`/`backup_and_write_atomic` helpers, hardened
  `save()`. (Task 1, 2)
- `crates/ninox-core/src/config_cli.rs` — `write_atomic` becomes a thin
  call into the shared helper; comment fix for the now-fixed port/font_size
  workaround. (Task 2)
- ~55 call sites across `ninox-core`, `ninox-ui-shared`, `ninox-tui`,
  `ninox-gui` — mechanical `.unwrap_or_default()` → `load_or_default()`.
  (Task 3)
- `crates/ninox-ui-shared/src/cli.rs` — one stderr check in `dispatch()`.
  (Task 4)
- `crates/ninox-tui/src/tui/state.rs`, `view.rs`, `mod.rs` —
  `config_warning: Option<String>` field + footer rendering. (Task 5)
- `crates/ninox-gui/src/app.rs`, new
  `crates/ninox-gui/src/components/config_banner.rs`,
  `components/shell/mod.rs` — `config_warning` field + banner row. (Task 6)

## Task 1: Core fix — defaults, structured parse error, line/column

**Files:**
- Modify: `crates/ninox-core/src/config.rs`
- Test: same file, `mod tests`

**Interfaces:**
- Produces: `pub struct ConfigParseError { pub message: String, pub line:
  Option<usize>, pub column: Option<usize> }` implementing `Display` (as
  `"{message}"` when `line` is `None`, else `"line {line}: {message}"`) and
  `std::error::Error`; `impl ConfigParseError { pub fn banner_message(&self)
  -> String }` returning exactly `"config.toml has an error on line {N}: {M}.
  Running with defaults; settings won't be saved until it's fixed."` (or,
  when `line` is `None`, the same sentence without the `on line {N}` clause)
  — this is the single string Tasks 4/5/6 all render/print verbatim.
- Produces: `AppConfig::load() -> anyhow::Result<Self>` (signature
  unchanged — existing `?` call sites in `machine.rs` keep compiling,
  `ConfigParseError: std::error::Error` satisfies anyhow's blanket `From`).
- Produces: `AppConfig::load_detailed() -> Result<Self, ConfigParseError>`
  — absent file → `Ok(default)`; valid file → `Ok(parsed)`; invalid file →
  `Err(ConfigParseError)`. `load()` becomes `Ok(Self::load_detailed()?)`.
- Produces: `AppConfig::load_or_default() -> Self` — `Ok` branch as-is;
  `Err` branch does `tracing::warn!("{}", e.banner_message())` and returns
  `Self::default()`. Task 3 replaces every `.unwrap_or_default()` call site
  with this.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn missing_port_and_font_size_load_with_defaults() {
    let cfg: AppConfig = toml::from_str("").unwrap();
    assert_eq!(cfg.port, 8080);
    assert_eq!(cfg.font_size, 13.0);
}

#[test]
fn a_syntax_error_surfaces_with_a_line_number_instead_of_defaulting() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "port = 8080\nfont_size = 13.0\nbad = [not valid\n").unwrap();
    with_env_override("NINOX_CONFIG", &path, || {
        let err = AppConfig::load_detailed().unwrap_err();
        assert_eq!(err.line, Some(3));
        assert!(err.banner_message().contains("line 3"), "{}", err.banner_message());
        assert!(err.banner_message().contains("Running with defaults"));
    });
}

#[test]
fn absent_file_gives_defaults_via_load_detailed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    with_env_override("NINOX_CONFIG", &path, || {
        assert!(AppConfig::load_detailed().is_ok());
    });
}

#[test]
fn load_or_default_falls_back_on_an_invalid_file_without_panicking() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "not = [valid").unwrap();
    with_env_override("NINOX_CONFIG", &path, || {
        let cfg = AppConfig::load_or_default();
        assert_eq!(cfg.port, AppConfig::default().port);
    });
}

#[test]
fn banner_message_with_no_line_omits_the_line_clause() {
    let e = ConfigParseError { message: "io error".into(), line: None, column: None };
    let msg = e.banner_message();
    assert!(msg.contains("io error"));
    assert!(!msg.to_lowercase().contains("line"));
}
```

- [ ] **Step 2: Run tests, confirm they fail** (`ConfigParseError`/
  `load_detailed`/`load_or_default` don't exist yet; the defaults test fails
  deserializing `""`).

Run: `cargo test -p ninox-core --lib config::tests -- --nocapture`

- [ ] **Step 3: Add `#[serde(default = "default_port")]` /
  `#[serde(default = "default_font_size")]`** to the `port`/`font_size`
  fields (values `8080u16`/`13.0f32`, matching `AppConfig::default()`).

- [ ] **Step 4: Implement `ConfigParseError` and a private
  `fn line_col(src: &str, byte_offset: usize) -> (usize, usize)`** (1-indexed
  line/column; count `\n` bytes up to `byte_offset.min(src.len())` — clamp
  so a span pointing past EOF, e.g. an unterminated string, can't panic).
  Build it from `toml::de::Error` via `.span()` (`Some(range)` → resolve
  `range.start` through `line_col`; `None` → `line: None, column: None`) and
  `.message()`.

- [ ] **Step 5: Implement `load_detailed()`/rewire `load()`/add
  `load_or_default()`** per the Interfaces block. `load_detailed()`'s io
  error path (can't read an existing file) maps to `ConfigParseError {
  message: e.to_string(), line: None, column: None }`.

- [ ] **Step 6: Run tests, confirm they pass.**

Run: `cargo test -p ninox-core --lib config::tests`

- [ ] **Step 7: Fix the now-stale comment** in
  `crates/ninox-core/src/config_cli.rs` above `read_raw_doc()` — it
  currently says port/font_size "carry no `#[serde(default)]`... unlike
  every other `AppConfig` field", which Step 3 makes false. Reword to
  describe the seed-with-full-defaults behavior as a deliberate choice
  (a fresh file starts fully populated) rather than a workaround. No
  behavior change.

- [ ] **Step 8: Commit.**

```bash
git add crates/ninox-core/src/config.rs crates/ninox-core/src/config_cli.rs
git commit -m "fix(config): default port/font_size, surface parse errors with line/column"
```

## Task 2: Hard safety — atomic write, backup, refuse-on-invalid

**Files:**
- Modify: `crates/ninox-core/src/config.rs` (`save()`, new helpers)
- Modify: `crates/ninox-core/src/config_cli.rs` (`write_atomic` → thin wrapper)
- Test: both files' `mod tests`

**Interfaces:**
- Consumes: `ConfigParseError`/`load_detailed()` (Task 1) is not required
  here — the invalid-on-disk check re-parses directly with
  `toml::from_str::<AppConfig>`, since `save()` only needs a yes/no, not a
  line number.
- Produces: `pub(crate) fn write_file_atomic(path: &Path, text: &str) ->
  Result<()>` — same-directory temp file (`<name>.tmp.<pid>`), permissions
  copied from the existing file on unix, `fsync`'d, then `fs::rename`;
  removes the temp file on any failure. (Lifted from `config_cli::
  write_atomic`, generalized to take an arbitrary path/text instead of a
  `toml::Value` tied to `AppConfig::config_path()`.)
- Produces: `pub(crate) fn backup_and_write_atomic(path: &Path, text: &str)
  -> Result<()>` — if `path` exists, copies it to `<path>.bak` first (e.g.
  `config.toml` → `config.toml.bak`), creates the parent dir when it
  doesn't exist, then calls `write_file_atomic`.
- Produces: `AppConfig::save(&self) -> Result<()>` — if `Self::path()`
  exists, reads it and tries `toml::from_str::<AppConfig>`; on failure,
  returns `Err` (refuses) without touching the file or writing a backup;
  on success (or file absent), calls `backup_and_write_atomic`.
- `config_cli::write_atomic(doc: &toml::Value)` becomes `backup_and_write_
  atomic(&AppConfig::config_path(), &toml::to_string(doc)?)` — its own
  temp-file/permission/fsync code is deleted, not duplicated.

- [ ] **Step 1: Write the failing tests** (in `config.rs`):

```rust
#[test]
fn save_refuses_to_overwrite_an_already_invalid_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "not = [valid").unwrap();
    with_env_override("NINOX_CONFIG", &path, || {
        assert!(AppConfig::default().save().is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not = [valid");
        assert!(!path.with_file_name("config.toml.bak").exists());
    });
}

#[test]
fn save_backs_up_the_previous_valid_file_before_overwriting() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let original = AppConfig { port: 1111, ..AppConfig::default() };
    std::fs::write(&path, toml::to_string(&original).unwrap()).unwrap();
    with_env_override("NINOX_CONFIG", &path, || {
        let updated = AppConfig { port: 2222, ..AppConfig::default() };
        updated.save().unwrap();
        let bak = path.with_file_name("config.toml.bak");
        let backed_up: AppConfig = toml::from_str(&std::fs::read_to_string(&bak).unwrap()).unwrap();
        assert_eq!(backed_up.port, 1111);
        assert_eq!(AppConfig::load().unwrap().port, 2222);
    });
}

#[test]
fn save_writes_atomically_leaving_no_temp_file_behind() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    with_env_override("NINOX_CONFIG", &path, || {
        AppConfig::default().save().unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(dir.path()).unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    });
}
```

- [ ] **Step 2: Run, confirm failures** (current `save()` has no refuse/
  backup logic — the first two tests fail; the third likely already
  passes, confirming it before the refactor, not after).

Run: `cargo test -p ninox-core --lib config::tests::save_`

- [ ] **Step 3: Implement `write_file_atomic`/`backup_and_write_atomic`,
  rewrite `save()`** per the Interfaces block.

- [ ] **Step 4: Rewire `config_cli::write_atomic`** to call
  `crate::config::backup_and_write_atomic`, deleting its now-dead
  temp-file/permission/fsync body. Re-run `config_cli`'s existing
  `set_preserves_the_config_files_existing_permissions` test unchanged —
  it must still pass (permission-preservation moved, not dropped).

- [ ] **Step 5: Run tests, confirm they pass.**

Run: `cargo test -p ninox-core --lib config:: config_cli::`

- [ ] **Step 6: Commit.**

```bash
git add crates/ninox-core/src/config.rs crates/ninox-core/src/config_cli.rs
git commit -m "fix(config): refuse to overwrite an unparseable config.toml; back up + write atomically"
```

## Task 3: Audit every `.unwrap_or_default()` call site

**Files:**
- Modify: every file `grep -rlF "AppConfig::load().unwrap_or_default()" crates/`
  currently lists (as of this writing: `ninox-core/src/{events.rs,
  worker_type.rs, worker_type_cli.rs, workspace/orphan.rs, runtime/mod.rs,
  lifecycle/poller.rs}`, `ninox-ui-shared/src/{runtime_cli.rs, fleet.rs,
  spawn_util.rs, connect.rs, cli.rs}`, `ninox-tui/src/{main.rs, tui/fleet.rs,
  tui/report.rs, tui/workspaces_panel.rs, tui/mod.rs, tui/worker_types.rs,
  tui/settings.rs, tui/spawn.rs, tui/state.rs}`,
  `ninox-server/src/routes/sessions.rs`, `ninox-gui/src/app.rs`).

**Interfaces:**
- Consumes: `AppConfig::load_or_default()` (Task 1).
- No new interfaces produced — pure call-site substitution.

- [ ] **Step 1: Confirm the two call sites that must NOT change** —
  `ninox-ui-shared/src/machine.rs` (`AppConfig::load()?`, already
  propagates) and `AppConfig::update()`'s internal `Self::load()?` — these
  already satisfy "invalid must surface"; grep them by name to make sure
  the mechanical replace in Step 2 doesn't touch them.

Run: `grep -rn "AppConfig::load()" crates/ninox-ui-shared/src/machine.rs crates/ninox-core/src/config.rs`

- [ ] **Step 2: Mechanically replace** every remaining
  `AppConfig::load().unwrap_or_default()` with `AppConfig::load_or_default()`
  across the files listed above (same-crate paths use `AppConfig::...`,
  cross-crate paths keep their existing `ninox_core::config::AppConfig::...`
  qualification — this is a value-level rename, not an import change).

- [ ] **Step 3: Verify zero remaining matches outside tests/this list.**

Run: `grep -rnF "AppConfig::load().unwrap_or_default()" --include="*.rs" crates/ | grep -v /target/`
Expected: no output (every production call site now reads
`AppConfig::load_or_default()`; test-module call sites that intentionally
exercise `load()` directly, e.g. `config.rs`'s own tests, are untouched
since they never wrote `.unwrap_or_default()` in the first place).

- [ ] **Step 4: Build the whole workspace** to catch any call site this
  grep-based pass missed (a type mismatch would fail to compile, not pass
  silently).

Run: `cargo build --workspace`

- [ ] **Step 5: Commit.**

```bash
git add -A
git commit -m "refactor(config): route every AppConfig::load() fallback through load_or_default()"
```

## Task 4: CLI stderr

**Files:**
- Modify: `crates/ninox-ui-shared/src/cli.rs`

**Interfaces:**
- Consumes: `AppConfig::load_detailed()` (Task 1), `ConfigParseError::
  banner_message()` (Task 1).

- [ ] **Step 1: Insert the check** immediately before the existing `if
  ninox_core::runtime::configured_backend() == ... Tmux { ... }` block in
  `dispatch()` (the point already documented as "reached only once every
  hot-path subcommand above has already returned"):

```rust
if let Err(e) = AppConfig::load_detailed() {
    eprintln!("{}", e.banner_message());
}
```

  (Matches the existing `eprintln!("failed to write tmux config: {e}")`
  style one block below it — no new error-handling convention introduced.)
  This re-parses the file a second time per invocation (bootstrap() etc.
  call `load_or_default()` separately) — acceptable: it's one extra TOML
  parse on an already-non-hot-path, so no new test is needed beyond Task 1's
  coverage of `banner_message()`'s text; this is a two-line integration
  with no new logic of its own.

- [ ] **Step 2: Build.**

Run: `cargo build -p ninox-ui-shared`

- [ ] **Step 3: Commit.**

```bash
git add crates/ninox-ui-shared/src/cli.rs
git commit -m "feat(cli): print the config.toml parse error to stderr on startup"
```

## Task 5: TUI persistent footer warning

**Files:**
- Modify: `crates/ninox-tui/src/tui/state.rs` (new field)
- Modify: `crates/ninox-tui/src/tui/mod.rs` (`run()`, populate the field)
- Modify: `crates/ninox-tui/src/tui/view.rs` (`draw_footer`)
- Test: `crates/ninox-tui/src/tui/view.rs`'s existing test module (uses a
  `ratatui::backend::TestBackend` buffer, per the existing
  `header_shows_clickable_tabs_and_a_tmux_session_renders_through_its_viewer`
  test's pattern)

**Interfaces:**
- Consumes: `AppConfig::load_detailed()`, `ConfigParseError::
  banner_message()` (Task 1).
- Produces: `TuiState::config_warning: Option<String>` — set once at
  startup, never auto-expires (unlike `notice`/`deferred_notice`, which
  are transient). `draw_footer` shows it in the left slot, overriding both
  the transient `notice` and the key-hints row, for as long as it's `Some`.

- [ ] **Step 1: Write the failing test** (append to `view.rs`'s test mod):

```rust
#[test]
fn footer_shows_the_config_warning_over_hints_and_notices() {
    let mut st = TuiState { config_warning: Some("config.toml has an error on line 3: bad".into()), ..Default::default() };
    st.notice = Some(Notice { text: "unrelated".into(), level: Level::Info, expires_ms: i64::MAX });
    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    let lay = super::super::layout::Layout::compute(ratatui::layout::Rect::new(0, 0, 80, 24), &st);
    terminal.draw(|f| draw(f, &st, &lay)).unwrap();
    let content = terminal.backend().buffer().content.iter().map(|c| c.symbol()).collect::<String>();
    assert!(content.contains("config.toml has an error"), "{content}");
}
```

  (Adjust `Layout::compute`'s actual signature/call to match whatever
  `view.rs`'s existing header test uses verbatim — copy that test's setup
  rather than re-deriving it.)

- [ ] **Step 2: Run, confirm it fails** (field doesn't exist yet).

- [ ] **Step 3: Add `pub config_warning: Option<String>` to `TuiState`**,
  defaulted to `None` in its `Default`/constructor.

- [ ] **Step 4: In `draw_footer`, check `st.config_warning` first** —
  when `Some(text)`, render it where `st.notice` currently renders (same
  span style, using `Level::Error`'s color), skipping both the `notice`
  and hints branches entirely.

- [ ] **Step 5: In `tui::run()`**, replace the `let config =
  AppConfig::load().unwrap_or_default();` line with:

```rust
let (config, parse_error) = match AppConfig::load_detailed() {
    Ok(cfg) => (cfg, None),
    Err(e) => (AppConfig::default(), Some(e.banner_message())),
};
st.config_warning = parse_error;
```

- [ ] **Step 6: Run tests, confirm they pass.**

Run: `cargo test -p ninox-tui --lib tui::view::tests`

- [ ] **Step 7: Commit.**

```bash
git add crates/ninox-tui/src/tui/state.rs crates/ninox-tui/src/tui/mod.rs crates/ninox-tui/src/tui/view.rs
git commit -m "feat(tui): show a persistent footer warning when config.toml fails to parse"
```

## Task 6: GUI banner

**Files:**
- Create: `crates/ninox-gui/src/components/config_banner.rs`
- Modify: `crates/ninox-gui/src/components/mod.rs` (register the module)
- Modify: `crates/ninox-gui/src/app.rs` (`App` struct, `App::new()`)
- Modify: `crates/ninox-gui/src/components/shell/mod.rs` (`shell()`)
- Test: `crates/ninox-gui/src/app.rs`'s existing test module (construct via
  `AppConfig::load_detailed()` directly against a temp `NINOX_CONFIG`,
  rather than driving the full `App::new()` — the state tests elsewhere in
  that module already load config this way for setup, not through the full
  Iced app)

**Interfaces:**
- Consumes: `AppConfig::load_detailed()`, `ConfigParseError::
  banner_message()` (Task 1); `tokens::AMBER`/`tokens::PANEL`/`tokens::TEXT`
  (`components/shell/tokens.rs`, already present).
- Produces: `pub fn config_banner(text: &str) -> Element<'_, Message>` — a
  full-width row, amber-accented, no dismiss control (stays until the file
  is fixed and the app restarts/reloads — no live file-watching in scope).
- Produces: `App::config_warning: Option<String>` field, set in
  `App::new()`.

- [ ] **Step 1: Write the failing test** (in `app.rs`'s test mod):

```rust
#[test]
fn config_warning_is_set_when_the_file_is_invalid() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "not = [valid").unwrap();
    ninox_core::config::with_env_override("NINOX_CONFIG", &path, || {
        let warning = ninox_core::config::AppConfig::load_detailed()
            .err()
            .map(|e| e.banner_message());
        assert!(warning.is_some());
        assert!(warning.unwrap().contains("Running with defaults"));
    });
}
```

  (This pins the value `App::new()` must compute; it doesn't drive the
  full Iced constructor, which needs a live `Engine`/`BrainIndex` this test
  has no reason to stand up.)

- [ ] **Step 2: Run, confirm it passes already** (it only exercises Task 1
  code) — this step is really a readiness check for Step 3's wiring, not a
  red/green cycle of its own; proceed once confirmed.

- [ ] **Step 3: Add `pub config_warning: Option<String>` to `App`**; in
  `App::new()`, replace the `let mut config = AppConfig::load().
  unwrap_or_default();` line the same way as Task 5's Step 5 (match on
  `load_detailed()`, store the banner message, default the config on
  `Err`).

- [ ] **Step 4: Create `components/config_banner.rs`**:

```rust
//! Persistent warning row shown when config.toml fails to parse —
//! stays until the file is fixed and the app restarts. No dismiss
//! control: `App::config_warning` only clears on a fresh, valid load.

use iced::widget::{container, text};
use iced::{Background, Element, Length};

use crate::app::Message;
use crate::components::shell::tokens;

pub fn config_banner(text_content: &str) -> Element<'_, Message> {
    container(text(text_content).color(tokens::BG))
        .width(Length::Fill)
        .padding(8)
        .style(|_| container::Style {
            background: Some(Background::Color(tokens::AMBER)),
            ..Default::default()
        })
        .into()
}
```

  (Signature/styling calls adjusted to match whichever `iced` widget API
  `notification_panel.rs` or another existing component already uses in
  this codebase — copy its container/text/style idiom rather than
  re-deriving one.)

- [ ] **Step 5: Register it** in `components/mod.rs` (`pub mod
  config_banner;`, alongside the other `pub mod` lines).

- [ ] **Step 6: Wire it into `shell()`** — in `components/shell/mod.rs`,
  when `app.config_warning` is `Some(text)`, push
  `config_banner::config_banner(text)` into `outer` immediately after
  `nav::nav(app)` (so it shows above every tab, matching Task 5's
  "persistent across every view" behavior).

- [ ] **Step 7: Build.**

Run: `cargo build -p ninox-gui`

- [ ] **Step 8: Commit.**

```bash
git add crates/ninox-gui/src/app.rs crates/ninox-gui/src/components/mod.rs \
        crates/ninox-gui/src/components/config_banner.rs crates/ninox-gui/src/components/shell/mod.rs
git commit -m "feat(gui): show a persistent banner when config.toml fails to parse"
```

## Task 7: Workspace validation, rebase, PR

- [ ] **Step 1: Full workspace check.**

Run: `cargo fmt --check -- $(git diff --name-only origin/v1... -- '*.rs') ; cargo clippy --workspace --all-targets -- -D warnings ; cargo test --workspace`

Fix any touched-line formatting or clippy findings (do not run a wholesale
`cargo fmt`).

- [ ] **Step 2: Manual verification per the ticket's capture rules** — launch
  an isolated instance (`env -u` every `NINOX_*`, scratch `--db`, scratch
  `NINOX_CONFIG` pointed at a deliberately invalid `config.toml`,
  `orchestrator_root`/`brain.path` redirected to scratch) for both GUI and
  TUI, confirm the banner/footer warning text and that the stderr line
  appears for a plain CLI command (e.g. `ninox capabilities`). Window-only
  capture via `CGWindowID` if screenshotting; no region/full-screen
  captures, no synthetic mouse input, no committed images (BRIEF-common.md
  §7's mechanics apply generally even though this PR isn't part of that
  design project).

- [ ] **Step 3: Rebase onto `origin/v1`** (other v1 workers land in
  parallel).

Run: `git fetch origin v1 && git rebase origin/v1`

- [ ] **Step 4: Re-run Step 1's checks post-rebase.**

- [ ] **Step 5: Open the PR** — title exactly `MLOPS-5457: fix(config):
  surface config.toml parse errors and never overwrite an unparseable
  config`, base `v1`, body `## Why` / `## Summary` / `## Test plan`, no
  "Generated with Claude Code" line, no test plan restated from this doc
  beyond a short bullet list. Run `now-playing` first and append its
  output as its own line at the bottom of the body if it returns a track.
