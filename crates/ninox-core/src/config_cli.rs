//! Backend for the `ninox config get/set/show` command: dotted-path get/set
//! over `config.toml`'s `toml::Value` tree, generic across `AppConfig`'s
//! current and future fields, plus the harness-specific write validation
//! `orchestrator.harness`/`worker.harness` require. Auth-free by design —
//! `ninox_ui_shared::cli` gates calls into this module via
//! `orchestrator_auth::authorize_orchestrator` before reaching it.

use anyhow::{Context, Result};

use crate::config::AppConfig;

/// Placeholder printed instead of a secret field's real value — `show`/`get`
/// output is read directly by (and often echoed back into the transcript
/// of) the orchestrator session that ran the command, so a real secret must
/// never appear in it.
const REDACTED: &str = "<redacted>";

/// Dotted keys whose value must never be printed by `get`/`show` — checked
/// by exact key only (not by prefix), since this module has no concept of
/// "every field under here is sensitive".
pub fn is_secret_key(key: &str) -> bool {
    key == "github_token"
}

/// Dumps the full resolved config (defaults applied) as TOML, with secret
/// fields redacted (see `is_secret_key`).
pub fn show() -> Result<String> {
    let mut cfg = AppConfig::load()?;
    if cfg.github_token.is_some() {
        cfg.github_token = Some(REDACTED.to_string());
    }
    Ok(toml::to_string_pretty(&cfg)?)
}

/// Whether `key` is a real `AppConfig` field: re-serializes `cfg` (as
/// returned by a successful `set`) and checks the key is still reachable —
/// a key `AppConfig`'s `Deserialize` doesn't recognize never round-trips,
/// since unknown TOML fields are silently dropped.
pub fn is_known_key(key: &str, cfg: &AppConfig) -> bool {
    toml::Value::try_from(cfg).ok().is_some_and(|v| get_path(&v, key).is_some())
}

/// Prints the current value at `key`: the literal `config.toml` value when
/// present, else the resolved (defaults-applied) value, so `get` always
/// reflects the effective setting even for a key never written to disk.
/// Secret fields (`is_secret_key`) are redacted regardless of which source
/// held the value.
pub fn get(key: &str) -> Result<String> {
    anyhow::ensure!(!key.is_empty(), "config key must not be empty");
    let redact = |v: &toml::Value| if is_secret_key(key) { REDACTED.to_string() } else { format_value(v) };
    let raw = read_raw_doc()?;
    if let Some(v) = get_path(&raw, key) {
        return Ok(redact(v));
    }
    let resolved = toml::Value::try_from(&AppConfig::load()?).context("serializing resolved config")?;
    if let Some(v) = get_path(&resolved, key) {
        return Ok(redact(v));
    }
    // Distinguish a genuinely unknown key from a real field that just has
    // no value right now (an `Option` field, e.g. `worker.model`, is
    // omitted from both trees above when `None` — TOML has no null) by
    // checking whether the key's parent section exists at all.
    if let Some((parent, _leaf)) = key.rsplit_once('.') {
        if get_path(&resolved, parent).is_some_and(toml::Value::is_table) {
            anyhow::bail!(
                "'{key}' has no value in the current config (its parent section exists, \
                 but this key is either unset or not a recognized field)"
            );
        }
    }
    anyhow::bail!("unknown config key '{key}'")
}

/// Sets `key` to `raw_value`, validated by requiring the result to
/// deserialize into `AppConfig` (the only semantic check beyond that is the
/// harness-registry check for `orchestrator.harness`/`worker.harness`), then
/// written atomically (temp file + rename — never a torn/partial write).
pub fn set(key: &str, raw_value: &str) -> Result<AppConfig> {
    anyhow::ensure!(!key.is_empty(), "config key must not be empty");
    let mut doc = read_raw_doc()?;
    // Same guard as `AppConfig::save`: never rewrite a config that no longer
    // parses, even when this edit would happen to make it valid again.
    if AppConfig::config_path().exists() {
        let _: AppConfig = doc.clone().try_into().map_err(|e| anyhow::anyhow!(
            "refusing to edit {}: it no longer parses as valid config ({e}) — fix it by hand first",
            AppConfig::config_path().display(),
        ))?;
    }
    seed_harness_spec_if_needed(&mut doc, key)?;
    set_path(&mut doc, key, parse_scalar(raw_value))?;
    let candidate: AppConfig = doc
        .clone()
        .try_into()
        .with_context(|| format!("'{raw_value}' is not a valid value for '{key}'"))?;
    validate_harness_keys(key, &candidate)?;
    write_atomic(&doc)?;
    Ok(candidate)
}

/// A write to `harnesses.<name>.<leaf>` targets one field of that harness's
/// spec, but `HarnessRegistry::from_config` replaces a `[harnesses.<name>]`
/// table *wholesale* when it's present (see `AppConfig::toggle_harness`'s
/// own doc comment) rather than deep-merging it onto the builtin — so
/// writing just the one leaf, the first time `<name>` gets an override
/// table, would silently zero every other field (`worker_args`,
/// `interactive_args`, ...). Seed the table with the harness's current
/// *effective* spec first, so a partial write only ever changes the one
/// field it named.
fn seed_harness_spec_if_needed(doc: &mut toml::Value, key: &str) -> Result<()> {
    let Some(rest) = key.strip_prefix("harnesses.") else { return Ok(()) };
    let Some(name) = rest.split('.').next().filter(|n| !n.is_empty()) else { return Ok(()) };
    if get_path(doc, &format!("harnesses.{name}")).is_some() {
        return Ok(()); // already has an override table — nothing to seed
    }
    let current: AppConfig = doc.clone().try_into().context("reading current config to seed harness spec")?;
    let spec_value = toml::Value::try_from(current.registry().spec(name)).context("serializing harness spec")?;
    set_path(doc, &format!("harnesses.{name}"), spec_value)
}

/// Seeds a missing file from the full default config (mirroring
/// `AppConfig::load()`'s own fallback to `Self::default()`) rather than an
/// empty table, so a fresh `config.toml` starts fully populated instead of
/// holding only whatever key the first `set` happened to write.
fn read_raw_doc() -> Result<toml::Value> {
    let path = AppConfig::config_path();
    if !path.exists() {
        return toml::Value::try_from(AppConfig::default()).context("serializing default config");
    }
    std::fs::read_to_string(&path)?.parse::<toml::Value>().context("parsing config.toml")
}

/// Writes `doc` to `config.toml`, backing up any previous file to
/// `config.toml.bak` first and writing atomically (same-directory temp file
/// plus `fs::rename`, permissions preserved, `fsync`'d before the rename) —
/// see `crate::config::backup_and_write_atomic`, the single choke point
/// every `config.toml` persist path funnels through.
fn write_atomic(doc: &toml::Value) -> Result<()> {
    crate::config::backup_and_write_atomic(&AppConfig::config_path(), &toml::to_string(doc)?)
}

/// The only semantic (beyond "valid TOML for `AppConfig`") write validation
/// this command performs — matches the checks `run_spawn` already applies
/// at spawn time, moved here so a bad harness name is rejected at `set`
/// time instead of the next spawn. Also re-checks whichever role(s) are
/// currently pointed at the harness a `harnesses.<name>.*` write just
/// touched, so disabling (or de-worker-capable-ing) the harness actually in
/// use is caught here too, not just a direct `orchestrator.harness`/
/// `worker.harness` write — scoped to the written name specifically so an
/// unrelated `harnesses.*` write, or a deliberately unregistered/verbatim
/// harness name (`harness.rs`: "unknown harnesses run their name verbatim
/// as the binary"), is never affected.
fn validate_harness_keys(key: &str, cfg: &AppConfig) -> Result<()> {
    let touched = match key {
        "orchestrator.harness" => Some(cfg.orchestrator.harness.as_str()),
        "worker.harness" => Some(cfg.worker.harness.as_str()),
        _ => key.strip_prefix("harnesses.").and_then(|rest| rest.split('.').next()),
    };
    if let Some(hint) = touched.and_then(|n| cfg.registry().missing_binary_hint(n)) {
        eprintln!("warning: {hint}");
    }
    match key {
        "orchestrator.harness" => return validate_harness_name(&cfg.orchestrator.harness, cfg, false),
        "worker.harness" => return validate_harness_name(&cfg.worker.harness, cfg, true),
        _ => {}
    }
    if let Some(name) = key.strip_prefix("harnesses.").and_then(|rest| rest.split('.').next()) {
        if cfg.orchestrator.harness == name {
            validate_harness_name(name, cfg, false)?;
        }
        if cfg.worker.harness == name {
            validate_harness_name(name, cfg, true)?;
        }
    }
    Ok(())
}

fn validate_harness_name(name: &str, cfg: &AppConfig, require_worker_capable: bool) -> Result<()> {
    let registry = cfg.registry();
    let enabled = registry.enabled_names();
    anyhow::ensure!(
        enabled.iter().any(|n| n == name),
        "unknown or disabled harness '{name}' — enabled harnesses: {}",
        enabled.join(", ")
    );
    if require_worker_capable {
        anyhow::ensure!(
            registry.spec(name).worker_args.is_some(),
            "harness '{name}' has no verified worker mode (no worker_args) — pick a worker-capable harness"
        );
    }
    Ok(())
}

fn get_path<'a>(root: &'a toml::Value, path: &str) -> Option<&'a toml::Value> {
    let mut cur = root;
    for seg in path.split('.') {
        cur = cur.as_table()?.get(seg)?;
    }
    Some(cur)
}

/// Sets `path` to `value` in `root`, creating intermediate tables as
/// needed. Errors on an empty/blank path or segment, and on attempting to
/// overwrite an existing table with a non-table value (the generic
/// mechanism must not silently clobber a whole config section).
fn set_path(root: &mut toml::Value, path: &str, value: toml::Value) -> anyhow::Result<()> {
    let segs: Vec<&str> = path.split('.').collect();
    anyhow::ensure!(
        !path.is_empty() && segs.iter().all(|s| !s.is_empty()),
        "config key must not be empty: '{path}'"
    );
    let (parents, last) = segs.split_at(segs.len() - 1);
    let last = last[0];

    let mut cur = root;
    for seg in parents {
        let table = cur
            .as_table_mut()
            .ok_or_else(|| anyhow::anyhow!("cannot set '{path}': '{seg}' is not a table"))?;
        cur = table
            .entry(seg.to_string())
            .or_insert_with(|| toml::Value::Table(Default::default()));
    }

    let table = cur
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("cannot set '{path}': parent is not a table"))?;
    if let Some(existing) = table.get(last) {
        anyhow::ensure!(
            !(existing.is_table() && !value.is_table()),
            "cannot set '{path}': it is a config section (table), not a single value"
        );
    }
    table.insert(last.to_string(), value);
    Ok(())
}

/// Parses a CLI-supplied value string into the narrowest matching TOML
/// scalar: bool, then int, then float, else a bare string.
fn parse_scalar(raw: &str) -> toml::Value {
    if let Ok(b) = raw.parse::<bool>() {
        return toml::Value::Boolean(b);
    }
    if let Ok(i) = raw.parse::<i64>() {
        return toml::Value::Integer(i);
    }
    if let Ok(f) = raw.parse::<f64>() {
        return toml::Value::Float(f);
    }
    toml::Value::String(raw.to_string())
}

/// Renders a value for `ninox config get` output: strings print bare
/// (unquoted) so they're directly usable by a caller, everything else uses
/// `toml::Value`'s own inline-TOML `Display`.
fn format_value(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn set_then_get_round_trips_a_known_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        {
            let _iso = crate::test_isolation::isolate_with_config(&path);
            set("worker.model", "claude-opus-5-5").unwrap();
            assert_eq!(get("worker.model").unwrap(), "claude-opus-5-5");
        }
    }

    #[test]
    fn set_writes_the_session_limit() {
        let _iso = crate::test_isolation::isolate();
        set("limits.max_concurrent_local", "3").unwrap();
        assert_eq!(AppConfig::load().unwrap().limits.limit_for(None), Some(3));
    }

    #[test]
    fn set_refuses_to_rewrite_a_config_that_no_longer_parses() {
        let _iso = crate::test_isolation::isolate();
        // Valid TOML, invalid config: `port` must be a number.
        std::fs::write(AppConfig::config_path(), "port = \"eighty\"\n").unwrap();
        let err = set("port", "8081").unwrap_err().to_string();
        assert!(err.contains("refusing to edit"), "{err}");
        assert_eq!(std::fs::read_to_string(AppConfig::config_path()).unwrap(), "port = \"eighty\"\n");
    }

    #[test]
    fn two_sequential_sets_to_different_keys_both_land() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        {
            let _iso = crate::test_isolation::isolate_with_config(&path);
            set("orchestrator.model", "claude-opus-5-5").unwrap();
            set("worker.model", "claude-sonnet-5-5").unwrap();
            assert_eq!(get("orchestrator.model").unwrap(), "claude-opus-5-5");
            assert_eq!(get("worker.model").unwrap(), "claude-sonnet-5-5");
        }
    }

    #[test]
    fn set_rejects_unknown_orchestrator_harness_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        {
            let _iso = crate::test_isolation::isolate_with_config(&path);
            let err = set("orchestrator.harness", "not-a-real-harness").unwrap_err();
            assert!(err.to_string().contains("unknown or disabled harness"), "{err}");
        }
    }

    #[test]
    fn set_rejects_worker_harness_with_no_worker_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        {
            let _iso = crate::test_isolation::isolate_with_config(&path);
            // Register an interactive-only harness so the failure exercises
            // the worker-mode check rather than the enabled-harness check.
            set("harnesses.interactive-only.enabled", "true").unwrap();
            let err = set("worker.harness", "interactive-only").unwrap_err();
            assert!(err.to_string().contains("no verified worker mode"), "{err}");
        }
    }

    #[test]
    fn get_falls_back_to_resolved_default_when_key_absent_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        {
            let _iso = crate::test_isolation::isolate_with_config(&path);
            assert_eq!(get("worker_completion.idle_after_merge_nudge_mins").unwrap(), "30");
        }
    }

    #[test]
    fn set_creates_a_missing_config_file_and_its_parent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.toml");
        {
            let _iso = crate::test_isolation::isolate_with_config(&path);
            assert!(!path.exists());
            set("worker.model", "claude-opus-5-5").unwrap();
            assert!(path.exists());
            assert_eq!(get("worker.model").unwrap(), "claude-opus-5-5");
        }
    }

    #[test]
    fn show_includes_every_top_level_section() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        {
            let _iso = crate::test_isolation::isolate_with_config(&path);
            let dump = show().unwrap();
            assert!(dump.contains("[orchestrator]"));
            assert!(dump.contains("[worker]"));
        }
    }

    #[test]
    fn show_redacts_github_token_when_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        {
            let _iso = crate::test_isolation::isolate_with_config(&path);
            set("github_token", "ghp_supersecrettoken").unwrap();
            let dump = show().unwrap();
            assert!(!dump.contains("ghp_supersecrettoken"), "{dump}");
            assert!(dump.contains("<redacted>"), "{dump}");
        }
    }

    #[test]
    fn get_redacts_github_token_when_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        {
            let _iso = crate::test_isolation::isolate_with_config(&path);
            set("github_token", "ghp_supersecrettoken").unwrap();
            assert_eq!(get("github_token").unwrap(), "<redacted>");
        }
    }

    #[test]
    fn get_on_an_unset_optional_field_says_no_value_not_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        {
            let _iso = crate::test_isolation::isolate_with_config(&path);
            let err = get("worker.model").unwrap_err();
            assert!(err.to_string().contains("has no value"), "{err}");
            assert!(!err.to_string().contains("unknown config key"), "{err}");
        }
    }

    #[test]
    fn get_on_a_key_with_no_known_parent_section_says_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        {
            let _iso = crate::test_isolation::isolate_with_config(&path);
            let err = get("totally.bogus.path").unwrap_err();
            assert!(err.to_string().contains("unknown config key"), "{err}");
        }
    }

    #[test]
    fn is_known_key_true_for_a_real_field_false_for_a_bogus_one() {
        let cfg = AppConfig::default();
        assert!(is_known_key("worker.harness", &cfg));
        assert!(!is_known_key("foo.bar", &cfg));
    }

    #[test]
    fn set_seeds_the_full_harness_spec_before_a_partial_harnesses_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        {
            let _iso = crate::test_isolation::isolate_with_config(&path);
            let cfg = set("harnesses.codex.enabled", "true").unwrap();
            let spec = cfg.registry().spec("codex");
            assert!(spec.enabled);
            assert!(spec.worker_args.is_some(), "partial harnesses write must not wipe the builtin's worker_args");
        }
    }

    #[test]
    fn set_accepts_a_valid_worker_capable_harness_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        {
            let _iso = crate::test_isolation::isolate_with_config(&path);
            let cfg = set("worker.harness", "claude-code").unwrap();
            assert_eq!(cfg.worker.harness, "claude-code");
        }
    }

    #[test]
    fn set_rejects_disabling_the_harness_currently_configured_for_worker() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        {
            let _iso = crate::test_isolation::isolate_with_config(&path);
            set("harnesses.codex.enabled", "true").unwrap();
            set("worker.harness", "codex").unwrap();
            let err = set("harnesses.codex.enabled", "false").unwrap_err();
            assert!(err.to_string().contains("unknown or disabled harness"), "{err}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn set_preserves_the_config_files_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        {
            let _iso = crate::test_isolation::isolate_with_config(&path);
            set("worker.model", "claude-opus-5-5").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            set("worker.model", "claude-sonnet-5-5").unwrap();
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "set() must preserve the file's existing permissions");
        }
    }
}
