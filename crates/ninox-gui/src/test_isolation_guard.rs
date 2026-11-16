//! ninox-gui compiles ninox-core as an ordinary dependency (no `cfg(test)`),
//! so these prove the guard still reaches this crate's tests through the
//! `test-isolation` feature its dev-dependency enables.

use ninox_core::config::AppConfig;
use ninox_ui_shared::cli::test_fixtures::ENV_TEST_GUARD;
use std::path::PathBuf;

const OVERRIDES: [&str; 3] = ["NINOX_CONFIG", "NINOX_BRAIN", "NINOX_DATA_DIR"];

fn real_config() -> PathBuf {
    dirs::config_dir()
        .unwrap()
        .join("ninox")
        .join("config.toml")
}

fn mtime() -> Option<std::time::SystemTime> {
    std::fs::metadata(real_config())
        .and_then(|m| m.modified())
        .ok()
}

/// Runs `AppConfig::update` with each path override set to `value` (or
/// removed) and returns its panic message.
fn update_panic(value: Option<&PathBuf>) -> String {
    let _guard = ENV_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let prior: Vec<_> = OVERRIDES.iter().map(|k| (k, std::env::var_os(k))).collect();
    for k in OVERRIDES {
        match value {
            Some(v) => std::env::set_var(k, v),
            None => std::env::remove_var(k),
        }
    }
    let result = std::panic::catch_unwind(|| {
        AppConfig::update(|c| c.port = c.port.wrapping_add(1)).map(|_| ())
    });
    for (k, v) in prior {
        match v {
            Some(v) => std::env::set_var(k, v),
            None => std::env::remove_var(k),
        }
    }
    let err = result.expect_err("AppConfig::update without isolate() must panic");
    err.downcast_ref::<String>().cloned().unwrap_or_default()
}

#[test]
fn config_update_without_isolation_panics_and_writes_nothing() {
    let before = mtime();
    let msg = update_panic(None);
    assert!(msg.contains("ninox test isolation"), "{msg}");
    assert_eq!(mtime(), before);
}

#[test]
fn config_update_through_an_env_override_aimed_at_the_real_config_panics() {
    let before = mtime();
    let msg = update_panic(Some(&real_config()));
    assert!(msg.contains("NINOX_CONFIG"), "{msg}");
    assert_eq!(mtime(), before);
}

#[test]
fn config_update_inside_isolate_writes_the_tempdir() {
    let iso = ninox_core::test_isolation::isolate();
    AppConfig::update(|c| c.port = 4141).unwrap();
    assert!(iso.config_path().starts_with(iso.root()));
    assert_eq!(AppConfig::load().unwrap().port, 4141);
}
