//! `ninox update` — replace this binary (and optionally Ninox.app) with the
//! prebuilt release from CodeArtifact. The pure parts live in
//! `ninox_core::lifecycle::binary_update`; this file runs `aws` and touches
//! the filesystem.

use anyhow::{bail, Context, Result};
use ninox_core::lifecycle::binary_update::{
    self as bu, CodeArtifactCoords, UpdatePlan, APP_ZIP_ASSET,
};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct UpdateArgs {
    pub check: bool,
    pub force: bool,
    pub version: Option<String>,
    pub app: bool,
}

const LOGIN_HINT: &str = "run `aws sso login` (set AWS_PROFILE / --profile to the account that owns the CodeArtifact domain) and retry";

fn owner_hint(coords: &CodeArtifactCoords, owner: &str) -> String {
    format!(
        "looked in {}/{} ({}) owned by account {owner}; if your SSO profile is a different AWS account \
         than the CodeArtifact domain owner, set NINOX_CODEARTIFACT_DOMAIN_OWNER=<owner account id> or switch AWS_PROFILE",
        coords.domain, coords.repository, coords.region
    )
}

fn is_not_found(e: &anyhow::Error) -> bool {
    e.to_string().contains("ResourceNotFoundException")
}

/// What this run replaces: the plain binary at `exe`, and/or a Ninox.app.
struct Targets {
    binary: Option<PathBuf>,
    app: Option<PathBuf>,
}

fn resolve_targets(exe: &Path, want_app: bool) -> Result<Targets> {
    if let Some(bundle) = bu::enclosing_app_bundle(exe) {
        println!(
            "running from inside {} — updating that bundle (not a separate binary)",
            bundle.display()
        );
        return Ok(Targets { binary: None, app: Some(bundle) });
    }
    let app = if want_app {
        let Some(found) = installed_app() else {
            bail!(
                "--app: no Ninox.app in /Applications or ~/Applications to update — install it with \
                 `scripts/install-macos.sh --app`, or drop --app to update only {}",
                exe.display()
            );
        };
        println!("--app: updating {} (checked /Applications, then ~/Applications)", found.display());
        Some(found)
    } else {
        None
    };
    Ok(Targets { binary: Some(exe.to_path_buf()), app })
}

pub fn run(args: UpdateArgs) -> Result<()> {
    if !bu::platform_supported(std::env::consts::OS, std::env::consts::ARCH) {
        bail!(
            "prebuilt ninox binaries are only published for Apple silicon macOS ({}); \
             update from source with `cargo install --force ninox` \
             (or `cargo install --force --registry synthesia-cargo ninox`)",
            bu::TARGET_TRIPLE
        );
    }
    let coords = CodeArtifactCoords::from_env();
    let owner = resolve_owner(&coords)?;
    let current = env!("CARGO_PKG_VERSION");

    let latest = match &args.version {
        Some(_) if !args.check => None,
        _ => match aws(&coords.list_versions_args(&owner)) {
            Ok(json) => bu::latest_published_version(&json)?,
            Err(e) if is_not_found(&e) => None,
            Err(e) => return Err(e),
        },
    };
    if args.check {
        match &latest {
            Some(l) if ninox_core::lifecycle::update_check::is_newer(current, l) =>
                println!("ninox {current} installed; {l} is available — run `ninox update`"),
            Some(l) => println!("ninox {current} is up to date (latest published: {l})"),
            None => println!(
                "ninox {current} installed; no published {} versions found ({})",
                bu::GENERIC_PACKAGE,
                owner_hint(&coords, &owner)
            ),
        }
        return Ok(());
    }
    if latest.is_none() && args.version.is_none() {
        bail!("no published {} versions found; {}", bu::GENERIC_PACKAGE, owner_hint(&coords, &owner));
    }

    let exe = ninox_core::hooks::canonical_exe().context("resolving the running ninox binary")?;
    let mut targets = resolve_targets(&exe, args.app)?;

    let version = match bu::plan_update(current, latest, args.version.as_deref(), args.force)? {
        // The CLI is current, but a requested app may still be missing or
        // older (a CLI-only install, a failed earlier swap): update just it.
        UpdatePlan::UpToDate { latest } => match targets.app.clone() {
            Some(dest) if app_needs_update(installed_app_version(&dest).as_ref(), &latest) => {
                targets = Targets { binary: None, app: Some(dest) };
                latest.to_string()
            }
            _ => {
                println!("{}", up_to_date_message(current, &latest));
                return Ok(());
            }
        },
        UpdatePlan::Install { version } => version.to_string(),
    };

    let assets_json = aws(&coords.list_assets_args(&owner, &version)).map_err(|e| {
        let hint = if is_not_found(&e) { format!("; {}", owner_hint(&coords, &owner)) } else { String::new() };
        e.context(format!("looking up {} {version}{hint}", bu::GENERIC_PACKAGE))
    })?;
    let work = TempDir::new()?;
    let mut replaced = Vec::new();

    if let Some(bin) = &targets.binary {
        let asset = bu::tarball_asset(&version);
        let tarball = download(&coords, &owner, &version, &asset, &assets_json, work.path())?;
        let new_bin = bu::extract_binary(&tarball, &version, work.path())?;
        bu::replace_binary(&new_bin, bin, |staged| {
            clear_quarantine(staged);
            bu::smoke_test_binary(staged, &version, bu::SMOKE_TEST_TIMEOUT)
        })?;
        let cargo_note = match dirs::home_dir() {
            Some(home) if bu::installed_by_cargo(bin, &home) => " (the `cargo install` copy — later `cargo install`s will overwrite it)",
            _ => "",
        };
        replaced.push(format!("{} (ninox {version}){cargo_note}", bin.display()));
        if let Err(e) = ninox_core::hooks::install_nx_alias(bin, std::env::var_os("PATH").as_deref()) {
            println!("nx alias not refreshed: {e}");
        }
    }

    if let Some(dest) = &targets.app {
        let zip = download(&coords, &owner, &version, APP_ZIP_ASSET, &assets_json, work.path())?;
        install_app(&zip, dest, &version)?;
        replaced.push(format!(
            "{} (Ninox.app {version}; its binary is {})",
            dest.display(),
            bu::app_bundle_binary(dest).display()
        ));
    }

    println!("ninox update replaced:");
    for path in &replaced {
        println!("  {path}");
    }
    println!("already-running ninox processes (desktop app, headless engine) keep the old version until restarted");
    Ok(())
}

fn resolve_owner(coords: &CodeArtifactCoords) -> Result<String> {
    if let Some(owner) = &coords.domain_owner {
        return Ok(owner.clone());
    }
    let account = aws(&["sts", "get-caller-identity", "--query", "Account", "--output", "text"].map(String::from))?;
    Ok(account.trim().to_string())
}

fn aws(args: &[String]) -> Result<String> {
    let output = Command::new("aws")
        .args(args)
        .env("AWS_PAGER", "")
        .output()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => anyhow::anyhow!(
                "the `aws` CLI is required for `ninox update` (`brew install awscli`)"
            ),
            _ => anyhow::Error::new(e).context("running aws"),
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let auth = ["sso", "token", "expired", "credentials", "ExpiredToken", "UnrecognizedClient"]
            .iter()
            .any(|needle| stderr.contains(needle));
        let sub = args.iter().take(2).cloned().collect::<Vec<_>>().join(" ");
        if auth {
            bail!("aws {sub} failed — no valid AWS session; {LOGIN_HINT}\n{}", stderr.trim());
        }
        bail!("aws {sub} failed: {}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn download(
    coords: &CodeArtifactCoords,
    owner: &str,
    version: &str,
    asset: &str,
    assets_json: &str,
    dir: &Path,
) -> Result<PathBuf> {
    let expected = bu::asset_sha256(assets_json, asset)?;
    let out = dir.join(asset);
    println!("downloading {asset}");
    aws(&coords.get_asset_args(owner, version, asset, &out))?;
    bu::verify_sha256(&out, &expected)?;
    Ok(out)
}

/// The version a bundle's own binary reports (`ninox X.Y.Z`); `None` when
/// the bundle is missing or predates `--version`.
fn installed_app_version(dest: &std::path::Path) -> Option<semver::Version> {
    let out = Command::new(bu::app_bundle_binary(dest)).arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    semver::Version::parse(text.split_whitespace().last()?.trim_start_matches('v')).ok()
}

fn app_needs_update(installed: Option<&semver::Version>, latest: &semver::Version) -> bool {
    installed.is_none_or(|v| v < latest)
}

/// `--force` only reinstalls an equal version, so it is only offered then;
/// a build ahead of every release is pointed at `--version` instead.
fn up_to_date_message(current: &str, latest: &semver::Version) -> String {
    if semver::Version::parse(current).is_ok_and(|c| c > *latest) {
        format!("ninox {current} is newer than the latest published release ({latest}); `--version {latest}` installs that one")
    } else {
        format!("ninox {current} is up to date (latest published: {latest}); --force reinstalls it")
    }
}

fn installed_app() -> Option<PathBuf> {
    let mut candidates = vec![PathBuf::from("/Applications/Ninox.app")];
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join("Applications/Ninox.app"));
    }
    candidates.into_iter().find(|p| p.is_dir())
}

fn install_app(zip: &Path, dest: &Path, version: &str) -> Result<()> {
    let parent = dest.parent().context("app destination has no parent")?;
    let staging = parent.join(format!(".ninox-update-{}", std::process::id()));
    std::fs::create_dir_all(&staging).with_context(|| format!("creating {}", staging.display()))?;
    let result = (|| -> Result<()> {
        let status = Command::new("ditto").arg("-x").arg("-k").arg(zip).arg(&staging).status()?;
        if !status.success() {
            bail!("ditto failed to extract {}", zip.display());
        }
        let new_app = staging.join("Ninox.app");
        anyhow::ensure!(new_app.is_dir(), "{} has no Ninox.app", zip.display());
        clear_quarantine(&new_app);
        bu::smoke_test_binary(&bu::app_bundle_binary(&new_app), version, bu::SMOKE_TEST_TIMEOUT)?;
        if let Some(backup) = bu::replace_app_bundle(&new_app, dest)? {
            println!("warning: installed {} but could not remove the previous bundle at {}; delete it by hand", dest.display(), backup.display());
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&staging);
    result
}

fn clear_quarantine(path: &Path) {
    let _ = Command::new("xattr")
        .args(["-dr", "com.apple.quarantine"])
        .arg(path)
        .stderr(std::process::Stdio::null())
        .status();
}

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Result<Self> {
        let path = std::env::temp_dir().join(format!("ninox-update-{}", std::process::id()));
        std::fs::create_dir_all(&path)?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod up_to_date_tests {
    #[test]
    fn force_is_only_offered_when_it_would_work() {
        let v = |s: &str| semver::Version::parse(s).unwrap();
        assert!(super::up_to_date_message("0.30.0", &v("0.30.0")).contains("--force"));
        let ahead = super::up_to_date_message("0.30.1", &v("0.30.0"));
        assert!(!ahead.contains("--force") && ahead.contains("--version 0.30.0"), "{ahead}");
    }

    #[test]
    fn a_current_cli_still_refreshes_a_missing_or_older_app() {
        let v = |s: &str| semver::Version::parse(s).unwrap();
        assert!(super::app_needs_update(None, &v("0.30.0")), "no app (or one too old to report) gets installed");
        assert!(super::app_needs_update(Some(&v("0.29.0")), &v("0.30.0")));
        assert!(!super::app_needs_update(Some(&v("0.30.0")), &v("0.30.0")));
        assert!(!super::app_needs_update(Some(&v("0.31.0")), &v("0.30.0")), "never downgrades the app");
    }
}
