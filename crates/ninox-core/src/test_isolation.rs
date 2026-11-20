//! Keeps tests away from the developer's real per-user ninox state
//! (`config.toml`, brain, sessions, store).
//!
//! In a test process — this crate's own unit tests, any test binary of a
//! crate that enables the `test-isolation` feature (every workspace crate
//! does, via its `ninox-core` dev-dependency), or any process run with
//! `NINOX_TEST_ISOLATION=1` — resolving a default per-user path panics
//! unless [`isolate`] is active on the current thread. An env override
//! (`NINOX_CONFIG`, `NINOX_BRAIN`, `NINOX_DATA_DIR`, …) that points back
//! inside the real directories panics too: ninox sessions export those
//! pointing at the real files, so a test run from inside one would
//! otherwise inherit them.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const ENV: &str = "NINOX_TEST_ISOLATION";

#[derive(Clone)]
struct Override {
    /// Tempdir the data root (store) lives under.
    root: PathBuf,
    /// What `AppConfig::config_path()` returns; its parent is the config
    /// root, mirroring how `NINOX_CONFIG`'s parent is.
    config_file: PathBuf,
}

thread_local! {
    static OVERRIDE: RefCell<Option<Override>> = const { RefCell::new(None) };
}

fn enforced() -> bool {
    static ENFORCED: OnceLock<bool> = OnceLock::new();
    *ENFORCED.get_or_init(|| {
        // The feature is unified into every binary of a `cargo test` build,
        // including the real `ninox`/`nx` bins, so it only counts inside a
        // test harness binary.
        cfg!(test)
            || (cfg!(feature = "test-isolation") && crate::tmux::is_test_binary())
            || std::env::var(ENV).is_ok_and(|v| v == "1")
    })
}

fn current() -> Option<Override> {
    OVERRIDE.with(|o| o.borrow().clone())
}

/// The config file [`isolate`]/[`isolate_with_config`] pinned on this thread.
pub(crate) fn config_file() -> Option<PathBuf> {
    current().map(|o| o.config_file)
}

/// Whether [`isolate`] is active on this thread. Callers skip their env
/// overrides when it is, so a parallel test mutating `NINOX_CONFIG` can't
/// leak into an isolated one.
pub(crate) fn active() -> bool {
    OVERRIDE.with(|o| o.borrow().is_some())
}

fn real_config_root() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ninox")
}

fn real_data_root() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ninox")
}

fn deny(what: &str, path: &Path) -> ! {
    panic!(
        "ninox test isolation: {what} resolved to the real per-user path {} — \
         hold `ninox_core::test_isolation::isolate()` for the duration of the test",
        path.display()
    )
}

/// Best-effort canonical form, so a symlinked or `..`-laden override can't
/// slip past the prefix check; falls back to the parent for paths that
/// don't exist yet.
fn normalize(p: &Path) -> PathBuf {
    if let Ok(c) = p.canonicalize() {
        return c;
    }
    match (
        p.parent().and_then(|d| d.canonicalize().ok()),
        p.file_name(),
    ) {
        (Some(parent), Some(name)) => parent.join(name),
        _ => p.to_path_buf(),
    }
}

fn is_real(p: &Path) -> bool {
    let p = normalize(p);
    [real_config_root(), real_data_root()]
        .iter()
        .any(|root| p.starts_with(root) || p.starts_with(normalize(root)))
}

/// The value of env override `var`, if set and non-empty, unless
/// [`isolate`] is active (it wins). Under enforcement an override pointing
/// into the real per-user directories panics.
pub(crate) fn env_path(var: &str) -> Option<PathBuf> {
    if active() {
        return None;
    }
    let p = PathBuf::from(std::env::var(var).ok().filter(|p| !p.is_empty())?);
    if enforced() && is_real(&p) {
        deny(var, &p);
    }
    Some(p)
}

/// `<config_dir>/ninox` — the root `config.toml`, the brain, sessions and
/// the bin dir default under.
pub fn config_root() -> PathBuf {
    if let Some(o) = current() {
        return o
            .config_file
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or(o.root);
    }
    let real = real_config_root();
    if enforced() {
        deny("the default ninox config directory", &real);
    }
    real
}

/// The directory of a `NINOX_CONFIG` that points somewhere other than the
/// real per-user config — a scratch instance, whose db, ptyd socket and logs
/// default there instead of into the real data dir. `None` under
/// [`isolate`] (its own redirect wins), with no override, or when the
/// override is the real config (as every ninox session exports it).
pub fn scratch_config_root() -> Option<PathBuf> {
    if active() {
        return None;
    }
    let file = PathBuf::from(std::env::var_os("NINOX_CONFIG").filter(|v| !v.is_empty())?);
    let dir = file.parent().filter(|d| !d.as_os_str().is_empty())?.to_path_buf();
    (!is_real(&dir)).then_some(dir)
}

/// The directory a non-default instance keeps its own bin dir and
/// worker-type exports under: [`scratch_config_root`], else a
/// `NINOX_DATA_DIR` outside the real per-user dirs. `None` for the default
/// instance — every ninox session exports both overrides pointing at the
/// real dirs, so their mere presence doesn't make an instance non-default.
///
/// Always absolute: it lands on spawned sessions' PATH, and they start in
/// their own workspace.
pub fn instance_root() -> Option<PathBuf> {
    let root = match scratch_config_root() {
        Some(root) => root,
        None if active() => return None,
        None => {
            let dir = PathBuf::from(std::env::var_os("NINOX_DATA_DIR").filter(|v| !v.is_empty())?);
            if is_real(&dir) {
                return None;
            }
            dir
        }
    };
    Some(std::path::absolute(&root).unwrap_or(root))
}

/// `<data_local_dir>/ninox` — the store database defaults under it.
pub fn data_root() -> PathBuf {
    if let Some(o) = current() {
        return o.root.join("data");
    }
    let real = real_data_root();
    if enforced() {
        deny("the default ninox data directory", &real);
    }
    real
}

/// `~/ninox/orchestrators` — where orchestrator roots default. Under
/// [`isolate`] it is `<tempdir>/orchestrators`; in a test process without
/// one it panics, since seeding the real root rewrites every live
/// orchestrator's hooks and statusLine.
pub fn default_orchestrator_root() -> PathBuf {
    if let Some(o) = current() {
        return o.root.join("orchestrators");
    }
    let real = dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join("ninox").join("orchestrators");
    if enforced() {
        deny("the default orchestrator root", &real);
    }
    real
}

/// Wraps `f` so it runs under the calling thread's [`isolate`] override
/// wherever it ends up running — for work handed to `spawn_blocking` or a
/// fresh thread, which would otherwise see no override. A plain call when
/// no override is active, so non-test builds are unaffected.
pub fn propagate<T>(f: impl FnOnce() -> T) -> impl FnOnce() -> T {
    let carried = current();
    move || {
        let Some(o) = carried else { return f() };
        struct Restore(Option<Override>);
        impl Drop for Restore {
            fn drop(&mut self) {
                OVERRIDE.with(|r| *r.borrow_mut() = self.0.take());
            }
        }
        let _restore = Restore(OVERRIDE.with(|r| r.replace(Some(o))));
        f()
    }
}

/// Redirects every default per-user ninox path on the current thread into
/// a fresh tempdir until dropped. Thread-local, so parallel tests never
/// race; `!Send`, since dropping it on another thread would clear the
/// wrong thread's override.
#[cfg(any(test, feature = "test-isolation"))]
pub fn isolate() -> Isolation {
    let dir = tempfile::tempdir().expect("create test-isolation tempdir");
    let config_file = dir.path().join("config").join("config.toml");
    enter(dir, config_file)
}

/// [`isolate`], but `AppConfig::config_path()` returns `config_file` — for
/// tests that prepare their config at a path of their own. Sessions, brain
/// and the other config-root paths default under its parent directory.
#[cfg(any(test, feature = "test-isolation"))]
pub fn isolate_with_config(config_file: impl AsRef<Path>) -> Isolation {
    let dir = tempfile::tempdir().expect("create test-isolation tempdir");
    enter(dir, config_file.as_ref().to_path_buf())
}

#[cfg(any(test, feature = "test-isolation"))]
fn enter(dir: tempfile::TempDir, config_file: PathBuf) -> Isolation {
    if let Some(parent) = config_file.parent() {
        std::fs::create_dir_all(parent).expect("create isolated config dir");
    }
    std::fs::create_dir_all(dir.path().join("data")).expect("create isolated data dir");
    let o = Override {
        root: dir.path().to_path_buf(),
        config_file,
    };
    let prior = OVERRIDE.with(|r| r.replace(Some(o)));
    Isolation {
        dir,
        prior,
        _not_send: std::marker::PhantomData,
    }
}

#[cfg(any(test, feature = "test-isolation"))]
pub struct Isolation {
    dir: tempfile::TempDir,
    prior: Option<Override>,
    _not_send: std::marker::PhantomData<*const ()>,
}

#[cfg(any(test, feature = "test-isolation"))]
impl Isolation {
    /// Where `AppConfig::load`/`save`/`update` read and write while this is held.
    pub fn config_path(&self) -> PathBuf {
        crate::config::AppConfig::config_path()
    }

    pub fn root(&self) -> &Path {
        self.dir.path()
    }
}

#[cfg(any(test, feature = "test-isolation"))]
impl Drop for Isolation {
    fn drop(&mut self) {
        OVERRIDE.with(|r| *r.borrow_mut() = self.prior.take());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppConfig, ENV_TEST_GUARD};
    use std::time::SystemTime;

    const OVERRIDES: [&str; 4] = [
        "NINOX_CONFIG",
        "NINOX_BRAIN",
        "NINOX_DATA_DIR",
        "NINOX_WORKER_TYPE_EXPORTS",
    ];

    /// Runs `f` with every path env override set to `value` (or removed),
    /// restoring them afterwards.
    fn with_overrides<T>(value: Option<&Path>, f: impl FnOnce() -> T) -> T {
        let _guard = ENV_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let prior: Vec<_> = OVERRIDES.iter().map(|k| (k, std::env::var_os(k))).collect();
        for k in OVERRIDES {
            match value {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        for (k, v) in prior {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        result.unwrap_or_else(|e| std::panic::resume_unwind(e))
    }

    fn real_config_file() -> PathBuf {
        real_config_root().join("config.toml")
    }

    fn mtime(p: &Path) -> Option<SystemTime> {
        std::fs::metadata(p).and_then(|m| m.modified()).ok()
    }

    fn panic_message(f: impl FnOnce()) -> String {
        let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
            .expect_err("expected a panic");
        err.downcast_ref::<String>().cloned().unwrap_or_default()
    }

    #[test]
    fn update_without_isolation_panics_and_writes_nothing() {
        let real = real_config_file();
        let before = mtime(&real);
        let msg = with_overrides(None, || {
            panic_message(|| {
                let _ = AppConfig::update(|c| c.port = c.port.wrapping_add(1));
            })
        });
        assert!(msg.contains("ninox test isolation"), "{msg}");
        assert!(msg.contains("isolate()"), "{msg}");
        assert_eq!(mtime(&real), before);
    }

    #[test]
    fn env_override_pointing_at_the_real_config_panics() {
        let real = real_config_file();
        let before = mtime(&real);
        let msg = with_overrides(Some(&real), || {
            panic_message(|| {
                let _ = AppConfig::update(|c| c.port = c.port.wrapping_add(1));
            })
        });
        assert!(msg.contains("NINOX_CONFIG"), "{msg}");
        assert_eq!(mtime(&real), before);
    }

    #[test]
    fn every_default_path_is_guarded() {
        with_overrides(None, || {
            panic_message(|| drop(AppConfig::config_path()));
            panic_message(|| drop(AppConfig::sessions_dir()));
            panic_message(|| drop(AppConfig::ninox_bin_dir()));
            panic_message(|| drop(AppConfig::worker_type_exports_dir()));
            panic_message(|| drop(AppConfig::prompt_agent_workspace_dir()));
            panic_message(|| drop(AppConfig::default().resolved_brain_path()));
            panic_message(|| drop(data_root()));
        });
    }

    #[test]
    fn isolate_redirects_every_default_path_into_its_tempdir() {
        let iso = isolate();
        let root = iso.root().to_path_buf();
        assert!(iso.config_path().starts_with(&root));
        assert!(AppConfig::sessions_dir().starts_with(&root));
        assert!(AppConfig::ninox_bin_dir().starts_with(&root));
        assert!(AppConfig::worker_type_exports_dir().starts_with(&root));
        assert!(AppConfig::default()
            .resolved_brain_path()
            .starts_with(&root));
        assert!(data_root().starts_with(&root));
        assert!(AppConfig::default().resolved_orchestrator_root().starts_with(&root));

        AppConfig::update(|c| c.port = 4242).unwrap();
        assert_eq!(AppConfig::load().unwrap().port, 4242);
        assert!(iso.config_path().exists());
    }

    /// The leak this guards: a test that seeds or spawns into the default
    /// orchestrator root rewrote the real `~/ninox/orchestrators/.claude`
    /// (hooks and statusLine of every live orchestrator) with a test binary.
    #[test]
    #[should_panic(expected = "default orchestrator root")]
    fn the_default_orchestrator_root_is_denied_without_isolation() {
        let _ = AppConfig::default().resolved_orchestrator_root();
    }

    #[test]
    fn a_configured_orchestrator_root_needs_no_isolation() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = AppConfig { orchestrator_root: Some(dir.path().to_path_buf()), ..Default::default() };
        assert_eq!(cfg.resolved_orchestrator_root(), dir.path());
    }

    #[test]
    fn isolate_wins_over_env_overrides() {
        let other = tempfile::tempdir().unwrap();
        with_overrides(Some(&other.path().join("config.toml")), || {
            let iso = isolate();
            assert!(AppConfig::config_path().starts_with(iso.root()));
        });
    }

    #[test]
    fn env_override_outside_the_real_dirs_is_still_honored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        with_overrides(Some(&path), || assert_eq!(AppConfig::config_path(), path));
    }

    #[test]
    fn a_relative_scratch_config_still_gives_an_absolute_bin_dir() {
        with_overrides(Some(Path::new("rel-scratch/config.toml")), || {
            let bin = AppConfig::ninox_bin_dir();
            assert!(bin.is_absolute(), "{}", bin.display());
            assert!(bin.ends_with("rel-scratch/bin"), "{}", bin.display());
        });
    }

    #[test]
    fn isolate_with_config_pins_the_given_file_and_roots_paths_beside_it() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("custom.toml");
        let _iso = isolate_with_config(&file);
        assert_eq!(AppConfig::config_path(), file);
        assert_eq!(AppConfig::sessions_dir(), dir.path().join("sessions"));
        AppConfig::update(|c| c.port = 4343).unwrap();
        assert!(file.exists());
    }

    #[test]
    fn isolation_is_per_thread_and_restored_on_drop() {
        let outer = isolate();
        {
            let inner = isolate();
            assert!(AppConfig::config_path().starts_with(inner.root()));
        }
        assert!(AppConfig::config_path().starts_with(outer.root()));
        let other_thread = std::thread::spawn(active).join().unwrap();
        assert!(!other_thread);
        drop(outer);
        assert!(!active());
    }

    #[test]
    fn propagate_carries_the_override_onto_another_thread() {
        let iso = isolate();
        let root = iso.root().to_path_buf();
        let seen = std::thread::spawn(propagate(AppConfig::config_path))
            .join()
            .unwrap();
        assert!(seen.starts_with(&root));
        assert!(std::thread::spawn(propagate(active)).join().unwrap());
        drop(iso);
        assert!(!std::thread::spawn(propagate(active)).join().unwrap());
    }

    /// Integration tests and other crates' tests compile ninox-core without
    /// `cfg(test)`, so the guard only reaches them through the feature.
    #[test]
    fn every_dependent_crate_enables_test_isolation_in_dev_dependencies() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let root: toml::Value =
            toml::from_str(&std::fs::read_to_string(workspace.join("Cargo.toml")).unwrap())
                .unwrap();
        let members = root["workspace"]["members"].as_array().unwrap();
        let mut checked = 0;
        for member in members {
            let manifest: toml::Value = toml::from_str(
                &std::fs::read_to_string(
                    workspace.join(member.as_str().unwrap()).join("Cargo.toml"),
                )
                .unwrap(),
            )
            .unwrap();
            if !manifest
                .get("dependencies")
                .is_some_and(|d| d.get("ninox-core").is_some())
            {
                continue;
            }
            let features = manifest
                .get("dev-dependencies")
                .and_then(|d| d.get("ninox-core"))
                .and_then(|c| c.get("features"))
                .and_then(|f| f.as_array())
                .cloned()
                .unwrap_or_default();
            assert!(
                features.iter().any(|f| f.as_str() == Some("test-isolation")),
                "{member} depends on ninox-core but its [dev-dependencies] entry doesn't enable \"test-isolation\""
            );
            checked += 1;
        }
        assert!(checked >= 5, "only {checked} dependent crates found");
    }
}
