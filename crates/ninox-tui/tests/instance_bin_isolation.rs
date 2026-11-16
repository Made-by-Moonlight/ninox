//! A non-default instance (`NINOX_CONFIG`/`NINOX_DATA_DIR` outside the real
//! per-user dirs) must keep its wrappers, `ninox` shim and tmux config in its
//! own dir, hand that dir to its sessions first on PATH, and write nothing
//! under the default `<config_dir>/ninox`. Its own test binary, because it
//! rewrites `HOME` and `NINOX_*` for the whole process.

use std::path::{Path, PathBuf};
use std::process::Command;

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
fn a_scoped_instance_writes_only_its_own_bin_dir_and_puts_it_first_on_path() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path().canonicalize().unwrap();
    for (k, _) in std::env::vars() {
        if k.starts_with("NINOX_") {
            // SAFETY: the only test in this binary, before any thread exists.
            unsafe { std::env::remove_var(k) };
        }
    }
    let instance = home.join("instance");
    let config = instance.join("config").join("config.toml");
    let data = instance.join("data");
    let db = data.join("ninox.db");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(&config, "port = 1\n").unwrap();
    // SAFETY: as above.
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("NINOX_CONFIG", &config);
        std::env::set_var("NINOX_DATA_DIR", &data);
    }
    let real_config_root = if cfg!(target_os = "macos") {
        home.join("Library/Application Support/ninox")
    } else {
        home.join(".config/ninox")
    };

    // A real binary start: wrappers, self-shim and tmux config get installed.
    let out = Command::new(env!("CARGO_BIN_EXE_ninox-tui"))
        .args(["--db", db.to_str().unwrap(), "workspaces", "list", "--json"])
        .env("TMUX_TMPDIR", home.join("tmux"))
        .env("NINOX_TMUX_SOCKET", "ninox-instance-bin-isolation-test")
        .env("NINOX_PTYD_SOCKET", home.join("ptyd.sock"))
        .output()
        .unwrap();
    assert!(out.status.success(), "ninox-tui failed: {}", String::from_utf8_lossy(&out.stderr));
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("reset-bin"),
        "a scoped instance must not hint at another instance's shim: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let bin = ninox_core::AppConfig::ninox_bin_dir();
    assert_eq!(bin, instance.join("config").join("bin"));
    for name in ["gh", "git", "ninox"] {
        assert!(bin.join(name).is_file(), "{name} missing from the instance bin dir");
    }
    let shim = ninox_core::hooks::read_shim(&bin.join("ninox")).expect("a parsable shim");
    assert_eq!(shim.target, Path::new(env!("CARGO_BIN_EXE_ninox-tui")).canonicalize().unwrap());
    assert!(ninox_core::AppConfig::worker_type_exports_dir().starts_with(&instance));
    assert!(ninox_core::AppConfig::default().resolved_brain_path().starts_with(&instance));

    let launch = ninox_core::hooks::with_bin_dir_on_path("claude");
    assert!(launch.starts_with(&format!("export PATH='{}':", bin.display())), "{launch}");

    // The UI launch records its `--db`, and `ninox` inside its sessions
    // (same NINOX_CONFIG, no `--db`) opens that store.
    assert!(!instance.join("config").join("db-path").exists(), "a one-off `--db` CLI call must not repoint the instance");
    ninox_core::AppConfig::register_instance_db(&db).unwrap();
    assert_eq!(ninox_core::AppConfig::default_db_path(), db);

    // A different instance started from inside one of those sessions keeps
    // its own default store.
    let nested = home.join("nested").join("config.toml");
    // SAFETY: as above.
    unsafe {
        std::env::set_var("NINOX_CONFIG", &nested);
        std::env::remove_var("NINOX_DATA_DIR");
    }
    assert_eq!(ninox_core::AppConfig::default_db_path(), home.join("nested").join("ninox.db"));

    assert!(
        files_under(&real_config_root).is_empty(),
        "files written under the default config dir: {:?}",
        files_under(&real_config_root)
    );
}
