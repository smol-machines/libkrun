// Copyright 2026 The libkrun Authors. Licensed under Apache-2.0.

use std::sync::atomic::Ordering;

use crate::cpu::{Cpu, PendingLoad, Reservation, extend};
use crate::csr::{MSTATUS_TSR, MSTATUS_TVM, MSTATUS_TW};
use crate::decode::{AluOp, AmoOp, BranchOp, CsrOp, Instr};
use crate::mmu::{Access, LoadResult, StoreResult};
use crate::trap::Exception;
use crate::{PrivMode, VmExit};

#[inline]
fn alu(op: AluOp, a: u64, b: u64) -> u64 {
    match op {
        AluOp::Add => a.wrapping_add(b),
        AluOp::Sub => a.wrapping_sub(b),
        AluOp::Sll => a << (b & 63),
        AluOp::Slt => ((a as i64) < (b as i64)) as u64,
        AluOp::Sltu => (a < b) as u64,
        AluOp::Xor => a ^ b,
        AluOp::Srl => a >> (b & 63),
        AluOp::Sra => ((a as i64) >> (b & 63)) as u64,
        AluOp::Or => a | b,
        AluOp::And => a & b,
        AluOp::Mul => a.wrapping_mul(b),
        AluOp::Mulh => (((a as i64 as i128) * (b as i64 as i128)) >> 64) as u64,
        AluOp::Mulhsu => (((a as i64 as i128) * (b as i128)) >> 64) as u64,
        AluOp::Mulhu => (((a as u128) * (b as u128)) >> 64) as u64,
        // Division by zero yields all-ones (Div) or the dividend (Rem);
        // i64::MIN / -1 wraps to i64::MIN with remainder 0. No traps.
        AluOp::Div => {
            if b == 0 {
                u64::MAX
            } else {
                (a as i64).wrapping_div(b as i64) as u64
            }
        }
        AluOp::Divu => a.checked_div(b).unwrap_or(u64::MAX),
        AluOp::Rem => {
            if b == 0 {
                a
            } else {
                (a as i64).wrapping_rem(b as i64) as u64
            }
        }
        AluOp::Remu => a.checked_rem(b).unwrap_or(a),
    }
}

#[inline]
fn alu32(op: AluOp, a: u64, b: u64) -> u64 {
    let sext = |v: i32| v as i64 as u64;
    match op {
        AluOp::Add => sext((a as u32).wrapping_add(b as u32) as i32),
        AluOp::Sub => sext((a as u32).wrapping_sub(b as u32) as i32),
        AluOp::Sll => sext(((a as u32) << (b & 31)) as i32),
        AluOp::Srl => sext(((a as u32) >> (b & 31)) as i32),
        AluOp::Sra => sext((a as i32) >> (b & 31)),
        AluOp::Mul => sext((a as i32).wrapping_mul(b as i32)),
        // Same div/rem edge semantics as the 64-bit forms, on 32-bit values.
        AluOp::Div => sext(if b as i32 == 0 {
            -1
        } else {
            (a as i32).wrapping_div(b as i32)
        }),
        AluOp::Divu => sext((a as u32).checked_div(b as u32).unwrap_or(u32::MAX) as i32),
        AluOp::Rem => sext(if b as i32 == 0 {
            a as i32
        } else {
            (a as i32).wrapping_rem(b as i32)
        }),
        AluOp::Remu => sext((a as u32).checked_rem(b as u32).unwrap_or(a as u32) as i32),
        // No W form exists; decode never emits these here. Kept free of
        // formatting so the function stays small enough to inline.
        _ => unreachable!(),
    }
}

fn amo32(op: AmoOp, cur: u32, src: u32) -> u32 {
    match op {
        AmoOp::Swap => src,
        AmoOp::Add => cur.wrapping_add(src),
        AmoOp::Xor => cur ^ src,
        AmoOp::And => cur & src,
        AmoOp::Or => cur | src,
        AmoOp::Min => (cur as i32).min(src as i32) as u32,
        AmoOp::Max => (cur as i32).max(src as i32) as u32,
        AmoOp::Minu => cur.min(src),
        AmoOp::Maxu => cur.max(src),
        // LR/SC are handled before the fetch-op path.
        AmoOp::Lr | AmoOp::Sc => unreachable!(),
    }
}

fn amo64(op: AmoOp, cur: u64, src: u64) -> u64 {
    match op {
        AmoOp::Swap => src,
        AmoOp::Add => cur.wrapping_add(src),
        AmoOp::Xor => cur ^ src,
        AmoOp::And => cur & src,
        AmoOp::Or => cur | src,
        AmoOp::Min => (cur as i64).min(src as i64) as u64,
        AmoOp::Max => (cur as i64).max(src as i64) as u64,
        AmoOp::Minu => cur.min(src),
        AmoOp::Maxu => cur.max(src),
        AmoOp::Lr | AmoOp::Sc => unreachable!(),
    }
}

#[inline]
fn branch_taken(op: BranchOp, a: u64, b: u64) -> bool {
    match op {
        BranchOp::Eq => a == b,
        BranchOp::Ne => a != b,
        BranchOp::Lt => (a as i64) < (b as i64),
        BranchOp::Ge => (a as i64) >= (b as i64),
        BranchOp::Ltu => a < b,
        BranchOp::Geu => a >= b,
    }
}

impl Cpu {
    pub(crate) fn execute(
        &mut self,
        instr: Instr,
        raw: u32,
        pc: u64,
        len: u64,
    ) -> Result<Option<VmExit>, Exception> {
        let ill = || Exception::IllegalInstruction(raw as u64);
        let mut next = pc.wrapping_add(len);
        let mut exit = None;

        match instr {
            Instr::Lui { rd, imm } => self.set_x(rd, imm as u64),
            Instr::Auipc { rd, imm } => self.set_x(rd, pc.wrapping_add(imm as u64)),
            Instr::Jal { rd, imm } => {
                self.set_x(rd, next);
                next = pc.wrapping_add(imm as u64);
            }
            Instr::Jalr { rd, rs1, imm } => {
                let target = self.xregs[rs1 as usize].wrapping_add(imm as u64) & !1;
                self.set_x(rd, next);
                next = target;
            }
            Instr::Branch { op, rs1, rs2, imm } => {
                if branch_taken(op, self.xregs[rs1 as usize], self.xregs[rs2 as usize]) {
                    next = pc.wrapping_add(imm as u64);
                }
            }
            Instr::Load { op, rd, rs1, imm } => {
                let va = self.xregs[rs1 as usize].wrapping_add(imm as u64);
                let size = op.size();
                if let Some(off) = self.fast_load_off(va, size) {
                    // SAFETY: a primed page lies wholly inside RAM and the
                    // access was checked not to leave it.
                    let v = unsafe { self.ram.load_at(off, size) };
                    self.set_x(rd, extend(v, size, op.signed()));
                    self.pc = next;
                    self.instret += 1;
                    return Ok(None);
                }
                match self.load_vaddr(va, size)? {
                    LoadResult::Ram(v) => self.set_x(rd, extend(v, size, op.signed())),
                    LoadResult::Mmio(pa) => {
                        // pc does not advance; complete_mmio_read() retires.
                        self.pending_load = Some(PendingLoad {
                            rd,
                            size: size as u8,
                            sign: op.signed(),
                            freg: false,
                            next_pc: next,
                        });
                        return Ok(Some(VmExit::MmioRead {
                            addr: pa,
                            size: size as u8,
                        }));
                    }
                }
            }
            Instr::Store { op, rs1, rs2, imm } => {
                // A store by this hart clears its own LR reservation.
                self.reservation = None;
                let va = self.xregs[rs1 as usize].wrapping_add(imm as u64);
                let size = op.size();
                let data = self.xregs[rs2 as usize] & mask_for(size);
                if let Some(off) = self.fast_store_off(va, size) {
                    // SAFETY: as the load fast path.
                    unsafe { self.ram.store_at(off, size, data) };
                    self.pc = next;
                    self.instret += 1;
                    return Ok(None);
                }
                match self.store_vaddr(va, size, data)? {
                    StoreResult::Ram(pa) => {
                        if self.tohost_addr() == Some(pa) {
                            let code = self.ram.load(pa & !7, 8).unwrap();
                            exit = Some(VmExit::Shutdown { code });
                        }
                    }
                    StoreResult::Mmio(pa) => {
                        exit = Some(VmExit::MmioWrite {
                            addr: pa,
                            size: size as u8,
                            data,
                        });
                    }
                }
            }
            Instr::OpImm { op, rd, rs1, imm } => {
                let v = alu(op, self.xregs[rs1 as usize], imm as u64);
                self.set_x(rd, v);
            }
            Instr::OpImm32 { op, rd, rs1, imm } => {
                let v = alu32(op, self.xregs[rs1 as usize], imm as u64);
                self.set_x(rd, v);
            }
            Instr::Op { op, rd, rs1, rs2 } => {
                let v = alu(op, self.xregs[rs1 as usize], self.xregs[rs2 as usize]);
                self.set_x(rd, v);
            }
            Instr::Op32 { op, rd, rs1, rs2 } => {
                let v = alu32(op, self.xregs[rs1 as usize], self.xregs[rs2 as usize]);
                self.set_x(rd, v);
            }
            // No caches to synchronize: fences are no-ops and every fetch
            // reads RAM directly, which also satisfies fence.i.
            Instr::Fence | Instr::FenceI => {}
            Instr::Ecall => {
                if self.priv_mode == PrivMode::S && self.sbi_mode {
                    self.pc = next;
                    self.instret += 1;
                    return Ok(Some(VmExit::Ecall { from: PrivMode::S }));
                }
                return Err(Exception::ecall_from(self.priv_mode));
            }
            Instr::Ebreak => return Err(Exception::Breakpoint(pc)),
            Instr::Mret => {
                if self.priv_mode != PrivMode::M {
                    return Err(ill());
                }
                next = self.do_mret();
            }
            Instr::Sret => {
                match self.priv_mode {
                    PrivMode::U => return Err(ill()),
                    PrivMode::S if self.csrs.mstatus & MSTATUS_TSR != 0 => return Err(ill()),
                    _ => {}
                }
                next = self.do_sret();
            }
            Instr::Wfi => {
                match self.priv_mode {
                    PrivMode::U => return Err(ill()),
                    PrivMode::S if self.csrs.mstatus & MSTATUS_TW != 0 => return Err(ill()),
                    _ => {}
                }
                // Wake condition ignores the global xIE bits per spec.
                if self.effective_mip() & self.csrs.mie == 0 {
                    self.pc = next;
                    self.instret += 1;
                    return Ok(Some(VmExit::Wfi));
                }
            }
            Instr::SfenceVma => {
                match self.priv_mode {
                    PrivMode::U => return Err(ill()),
                    PrivMode::S if self.csrs.mstatus & MSTATUS_TVM != 0 => return Err(ill()),
                    _ => {}
                }
                // Full flush regardless of the rs1/rs2 (vaddr/ASID) hints.
                self.flush_tlb();
            }
            Instr::Csr {
                op,
                rd,
                rs1,
                csr,
                imm,
            } => {
                let src = if imm {
                    rs1 as u64
                } else {
                    self.xregs[rs1 as usize]
                };
                let fix = |e| match e {
                    Exception::IllegalInstruction(_) => ill(),
                    other => other,
                };
                let old = self.csr_read(csr).map_err(fix)?;
                match op {
                    CsrOp::Rw => self.csr_write(csr, src).map_err(fix)?,
                    // rs1/zimm == 0 means no write (and no write side effects).
                    CsrOp::Rs if rs1 != 0 => self.csr_write(csr, old | src).map_err(fix)?,
                    CsrOp::Rc if rs1 != 0 => self.csr_write(csr, old & !src).map_err(fix)?,
                    _ => {}
                }
                self.set_x(rd, old);
            }
            Instr::Amo {
                op,
                rd,
                rs1,
                rs2,
                width,
                ..
            } => self.exec_amo(op, rd, rs1, rs2, width)?,
            Instr::Fp { raw: fp_raw } => {
                let (fp_exit, holds_pc) = self.exec_fp(fp_raw, next)?;
                if holds_pc {
                    // MMIO read: pc retires in complete_mmio_read().
                    return Ok(fp_exit);
                }
                exit = fp_exit;
            }
            Instr::Illegal(bits) => return Err(Exception::IllegalInstruction(bits as u64)),
        }

        self.pc = next;
        self.instret += 1;
        Ok(exit)
    }

    /// LR/SC/AMO. All go through `translate` like ordinary accesses;
    /// misaligned addresses and MMIO targets raise exceptions (atomics
    /// never exit to the bus). LR faults as a load, SC/AMO as stores.
    fn exec_amo(
        &mut self,
        op: AmoOp,
        rd: u8,
        rs1: u8,
        rs2: u8,
        width: u8,
    ) -> Result<(), Exception> {
        let va = self.xregs[rs1 as usize];
        let src = self.xregs[rs2 as usize];
        let is_lr = op == AmoOp::Lr;
        if va & (width as u64 - 1) != 0 {
            return Err(if is_lr {
                Exception::LoadAddrMisaligned(va)
            } else {
                Exception::StoreAddrMisaligned(va)
            });
        }
        let access = if is_lr { Access::Read } else { Access::Write };
        let prv = self.data_priv();
        let pa = self.translate(va, access, prv, width as u64)?;
        if !self.ram.contains(pa, width as u64) {
            return Err(if is_lr {
                Exception::LoadAccessFault(va)
            } else {
                Exception::StoreAccessFault(va)
            });
        }
        let old = match op {
            AmoOp::Lr => {
                let val = if width == 4 {
                    self.ram.atomic_u32(pa).unwrap().load(Ordering::SeqCst) as u64
                } else {
                    self.ram.atomic_u64(pa).unwrap().load(Ordering::SeqCst)
                };
                self.reservation = Some(Reservation {
                    addr: pa,
                    width,
                    val,
                });
                val
            }
            AmoOp::Sc => {
                // SC always consumes the reservation and only stores when
                // the reserved word still holds the value LR observed (see
                // the ABA note on `Reservation`).
                let ok = match self.reservation.take() {
                    Some(r) if r.addr == pa && r.width == width => {
                        if width == 4 {
                            self.ram
                                .atomic_u32(pa)
                                .unwrap()
                                .compare_exchange(
                                    r.val as u32,
                                    src as u32,
                                    Ordering::SeqCst,
                                    Ordering::SeqCst,
                                )
                                .is_ok()
                        } else {
                            self.ram
                                .atomic_u64(pa)
                                .unwrap()
                                .compare_exchange(r.val, src, Ordering::SeqCst, Ordering::SeqCst)
                                .is_ok()
                        }
                    }
                    _ => false,
                };
                self.set_x(rd, u64::from(!ok));
                return Ok(());
            }
            _ => {
                // Fetch-op AMOs are stores: they clear this hart's own
                // reservation like any other store.
                self.reservation = None;
                if width == 4 {
                    self.ram
                        .atomic_u32(pa)
                        .unwrap()
                        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |cur| {
                            Some(amo32(op, cur, src as u32))
                        })
                        .unwrap() as u64
                } else {
                    self.ram
                        .atomic_u64(pa)
                        .unwrap()
                        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |cur| {
                            Some(amo64(op, cur, src))
                        })
                        .unwrap()
                }
            }
        };
        // `.w` forms sign-extend the loaded value into rd.
        self.set_x(rd, if width == 4 { old as i32 as u64 } else { old });
        Ok(())
    }
}

fn mask_for(size: usize) -> u64 {
    match size {
        1 => 0xff,
        2 => 0xffff,
        4 => 0xffff_ffff,
        _ => u64::MAX,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn m_multiply() {
        assert_eq!(alu(AluOp::Mul, 7, 6), 42);
        let m1 = u64::MAX; // -1
        assert_eq!(alu(AluOp::Mulh, m1, m1), 0); // (-1)*(-1) = 1
        assert_eq!(alu(AluOp::Mulhu, m1, m1), u64::MAX - 1);
        assert_eq!(alu(AluOp::Mulhsu, m1, m1), u64::MAX); // -1 * (2^64-1)
        assert_eq!(alu(AluOp::Mulh, 1 << 63, 2), u64::MAX); // MIN*2 >> 64
        assert_eq!(alu32(AluOp::Mul, 0x7fff_ffff, 2), 0xffff_ffff_ffff_fffe);
    }

    #[test]
    fn m_divide_edge_cases() {
        let neg = |v: i64| v as u64;
        // Division by zero: quotient all-ones, remainder = dividend.
        assert_eq!(alu(AluOp::Div, 42, 0), u64::MAX);
        assert_eq!(alu(AluOp::Divu, 42, 0), u64::MAX);
        assert_eq!(alu(AluOp::Rem, 42, 0), 42);
        assert_eq!(alu(AluOp::Remu, neg(-42), 0), neg(-42));
        // Signed overflow: MIN / -1 = MIN, remainder 0.
        assert_eq!(alu(AluOp::Div, neg(i64::MIN), neg(-1)), neg(i64::MIN));
        assert_eq!(alu(AluOp::Rem, neg(i64::MIN), neg(-1)), 0);
        // Truncating division.
        assert_eq!(alu(AluOp::Div, 20, neg(-6)), neg(-3));
        assert_eq!(alu(AluOp::Rem, 20, neg(-6)), 2);
        assert_eq!(alu(AluOp::Rem, neg(-20), 6), neg(-2));
        // Word forms sign-extend and use 32-bit edge values.
        assert_eq!(alu32(AluOp::Div, 42, 0), u64::MAX);
        assert_eq!(alu32(AluOp::Divu, 42, 0), u64::MAX);
        assert_eq!(alu32(AluOp::Rem, neg(-42), 0), neg(-42));
        assert_eq!(alu32(AluOp::Remu, 0x8000_0000, 0), 0xffff_ffff_8000_0000);
        assert_eq!(
            alu32(AluOp::Div, i32::MIN as u32 as u64, u32::MAX as u64),
            i32::MIN as i64 as u64
        );
        assert_eq!(
            alu32(AluOp::Rem, i32::MIN as u32 as u64, u32::MAX as u64),
            0
        );
        assert_eq!(alu32(AluOp::Divu, 0x8000_0000, 1), 0xffff_ffff_8000_0000);
    }
}
