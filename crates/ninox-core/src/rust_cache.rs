use crate::{
    config::RustCacheConfig,
    worktree::{PooledWorktree, RepositoryIdentity},
};
use anyhow::{Context, Result};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RustCacheEnvironment {
    Disabled,
    NotManagedCheckout,
    NotRust,
    ExplicitPolicy(Vec<(String, String)>),
    Unavailable(String),
    Enabled(Vec<(String, String)>),
}

impl RustCacheEnvironment {
    pub fn enabled_variables(&self) -> Option<BTreeMap<String, String>> {
        let Self::Enabled(variables) = self else {
            return None;
        };
        Some(variables.iter().cloned().collect())
    }

    pub fn into_enabled_variables(self) -> Vec<(String, String)> {
        match self {
            Self::Enabled(variables) => variables,
            _ => Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CargoPruneReport {
    pub removed_directories: u8,
    pub removed_metadata_files: u8,
}

pub fn worker_environment(
    config: &RustCacheConfig,
    workspace: &Path,
) -> Result<RustCacheEnvironment> {
    let ambient = std::env::vars_os()
        .filter_map(|(key, value)| {
            key.into_string()
                .ok()
                .map(|key| (key, value.to_string_lossy().into_owned()))
        })
        .collect();
    worker_environment_with_probe(config, workspace, &ambient, probe_sccache)
}

pub fn worker_environment_with_probe<F>(
    config: &RustCacheConfig,
    workspace: &Path,
    ambient: &BTreeMap<String, String>,
    probe: F,
) -> Result<RustCacheEnvironment>
where
    F: FnOnce(&Path) -> Result<PathBuf>,
{
    if !config.enabled {
        return Ok(RustCacheEnvironment::Disabled);
    }
    config.validate()?;

    let identity = match RepositoryIdentity::resolve(workspace) {
        Ok(identity) if !identity.is_primary_checkout() && has_ninox_identity(&identity) => identity,
        Ok(_) | Err(_) => return Ok(RustCacheEnvironment::NotManagedCheckout),
    };
    if !is_rust_repository(&identity.top_level)? {
        return Ok(RustCacheEnvironment::NotRust);
    }

    let explicit = explicit_policy(ambient);
    if !explicit.is_empty() {
        return Ok(RustCacheEnvironment::ExplicitPolicy(explicit));
    }

    let configured_executable = config.resolved_executable();
    let executable = match probe(&configured_executable) {
        Ok(executable) => executable,
        Err(error) => {
            return Ok(RustCacheEnvironment::Unavailable(format!(
                "shared Rust caching is enabled, but sccache executable {:?} is unavailable: {error}",
                config.executable
            )));
        }
    };
    let workspace = identity
        .top_level
        .canonicalize()
        .context("canonicalize Rust worker checkout")?;
    let configured_cache = config.resolved_cache_dir();
    let cache = canonicalize_allow_missing(&configured_cache).with_context(|| {
        format!(
            "resolve shared sccache directory {}",
            configured_cache.display()
        )
    })?;
    ensure_cache_outside_worktrees(&workspace, &cache)?;
    ensure_cache_outside_managed_checkout(&cache)?;
    fs::create_dir_all(&cache)
        .with_context(|| format!("create shared sccache directory {}", cache.display()))?;
    let cache = cache
        .canonicalize()
        .with_context(|| format!("canonicalize shared sccache directory {}", cache.display()))?;
    ensure_cache_outside_worktrees(&workspace, &cache)?;
    ensure_cache_outside_managed_checkout(&cache)?;

    Ok(RustCacheEnvironment::Enabled(vec![
        (
            "RUSTC_WRAPPER".to_string(),
            executable.to_string_lossy().into_owned(),
        ),
        ("CARGO_INCREMENTAL".to_string(), "0".to_string()),
        (
            "SCCACHE_DIR".to_string(),
            cache.to_string_lossy().into_owned(),
        ),
        (
            "SCCACHE_CACHE_SIZE".to_string(),
            format!("{}G", config.cache_size_gib),
        ),
    ]))
}

pub fn prune_cargo_outputs(pooled: &PooledWorktree) -> Result<CargoPruneReport> {
    anyhow::ensure!(
        pooled.matches_identity()?,
        "pooled checkout identity changed; refusing Cargo cleanup"
    );
    let identity = RepositoryIdentity::resolve(&pooled.path)?;
    anyhow::ensure!(
        !identity.is_primary_checkout(),
        "refusing Cargo cleanup in a primary checkout"
    );
    anyhow::ensure!(
        identity.top_level == pooled.path,
        "pooled checkout path no longer resolves to its exact workspace"
    );
    if !is_rust_repository(&pooled.path)? {
        return Ok(CargoPruneReport::default());
    }
    ensure_git_clean(&pooled.path)?;

    let target = pooled.path.join("target");
    let target_metadata = match fs::symlink_metadata(&target) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CargoPruneReport::default());
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect Cargo target {}", target.display()));
        }
    };
    anyhow::ensure!(
        !target_metadata.file_type().is_symlink(),
        "Cargo target path {} is a symlink; refusing cleanup",
        target.display()
    );
    anyhow::ensure!(
        target_metadata.is_dir(),
        "Cargo target path {} is not a directory; refusing cleanup",
        target.display()
    );

    let mut report = CargoPruneReport::default();
    for name in ["debug", "release"] {
        let path = target.join(name);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect Cargo output {}", path.display()));
            }
        };
        anyhow::ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "Cargo output {} is not an ordinary directory; refusing cleanup",
            path.display()
        );
        fs::remove_dir_all(&path)
            .with_context(|| format!("prune Cargo output {}", path.display()))?;
        report.removed_directories += 1;
    }
    for name in [".rustc_info.json", "CACHEDIR.TAG", ".cargo-lock"] {
        let path = target.join(name);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect Cargo metadata {}", path.display()));
            }
        };
        anyhow::ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "Cargo metadata {} is not an ordinary file; refusing cleanup",
            path.display()
        );
        fs::remove_file(&path)
            .with_context(|| format!("prune Cargo metadata {}", path.display()))?;
        report.removed_metadata_files += 1;
    }
    Ok(report)
}

fn explicit_policy(ambient: &BTreeMap<String, String>) -> Vec<(String, String)> {
    ambient
        .iter()
        .filter(|(key, _)| {
            matches!(key.as_str(), "RUSTC_WRAPPER" | "CARGO_INCREMENTAL")
                || key.starts_with("SCCACHE_")
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn has_ninox_identity(identity: &RepositoryIdentity) -> bool {
    fs::read_to_string(identity.git_dir.join("ninox-identity"))
        .ok()
        .is_some_and(|value| uuid::Uuid::parse_str(value.trim()).is_ok())
}

fn is_rust_repository(workspace: &Path) -> Result<bool> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["ls-files", "-z"])
        .output()
        .context("inspect tracked repository files for Cargo manifests")?;
    anyhow::ensure!(
        output.status.success(),
        "git ls-files failed while detecting Rust repository: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .any(|path| {
            let path = String::from_utf8_lossy(path);
            Path::new(path.as_ref()).file_name() == Some(std::ffi::OsStr::new("Cargo.toml"))
        }))
}

fn ensure_git_clean(workspace: &Path) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["status", "--porcelain"])
        .output()
        .context("inspect worker checkout cleanliness before Cargo cleanup")?;
    anyhow::ensure!(
        output.status.success(),
        "git status failed before Cargo cleanup: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    anyhow::ensure!(
        output.stdout.is_empty(),
        "worker checkout is dirty; refusing Cargo cleanup"
    );
    Ok(())
}

fn ensure_cache_outside_worktrees(workspace: &Path, cache: &Path) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["worktree", "list", "--porcelain", "-z"])
        .output()
        .context("inspect repository worktrees for shared cache containment")?;
    anyhow::ensure!(
        output.status.success(),
        "git worktree list failed while validating shared cache path: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    for field in output.stdout.split(|byte| *byte == 0) {
        let Some(path) = field.strip_prefix(b"worktree ") else {
            continue;
        };
        let path = PathBuf::from(String::from_utf8_lossy(path).into_owned());
        let path = path
            .canonicalize()
            .with_context(|| format!("canonicalize Git worktree {}", path.display()))?;
        anyhow::ensure!(
            !cache.starts_with(&path),
            "shared sccache directory {} resolves inside worker checkout {}",
            cache.display(),
            path.display()
        );
    }
    Ok(())
}

fn ensure_cache_outside_managed_checkout(cache: &Path) -> Result<()> {
    for ancestor in cache.ancestors() {
        match fs::symlink_metadata(ancestor.join(".git")) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| {
                format!("inspect Git boundary at {}", ancestor.display())
            }),
        }
        let identity = RepositoryIdentity::resolve(ancestor).with_context(|| {
            format!(
                "shared sccache path crosses an unverifiable Git boundary at {}",
                ancestor.display()
            )
        })?;
        match fs::symlink_metadata(identity.git_dir.join("ninox-identity")) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "inspect managed worker marker for Git checkout {}",
                        identity.top_level.display()
                    )
                });
            }
        }
        anyhow::bail!(
            "shared sccache directory {} resolves inside managed worker checkout {}",
            cache.display(),
            identity.top_level.display()
        );
    }
    Ok(())
}

fn canonicalize_allow_missing(path: &Path) -> Result<PathBuf> {
    anyhow::ensure!(
        path.is_absolute(),
        "shared sccache directory must resolve to an absolute path"
    );
    let mut ancestor = path;
    let mut missing = Vec::new();
    loop {
        match ancestor.try_exists() {
            Ok(true) => break,
            Ok(false) => {
                missing.push(
                    ancestor
                        .file_name()
                        .context("shared sccache path has no existing ancestor")?
                        .to_os_string(),
                );
                ancestor = ancestor
                    .parent()
                    .context("shared sccache path has no existing ancestor")?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    let mut resolved = ancestor.canonicalize()?;
    for component in missing.into_iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn probe_sccache(configured: &Path) -> Result<PathBuf> {
    let executable = resolve_executable(configured)?;
    let output = Command::new(&executable)
        .arg("--version")
        .output()
        .with_context(|| format!("run {} --version", executable.display()))?;
    anyhow::ensure!(
        output.status.success(),
        "{} --version failed: {}",
        executable.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(executable)
}

fn resolve_executable(configured: &Path) -> Result<PathBuf> {
    if configured.components().count() > 1 || configured.is_absolute() {
        return configured
            .canonicalize()
            .with_context(|| format!("resolve sccache executable {}", configured.display()));
    }
    let path = std::env::var_os("PATH").context("PATH is unavailable")?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(configured);
        if candidate.is_file() && is_executable(&candidate)? {
            return candidate
                .canonicalize()
                .with_context(|| format!("resolve sccache executable {}", candidate.display()));
        }
    }
    anyhow::bail!("{:?} was not found as an executable on PATH", configured)
}

#[cfg(unix)]
fn is_executable(path: &Path) -> Result<bool> {
    use std::os::unix::fs::PermissionsExt;
    Ok(path.metadata()?.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> Result<bool> {
    Ok(path.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RustCacheConfig;
    use std::{
        collections::BTreeMap,
        fs,
        path::{Path, PathBuf},
        process::Command,
    };
    use tempfile::TempDir;

    fn git(path: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_text(path: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn worker_checkout(rust: bool) -> (TempDir, PathBuf, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let primary = root.path().join("repo");
        fs::create_dir(&primary).unwrap();
        git(&primary, &["init", "-b", "main"]);
        fs::write(primary.join(".gitignore"), "/target\nignored-evidence/\n").unwrap();
        if rust {
            fs::write(
                primary.join("Cargo.toml"),
                "[package]\nname='fixture'\nversion='0.1.0'\n",
            )
            .unwrap();
        } else {
            fs::write(primary.join("README.md"), "not rust\n").unwrap();
        }
        git(&primary, &["add", "."]);
        git(
            &primary,
            &[
                "-c",
                "user.name=Ninox Tests",
                "-c",
                "user.email=ninox@example.invalid",
                "commit",
                "-m",
                "fixture",
            ],
        );
        let worker = root.path().join("repo-w1");
        git(
            &primary,
            &[
                "worktree",
                "add",
                "-b",
                "worker",
                worker.to_str().unwrap(),
                "HEAD",
            ],
        );
        let identity = RepositoryIdentity::resolve(&worker).unwrap();
        fs::write(
            identity.git_dir.join("ninox-identity"),
            "00000000-0000-4000-8000-000000000001",
        )
        .unwrap();
        let cache = root.path().join("shared-sccache");
        (root, worker, cache)
    }

    fn pooled(path: &Path) -> PooledWorktree {
        let identity = RepositoryIdentity::resolve(path).unwrap();
        let marker = "00000000-0000-4000-8000-000000000001".to_string();
        fs::write(identity.git_dir.join("ninox-identity"), &marker).unwrap();
        PooledWorktree {
            source_repo: path.to_path_buf(),
            path: identity.top_level,
            common_git_dir: identity.common_git_dir,
            worktree_git_dir: identity.git_dir,
            worktree_identity: marker,
        }
    }

    fn config(cache_dir: PathBuf) -> RustCacheConfig {
        RustCacheConfig {
            enabled: true,
            executable: PathBuf::from("sccache"),
            cache_dir: Some(cache_dir),
            cache_size_gib: 12,
            prune_on_release: true,
        }
    }

    fn env(
        config: &RustCacheConfig,
        workspace: &Path,
        ambient: &BTreeMap<String, String>,
    ) -> RustCacheEnvironment {
        worker_environment_with_probe(config, workspace, ambient, |_| {
            Ok(PathBuf::from("/opt/homebrew/bin/sccache"))
        })
        .unwrap()
    }

    #[test]
    fn explicit_rust_or_sccache_policy_is_forwarded_unchanged() {
        let (_root, worker, cache) = worker_checkout(true);
        for key in [
            "RUSTC_WRAPPER",
            "CARGO_INCREMENTAL",
            "SCCACHE_DIR",
            "SCCACHE_CACHE_SIZE",
            "SCCACHE_IDLE_TIMEOUT",
        ] {
            let ambient = BTreeMap::from([(key.to_string(), "user-policy".to_string())]);
            assert_eq!(
                env(&config(cache.clone()), &worker, &ambient),
                RustCacheEnvironment::ExplicitPolicy(vec![(
                    key.to_string(),
                    "user-policy".to_string()
                )])
            );
        }

        let ambient = BTreeMap::from([
            ("SCCACHE_DIR".to_string(), "/user/cache".to_string()),
            ("SCCACHE_CACHE_SIZE".to_string(), "7G".to_string()),
        ]);
        assert_eq!(
            env(&config(cache), &worker, &ambient),
            RustCacheEnvironment::ExplicitPolicy(ambient.into_iter().collect())
        );
    }

    #[test]
    fn only_linked_rust_worker_checkouts_are_eligible() {
        let (_rust_root, rust_worker, rust_cache) = worker_checkout(true);
        assert!(matches!(
            env(&config(rust_cache), &rust_worker, &BTreeMap::new()),
            RustCacheEnvironment::Enabled(_)
        ));
        let rust_identity = RepositoryIdentity::resolve(&rust_worker).unwrap();
        fs::remove_file(rust_identity.git_dir.join("ninox-identity")).unwrap();
        assert_eq!(
            env(
                &config(rust_worker.parent().unwrap().join("unused-cache")),
                &rust_worker,
                &BTreeMap::new()
            ),
            RustCacheEnvironment::NotManagedCheckout
        );

        let (_non_rust_root, non_rust_worker, non_rust_cache) = worker_checkout(false);
        assert_eq!(
            env(&config(non_rust_cache), &non_rust_worker, &BTreeMap::new()),
            RustCacheEnvironment::NotRust
        );
    }

    #[test]
    fn worktrees_share_only_the_bounded_sccache() {
        let (root, worker_one, cache) = worker_checkout(true);
        let primary = root.path().join("repo");
        let worker_two = root.path().join("repo-w2");
        git(
            &primary,
            &[
                "worktree",
                "add",
                "-b",
                "worker-two",
                worker_two.to_str().unwrap(),
                "HEAD",
            ],
        );
        let _ = pooled(&worker_two);
        let policy = config(cache);
        let first = env(&policy, &worker_one, &BTreeMap::new())
            .enabled_variables()
            .unwrap();
        let second = env(&policy, &worker_two, &BTreeMap::new())
            .enabled_variables()
            .unwrap();

        assert_eq!(first.get("SCCACHE_DIR"), second.get("SCCACHE_DIR"));
        assert_eq!(
            first.get("SCCACHE_CACHE_SIZE").map(String::as_str),
            Some("12G")
        );
        assert_eq!(
            first.get("CARGO_INCREMENTAL").map(String::as_str),
            Some("0")
        );
        assert_eq!(
            first.get("RUSTC_WRAPPER").map(String::as_str),
            Some("/opt/homebrew/bin/sccache")
        );
        assert!(!first.contains_key("CARGO_TARGET_DIR"));
        assert!(!second.contains_key("CARGO_TARGET_DIR"));
    }

    #[test]
    fn unavailable_sccache_is_clear_and_non_mutating() {
        let (_root, worker, cache) = worker_checkout(true);
        let status =
            worker_environment_with_probe(&config(cache), &worker, &BTreeMap::new(), |_| {
                anyhow::bail!("not found on PATH")
            })
            .unwrap();
        assert!(matches!(
            status,
            RustCacheEnvironment::Unavailable(message)
                if message.contains("sccache") && message.contains("not found on PATH")
        ));
    }

    #[test]
    fn cache_path_cannot_resolve_inside_the_worker() {
        let (root, worker, _cache) = worker_checkout(true);
        let direct = config(worker.join(".cache/sccache"));
        assert!(
            worker_environment_with_probe(&direct, &worker, &BTreeMap::new(), |_| Ok(
                PathBuf::from("/bin/sccache")
            ),)
            .unwrap_err()
            .to_string()
            .contains("inside")
        );

        let outside_link = worker.parent().unwrap().join("cache-link");
        fs::create_dir_all(worker.join("nested-cache")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(worker.join("nested-cache"), &outside_link).unwrap();
        let linked = config(outside_link);
        assert!(
            worker_environment_with_probe(&linked, &worker, &BTreeMap::new(), |_| Ok(
                PathBuf::from("/bin/sccache")
            ),)
            .unwrap_err()
            .to_string()
            .contains("inside")
        );

        let other_worker = root.path().join("repo-w2");
        git(
            &root.path().join("repo"),
            &[
                "worktree",
                "add",
                "-b",
                "worker-two",
                other_worker.to_str().unwrap(),
                "HEAD",
            ],
        );
        assert!(worker_environment_with_probe(
            &config(other_worker.join(".sccache")),
            &worker,
            &BTreeMap::new(),
            |_| Ok(PathBuf::from("/bin/sccache")),
        )
        .unwrap_err()
        .to_string()
        .contains("inside"));
    }

    #[test]
    fn unmanaged_git_ancestor_does_not_disable_platform_cache() {
        let home = tempfile::tempdir().unwrap();
        git(home.path(), &["init", "-b", "dotfiles"]);
        let cache = home.path().join(".cache/ninox/sccache");
        fs::create_dir_all(&cache).unwrap();

        ensure_cache_outside_managed_checkout(&cache).unwrap();

        let (_root, worker, _cache) = worker_checkout(true);
        let (_foreign_root, foreign_worker, _foreign_cache) = worker_checkout(true);
        let error = worker_environment_with_probe(
            &config(foreign_worker.join(".sccache")),
            &worker,
            &BTreeMap::new(),
            |_| Ok(PathBuf::from("/bin/sccache")),
        )
        .unwrap_err();
        assert!(error.to_string().contains("managed worker checkout"));
    }

    #[test]
    fn release_prune_removes_only_known_cargo_outputs() {
        let (root, worker, _cache) = worker_checkout(true);
        for path in [
            "target/debug/deps/a",
            "target/release/deps/b",
            "target/rollout-evidence/report.json",
            "target/debug-backup/keep",
            "ignored-evidence/keep",
        ] {
            let path = worker.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "data").unwrap();
        }
        for name in [".rustc_info.json", "CACHEDIR.TAG", ".cargo-lock"] {
            fs::write(worker.join("target").join(name), "metadata").unwrap();
        }
        #[cfg(unix)]
        {
            let external = root.path().join("external-artifact");
            fs::write(&external, "must survive").unwrap();
            std::os::unix::fs::symlink(&external, worker.join("target/debug/external-link"))
                .unwrap();
        }

        let report = prune_cargo_outputs(&pooled(&worker)).unwrap();

        assert_eq!(report.removed_directories, 2);
        assert_eq!(report.removed_metadata_files, 3);
        assert!(!worker.join("target/debug").exists());
        assert!(!worker.join("target/release").exists());
        assert!(worker.join("target/rollout-evidence/report.json").exists());
        assert!(worker.join("target/debug-backup/keep").exists());
        assert!(worker.join("ignored-evidence/keep").exists());
        #[cfg(unix)]
        assert!(root.path().join("external-artifact").exists());
    }

    #[test]
    fn prune_refuses_primary_dirty_and_symlinked_targets() {
        let (root, worker, _cache) = worker_checkout(true);
        assert!(prune_cargo_outputs(&pooled(&root.path().join("repo"))).is_err());

        fs::write(worker.join("untracked.txt"), "dirty").unwrap();
        let pooled_worker = pooled(&worker);
        assert!(prune_cargo_outputs(&pooled_worker)
            .unwrap_err()
            .to_string()
            .contains("dirty"));
        fs::remove_file(worker.join("untracked.txt")).unwrap();

        let external = root.path().join("external-target");
        fs::create_dir(&external).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&external, worker.join("target")).unwrap();
        assert!(prune_cargo_outputs(&pooled_worker)
            .unwrap_err()
            .to_string()
            .contains("symlink"));
        assert!(external.exists());
    }

    #[test]
    fn prune_refuses_reassigned_checkout_identity() {
        let (_root, worker, _cache) = worker_checkout(true);
        let pooled_worker = pooled(&worker);
        let output = worker.join("target/debug/deps/output");
        fs::create_dir_all(output.parent().unwrap()).unwrap();
        fs::write(&output, "keep").unwrap();
        fs::write(
            pooled_worker.worktree_git_dir.join("ninox-identity"),
            "00000000-0000-4000-8000-000000000002",
        )
        .unwrap();

        assert!(prune_cargo_outputs(&pooled_worker)
            .unwrap_err()
            .to_string()
            .contains("identity changed"));
        assert!(output.exists());
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_failure_is_actionable_and_retryable() {
        use std::os::unix::fs::PermissionsExt;

        let (_root, worker, _cache) = worker_checkout(true);
        let pooled_worker = pooled(&worker);
        let output = worker.join("target/debug/deps/output");
        fs::create_dir_all(output.parent().unwrap()).unwrap();
        fs::write(&output, "retry").unwrap();
        let target = worker.join("target");
        let head = git_text(&worker, &["rev-parse", "HEAD"]);
        git(
            &worker,
            &["update-ref", "refs/remotes/origin/worker", &head],
        );
        fs::set_permissions(&target, fs::Permissions::from_mode(0o500)).unwrap();

        pooled_worker.ensure_recyclable("worker").unwrap();
        let first = prune_cargo_outputs(&pooled_worker).unwrap_err();
        assert!(first.to_string().contains("prune Cargo output"));
        assert!(worker.join("target/debug").exists());
        assert_eq!(
            git_text(&worker, &["branch", "--show-current"]),
            "worker"
        );

        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        let retry = prune_cargo_outputs(&pooled_worker).unwrap();
        assert_eq!(retry.removed_directories, 1);
        assert!(!worker.join("target/debug").exists());
    }
}
