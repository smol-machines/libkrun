fn main() {
    #[cfg(target_os = "linux")]
    println!(
        "cargo:rustc-cdylib-link-arg=-Wl,-soname,libkrun.so.{}",
        std::env::var("CARGO_PKG_VERSION_MAJOR").unwrap()
    );
    // Force lazy binding (override the build host's hardened `-z now` default).
    // libkrun carries undefined `virgl_renderer_*` references that live in
    // libvirglrenderer, which is dlopen'd ONLY on a GPU request and is absent
    // from GPU-less hosts (it is deliberately not in DT_NEEDED). Under `-z now`
    // (DF_1_NOW / full RELRO) the loader resolves every undefined symbol eagerly
    // at dlopen() time, overriding the caller's RTLD_LAZY, so loading libkrun on
    // a GPU-less host fails with `undefined symbol: virgl_renderer_poll` and every
    // microVM boot dies. `-z lazy` defers those symbols until called (never, with
    // no GPU), restoring the pre-2.0 behavior. `-z relro` (partial RELRO) is kept —
    // only eager PLT binding is dropped. See the 2026-06-28 fleet boot incident.
    #[cfg(target_os = "linux")]
    println!("cargo:rustc-cdylib-link-arg=-Wl,-z,lazy");
    #[cfg(target_os = "macos")]
    println!(
        "cargo:rustc-cdylib-link-arg=-Wl,-install_name,libkrun.{}.dylib,-compatibility_version,{}.0.0,-current_version,{}.{}.0",
        std::env::var("CARGO_PKG_VERSION_MAJOR").unwrap(), std::env::var("CARGO_PKG_VERSION_MAJOR").unwrap(),
        std::env::var("CARGO_PKG_VERSION_MAJOR").unwrap(), std::env::var("CARGO_PKG_VERSION_MINOR").unwrap()
    );
    #[cfg(target_os = "macos")]
    println!("cargo:rustc-link-lib=framework=Hypervisor");
}
