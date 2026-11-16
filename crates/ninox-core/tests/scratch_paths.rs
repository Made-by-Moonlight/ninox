//! A scratch instance (`NINOX_CONFIG` outside the real config dir) must
//! write nothing under the real data dir. Its own test binary, because it
//! rewrites `HOME` and `NINOX_*` for the whole process.

use std::path::{Path, PathBuf};

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries
        .flatten()
        .flat_map(|e| {
            let p = e.path();
            if p.is_dir() { files_under(&p) } else { vec![p] }
        })
        .collect()
}

#[test]
fn a_scratch_config_keeps_the_db_ptyd_and_logs_out_of_the_real_data_dir() {
    let home = tempfile::tempdir().unwrap();
    for (k, _) in std::env::vars() {
        if k.starts_with("NINOX_") {
            // SAFETY: the only test in this binary, before any thread exists.
            unsafe { std::env::remove_var(k) };
        }
    }
    let scratch = home.path().join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    // SAFETY: as above.
    unsafe {
        std::env::set_var("HOME", home.path());
        std::env::set_var("NINOX_CONFIG", scratch.join("config.toml"));
    }
    let real_data = dirs::data_local_dir().unwrap().join("ninox");
    assert!(real_data.starts_with(home.path()), "the fake HOME must own the data dir: {}", real_data.display());

    let db = ninox_core::AppConfig::default_db_path();
    let socket = ninox_core::runtime::ptyd_socket_path();
    let checkpoints = ninox_core::runtime::ptyd_checkpoint_dir();
    for (what, path) in [("db", &db), ("ptyd socket", &socket), ("ptyd checkpoints", &checkpoints)] {
        assert!(!path.starts_with(&real_data), "{what} resolved into the real data dir: {}", path.display());
    }
    assert_eq!(db, scratch.join("ninox.db"));

    // daemon.log goes next to the db (`daemon::spawn_headless`).
    ninox_core::store::Store::open(&db).unwrap();
    std::fs::write(db.parent().unwrap().join("daemon.log"), "x").unwrap();
    assert!(files_under(&real_data).is_empty(), "files written under the real data dir: {:?}", files_under(&real_data));

    // The real config, as every ninox session exports it, is not scratch.
    // (Resolving the real data dir itself panics under workspace test
    // isolation, so the check stops at the classification.)
    let real_config = dirs::config_dir().unwrap().join("ninox").join("config.toml");
    // SAFETY: as above.
    unsafe { std::env::set_var("NINOX_CONFIG", &real_config) };
    assert_eq!(ninox_core::test_isolation::scratch_config_root(), None);
}
