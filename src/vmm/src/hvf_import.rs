// SPDX-License-Identifier: Apache-2.0

//! Restore a checkpoint captured by the macOS (Hypervisor.framework) backend
//! on Linux/KVM.
//!
//! Guest RAM and virtio device state are hypervisor-neutral and restore as
//! they are. The VM and vCPU sections are HVF-shaped: HVF system-register
//! values, the HVF GIC's distributor/redistributor/CPU-interface registers,
//! and a virtual-timer offset against the Mac's own counter. This module
//! parses those sections and rewrites them as the KVM register and vGIC
//! attribute lists the native restore path replays. The board the guest
//! booted on (GIC placement, device addresses and interrupts) travels with
//! the result so the builder can recreate it.
//!
//! Time: the guest's virtual counter continues from the value it had at
//! capture. The new host's counter can tick at another rate; the guest kernel
//! follows that itself after its next timer interrupt, which the import makes
//! immediate by arming every enabled vCPU timer to fire on resume.

use std::mem::{offset_of, size_of};

use kvm_bindings::{
    KVM_REG_ARM_CORE, KVM_REG_ARM64, KVM_REG_ARM64_SYSREG, KVM_REG_SIZE_U32, KVM_REG_SIZE_U64,
    KVM_REG_SIZE_U128, kvm_regs, user_fpsimd_state, user_pt_regs,
};

use crate::arm_board::BoardLayout;
use crate::vstate::{VcpuState, VmState};
use crate::{CheckpointSections, VmCheckpoint};

/// KVM assigns MPIDR Aff0 = vCPU index only below 16; HVF uses the raw index.
const MAX_IMPORTED_VCPUS: usize = 16;
/// SPIs the KVM vGIC is created with (`IRQ_MAX - IRQ_BASE + 1` plus the 32
/// private interrupts): distributor state for higher INTIDs has no home.
const KVM_NR_IRQS: u32 = arch::aarch64::layout::IRQ_MAX - arch::aarch64::layout::IRQ_BASE + 1;

// HVF `hv_sys_reg_t` values are the architectural sysreg encodings.
const SYS_SP_EL0: u16 = 0xc208;
const SYS_SP_EL1: u16 = 0xe208;
const SYS_ELR_EL1: u16 = 0xc201;
const SYS_SPSR_EL1: u16 = 0xc200;
const SYS_SCTLR_EL1: u16 = 0xc080;
const SYS_ACTLR_EL1: u16 = 0xc081;
const SYS_CNTV_CTL_EL0: u16 = 0xdf19;
const SYS_CNTV_CVAL_EL0: u16 = 0xdf1a;
/// APIAKEYLO_EL1 ..= APGAKEYHI_EL1.
const SYS_PAC_KEYS: std::ops::RangeInclusive<u16> = 0xc108..=0xc119;

/// KVM's timer registers use legacy, non-architectural ids: the CNTVCT slot
/// carries the CNTV_CVAL encoding and vice versa (arch/arm64/include/uapi/asm/kvm.h).
const KVM_TIMER_CTL: u16 = 0xdf19; // (3, 3, 14, 3, 1)
const KVM_TIMER_CNT: u16 = 0xdf1a; // (3, 3, 14, 3, 2)
const KVM_TIMER_CVAL: u16 = 0xdf02; // (3, 3, 14, 0, 2)

const CNTV_CTL_ENABLE: u64 = 1;

/// SCTLR_EL1.{EnIA, EnIB, EnDA, EnDB}: pointer authentication in use.
const SCTLR_PAC_ENABLES: u64 = (1 << 31) | (1 << 30) | (1 << 27) | (1 << 13);

const KVM_MP_STATE_RUNNABLE: u32 = 0;

/// The macOS `VmState`: GIC distributor registers plus the board layout.
struct HvfVm {
    gic_distributor: Vec<(u32, u64)>,
    board: Option<BoardLayout>,
}

/// The macOS `HvfVcpuState` (see `hvf::HvfVcpuState::serialize`).
#[derive(Debug)]
struct HvfVcpu {
    gp: [u64; 31],
    pc: u64,
    cpsr: u64,
    fpcr: u64,
    fpsr: u64,
    simd: [u128; 32],
    sys: Vec<(u16, u64)>,
    gic_redist: Vec<(u32, u64)>,
    gic_icc: Vec<(u32, u64)>,
    guest_counter: u64,
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
    what: &'static str,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8], what: &'static str) -> Self {
        Reader {
            bytes,
            pos: 0,
            what,
        }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| format!("macOS {} blob truncated", self.what))?;
        let s = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn u128(&mut self) -> Result<u128, String> {
        Ok(u128::from_le_bytes(self.take(16)?.try_into().unwrap()))
    }
    fn more(&self) -> bool {
        self.pos < self.bytes.len()
    }
    fn u32_pairs(&mut self) -> Result<Vec<(u32, u64)>, String> {
        let n = self.u32()? as usize;
        let mut out = Vec::with_capacity(n.min(4096));
        for _ in 0..n {
            out.push((self.u32()?, self.u64()?));
        }
        Ok(out)
    }
}

impl HvfVm {
    fn parse(bytes: &[u8]) -> Result<Self, String> {
        if bytes.is_empty() {
            return Ok(HvfVm {
                gic_distributor: Vec::new(),
                board: None,
            });
        }
        let mut r = Reader::new(bytes, "VM state");
        let gic_distributor = r.u32_pairs()?;
        let board = if r.more() {
            let len = r.u32()? as usize;
            Some(BoardLayout::decode(r.take(len)?)?)
        } else {
            None
        };
        Ok(HvfVm {
            gic_distributor,
            board,
        })
    }
}

impl HvfVcpu {
    fn parse(bytes: &[u8]) -> Result<Self, String> {
        let mut r = Reader::new(bytes, "vCPU state");
        let mut gp = [0u64; 31];
        for slot in gp.iter_mut() {
            *slot = r.u64()?;
        }
        let pc = r.u64()?;
        let cpsr = r.u64()?;
        let fpcr = r.u64()?;
        let fpsr = r.u64()?;
        let mut simd = [0u128; 32];
        for slot in simd.iter_mut() {
            *slot = r.u128()?;
        }
        let n = r.u32()? as usize;
        let mut sys = Vec::with_capacity(n.min(256));
        for _ in 0..n {
            sys.push((r.u16()?, r.u64()?));
        }
        let gic_redist = r.u32_pairs()?;
        let gic_icc = r.u32_pairs()?;
        let _vtimer_offset = if r.more() { r.u64()? } else { 0 };
        let guest_counter = if r.more() { r.u64()? } else { 0 };
        Ok(HvfVcpu {
            gp,
            pc,
            cpsr,
            fpcr,
            fpsr,
            simd,
            sys,
            gic_redist,
            gic_icc,
            guest_counter,
        })
    }

    fn sys(&self, reg: u16) -> Option<u64> {
        self.sys.iter().find(|(r, _)| *r == reg).map(|(_, v)| *v)
    }
}

fn core_reg(byte_offset: usize, size: u64) -> u64 {
    KVM_REG_ARM64 | size | u64::from(KVM_REG_ARM_CORE) | (byte_offset / size_of::<u32>()) as u64
}

fn sys_reg(encoding: u16) -> u64 {
    KVM_REG_ARM64 | KVM_REG_SIZE_U64 | KVM_REG_ARM64_SYSREG as u64 | encoding as u64
}

/// KVM's register list for one vCPU, in the order KVM must receive it: the
/// counter before the compare value and control so the timer is judged
/// against the restored timeline.
fn kvm_vcpu_regs(vcpu: &HvfVcpu, guest_counter: u64) -> Result<Vec<(u64, u128)>, String> {
    let sctlr = vcpu
        .sys(SYS_SCTLR_EL1)
        .ok_or("macOS vCPU state has no SCTLR_EL1")?;
    if sctlr & SCTLR_PAC_ENABLES != 0 {
        return Err(
            "the guest kernel signs pointers with Apple's pointer-authentication \
                    algorithm, which other CPUs cannot verify; the machine must boot \
                    with pointer authentication disabled to move off a Mac"
                .to_string(),
        );
    }

    let pt = offset_of!(kvm_regs, regs);
    let fp = offset_of!(kvm_regs, fp_regs);
    let u64_size = KVM_REG_SIZE_U64;
    let mut regs: Vec<(u64, u128)> = Vec::with_capacity(96);

    // Timer first: CNT, then CVAL armed to fire immediately, then CTL.
    let ctl = vcpu.sys(SYS_CNTV_CTL_EL0).unwrap_or(0);
    let cval = vcpu.sys(SYS_CNTV_CVAL_EL0).unwrap_or(0);
    let cval = if ctl & CNTV_CTL_ENABLE != 0 {
        guest_counter
    } else {
        cval
    };
    regs.push((sys_reg(KVM_TIMER_CNT), u128::from(guest_counter)));
    regs.push((sys_reg(KVM_TIMER_CVAL), u128::from(cval)));
    regs.push((sys_reg(KVM_TIMER_CTL), u128::from(ctl)));

    for &(reg, val) in &vcpu.sys {
        let id = match reg {
            SYS_SP_EL0 => core_reg(pt + offset_of!(user_pt_regs, sp), u64_size),
            SYS_SP_EL1 => core_reg(offset_of!(kvm_regs, sp_el1), u64_size),
            SYS_ELR_EL1 => core_reg(offset_of!(kvm_regs, elr_el1), u64_size),
            // spsr[0] is SPSR_EL1 (KVM_SPSR_EL1).
            SYS_SPSR_EL1 => core_reg(offset_of!(kvm_regs, spsr), u64_size),
            // Apple-specific bits; the KVM host's reset value is right for it.
            SYS_ACTLR_EL1 => continue,
            SYS_CNTV_CTL_EL0 | SYS_CNTV_CVAL_EL0 => continue,
            // Unused with pointer authentication off (checked above), and the
            // KVM vCPU may not expose them.
            r if SYS_PAC_KEYS.contains(&r) => continue,
            other => sys_reg(other),
        };
        regs.push((id, u128::from(val)));
    }

    for (i, &val) in vcpu.gp.iter().enumerate() {
        let off = pt + offset_of!(user_pt_regs, regs) + i * size_of::<u64>();
        regs.push((core_reg(off, u64_size), u128::from(val)));
    }
    regs.push((
        core_reg(pt + offset_of!(user_pt_regs, pc), u64_size),
        u128::from(vcpu.pc),
    ));
    regs.push((
        core_reg(pt + offset_of!(user_pt_regs, pstate), u64_size),
        u128::from(vcpu.cpsr),
    ));
    for (i, &val) in vcpu.simd.iter().enumerate() {
        let off = fp + offset_of!(user_fpsimd_state, vregs) + i * size_of::<u128>();
        regs.push((core_reg(off, KVM_REG_SIZE_U128), val));
    }
    regs.push((
        core_reg(fp + offset_of!(user_fpsimd_state, fpsr), KVM_REG_SIZE_U32),
        u128::from(vcpu.fpsr as u32),
    ));
    regs.push((
        core_reg(fp + offset_of!(user_fpsimd_state, fpcr), KVM_REG_SIZE_U32),
        u128::from(vcpu.fpcr as u32),
    ));
    Ok(regs)
}

/// Distributor registers as the KVM vGIC's 32-bit attribute writes. Banked
/// SGI/PPI registers (INTID < 32) live in the redistributors on both sides,
/// and INTIDs past the KVM vGIC's range must be idle.
fn kvm_dist_regs(dist: &[(u32, u64)]) -> Result<Vec<(u32, u32)>, String> {
    // (base, bits per interrupt) of the per-interrupt register banks.
    const BANKS: [(u32, u32); 6] = [
        (0x0080, 1), // IGROUPR
        (0x0100, 1), // ISENABLER
        (0x0200, 1), // ISPENDR
        (0x0300, 1), // ISACTIVER
        (0x0400, 8), // IPRIORITYR
        (0x0c00, 2), // ICFGR
    ];
    const IROUTER: u32 = 0x6000;
    const IROUTER_END: u32 = 0x6000 + 1020 * 8;

    let mut out = Vec::with_capacity(dist.len() + 128);
    for &(offset, val) in dist {
        if offset == 0 {
            out.push((0, val as u32)); // GICD_CTLR (merged with KVM's at restore)
            continue;
        }
        if (IROUTER..IROUTER_END).contains(&offset) {
            let intid = (offset - IROUTER) / 8;
            if intid < 32 {
                continue;
            }
            if intid >= KVM_NR_IRQS {
                continue;
            }
            out.push((offset, val as u32));
            out.push((offset + 4, (val >> 32) as u32));
            continue;
        }
        let Some(&(base, bits)) = BANKS
            .iter()
            .filter(|(base, _)| offset >= *base)
            .max_by_key(|(base, _)| *base)
        else {
            continue;
        };
        let first_intid = (offset - base) * 8 / bits;
        if first_intid < 32 {
            continue;
        }
        if first_intid >= KVM_NR_IRQS {
            let enabled_or_pending = matches!(base, 0x0100 | 0x0200 | 0x0300);
            if enabled_or_pending && val as u32 != 0 {
                return Err(format!(
                    "interrupts from INTID {first_intid} are in use but the KVM GIC \
                     implements only {KVM_NR_IRQS}"
                ));
            }
            continue;
        }
        out.push((offset, val as u32));
    }
    Ok(out)
}

/// The vGIC attribute id's CPU-affinity field for a vCPU index below 16.
fn affinity(index: usize) -> u64 {
    (index as u64) << 32
}

pub(crate) fn import(sections: CheckpointSections<'_>) -> Result<VmCheckpoint, String> {
    let vm = HvfVm::parse(sections.vm)?;
    let board = vm.board.ok_or(
        "this checkpoint was captured by a macOS smolvm too old to record its \
         virtual board; capture it again with a newer smolvm",
    )?;
    if vm.gic_distributor.is_empty() {
        return Err(
            "this checkpoint was captured without macOS's in-kernel GIC and cannot \
             be restored on Linux"
                .to_string(),
        );
    }
    let vcpus = sections
        .vcpus
        .iter()
        .map(|bytes| HvfVcpu::parse(bytes))
        .collect::<Result<Vec<_>, _>>()?;
    if vcpus.is_empty() || vcpus.len() > MAX_IMPORTED_VCPUS {
        return Err(format!(
            "a macOS checkpoint with {} vCPUs cannot be restored on Linux (1 to {MAX_IMPORTED_VCPUS})",
            vcpus.len()
        ));
    }
    // vCPUs are captured one after another; the latest counter reading keeps
    // every vCPU's view of time moving forward.
    let guest_counter = vcpus.iter().map(|v| v.guest_counter).max().unwrap_or(0);
    if guest_counter == 0 {
        return Err(
            "this checkpoint was captured by a macOS smolvm too old to record the \
             guest clock; capture it again with a newer smolvm"
                .to_string(),
        );
    }

    let mut vcpu_states = Vec::with_capacity(vcpus.len());
    let mut redist_regs = Vec::new();
    let mut icc_regs = Vec::new();
    for (index, vcpu) in vcpus.iter().enumerate() {
        vcpu_states.push(VcpuState::imported(
            KVM_MP_STATE_RUNNABLE,
            kvm_vcpu_regs(vcpu, guest_counter)?,
        ));
        let aff = affinity(index);
        for &(offset, val) in &vcpu.gic_redist {
            redist_regs.push((aff | u64::from(offset), val as u32));
        }
        // The HVF list is ordered SRE first and group enables last, as KVM
        // needs. Active priorities only travel when set: their layout depends
        // on the implemented priority bits.
        for &(reg, val) in &vcpu.gic_icc {
            let is_apr = matches!(reg, 50756 | 50760); // ICC_AP0R0_EL1, ICC_AP1R0_EL1
            if is_apr && val == 0 {
                continue;
            }
            icc_regs.push((aff | u64::from(reg), val));
        }
    }

    let vm_state = VmState::imported_v3(
        kvm_dist_regs(&vm.gic_distributor)?,
        redist_regs,
        icc_regs,
        board,
    );
    Ok(VmCheckpoint {
        vm_state,
        vcpu_states,
        devices: sections.devices,
        ioapic: None,
        // KVM on arm64 cannot add vCPUs to a running guest; the imported
        // machine keeps the CPUs it has.
        cpu_growth: None,
        memory_growth: sections.memory_growth,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hvf_vcpu_blob(sys: &[(u16, u64)], guest_counter: u64) -> Vec<u8> {
        let mut out = Vec::new();
        for i in 0..31u64 {
            out.extend_from_slice(&i.to_le_bytes());
        }
        for v in [0x8000_1000u64, 0x3c5, 0, 0x10] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        for i in 0..32u128 {
            out.extend_from_slice(&(i << 64).to_le_bytes());
        }
        out.extend_from_slice(&(sys.len() as u32).to_le_bytes());
        for &(reg, val) in sys {
            out.extend_from_slice(&reg.to_le_bytes());
            out.extend_from_slice(&val.to_le_bytes());
        }
        for pairs in [
            &[(0x10100u32, 1u64)][..],
            &[(50789u32, 7u64), (50756, 0)][..],
        ] {
            out.extend_from_slice(&(pairs.len() as u32).to_le_bytes());
            for &(reg, val) in pairs {
                out.extend_from_slice(&reg.to_le_bytes());
                out.extend_from_slice(&val.to_le_bytes());
            }
        }
        out.extend_from_slice(&123u64.to_le_bytes()); // vtimer offset
        out.extend_from_slice(&guest_counter.to_le_bytes());
        out
    }

    fn find(regs: &[(u64, u128)], id: u64) -> Option<u128> {
        regs.iter().find(|(r, _)| *r == id).map(|(_, v)| *v)
    }

    #[test]
    fn vcpu_registers_map_to_kvm_ids() {
        let blob = hvf_vcpu_blob(
            &[
                (SYS_SCTLR_EL1, 0x30d0_1805),
                (SYS_SP_EL0, 0xaaaa),
                (SYS_SP_EL1, 0xbbbb),
                (SYS_ELR_EL1, 0xcccc),
                (SYS_SPSR_EL1, 0x3c5),
                (SYS_ACTLR_EL1, 0x1),
                (0xc108, 0x55), // APIAKEYLO_EL1
                (SYS_CNTV_CTL_EL0, CNTV_CTL_ENABLE),
                (SYS_CNTV_CVAL_EL0, 9_999_999),
                (0xc708, 0x6), // CNTKCTL_EL1
            ],
            5_000,
        );
        let vcpu = HvfVcpu::parse(&blob).unwrap();
        let regs = kvm_vcpu_regs(&vcpu, 6_000).unwrap();

        // Counter, then an immediately-due compare, then control.
        assert_eq!(regs[0], (sys_reg(KVM_TIMER_CNT), 6_000));
        assert_eq!(regs[1], (sys_reg(KVM_TIMER_CVAL), 6_000));
        assert_eq!(regs[2], (sys_reg(KVM_TIMER_CTL), 1));

        let pt = offset_of!(kvm_regs, regs);
        let u64s = KVM_REG_SIZE_U64;
        assert_eq!(
            find(&regs, core_reg(pt + offset_of!(user_pt_regs, sp), u64s)),
            Some(0xaaaa)
        );
        assert_eq!(
            find(&regs, core_reg(offset_of!(kvm_regs, sp_el1), u64s)),
            Some(0xbbbb)
        );
        assert_eq!(
            find(&regs, core_reg(pt + offset_of!(user_pt_regs, pc), u64s)),
            Some(0x8000_1000)
        );
        assert_eq!(find(&regs, core_reg(pt + 30 * 8, u64s)), Some(30));
        assert_eq!(find(&regs, sys_reg(0xc708)), Some(0x6));
        assert_eq!(find(&regs, sys_reg(SYS_SCTLR_EL1)), Some(0x30d0_1805));
        // Apple-only and PAC registers do not travel.
        assert_eq!(find(&regs, sys_reg(SYS_ACTLR_EL1)), None);
        assert_eq!(find(&regs, sys_reg(0xc108)), None);
        // Known KVM UAPI ids.
        assert_eq!(
            core_reg(pt + offset_of!(user_pt_regs, pc), u64s),
            0x6030_0000_0010_0040
        );
        assert_eq!(sys_reg(KVM_TIMER_CNT), 0x6030_0000_0013_df1a);
    }

    #[test]
    fn a_disabled_timer_keeps_its_compare_value() {
        let blob = hvf_vcpu_blob(
            &[
                (SYS_SCTLR_EL1, 0),
                (SYS_CNTV_CTL_EL0, 0),
                (SYS_CNTV_CVAL_EL0, 42),
            ],
            5_000,
        );
        let regs = kvm_vcpu_regs(&HvfVcpu::parse(&blob).unwrap(), 5_000).unwrap();
        assert_eq!(regs[1], (sys_reg(KVM_TIMER_CVAL), 42));
    }

    #[test]
    fn pointer_authentication_in_use_is_refused() {
        let blob = hvf_vcpu_blob(&[(SYS_SCTLR_EL1, 1 << 31)], 5_000);
        let err = kvm_vcpu_regs(&HvfVcpu::parse(&blob).unwrap(), 5_000).unwrap_err();
        assert!(err.contains("pointer-authentication"), "{err}");
    }

    #[test]
    fn distributor_registers_split_and_skip_banked_ranges() {
        let regs = kvm_dist_regs(&[
            (0x0000, 0x12),                   // CTLR
            (0x0080, 0xffff_ffff),            // IGROUPR0: banked, skipped
            (0x0084, 0x1),                    // IGROUPR1: INTID 32..63
            (0x0400, 0xa0a0_a0a0),            // IPRIORITYR0: banked
            (0x0420, 0x8080_8080),            // IPRIORITYR8: INTID 32..35
            (0x0c04, 0x5),                    // ICFGR1: banked PPIs
            (0x0c08, 0xaaaa),                 // ICFGR2: INTID 32..47
            (0x6000 + 33 * 8, 0x1_0000_0002), // IROUTER33
            (0x6000 + 200 * 8, 0x3),          // beyond KVM's range
            (0x0100 + 4 * 4, 0),              // ISENABLER4 (INTID 128..), idle
        ])
        .unwrap();
        assert_eq!(
            regs,
            vec![
                (0x0000, 0x12),
                (0x0084, 0x1),
                (0x0420, 0x8080_8080),
                (0x0c08, 0xaaaa),
                (0x6000 + 33 * 8, 0x2),
                (0x6000 + 33 * 8 + 4, 0x1),
            ]
        );
    }

    #[test]
    fn interrupts_beyond_the_kvm_gic_in_use_are_refused() {
        assert!(kvm_dist_regs(&[(0x0100 + 4 * 4, 1)]).is_err());
    }
}
