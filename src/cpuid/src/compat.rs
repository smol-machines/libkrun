// Copyright 2026 The libkrun Authors
// SPDX-License-Identifier: Apache-2.0

//! Whether a host can run a guest whose CPUID was recorded elsewhere.
//!
//! A checkpoint carries the exact CPUID its guest was given, and restore
//! installs that CPUID rather than the destination's profile. `KVM_SET_CPUID2`
//! does not check feature bits against the host: a guest told it has SHA on a
//! host without it would take #UD the first time it hashes. So restore
//! compares the recorded instruction features and XCR0 with
//! `KVM_GET_SUPPORTED_CPUID` first, and a vCPU added to a restored guest is
//! given the recorded features rather than this libkrun's profile.

use std::fmt;

use kvm_bindings::{CpuId, kvm_cpuid_entry2};

use crate::intel_portable::required;

/// A CPUID register.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Reg {
    /// EAX
    Eax,
    /// EBX
    Ebx,
    /// ECX
    Ecx,
    /// EDX
    Edx,
}

impl Reg {
    fn read(self, entry: &kvm_cpuid_entry2) -> u32 {
        match self {
            Reg::Eax => entry.eax,
            Reg::Ebx => entry.ebx,
            Reg::Ecx => entry.ecx,
            Reg::Edx => entry.edx,
        }
    }
}

impl fmt::Display for Reg {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            Reg::Eax => "EAX",
            Reg::Ebx => "EBX",
            Reg::Ecx => "ECX",
            Reg::Edx => "EDX",
        })
    }
}

/// A feature flag the guest was given that the host cannot provide.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MissingFeature {
    /// CPUID leaf.
    pub function: u32,
    /// CPUID subleaf.
    pub index: u32,
    /// Register holding the flag.
    pub reg: Reg,
    /// Bit within the register.
    pub bit: u32,
}

impl MissingFeature {
    /// The feature's Linux `/proc/cpuinfo` name, when it has one.
    pub fn name(&self) -> Option<&'static str> {
        feature_name(self.function, self.index, self.reg, self.bit)
    }
}

impl fmt::Display for MissingFeature {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if let Some(name) = self.name() {
            write!(f, "{name} ")?;
        }
        write!(
            f,
            "(CPUID.(EAX={:#x},ECX={}):{}[{}])",
            self.function, self.index, self.reg, self.bit
        )
    }
}

/// The features restore requires of a destination: the instructions the
/// portable profile can expose (see [`crate::intel_portable::required`]),
/// wherever the guest was given them.
const REQUIRED: &[(u32, u32, Reg, u32)] = &[
    (0x1, 0, Reg::Ecx, required::LEAF1_ECX),
    (0x1, 0, Reg::Edx, required::LEAF1_EDX),
    (0x7, 0, Reg::Ebx, required::LEAF7_0_EBX),
    (0x7, 0, Reg::Ecx, required::LEAF7_0_ECX),
    (0x7, 0, Reg::Edx, required::LEAF7_0_EDX),
    (0x7, 1, Reg::Eax, required::LEAF7_1_EAX),
];

fn find(cpuid: &CpuId, function: u32, index: u32) -> Option<&kvm_cpuid_entry2> {
    cpuid
        .as_slice()
        .iter()
        .find(|e| e.function == function && e.index == index)
}

/// Instructions the guest was given (CPUID bits set in `guest`, among the
/// ones restore requires) that `supported`, the destination's
/// `KVM_GET_SUPPORTED_CPUID`, does not report. Empty means the host can run
/// the guest.
///
/// Every other recorded bit is ignored: speculation controls, emulated
/// facilities and hints follow microcode and the host kernel, and hosts with
/// identical CPUs on different kernels must keep accepting each other's
/// checkpoints. So are bits the old T2-based mask let through (AVX-512 and
/// AMX sub-features without their state): a guest cannot use them.
pub fn missing_features(guest: &CpuId, supported: &CpuId) -> Vec<MissingFeature> {
    let mut missing = Vec::new();
    for &(function, index, reg, required) in REQUIRED {
        let Some(wanted) = find(guest, function, index).map(|e| reg.read(e) & required) else {
            continue;
        };
        let have = find(supported, function, index).map_or(0, |e| reg.read(e));
        let lacking = wanted & !have;
        missing.extend(
            (0..32)
                .filter(|bit| lacking & (1 << bit) != 0)
                .map(|bit| MissingFeature {
                    function,
                    index,
                    reg,
                    bit,
                }),
        );
    }
    missing
}

/// XSAVE state components enabled in the guest's `xcr0` that the host's
/// `KVM_GET_SUPPORTED_CPUID` cannot hold. Zero means the host can.
pub fn missing_xcr0(xcr0: u64, supported: &CpuId) -> u64 {
    let have = find(supported, 0xd, 0).map_or(0, |e| u64::from(e.eax) | u64::from(e.edx) << 32);
    xcr0 & !have
}

/// Leaves that say what the CPU can do, as opposed to topology, caches or
/// the brand string.
const FEATURE_LEAVES: [u32; 4] = [0x1, 0x7, 0xd, 0x8000_0001];

/// Give `target`, a vCPU's freshly built CPUID, the feature leaves of
/// `profile`, the CPUID a restored guest is already running with.
///
/// A vCPU added to a restored VM must offer the same features as the vCPUs
/// the guest booted with, even when this libkrun's profile has since
/// changed: the guest decided at boot what it may use. Topology stays the
/// new vCPU's own; leaf 1 EBX (its APIC ID and package size) is kept too.
pub fn adopt_feature_leaves(
    target: &mut CpuId,
    profile: &CpuId,
) -> Result<(), vmm_sys_util::fam::Error> {
    let own_leaf1_ebx = find(target, 0x1, 0).map(|e| e.ebx);
    target.retain(|e| !FEATURE_LEAVES.contains(&e.function));
    for entry in profile
        .as_slice()
        .iter()
        .filter(|e| FEATURE_LEAVES.contains(&e.function))
    {
        let mut entry = *entry;
        if entry.function == 0x1
            && let Some(ebx) = own_leaf1_ebx
        {
            entry.ebx = ebx;
        }
        target.push(entry)?;
    }
    Ok(())
}

// One row per register, in bit order, so the names line up with the SDM.
#[rustfmt::skip]
fn feature_name(function: u32, index: u32, reg: Reg, bit: u32) -> Option<&'static str> {
    if (function, index, reg) == (0xd, 1, Reg::Eax) {
        return ["xsaveopt", "xsavec", "xgetbv1", "xsaves", "xfd"].get(bit as usize).copied();
    }
    let table: &[&str; 32] = match (function, index, reg) {
        (0x1, 0, Reg::Ecx) => &[
            "pni", "pclmulqdq", "dtes64", "monitor", "ds_cpl", "vmx", "smx", "est", "tm2",
            "ssse3", "cid", "sdbg", "fma", "cx16", "xtpr", "pdcm", "", "pcid", "dca", "sse4_1",
            "sse4_2", "x2apic", "movbe", "popcnt", "tsc_deadline_timer", "aes", "xsave",
            "osxsave", "avx", "f16c", "rdrand", "hypervisor",
        ],
        (0x1, 0, Reg::Edx) => &[
            "fpu", "vme", "de", "pse", "tsc", "msr", "pae", "mce", "cx8", "apic", "", "sep",
            "mtrr", "pge", "mca", "cmov", "pat", "pse36", "pn", "clflush", "", "dts", "acpi",
            "mmx", "fxsr", "sse", "sse2", "ss", "ht", "tm", "ia64", "pbe",
        ],
        (0x7, 0, Reg::Ebx) => &[
            "fsgsbase", "tsc_adjust", "sgx", "bmi1", "hle", "avx2", "fdp_excptn_only", "smep",
            "bmi2", "erms", "invpcid", "rtm", "cqm", "zero_fcs_fds", "mpx", "rdt_a",
            "avx512f", "avx512dq", "rdseed", "adx", "smap", "avx512ifma", "", "clflushopt",
            "clwb", "intel_pt", "avx512pf", "avx512er", "avx512cd", "sha_ni", "avx512bw",
            "avx512vl",
        ],
        (0x7, 0, Reg::Ecx) => &[
            "prefetchwt1", "avx512vbmi", "umip", "pku", "ospke", "waitpkg", "avx512_vbmi2",
            "cet_ss", "gfni", "vaes", "vpclmulqdq", "avx512_vnni", "avx512_bitalg", "tme",
            "avx512_vpopcntdq", "", "la57", "", "", "", "", "", "rdpid", "keylocker",
            "bus_lock_detect", "cldemote", "", "movdiri", "movdir64b", "enqcmd", "sgx_lc",
            "pks",
        ],
        (0x7, 0, Reg::Edx) => &[
            "", "sgx_keys", "avx512_4vnniw", "avx512_4fmaps", "fsrm", "uintr", "", "",
            "avx512_vp2intersect", "srbds_ctrl", "md_clear", "rtm_always_abort", "",
            "tsx_force_abort", "serialize", "hybrid_cpu", "tsxldtrk", "", "pconfig", "arch_lbr",
            "ibt", "", "amx_bf16", "avx512_fp16", "amx_tile", "amx_int8", "spec_ctrl",
            "intel_stibp", "flush_l1d", "arch_capabilities", "core_capabilities", "ssbd",
        ],
        (0x7, 1, Reg::Eax) => &[
            "", "", "", "", "avx_vnni", "avx512_bf16", "", "cmpccxadd", "", "", "fzrm", "fsrs",
            "fsrc", "", "", "", "", "fred", "lkgs", "wrmsrns", "", "amx_fp16", "hreset",
            "avx_ifma", "", "", "lam", "msrlist", "", "", "", "",
        ],
        _ => return None,
    };
    let name = table[bit as usize];
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(function: u32, index: u32, regs: [u32; 4]) -> kvm_cpuid_entry2 {
        kvm_cpuid_entry2 {
            function,
            index,
            flags: 0,
            eax: regs[0],
            ebx: regs[1],
            ecx: regs[2],
            edx: regs[3],
            padding: [0; 3],
        }
    }

    const SHA: u32 = 1 << 29;
    const AVX2: u32 = 1 << 5;

    fn cpuid(leaf7_ebx: u32) -> CpuId {
        CpuId::from_entries(&[
            entry(0x1, 0, [0, 0, 1 << 28, 1 << 25]),
            entry(0x7, 0, [2, leaf7_ebx, 0, 0]),
            entry(0xd, 0, [0b111, 0, 0, 0]),
        ])
        .unwrap()
    }

    #[test]
    fn a_host_with_every_recorded_feature_is_accepted() {
        let guest = cpuid(SHA | AVX2);
        assert!(missing_features(&guest, &cpuid(SHA | AVX2)).is_empty());
        // Extra host features are harmless: the guest already chose.
        assert!(missing_features(&cpuid(AVX2), &cpuid(SHA | AVX2)).is_empty());
    }

    #[test]
    fn a_checkpoint_with_sha_is_refused_by_a_host_without_it() {
        let missing = missing_features(&cpuid(SHA | AVX2), &cpuid(AVX2));
        assert_eq!(
            missing,
            [MissingFeature {
                function: 7,
                index: 0,
                reg: Reg::Ebx,
                bit: 29
            }]
        );
        assert_eq!(
            missing[0].to_string(),
            "sha_ni (CPUID.(EAX=0x7,ECX=0):EBX[29])"
        );
    }

    #[test]
    fn a_checkpoint_from_the_old_mask_restores_on_a_sha_host() {
        // Captured before SHA was exposed: the recorded CPUID lacks it, and a
        // host that has it can run that guest unchanged.
        assert!(missing_features(&cpuid(AVX2), &cpuid(SHA | AVX2)).is_empty());
    }

    #[test]
    fn a_leaf_the_host_does_not_report_at_all_is_missing_entirely() {
        let guest = CpuId::from_entries(&[entry(0x7, 1, [1 << 4, 0, 0, 0])]).unwrap();
        let missing = missing_features(&guest, &cpuid(0));
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].name(), Some("avx_vnni"));
    }

    #[test]
    fn bits_libkrun_or_kvm_derive_per_vm_are_not_host_requirements() {
        let derived = CpuId::from_entries(&[
            entry(0x1, 0, [0, 0, (1 << 24) | (1 << 27) | (1 << 31), 1 << 28]),
            entry(0x7, 0, [0, 0, 1 << 4, 0]),
        ])
        .unwrap();
        let bare = CpuId::from_entries(&[entry(0x1, 0, [0; 4]), entry(0x7, 0, [0; 4])]).unwrap();
        assert!(missing_features(&derived, &bare).is_empty());
    }

    #[test]
    fn unnamed_bits_are_still_reported_by_location() {
        let unnamed = MissingFeature {
            function: 7,
            index: 0,
            reg: Reg::Ebx,
            bit: 22,
        };
        assert_eq!(unnamed.to_string(), "(CPUID.(EAX=0x7,ECX=0):EBX[22])");
    }

    /// A Sapphire Rapids host's KVM table as one kernel reports it.
    fn spr_kvm_table(md_clear: bool, amx_tile: bool, sha: bool) -> CpuId {
        const AVX512F: u32 = 1 << 16;
        let ebx = AVX2 | AVX512F | (1 << 19) | (1 << 18) | if sha { SHA } else { 0 };
        let edx = (1 << 26)
            | (1 << 29)
            | if md_clear { 1 << 10 } else { 0 }
            | if amx_tile { 1 << 24 } else { 0 };
        CpuId::from_entries(&[
            entry(0x1, 0, [0, 0, (1 << 25) | (1 << 28), 1 << 25]),
            entry(0x7, 0, [2, ebx, (1 << 9) | (1 << 22), edx]),
            entry(0xd, 0, [0x602e7, 0, 0, 0]),
        ])
        .unwrap()
    }

    #[test]
    fn a_spill_between_kernels_that_differ_only_in_unchecked_bits_is_accepted() {
        // The guest booted where KVM reported MD_CLEAR and AMX_TILE (an old
        // T2-era checkpoint carries AMX_TILE too); the destination's kernel
        // reports neither. Neither is an instruction the guest can run.
        let source = spr_kvm_table(true, true, true);
        let destination = spr_kvm_table(false, false, true);
        assert_ne!(
            find(&source, 7, 0).unwrap().edx,
            find(&destination, 7, 0).unwrap().edx
        );
        assert!(missing_features(&source, &destination).is_empty());
        assert_eq!(missing_xcr0(0b111, &destination), 0);
    }

    #[test]
    fn a_destination_without_sha_ni_is_refused() {
        let source = spr_kvm_table(true, true, true);
        let destination = spr_kvm_table(true, true, false);
        let missing = missing_features(&source, &destination);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].name(), Some("sha_ni"));
    }

    #[test]
    fn a_vcpu_added_after_restore_takes_the_restored_features_but_keeps_its_topology() {
        // The restored guest booted without SHA; this libkrun's profile would
        // now offer it to a new vCPU.
        let restored = CpuId::from_entries(&[
            entry(0x1, 0, [0x306f2, 0x0001_0800, 1 << 25, 1]),
            entry(0x7, 0, [0, AVX2, 0, 0]),
            entry(0xd, 0, [0b111, 0, 832, 0]),
            entry(0xd, 2, [256, 576, 0, 0]),
            entry(0xb, 0, [0, 1, 0x100, 0]),
        ])
        .unwrap();
        let mut fresh = CpuId::from_entries(&[
            entry(0x1, 0, [0x306f2, 0x0301_0800, 1 << 25, 1]),
            entry(0x7, 0, [0, AVX2 | SHA, 0, 0]),
            entry(0x7, 1, [1 << 4, 0, 0, 0]),
            entry(0xd, 0, [0b111, 0, 832, 0]),
            entry(0xb, 0, [0, 1, 0x100, 3]),
        ])
        .unwrap();

        adopt_feature_leaves(&mut fresh, &restored).unwrap();

        let leaf7 = find(&fresh, 0x7, 0).unwrap();
        assert_eq!(leaf7.ebx, AVX2, "no SHA the restored guest was never given");
        assert!(
            find(&fresh, 0x7, 1).is_none(),
            "no subleaf the guest lacked"
        );
        assert!(find(&fresh, 0xd, 2).is_some());
        let leaf1 = find(&fresh, 0x1, 0).unwrap();
        assert_eq!(leaf1.ebx >> 24, 3, "the new vCPU keeps its own APIC ID");
        assert_eq!(find(&fresh, 0xb, 0).unwrap().edx, 3, "topology untouched");
        assert!(missing_features(&fresh, &restored).is_empty());
    }

    #[test]
    fn xcr0_components_the_host_cannot_hold_are_reported() {
        let host = cpuid(0);
        assert_eq!(missing_xcr0(0b111, &host), 0);
        // AVX-512 opmask/ZMM state enabled by a guest on another host.
        assert_eq!(missing_xcr0(0b1110_0111, &host), 0b1110_0000);
    }
}
