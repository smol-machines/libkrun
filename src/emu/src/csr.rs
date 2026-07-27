// Copyright 2026 The libkrun Authors. Licensed under Apache-2.0.

use crate::PrivMode;
use crate::cpu::Cpu;
use crate::trap::Exception;

pub const MSTATUS_SIE: u64 = 1 << 1;
pub const MSTATUS_MIE: u64 = 1 << 3;
pub const MSTATUS_SPIE: u64 = 1 << 5;
pub const MSTATUS_MPIE: u64 = 1 << 7;
pub const MSTATUS_SPP: u64 = 1 << 8;
pub const MSTATUS_MPP_MASK: u64 = 3 << 11;
pub const MSTATUS_FS_MASK: u64 = 3 << 13;
pub const MSTATUS_MPRV: u64 = 1 << 17;
pub const MSTATUS_SUM: u64 = 1 << 18;
pub const MSTATUS_MXR: u64 = 1 << 19;
pub const MSTATUS_TVM: u64 = 1 << 20;
pub const MSTATUS_TW: u64 = 1 << 21;
pub const MSTATUS_TSR: u64 = 1 << 22;
// UXL and SXL are read-only 64-bit.
const MSTATUS_XL: u64 = (2 << 32) | (2 << 34);
const MSTATUS_SD: u64 = 1 << 63;

const MSTATUS_WMASK: u64 = MSTATUS_SIE
    | MSTATUS_MIE
    | MSTATUS_SPIE
    | MSTATUS_MPIE
    | MSTATUS_SPP
    | MSTATUS_MPP_MASK
    | MSTATUS_FS_MASK
    | MSTATUS_MPRV
    | MSTATUS_SUM
    | MSTATUS_MXR
    | MSTATUS_TVM
    | MSTATUS_TW
    | MSTATUS_TSR;

const SSTATUS_WMASK: u64 =
    MSTATUS_SIE | MSTATUS_SPIE | MSTATUS_SPP | MSTATUS_FS_MASK | MSTATUS_SUM | MSTATUS_MXR;

/// sstatus UXL is read-only 2 (RV64); SXL must not leak into sstatus reads.
const SSTATUS_UXL: u64 = 3 << 32;

/// RV64, IMAFDC + S + U.
const MISA: u64 = (2 << 62)
    | (1 << 0)  // A
    | (1 << 2)  // C
    | (1 << 3)  // D
    | (1 << 5)  // F
    | (1 << 8)  // I
    | (1 << 12) // M
    | (1 << 18) // S
    | (1 << 20); // U

/// Delegatable exceptions: everything defined except ecall-from-M.
const MEDELEG_MASK: u64 = 0xb3ff;
/// Delegatable interrupts: SSI, STI, SEI.
const MIDELEG_MASK: u64 = 0x222;
const MIE_MASK: u64 = 0xaaa;
/// mip bits writable through the CSR interface (SSIP, STIP, SEIP).
const MIP_WMASK: u64 = 0x222;
/// mip.SSIP: the supervisor software-interrupt pending bit.
pub const MIP_SSIP: u64 = 1 << 1;

/// CSR storage. Interrupt lines live in `Cpu` as atomics and are OR-ed into
/// mip on read.
#[derive(Debug)]
pub struct CsrFile {
    pub mstatus: u64,
    /// Reset value is the full RV64 IMAFDC+S+U set; `Cpu::mask_misa` can
    /// clear bits (e.g. F/D while no FPU is wired up). Guest writes are
    /// ignored.
    pub misa: u64,
    pub medeleg: u64,
    pub mideleg: u64,
    pub mie: u64,
    pub mtvec: u64,
    pub mcounteren: u64,
    pub mcountinhibit: u64,
    pub menvcfg: u64,
    pub mscratch: u64,
    pub mepc: u64,
    pub mcause: u64,
    pub mtval: u64,
    pub mip: u64,
    pub mhartid: u64,
    pub stvec: u64,
    pub scounteren: u64,
    pub senvcfg: u64,
    pub sscratch: u64,
    pub sepc: u64,
    pub scause: u64,
    pub stval: u64,
    pub satp: u64,
    pub fcsr: u64,
    pub pmpcfg: [u64; 16],
    pub pmpaddr: [u64; 64],
}

impl CsrFile {
    pub fn new(hart_id: u64) -> Self {
        CsrFile {
            mstatus: 0,
            misa: MISA,
            medeleg: 0,
            mideleg: 0,
            mie: 0,
            mtvec: 0,
            mcounteren: 0,
            mcountinhibit: 0,
            menvcfg: 0,
            mscratch: 0,
            mepc: 0,
            mcause: 0,
            mtval: 0,
            mip: 0,
            mhartid: hart_id,
            stvec: 0,
            scounteren: 0,
            senvcfg: 0,
            sscratch: 0,
            sepc: 0,
            scause: 0,
            stval: 0,
            satp: 0,
            fcsr: 0,
            pmpcfg: [0; 16],
            pmpaddr: [0; 64],
        }
    }

    pub fn mstatus_read(&self) -> u64 {
        let mut v = self.mstatus | MSTATUS_XL;
        if self.mstatus & MSTATUS_FS_MASK == MSTATUS_FS_MASK {
            v |= MSTATUS_SD;
        }
        v
    }
}

impl Cpu {
    pub(crate) fn check_fs_on(&self) -> Result<(), Exception> {
        if self.csrs.mstatus & MSTATUS_FS_MASK == 0 {
            // FS = Off: fp CSR accesses are illegal. tval filled by caller.
            Err(Exception::IllegalInstruction(0))
        } else {
            Ok(())
        }
    }

    pub(crate) fn mark_fs_dirty(&mut self) {
        self.csrs.mstatus |= MSTATUS_FS_MASK;
    }

    /// cycle/time/instret/hpmcounterN access from S and U is gated by
    /// mcounteren (and scounteren for U).
    fn check_counter(&self, bit: u32) -> Result<(), Exception> {
        match self.priv_mode {
            PrivMode::M => Ok(()),
            PrivMode::S => {
                if self.csrs.mcounteren & (1 << bit) != 0 {
                    Ok(())
                } else {
                    Err(Exception::IllegalInstruction(0))
                }
            }
            PrivMode::U => {
                if self.csrs.mcounteren & self.csrs.scounteren & (1 << bit) != 0 {
                    Ok(())
                } else {
                    Err(Exception::IllegalInstruction(0))
                }
            }
        }
    }

    fn check_csr_priv(&self, addr: u16) -> Result<(), Exception> {
        if (self.priv_mode as u64) < ((addr >> 8) & 3) as u64 {
            Err(Exception::IllegalInstruction(0))
        } else {
            Ok(())
        }
    }

    pub(crate) fn csr_read(&mut self, addr: u16) -> Result<u64, Exception> {
        self.check_csr_priv(addr)?;
        let c = &self.csrs;
        let v = match addr {
            0x001 => {
                self.check_fs_on()?;
                c.fcsr & 0x1f
            }
            0x002 => {
                self.check_fs_on()?;
                (c.fcsr >> 5) & 7
            }
            0x003 => {
                self.check_fs_on()?;
                c.fcsr & 0xff
            }
            0x100 => c.mstatus_read() & (SSTATUS_WMASK | SSTATUS_UXL | MSTATUS_SD),
            0x104 => c.mie & c.mideleg,
            0x105 => c.stvec,
            0x106 => c.scounteren,
            0x10a => c.senvcfg,
            0x140 => c.sscratch,
            0x141 => c.sepc,
            0x142 => c.scause,
            0x143 => c.stval,
            0x144 => self.effective_mip() & c.mideleg,
            0x180 => {
                if self.priv_mode == PrivMode::S && c.mstatus & MSTATUS_TVM != 0 {
                    return Err(Exception::IllegalInstruction(0));
                }
                c.satp
            }
            0x300 => c.mstatus_read(),
            0x301 => c.misa,
            0x302 => c.medeleg,
            0x303 => c.mideleg,
            0x304 => c.mie,
            0x305 => c.mtvec,
            0x306 => c.mcounteren,
            0x30a => c.menvcfg,
            0x320 => c.mcountinhibit,
            0x323..=0x33f => 0, // mhpmevent3..31
            0x340 => c.mscratch,
            0x341 => c.mepc,
            0x342 => c.mcause,
            0x343 => c.mtval,
            0x344 => self.effective_mip(),
            // RV64: only the even-numbered pmpcfg registers exist.
            0x3a0..=0x3af if addr & 1 == 0 => c.pmpcfg[(addr - 0x3a0) as usize],
            0x3b0..=0x3ef => c.pmpaddr[(addr - 0x3b0) as usize],
            // No triggers: tselect is WARL and reads back a value that can
            // never be written, which tells software none exist.
            0x7a0 => u64::MAX,
            0xb00 => self.instret,
            0xb02 => self.instret,
            0xb03..=0xb1f => 0, // mhpmcounter3..31
            0xc00 => {
                self.check_counter(0)?;
                self.instret
            }
            0xc01 => {
                self.check_counter(1)?;
                self.clock.now(self.instret)
            }
            0xc02 => {
                self.check_counter(2)?;
                self.instret
            }
            0xc03..=0xc1f => {
                self.check_counter((addr - 0xc00) as u32)?;
                0
            }
            0xf11 | 0xf12 | 0xf13 | 0xf15 => 0, // mvendorid/marchid/mimpid/mconfigptr
            0xf14 => c.mhartid,
            _ => return Err(Exception::IllegalInstruction(0)),
        };
        Ok(v)
    }

    pub(crate) fn csr_write(&mut self, addr: u16, val: u64) -> Result<(), Exception> {
        self.check_csr_priv(addr)?;
        // Bits 11:10 == 11 marks the read-only address space.
        if addr >> 10 == 3 {
            return Err(Exception::IllegalInstruction(0));
        }
        match addr {
            0x001 => {
                self.check_fs_on()?;
                self.csrs.fcsr = (self.csrs.fcsr & !0x1f) | (val & 0x1f);
                self.mark_fs_dirty();
            }
            0x002 => {
                self.check_fs_on()?;
                self.csrs.fcsr = (self.csrs.fcsr & !0xe0) | ((val & 7) << 5);
                self.mark_fs_dirty();
            }
            0x003 => {
                self.check_fs_on()?;
                self.csrs.fcsr = val & 0xff;
                self.mark_fs_dirty();
            }
            0x100 => {
                self.csrs.mstatus = (self.csrs.mstatus & !SSTATUS_WMASK) | (val & SSTATUS_WMASK);
            }
            0x104 => {
                let mask = self.csrs.mideleg & MIE_MASK;
                self.csrs.mie = (self.csrs.mie & !mask) | (val & mask);
            }
            0x105 => self.csrs.stvec = warl_tvec(self.csrs.stvec, val),
            0x106 => self.csrs.scounteren = val & 0xffff_ffff,
            0x10a => self.csrs.senvcfg = val,
            0x140 => self.csrs.sscratch = val,
            0x141 => self.csrs.sepc = val & !1,
            0x142 => self.csrs.scause = val,
            0x143 => self.csrs.stval = val,
            0x144 => {
                // Only SSIP is software-writable from the S view.
                let mask = 0x2 & self.csrs.mideleg;
                self.csrs.mip = (self.csrs.mip & !mask) | (val & mask);
            }
            0x180 => {
                if self.priv_mode == PrivMode::S && self.csrs.mstatus & MSTATUS_TVM != 0 {
                    return Err(Exception::IllegalInstruction(0));
                }
                // WARL: only Bare/Sv39/Sv48 are accepted; writes with other
                // modes leave satp unchanged (Linux probes modes this way).
                // ASID is hard-wired to 0: every accepted write flushes the
                // TLB, so translations can never outlive their satp.
                let mode = val >> 60;
                if mode == 0 || mode == 8 || mode == 9 {
                    self.csrs.satp = val & !(0xffff << 44);
                    self.flush_tlb();
                }
            }
            0x300 => {
                let old = self.csrs.mstatus;
                let mut new = (old & !MSTATUS_WMASK) | (val & MSTATUS_WMASK);
                // MPP is WARL: 2 is not a mode; keep the old value.
                if new & MSTATUS_MPP_MASK == 2 << 11 {
                    new = (new & !MSTATUS_MPP_MASK) | (old & MSTATUS_MPP_MASK);
                }
                self.csrs.mstatus = new;
            }
            0x301 => {} // misa writes ignored
            0x302 => self.csrs.medeleg = val & MEDELEG_MASK,
            0x303 => self.csrs.mideleg = val & MIDELEG_MASK,
            0x304 => self.csrs.mie = val & MIE_MASK,
            0x305 => self.csrs.mtvec = warl_tvec(self.csrs.mtvec, val),
            0x306 => self.csrs.mcounteren = val & 0xffff_ffff,
            0x30a => self.csrs.menvcfg = val,
            0x320 => self.csrs.mcountinhibit = val & 0xffff_ffff,
            0x323..=0x33f => {} // mhpmevent writes ignored
            0x340 => self.csrs.mscratch = val,
            0x341 => self.csrs.mepc = val & !1,
            0x342 => self.csrs.mcause = val,
            0x343 => self.csrs.mtval = val,
            0x344 => self.csrs.mip = val & MIP_WMASK,
            0x3a0..=0x3af if addr & 1 == 0 => {
                // Locked entries are read-only; bits 6:5 of each byte are
                // reserved (WARL zero).
                let idx = (addr - 0x3a0) as usize;
                let old = self.csrs.pmpcfg[idx];
                let mut new = 0;
                for j in 0..8 {
                    let ob = (old >> (8 * j)) & 0xff;
                    let nb = if ob & 0x80 != 0 {
                        ob
                    } else {
                        (val >> (8 * j)) & 0x9f
                    };
                    new |= nb << (8 * j);
                }
                self.csrs.pmpcfg[idx] = new;
                self.update_pmp_active();
            }
            0x3b0..=0x3ef => {
                let i = (addr - 0x3b0) as usize;
                if !self.pmpaddr_locked(i) {
                    // Address bits are 54-bit WARL (grain G = 0).
                    self.csrs.pmpaddr[i] = val & ((1 << 54) - 1);
                }
            }
            0x7a0 => {} // tselect: no triggers, writes ignored
            // mcycle/minstret (cycle mirrors instret). A write suppresses
            // the writing instruction's own increment, so the next
            // instruction reads exactly the written value.
            0xb00 | 0xb02 => self.instret = val.wrapping_sub(1),
            0xb03..=0xb1f => {} // mhpmcounter writes ignored
            _ => return Err(Exception::IllegalInstruction(0)),
        }
        Ok(())
    }
}

/// tvec is WARL: only direct (0) and vectored (1) modes exist.
fn warl_tvec(old: u64, val: u64) -> u64 {
    if val & 3 <= 1 {
        val
    } else {
        (val & !3) | (old & 3)
    }
}
