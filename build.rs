use std::env;

fn main() {
    #[cfg(target_os = "windows")]
    enable_gpu_flags();
}

fn enable_gpu_flags() {
    if requires_gpu_flag() {
        println!("cargo:rustc-link-arg=/EXPORT:NvOptimusEnablement");
        println!("cargo:rustc-link-arg=/EXPORT:AmdPowerXpressRequestHighPerformance");
    }
}

fn requires_gpu_flag() -> bool {
    match env::var("CARGO_FEATURE_WIN_NO_GPU") {
        Ok(_) => false,
        _ => true,
    }
}
