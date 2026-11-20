//! `ninox update` — replace this binary (and optionally Ninox.app) with the
//! prebuilt release from CodeArtifact. The pure parts live in
//! `ninox_core::lifecycle::binary_update`; this file runs `aws` and touches
//! the filesystem.

use anyhow::{bail, Context, Result};
use ninox_core::config::{AppConfig, UpdateChannel};
use ninox_core::lifecycle::binary_update::{
    self as bu, BinaryPlan, CodeArtifactCoords, Flavor, UpdatePlan, APP_ZIP_ASSET,
};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct UpdateArgs {
    pub check: bool,
    pub force: bool,
    pub version: Option<String>,
    pub app: bool,
    pub channel: Option<UpdateChannel>,
    pub aws_profile: Option<String>,
    /// Restart the server on the new build without asking.
    pub yes: bool,
    /// Never offer the server restart (the GUI's in-app update, which has
    /// no terminal to ask on, runs this way and offers it itself).
    pub no_restart: bool,
    /// Persist `--channel` / `--aws-profile` and stop (the installer's step).
    pub save_only: bool,
    pub port: Option<u16>,
    pub db_path: PathBuf,
}

static FLAVOR: std::sync::OnceLock<Flavor> = std::sync::OnceLock::new();

/// Called once by each UI binary's `dispatch`: which prebuilt this process
/// is, so a plain-file copy of it can be refreshed without guessing.
pub fn set_flavor(is_tui: bool) {
    let _ = FLAVOR.set(if is_tui { Flavor::Tui } else { Flavor::Gui });
}

fn running_flavor(exe: &Path) -> Flavor {
    FLAVOR.get().copied().unwrap_or_else(|| {
        let name = exe.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if name == "nx" || name.contains("tui") { Flavor::Tui } else { Flavor::Gui }
    })
}

/// `ninox --version`. The `ninox X.Y.Z` line stays last: deployed clients
/// (`machine add`'s remote probe, the bundle version probe) read the final
/// whitespace token of this output as the version.
pub fn version_text() -> String {
    format!(
        "update channel: {}\nninox {}\n",
        AppConfig::load_or_default().update.channel,
        env!("CARGO_PKG_VERSION"),
    )
}

fn owner_hint(coords: &CodeArtifactCoords, owner: &str) -> String {
    format!(
        "looked in {}/{} ({}) owned by account {owner}; if your SSO profile is a different AWS account \
         than the CodeArtifact domain owner, set NINOX_CODEARTIFACT_DOMAIN_OWNER=<owner account id> or switch AWS_PROFILE",
        coords.domain, coords.repository, coords.region
    )
}

/// The flag to append to a suggested `ninox update` so it follows the same
/// channel that was just checked.
fn channel_flag(checked: UpdateChannel, configured: UpdateChannel) -> String {
    if checked == configured { String::new() } else { format!(" --channel {checked}") }
}

fn is_not_found(e: &anyhow::Error) -> bool {
    e.to_string().contains("ResourceNotFoundException")
}

/// What this run replaces: every installed ninox binary (planned against
/// the old siblings) and/or a Ninox.app.
struct Targets {
    binaries: BinaryPlan,
    app: Option<PathBuf>,
}

/// The self-shim's target when it is a plain file the server runs.
fn shim_target() -> Option<PathBuf> {
    ninox_core::hooks::read_shim(&ninox_core::hooks::self_shim_path()).map(|s| s.target).filter(|p| p.is_file())
}

fn resolve_targets(exe: &Path, want_app: bool, os: &str) -> Result<Targets> {
    let shim = shim_target();
    let (binaries, bundle) = if let Some(bundle) = bu::enclosing_app_bundle(exe) {
        println!("running from inside {} — updating that bundle", bundle.display());
        let outside: Vec<PathBuf> = shim.into_iter().filter(|t| !bu::inside_bundle(t, &bundle)).collect();
        let install_dir = std::env::var_os("NINOX_INSTALL_DIR").map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".local/bin")))
            .filter(|d| d.join("ninox-gui").is_file() || d.join("ninox-tui").is_file());
        // The server's own tree (the shim target) is planned on its own so a
        // leftover default install can't become its hash baseline.
        let mut plan = match outside.first().and_then(|t| t.parent()) {
            Some(dir) => bu::plan_binaries(dir, None, &outside, os),
            None => BinaryPlan::default(),
        };
        if let Some(dir) = install_dir.filter(|d| outside.first().and_then(|t| t.parent()) != Some(d.as_path())) {
            let more = bu::plan_binaries(&dir, None, &[], os);
            for entry in more.replace {
                if !plan.replace.iter().any(|(p, _)| p == &entry.0) {
                    plan.replace.push(entry);
                }
            }
            plan.warnings.extend(more.warnings);
        }
        (plan, Some(bundle))
    } else {
        let dir = exe.parent().context("the running binary has no parent directory")?;
        let extra: Vec<PathBuf> = shim.into_iter().filter(|t| t.parent() != Some(dir)).collect();
        (bu::plan_binaries(dir, Some((exe, running_flavor(exe))), &extra, os), None)
    };
    let app = match bundle {
        Some(b) => Some(b),
        None => match installed_app() {
            Some(found) => {
                println!("updating {} too (checked /Applications, then ~/Applications)", found.display());
                Some(found)
            }
            None if want_app => bail!(
                "--app: no Ninox.app in /Applications or ~/Applications to update — install it with \
                 `scripts/install-macos.sh --app`, or drop --app"
            ),
            None => None,
        },
    };
    Ok(Targets { binaries, app })
}

/// Persisted only once the profile has worked (or, for the installer, as given).
fn save_profile(profile: &str) -> Result<()> {
    AppConfig::update(|c| c.update.aws_profile = Some(profile.to_string())).context("saving the AWS profile to config.toml")?;
    println!("AWS profile set to {profile} (persisted in config.toml as [update] aws_profile)");
    Ok(())
}

/// A check that could not run reads as the cause plus its fix command.
fn explain(e: anyhow::Error) -> anyhow::Error {
    if bu::classify_check_error(&e) == bu::CheckProblem::Other {
        e
    } else {
        anyhow::anyhow!(bu::explain_check_error(&e, bu::known_aws_profile().as_deref()))
    }
}

pub async fn run(args: UpdateArgs) -> Result<()> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    if !bu::platform_supported(os, arch) {
        bail!(
            "prebuilt ninox binaries aren't published for {os}/{arch}; \
             build from source with `cargo install --force ninox-gui ninox-tui` \
             (or `cargo install --force --registry synthesia-cargo ninox-gui ninox-tui`)"
        );
    }
    let triple = bu::target_triple(os, arch).expect("checked by platform_supported above");
    if let Some(profile) = &args.aws_profile {
        std::env::set_var("AWS_PROFILE", profile);
        if args.save_only {
            save_profile(profile)?;
        }
    }
    let configured = AppConfig::load_or_default().update.channel;
    let channel = args.channel.unwrap_or(configured);
    if args.save_only {
        if channel != configured {
            AppConfig::update(|c| c.update.channel = channel).context("saving the update channel to config.toml")?;
            println!("update channel set to {channel} (persisted in config.toml)");
        }
        return Ok(());
    }
    let package = bu::generic_package(os, channel).expect("checked by platform_supported above");
    let coords = CodeArtifactCoords::from_env();
    let owner = resolve_owner(&coords).map_err(explain)?;
    let saved_profile = args.aws_profile.clone();
    let current = env!("CARGO_PKG_VERSION");

    let latest = match &args.version {
        Some(_) if !args.check => None,
        _ => match aws(&coords.list_versions_args(&owner, package)) {
            Ok(json) => bu::latest_published_version(&json, channel)?,
            Err(e) if is_not_found(&e) => None,
            Err(e) => return Err(explain(e)),
        },
    };
    let listed = args.version.is_none() || args.check;
    if let (Some(profile), true) = (&saved_profile, listed) {
        save_profile(profile)?;
    }
    if channel != configured && !args.check {
        AppConfig::update(|c| c.update.channel = channel).context("saving the update channel to config.toml")?;
        println!("update channel set to {channel} (persisted in config.toml)");
    }
    if args.check {
        match &latest {
            Some(l) if ninox_core::lifecycle::update_check::is_newer(current, l) =>
                println!("ninox {current} installed ({channel} channel); {l} is available — run `ninox update{}`", channel_flag(channel, configured)),
            Some(l) => println!("ninox {current} is up to date on the {channel} channel (latest published: {l})"),
            None => println!(
                "ninox {current} installed ({channel} channel); no published {} versions found ({})",
                package,
                owner_hint(&coords, &owner)
            ),
        }
        return Ok(());
    }
    if latest.is_none() && args.version.is_none() {
        bail!("no published {} versions found; {}", package, owner_hint(&coords, &owner));
    }

    let exe = ninox_core::hooks::canonical_exe().context("resolving the running ninox binary")?;
    let mut targets = resolve_targets(&exe, args.app, os)?;
    for w in &targets.binaries.warnings {
        println!("warning: {w}");
    }

    let version = match bu::plan_update(current, latest, args.version.as_deref(), args.force, package)? {
        // The running binary is current, but a sibling binary or the app may
        // still be older (a failed earlier swap, or an update that only
        // replaced the running app): refresh just those.
        UpdatePlan::UpToDate { latest } => {
            let stale_bins: Vec<_> = targets.binaries.replace.iter()
                .filter(|(p, _)| binary_version(p).as_ref().is_none_or(|v| v < &latest))
                .cloned()
                .collect();
            let stale_app = targets.app.as_ref()
                .filter(|dest| app_needs_update(installed_app_version(dest).as_ref(), &latest))
                .cloned();
            if stale_bins.is_empty() && stale_app.is_none() {
                println!("{}", up_to_date_message(current, &latest));
                return Ok(());
            }
            targets.binaries.replace = stale_bins;
            targets.app = stale_app;
            latest.to_string()
        }
        UpdatePlan::Install { version } => version.to_string(),
    };

    let assets_json = aws(&coords.list_assets_args(&owner, package, &version)).map_err(|e| {
        let hint = if is_not_found(&e) { format!("; {}", owner_hint(&coords, &owner)) } else { String::new() };
        explain(e).context(format!("looking up {package} {version}{hint}"))
    })?;
    if let (Some(profile), false) = (&saved_profile, listed) {
        save_profile(profile)?;
    }
    let work = TempDir::new()?;
    let mut replaced = Vec::new();
    let mut updated_paths: Vec<PathBuf> = Vec::new();

    // Both downloads (and their checksums) finish before anything is
    // swapped, so a failed download never leaves binaries and app on
    // different versions.
    let app_zip = match &targets.app {
        Some(_) => Some(download(&coords, &owner, package, &version, APP_ZIP_ASSET, &assets_json, work.path())?),
        None => None,
    };
    if !targets.binaries.replace.is_empty() {
        let asset = bu::tarball_asset(&version, triple);
        let tarball = download(&coords, &owner, package, &version, &asset, &assets_json, work.path())?;
        for (path, flavor) in install_binaries(&targets.binaries, &tarball, &version, triple, work.path())? {
            let cargo_note = match dirs::home_dir() {
                Some(home) if bu::installed_by_cargo(&path, &home) => " (the ~/.cargo/bin copy — a later `cargo install` or `cargo binstall` overwrites it)",
                _ => "",
            };
            replaced.push(format!("{} ({}, ninox {version}){cargo_note}", path.display(), flavor.entry()));
            updated_paths.push(path);
        }
    }

    if let (Some(dest), Some(zip)) = (&targets.app, &app_zip) {
        install_app(zip, dest, &version)?;
        replaced.push(format!(
            "{} (Ninox.app {version}; its binary is {})",
            dest.display(),
            bu::app_bundle_binary(dest).display()
        ));
        updated_paths.push(bu::app_bundle_binary(dest));
    }

    println!("ninox update replaced:");
    for path in &replaced {
        println!("  {path}");
    }
    refresh_shim(&updated_paths, &version);
    offer_server_restart(&args, &version).await
}

/// Replaces every planned binary from the one release tarball, each
/// smoke-tested before it swaps in. A failure stops the run: binaries
/// already swapped are individually complete, so nothing is left half-written.
fn install_binaries(plan: &BinaryPlan, tarball: &Path, version: &str, triple: &str, work: &Path) -> Result<Vec<(PathBuf, Flavor)>> {
    let mut extracted: Vec<(Flavor, PathBuf)> = Vec::new();
    let mut done = Vec::new();
    for (path, flavor) in &plan.replace {
        let new_bin = match extracted.iter().find(|(f, _)| f == flavor) {
            Some((_, p)) => p.clone(),
            None => {
                let p = bu::extract_binary(tarball, version, triple, flavor.entry(), work)?;
                extracted.push((*flavor, p.clone()));
                p
            }
        };
        bu::replace_binary(&new_bin, path, |staged| {
            clear_quarantine(staged);
            bu::smoke_test_binary(staged, version, bu::SMOKE_TEST_TIMEOUT)
        })?;
        done.push((path.clone(), *flavor));
    }
    Ok(done)
}

/// Re-marks the self-shim when what it forwards to was just replaced, so
/// the server restart (and every agent's `ninox`) runs the new build.
fn refresh_shim(updated: &[PathBuf], version: &str) {
    let Some(target) = shim_target() else { return };
    if updated.iter().any(|p| p.canonicalize().ok() == target.canonicalize().ok()) {
        match ninox_core::hooks::reset_self_shim(&target, version) {
            Ok(()) => println!("ninox shim now forwards to {} ({version})", target.display()),
            Err(e) => println!("warning: could not refresh the ninox shim ({e}); run `ninox reset-bin`"),
        }
    } else {
        println!(
            "warning: the ninox shim forwards to {}, which this update did not replace; the server would restart on it. \
             Point the shim at an updated binary with `ninox reset-bin` (run from that binary)",
            target.display()
        );
    }
}

/// After an install the running server is still the old build. Offer the
/// restart (sessions survive it); never restart without a yes.
async fn offer_server_restart(args: &UpdateArgs, version: &str) -> Result<()> {
    let port = args.port.unwrap_or_else(|| AppConfig::load_or_default().port);
    let Some(info) = crate::server_restart::fetch_info(port).await else {
        println!("no ninox server answering on :{port}; it starts on the new build next time");
        return Ok(());
    };
    if info.version == version {
        return Ok(());
    }
    let hint = "restart it with `ninox server restart` (sessions are kept)";
    if args.no_restart {
        println!("the server on :{port} still runs {}; {hint}", info.version);
        return Ok(());
    }
    let go = args.yes || {
        use std::io::{BufRead, IsTerminal, Write};
        if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
            print!("The server on :{port} still runs {}. Restart it on {version} now? Sessions are kept. [y/N] ", info.version);
            let _ = std::io::stdout().flush();
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line).is_ok() && matches!(line.trim(), "y" | "Y" | "yes")
        } else {
            false
        }
    };
    if !go {
        println!("the server on :{port} still runs {}; {hint}", info.version);
        return Ok(());
    }
    crate::server_restart::run_cli(Some(port), args.db_path.clone()).await
}

/// `<bin> --version`'s trailing token, or `None` if it won't run.
fn binary_version(path: &Path) -> Option<semver::Version> {
    let out = Command::new(path).arg("--version").stdin(std::process::Stdio::null()).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    semver::Version::parse(text.split_whitespace().last()?.trim_start_matches('v')).ok()
}

fn resolve_owner(coords: &CodeArtifactCoords) -> Result<String> {
    bu::resolve_owner(coords)
}

fn aws(args: &[String]) -> Result<String> {
    bu::run_aws(args)
}

fn download(
    coords: &CodeArtifactCoords,
    owner: &str,
    package: &str,
    version: &str,
    asset: &str,
    assets_json: &str,
    dir: &Path,
) -> Result<PathBuf> {
    let expected = bu::asset_sha256(assets_json, asset)?;
    let out = dir.join(asset);
    println!("downloading {asset}");
    aws(&coords.get_asset_args(owner, package, version, asset, &out))?;
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

/// `NINOX_APP_DIRS` (colon-separated) replaces the default search dirs, so a
/// scratch run never reaches the real /Applications.
fn installed_app() -> Option<PathBuf> {
    let dirs_list: Vec<PathBuf> = match std::env::var_os("NINOX_APP_DIRS") {
        Some(v) => std::env::split_paths(&v).collect(),
        None => [Some(PathBuf::from("/Applications")), dirs::home_dir().map(|h| h.join("Applications"))]
            .into_iter()
            .flatten()
            .collect(),
    };
    dirs_list.into_iter().map(|d| d.join("Ninox.app")).find(|p| p.is_dir())
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
    use super::{install_binaries, bu, Flavor};
    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn fake_tarball(dir: &std::path::Path, version: &str, triple: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let stem = bu::tarball_stem(version, triple);
        let root = dir.join("pkg").join(&stem);
        std::fs::create_dir_all(&root).unwrap();
        for name in ["ninox-gui", "ninox-tui"] {
            let f = root.join(name);
            std::fs::write(&f, format!("#!/bin/sh\necho \"{name} ninox {version}\"\n")).unwrap();
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let tarball = dir.join(bu::tarball_asset(version, triple));
        let status = Command::new("tar").arg("-czf").arg(&tarball).arg("-C").arg(dir.join("pkg")).arg(&stem).status().unwrap();
        assert!(status.success());
        tarball
    }

    fn old_bin(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn one_update_replaces_every_binary_in_the_install_dir_and_repairs_a_deploy_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        old_bin(&bin.join("ninox-gui"), "#!/bin/sh\necho old-gui\n");
        old_bin(&bin.join("ninox-tui"), "#!/bin/sh\necho old-tui\n");
        std::fs::copy(bin.join("ninox-tui"), bin.join("ninox")).unwrap();
        std::os::unix::fs::symlink("ninox-tui", bin.join("nx")).unwrap();
        let plan = bu::plan_binaries(&bin, Some((&bin.join("ninox-gui"), Flavor::Gui)), &[], "macos");
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let tarball = fake_tarball(tmp.path(), "9.9.9", "aarch64-apple-darwin");

        let done = install_binaries(&plan, &tarball, "9.9.9", "aarch64-apple-darwin", &work).unwrap();

        let names: Vec<_> = done.iter().map(|(p, _)| p.file_name().unwrap().to_str().unwrap().to_string()).collect();
        assert_eq!(names, ["ninox-gui", "ninox-tui", "ninox"]);
        for (name, want) in [("ninox-gui", "ninox-gui"), ("ninox-tui", "ninox-tui"), ("ninox", "ninox-tui")] {
            let out = Command::new(bin.join(name)).arg("--version").output().unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), format!("{want} ninox 9.9.9"), "{name}");
        }
        assert!(std::fs::symlink_metadata(bin.join("nx")).unwrap().file_type().is_symlink(), "symlinks are left alone");
    }

    #[test]
    fn a_copy_that_matches_nothing_is_left_alone_with_a_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        old_bin(&bin.join("ninox-gui"), "#!/bin/sh\necho old-gui\n");
        old_bin(&bin.join("ninox"), "#!/bin/sh\necho something-else\n");
        let plan = bu::plan_binaries(&bin, Some((&bin.join("ninox-gui"), Flavor::Gui)), &[], "macos");
        assert!(plan.replace.iter().all(|(p, _)| !p.ends_with("ninox")));
        assert_eq!(plan.warnings.len(), 1);
        assert!(plan.warnings[0].contains("ln -sf"), "{:?}", plan.warnings);
    }

    /// Runs the real installer against a fake `aws` serving a tarball of stub
    /// binaries, with a scratch HOME and install dir; the stub `ninox-tui`
    /// records its arguments.
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    fn run_installer(channel: &str, profile: Option<&str>) -> (bool, String, String) {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let version = if channel == "alpha" { "1.0.0-alpha.3" } else { "1.0.0" };
        let stem = bu::tarball_stem(version, "aarch64-apple-darwin");
        let pkg = tmp.path().join("pkg").join(&stem);
        std::fs::create_dir_all(&pkg).unwrap();
        let record = tmp.path().join("record");
        for (name, body) in [
            ("ninox-gui", "#!/bin/sh\nexit 0\n".to_string()),
            ("ninox-tui", format!("#!/bin/sh\necho \"$@\" >> {}\n", record.display())),
        ] {
            let f = pkg.join(name);
            std::fs::write(&f, body).unwrap();
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let tarball = tmp.path().join("pkg.tar.gz");
        assert!(Command::new("tar").arg("-czf").arg(&tarball).arg("-C").arg(tmp.path().join("pkg")).arg(&stem).status().unwrap().success());
        let sha = bu::sha256_file(&tarball).unwrap();
        let fake_bin = tmp.path().join("fakebin");
        std::fs::create_dir_all(&fake_bin).unwrap();
        let aws = fake_bin.join("aws");
        std::fs::write(&aws, format!(
            "#!/bin/sh\ncase \"$1 $2\" in\n  'sts get-caller-identity') echo 123456789012 ;;\n  'codeartifact list-package-versions') echo {version} ;;\n  'codeartifact list-package-version-assets') echo {sha} ;;\n  'codeartifact get-package-version-asset') for last; do :; done; cp {} \"$last\" ;;\nesac\n",
            tarball.display(),
        )).unwrap();
        std::fs::set_permissions(&aws, std::fs::Permissions::from_mode(0o755)).unwrap();
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/install-macos.sh");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let mut cmd = Command::new("bash");
        cmd.arg(script).args(["--channel", channel, "--no-app"])
            .env_clear()
            .env("HOME", &home)
            .env("PATH", format!("{}:/usr/bin:/bin:/usr/sbin", fake_bin.display()))
            .env("NINOX_INSTALL_DIR", home.join("bin"));
        if let Some(p) = profile {
            cmd.env("AWS_PROFILE", p);
        }
        let out = cmd.output().unwrap();
        let recorded = std::fs::read_to_string(&record).unwrap_or_default();
        (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned(), recorded)
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn installer_saves_the_channel_and_aws_profile_it_installed_with() {
        let (ok, out, recorded) = run_installer("alpha", Some("prd-build-readonly"));
        assert!(ok, "{out}");
        assert_eq!(recorded.trim(), "update --save-only --channel alpha --aws-profile prd-build-readonly");
        assert!(out.contains("Saved update settings to config.toml (channel: alpha, aws profile: prd-build-readonly)"), "{out}");
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn installer_saves_the_stable_channel_too_and_skips_an_unset_profile() {
        let (ok, _, recorded) = run_installer("stable", None);
        assert!(ok);
        assert_eq!(recorded.trim(), "update --save-only --channel stable");
    }

    #[test]
    fn check_suggests_the_channel_flag_only_when_it_differs_from_config() {
        use ninox_core::config::UpdateChannel::{Alpha, Stable};
        assert_eq!(super::channel_flag(Alpha, Alpha), "");
        assert_eq!(super::channel_flag(Alpha, Stable), " --channel alpha");
    }

    #[test]
    fn version_text_names_the_channel_and_ends_with_the_version_token() {
        let _iso = ninox_core::test_isolation::isolate();
        let text = super::version_text();
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some("update channel: stable"));
        assert_eq!(lines.next(), Some(concat!("ninox ", env!("CARGO_PKG_VERSION"))));
        assert_eq!(text.split_whitespace().last(), Some(env!("CARGO_PKG_VERSION")), "old clients parse the last token");
    }

    fn install_script_parse(args: &[&str]) -> (bool, String) {
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/install-macos.sh");
        let out = std::process::Command::new("bash")
            .arg(script)
            .args(args)
            .env("NINOX_INSTALL_PARSE_ONLY", "1")
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(if out.status.success() { &out.stdout } else { &out.stderr }).into_owned();
        (out.status.success(), text)
    }

    #[test]
    fn install_script_defaults_to_stable_and_maps_alpha_to_its_own_package() {
        assert_eq!(
            install_script_parse(&[]),
            (true, "channel=stable package=ninox-macos version= app= apps_dir=/Applications\n".into())
        );
        let (ok, text) = install_script_parse(&["--channel", "alpha", "--version", "v1.0.0-alpha.3"]);
        assert!(ok && text.contains("channel=alpha package=ninox-macos-alpha version=1.0.0-alpha.3"), "{text}");
        let (ok, text) = install_script_parse(&["--channel=alpha"]);
        assert!(ok && text.contains("package=ninox-macos-alpha"), "{text}");
        let (ok, text) = install_script_parse(&["--channel", "beta"]);
        assert!(!ok && text.contains("unknown channel"), "{text}");
        let (ok, text) = install_script_parse(&["--channel"]);
        assert!(!ok && text.contains("--channel needs a value"), "{text}");
    }

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
