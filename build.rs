fn main() {
    #[cfg(target_os = "windows")]
    println!("cargo:rustc-link-arg=/INCLUDE:NvOptimusEnablement");
    #[cfg(target_os = "windows")]
    println!("cargo:rustc-link-arg=/INCLUDE:AmdPowerXpressRequestHighPerformance");
}
