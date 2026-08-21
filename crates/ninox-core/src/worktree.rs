use crate::types::{PooledCheckoutLease, PooledCheckoutRecord};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};

const METADATA_SUFFIX: &str = ".ninox-worktree.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryIdentity {
    pub top_level: PathBuf,
    pub common_git_dir: PathBuf,
    pub git_dir: PathBuf,
}

impl RepositoryIdentity {
    pub fn resolve(workspace: &Path) -> Result<Self> {
        let top_level = git_path(workspace, &["rev-parse", "--show-toplevel"])
            .context("resolve repository top-level")?;
        let common_git_dir = git_path(
            workspace,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .context("resolve repository common git directory")?;
        let git_dir = git_path(
            workspace,
            &["rev-parse", "--path-format=absolute", "--git-dir"],
        )
        .context("resolve checkout git directory")?;
        Ok(Self {
            top_level,
            common_git_dir,
            git_dir,
        })
    }

    pub fn is_primary_checkout(&self) -> bool {
        self.git_dir == self.common_git_dir
    }
}

/// Whether a repository can use pooled worker checkouts.
///
/// Both paths must already be canonical (or otherwise normalized absolute
/// paths). Requiring the repository top-level to be an immediate child keeps
/// similarly-prefixed, nested, and unrelated repositories out of the pool.
pub fn is_pooling_eligible(
    repository: &RepositoryIdentity,
    canonical_repositories_root: Option<&Path>,
) -> bool {
    let Some(repositories_root) = canonical_repositories_root else {
        return false;
    };
    repository.is_primary_checkout()
        && repository.top_level.is_absolute()
        && repositories_root.is_absolute()
        && repository.top_level.starts_with(repositories_root)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddCheckout {
    Created,
    ExistingBranch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedWorktree {
    pub session_id: String,
    pub source_repo: PathBuf,
    pub worktree_path: PathBuf,
    /// Canonical shared git control directory. Optional only so sidecars from
    /// older builds deserialize; destructive operations reject missing data.
    #[serde(default)]
    pub common_git_dir: Option<PathBuf>,
    /// Canonical per-worktree git administration directory captured after add.
    #[serde(default)]
    pub worktree_git_dir: Option<PathBuf>,
    /// Random marker stored inside `worktree_git_dir`. Git may reuse the same
    /// admin path after remove/add; this token distinguishes that replacement.
    #[serde(default)]
    pub worktree_identity: Option<String>,
    /// Checked-out branch. Exact standalone paths need not match session IDs.
    #[serde(default)]
    pub branch: Option<String>,
}

/// Immutable ownership proof for a reusable pooled linked worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PooledWorktree {
    pub source_repo: PathBuf,
    pub path: PathBuf,
    pub common_git_dir: PathBuf,
    pub worktree_git_dir: PathBuf,
    pub worktree_identity: String,
}

impl PooledWorktree {
    /// Creates a linked worktree at the lease's exact path from the source
    /// checkout's committed HEAD. Existing paths are never adopted.
    pub fn create(lease: &PooledCheckoutLease) -> Result<Self> {
        anyhow::ensure!(
            lease.worktree_git_dir.is_none() && lease.worktree_identity.is_none(),
            "new pooled checkout lease already contains worktree identity"
        );
        validate_branch(&lease.source_repo, &lease.branch)?;
        let source = RepositoryIdentity::resolve(&lease.source_repo)?;
        anyhow::ensure!(
            source.common_git_dir == lease.common_git_dir,
            "pooled checkout source belongs to a different repository"
        );
        anyhow::ensure!(
            matches!(
                std::fs::symlink_metadata(&lease.path),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound
            ),
            "refusing to adopt existing pooled checkout path {}",
            lease.path.display()
        );
        if let Some(parent) = lease.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create pooled checkout parent {}", parent.display()))?;
        }
        let head = git_text(&lease.source_repo, &["rev-parse", "HEAD"])
            .context("resolve source checkout HEAD")?;
        let output = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(&lease.common_git_dir)
            .args(["worktree", "add", "-b"])
            .arg(&lease.branch)
            .arg(&lease.path)
            .arg(&head)
            .output()
            .context("create pooled linked worktree")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::ensure!(
                stderr.contains("already exists"),
                "git worktree add: {}",
                stderr.trim()
            );
            let existing = std::process::Command::new("git")
                .arg("--git-dir")
                .arg(&lease.common_git_dir)
                .args(["worktree", "add"])
                .arg(&lease.path)
                .arg(&lease.branch)
                .output()
                .context("create pooled linked worktree from existing branch")?;
            ensure_git_success(existing, "git worktree add existing branch")?;
        }

        let identity = RepositoryIdentity::resolve(&lease.path)?;
        anyhow::ensure!(
            identity.common_git_dir == lease.common_git_dir,
            "created pooled worktree belongs to a different repository"
        );
        let worktree_git_dir = git_path(
            &lease.path,
            &["rev-parse", "--path-format=absolute", "--git-dir"],
        )?;
        let worktree_identity = uuid::Uuid::new_v4().to_string();
        let marker = worktree_git_dir.join("ninox-identity");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker)
            .with_context(|| {
                format!(
                    "create pooled worktree identity marker {}",
                    marker.display()
                )
            })?;
        file.write_all(worktree_identity.as_bytes())
            .with_context(|| {
                format!("write pooled worktree identity marker {}", marker.display())
            })?;
        file.sync_all().with_context(|| {
            format!("sync pooled worktree identity marker {}", marker.display())
        })?;

        Ok(Self {
            source_repo: source.top_level,
            path: identity.top_level,
            common_git_dir: identity.common_git_dir,
            worktree_git_dir,
            worktree_identity,
        })
    }

    pub fn from_lease(lease: &PooledCheckoutLease) -> Result<Self> {
        Ok(Self {
            source_repo: lease.source_repo.clone(),
            path: lease.path.clone(),
            common_git_dir: lease.common_git_dir.clone(),
            worktree_git_dir: lease
                .worktree_git_dir
                .clone()
                .context("pooled checkout lease has no worktree git directory")?,
            worktree_identity: lease
                .worktree_identity
                .clone()
                .context("pooled checkout lease has no worktree identity")?,
        })
    }

    pub fn from_record(record: &PooledCheckoutRecord) -> Result<Self> {
        Ok(Self {
            source_repo: record.source_repo.clone(),
            path: record.path.clone(),
            common_git_dir: record.common_git_dir.clone(),
            worktree_git_dir: record
                .worktree_git_dir
                .clone()
                .context("pooled checkout record has no worktree git directory")?,
            worktree_identity: record
                .worktree_identity
                .clone()
                .context("pooled checkout record has no worktree identity")?,
        })
    }

    /// Recover a crash-interrupted reservation after Git created the linked
    /// worktree and identity marker but before SQLite was finalized.
    pub fn recover_provisioning(record: &PooledCheckoutRecord) -> Result<Self> {
        anyhow::ensure!(
            matches!(
                record.state,
                crate::types::PooledCheckoutState::Provisioning
            ),
            "pooled checkout is not awaiting provisioning recovery"
        );
        let identity = RepositoryIdentity::resolve(&record.path)?;
        anyhow::ensure!(
            identity.common_git_dir == record.common_git_dir,
            "provisioned checkout belongs to a different repository"
        );
        let worktree_git_dir = git_path(
            &record.path,
            &["rev-parse", "--path-format=absolute", "--git-dir"],
        )?;
        let worktree_identity = std::fs::read_to_string(worktree_git_dir.join("ninox-identity"))
            .context("read pooled worktree identity marker")?;
        anyhow::ensure!(
            !worktree_identity.is_empty(),
            "pooled worktree identity marker is empty"
        );
        Ok(Self {
            source_repo: record.source_repo.clone(),
            path: identity.top_level,
            common_git_dir: identity.common_git_dir,
            worktree_git_dir,
            worktree_identity,
        })
    }

    /// Verifies path, shared repository, per-worktree admin directory, and
    /// the immutable random marker. Missing/unmarked replacements never match.
    pub fn matches_identity(&self) -> Result<bool> {
        if !self.path.is_dir() {
            return Ok(false);
        }
        let Ok(identity) = RepositoryIdentity::resolve(&self.path) else {
            return Ok(false);
        };
        let Ok(worktree_git_dir) = git_path(
            &self.path,
            &["rev-parse", "--path-format=absolute", "--git-dir"],
        ) else {
            return Ok(false);
        };
        let marker = std::fs::read_to_string(worktree_git_dir.join("ninox-identity")).ok();
        Ok(identity.common_git_dir == self.common_git_dir
            && worktree_git_dir == self.worktree_git_dir
            && marker.as_deref() == Some(self.worktree_identity.as_str()))
    }

    pub fn lock_identity(&self) -> Result<std::fs::File> {
        let marker = self.worktree_git_dir.join("ninox-identity");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&marker)
            .with_context(|| format!("open pooled checkout identity {}", marker.display()))?;
        file.lock()
            .with_context(|| format!("lock pooled checkout identity {}", marker.display()))?;
        anyhow::ensure!(
            self.matches_identity()?,
            "pooled checkout identity changed while acquiring release lock"
        );
        Ok(file)
    }

    /// Switches an owned clean free slot to a fresh branch at source HEAD.
    /// No reset or clean is performed, so ignored caches remain in place.
    pub fn prepare_for_lease(lease: &PooledCheckoutLease) -> Result<Self> {
        let pooled = Self::from_lease(lease)?;
        anyhow::ensure!(
            pooled.matches_identity()?,
            "pooled checkout identity does not match registry"
        );
        validate_branch(&lease.source_repo, &lease.branch)?;
        let source = RepositoryIdentity::resolve(&lease.source_repo)?;
        anyhow::ensure!(
            source.common_git_dir == pooled.common_git_dir,
            "pooled checkout source belongs to a different repository"
        );
        ensure_clean(&pooled.path)?;
        let head = git_text(&lease.source_repo, &["rev-parse", "HEAD"])
            .context("resolve source checkout HEAD")?;
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&pooled.path)
            .args(["switch", "-c"])
            .arg(&lease.branch)
            .arg(&head)
            .output()
            .context("prepare pooled checkout branch")?;
        if output.status.success() {
            return Ok(pooled);
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::ensure!(
            stderr.contains("already exists"),
            "git switch -c failed: {}",
            stderr.trim()
        );
        let existing = std::process::Command::new("git")
            .arg("-C")
            .arg(&pooled.path)
            .args(["switch", &lease.branch])
            .output()
            .context("prepare pooled checkout existing branch")?;
        ensure_git_success(existing, "git switch existing branch")?;
        Ok(pooled)
    }

    pub fn recover_quarantined_creation(record: &PooledCheckoutRecord) -> Result<Self> {
        anyhow::ensure!(
            matches!(record.state, crate::types::PooledCheckoutState::Quarantined),
            "pooled checkout is not quarantined"
        );
        let branch = record
            .branch
            .as_deref()
            .context("quarantined pooled checkout has no branch")?;
        let identity = RepositoryIdentity::resolve(&record.path)?;
        anyhow::ensure!(
            identity.common_git_dir == record.common_git_dir
                && identity.top_level == record.path,
            "quarantined checkout does not match its registered repository/path"
        );
        let actual_branch =
            git_text(&record.path, &["symbolic-ref", "--quiet", "--short", "HEAD"])?;
        anyhow::ensure!(
            actual_branch == branch,
            "quarantined checkout branch is {actual_branch}, expected {branch}"
        );
        let worktree_git_dir = identity.git_dir;
        let marker = worktree_git_dir.join("ninox-identity");
        let worktree_identity = read_identity_marker(&marker)?
            .map_or_else(
                || recover_identity_marker_exclusively(&worktree_git_dir, &marker),
                Ok,
            )?;
        Ok(Self {
            source_repo: record.source_repo.clone(),
            path: identity.top_level,
            common_git_dir: identity.common_git_dir,
            worktree_git_dir,
            worktree_identity,
        })
    }

    /// Detaches a clean leased slot while retaining ignored files and the old
    /// branch. Dirty content is reported and never reset or deleted.
    pub fn release_clean(&self) -> Result<String> {
        anyhow::ensure!(
            self.matches_identity()?,
            "pooled checkout identity does not match registry"
        );
        ensure_clean(&self.path)?;
        let branch = git_text(&self.path, &["symbolic-ref", "--quiet", "--short", "HEAD"])
            .context("pooled checkout is not on a branch")?;
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&self.path)
            .args(["switch", "--detach"])
            .output()
            .context("detach pooled checkout")?;
        ensure_git_success(output, "git switch --detach")?;
        Ok(branch)
    }

    /// Release a finalized checkout only when its exact branch is clean and
    /// every commit at its tip is represented by a remote-tracking ref.
    pub fn release_recyclable(&self, expected_branch: &str) -> Result<String> {
        anyhow::ensure!(
            self.matches_identity()?,
            "pooled checkout identity does not match registry"
        );
        ensure_clean(&self.path)?;
        let symbolic = std::process::Command::new("git")
            .arg("-C")
            .arg(&self.path)
            .args(["symbolic-ref", "--quiet", "--short", "HEAD"])
            .output()
            .context("inspect pooled checkout branch")?;
        let already_detached = !symbolic.status.success();
        if already_detached {
            let head = git_text(&self.path, &["rev-parse", "HEAD"])?;
            let branch_head =
                git_text(&self.path, &["rev-parse", &format!("refs/heads/{expected_branch}")])?;
            anyhow::ensure!(
                head == branch_head,
                "detached pooled checkout no longer matches retained branch {expected_branch}"
            );
        } else {
            let branch = String::from_utf8(symbolic.stdout)
                .context("pooled checkout branch is not UTF-8")?
                .trim()
                .to_string();
            anyhow::ensure!(
                branch == expected_branch,
                "pooled checkout branch is {branch}, expected {expected_branch}"
            );
        }
        let containing = git_text(
            &self.path,
            &[
                "for-each-ref",
                "--format=%(refname)",
                "--contains=HEAD",
                "refs/remotes",
            ],
        )?;
        anyhow::ensure!(
            !containing.trim().is_empty(),
            "pooled checkout contains unpushed or remotely unreachable work"
        );
        if already_detached {
            Ok(expected_branch.to_string())
        } else {
            self.release_clean()
        }
    }

    pub fn ensure_clean(&self) -> Result<()> {
        anyhow::ensure!(
            self.matches_identity()?,
            "pooled checkout identity does not match registry"
        );
        ensure_clean(&self.path)
    }

    /// Make an owned clean checkout pool-ready. Already-detached slots are a
    /// no-op; attached branches are retained and merely detached.
    pub fn detach_if_needed(&self) -> Result<()> {
        self.ensure_clean()?;
        let branch = std::process::Command::new("git")
            .arg("-C")
            .arg(&self.path)
            .args(["symbolic-ref", "--quiet", "--short", "HEAD"])
            .output()
            .context("inspect pooled checkout branch")?;
        if !branch.status.success() {
            return Ok(());
        }
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&self.path)
            .args(["switch", "--detach"])
            .output()
            .context("detach pooled checkout")?;
        ensure_git_success(output, "git switch --detach")
    }
}

fn read_identity_marker(marker: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(marker) {
        Ok(value) if value.is_empty() => Ok(None),
        Ok(value) => {
            uuid::Uuid::parse_str(&value)
                .context("quarantined pooled worktree marker is not a UUID")?;
            Ok(Some(value))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error)
            .with_context(|| format!("read pooled worktree marker {}", marker.display())),
    }
}

fn recover_identity_marker_exclusively(worktree_git_dir: &Path, marker: &Path) -> Result<String> {
    let claim = worktree_git_dir.join("ninox-identity-recovery");
    let claim_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&claim)?;
    claim_file.lock()?;
    let result = (|| {
        if let Some(existing) = read_identity_marker(marker)? {
            return Ok(existing);
        }
        if marker.try_exists()? {
            std::fs::remove_file(marker)?;
        }
        let identity = uuid::Uuid::new_v4().to_string();
        let temporary =
            worktree_git_dir.join(format!(".ninox-identity-{}.tmp", uuid::Uuid::new_v4()));
        let mut file =
            std::fs::OpenOptions::new().write(true).create_new(true).open(&temporary)?;
        file.write_all(identity.as_bytes())?;
        file.sync_all()?;
        let published = match std::fs::hard_link(&temporary, marker) {
            Ok(()) => identity,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                read_identity_marker(marker)?
                    .context("concurrent marker winner published an empty identity")?
            }
            Err(error) => return Err(error.into()),
        };
        std::fs::remove_file(temporary)?;
        Ok(published)
    })();
    let unlock = claim_file.unlock();
    result.and_then(|identity| {
        unlock?;
        Ok(identity)
    })
}

impl ManagedWorktree {
    pub fn new(
        source_repo: &Path,
        remote_repo: Option<&str>,
        worktree_root: &Path,
        session_id: &str,
    ) -> Result<Self> {
        validate_session_id(session_id)?;
        let identity = RepositoryIdentity::resolve(source_repo)?;
        let root = absolute_path(worktree_root)?;
        let key = repo_key(&identity.common_git_dir, remote_repo);
        Ok(Self {
            session_id: session_id.to_string(),
            source_repo: identity.top_level,
            worktree_path: root.join(key).join(session_id),
            common_git_dir: Some(identity.common_git_dir),
            worktree_git_dir: None,
            worktree_identity: None,
            branch: Some(session_id.to_string()),
        })
    }

    pub fn new_at(
        source_repo: &Path,
        target: &Path,
        branch: &str,
        session_id: &str,
    ) -> Result<Self> {
        validate_session_id(session_id)?;
        anyhow::ensure!(!branch.is_empty(), "worktree branch cannot be empty");
        let identity = RepositoryIdentity::resolve(source_repo)?;
        Ok(Self {
            session_id: session_id.to_string(),
            source_repo: identity.top_level,
            worktree_path: absolute_path(target)?,
            common_git_dir: Some(identity.common_git_dir),
            worktree_git_dir: None,
            worktree_identity: None,
            branch: Some(branch.to_string()),
        })
    }

    pub fn add_checkout(&mut self) -> Result<AddCheckout> {
        let common_git_dir = self
            .common_git_dir
            .as_ref()
            .context("managed worktree metadata has no common git directory")?;
        let branch = self
            .branch
            .as_deref()
            .context("managed worktree metadata has no branch")?;
        let head = git_text(&self.source_repo, &["rev-parse", "HEAD"])
            .context("resolve authoritative worktree HEAD")?;

        let _ = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(common_git_dir)
            .args(["worktree", "prune"])
            .output();

        let out = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(common_git_dir)
            .args(["worktree", "add"])
            .arg(&self.worktree_path)
            .args(["-b", branch])
            .arg(&head)
            .output()
            .context("git worktree add")?;
        let result = if out.status.success() {
            AddCheckout::Created
        } else {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !stderr.contains("already exists") {
                anyhow::bail!("{}", stderr.trim());
            }
            let existing = std::process::Command::new("git")
                .arg("--git-dir")
                .arg(common_git_dir)
                .args(["worktree", "add"])
                .arg(&self.worktree_path)
                .arg(branch)
                .output()
                .context("git worktree add (existing branch)")?;
            if !existing.status.success() {
                anyhow::bail!("{}", String::from_utf8_lossy(&existing.stderr).trim());
            }
            AddCheckout::ExistingBranch
        };
        self.capture_checkout_identity()?;
        Ok(result)
    }

    pub fn capture_checkout_identity(&mut self) -> Result<()> {
        let identity = RepositoryIdentity::resolve(&self.worktree_path)?;
        let expected_common = self
            .common_git_dir
            .as_ref()
            .context("managed worktree metadata has no common git directory")?;
        anyhow::ensure!(
            identity.common_git_dir == *expected_common,
            "created worktree belongs to a different repository"
        );
        let worktree_git_dir = git_path(
            &self.worktree_path,
            &["rev-parse", "--path-format=absolute", "--git-dir"],
        )
        .context("resolve created worktree git directory")?;
        let identity = uuid::Uuid::new_v4().to_string();
        std::fs::write(worktree_git_dir.join("ninox-identity"), &identity)
            .context("write managed worktree identity marker")?;
        self.worktree_git_dir = Some(worktree_git_dir);
        self.worktree_identity = Some(identity);
        Ok(())
    }

    pub fn matches_existing_checkout(&self) -> Result<bool> {
        if !self.worktree_path.is_dir() {
            return Ok(false);
        }
        let (Some(expected_common), Some(expected_worktree), Some(expected_identity)) = (
            &self.common_git_dir,
            &self.worktree_git_dir,
            &self.worktree_identity,
        ) else {
            return Ok(false);
        };
        let Ok(identity) = RepositoryIdentity::resolve(&self.worktree_path) else {
            return Ok(false);
        };
        let Ok(worktree_git_dir) = git_path(
            &self.worktree_path,
            &["rev-parse", "--path-format=absolute", "--git-dir"],
        ) else {
            return Ok(false);
        };
        let marker = std::fs::read_to_string(worktree_git_dir.join("ninox-identity")).ok();
        Ok(identity.common_git_dir == *expected_common
            && worktree_git_dir == *expected_worktree
            && marker.as_deref() == Some(expected_identity))
    }

    /// Remove only the checkout whose immutable git identity was captured in
    /// this sidecar. Returns false for stale/partial/mismatched metadata.
    pub fn remove_checkout_if_matches(&self) -> Result<bool> {
        self.remove_checkout_if_matches_with_metadata(true)
    }

    pub fn remove_checkout_if_matches_with_metadata(&self, remove_metadata: bool) -> Result<bool> {
        let Some(common_git_dir) = self.common_git_dir.as_ref() else {
            return Ok(false);
        };
        if !self.worktree_path.exists() {
            let _ = std::process::Command::new("git")
                .arg("--git-dir")
                .arg(common_git_dir)
                .args(["worktree", "prune"])
                .output();
            if remove_metadata {
                self.remove_metadata()?;
            }
            return Ok(true);
        }
        if !self.matches_existing_checkout()? {
            return Ok(false);
        }
        let output = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(common_git_dir)
            .args(["worktree", "remove", "--force"])
            .arg(&self.worktree_path)
            .output()
            .context("git worktree remove")?;
        if !output.status.success() {
            anyhow::bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
        }
        if remove_metadata {
            self.remove_metadata()?;
        }
        Ok(true)
    }

    pub fn metadata_path(&self) -> PathBuf {
        metadata_path(&self.worktree_path, &self.session_id)
    }

    pub fn persist(&self) -> Result<()> {
        anyhow::ensure!(
            self.common_git_dir.is_some()
                && self.worktree_git_dir.is_some()
                && self.worktree_identity.is_some()
                && self.branch.is_some(),
            "refusing to persist incomplete managed worktree identity"
        );
        let path = self.metadata_path();
        let parent = path
            .parent()
            .context("managed worktree metadata has no parent")?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create worktree metadata directory {}", parent.display()))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("write worktree metadata {}", tmp.display()))?;
        std::fs::rename(&tmp, &path)
            .with_context(|| format!("install worktree metadata {}", path.display()))?;
        Ok(())
    }

    pub fn load_for_workspace(workspace: &Path, session_id: &str) -> Result<Option<Self>> {
        validate_session_id(session_id)?;
        let path = metadata_path(workspace, session_id);
        if !path.is_file() {
            return Ok(None);
        }
        let metadata: Self = serde_json::from_slice(
            &std::fs::read(&path)
                .with_context(|| format!("read worktree metadata {}", path.display()))?,
        )?;
        anyhow::ensure!(
            metadata.session_id == session_id,
            "worktree metadata session mismatch"
        );
        anyhow::ensure!(
            metadata.worktree_path == workspace,
            "worktree metadata path does not match workspace"
        );
        Ok(Some(metadata))
    }

    pub fn remove_metadata(&self) -> std::io::Result<()> {
        match std::fs::remove_file(self.metadata_path()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

pub fn repo_key(common_git_dir: &Path, remote_repo: Option<&str>) -> String {
    let readable = remote_repo
        .and_then(|slug| slug.split_once('/'))
        .map(|(owner, repo)| format!("{}--{}", key_part(owner), key_part(repo)))
        .unwrap_or_else(|| {
            common_git_dir
                .file_name()
                .and_then(|name| name.to_str())
                .map(key_part)
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "repo".to_string())
        });
    let digest = Sha256::digest(common_git_dir.as_os_str().as_encoded_bytes());
    let hash = digest[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{readable}-{hash}")
}

fn validate_branch(source_repo: &Path, branch: &str) -> Result<()> {
    anyhow::ensure!(!branch.is_empty(), "worktree branch cannot be empty");
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(source_repo)
        .args(["check-ref-format", "--branch"])
        .arg(branch)
        .output()
        .context("validate pooled checkout branch")?;
    ensure_git_success(output, "git check-ref-format --branch")
}

fn ensure_clean(worktree: &Path) -> Result<()> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["status", "--porcelain=v1", "--untracked-files=normal"])
        .output()
        .context("inspect pooled checkout status")?;
    anyhow::ensure!(
        output.status.success(),
        "git status failed in {}: {}",
        worktree.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    anyhow::ensure!(
        output.stdout.is_empty(),
        "pooled checkout is dirty:\n{}",
        String::from_utf8_lossy(&output.stdout).trim_end()
    );
    Ok(())
}

fn git_text(workspace: &Path, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(args)
        .output()
        .with_context(|| format!("run git {} in {}", args.join(" "), workspace.display()))?;
    anyhow::ensure!(
        output.status.success(),
        "git {} failed in {}: {}",
        args.join(" "),
        workspace.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8(output.stdout)
        .context("git returned non-UTF-8 output")?
        .trim()
        .to_string())
}

fn ensure_git_success(output: std::process::Output, operation: &str) -> Result<()> {
    anyhow::ensure!(
        output.status.success(),
        "{operation} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

fn validate_session_id(session_id: &str) -> Result<()> {
    let mut components = Path::new(session_id).components();
    anyhow::ensure!(
        matches!(components.next(), Some(std::path::Component::Normal(_)))
            && components.next().is_none(),
        "session id must be one safe path component"
    );
    Ok(())
}

fn git_path(workspace: &Path, args: &[&str]) -> Result<PathBuf> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(args)
        .output()
        .with_context(|| format!("run git in {}", workspace.display()))?;
    anyhow::ensure!(
        output.status.success(),
        "git {} failed in {}: {}",
        args.join(" "),
        workspace.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let raw = String::from_utf8(output.stdout).context("git returned a non-UTF-8 path")?;
    let path = PathBuf::from(raw.trim());
    let absolute = if path.is_absolute() {
        path
    } else {
        workspace.join(path)
    };
    absolute
        .canonicalize()
        .with_context(|| format!("canonicalize git path {}", absolute.display()))
}

fn key_part(value: &str) -> String {
    value
        .trim_end_matches(".git")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

fn metadata_path(workspace: &Path, session_id: &str) -> PathBuf {
    workspace
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(".{session_id}{METADATA_SUFFIX}"))
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn init_git_repo() -> PathBuf {
        let repo = tempdir().unwrap().keep();
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["init", "-q"]);
        run(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "--allow-empty",
            "-q",
            "-m",
            "init",
        ]);
        repo
    }

    fn pooled_lease(
        repo: &Path,
        target: &Path,
        session_id: &str,
        branch: &str,
        identity: Option<&PooledWorktree>,
    ) -> PooledCheckoutLease {
        let repo_identity = RepositoryIdentity::resolve(repo).unwrap();
        PooledCheckoutLease {
            path: target.to_path_buf(),
            source_repo: repo_identity.top_level,
            common_git_dir: repo_identity.common_git_dir,
            slot: 0,
            worktree_git_dir: identity.map(|pooled| pooled.worktree_git_dir.clone()),
            worktree_identity: identity.map(|pooled| pooled.worktree_identity.clone()),
            session_id: session_id.to_string(),
            owner_incarnation_id: session_id.to_string(),
            lease_id: uuid::Uuid::new_v4().to_string(),
            branch: branch.to_string(),
        }
    }

    #[test]
    fn pooling_eligibility_uses_component_containment_and_primary_checkout_identity() {
        let repositories_root = tempdir().unwrap();
        let direct = repositories_root.path().join("direct");
        let nested = repositories_root.path().join("group/nested");
        let outside_root = tempdir().unwrap();
        let outside = outside_root.path().join("outside");
        for path in [&direct, &nested, &outside] {
            std::fs::create_dir_all(path).unwrap();
        }
        let repositories_root = repositories_root.path().canonicalize().unwrap();
        let direct = direct.canonicalize().unwrap();
        let nested = nested.canonicalize().unwrap();
        let outside = outside.canonicalize().unwrap();

        let identity = |top_level: PathBuf, linked: bool| RepositoryIdentity {
            common_git_dir: top_level.join(".git"),
            git_dir: if linked {
                top_level.join(".git/worktrees/linked")
            } else {
                top_level.join(".git")
            },
            top_level,
        };
        assert!(is_pooling_eligible(
            &identity(direct.clone(), false),
            Some(&repositories_root)
        ));
        assert!(is_pooling_eligible(
            &identity(nested, false),
            Some(&repositories_root)
        ));
        assert!(!is_pooling_eligible(
            &identity(outside, false),
            Some(&repositories_root)
        ));
        assert!(!is_pooling_eligible(
            &identity(direct.clone(), true),
            Some(&repositories_root)
        ));
        assert!(!is_pooling_eligible(&identity(direct, false), None));
    }

    #[test]
    fn repository_identity_is_stable_for_subdirs_and_linked_worktrees() {
        let repo = init_git_repo();
        let subdir = repo.join("nested");
        std::fs::create_dir_all(&subdir).unwrap();
        let linked = repo.with_extension("identity-linked");
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["worktree", "add", "-q"])
            .arg(&linked)
            .arg("-b")
            .arg("identity-linked")
            .status()
            .unwrap();
        assert!(status.success());

        let root = RepositoryIdentity::resolve(&repo).unwrap();
        let nested = RepositoryIdentity::resolve(&subdir).unwrap();
        let linked = RepositoryIdentity::resolve(&linked).unwrap();

        assert_eq!(root.common_git_dir, nested.common_git_dir);
        assert_eq!(root.common_git_dir, linked.common_git_dir);
        assert_eq!(
            repo_key(&root.common_git_dir, Some("Acme/widgets")),
            repo_key(&linked.common_git_dir, Some("Acme/widgets")),
        );
    }

    #[test]
    fn exact_worktree_metadata_supports_a_different_session_name() {
        let repo = init_git_repo();
        let target = repo.with_extension("explicit-feature-path");
        let mut managed =
            ManagedWorktree::new_at(&repo, &target, "feature-branch", "different-session").unwrap();
        let status = managed.add_checkout().unwrap();
        assert_eq!(status, AddCheckout::Created);
        managed.persist().unwrap();

        let loaded = ManagedWorktree::load_for_workspace(&target, "different-session")
            .unwrap()
            .unwrap();
        assert_eq!(loaded.branch.as_deref(), Some("feature-branch"));
        assert!(loaded.matches_existing_checkout().unwrap());
    }

    #[test]
    fn replacement_worktree_does_not_match_stale_sidecar_identity() {
        let repo = init_git_repo();
        let root = repo.with_extension("managed-root");
        let mut managed =
            ManagedWorktree::new(&repo, Some("Acme/widgets"), &root, "stale").unwrap();
        managed.add_checkout().unwrap();
        managed.persist().unwrap();

        let status = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(managed.common_git_dir.as_ref().unwrap())
            .args(["worktree", "remove", "--force"])
            .arg(&managed.worktree_path)
            .status()
            .unwrap();
        assert!(status.success());
        let status = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(managed.common_git_dir.as_ref().unwrap())
            .args(["worktree", "add", "-q"])
            .arg(&managed.worktree_path)
            .arg("-b")
            .arg("replacement")
            .status()
            .unwrap();
        assert!(status.success());

        assert!(!managed.matches_existing_checkout().unwrap());
        assert!(!managed.remove_checkout_if_matches().unwrap());
        assert!(managed.worktree_path.exists());
    }

    #[test]
    fn management_survives_removal_of_the_linked_source_checkout() {
        let repo = init_git_repo();
        let linked = repo.with_extension("source-linked");
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["worktree", "add", "-q"])
            .arg(&linked)
            .arg("-b")
            .arg("source-linked")
            .status()
            .unwrap();
        assert!(status.success());
        let mut managed = ManagedWorktree::new(
            &linked,
            Some("Acme/widgets"),
            &repo.with_extension("managed-root"),
            "durable",
        )
        .unwrap();
        managed.add_checkout().unwrap();
        managed.persist().unwrap();

        let status = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(managed.common_git_dir.as_ref().unwrap())
            .args(["worktree", "remove", "--force"])
            .arg(&linked)
            .status()
            .unwrap();
        assert!(status.success());
        assert!(!linked.exists());

        assert!(managed.remove_checkout_if_matches().unwrap());
        assert!(!managed.worktree_path.exists());
        assert!(!managed.metadata_path().exists());
    }

    #[test]
    fn legacy_partial_sidecar_never_authorizes_removal() {
        let repo = init_git_repo();
        let target = repo.with_extension("legacy-partial");
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["worktree", "add", "-q"])
            .arg(&target)
            .arg("-b")
            .arg("legacy-partial")
            .status()
            .unwrap();
        assert!(status.success());
        let sidecar = metadata_path(&target, "old-session");
        std::fs::write(
            &sidecar,
            serde_json::json!({
                "session_id": "old-session",
                "source_repo": repo,
                "worktree_path": target,
            })
            .to_string(),
        )
        .unwrap();

        let loaded = ManagedWorktree::load_for_workspace(&target, "old-session")
            .unwrap()
            .unwrap();
        assert!(!loaded.remove_checkout_if_matches().unwrap());
        assert!(target.exists());
        assert!(sidecar.exists());
    }

    #[test]
    fn repo_key_uses_remote_slug_and_distinguishes_local_clones() {
        let root = tempdir().unwrap();
        let first = root.path().join("clone-a");
        let second = root.path().join("clone-b");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let first = first.canonicalize().unwrap();
        let second = second.canonicalize().unwrap();

        let a = repo_key(&first, Some("Acme/widgets"));
        let b = repo_key(&second, Some("Acme/widgets"));

        assert!(a.starts_with("Acme--widgets-"));
        assert!(b.starts_with("Acme--widgets-"));
        assert_ne!(a, b);
    }

    #[test]
    fn metadata_round_trips_outside_checkout() {
        let repo = init_git_repo();
        let mut managed =
            ManagedWorktree::new(&repo, None, &repo.with_extension("worktrees"), "s1").unwrap();
        managed.add_checkout().unwrap();
        managed.persist().unwrap();

        assert!(!managed.metadata_path().starts_with(&managed.worktree_path));
        assert_eq!(
            ManagedWorktree::load_for_workspace(&managed.worktree_path, "s1").unwrap(),
            Some(managed)
        );
    }

    #[test]
    fn managed_worktree_rejects_session_path_traversal() {
        let repo = init_git_repo();
        assert!(
            ManagedWorktree::new(&repo, None, &repo.with_extension("root"), "../escape").is_err()
        );
        assert!(ManagedWorktree::new(&repo, None, &repo.with_extension("root"), "").is_err());
    }

    #[test]
    fn managed_worktree_starts_at_supplied_linked_worktree_head() {
        let repo = init_git_repo();
        let linked = repo.with_extension("authoritative-linked");
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["worktree", "add", "-q", "-b", "authoritative"])
            .arg(&linked)
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::write(linked.join("authoritative.txt"), "linked\n").unwrap();
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&linked)
            .args(["add", "authoritative.txt"])
            .status()
            .unwrap();
        assert!(status.success());
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&linked)
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "authoritative",
            ])
            .status()
            .unwrap();
        assert!(status.success());
        let authoritative_head = git_text(&linked, &["rev-parse", "HEAD"]).unwrap();
        assert_ne!(authoritative_head, git_text(&repo, &["rev-parse", "HEAD"]).unwrap());

        let target = repo.with_extension("authoritative-managed");
        let mut managed =
            ManagedWorktree::new_at(&linked, &target, "managed-authoritative", "managed").unwrap();
        managed.add_checkout().unwrap();

        assert_eq!(git_text(&target, &["rev-parse", "HEAD"]).unwrap(), authoritative_head);
    }

    #[test]
    fn pooled_worktree_identity_rejects_replaced_marker() {
        let repo = init_git_repo();
        let target = repo.with_extension("pooled-identity");
        let lease = pooled_lease(&repo, &target, "identity-session", "identity-session", None);
        let pooled = PooledWorktree::create(&lease).unwrap();
        assert!(pooled.matches_identity().unwrap());

        std::fs::write(
            pooled.worktree_git_dir.join("ninox-identity"),
            "replacement-identity",
        )
        .unwrap();
        assert!(!pooled.matches_identity().unwrap());
    }

    #[test]
    fn pooled_release_rejects_dirty_checkout_without_modifying_it() {
        let repo = init_git_repo();
        let target = repo.with_extension("pooled-dirty");
        let lease = pooled_lease(&repo, &target, "dirty-session", "dirty-session", None);
        let pooled = PooledWorktree::create(&lease).unwrap();
        let dirty = target.join("untracked.txt");
        std::fs::write(&dirty, "do not delete").unwrap();

        let error = pooled.release_clean().unwrap_err().to_string();
        assert!(error.contains("pooled checkout is dirty"));
        assert_eq!(std::fs::read_to_string(dirty).unwrap(), "do not delete");
        assert_eq!(
            git_text(&target, &["branch", "--show-current"]).unwrap(),
            "dirty-session",
        );
    }

    #[test]
    fn recyclable_release_refuses_unpushed_or_wrong_branch_without_detaching() {
        let repo = init_git_repo();
        let target = repo.with_extension("pooled-unreachable");
        let lease = pooled_lease(&repo, &target, "release-session", "release-session", None);
        let pooled = PooledWorktree::create(&lease).unwrap();

        let error = pooled
            .release_recyclable("release-session")
            .unwrap_err()
            .to_string();
        assert!(error.contains("unpushed or remotely unreachable"));
        assert_eq!(
            git_text(&target, &["branch", "--show-current"]).unwrap(),
            "release-session"
        );
        assert!(pooled.release_recyclable("other-branch").is_err());
    }

    #[test]
    fn recyclable_release_preserves_branch_ref_and_detaches_reachable_tip() {
        let repo = init_git_repo();
        let target = repo.with_extension("pooled-reachable");
        let lease = pooled_lease(&repo, &target, "release-session", "release-session", None);
        let pooled = PooledWorktree::create(&lease).unwrap();
        let head = git_text(&target, &["rev-parse", "HEAD"]).unwrap();
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&target)
            .args(["update-ref", "refs/remotes/origin/release-session", &head])
            .status()
            .unwrap();
        assert!(status.success());

        assert_eq!(
            pooled.release_recyclable("release-session").unwrap(),
            "release-session"
        );
        assert_eq!(
            git_text(&repo, &["rev-parse", "refs/heads/release-session"]).unwrap(),
            head
        );
        assert!(git_text(&target, &["branch", "--show-current"]).unwrap().is_empty());
        assert_eq!(
            pooled.release_recyclable("release-session").unwrap(),
            "release-session"
        );
    }

    #[test]
    fn pooled_reuse_preserves_ignored_cache_old_branch_and_exact_path() {
        let repo = init_git_repo();
        std::fs::write(repo.join(".gitignore"), ".cache/\n").unwrap();
        std::fs::write(repo.join("tracked.txt"), "first\n").unwrap();
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["add", ".gitignore", "tracked.txt"])
            .status()
            .unwrap();
        assert!(status.success());
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "tracked",
            ])
            .status()
            .unwrap();
        assert!(status.success());

        let target = repo.with_extension("pooled-reuse");
        let first = pooled_lease(&repo, &target, "first-session", "session-first", None);
        let pooled = PooledWorktree::create(&first).unwrap();
        let cache = target.join(".cache/artifact");
        std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
        std::fs::write(&cache, "cached").unwrap();
        assert_eq!(pooled.release_clean().unwrap(), "session-first");

        std::fs::write(repo.join("second.txt"), "second\n").unwrap();
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["add", "second.txt"])
            .status()
            .unwrap();
        assert!(status.success());
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "second",
            ])
            .status()
            .unwrap();
        assert!(status.success());
        let source_head = git_text(&repo, &["rev-parse", "HEAD"]).unwrap();

        let second = pooled_lease(
            &repo,
            &target,
            "second-session",
            "session-second",
            Some(&pooled),
        );
        let reused = PooledWorktree::prepare_for_lease(&second).unwrap();

        assert_eq!(reused.path, target);
        assert_eq!(std::fs::read_to_string(cache).unwrap(), "cached");
        assert_eq!(
            git_text(&target, &["branch", "--show-current"]).unwrap(),
            "session-second",
        );
        assert_eq!(
            git_text(&target, &["rev-parse", "HEAD"]).unwrap(),
            source_head
        );
        assert!(
            git_text(&repo, &["show-ref", "--verify", "refs/heads/session-first"])
                .unwrap()
                .ends_with("refs/heads/session-first")
        );
    }
}
