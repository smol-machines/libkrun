fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-lib=framework=Hypervisor");
    }
    emit_snapshot_cfgs();
}

/// Emits the `snapshot_supported` / `fork_supported` cfgs used to gate the
/// checkpoint/restore (snapshot) and 1→N clone (fork) code paths.
///
/// - `snapshot_supported`: the platform can capture/restore full VM + vCPU +
///   device state in process — Linux x86_64 (KVM), macOS aarch64 (HVF), and
///   Windows x86_64 (WHP).
/// - `fork_supported`: the platform additionally has cross-process CoW guest
///   RAM for golden→clone forking — Linux x86_64 and macOS aarch64. Windows is
///   not yet enabled here (the WHP CoW memory path is still in progress).
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
    if linux_x86 || macos_arm {
        println!("cargo:rustc-cfg=fork_supported");
    }
}
