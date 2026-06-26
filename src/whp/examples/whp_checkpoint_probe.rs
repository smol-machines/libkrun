//! WHP checkpoint round-trip probe — exercises the real `WhpVcpuState`
//! save/restore path (registers + LAPIC + XSAVE) through the `whp` crate API,
//! the same code the VMM's snapshot/fork uses.
//!
//! Build:  cargo build --example whp_checkpoint_probe --target x86_64-pc-windows-gnu --release
//! Run on the WHP host:  whp_checkpoint_probe.exe

#[cfg(not(windows))]
fn main() {
    eprintln!("whp_checkpoint_probe only runs on Windows");
}

#[cfg(windows)]
fn main() {
    use std::sync::Arc;
    use krun_whp::{WhpVcpu, WhpVm};
    use windows_sys::Win32::System::Hypervisor::{
        WHvSetVirtualProcessorRegisters, WHvX64RegisterRax, WHvX64RegisterRip, WHV_REGISTER_VALUE,
    };

    println!("== WHP CHECKPOINT ROUND-TRIP PROBE ==");

    let vm = match WhpVm::new(1) {
        Ok(vm) => Arc::new(vm),
        Err(e) => {
            println!("FATAL WhpVm::new: {e}");
            return;
        }
    };
    let vcpu = match WhpVcpu::new(vm.clone(), 0) {
        Ok(v) => v,
        Err(e) => {
            println!("FATAL WhpVcpu::new: {e}");
            return;
        }
    };

    // Seed a couple of registers with known values.
    let part = vm.partition_handle();
    let set = |name, val: u64| unsafe {
        let mut v: WHV_REGISTER_VALUE = std::mem::zeroed();
        v.Reg64 = val;
        WHvSetVirtualProcessorRegisters(part, 0, &name, 1, &v);
    };
    set(WHvX64RegisterRax, 0xCAFEF00D_12345678);
    set(WHvX64RegisterRip, 0x0000_0000_0010_0000);

    // Capture.
    let state = match vcpu.save_state() {
        Ok(s) => s,
        Err(e) => {
            println!("FATAL save_state: {e}");
            return;
        }
    };
    let blob = state.to_bytes();
    println!("save_state OK; serialized {} bytes", blob.len());

    // Mutate RAX out from under the checkpoint.
    set(WHvX64RegisterRax, 0xDEAD_DEAD_DEAD_DEAD);

    // Round-trip the serialized form, then restore.
    let restored = match krun_whp::WhpVcpuState::from_bytes(&blob) {
        Ok(s) => s,
        Err(e) => {
            println!("FATAL from_bytes: {e}");
            return;
        }
    };
    if let Err(e) = vcpu.restore_state(&restored) {
        println!("FATAL restore_state: {e}");
        return;
    }

    // Verify RAX came back to the checkpointed value.
    let [rax, rip] = match vcpu.get_registers64([WHvX64RegisterRax, WHvX64RegisterRip]) {
        Ok(v) => v,
        Err(e) => {
            println!("FATAL get_registers64: {e}");
            return;
        }
    };
    let rax_ok = rax == 0xCAFEF00D_12345678;
    let rip_ok = rip == 0x0000_0000_0010_0000;
    println!(
        "RESTORE-VERIFY rax=0x{rax:016x} rip=0x{rip:016x} -> {}",
        if rax_ok && rip_ok { "PASS" } else { "FAIL" }
    );

    println!("== PROBE DONE ==");
}
