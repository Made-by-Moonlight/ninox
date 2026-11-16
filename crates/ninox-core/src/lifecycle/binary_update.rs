//! Prebuilt-binary self-update (`ninox update`): resolves the newest
//! `ninox-macos` generic package that `publish-codeartifact.yml` publishes to
//! CodeArtifact on every release (or `ninox-macos-alpha`, published from
//! `v1` by `publish-alpha.yml`, on the alpha channel), downloads it through the user's `aws` CLI
//! (their SSO session is the only credential), verifies it against the
//! SHA-256 CodeArtifact recorded at publish time, and swaps it in.
//!
//! Shells out to `aws` rather than linking a CodeArtifact SDK client: the
//! install script needs the CLI anyway, and SSO token refresh is its job.
//! `scripts/install-macos.sh` mirrors the coordinates and asset names here —
//! change both together.

use anyhow::{bail, ensure, Context, Result};
use crate::config::UpdateChannel;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

pub const GENERIC_NAMESPACE: &str = "ninox";
pub const APP_ZIP_ASSET: &str = "Ninox.app.zip";
pub const INSTALL_SCRIPT_ASSET: &str = "install-macos.sh";

/// The target triple CodeArtifact publishes a prebuilt for, or `None` if
/// `(os, arch)` isn't one of the supported platforms.
pub fn target_triple(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-gnu"),
        _ => None,
    }
}

/// The CodeArtifact generic package that publishes `channel`'s prebuilts for
/// `os`, or `None` if `os` isn't one of the supported platforms. Alphas live
/// in their own `-alpha` package so a stable lookup can never see one.
pub fn generic_package(os: &str, channel: UpdateChannel) -> Option<&'static str> {
    match (os, channel) {
        ("macos", UpdateChannel::Stable) => Some("ninox-macos"),
        ("linux", UpdateChannel::Stable) => Some("ninox-linux"),
        ("macos", UpdateChannel::Alpha) => Some("ninox-macos-alpha"),
        ("linux", UpdateChannel::Alpha) => Some("ninox-linux-alpha"),
        _ => None,
    }
}

pub fn platform_supported(os: &str, arch: &str) -> bool {
    target_triple(os, arch).is_some()
}

pub fn tarball_stem(version: &str, target_triple: &str) -> String {
    format!("ninox-{version}-{target_triple}")
}

pub fn tarball_asset(version: &str, target_triple: &str) -> String {
    format!("{}.tar.gz", tarball_stem(version, target_triple))
}

/// Where the published package lives. Defaults match the CodeArtifact
/// registry the README's private-registry section documents; the owner
/// account is derived from the caller's identity when unset so no account
/// id is baked in.
#[derive(Debug, Clone, PartialEq)]
pub struct CodeArtifactCoords {
    pub domain: String,
    pub domain_owner: Option<String>,
    pub repository: String,
    pub region: String,
}

impl CodeArtifactCoords {
    pub fn from_env_with(get: impl Fn(&str) -> Option<String>) -> Self {
        let get = |k: &str| get(k).filter(|v| !v.is_empty());
        Self {
            domain: get("NINOX_CODEARTIFACT_DOMAIN").unwrap_or_else(|| "synthesia-build".into()),
            domain_owner: get("NINOX_CODEARTIFACT_DOMAIN_OWNER"),
            repository: get("NINOX_CODEARTIFACT_REPOSITORY").unwrap_or_else(|| "synthesia-cargo".into()),
            region: get("NINOX_CODEARTIFACT_REGION").unwrap_or_else(|| "eu-west-1".into()),
        }
    }

    pub fn from_env() -> Self {
        Self::from_env_with(|k| std::env::var(k).ok())
    }

    fn base_args(&self, subcommand: &str, owner: &str, package: &str) -> Vec<String> {
        [
            "codeartifact", subcommand,
            "--domain", &self.domain,
            "--domain-owner", owner,
            "--repository", &self.repository,
            "--region", &self.region,
            "--format", "generic",
            "--namespace", GENERIC_NAMESPACE,
            "--package", package,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    pub fn list_versions_args(&self, owner: &str, package: &str) -> Vec<String> {
        let mut args = self.base_args("list-package-versions", owner, package);
        args.extend(["--status", "Published", "--output", "json"].map(String::from));
        args
    }

    pub fn list_assets_args(&self, owner: &str, package: &str, version: &str) -> Vec<String> {
        let mut args = self.base_args("list-package-version-assets", owner, package);
        args.extend(["--package-version", version, "--output", "json"].map(String::from));
        args
    }

    pub fn get_asset_args(&self, owner: &str, package: &str, version: &str, asset: &str, outfile: &Path) -> Vec<String> {
        let mut args = self.base_args("get-package-version-asset", owner, package);
        args.extend(["--package-version", version, "--asset", asset].map(String::from));
        args.push(outfile.to_string_lossy().into_owned());
        args
    }
}

#[derive(Deserialize)]
struct VersionList {
    #[serde(default)]
    versions: Vec<VersionEntry>,
}

#[derive(Deserialize)]
struct VersionEntry {
    version: String,
    #[serde(default)]
    status: String,
}

/// Highest Published semver in `list-package-versions` JSON. Stable skips
/// prereleases (they install only via an explicit `--version`); the alpha
/// package holds nothing but prereleases, so alpha keeps them and relies on
/// semver ordering (`alpha.10` > `alpha.9`). Sorted locally because the API
/// only offers PUBLISHED_TIME ordering, and a re-published old patch must
/// not outrank a newer release.
pub fn latest_published_version(json: &str, channel: UpdateChannel) -> Result<Option<semver::Version>> {
    let list: VersionList = serde_json::from_str(json).context("parsing list-package-versions output")?;
    Ok(list
        .versions
        .into_iter()
        .filter(|v| v.status == "Published")
        .filter_map(|v| semver::Version::parse(&v.version).ok())
        .filter(|v| channel == UpdateChannel::Alpha || v.pre.is_empty())
        .max())
}

#[derive(Deserialize)]
struct AssetList {
    #[serde(default)]
    assets: Vec<AssetEntry>,
}

#[derive(Deserialize)]
struct AssetEntry {
    name: String,
    #[serde(default)]
    hashes: std::collections::HashMap<String, String>,
}

/// SHA-256 CodeArtifact recorded for `asset`, from
/// `list-package-version-assets` JSON.
pub fn asset_sha256(json: &str, asset: &str) -> Result<String> {
    let list: AssetList = serde_json::from_str(json).context("parsing list-package-version-assets output")?;
    let entry = list
        .assets
        .into_iter()
        .find(|a| a.name == asset)
        .with_context(|| format!("asset {asset} is not part of this package version"))?;
    entry
        .hashes
        .get("SHA-256")
        .map(|h| h.to_ascii_lowercase())
        .with_context(|| format!("CodeArtifact reported no SHA-256 for {asset}"))
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub fn verify_sha256(path: &Path, expected: &str) -> Result<()> {
    let actual = sha256_file(path)?;
    ensure!(
        actual.eq_ignore_ascii_case(expected.trim()),
        "checksum mismatch for {}: expected {expected}, got {actual}",
        path.display()
    );
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
pub enum UpdatePlan {
    UpToDate { latest: semver::Version },
    Install { version: semver::Version },
}

/// A `--version` pin installs exactly that version (downgrades included);
/// otherwise only a strictly newer release installs. `force` additionally
/// reinstalls an equal version but never downgrades.
pub fn plan_update(
    current: &str,
    latest: Option<semver::Version>,
    pin: Option<&str>,
    force: bool,
    package: &str,
) -> Result<UpdatePlan> {
    if let Some(pin) = pin {
        let version = semver::Version::parse(pin.trim_start_matches('v'))
            .with_context(|| format!("--version {pin:?} is not a semver version"))?;
        return Ok(UpdatePlan::Install { version });
    }
    let Some(latest) = latest else {
        bail!("no published {package} versions found in CodeArtifact");
    };
    let current_v = semver::Version::parse(current)
        .with_context(|| format!("the running ninox version {current:?} is not semver"))?;
    if latest > current_v || (force && latest == current_v) {
        Ok(UpdatePlan::Install { version: latest })
    } else if force {
        bail!(
            "the latest published {package} ({latest}) is older than the running ninox {current}; \
             --force never downgrades — pass `--version {latest}` to install it anyway"
        )
    } else {
        Ok(UpdatePlan::UpToDate { latest })
    }
}

/// Pulls `<stem>/<bin_name>` out of the release tarball into `dest_dir` and
/// returns its path. `bin_name` is the running binary's own name
/// (`ninox-gui` or `ninox-tui` — the tarball ships both). Only that one
/// entry is extracted, and only if it is a regular file, so a hostile
/// archive can't write anywhere else or smuggle in a symlink.
pub fn extract_binary(tarball: &Path, version: &str, target_triple: &str, bin_name: &str, dest_dir: &Path) -> Result<PathBuf> {
    let wanted = Path::new(&tarball_stem(version, target_triple)).join(bin_name);
    let file = std::fs::File::open(tarball).with_context(|| format!("opening {}", tarball.display()))?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    for entry in archive.entries()? {
        let mut entry = entry?;
        if entry.path()?.as_ref() == wanted.as_path() {
            ensure!(
                entry.header().entry_type().is_file(),
                "{} in {} is not a regular file",
                wanted.display(),
                tarball.display()
            );
            let out = dest_dir.join(bin_name);
            let mut f = std::fs::File::create(&out)?;
            std::io::copy(&mut entry, &mut f)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o755))?;
            }
            return Ok(out);
        }
    }
    bail!("{} has no {} entry", tarball.display(), wanted.display())
}

pub const SMOKE_TEST_TIMEOUT: Duration = Duration::from_secs(10);

/// `--version` output must name `version` as a whole token (`ninox 0.29.0`),
/// so `0.29.0` doesn't match `0.29.01`.
pub fn check_version_output(stdout: &str, version: &str) -> Result<()> {
    ensure!(
        stdout.split_whitespace().any(|w| w.trim_start_matches('v') == version),
        "expected version {version} in --version output, got {:?}",
        stdout.trim()
    );
    Ok(())
}

/// Runs `<bin> --version` and requires it to exit 0 within `timeout`
/// reporting `version` — catches a truncated, wrong-arch or mislabelled
/// binary before it replaces a working one.
pub fn smoke_test_binary(bin: &Path, version: &str, timeout: Duration) -> Result<()> {
    use std::process::{Command, Stdio};
    let deadline = Instant::now() + timeout;
    let mut child = loop {
        let spawned = Command::new(bin)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        match spawned {
            // `bin` was just written: until every child forked while its
            // write fd was open (any thread's spawn, in this multi-threaded
            // process) has exec'd and closed that fd, Linux refuses to
            // exec it. Closing or syncing our own fd can't prevent that.
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            other => break other.with_context(|| format!("running {} --version", bin.display()))?,
        }
    };
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{} --version did not exit within {}s", bin.display(), timeout.as_secs_f32());
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let mut out = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        stdout.read_to_string(&mut out)?;
    }
    ensure!(status.success(), "{} --version exited with {status}", bin.display());
    check_version_output(&out, version).with_context(|| format!("smoke-testing {}", bin.display()))
}

/// Atomically replaces `dest` with `new_binary`: copies into a temp file in
/// `dest`'s directory (rename is only atomic within a filesystem), marks it
/// executable, runs `verify` on that exact file, then renames over. A running
/// process keeps its old inode.
pub fn replace_binary(
    new_binary: &Path,
    dest: &Path,
    verify: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    let dir = dest.parent().context("destination has no parent directory")?;
    let file_name = dest.file_name().context("destination has no file name")?.to_string_lossy();
    let tmp = dir.join(format!(".{file_name}.update-{}", std::process::id()));
    let result = (|| -> Result<()> {
        std::fs::copy(new_binary, &tmp).with_context(|| format!("writing {}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
        }
        verify(&tmp)?;
        std::fs::rename(&tmp, dest).with_context(|| format!("replacing {}", dest.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Swaps `new_app` in at `dest`, keeping the old bundle until the new one is
/// in place so a failure never leaves no app at all. Both paths must share a
/// filesystem. Returns the old bundle's path if the swap succeeded but
/// deleting it didn't — the caller warns rather than failing the update.
pub fn replace_app_bundle(new_app: &Path, dest: &Path) -> Result<Option<PathBuf>> {
    let backup = dest.with_extension(format!("app.old-{}", std::process::id()));
    let had_old = dest.exists();
    if had_old {
        std::fs::rename(dest, &backup).with_context(|| format!("moving aside {}", dest.display()))?;
    }
    if let Err(e) = std::fs::rename(new_app, dest) {
        if had_old {
            if let Err(restore) = std::fs::rename(&backup, dest) {
                return Err(e).with_context(|| {
                    format!(
                        "installing {} failed, and restoring the previous bundle failed too ({restore}); \
                         it is at {} — move it back by hand",
                        dest.display(),
                        backup.display()
                    )
                });
            }
        }
        return Err(e).with_context(|| format!("installing {} (previous bundle left in place)", dest.display()));
    }
    if had_old && std::fs::remove_dir_all(&backup).is_err() {
        return Ok(Some(backup));
    }
    Ok(None)
}

/// The `.app` bundle the binary at `exe` lives in (nearest `<X>.app` ancestor
/// with `exe` under its `Contents/`), so a bundled ninox updates its own
/// bundle rather than whichever copy sits in /Applications.
pub fn enclosing_app_bundle(exe: &Path) -> Option<PathBuf> {
    exe.ancestors()
        .skip(1)
        .find(|p| {
            p.file_name().is_some_and(|n| n.to_string_lossy().ends_with(".app"))
                && exe.starts_with(p.join("Contents"))
        })
        .map(Path::to_path_buf)
}

/// Ninox.app only ever bundles the desktop (ninox-gui) binary.
pub fn app_bundle_binary(bundle: &Path) -> PathBuf {
    bundle.join("Contents/MacOS/ninox-gui")
}

pub fn installed_by_cargo(exe: &Path, home: &Path) -> bool {
    exe.starts_with(home.join(".cargo").join("bin"))
}

/// The domain owner account from a cargo config's CodeArtifact registry
/// index URL (`sparse+https://<domain>-<owner>.d.codeartifact.<region>…`),
/// which the registry setup writes for everyone who installs ninox — so the
/// owner is found without baking an account id in, even when the SSO
/// profile is a different account.
pub fn owner_from_cargo_config(config: &str, domain: &str) -> Option<String> {
    let needle = format!("{domain}-");
    config.match_indices(&needle).find_map(|(i, _)| {
        let rest = &config[i + needle.len()..];
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        (digits.len() == 12 && rest[digits.len()..].starts_with(".d.codeartifact.")).then_some(digits)
    })
}

/// `$CARGO_HOME/config.toml` (or `config`), else `~/.cargo/config.toml`.
pub fn cargo_config_text() -> Option<String> {
    let home = std::env::var_os("CARGO_HOME").map(std::path::PathBuf::from).or_else(|| dirs::home_dir().map(|h| h.join(".cargo")))?;
    ["config.toml", "config"].iter().find_map(|f| std::fs::read_to_string(home.join(f)).ok())
}

/// Runs `aws <args>`, surfacing a clear hint when the failure looks like an
/// expired/missing AWS session rather than dumping raw CLI stderr. Shared by
/// `ninox update` and the `machine add` cross-arch install path (`remote.rs`)
/// so both resolve CodeArtifact the same way instead of each shelling out to
/// `aws` with their own error handling.
pub fn run_aws(args: &[String]) -> Result<String> {
    let output = Command::new("aws").args(args).env("AWS_PAGER", "").output().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => anyhow::anyhow!("{AWS_MISSING_MARKER} (`brew install awscli`)"),
        _ => anyhow::Error::new(e).context("running aws"),
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let auth = ["sso", "token", "expired", "credentials", "ExpiredToken", "UnrecognizedClient"]
            .iter()
            .any(|needle| stderr.contains(needle));
        let sub = args.iter().take(2).cloned().collect::<Vec<_>>().join(" ");
        if auth {
            bail!(
                "aws {sub} failed — {SESSION_ERROR_MARKER}; run `aws sso login` (set AWS_PROFILE / --profile \
                 to the account that owns the CodeArtifact domain) and retry\n{}",
                stderr.trim()
            );
        }
        bail!("aws {sub} failed: {}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Substring of the `run_aws` error that means the caller's AWS/SSO session
/// is missing or expired, as opposed to a genuine failure.
/// Prefix of the `run_aws` error for a missing `aws` binary.
pub const AWS_MISSING_MARKER: &str = "the `aws` CLI is required";

const SESSION_ERROR_MARKER: &str = "no valid AWS session";

/// Whether `e` (from [`run_aws`]) means "run `aws sso login`" rather than a
/// real error — the daemon reports that as a prompt, not a failure.
pub fn is_session_error(e: &anyhow::Error) -> bool {
    e.to_string().contains(SESSION_ERROR_MARKER)
}

/// The CodeArtifact domain-owner account to query: `coords`' own override,
/// else whatever `cargo_config_text` already points at (no `aws` call
/// needed), else the caller's own AWS account via `sts get-caller-identity`.
pub fn resolve_owner(coords: &CodeArtifactCoords) -> Result<String> {
    if let Some(owner) = &coords.domain_owner {
        return Ok(owner.clone());
    }
    if let Some(owner) = cargo_config_text().and_then(|c| owner_from_cargo_config(&c, &coords.domain)) {
        return Ok(owner);
    }
    let account = run_aws(&["sts", "get-caller-identity", "--query", "Account", "--output", "text"].map(String::from))?;
    Ok(account.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coords() -> CodeArtifactCoords {
        CodeArtifactCoords::from_env_with(|_| None)
    }

    #[test]
    fn target_triple_maps_each_supported_platform() {
        assert_eq!(target_triple("macos", "aarch64"), Some("aarch64-apple-darwin"));
        assert_eq!(target_triple("linux", "x86_64"), Some("x86_64-unknown-linux-gnu"));
        assert_eq!(target_triple("linux", "aarch64"), Some("aarch64-unknown-linux-gnu"));
        assert_eq!(target_triple("macos", "x86_64"), None);
        assert_eq!(target_triple("linux", "arm"), None);
        assert_eq!(target_triple("windows", "x86_64"), None);
    }

    #[test]
    fn generic_package_maps_each_supported_os() {
        use UpdateChannel::{Alpha, Stable};
        assert_eq!(generic_package("macos", Stable), Some("ninox-macos"));
        assert_eq!(generic_package("linux", Stable), Some("ninox-linux"));
        assert_eq!(generic_package("macos", Alpha), Some("ninox-macos-alpha"));
        assert_eq!(generic_package("linux", Alpha), Some("ninox-linux-alpha"));
        assert_eq!(generic_package("windows", Stable), None);
        assert_eq!(generic_package("windows", Alpha), None);
    }

    #[test]
    fn platform_supported_covers_macos_and_both_linux_arches() {
        assert!(platform_supported("macos", "aarch64"));
        assert!(platform_supported("linux", "x86_64"));
        assert!(platform_supported("linux", "aarch64"));
        assert!(!platform_supported("macos", "x86_64"));
        assert!(!platform_supported("linux", "arm"));
    }

    #[test]
    fn tarball_naming_is_keyed_by_target_triple() {
        assert_eq!(tarball_stem("0.29.0", "aarch64-apple-darwin"), "ninox-0.29.0-aarch64-apple-darwin");
        assert_eq!(tarball_asset("0.29.0", "aarch64-apple-darwin"), "ninox-0.29.0-aarch64-apple-darwin.tar.gz");
        assert_eq!(tarball_stem("0.29.0", "x86_64-unknown-linux-gnu"), "ninox-0.29.0-x86_64-unknown-linux-gnu");
        assert_eq!(tarball_asset("0.29.0", "aarch64-unknown-linux-gnu"), "ninox-0.29.0-aarch64-unknown-linux-gnu.tar.gz");
    }

    #[test]
    fn coords_default_and_override_from_env() {
        let c = coords();
        assert_eq!(c.domain, "synthesia-build");
        assert_eq!(c.repository, "synthesia-cargo");
        assert_eq!(c.region, "eu-west-1");
        assert_eq!(c.domain_owner, None);

        let c = CodeArtifactCoords::from_env_with(|k| match k {
            "NINOX_CODEARTIFACT_DOMAIN" => Some("d".into()),
            "NINOX_CODEARTIFACT_DOMAIN_OWNER" => Some("123".into()),
            "NINOX_CODEARTIFACT_REPOSITORY" => Some("".into()),
            _ => None,
        });
        assert_eq!(c.domain, "d");
        assert_eq!(c.domain_owner.as_deref(), Some("123"));
        assert_eq!(c.repository, "synthesia-cargo");
    }

    #[test]
    fn domain_owner_comes_from_the_cargo_registry_url() {
        let cfg = "[registries.x]\nindex = \"sparse+https://acme-build-123456789012.d.codeartifact.eu-west-1.amazonaws.com/cargo/r/\"\n";
        assert_eq!(owner_from_cargo_config(cfg, "acme-build").as_deref(), Some("123456789012"));
        assert_eq!(owner_from_cargo_config(cfg, "other"), None);
        assert_eq!(owner_from_cargo_config("acme-build-12345.d.codeartifact.x", "acme-build"), None, "not an account id");
    }

    #[test]
    fn aws_args_target_the_generic_package() {
        let args = coords().get_asset_args("123", "ninox-macos", "0.29.0", "Ninox.app.zip", Path::new("/tmp/out"));
        let joined = args.join(" ");
        assert!(joined.starts_with("codeartifact get-package-version-asset --domain synthesia-build --domain-owner 123"));
        assert!(joined.contains("--format generic --namespace ninox --package ninox-macos"));
        assert!(joined.contains("--package-version 0.29.0 --asset Ninox.app.zip"));
        assert_eq!(args.last().unwrap(), "/tmp/out");

        let linux_args = coords().get_asset_args("123", "ninox-linux", "0.29.0", "ninox-0.29.0-x86_64-unknown-linux-gnu.tar.gz", Path::new("/tmp/out"));
        assert!(linux_args.join(" ").contains("--format generic --namespace ninox --package ninox-linux"));

        let list = coords().list_versions_args("123", "ninox-macos").join(" ");
        assert!(list.contains("list-package-versions"));
        assert!(list.contains("--package ninox-macos"));
        assert!(list.contains("--status Published"));
    }

    #[test]
    fn latest_published_version_uses_semver_not_order() {
        let json = r#"{"versions":[
            {"version":"0.28.10","status":"Published"},
            {"version":"0.29.0","status":"Unfinished"},
            {"version":"0.28.9","status":"Published"},
            {"version":"garbage","status":"Published"}
        ]}"#;
        assert_eq!(latest_published_version(json, UpdateChannel::Stable).unwrap(), Some(semver::Version::new(0, 28, 10)));
        assert_eq!(latest_published_version(r#"{"versions":[]}"#, UpdateChannel::Stable).unwrap(), None);
        assert!(latest_published_version("not json", UpdateChannel::Stable).is_err());
    }

    #[test]
    fn latest_published_version_skips_prereleases() {
        let json = r#"{"versions":[
            {"version":"0.29.0-rc.1","status":"Published"},
            {"version":"0.28.10","status":"Published"}
        ]}"#;
        assert_eq!(latest_published_version(json, UpdateChannel::Stable).unwrap(), Some(semver::Version::new(0, 28, 10)));
        let only_pre = r#"{"versions":[{"version":"0.29.0-rc.1","status":"Published"}]}"#;
        assert_eq!(latest_published_version(only_pre, UpdateChannel::Stable).unwrap(), None);
    }

    #[test]
    fn alpha_channel_picks_the_newest_prerelease_by_semver_order() {
        let json = r#"{"versions":[
            {"version":"1.0.0-alpha.9","status":"Published"},
            {"version":"1.0.0-alpha.10","status":"Published"},
            {"version":"1.0.0-alpha.2","status":"Published"},
            {"version":"1.0.0-alpha.11","status":"Unfinished"}]}"#;
        assert_eq!(
            latest_published_version(json, UpdateChannel::Alpha).unwrap(),
            Some(semver::Version::parse("1.0.0-alpha.10").unwrap()),
            "alpha.10 outranks alpha.9 (a string sort would not); Unfinished versions are ignored"
        );
        assert_eq!(latest_published_version(json, UpdateChannel::Stable).unwrap(), None);
    }

    #[test]
    fn alphas_sort_between_the_last_stable_and_the_next_stable() {
        let v = |s: &str| semver::Version::parse(s).unwrap();
        assert!(v("1.0.0-alpha.1") > v("0.37.0"), "an installed 0.37.0 sees the first alpha as newer");
        assert!(v("1.0.0") > v("1.0.0-alpha.99"), "switching back to a stable 1.0.0 is an upgrade");
    }

    #[test]
    fn session_errors_are_distinguished_from_real_failures() {
        let sso = anyhow::anyhow!("aws codeartifact list-package-versions failed — {SESSION_ERROR_MARKER}; run `aws sso login`");
        assert!(is_session_error(&sso));
        assert!(!is_session_error(&anyhow::anyhow!("aws codeartifact failed: AccessDenied")));
    }

    #[test]
    fn asset_sha256_finds_the_named_asset() {
        let json = r#"{"assets":[
            {"name":"Ninox.app.zip","hashes":{"SHA-256":"ABCDEF","MD5":"x"}},
            {"name":"other","hashes":{}}
        ]}"#;
        assert_eq!(asset_sha256(json, "Ninox.app.zip").unwrap(), "abcdef");
        assert!(asset_sha256(json, "other").is_err());
        assert!(asset_sha256(json, "missing").is_err());
    }

    #[test]
    fn checksum_verification_on_a_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, b"hello").unwrap();
        let hello = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert_eq!(sha256_file(&path).unwrap(), hello);
        verify_sha256(&path, &hello.to_uppercase()).unwrap();
        assert!(verify_sha256(&path, "00").is_err());
    }

    #[test]
    fn plan_update_decisions() {
        let v = |s| semver::Version::parse(s).unwrap();
        assert_eq!(
            plan_update("0.28.1", Some(v("0.29.0")), None, false, "ninox-macos").unwrap(),
            UpdatePlan::Install { version: v("0.29.0") }
        );
        assert_eq!(
            plan_update("0.28.1", Some(v("0.28.1")), None, false, "ninox-macos").unwrap(),
            UpdatePlan::UpToDate { latest: v("0.28.1") }
        );
        assert_eq!(
            plan_update("0.28.1", Some(v("0.28.1")), None, true, "ninox-macos").unwrap(),
            UpdatePlan::Install { version: v("0.28.1") }
        );
        assert_eq!(
            plan_update("0.28.1", None, Some("v0.27.0"), false, "ninox-macos").unwrap(),
            UpdatePlan::Install { version: v("0.27.0") }
        );
        assert!(plan_update("0.28.1", None, None, false, "ninox-macos").is_err());
        assert!(plan_update("0.28.1", None, Some("nope"), false, "ninox-macos").is_err());
        assert_eq!(
            plan_update("0.28.1", None, Some("0.30.0-rc.1"), false, "ninox-macos").unwrap(),
            UpdatePlan::Install { version: v("0.30.0-rc.1") }
        );
    }

    #[test]
    fn force_never_downgrades() {
        let v = |s| semver::Version::parse(s).unwrap();
        assert_eq!(
            plan_update("0.29.0", Some(v("0.28.1")), None, false, "ninox-macos").unwrap(),
            UpdatePlan::UpToDate { latest: v("0.28.1") }
        );
        let err = plan_update("0.29.0", Some(v("0.28.1")), None, true, "ninox-macos").unwrap_err().to_string();
        assert!(err.contains("--version 0.28.1"), "{err}");
        assert_eq!(
            plan_update("0.29.0", Some(v("0.29.1")), None, true, "ninox-macos").unwrap(),
            UpdatePlan::Install { version: v("0.29.1") }
        );
        assert_eq!(
            plan_update("0.29.0-rc.1", Some(v("0.29.0")), None, false, "ninox-macos").unwrap(),
            UpdatePlan::Install { version: v("0.29.0") }
        );
    }

    fn write_tarball(path: &Path, entry: &str, body: &[u8]) {
        let file = std::fs::File::create(path).unwrap();
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(file, flate2::Compression::fast()));
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append_data(&mut header, entry, body).unwrap();
        builder.into_inner().unwrap().finish().unwrap();
    }

    #[test]
    fn extract_binary_takes_only_the_named_entry() {
        let dir = tempfile::tempdir().unwrap();
        let tarball = dir.path().join("t.tar.gz");
        write_tarball(&tarball, "ninox-0.29.0-aarch64-apple-darwin/ninox-tui", b"bin");
        let out = extract_binary(&tarball, "0.29.0", "aarch64-apple-darwin", "ninox-tui", dir.path()).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"bin");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&out).unwrap().permissions().mode() & 0o777, 0o755);
        assert!(extract_binary(&tarball, "0.30.0", "aarch64-apple-darwin", "ninox-tui", dir.path()).is_err());
        assert!(extract_binary(&tarball, "0.29.0", "aarch64-apple-darwin", "ninox-gui", dir.path()).is_err());
    }

    #[test]
    fn extract_binary_rejects_a_symlink_entry() {
        let dir = tempfile::tempdir().unwrap();
        let tarball = dir.path().join("t.tar.gz");
        let file = std::fs::File::create(&tarball).unwrap();
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(file, flate2::Compression::fast()));
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        builder
            .append_link(&mut header, "ninox-0.29.0-aarch64-apple-darwin/ninox-gui", "/bin/sh")
            .unwrap();
        builder.into_inner().unwrap().finish().unwrap();
        let err = extract_binary(&tarball, "0.29.0", "aarch64-apple-darwin", "ninox-gui", dir.path()).unwrap_err().to_string();
        assert!(err.contains("not a regular file"), "{err}");
        assert!(!dir.path().join("ninox-gui").exists());
    }

    #[test]
    fn version_output_must_name_the_target_version() {
        check_version_output("ninox 0.29.0\n", "0.29.0").unwrap();
        check_version_output("ninox v0.29.0", "0.29.0").unwrap();
        assert!(check_version_output("ninox 0.28.1\n", "0.29.0").is_err());
        assert!(check_version_output("ninox 0.29.01", "0.29.0").is_err());
        assert!(check_version_output("", "0.29.0").is_err());
    }

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn smoke_test_runs_the_binary() {
        let dir = tempfile::tempdir().unwrap();
        let ok = script(dir.path(), "ok", r#"[ "$1" = --version ] && echo "ninox 0.29.0""#);
        smoke_test_binary(&ok, "0.29.0", SMOKE_TEST_TIMEOUT).unwrap();
        assert!(smoke_test_binary(&ok, "0.30.0", SMOKE_TEST_TIMEOUT).is_err());
        let fails = script(dir.path(), "fails", "echo 'ninox 0.29.0'; exit 3");
        assert!(smoke_test_binary(&fails, "0.29.0", SMOKE_TEST_TIMEOUT).is_err());
        let hangs = script(dir.path(), "hangs", "exec sleep 30");
        let err = smoke_test_binary(&hangs, "0.29.0", Duration::from_millis(200)).unwrap_err().to_string();
        assert!(err.contains("did not exit"), "{err}");
        let garbage = dir.path().join("garbage");
        std::fs::write(&garbage, b"\0\0\0\0").unwrap();
        assert!(smoke_test_binary(&garbage, "0.29.0", SMOKE_TEST_TIMEOUT).is_err());
    }

    #[test]
    fn replace_binary_swaps_contents_and_sets_exec_bit() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let new = dir.path().join("new");
        let dest = dir.path().join("ninox");
        std::fs::write(&new, b"new").unwrap();
        std::fs::write(&dest, b"old").unwrap();
        let mut verified = None;
        replace_binary(&new, &dest, |tmp| {
            verified = Some(std::fs::read(tmp)?);
            Ok(())
        })
        .unwrap();
        assert_eq!(verified.as_deref(), Some(&b"new"[..]), "verify sees the staged copy");
        assert_eq!(std::fs::read(&dest).unwrap(), b"new");
        assert_eq!(std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777, 0o755);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2, "no temp file left behind");
    }

    /// The ptyd host runs from the installed binary; rewriting that file in
    /// place can kill every agent, so the update must land as a new inode.
    #[cfg(unix)]
    #[test]
    fn replace_binary_installs_a_new_inode_and_leaves_a_running_file_untouched() {
        use std::io::Read;
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("ninox");
        let new = dir.path().join("new");
        std::fs::write(&dest, b"old binary").unwrap();
        std::fs::write(&new, b"new binary").unwrap();
        let old_inode = std::fs::metadata(&dest).unwrap().ino();
        let mut running = std::fs::File::open(&dest).unwrap();

        replace_binary(&new, &dest, |_| Ok(())).unwrap();

        assert_ne!(std::fs::metadata(&dest).unwrap().ino(), old_inode, "must rename a new file over, not rewrite in place");
        assert_eq!(std::fs::read(&dest).unwrap(), b"new binary");
        let mut still_old = Vec::new();
        running.read_to_end(&mut still_old).unwrap();
        assert_eq!(still_old, b"old binary", "a process holding the old file keeps its bytes");
    }

    #[test]
    fn replace_binary_keeps_the_old_one_when_verify_fails() {
        let dir = tempfile::tempdir().unwrap();
        let new = dir.path().join("new");
        let dest = dir.path().join("ninox");
        std::fs::write(&new, b"new").unwrap();
        std::fs::write(&dest, b"old").unwrap();
        assert!(replace_binary(&new, &dest, |_| anyhow::bail!("bad")).is_err());
        assert_eq!(std::fs::read(&dest).unwrap(), b"old");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2, "no temp file left behind");
    }

    #[test]
    fn replace_app_bundle_swaps_directories() {
        let dir = tempfile::tempdir().unwrap();
        let new = dir.path().join("staging/Ninox.app");
        let dest = dir.path().join("Ninox.app");
        std::fs::create_dir_all(new.join("Contents")).unwrap();
        std::fs::write(new.join("Contents/marker"), b"new").unwrap();
        std::fs::create_dir_all(dest.join("Contents")).unwrap();
        std::fs::write(dest.join("Contents/marker"), b"old").unwrap();
        assert_eq!(replace_app_bundle(&new, &dest).unwrap(), None);
        assert_eq!(std::fs::read(dest.join("Contents/marker")).unwrap(), b"new");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path()).unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".old-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn replace_app_bundle_restores_the_old_bundle_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("Ninox.app");
        std::fs::create_dir_all(dest.join("Contents")).unwrap();
        std::fs::write(dest.join("Contents/marker"), b"old").unwrap();
        assert!(replace_app_bundle(&dir.path().join("missing/Ninox.app"), &dest).is_err());
        assert_eq!(std::fs::read(dest.join("Contents/marker")).unwrap(), b"old");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "no backup left behind");
    }

    #[test]
    fn enclosing_app_bundle_is_the_nearest_bundle() {
        assert_eq!(
            enclosing_app_bundle(Path::new("/Users/u/Applications/Ninox.app/Contents/MacOS/ninox")),
            Some(PathBuf::from("/Users/u/Applications/Ninox.app"))
        );
        assert_eq!(
            enclosing_app_bundle(Path::new("/x/Outer.app/Contents/Resources/Ninox.app/Contents/MacOS/ninox")),
            Some(PathBuf::from("/x/Outer.app/Contents/Resources/Ninox.app"))
        );
        assert_eq!(enclosing_app_bundle(Path::new("/Users/u/.local/bin/ninox")), None);
        assert_eq!(enclosing_app_bundle(Path::new("/x/Ninox.app/ninox")), None);
        assert_eq!(
            app_bundle_binary(Path::new("/Applications/Ninox.app")),
            PathBuf::from("/Applications/Ninox.app/Contents/MacOS/ninox-gui")
        );
    }

    #[test]
    fn cargo_install_detection() {
        let home = Path::new("/Users/u");
        assert!(installed_by_cargo(Path::new("/Users/u/.cargo/bin/ninox"), home));
        assert!(!installed_by_cargo(Path::new("/Users/u/.local/bin/ninox"), home));
    }
}
