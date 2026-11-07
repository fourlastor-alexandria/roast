fn main() {
    static_vcruntime::metabuild();
    #[cfg(target_os = "windows")]
    enable_gpu_flags();
    embed_windows_manifest();
}

#[cfg(target_os = "windows")]
fn enable_gpu_flags() {
    if requires_gpu_flag() {
        println!("cargo:rustc-link-arg=/EXPORT:NvOptimusEnablement");
        println!("cargo:rustc-link-arg=/EXPORT:AmdPowerXpressRequestHighPerformance");
    }
}

#[cfg(target_os = "windows")]
fn requires_gpu_flag() -> bool {
    use std::env;
    match env::var("CARGO_FEATURE_WIN_NO_GPU") {
        Ok(_) => false,
        _ => true,
    }
}

// Only the MSVC linker can embed a manifest file, so other Windows toolchains build without it
fn embed_windows_manifest() {
    use std::{env, path::Path};

    const MANIFEST: &str = "windows/roast.manifest";
    println!("cargo:rerun-if-changed={MANIFEST}");
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os != "windows" || target_env != "msvc" {
        return;
    }
    let manifest = Path::new(&env::var("CARGO_MANIFEST_DIR").unwrap()).join(MANIFEST);
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
        manifest.display()
    );
}
