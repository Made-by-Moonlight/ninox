//! Self-update: checks the private Cargo registry ninox is already
//! distributed through for a newer version, and (on user action) applies it
//! via `cargo install --force`.
//!
//! ## Why the Cargo registry, not GitHub Releases
//!
//! Two distribution channels exist (see the repo's `.github/workflows/`):
//! `github.com/Made-by-Moonlight/ninox` tags commits but its `release.yml`
//! workflow is disabled and publishes no binaries/bundles anywhere — there
//! is nothing there to check against or download. The private mirror
//! (`Synthesia-Technologies/ninox`) publishes every version bump to a
//! private Cargo registry (`synthesia-cargo`, on AWS CodeArtifact) via
//! `publish-codeartifact.yml`, and `cargo install ninox` against that
//! registry is already the primary install path (the registry is the
//! default via `~/.cargo/config.toml` source replacement — see
//! `.github/workflows/publish-codeartifact.yml`'s header comment for the
//! exact setup). That registry's sparse index is therefore both the
//! authoritative "what's the latest version" source AND requires no new
//! infrastructure: this module reads the same `~/.cargo/config.toml` cargo
//! itself uses and mints tokens via the same
//! `cargo:token-from-stdout` credential provider CI configures there.
//!
//! If ninox ever starts publishing binaries to GitHub Releases, swap
//! `CargoRegistryUpdateSource` for a GitHub-releases-backed `UpdateSource` —
//! nothing else in this module (or its callers) needs to change.

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// A resolved Cargo registry: where its sparse index lives, and how to
/// authenticate against it (`None` for a registry that needs no auth).
#[derive(Debug, Clone, PartialEq)]
pub struct RegistrySource {
    /// Sparse index base URL, `sparse+` prefix already stripped, always
    /// ending in `/`.
    pub index_url: String,
    pub credential_provider: Option<String>,
}

/// Reads `{cargo_home}/config.toml` (falling back to the extensionless
/// `config`, cargo's older name for the same file) and follows
/// `[source.crates-io] replace-with` to the registry ninox is actually
/// installed from. Returns `None` — not an error — whenever any piece of
/// that chain is missing: no config file, no source replacement, or a
/// referenced registry with no `index`. Any of those just means "this
/// machine isn't set up to resolve ninox's registry", and the caller should
/// skip the check rather than fail loudly.
pub fn resolve_registry_source(cargo_home: &Path) -> Option<RegistrySource> {
    let text = std::fs::read_to_string(cargo_home.join("config.toml"))
        .or_else(|_| std::fs::read_to_string(cargo_home.join("config")))
        .ok()?;
    let value: toml::Value = text.parse().ok()?;

    let registry_name = value
        .get("source")?
        .get("crates-io")?
        .get("replace-with")?
        .as_str()?;
    let registry = value.get("registries")?.get(registry_name)?;
    let index = registry.get("index")?.as_str()?;
    // Only the sparse HTTP protocol is supported — a git-based index (no
    // `sparse+` prefix) would need a full git checkout to read, which this
    // module doesn't do. Treat it the same as "nothing configured" rather
    // than sending a bogus HTTP request to a git URL.
    let index_url = format!("{}/", index.strip_prefix("sparse+")?.trim_end_matches('/'));
    let credential_provider = registry
        .get("credential-provider")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    Some(RegistrySource { index_url, credential_provider })
}

/// Cargo's sparse-index path scheme for a package name: `>=4` chars uses
/// the first two / next two / full-name layout; shorter names get their own
/// documented special cases. Names are lowercased — the index is
/// case-insensitive but stored lowercase.
pub fn sparse_index_path(name: &str) -> String {
    let name = name.to_lowercase();
    match name.len() {
        1 => format!("1/{name}"),
        2 => format!("2/{name}"),
        3 => format!("3/{}/{name}", &name[..1]),
        _ => format!("{}/{}/{name}", &name[..2], &name[2..4]),
    }
}

#[derive(Deserialize)]
struct IndexEntry {
    vers: String,
    #[serde(default)]
    yanked: bool,
}

/// Parses a sparse-index response body (newline-delimited JSON, one line
/// per published version) and returns the highest non-yanked semver
/// version, if any. Malformed lines are skipped rather than failing the
/// whole parse — the index is append-only and any one line being odd
/// shouldn't blind the check to every other (valid) line.
pub fn parse_latest_version(body: &str) -> Option<semver::Version> {
    body.lines()
        .filter_map(|line| serde_json::from_str::<IndexEntry>(line).ok())
        .filter(|entry| !entry.yanked)
        .filter_map(|entry| semver::Version::parse(&entry.vers).ok())
        .max()
}

/// Whether `latest` is newer than `current_version`. `current_version`
/// failing to parse (should never happen — it's `env!("CARGO_PKG_VERSION")`
/// — but a corrupted build could do it) is treated as "no update", not an
/// error: better to silently skip a notification than nag on bad data.
pub fn is_newer(current_version: &str, latest: &semver::Version) -> bool {
    match semver::Version::parse(current_version) {
        Ok(current) => *latest > current,
        Err(e) => {
            tracing::warn!("update check: couldn't parse current version {current_version:?}: {e}");
            false
        }
    }
}

/// Runs a credential-provider command and returns its stdout as the bearer
/// token, mirroring cargo's own `cargo:token-from-stdout` protocol (the
/// only provider scheme this supports — see the module doc for why that's
/// the one actually configured). Splits the command on whitespace, which
/// is fine for the documented AWS CLI invocation but wouldn't survive a
/// quoted argument; not needed for what's actually configured today.
async fn mint_token(credential_provider: &str) -> Result<Option<String>> {
    let Some(rest) = credential_provider.strip_prefix("cargo:token-from-stdout ") else {
        tracing::warn!("update check: unsupported credential-provider scheme: {credential_provider}");
        return Ok(None);
    };
    let mut parts = rest.split_whitespace();
    let program = parts.next().context("empty credential-provider command")?;
    let output = tokio::process::Command::new(program)
        .args(parts)
        .output()
        .await
        .with_context(|| format!("running credential-provider `{credential_provider}`"))?;
    if !output.status.success() {
        anyhow::bail!(
            "credential-provider exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim(),
        );
    }
    Ok(Some(String::from_utf8_lossy(&output.stdout).trim().to_string()))
}

/// Source of "what's the latest published version of `package`" — real
/// production code uses [`CargoRegistryUpdateSource`]; tests inject a fake
/// so they never make a network call.
#[async_trait]
pub trait UpdateSource: Send + Sync {
    async fn latest_version(&self, package: &str) -> Result<Option<semver::Version>>;
}

/// Queries the Cargo registry resolved from `~/.cargo/config.toml` (or
/// `$CARGO_HOME/config.toml`) — see the module doc for why this, not
/// GitHub Releases. `Ok(None)` (not an error) when this machine has no
/// registry source-replacement configured at all.
pub struct CargoRegistryUpdateSource;

fn cargo_home() -> PathBuf {
    std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".cargo")))
        .unwrap_or_else(|| PathBuf::from(".cargo"))
}

#[async_trait]
impl UpdateSource for CargoRegistryUpdateSource {
    async fn latest_version(&self, package: &str) -> Result<Option<semver::Version>> {
        let Some(source) = resolve_registry_source(&cargo_home()) else {
            return Ok(None);
        };
        let token = match &source.credential_provider {
            Some(provider) => mint_token(provider).await?,
            None => None,
        };

        let url = format!("{}{}", source.index_url, sparse_index_path(package));
        let mut request = reqwest::Client::new().get(&url);
        if let Some(token) = token {
            // Raw token, no "Bearer " prefix — CodeArtifact's cargo data
            // plane rejects a prefixed token with 401.
            request = request.header(reqwest::header::AUTHORIZATION, token);
        }
        let response = request.send().await.context("fetching sparse index")?;
        if !response.status().is_success() {
            anyhow::bail!("sparse index request for {package} failed: {}", response.status());
        }
        let body = response.text().await.context("reading sparse index response")?;
        Ok(parse_latest_version(&body))
    }
}

/// Checks `source` for a newer version of `package` than `current_version`.
/// Returns `Ok(None)` both when there's nothing to check against (no
/// registry configured) and when the latest published version isn't newer
/// than what's already running.
pub async fn check_for_update(
    source: &dyn UpdateSource,
    package: &str,
    current_version: &str,
) -> Result<Option<semver::Version>> {
    let Some(latest) = source.latest_version(package).await? else {
        return Ok(None);
    };
    Ok(is_newer(current_version, &latest).then_some(latest))
}

/// Applies an update — production code uses [`CargoInstallInstaller`];
/// tests inject a fake so they never spawn a real `cargo install`.
#[async_trait]
pub trait UpdateInstaller: Send + Sync {
    async fn install(&self, package: &str) -> Result<()>;
}

/// Re-runs `cargo install <package> --force --locked` — the simplest
/// reliable update path given the app is already distributed via `cargo
/// install`. `--locked` matches the already-published `Cargo.lock` exactly
/// rather than re-resolving dependencies; `--force` overwrites the existing
/// `~/.cargo/bin/<package>` binary.
pub struct CargoInstallInstaller;

#[async_trait]
impl UpdateInstaller for CargoInstallInstaller {
    async fn install(&self, package: &str) -> Result<()> {
        let output = tokio::process::Command::new("cargo")
            .args(["install", package, "--force", "--locked"])
            .output()
            .await
            .context("spawning cargo install")?;
        if !output.status.success() {
            anyhow::bail!(
                "cargo install {package} exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim(),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_cargo_config(dir: &Path, contents: &str) {
        std::fs::write(dir.join("config.toml"), contents).unwrap();
    }

    #[test]
    fn resolve_registry_source_reads_replace_with_chain() {
        let dir = tempfile::tempdir().unwrap();
        write_cargo_config(dir.path(), r#"
[registries.synthesia-cargo]
index = "sparse+https://example.com/cargo/synthesia-cargo/"
credential-provider = "cargo:token-from-stdout aws codeartifact get-authorization-token --domain d --domain-owner o --region r --query authorizationToken --output text"

[source.crates-io]
replace-with = "synthesia-cargo"
"#);
        let source = resolve_registry_source(dir.path()).expect("must resolve");
        assert_eq!(source.index_url, "https://example.com/cargo/synthesia-cargo/");
        assert_eq!(
            source.credential_provider.as_deref(),
            Some("cargo:token-from-stdout aws codeartifact get-authorization-token --domain d --domain-owner o --region r --query authorizationToken --output text"),
        );
    }

    #[test]
    fn resolve_registry_source_none_without_replace_with() {
        let dir = tempfile::tempdir().unwrap();
        write_cargo_config(dir.path(), r#"
[registries.synthesia-cargo]
index = "sparse+https://example.com/cargo/synthesia-cargo/"
"#);
        assert!(resolve_registry_source(dir.path()).is_none());
    }

    #[test]
    fn resolve_registry_source_none_when_config_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(resolve_registry_source(dir.path()).is_none());
    }

    #[test]
    fn resolve_registry_source_none_when_registry_has_no_index() {
        let dir = tempfile::tempdir().unwrap();
        write_cargo_config(dir.path(), r#"
[registries.synthesia-cargo]

[source.crates-io]
replace-with = "synthesia-cargo"
"#);
        assert!(resolve_registry_source(dir.path()).is_none());
    }

    #[test]
    fn resolve_registry_source_none_for_a_non_sparse_index() {
        let dir = tempfile::tempdir().unwrap();
        write_cargo_config(dir.path(), r#"
[registries.git-registry]
index = "https://example.com/git-index.git"

[source.crates-io]
replace-with = "git-registry"
"#);
        assert!(resolve_registry_source(dir.path()).is_none());
    }

    #[test]
    fn resolve_registry_source_credential_provider_optional() {
        let dir = tempfile::tempdir().unwrap();
        write_cargo_config(dir.path(), r#"
[registries.public-mirror]
index = "sparse+https://example.com/cargo/public-mirror/"

[source.crates-io]
replace-with = "public-mirror"
"#);
        let source = resolve_registry_source(dir.path()).expect("must resolve");
        assert_eq!(source.credential_provider, None);
    }

    #[test]
    fn sparse_index_path_matches_cargo_scheme() {
        assert_eq!(sparse_index_path("a"), "1/a");
        assert_eq!(sparse_index_path("ab"), "2/ab");
        assert_eq!(sparse_index_path("abc"), "3/a/abc");
        assert_eq!(sparse_index_path("ninox"), "ni/no/ninox");
        assert_eq!(sparse_index_path("Ninox"), "ni/no/ninox");
    }

    #[test]
    fn parse_latest_version_picks_highest_unyanked_semver() {
        let body = "\
{\"vers\":\"0.9.0\",\"yanked\":false}
{\"vers\":\"0.13.0\",\"yanked\":false}
{\"vers\":\"0.14.0\",\"yanked\":true}
{\"vers\":\"0.10.0\",\"yanked\":false}
";
        assert_eq!(parse_latest_version(body), Some(semver::Version::new(0, 13, 0)));
    }

    #[test]
    fn parse_latest_version_skips_malformed_lines() {
        let body = "not json\n{\"vers\":\"1.0.0\",\"yanked\":false}\n{\"vers\":\"not-semver\",\"yanked\":false}\n";
        assert_eq!(parse_latest_version(body), Some(semver::Version::new(1, 0, 0)));
    }

    #[test]
    fn parse_latest_version_none_when_everything_yanked_or_empty() {
        assert_eq!(parse_latest_version(""), None);
        assert_eq!(parse_latest_version("{\"vers\":\"1.0.0\",\"yanked\":true}\n"), None);
    }

    #[test]
    fn is_newer_true_when_latest_greater() {
        assert!(is_newer("0.13.0", &semver::Version::new(0, 14, 0)));
    }

    #[test]
    fn is_newer_false_when_equal_or_behind() {
        assert!(!is_newer("0.13.0", &semver::Version::new(0, 13, 0)));
        assert!(!is_newer("0.14.0", &semver::Version::new(0, 13, 0)));
    }

    #[test]
    fn is_newer_false_when_current_unparsable() {
        assert!(!is_newer("not-a-version", &semver::Version::new(0, 13, 0)));
    }

    struct FakeSource(Option<semver::Version>);

    #[async_trait]
    impl UpdateSource for FakeSource {
        async fn latest_version(&self, _package: &str) -> Result<Option<semver::Version>> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn check_for_update_returns_newer_version() {
        let source = FakeSource(Some(semver::Version::new(0, 14, 0)));
        let result = check_for_update(&source, "ninox", "0.13.0").await.unwrap();
        assert_eq!(result, Some(semver::Version::new(0, 14, 0)));
    }

    #[tokio::test]
    async fn check_for_update_none_when_already_current() {
        let source = FakeSource(Some(semver::Version::new(0, 13, 0)));
        let result = check_for_update(&source, "ninox", "0.13.0").await.unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn check_for_update_none_when_source_has_nothing() {
        let source = FakeSource(None);
        let result = check_for_update(&source, "ninox", "0.13.0").await.unwrap();
        assert_eq!(result, None);
    }
}
