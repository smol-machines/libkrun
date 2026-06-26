//! WHP fork-feasibility probe (run on a Windows Hypervisor Platform host).
//!
//! Answers the three make-or-break questions for porting checkpoint/fork to WHP:
//!  1. Can a vCPU register be set and read back (register round-trip)?
//!  2. Can the LAPIC / interrupt-controller state be captured and restored?
//!  3. Can a `PAGE_WRITECOPY` view be mapped as guest RAM, with real CoW?
//!
//! Build:  cargo build --example whp_probe --target x86_64-pc-windows-gnu --release
//! Run on the WHP host:  whp_probe.exe

#[cfg(not(windows))]
fn main() {
    eprintln!("whp_probe only runs on Windows");
}

#[cfg(windows)]
fn main() {
    use std::ffi::c_void;
    use std::ptr;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE, S_OK};
    use windows_sys::Win32::System::Hypervisor::{
        WHV_PARTITION_HANDLE, WHV_REGISTER_VALUE, WHvCreatePartition, WHvCreateVirtualProcessor,
        WHvGetVirtualProcessorInterruptControllerState, WHvGetVirtualProcessorRegisters,
        WHvGetVirtualProcessorXsaveState, WHvMapGpaRange, WHvMapGpaRangeFlagExecute,
        WHvMapGpaRangeFlagRead, WHvMapGpaRangeFlagWrite,
        WHvPartitionPropertyCodeLocalApicEmulationMode, WHvPartitionPropertyCodeProcessorCount,
        WHvSetPartitionProperty, WHvSetVirtualProcessorInterruptControllerState,
        WHvSetVirtualProcessorRegisters, WHvSetVirtualProcessorXsaveState, WHvSetupPartition,
        WHvX64LocalApicEmulationModeXApic, WHvX64RegisterRax,
    };
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, FILE_MAP_ALL_ACCESS, FILE_MAP_COPY, MapViewOfFile,
        PAGE_EXECUTE_READWRITE,
    };

    println!("== WHP FORK FEASIBILITY PROBE ==");

    unsafe {
        // --- minimal partition + vCPU ---
        let mut part: WHV_PARTITION_HANDLE = 0;
        let hr = WHvCreatePartition(&mut part);
        if hr != S_OK {
            println!("FATAL WHvCreatePartition hr=0x{hr:08x}");
            return;
        }
        let count: u32 = 1;
        let _ = WHvSetPartitionProperty(
            part,
            WHvPartitionPropertyCodeProcessorCount,
            &count as *const _ as *const c_void,
            4,
        );
        let apic: i32 = WHvX64LocalApicEmulationModeXApic;
        let apic_hr = WHvSetPartitionProperty(
            part,
            WHvPartitionPropertyCodeLocalApicEmulationMode,
            &apic as *const _ as *const c_void,
            4,
        );
        println!("APIC-MODE-SET hr=0x{apic_hr:08x}");
        let su = WHvSetupPartition(part);
        if su != S_OK {
            println!("FATAL WHvSetupPartition hr=0x{su:08x}");
            return;
        }
        let cv = WHvCreateVirtualProcessor(part, 0, 0);
        if cv != S_OK {
            println!("FATAL WHvCreateVirtualProcessor hr=0x{cv:08x}");
            return;
        }

        // --- 1. register round-trip ---
        let names = [WHvX64RegisterRax];
        let mut setv: WHV_REGISTER_VALUE = std::mem::zeroed();
        setv.Reg64 = 0xDEAD_BEEF_1234_5678;
        let sr = WHvSetVirtualProcessorRegisters(part, 0, names.as_ptr(), 1, &setv);
        let mut getv: WHV_REGISTER_VALUE = std::mem::zeroed();
        let gr = WHvGetVirtualProcessorRegisters(part, 0, names.as_ptr(), 1, &mut getv);
        let rax = getv.Reg64;
        println!(
            "1) REG-ROUNDTRIP set_hr=0x{sr:08x} get_hr=0x{gr:08x} rax=0x{rax:016x} -> {}",
            if rax == 0xDEAD_BEEF_1234_5678 {
                "PASS"
            } else {
                "FAIL"
            }
        );

        // --- 2. LAPIC / interrupt-controller state round-trip ---
        let mut lbuf = vec![0u8; 16 * 1024];
        let mut written: u32 = 0;
        let lg = WHvGetVirtualProcessorInterruptControllerState(
            part,
            0,
            lbuf.as_mut_ptr() as *mut c_void,
            lbuf.len() as u32,
            &mut written,
        );
        let ls = if lg == S_OK {
            WHvSetVirtualProcessorInterruptControllerState(
                part,
                0,
                lbuf.as_ptr() as *const c_void,
                written,
            )
        } else {
            -1
        };
        println!(
            "2) LAPIC-ROUNDTRIP get_hr=0x{lg:08x} bytes={written} set_hr=0x{ls:08x} -> {}",
            if lg == S_OK && ls == S_OK {
                "PASS"
            } else {
                "FAIL"
            }
        );

        // --- 2b. XSAVE (FPU/SSE) round-trip ---
        let mut xbuf = vec![0u8; 16 * 1024];
        let mut xw: u32 = 0;
        let xg = WHvGetVirtualProcessorXsaveState(
            part,
            0,
            xbuf.as_mut_ptr() as *mut c_void,
            xbuf.len() as u32,
            &mut xw,
        );
        let xs = if xg == S_OK {
            WHvSetVirtualProcessorXsaveState(part, 0, xbuf.as_ptr() as *const c_void, xw)
        } else {
            -1
        };
        println!(
            "2b) XSAVE-ROUNDTRIP get_hr=0x{xg:08x} bytes={xw} set_hr=0x{xs:08x} -> {}",
            if xg == S_OK && xs == S_OK {
                "PASS"
            } else {
                "FAIL"
            }
        );

        // --- 3. CoW guest RAM via PAGE_WRITECOPY + WHvMapGpaRange ---
        let size: usize = 0x10_0000; // 1 MiB
        let sect = CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            ptr::null(),
            PAGE_EXECUTE_READWRITE,
            0,
            size as u32,
            ptr::null(),
        );
        if sect.is_null() {
            println!("3) COW FAIL CreateFileMapping null");
            return;
        }
        let golden = MapViewOfFile(sect, FILE_MAP_ALL_ACCESS, 0, 0, size);
        let cow = MapViewOfFile(sect, FILE_MAP_COPY, 0, 0, size);
        let gp = golden.Value as *mut u8;
        let cp = cow.Value as *mut u8;
        if gp.is_null() || cp.is_null() {
            println!("3) COW FAIL MapViewOfFile null golden={gp:?} cow={cp:?}");
            return;
        }
        *gp = 0xAA;
        let cow_saw = *cp; // should observe 0xAA (clean shared page)
        *cp = 0xBB; // triggers copy-on-write for the cow view
        let golden_after = *gp; // should remain 0xAA
        let cow_ok = cow_saw == 0xAA && golden_after == 0xAA && *cp == 0xBB;
        println!(
            "3) COW cow_saw=0x{cow_saw:02x} golden_after_cow_write=0x{golden_after:02x} -> {}",
            if cow_ok { "PASS" } else { "FAIL" }
        );

        // Map the CoW view as guest RAM.
        let mr = WHvMapGpaRange(
            part,
            cow.Value,
            0,
            size as u64,
            WHvMapGpaRangeFlagRead | WHvMapGpaRangeFlagWrite | WHvMapGpaRangeFlagExecute,
        );
        println!(
            "3b) MAP-COW-AS-GUEST-RAM hr=0x{mr:08x} -> {}",
            if mr == S_OK { "PASS" } else { "FAIL" }
        );

        let _ = CloseHandle(sect);
    }

    println!("== PROBE DONE ==");
}
