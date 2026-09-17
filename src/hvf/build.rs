fn main() {
    // Device-level tests use HVF without linking the VMM crate. Keep this
    // backend's native dependency attached to the crate that calls it.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-lib=framework=Hypervisor");
    }
}
