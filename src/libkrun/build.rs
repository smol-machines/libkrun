fn main() {
    // NOTE: build scripts are compiled and run on the HOST, so `#[cfg(target_os)]`
    // here reflects the host, not the build target. Use `CARGO_CFG_TARGET_OS` so
    // the soname / install_name link args are chosen by the actual target (this
    // also makes cross-compilation, e.g. macOS -> Windows, link correctly).
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let major = std::env::var("CARGO_PKG_VERSION_MAJOR").unwrap();
    let minor = std::env::var("CARGO_PKG_VERSION_MINOR").unwrap();

    match target_os.as_str() {
        "linux" => {
            println!("cargo:rustc-cdylib-link-arg=-Wl,-soname,libkrun.so.{major}");
        }
        "macos" => {
            println!(
                "cargo:rustc-cdylib-link-arg=-Wl,-install_name,libkrun.{major}.dylib,-compatibility_version,{major}.0.0,-current_version,{major}.{minor}.0"
            );
        }
        // Windows (and others): the default cdylib link is correct; krun.dll needs
        // no soname/install_name link args.
        _ => {}
    }

    emit_snapshot_cfgs();
}

/// Mirrors `vmm`'s `snapshot_supported` / `fork_supported` cfgs so the C API
/// layer gates its checkpoint/restore (snapshot) and fork entry points on the
/// same predicate. See `src/vmm/build.rs` for the definitions.
fn emit_snapshot_cfgs() {
    println!("cargo:rustc-check-cfg=cfg(snapshot_supported)");
    println!("cargo:rustc-check-cfg=cfg(fork_supported)");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let linux_x86 = os == "linux" && arch == "x86_64";
    let macos_arm = os == "macos" && arch == "aarch64";
    let windows_x86 = os == "windows" && arch == "x86_64";
    if linux_x86 || macos_arm || windows_x86 {
        println!("cargo:rustc-cfg=snapshot_supported");
    }
    if linux_x86 || macos_arm || windows_x86 {
        println!("cargo:rustc-cfg=fork_supported");
    }
}
