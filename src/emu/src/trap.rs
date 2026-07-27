// Copyright 2026 The libkrun Authors. Licensed under Apache-2.0.

use crate::PrivMode;
use crate::cpu::Cpu;
use crate::csr::{
    MSTATUS_MIE, MSTATUS_MPIE, MSTATUS_MPP_MASK, MSTATUS_MPRV, MSTATUS_SIE, MSTATUS_SPIE,
    MSTATUS_SPP,
};

pub const INTERRUPT_BIT: u64 = 1 << 63;

/// Synchronous exceptions. The payload is the xtval value (faulting address,
/// or raw instruction bits for IllegalInstruction).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exception {
    InstrAddrMisaligned(u64),
    InstrAccessFault(u64),
    IllegalInstruction(u64),
    Breakpoint(u64),
    LoadAddrMisaligned(u64),
    LoadAccessFault(u64),
    StoreAddrMisaligned(u64),
    StoreAccessFault(u64),
    EcallFromU,
    EcallFromS,
    EcallFromM,
    InstrPageFault(u64),
    LoadPageFault(u64),
    StorePageFault(u64),
}

impl Exception {
    pub fn code(self) -> u64 {
        match self {
            Exception::InstrAddrMisaligned(_) => 0,
            Exception::InstrAccessFault(_) => 1,
            Exception::IllegalInstruction(_) => 2,
            Exception::Breakpoint(_) => 3,
            Exception::LoadAddrMisaligned(_) => 4,
            Exception::LoadAccessFault(_) => 5,
            Exception::StoreAddrMisaligned(_) => 6,
            Exception::StoreAccessFault(_) => 7,
            Exception::EcallFromU => 8,
            Exception::EcallFromS => 9,
            Exception::EcallFromM => 11,
            Exception::InstrPageFault(_) => 12,
            Exception::LoadPageFault(_) => 13,
            Exception::StorePageFault(_) => 15,
        }
    }

    pub fn tval(self) -> u64 {
        match self {
            Exception::InstrAddrMisaligned(v)
            | Exception::InstrAccessFault(v)
            | Exception::IllegalInstruction(v)
            | Exception::Breakpoint(v)
            | Exception::LoadAddrMisaligned(v)
            | Exception::LoadAccessFault(v)
            | Exception::StoreAddrMisaligned(v)
            | Exception::StoreAccessFault(v)
            | Exception::InstrPageFault(v)
            | Exception::LoadPageFault(v)
            | Exception::StorePageFault(v) => v,
            Exception::EcallFromU | Exception::EcallFromS | Exception::EcallFromM => 0,
        }
    }

    pub fn ecall_from(mode: PrivMode) -> Self {
        match mode {
            PrivMode::U => Exception::EcallFromU,
            PrivMode::S => Exception::EcallFromS,
            PrivMode::M => Exception::EcallFromM,
        }
    }
}

fn priv_from_bits(bits: u64) -> PrivMode {
    match bits {
        0 => PrivMode::U,
        1 => PrivMode::S,
        // 2 is never stored: MPP writes are WARL-filtered and trap entry
        // only records real modes.
        _ => PrivMode::M,
    }
}

fn vector_target(tvec: u64, cause: u64) -> u64 {
    let base = tvec & !3;
    if tvec & 3 == 1 && cause & INTERRUPT_BIT != 0 {
        base + 4 * (cause & 0x3f)
    } else {
        base
    }
}

impl Cpu {
    pub(crate) fn take_exception(&mut self, e: Exception, epc: u64) {
        self.take_trap(e.code(), e.tval(), epc);
    }

    /// Standard interrupt priority: MEI > MSI > MTI > SEI > SSI > STI.
    /// Returns the interrupt cause code to take, if any.
    pub(crate) fn pending_interrupt(&self) -> Option<u64> {
        let pending = self.effective_mip_snapshot() & self.csrs.mie;
        if pending == 0 {
            return None;
        }
        let mstatus = self.csrs.mstatus;

        let m_pending = pending & !self.csrs.mideleg;
        let m_enabled = self.priv_mode < PrivMode::M || (mstatus & MSTATUS_MIE != 0);
        if m_enabled && m_pending != 0 {
            for code in [11, 3, 7, 9, 1, 5] {
                if m_pending & (1 << code) != 0 {
                    return Some(code);
                }
            }
        }

        let s_pending = pending & self.csrs.mideleg;
        let s_enabled = self.priv_mode < PrivMode::S
            || (self.priv_mode == PrivMode::S && mstatus & MSTATUS_SIE != 0);
        if s_enabled && s_pending != 0 {
            for code in [9, 1, 5] {
                if s_pending & (1 << code) != 0 {
                    return Some(code);
                }
            }
        }
        None
    }

    pub(crate) fn take_interrupt(&mut self, code: u64) {
        self.take_trap(code | INTERRUPT_BIT, 0, self.pc);
    }

    fn take_trap(&mut self, cause: u64, tval: u64, epc: u64) {
        let code = cause & 0x3f;
        let deleg = if cause & INTERRUPT_BIT != 0 {
            self.csrs.mideleg
        } else {
            self.csrs.medeleg
        };
        let to_s = self.priv_mode != PrivMode::M && deleg & (1 << code) != 0;

        if to_s {
            self.csrs.scause = cause;
            self.csrs.sepc = epc & !1;
            self.csrs.stval = tval;
            let m = self.csrs.mstatus;
            let sie = (m >> 1) & 1;
            let spp = if self.priv_mode == PrivMode::S {
                MSTATUS_SPP
            } else {
                0
            };
            self.csrs.mstatus =
                (m & !(MSTATUS_SIE | MSTATUS_SPIE | MSTATUS_SPP)) | (sie << 5) | spp;
            self.priv_mode = PrivMode::S;
            self.pc = vector_target(self.csrs.stvec, cause);
        } else {
            self.csrs.mcause = cause;
            self.csrs.mepc = epc & !1;
            self.csrs.mtval = tval;
            let m = self.csrs.mstatus;
            let mie = (m >> 3) & 1;
            let mpp = (self.priv_mode as u64) << 11;
            self.csrs.mstatus =
                (m & !(MSTATUS_MIE | MSTATUS_MPIE | MSTATUS_MPP_MASK)) | (mie << 7) | mpp;
            self.priv_mode = PrivMode::M;
            self.pc = vector_target(self.csrs.mtvec, cause);
        }
    }

    /// mret bookkeeping; returns the target pc.
    pub(crate) fn do_mret(&mut self) -> u64 {
        let m = self.csrs.mstatus;
        let mpp = (m >> 11) & 3;
        let mpie = (m >> 7) & 1;
        let mut new = m & !(MSTATUS_MIE | MSTATUS_MPIE | MSTATUS_MPP_MASK);
        new |= mpie << 3;
        new |= MSTATUS_MPIE;
        if mpp != 3 {
            new &= !MSTATUS_MPRV;
        }
        self.csrs.mstatus = new;
        self.priv_mode = priv_from_bits(mpp);
        self.csrs.mepc
    }

    /// sret bookkeeping; returns the target pc.
    pub(crate) fn do_sret(&mut self) -> u64 {
        let m = self.csrs.mstatus;
        let spp = (m >> 8) & 1;
        let spie = (m >> 5) & 1;
        let mut new = m & !(MSTATUS_SIE | MSTATUS_SPIE | MSTATUS_SPP | MSTATUS_MPRV);
        new |= spie << 1;
        new |= MSTATUS_SPIE;
        self.csrs.mstatus = new;
        self.priv_mode = priv_from_bits(spp);
        self.csrs.sepc
    }
}
