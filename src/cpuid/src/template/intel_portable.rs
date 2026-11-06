// Copyright 2026 The libkrun Authors
// SPDX-License-Identifier: Apache-2.0

//! libkrun's portable Intel CPU profile, the one selected by
//! `krun_set_cpu_template(ctx, KRUN_CPU_TEMPLATE_PORTABLE)`.
//!
//! A VM booted under this profile can be checkpointed on one Intel KVM host
//! and restored on another. Two rules decide what the guest sees:
//!
//! 1. **No extra saved state.** Every feature the guest may use is either a
//!    plain instruction or uses state the checkpoint already carries (general
//!    registers, x87/SSE/AVX in XSAVE, control registers, and the MSRs KVM
//!    lists). Anything that adds an XSAVE state component (MPX, AVX-512, PT,
//!    PKRU, PASID, CET, UINTR, arch LBR, AMX, APX) stays hidden, and so does
//!    the XSAVE machinery that only exists for such state (XSAVES/XSS,
//!    XSAVEC, XGETBV with ECX=1, XFD).
//! 2. **Every host we run.** The feature exists on every Intel host the
//!    profile is meant to move between (Sapphire Rapids in production).
//!    Host-specific facilities (SGX, TSX, PT, RDT, protection keys, Key
//!    Locker, TME, hybrid topology) stay hidden.
//!
//! The profile is a cap, not a floor: a bit the host's KVM does not report
//! stays clear, so the same profile runs on older development machines with
//! fewer features. A checkpoint records the exact CPUID its guest was given,
//! and restore refuses a host that lacks one of its instructions (see
//! [`required`] and [`crate::compat::missing_features`]).
//!
//! Leaves 7 and 0xD gain new bits with every CPU generation, so they are
//! allowlists: a bit not listed here stays hidden even if a future host
//! reports it. Leaves 1 and 0x8000_0001 are long complete; they keep the
//! Haswell-era masking this profile has always applied.
//!
//! # Baseline: CPUID.(EAX=7,ECX=0)
//!
//! EBX: FSGSBASE, TSC_ADJUST, BMI1, AVX2, SMEP, BMI2, ERMS, INVPCID,
//! FPU CS/DS deprecation, **RDSEED**, **ADX**, SMAP, **CLFLUSHOPT**,
//! **CLWB**, **SHA**.
//!
//! ECX: UMIP, WAITPKG, GFNI, VAES, VPCLMULQDQ, LA57, **RDPID**,
//! BUS_LOCK_DETECT, CLDEMOTE, MOVDIRI, MOVDIR64B.
//!
//! EDX: FSRM, SRBDS_CTRL, MD_CLEAR, SERIALIZE, SPEC_CTRL, STIBP, FLUSH_L1D,
//! ARCH_CAPABILITIES, CORE_CAPABILITIES, SSBD.
//!
//! The bold ones were hidden by the Firecracker T2 mask this profile used to
//! reuse. None of them adds state:
//!
//! - SHA (SHA1RNDS4, SHA1NEXTE, SHA1MSG1/2, SHA256RNDS2, SHA256MSG1/2) works
//!   on XMM registers, which SSE state already holds (Intel SDM Vol. 2,
//!   "SHA Extensions").
//! - ADX (ADCX, ADOX) works on general registers and the CF/OF flags.
//! - RDSEED reads the hardware entropy source into a general register.
//! - CLFLUSHOPT and CLWB write back cache lines; they hold no state.
//! - RDPID reads IA32_TSC_AUX, which the guest already has through RDTSCP
//!   and which the checkpoint already saves with the other MSRs.
//!
//! # Baseline: CPUID.(EAX=7,ECX=1) and (EAX=7,ECX=2)
//!
//! Subleaf 1 EAX: AVX-VNNI (VEX-encoded, YMM state only), FZRM, FSRS, FSRC.
//! Subleaf 2 EDX: the speculation controls PSFD, IPRED_CTRL, RRSBA_CTRL,
//! DDPD_U, BHI_CTRL, MCDT_NO. Everything else in these subleaves is hidden,
//! including AVX512_BF16, AMX-FP16, LAM, FRED, AVX10 and APX.
//!
//! # Baseline: CPUID.(EAX=0xD)
//!
//! XCR0 may hold x87, SSE and AVX only (subleaf 0 EAX bits 0-2, EDX zero),
//! and subleaf 0 ECX is the standard-format size of those three, 832 bytes
//! (576 without AVX). Subleaf 1 keeps XSAVEOPT and drops XSAVEC,
//! XGETBV1, XSAVES and XFD, with no supervisor (XSS) components. Subleaves
//! for components the guest cannot enable read as zero, as the SDM specifies
//! for invalid subleaves.

use crate::bit_helper::BitHelper;
use crate::cpu_leaf::*;
use crate::transformer::*;
use kvm_bindings::{CpuId, kvm_cpuid_entry2};

const fn bits(list: &[u32]) -> u32 {
    let mut mask = 0;
    let mut i = 0;
    while i < list.len() {
        mask |= 1 << list[i];
        i += 1;
    }
    mask
}

/// CPUID.(EAX=7,ECX=0):EBX bits the guest may see.
pub const LEAF7_0_EBX: u32 = bits(&[
    0,  // FSGSBASE: FS/GS base live in the saved segment registers
    1,  // TSC_ADJUST: IA32_TSC_ADJUST, emulated by KVM and saved
    3,  // BMI1
    5,  // AVX2: YMM state
    7,  // SMEP: CR4 bit, saved in sregs
    8,  // BMI2
    9,  // ERMS: REP MOVSB/STOSB performance hint
    10, // INVPCID
    13, // FPU CS/DS deprecated: informational
    18, // RDSEED
    19, // ADX
    20, // SMAP: CR4 bit, saved in sregs
    23, // CLFLUSHOPT
    24, // CLWB
    29, // SHA
]);

/// CPUID.(EAX=7,ECX=0):ECX bits the guest may see.
pub const LEAF7_0_ECX: u32 = bits(&[
    2,  // UMIP: CR4 bit, saved in sregs
    5,  // WAITPKG: IA32_UMWAIT_CONTROL, saved with the other MSRs
    8,  // GFNI: legacy and VEX forms use XMM/YMM state
    9,  // VAES: VEX form uses YMM state
    10, // VPCLMULQDQ: VEX form uses YMM state
    16, // LA57: CR4 bit, saved in sregs
    22, // RDPID: reads IA32_TSC_AUX, already saved
    24, // BUS_LOCK_DETECT: a DEBUGCTL bit, saved with the other MSRs
    25, // CLDEMOTE: cache hint
    27, // MOVDIRI
    28, // MOVDIR64B
]);

/// CPUID.(EAX=7,ECX=0):EDX bits the guest may see.
pub const LEAF7_0_EDX: u32 = bits(&[
    4,  // FSRM: fast short REP MOVSB
    9,  // SRBDS_CTRL
    10, // MD_CLEAR: VERW clears CPU buffers
    14, // SERIALIZE
    26, // SPEC_CTRL: IBRS/IBPB, IA32_SPEC_CTRL is saved
    27, // STIBP
    28, // FLUSH_L1D
    29, // ARCH_CAPABILITIES: emulated by KVM
    30, // CORE_CAPABILITIES: emulated by KVM
    31, // SSBD
]);

/// CPUID.(EAX=7,ECX=1):EAX bits the guest may see.
pub const LEAF7_1_EAX: u32 = bits(&[
    4,  // AVX-VNNI: VEX-encoded, YMM state
    10, // FZRM: fast zero-length REP MOVSB
    11, // FSRS: fast short REP STOSB
    12, // FSRC: fast short REP CMPSB/SCASB
]);

/// CPUID.(EAX=7,ECX=2):EDX bits the guest may see: speculation controls,
/// all of them bits of the already saved IA32_SPEC_CTRL.
pub const LEAF7_2_EDX: u32 = bits(&[
    0, // PSFD
    1, // IPRED_CTRL
    2, // RRSBA_CTRL
    3, // DDPD_U
    4, // BHI_CTRL
    5, // MCDT_NO
]);

/// The instructions among the features above, with the leaf-1 instruction
/// sets the profile passes through: what a guest can execute and would take
/// #UD on if a host lacked it. Restore requires exactly these of the
/// destination (see [`crate::compat::missing_features`]).
///
/// The rest of the allowlist is left out on purpose: performance hints
/// (ERMS, FSRM, FZRM, ...), facilities KVM emulates on any host (TSC_ADJUST,
/// UMIP, ARCH_CAPABILITIES, CORE_CAPABILITIES, BUS_LOCK_DETECT), CLDEMOTE
/// (a hint that executes as a NOP where unsupported), LA57 (KVM refuses the
/// control register itself), and the speculation-control bits (MD_CLEAR,
/// SPEC_CTRL, the leaf 7.2 controls), which follow microcode and the host
/// kernel and differ between otherwise identical hosts.
pub mod required {
    use super::bits;

    /// CPUID.(EAX=1):ECX: SSE3, PCLMULQDQ, SSSE3, FMA, CX16, SSE4.1, SSE4.2,
    /// MOVBE, POPCNT, AES, XSAVE, AVX, F16C, RDRAND.
    pub const LEAF1_ECX: u32 = bits(&[0, 1, 9, 12, 13, 19, 20, 22, 23, 25, 26, 28, 29, 30]);
    /// CPUID.(EAX=1):EDX: FPU, TSC, CX8, CMOV, CLFLUSH, MMX, FXSR, SSE, SSE2.
    pub const LEAF1_EDX: u32 = bits(&[0, 4, 8, 15, 19, 23, 24, 25, 26]);
    /// CPUID.(EAX=7,ECX=0):EBX: FSGSBASE, BMI1, AVX2, SMEP, BMI2, INVPCID,
    /// RDSEED, ADX, SMAP, CLFLUSHOPT, CLWB, SHA.
    pub const LEAF7_0_EBX: u32 = bits(&[0, 3, 5, 7, 8, 10, 18, 19, 20, 23, 24, 29]);
    /// CPUID.(EAX=7,ECX=0):ECX: WAITPKG, GFNI, VAES, VPCLMULQDQ, RDPID,
    /// MOVDIRI, MOVDIR64B.
    pub const LEAF7_0_ECX: u32 = bits(&[5, 8, 9, 10, 22, 27, 28]);
    /// CPUID.(EAX=7,ECX=0):EDX: SERIALIZE.
    pub const LEAF7_0_EDX: u32 = bits(&[14]);
    /// CPUID.(EAX=7,ECX=1):EAX: AVX-VNNI.
    pub const LEAF7_1_EAX: u32 = bits(&[4]);
}

/// XSAVE state components the guest may enable in XCR0: x87, SSE and AVX.
pub const XCR0: u64 = 0b111;

/// CPUID.(EAX=0xD,ECX=1):EAX bits the guest may see: XSAVEOPT only.
pub const LEAF_D_1_EAX: u32 = bits(&[0]);

/// Standard-format XSAVE area for x87 and SSE: the 512-byte legacy region
/// plus the 64-byte header.
const XSAVE_LEGACY_AND_HEADER: u32 = 576;
/// Standard-format XSAVE area once AVX is added: the AVX component sits at
/// the architecturally fixed offset 576 and is 256 bytes long.
const XSAVE_WITH_AVX: u32 = 832;
const XCR0_AVX_BIT: u32 = 2;

fn update_feature_info_entry(entry: &mut kvm_cpuid_entry2, _vm_spec: &VmSpec) -> Result<(), Error> {
    use crate::cpu_leaf::leaf_0x1::*;

    // A fixed family/model/stepping (Haswell, 06_3F_2) so the guest kernel
    // applies the same model quirks on every host.
    entry
        .eax
        .write_bits_in_range(&eax::EXTENDED_FAMILY_ID_BITRANGE, 0)
        .write_bits_in_range(&eax::EXTENDED_PROCESSOR_MODEL_BITRANGE, 3)
        .write_bits_in_range(&eax::PROCESSOR_TYPE_BITRANGE, 0)
        .write_bits_in_range(&eax::PROCESSOR_FAMILY_BITRANGE, 6)
        .write_bits_in_range(&eax::PROCESSOR_MODEL_BITRANGE, 15)
        .write_bits_in_range(&eax::STEPPING_BITRANGE, 2);

    entry
        .ecx
        .write_bit(ecx::DTES64_BITINDEX, false)
        .write_bit(ecx::MONITOR_BITINDEX, false)
        .write_bit(ecx::DS_CPL_SHIFT, false)
        .write_bit(ecx::TM2_BITINDEX, false)
        .write_bit(ecx::CNXT_ID_BITINDEX, false)
        .write_bit(ecx::SDBG_BITINDEX, false)
        .write_bit(ecx::XTPR_UPDATE_BITINDEX, false)
        .write_bit(ecx::PDCM_BITINDEX, false)
        .write_bit(ecx::OSXSAVE_BITINDEX, false);

    entry
        .edx
        .write_bit(edx::PSN_BITINDEX, false)
        .write_bit(edx::DS_BITINDEX, false)
        .write_bit(edx::ACPI_BITINDEX, false)
        .write_bit(edx::SS_BITINDEX, false)
        .write_bit(edx::TM_BITINDEX, false)
        .write_bit(edx::PBE_BITINDEX, false);

    Ok(())
}

fn update_structured_extended_entry(
    entry: &mut kvm_cpuid_entry2,
    _vm_spec: &VmSpec,
) -> Result<(), Error> {
    match entry.index {
        // EAX is the highest valid subleaf; KVM already caps it.
        0 => {
            entry.ebx &= LEAF7_0_EBX;
            entry.ecx &= LEAF7_0_ECX;
            entry.edx &= LEAF7_0_EDX;
        }
        1 => {
            entry.eax &= LEAF7_1_EAX;
            entry.ebx = 0;
            entry.ecx = 0;
            entry.edx = 0;
        }
        2 => {
            entry.eax = 0;
            entry.ebx = 0;
            entry.ecx = 0;
            entry.edx &= LEAF7_2_EDX;
        }
        _ => {
            entry.eax = 0;
            entry.ebx = 0;
            entry.ecx = 0;
            entry.edx = 0;
        }
    }
    Ok(())
}

fn update_xsave_features_entry(
    entry: &mut kvm_cpuid_entry2,
    _vm_spec: &VmSpec,
) -> Result<(), Error> {
    match entry.index {
        0 => {
            entry.eax &= XCR0 as u32;
            entry.edx &= (XCR0 >> 32) as u32;
            // ECX is the largest area any XCR0 this CPUID permits can need.
            // EBX tracks the guest's current XCR0, which KVM keeps up to date.
            entry.ecx = if entry.eax.read_bit(XCR0_AVX_BIT) {
                XSAVE_WITH_AVX
            } else {
                XSAVE_LEGACY_AND_HEADER
            };
        }
        1 => {
            entry.eax &= LEAF_D_1_EAX;
            // No supervisor components: IA32_XSS can only be zero.
            entry.ecx = 0;
            entry.edx = 0;
        }
        index => {
            let permitted = index < 64 && (XCR0 >> index) & 1 == 1;
            if !permitted {
                entry.eax = 0;
                entry.ebx = 0;
                entry.ecx = 0;
                entry.edx = 0;
            }
        }
    }
    Ok(())
}

fn update_extended_feature_info_entry(
    entry: &mut kvm_cpuid_entry2,
    _vm_spec: &VmSpec,
) -> Result<(), Error> {
    use crate::cpu_leaf::leaf_0x80000001::*;

    entry.ecx.write_bit(ecx::PREFETCH_BITINDEX, false);
    entry.edx.write_bit(edx::PDPE1GB_BITINDEX, false);

    Ok(())
}

struct IntelPortableCpuidTransformer;

impl CpuidTransformer for IntelPortableCpuidTransformer {
    fn entry_transformer_fn(&self, entry: &mut kvm_cpuid_entry2) -> Option<EntryTransformerFn> {
        match entry.function {
            leaf_0x1::LEAF_NUM => Some(update_feature_info_entry),
            leaf_0x7::LEAF_NUM => Some(update_structured_extended_entry),
            leaf_0xd::LEAF_NUM => Some(update_xsave_features_entry),
            leaf_0x80000001::LEAF_NUM => Some(update_extended_feature_info_entry),
            _ => None,
        }
    }
}

/// Apply the portable Intel profile to a vCPU's CPUID (see the module
/// documentation for exactly what the guest is left with).
pub fn set_cpuid_entries(kvm_cpuid: &mut CpuId, vm_spec: &VmSpec) -> Result<(), Error> {
    IntelPortableCpuidTransformer.process_cpuid(kvm_cpuid, vm_spec)
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

    /// A CPUID with every feature bit set, standing in for a host that has
    /// everything: whatever survives is exactly what the profile allows.
    fn everything() -> CpuId {
        let ones = [u32::MAX; 4];
        let mut entries = vec![
            entry(0x1, 0, ones),
            entry(0x7, 0, [2, u32::MAX, u32::MAX, u32::MAX]),
            entry(0x7, 1, ones),
            entry(0x7, 2, ones),
            entry(0xd, 0, [u32::MAX, 2696, 11008, u32::MAX]),
            entry(0xd, 1, ones),
            entry(0xd, 2, [256, 576, 0, 0]),
            entry(0x8000_0001, 0, ones),
        ];
        for component in 3..=19 {
            entries.push(entry(0xd, component, [64, 1024, 0, 0]));
        }
        CpuId::from_entries(&entries).unwrap()
    }

    fn masked(cpuid: &mut CpuId) {
        let vm_spec = VmSpec::new(0, 1, false, false).unwrap();
        set_cpuid_entries(cpuid, &vm_spec).unwrap();
    }

    fn find(cpuid: &CpuId, function: u32, index: u32) -> kvm_cpuid_entry2 {
        *cpuid
            .as_slice()
            .iter()
            .find(|e| e.function == function && e.index == index)
            .unwrap()
    }

    fn set_bits(value: u32) -> Vec<u32> {
        (0..32).filter(|bit| value & (1 << bit) != 0).collect()
    }

    #[test]
    fn exposes_exactly_the_leaf7_baseline() {
        let mut cpuid = everything();
        masked(&mut cpuid);

        let leaf = find(&cpuid, 0x7, 0);
        assert_eq!(leaf.eax, 2, "max subleaf is left to KVM");
        assert_eq!(
            set_bits(leaf.ebx),
            [0, 1, 3, 5, 7, 8, 9, 10, 13, 18, 19, 20, 23, 24, 29]
        );
        assert_eq!(set_bits(leaf.ecx), [2, 5, 8, 9, 10, 16, 22, 24, 25, 27, 28]);
        assert_eq!(set_bits(leaf.edx), [4, 9, 10, 14, 26, 27, 28, 29, 30, 31]);

        let sub1 = find(&cpuid, 0x7, 1);
        assert_eq!(set_bits(sub1.eax), [4, 10, 11, 12]);
        assert_eq!((sub1.ebx, sub1.ecx, sub1.edx), (0, 0, 0));

        let sub2 = find(&cpuid, 0x7, 2);
        assert_eq!((sub2.eax, sub2.ebx, sub2.ecx), (0, 0, 0));
        assert_eq!(set_bits(sub2.edx), [0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn keeps_stateless_features_the_t2_mask_hid() {
        use crate::cpu_leaf::leaf_0x7::index0::*;

        let mut cpuid = everything();
        masked(&mut cpuid);
        let leaf = find(&cpuid, 0x7, 0);
        for bit in [
            ebx::SHA_BITINDEX,
            ebx::ADX_BITINDEX,
            ebx::RDSEED_BITINDEX,
            ebx::CLFLUSHOPT_BITINDEX,
            ebx::CLWB_BITINDEX,
        ] {
            assert!(leaf.ebx.read_bit(bit), "EBX bit {bit} must be exposed");
        }
        assert!(leaf.ecx.read_bit(ecx::RDPID_BITINDEX));
    }

    #[test]
    fn hides_every_feature_that_adds_state_or_is_host_specific() {
        use crate::cpu_leaf::leaf_0x7::index0::*;

        let mut cpuid = everything();
        masked(&mut cpuid);
        let leaf = find(&cpuid, 0x7, 0);
        for bit in [
            ebx::SGX_BITINDEX,
            ebx::HLE_BITINDEX,
            ebx::FPDP_BITINDEX,
            ebx::RTM_BITINDEX,
            ebx::RDT_M_BITINDEX,
            ebx::MPX_BITINDEX,
            ebx::RDT_A_BITINDEX,
            ebx::AVX512F_BITINDEX,
            ebx::AVX512DQ_BITINDEX,
            ebx::AVX512IFMA_BITINDEX,
            ebx::PT_BITINDEX,
            ebx::AVX512PF_BITINDEX,
            ebx::AVX512ER_BITINDEX,
            ebx::AVX512CD_BITINDEX,
            ebx::AVX512BW_BITINDEX,
            ebx::AVX512VL_BITINDEX,
        ] {
            assert!(!leaf.ebx.read_bit(bit), "EBX bit {bit} must be hidden");
        }
        for bit in [
            0, // PREFETCHWT1
            ecx::AVX512_VBMI_BITINDEX,
            ecx::PKU_BITINDEX,
            ecx::OSPKE_BITINDEX,
            6,  // AVX512_VBMI2
            7,  // CET_SS
            11, // AVX512_VNNI
            12, // AVX512_BITALG
            13, // TME
            ecx::AVX512_VPOPCNTDQ_BITINDEX,
            23, // Key Locker
            29, // ENQCMD (PASID state)
            ecx::SGX_LC_BITINDEX,
            31, // PKS
        ] {
            assert!(!leaf.ecx.read_bit(bit), "ECX bit {bit} must be hidden");
        }
        for bit in [
            edx::AVX512_4VNNIW_BITINDEX,
            edx::AVX512_4FMAPS_BITINDEX,
            5,  // UINTR
            8,  // AVX512_VP2INTERSECT
            11, // RTM_ALWAYS_ABORT
            13, // TSX_FORCE_ABORT
            15, // HYBRID
            16, // TSXLDTRK
            18, // PCONFIG
            19, // ARCH_LBR
            20, // CET_IBT
            22, // AMX_BF16
            23, // AVX512_FP16
            24, // AMX_TILE
            25, // AMX_INT8
        ] {
            assert!(!leaf.edx.read_bit(bit), "EDX bit {bit} must be hidden");
        }
        let sub1 = find(&cpuid, 0x7, 1);
        for bit in [
            5,  /* AVX512_BF16 */
            21, /* AMX-FP16 */
            26, /* LAM */
        ] {
            assert!(!sub1.eax.read_bit(bit), "7.1 EAX bit {bit} must be hidden");
        }
        assert!(!sub1.edx.read_bit(21), "APX must be hidden");
    }

    #[test]
    fn xsave_leaf_matches_the_permitted_xcr0() {
        let mut cpuid = everything();
        masked(&mut cpuid);

        let sub0 = find(&cpuid, 0xd, 0);
        assert_eq!(sub0.eax, 0b111, "x87, SSE and AVX only");
        assert_eq!(sub0.edx, 0);
        assert_eq!(sub0.ecx, 832, "standard-format size of x87+SSE+AVX");
        assert_eq!(sub0.ebx, 2696, "the current-XCR0 size is KVM's to maintain");

        let sub1 = find(&cpuid, 0xd, 1);
        assert_eq!(sub1.eax, 1, "XSAVEOPT only");
        assert_eq!((sub1.ecx, sub1.edx), (0, 0), "no supervisor components");

        let avx = find(&cpuid, 0xd, 2);
        assert_eq!((avx.eax, avx.ebx), (256, 576), "AVX component kept");
        for component in 3..=19 {
            let leaf = find(&cpuid, 0xd, component);
            assert_eq!(
                (leaf.eax, leaf.ebx, leaf.ecx, leaf.edx),
                (0, 0, 0, 0),
                "component {component} must read as an invalid subleaf"
            );
        }
    }

    #[test]
    fn every_exposed_feature_has_its_state_in_xcr0() {
        // AVX2, AVX-VNNI and the VEX forms of VAES/VPCLMULQDQ/GFNI need YMM
        // state; nothing exposed needs more than x87|SSE|AVX.
        let mut cpuid = everything();
        masked(&mut cpuid);
        let leaf7 = find(&cpuid, 0x7, 0);
        let xcr0 = u64::from(find(&cpuid, 0xd, 0).eax);
        assert!(leaf7.ebx & (1 << 5) != 0 && xcr0 & 0b100 != 0);
        assert_eq!(xcr0 & !XCR0, 0);
    }

    #[test]
    fn restore_requires_only_exposed_instructions() {
        assert_eq!(required::LEAF7_0_EBX & !LEAF7_0_EBX, 0);
        assert_eq!(required::LEAF7_0_ECX & !LEAF7_0_ECX, 0);
        assert_eq!(required::LEAF7_0_EDX & !LEAF7_0_EDX, 0);
        assert_eq!(required::LEAF7_1_EAX & !LEAF7_1_EAX, 0);
        // Microcode- and kernel-dependent bits never decide a restore.
        assert_eq!(required::LEAF7_0_EDX & (1 << 10), 0, "MD_CLEAR");
        assert_eq!(required::LEAF7_0_EDX & (1 << 29), 0, "ARCH_CAPABILITIES");
        assert_eq!(required::LEAF7_0_EDX & (1 << 26), 0, "SPEC_CTRL");
    }

    #[test]
    fn a_host_without_a_feature_keeps_it_hidden() {
        let mut cpuid = everything();
        for e in cpuid.as_mut_slice() {
            if e.function == 0x7 && e.index == 0 {
                e.ebx &= !(1 << 29);
                e.ecx &= !(1 << 22);
            }
            if e.function == 0xd && e.index == 0 {
                e.eax &= !(1 << XCR0_AVX_BIT);
            }
        }
        masked(&mut cpuid);
        let leaf = find(&cpuid, 0x7, 0);
        assert!(!leaf.ebx.read_bit(29), "the profile never adds SHA");
        assert!(!leaf.ecx.read_bit(22), "the profile never adds RDPID");
        assert_eq!(find(&cpuid, 0xd, 0).ecx, 576);
    }

    #[test]
    fn leaf1_keeps_the_fixed_signature_and_masks() {
        use crate::cpu_leaf::leaf_0x1::*;

        let mut cpuid = everything();
        masked(&mut cpuid);
        let leaf = find(&cpuid, 0x1, 0);
        assert_eq!(leaf.eax & 0x0fff_3fff, 0x0003_06f2);
        assert!(!leaf.ecx.read_bit(ecx::OSXSAVE_BITINDEX));
        assert!(!leaf.ecx.read_bit(ecx::MONITOR_BITINDEX));
        assert!(!leaf.edx.read_bit(edx::DS_BITINDEX));
        let ext = find(&cpuid, 0x8000_0001, 0);
        assert!(!ext.edx.read_bit(26), "1 GiB pages stay hidden");
    }
}
