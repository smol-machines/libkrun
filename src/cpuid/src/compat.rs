//! Whether a saved guest CPU can keep running on this host.
//!
//! A guest probes its CPU once, at boot, and keeps the answers: which
//! instructions to use and which speculation mitigations to skip. A checkpoint
//! therefore carries those conclusions to whatever host restores it. KVM does
//! not compare the restored CPUID with the host (`KVM_SET_CPUID2` accepts any
//! bits), so the check lives here: every feature or "not affected" bit the guest
//! was given must also be given to a fresh guest on this host.

use kvm_bindings::{CpuId, kvm_cpuid_entry2};

#[derive(Clone, Copy, Debug)]
enum Reg {
    Eax,
    Ebx,
    Ecx,
    Edx,
}

struct Checked {
    leaf: u32,
    index: u32,
    reg: Reg,
    /// Bits KVM derives from guest state rather than from the CPU.
    ignore: u32,
}

const CHECKED: &[Checked] = &[
    // OSXSAVE mirrors the guest's CR4.OSXSAVE.
    Checked {
        leaf: 0x1,
        index: 0,
        reg: Reg::Ecx,
        ignore: 1 << 27,
    },
    Checked {
        leaf: 0x1,
        index: 0,
        reg: Reg::Edx,
        ignore: 0,
    },
    Checked {
        leaf: 0x7,
        index: 0,
        reg: Reg::Ebx,
        ignore: 0,
    },
    // OSPKE mirrors the guest's CR4.PKE.
    Checked {
        leaf: 0x7,
        index: 0,
        reg: Reg::Ecx,
        ignore: 1 << 4,
    },
    Checked {
        leaf: 0x7,
        index: 0,
        reg: Reg::Edx,
        ignore: 0,
    },
    Checked {
        leaf: 0x7,
        index: 1,
        reg: Reg::Eax,
        ignore: 0,
    },
    Checked {
        leaf: 0xd,
        index: 1,
        reg: Reg::Eax,
        ignore: 0,
    },
    Checked {
        leaf: 0x8000_0001,
        index: 0,
        reg: Reg::Ecx,
        ignore: 0,
    },
    Checked {
        leaf: 0x8000_0001,
        index: 0,
        reg: Reg::Edx,
        ignore: 0,
    },
    Checked {
        leaf: 0x8000_0008,
        index: 0,
        reg: Reg::Ebx,
        ignore: 0,
    },
    Checked {
        leaf: 0x8000_0021,
        index: 0,
        reg: Reg::Eax,
        ignore: 0,
    },
];

/// Names for the bits most likely to be the reason a restore is refused.
fn bit_name(leaf: u32, index: u32, reg: Reg, bit: u32) -> Option<&'static str> {
    Some(match (leaf, index, reg, bit) {
        (0x7, 0, Reg::Ebx, 5) => "AVX2",
        (0x7, 0, Reg::Ebx, 16) => "AVX512F",
        (0x7, 0, Reg::Ebx, 17) => "AVX512DQ",
        (0x7, 0, Reg::Ebx, 29) => "SHA",
        (0x7, 0, Reg::Ebx, 30) => "AVX512BW",
        (0x7, 0, Reg::Ebx, 31) => "AVX512VL",
        (0x7, 0, Reg::Ecx, 8) => "GFNI",
        (0x7, 0, Reg::Ecx, 9) => "VAES",
        (0x7, 0, Reg::Ecx, 10) => "VPCLMULQDQ",
        (0x7, 0, Reg::Ecx, 11) => "AVX512_VNNI",
        (0x7, 0, Reg::Edx, 8) => "AVX512_VP2INTERSECT",
        (0x7, 0, Reg::Edx, 10) => "MD_CLEAR",
        (0x7, 0, Reg::Edx, 26) => "SPEC_CTRL",
        (0x7, 0, Reg::Edx, 27) => "STIBP",
        (0x7, 0, Reg::Edx, 28) => "FLUSH_L1D",
        (0x7, 0, Reg::Edx, 29) => "ARCH_CAPABILITIES",
        (0x7, 0, Reg::Edx, 31) => "SSBD",
        (0x7, 1, Reg::Eax, 4) => "AVX_VNNI",
        (0x7, 1, Reg::Eax, 5) => "AVX512_BF16",
        (0x8000_0008, 0, Reg::Ebx, 12) => "IBPB",
        (0x8000_0008, 0, Reg::Ebx, 14) => "IBRS",
        (0x8000_0008, 0, Reg::Ebx, 15) => "AMD_STIBP",
        (0x8000_0008, 0, Reg::Ebx, 24) => "AMD_SSBD",
        (0x8000_0008, 0, Reg::Ebx, 25) => "VIRT_SSBD",
        (0x8000_0008, 0, Reg::Ebx, 26) => "SSB_NO",
        (0x8000_0008, 0, Reg::Ebx, 29) => "BTC_NO",
        (0x8000_0021, 0, Reg::Eax, 2) => "LFENCE_RDTSC",
        (0x8000_0021, 0, Reg::Eax, 6) => "NULL_SEL_CLR_BASE",
        (0x8000_0021, 0, Reg::Eax, 8) => "AUTOIBRS",
        (0x8000_0021, 0, Reg::Eax, 27) => "SBPB",
        (0x8000_0021, 0, Reg::Eax, 28) => "IBPB_BRTYPE",
        (0x8000_0021, 0, Reg::Eax, 29) => "SRSO_NO",
        (0x8000_0021, 0, Reg::Eax, 30) => "SRSO_USER_KERNEL_NO",
        (0x8000_0021, 0, Reg::Eax, 31) => "SRSO_MSR_FIX",
        _ => return None,
    })
}

fn entry(cpuid: &CpuId, leaf: u32, index: u32) -> Option<&kvm_cpuid_entry2> {
    cpuid
        .as_slice()
        .iter()
        .find(|e| e.function == leaf && e.index == index)
}

fn reg(entry: Option<&kvm_cpuid_entry2>, reg: Reg) -> u32 {
    entry.map_or(0, |e| match reg {
        Reg::Eax => e.eax,
        Reg::Ebx => e.ebx,
        Reg::Ecx => e.ecx,
        Reg::Edx => e.edx,
    })
}

/// CPUID features the saved guest was given that this host's guest profile
/// lacks, by name.
pub fn missing_features(saved: &CpuId, host: &CpuId) -> Vec<String> {
    let mut missing = Vec::new();
    for c in CHECKED {
        let want = reg(entry(saved, c.leaf, c.index), c.reg) & !c.ignore;
        let have = reg(entry(host, c.leaf, c.index), c.reg);
        let lacking = want & !have;
        for bit in (0..32).filter(|bit| lacking & (1 << bit) != 0) {
            missing.push(bit_name(c.leaf, c.index, c.reg, bit).map_or_else(
                || format!("CPUID.{:#x}.{}.{:?}[{bit}]", c.leaf, c.index, c.reg),
                str::to_string,
            ));
        }
    }
    missing
}

/// `IA32_ARCH_CAPABILITIES`.
pub const MSR_IA32_ARCH_CAPABILITIES: u32 = 0x10a;

/// Bits that report the CPU *is* affected; every other bit reports immunity
/// or an available control.
const ARCH_CAP_AFFECTED: u64 = (1 << 2) | (1 << 19); // RSBA, RRSBA

fn arch_cap_name(bit: u32) -> Option<&'static str> {
    Some(match bit {
        0 => "RDCL_NO",
        1 => "IBRS_ALL",
        2 => "RSBA",
        3 => "SKIP_VMENTRY_L1DFLUSH",
        4 => "SSB_NO",
        5 => "MDS_NO",
        6 => "PSCHANGE_MC_NO",
        7 => "TSX_CTRL_MSR",
        8 => "TAA_NO",
        13 => "SBDR_SSDP_NO",
        14 => "FBSDP_NO",
        15 => "PSDP_NO",
        17 => "FB_CLEAR",
        18 => "FB_CLEAR_CTRL",
        19 => "RRSBA",
        20 => "BHI_NO",
        21 => "XAPIC_DISABLE",
        24 => "PBRSB_NO",
        25 => "GDS_CTRL",
        26 => "GDS_NO",
        27 => "RFDS_NO",
        28 => "RFDS_CLEAR",
        62 => "ITS_NO",
        _ => return None,
    })
}

/// `IA32_ARCH_CAPABILITIES` conclusions the saved guest drew that do not hold
/// on this host: an immunity or control it had and the host lacks, or an
/// "affected" bit the host sets and the guest never saw.
pub fn arch_capabilities_conflicts(saved: u64, host: u64) -> Vec<String> {
    let conflicts = (saved & !host & !ARCH_CAP_AFFECTED) | (host & !saved & ARCH_CAP_AFFECTED);
    (0..64)
        .filter(|bit| conflicts & (1 << bit) != 0)
        .map(|bit| {
            arch_cap_name(bit).map_or_else(|| format!("ARCH_CAPABILITIES[{bit}]"), str::to_string)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cpuid(entries: &[(u32, u32, [u32; 4])]) -> CpuId {
        let list: Vec<kvm_cpuid_entry2> = entries
            .iter()
            .map(
                |&(function, index, [eax, ebx, ecx, edx])| kvm_cpuid_entry2 {
                    function,
                    index,
                    eax,
                    ebx,
                    ecx,
                    edx,
                    ..Default::default()
                },
            )
            .collect();
        CpuId::from_entries(&list).unwrap()
    }

    #[test]
    fn identical_cpus_are_compatible() {
        let c = cpuid(&[
            (0x7, 0, [0, 1 << 5, 0, 1 << 26]),
            (0x8000_0021, 0, [1 << 30, 0, 0, 0]),
        ]);
        assert!(missing_features(&c, &c).is_empty());
    }

    #[test]
    fn a_missing_immunity_is_named() {
        let zen5 = cpuid(&[(0x8000_0021, 0, [(1 << 30) | (1 << 8), 0, 0, 0])]);
        let zen3 = cpuid(&[(0x8000_0021, 0, [0, 0, 0, 0])]);
        assert_eq!(
            missing_features(&zen5, &zen3),
            ["AUTOIBRS", "SRSO_USER_KERNEL_NO"]
        );
        // A newer destination is a superset, so the move is fine.
        assert!(missing_features(&zen3, &zen5).is_empty());
    }

    #[test]
    fn a_leaf_the_host_lacks_counts_as_zero() {
        let saved = cpuid(&[(0x8000_0021, 0, [1 << 29, 0, 0, 0])]);
        let host = cpuid(&[(0x1, 0, [0; 4])]);
        assert_eq!(missing_features(&saved, &host), ["SRSO_NO"]);
    }

    #[test]
    fn guest_derived_bits_are_ignored() {
        let saved = cpuid(&[(0x1, 0, [0, 0, 1 << 27, 0]), (0x7, 0, [0, 0, 1 << 4, 0])]);
        let host = cpuid(&[(0x1, 0, [0; 4]), (0x7, 0, [0; 4])]);
        assert!(missing_features(&saved, &host).is_empty());
    }

    #[test]
    fn arch_capabilities_compare_by_meaning() {
        let mds_no = 1 << 5;
        let rsba = 1 << 2;
        // Immunity the destination lacks.
        assert_eq!(arch_capabilities_conflicts(mds_no, 0), ["MDS_NO"]);
        // The destination is affected where the source was not.
        assert_eq!(arch_capabilities_conflicts(0, rsba), ["RSBA"]);
        // The guest already assumed RSBA; a host without it is fine.
        assert!(arch_capabilities_conflicts(rsba, 0).is_empty());
        assert!(arch_capabilities_conflicts(mds_no, mds_no | (1 << 26)).is_empty());
    }
}
