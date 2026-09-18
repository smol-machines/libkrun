fn main() {
    emit_snapshot_cfgs();
}

/// Emits the `snapshot_supported` / `fork_supported` cfgs used to gate the
/// checkpoint/restore (snapshot) and 1→N clone (fork) code paths.
///
/// - `snapshot_supported`: the platform can capture/restore full VM + vCPU +
///   device state in process — Linux x86_64/aarch64 (KVM), macOS aarch64 (HVF),
///   and Windows x86_64 (WHP).
/// - `fork_supported`: the platform additionally has cross-process CoW guest
///   RAM for golden→clone forking — Linux x86_64/aarch64 (memfd), macOS aarch64
///   (backing file), and Windows x86_64 (FILE_MAP_COPY view of a backing file).
///   Currently the same set as `snapshot_supported`; kept distinct so a future
///   platform that can snapshot but not fork can diverge.
fn emit_snapshot_cfgs() {
    println!("cargo:rustc-check-cfg=cfg(snapshot_supported)");
    println!("cargo:rustc-check-cfg=cfg(deferred_stream_supported)");
    println!("cargo:rustc-check-cfg=cfg(fork_supported)");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let linux_x86 = os == "linux" && arch == "x86_64";
    let linux_arm = os == "linux" && arch == "aarch64";
    let macos_arm = os == "macos" && arch == "aarch64";
    let windows_x86 = os == "windows" && arch == "x86_64";
    if linux_x86 || linux_arm || macos_arm || windows_x86 {
        println!("cargo:rustc-cfg=snapshot_supported");
    }
    // Deferring the guest-RAM write to a caller-provided stream (what an
    // incremental checkpoint store consumes) needs retained CoW RAM, which is
    // every KVM/HVF platform here; Windows has no equivalent yet.
    if linux_x86 || linux_arm || macos_arm {
        println!("cargo:rustc-cfg=deferred_stream_supported");
    }
    if linux_x86 || linux_arm || macos_arm || windows_x86 {
        println!("cargo:rustc-cfg=fork_supported");
    }
}
