//! `ninox update` — replace this binary (and optionally Ninox.app) with the
//! prebuilt release from CodeArtifact. The pure parts live in
//! `ninox_core::lifecycle::binary_update`; this file runs `aws` and touches
//! the filesystem.

use anyhow::{bail, Context, Result};
use ninox_core::config::{AppConfig, UpdateChannel};
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
    pub channel: Option<UpdateChannel>,
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
    let configured = AppConfig::load_or_default().update.channel;
    let channel = args.channel.unwrap_or(configured);
    let package = bu::generic_package(os, channel).expect("checked by platform_supported above");
    let coords = CodeArtifactCoords::from_env();
    let owner = resolve_owner(&coords)?;
    let current = env!("CARGO_PKG_VERSION");

    let latest = match &args.version {
        Some(_) if !args.check => None,
        _ => match aws(&coords.list_versions_args(&owner, package)) {
            Ok(json) => bu::latest_published_version(&json, channel)?,
            Err(e) if is_not_found(&e) => None,
            Err(e) => return Err(e),
        },
    };
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
    let mut targets = resolve_targets(&exe, args.app)?;

    let version = match bu::plan_update(current, latest, args.version.as_deref(), args.force, package)? {
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

    let assets_json = aws(&coords.list_assets_args(&owner, package, &version)).map_err(|e| {
        let hint = if is_not_found(&e) { format!("; {}", owner_hint(&coords, &owner)) } else { String::new() };
        e.context(format!("looking up {package} {version}{hint}"))
    })?;
    let work = TempDir::new()?;
    let mut replaced = Vec::new();

    if let Some(bin) = &targets.binary {
        let bin_name = bin.file_name().and_then(|n| n.to_str())
            .context("the running binary's path has no file name")?;
        let bin_name = tarball_bin_name(bin_name, os);
        let asset = bu::tarball_asset(&version, triple);
        let tarball = download(&coords, &owner, package, &version, &asset, &assets_json, work.path())?;
        let new_bin = bu::extract_binary(&tarball, &version, triple, bin_name, work.path())?;
        bu::replace_binary(&new_bin, bin, |staged| {
            clear_quarantine(staged);
            bu::smoke_test_binary(staged, &version, bu::SMOKE_TEST_TIMEOUT)
        })?;
        let cargo_note = match dirs::home_dir() {
            Some(home) if bu::installed_by_cargo(bin, &home) => " (the ~/.cargo/bin copy — a later `cargo install` or `cargo binstall` overwrites it; \
                `cargo binstall ninox-gui ninox-tui` installs prebuilt binaries without compiling)",
            _ => "",
        };
        replaced.push(format!("{} (ninox {version}){cargo_note}", bin.display()));
        // `nx` has always meant "open the terminal UI" — only ninox-tui
        // refreshes it, so updating ninox-gui never repoints `nx` at a
        // binary with no TUI.
        if bin_name == "ninox-tui" {
            if let Err(e) = ninox_core::hooks::install_nx_alias(bin, std::env::var_os("PATH").as_deref()) {
                println!("nx alias not refreshed: {e}");
            }
        }
    }

    if let Some(dest) = &targets.app {
        let zip = download(&coords, &owner, package, &version, APP_ZIP_ASSET, &assets_json, work.path())?;
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

/// Maps the running binary's own file name to the entry name it has inside
/// the release tarball. `nx` is its own compiled `[[bin]]` target (not a
/// symlink) when installed via `cargo install`, so `canonical_exe()`
/// resolves to a real file named `nx` — but the tarball only ever ships
/// `ninox-gui`/`ninox-tui`. `scripts/install-macos.sh`'s `nx` is a real
/// symlink to `ninox-tui` instead, which `canonical_exe()` already resolves
/// through, so this only ever rewrites the cargo-installed case.
/// Linux only ever ships `ninox-tui` (remotes are ptyd/TUI-only — spec
/// resolution #6), so any name resolves to it there, same as the existing
/// `nx` cargo-install alias does on every platform.
fn tarball_bin_name<'a>(bin_name: &'a str, os: &str) -> &'a str {
    if os == "linux" || bin_name == "nx" { "ninox-tui" } else { bin_name }
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

    #[test]
    fn tarball_bin_name_maps_the_cargo_installed_nx_binary_to_ninox_tui() {
        assert_eq!(super::tarball_bin_name("nx", "macos"), "ninox-tui");
        assert_eq!(super::tarball_bin_name("ninox-tui", "macos"), "ninox-tui");
        assert_eq!(super::tarball_bin_name("ninox-gui", "macos"), "ninox-gui");
    }

    /// Regression for a real finding: `machine add` installs the remote
    /// binary as a plain file literally named `ninox` (not a symlink, see
    /// `install_local_binary` in remote.rs), so `ninox update` run on that
    /// remote resolves its own exe name to `"ninox"` — which isn't `"nx"`,
    /// so the old mapping left it unchanged and `extract_binary` failed
    /// looking for a `"ninox"` entry the Linux tarball never ships. Linux
    /// only ever ships `ninox-tui`, so any name must resolve to it there.
    #[test]
    fn tarball_bin_name_always_resolves_to_ninox_tui_on_linux() {
        assert_eq!(super::tarball_bin_name("ninox", "linux"), "ninox-tui");
        assert_eq!(super::tarball_bin_name("ninox-tui", "linux"), "ninox-tui");
        assert_eq!(super::tarball_bin_name("nx", "linux"), "ninox-tui");
    }
}
