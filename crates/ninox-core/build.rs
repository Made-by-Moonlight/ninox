fn main() {
    println!("cargo:rerun-if-changed=src/ort_link_compat.cpp");
    emit_build_hash();

    // Companion to the `ort_link_compat` module in src/embeddings.rs — see
    // there for the full story. Only relevant where the prebuilt ONNX
    // Runtime static libraries can be linked against a libstdc++ older than
    // the GCC 13+ toolchain they were built with (i.e. glibc Linux).
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if os == "linux" && env == "gnu" {
        cc::Build::new()
            .cpp(true)
            .file("src/ort_link_compat.cpp")
            .compile("ninox_ort_link_compat");
    }
}

/// `NINOX_BUILD_HASH`: the short commit a binary was built from, so a
/// restarted daemon can report old → new builds that share a version.
/// "unknown" outside a git checkout (e.g. a crates.io build).
fn emit_build_hash() {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    // logs/HEAD changes on every commit, checkout and reset of this
    // worktree, so the hash never goes stale in an incremental build.
    if let Some(dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        println!("cargo:rerun-if-changed={dir}/HEAD");
        println!("cargo:rerun-if-changed={dir}/logs/HEAD");
    }
    let hash = git(&["rev-parse", "--short=10", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=NINOX_BUILD_HASH={hash}");
}
