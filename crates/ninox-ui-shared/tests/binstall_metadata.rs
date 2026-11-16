//! `cargo binstall` downloads whatever `[package.metadata.binstall]` in the
//! gui/tui manifests expands to. A mismatch with the tarballs
//! `scripts/package-release.sh` produces (or with the names `ninox update`
//! looks up) silently falls back to a source build, so pin them together here.

use ninox_core::lifecycle::binary_update::{tarball_asset, tarball_stem, target_triple};
use std::path::{Path, PathBuf};
use std::process::Command;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const TARGETS: [(&str, &str); 3] = [("macos", "aarch64"), ("linux", "x86_64"), ("linux", "aarch64")];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn manifest(krate: &str) -> toml::Value {
    let text = std::fs::read_to_string(repo_root().join("crates").join(krate).join("Cargo.toml")).unwrap();
    text.parse().unwrap()
}

fn binstall_field(m: &toml::Value, field: &str) -> String {
    m["package"]["metadata"]["binstall"][field].as_str().unwrap_or_else(|| panic!("missing binstall {field}")).to_string()
}

fn expand(template: &str, repo: &str, target: &str, bin: &str) -> String {
    template
        .replace("{ repo }", repo)
        .replace("{ version }", VERSION)
        .replace("{ target }", target)
        .replace("{ bin }", bin)
        .replace("{ binary-ext }", "")
}

#[test]
fn binstall_urls_point_at_the_assets_ninox_update_also_uses() {
    for krate in ["ninox-gui", "ninox-tui"] {
        let m = manifest(krate);
        let repo = m["package"]["repository"].as_str().unwrap().trim_end_matches('/');
        assert!(repo.starts_with("https://github.com/"), "{krate}: binstall resolves {{ repo }} against GitHub releases");
        assert_eq!(binstall_field(&m, "pkg-fmt"), "tgz");
        let pkg_url = binstall_field(&m, "pkg-url");
        let bin_dir = binstall_field(&m, "bin-dir");
        for (os, arch) in TARGETS {
            let triple = target_triple(os, arch).unwrap();
            assert_eq!(
                expand(&pkg_url, repo, triple, ""),
                format!("{repo}/releases/download/v{VERSION}/{}", tarball_asset(VERSION, triple)),
                "{krate} {triple}"
            );
            assert_eq!(
                expand(&bin_dir, repo, triple, krate),
                format!("{}/{krate}", tarball_stem(VERSION, triple)),
                "{krate} {triple}"
            );
        }
    }
}

fn package(bins: &[&str], target: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir(&bin_dir).unwrap();
    for b in bins {
        std::fs::write(bin_dir.join(b), format!("fake {b}")).unwrap();
        std::fs::set_permissions(bin_dir.join(b), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    }
    let dist = tmp.path().join("dist");
    let out = Command::new("bash")
        .arg(repo_root().join("scripts/package-release.sh"))
        .args([VERSION, target])
        .arg(&bin_dir)
        .arg(&dist)
        .args(bins)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    (tmp, dist)
}

fn entries(tarball: &Path) -> Vec<String> {
    let out = Command::new("tar").arg("-tzf").arg(tarball).output().unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap().lines().map(|l| l.trim_end_matches('/').to_string()).collect()
}

#[test]
fn packaged_tarball_has_the_name_and_every_bin_binstall_requires() {
    for (os, arch) in TARGETS {
        let triple = target_triple(os, arch).unwrap();
        let bins: &[&str] = if os == "macos" { &["ninox-gui", "ninox-tui"] } else { &["ninox-tui"] };
        let (_tmp, dist) = package(bins, triple);
        let tarball = dist.join(tarball_asset(VERSION, triple));
        assert!(tarball.is_file(), "expected {}", tarball.display());
        let listed = entries(&tarball);
        let stem = tarball_stem(VERSION, triple);
        let has = |bin: &str| listed.contains(&format!("{stem}/{bin}"));

        // binstall fails the whole strategy if any [[bin]] of the crate is absent.
        let tui = manifest("ninox-tui");
        for bin in tui["bin"].as_array().unwrap() {
            assert!(has(bin["name"].as_str().unwrap()), "{triple}: ninox-tui bin missing from {listed:?}");
        }
        assert_eq!(has("ninox-gui"), os == "macos", "{triple}: ninox-gui only ships in the macOS tarball");
    }
}

#[test]
fn packaged_nx_is_a_regular_copy_of_ninox_tui() {
    let (_tmp, dist) = package(&["ninox-tui"], "x86_64-unknown-linux-gnu");
    let tarball = dist.join(tarball_asset(VERSION, "x86_64-unknown-linux-gnu"));
    let long = Command::new("tar").arg("-tvzf").arg(&tarball).output().unwrap();
    let long = String::from_utf8(long.stdout).unwrap();
    for name in ["/ninox-tui", "/nx"] {
        let line = long.lines().find(|l| l.ends_with(name)).unwrap_or_else(|| panic!("{name} entry"));
        assert!(line.contains('x'), "{name} must keep its executable bit: {line}");
    }
    let nx = long.lines().find(|l| l.ends_with("/nx")).expect("nx entry");
    assert!(nx.starts_with('-'), "binstall ignores hard-link and symlink entries: {nx}");
}
