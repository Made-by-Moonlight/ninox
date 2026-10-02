fn main() {
    println!("cargo:rerun-if-changed=src/ort_link_compat.cpp");

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
