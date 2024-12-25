fn main() {
    #[cfg(target_os = "windows")]
    enable_gpu_flags();
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
