// Copyright 2026 The libkrun Authors. Licensed under Apache-2.0.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::csr::{CsrFile, MIP_SSIP};
use crate::decode::Instr;
use crate::mem::GuestRam;
use crate::mmu::{Access, Tlb};
use crate::trap::Exception;
use crate::{Clock, IrqLine, PrivMode, VmExit, decode};

/// Instructions executed between `kick` polls. The kick is architecturally
/// asynchronous, so observing it a few hundred instructions late is legal
/// (QEMU polls at translation-block boundaries) and it keeps a cross-thread
/// atomic RMW off the per-instruction path. At interpreter speeds this is
/// well under a microsecond of latency.
const KICK_POLL: u64 = 256;

const DCACHE_BITS: u32 = 14;
const DCACHE_SIZE: usize = 1 << DCACHE_BITS;

/// One memoized decode. The empty value is `raw = 0`, which is genuinely the
/// decode of an all-zero halfword, so a fresh cache needs no separate valid
/// bit.
#[derive(Clone, Copy)]
struct DecodeEntry {
    raw: u32,
    instr: Instr,
}

pub(crate) const DPAGE_SIZE: usize = 16;

/// One primed data page: `tag` is `(vpn << 4) | data-context` (see
/// `Cpu::data_tag`) and `off` its RAM byte offset. Slots are only filled for
/// pages that lie wholly inside RAM with no PMP entry active, so a hit
/// authorizes any access of the recorded kind anywhere in the page without a
/// bounds, permission or PMP re-check. `u64::MAX` is never a valid tag.
#[derive(Clone, Copy)]
pub(crate) struct DataPage {
    pub(crate) tag: u64,
    pub(crate) off: usize,
}

pub(crate) const EMPTY_DATA_PAGE: DataPage = DataPage {
    tag: u64::MAX,
    off: 0,
};

fn new_dcache() -> Box<[DecodeEntry; DCACHE_SIZE]> {
    let empty = DecodeEntry {
        raw: 0,
        instr: Instr::Illegal(0),
    };
    let v = vec![empty; DCACHE_SIZE].into_boxed_slice();
    // SAFETY: the allocation holds exactly DCACHE_SIZE elements.
    unsafe { Box::from_raw(Box::into_raw(v) as *mut [DecodeEntry; DCACHE_SIZE]) }
}

/// State of an in-flight MMIO load: the instruction has not retired, so pc
/// stays at the load until `complete_mmio_read()` supplies the data.
pub(crate) struct PendingLoad {
    pub rd: u8,
    pub size: u8,
    pub sign: bool,
    /// Destination is an f-register (flw/fld); flw results are NaN-boxed.
    pub freg: bool,
    pub next_pc: u64,
}

/// LR/SC reservation, recorded by LR: physical address, access width, and
/// the value observed. SC succeeds by compare-exchanging the reserved word
/// against `val`, so conflicting writes are detected by value rather than
/// by monitoring the address. Caveat (ABA): if other harts change the word
/// and restore the exact original value between LR and SC, the SC succeeds
/// where real hardware could fail it. Protocols built on LR/SC (locks,
/// refcounts, lock-free lists over word-sized state) remain correct because
/// the final compare-exchange itself is still atomic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reservation {
    pub addr: u64,
    pub width: u8,
    pub val: u64,
}

/// Cross-thread handle for kicking a hart and driving its interrupt lines.
#[derive(Clone)]
pub struct CpuIntc {
    kick: Arc<AtomicBool>,
    irq_lines: Arc<AtomicU64>,
}

impl CpuIntc {
    pub fn kick(&self) {
        self.kick.store(true, Ordering::Release);
    }

    pub fn set_irq(&self, line: IrqLine, level: bool) {
        let bit = 1u64 << (line as u32);
        if level {
            self.irq_lines.fetch_or(bit, Ordering::AcqRel);
        } else {
            self.irq_lines.fetch_and(!bit, Ordering::AcqRel);
        }
    }
}

pub struct Cpu {
    pub xregs: [u64; 32],
    /// F/D register file (storage only until the FP code lands).
    pub fregs: [u64; 32],
    pub pc: u64,
    pub priv_mode: PrivMode,
    pub csrs: CsrFile,
    /// LR/SC reservation (A extension). Consumed by SC; cleared by any
    /// store or AMO executed by this hart.
    pub reservation: Option<Reservation>,
    /// When set, `ecall` from S-mode exits with `VmExit::Ecall` (SBI call)
    /// instead of trapping. Off by default (riscv-tests semantics).
    pub sbi_mode: bool,
    pub(crate) ram: GuestRam,
    pub(crate) clock: Clock,
    pub(crate) instret: u64,
    pub(crate) pending_load: Option<PendingLoad>,
    pub(crate) tlb: Tlb,
    /// Page the fetch fast path is primed for, tagged `(vpn << 2) | priv`;
    /// `u64::MAX` (never a valid tag) marks it empty. Fetch permission
    /// depends on the privilege mode and on nothing else in mstatus, so the
    /// mode rides in the tag instead of needing an invalidation hook.
    fetch_tag: u64,
    /// RAM byte offset of that page's physical base. Only installed when the
    /// whole 4 KiB page lies inside RAM and no PMP entry is active, so a hit
    /// needs neither a bounds check nor a PMP re-check. What the translation
    /// resolves to can still change, so every TLB flush and every PMP
    /// reconfiguration invalidates the entry.
    fetch_page: usize,
    /// Direct-mapped data-page caches, indexed by the low virtual page
    /// number bits: loads and stores are kept apart so a read-only page
    /// primed for loads can never satisfy a store.
    pub(crate) load_pages: [DataPage; DPAGE_SIZE],
    pub(crate) store_pages: [DataPage; DPAGE_SIZE],
    /// Snapshot of the external interrupt lines, refreshed between polls.
    /// Lines are asserted by other threads and interrupts are asynchronous,
    /// so serving the per-instruction pending check from a snapshot keeps a
    /// shared-memory load off the hot path without changing what the guest
    /// can observe. CSR reads of mip/sip and the WFI wake condition still
    /// consult the atomic directly.
    mip_ext: u64,
    /// Direct-mapped decode cache. Entries are indexed by pc but *validated*
    /// against the raw word re-read from memory on every hit, which makes the
    /// cache a pure memoization of `decode` (a function of the raw bits
    /// alone). Nothing can invalidate it: self-modifying code, satp changes
    /// and remote fences either change the raw bits — and miss — or leave
    /// them identical, in which case the cached decode is still the answer.
    dcache: Box<[DecodeEntry; DCACHE_SIZE]>,
    /// Cached "some PMP entry has A != OFF"; kept in sync by pmpcfg writes.
    pub(crate) pmp_active: bool,
    /// Store hook: a store to exactly this physical address exits with
    /// `VmExit::Shutdown` carrying the 64-bit RAM word (HTIF tohost).
    tohost_addr: Option<u64>,
    kick: Arc<AtomicBool>,
    irq_lines: Arc<AtomicU64>,
}

impl Cpu {
    pub fn new(hart_id: u64, ram: GuestRam, clock: Clock) -> Self {
        Cpu {
            xregs: [0; 32],
            fregs: [0; 32],
            pc: 0,
            priv_mode: PrivMode::M,
            csrs: CsrFile::new(hart_id),
            reservation: None,
            sbi_mode: false,
            ram,
            clock,
            instret: 0,
            pending_load: None,
            tlb: Tlb::new(),
            fetch_tag: u64::MAX,
            fetch_page: 0,
            load_pages: [EMPTY_DATA_PAGE; DPAGE_SIZE],
            store_pages: [EMPTY_DATA_PAGE; DPAGE_SIZE],
            mip_ext: 0,
            dcache: new_dcache(),
            pmp_active: false,
            tohost_addr: None,
            kick: Arc::new(AtomicBool::new(false)),
            irq_lines: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn set_pc(&mut self, pc: u64) {
        self.pc = pc;
    }

    pub fn read_reg(&self, r: usize) -> u64 {
        self.xregs[r]
    }

    pub fn set_reg(&mut self, r: usize, val: u64) {
        if r != 0 {
            self.xregs[r] = val;
        }
    }

    #[inline]
    pub(crate) fn set_x(&mut self, rd: u8, val: u64) {
        if rd != 0 {
            self.xregs[rd as usize] = val;
        }
    }

    /// Host-side CSR access (no privilege checks beyond current mode).
    pub fn read_csr(&mut self, addr: u16) -> Result<u64, Exception> {
        self.csr_read(addr)
    }

    pub fn write_csr(&mut self, addr: u16, val: u64) -> Result<(), Exception> {
        self.csr_write(addr, val)
    }

    pub fn instret(&self) -> u64 {
        self.instret
    }

    /// Clear bits in misa (e.g. F | D = 0x28 while no FPU is wired up, so
    /// environments that probe misa never touch FP state).
    pub fn mask_misa(&mut self, bits: u64) {
        self.csrs.misa &= !bits;
    }

    pub fn set_tohost(&mut self, addr: Option<u64>) {
        self.tohost_addr = addr;
        // Stores served from a primed page never see the hook.
        self.invalidate_data_pages();
    }

    pub(crate) fn tohost_addr(&self) -> Option<u64> {
        self.tohost_addr
    }

    pub fn intc(&self) -> CpuIntc {
        CpuIntc {
            kick: self.kick.clone(),
            irq_lines: self.irq_lines.clone(),
        }
    }

    pub fn kick(&self) {
        self.kick.store(true, Ordering::Release);
    }

    pub fn set_irq(&self, line: IrqLine, level: bool) {
        self.intc().set_irq(line, level);
    }

    /// Set or clear the guest-visible supervisor software-interrupt pending
    /// bit (mip.SSIP), the delivery path for SBI IPIs.
    ///
    /// This is the software-writable mip bit rather than the external
    /// `IrqLine::SSoft` line precisely so the guest can clear it by writing
    /// `sip`, exactly as on real hardware; the external line would latch.
    /// It is per-hart state, so an IPI from another hart has to be handed to
    /// the target's own thread before it lands here.
    pub fn set_soft_ip(&mut self, level: bool) {
        if level {
            self.csrs.mip |= MIP_SSIP;
        } else {
            self.csrs.mip &= !MIP_SSIP;
        }
    }

    /// mip as the guest sees it: software-writable bits OR external lines.
    pub(crate) fn effective_mip(&self) -> u64 {
        self.csrs.mip | self.irq_lines.load(Ordering::Acquire)
    }

    /// The same view built from the run loop's line snapshot.
    #[inline]
    pub(crate) fn effective_mip_snapshot(&self) -> u64 {
        self.csrs.mip | self.mip_ext
    }

    #[inline]
    fn sync_irq_lines(&mut self) {
        self.mip_ext = self.irq_lines.load(Ordering::Acquire);
    }

    /// Complete an MmioRead exit: write back the loaded data and retire the
    /// load. Must be called exactly once per MmioRead before resuming.
    pub fn complete_mmio_read(&mut self, data: u64) {
        let p = self.pending_load.take().expect("no pending MMIO read");
        if p.freg {
            let bits = if p.size == 4 {
                0xffff_ffff_0000_0000 | (data & 0xffff_ffff)
            } else {
                data
            };
            self.fregs[p.rd as usize] = bits;
            self.csrs.mstatus |= crate::csr::MSTATUS_FS_MASK;
        } else {
            let val = extend(data, p.size as usize, p.sign);
            self.set_x(p.rd, val);
        }
        self.pc = p.next_pc;
        self.instret += 1;
    }

    /// Run up to `max_insns` instructions.
    pub fn run(&mut self, max_insns: u64) -> VmExit {
        debug_assert!(
            self.pending_load.is_none(),
            "run() with an incomplete MMIO read"
        );
        let mut remaining = max_insns;
        while remaining > 0 {
            if self.kick.swap(false, Ordering::AcqRel) {
                return VmExit::Interrupted;
            }
            self.sync_irq_lines();
            let chunk = remaining.min(KICK_POLL);
            remaining -= chunk;
            for _ in 0..chunk {
                if let Some(code) = self.pending_interrupt() {
                    self.take_interrupt(code);
                }
                let pc = self.pc;
                match self.step(pc) {
                    Ok(None) => {}
                    Ok(Some(exit)) => return exit,
                    Err(e) => self.take_exception(e, pc),
                }
            }
        }
        VmExit::InstrLimit
    }

    fn step(&mut self, pc: u64) -> Result<Option<VmExit>, Exception> {
        let (raw, len) = self.fetch(pc)?;
        // Folding the high pc bits into the index spreads hot code that is
        // more than one cache span apart instead of aliasing it.
        let idx = ((pc >> 1) ^ (pc >> (DCACHE_BITS + 1))) as usize & (DCACHE_SIZE - 1);
        let instr = if self.dcache[idx].raw == raw {
            self.dcache[idx].instr
        } else {
            let instr = if len == 4 {
                decode::decode(raw)
            } else {
                decode::decode_compressed(raw as u16)
            };
            self.dcache[idx] = DecodeEntry { raw, instr };
            instr
        };
        self.execute(instr, raw, pc, len)
    }

    /// Fetch one instruction and its length. IALIGN=16 (C implemented): pc is
    /// always even by construction (JALR masks bit 0, branch/JAL immediates
    /// are even, xepc/xtvec writes are masked), so no misaligned-fetch check
    /// is needed here.
    ///
    /// The fast path covers the common case — an instruction wholly inside a
    /// page already primed in `fetch_page` — with one unaligned host read and
    /// no translation. `0xffc` is the last offset at which four bytes still
    /// fit in the page.
    #[inline]
    fn fetch(&mut self, pc: u64) -> Result<(u32, u64), Exception> {
        let off = (pc & 0xfff) as usize;
        if self.fetch_tag_for(pc) == self.fetch_tag && off <= 0xffc {
            // SAFETY: fetch_page is the offset of a 4 KiB page fully inside
            // RAM, so off + 4 is in bounds.
            let word = unsafe { self.ram.read_u32_unchecked(self.fetch_page + off) };
            return Ok(if word & 3 == 3 {
                (word, 4)
            } else {
                (word & 0xffff, 2)
            });
        }
        let lo = self.fetch_half(pc)? as u32;
        if lo & 3 == 3 {
            let hi = self.fetch_half(pc.wrapping_add(2))? as u32;
            Ok(((hi << 16) | lo, 4))
        } else {
            Ok((lo, 2))
        }
    }

    /// One 16-bit fetch parcel, translated independently: a 4-byte
    /// instruction straddling a page boundary translates each half, and a
    /// fault reports the virtual address of the faulting half in xtval
    /// (while xepc stays at the instruction start). Primes the fetch fast
    /// path on the way through.
    fn fetch_half(&mut self, va: u64) -> Result<u16, Exception> {
        let pa = self.translate(va, Access::Fetch, self.priv_mode, 2)?;
        if !self.pmp_active
            && let Some(off) = self.ram.page_offset(pa)
        {
            self.fetch_tag = self.fetch_tag_for(va);
            self.fetch_page = off;
        }
        self.ram
            .load(pa, 2)
            .map(|v| v as u16)
            .ok_or(Exception::InstrAccessFault(va))
    }

    #[inline]
    fn fetch_tag_for(&self, va: u64) -> u64 {
        ((va >> 10) & !3) | self.priv_mode as u64
    }

    /// Drop the fetch fast path. Called wherever what a fetch of the primed
    /// page resolves to, or is permitted to do, can have changed.
    #[inline]
    pub(crate) fn invalidate_fetch_page(&mut self) {
        self.fetch_tag = u64::MAX;
    }
}

/// Truncate to `size` bytes and sign- or zero-extend to 64 bits.
pub(crate) fn extend(data: u64, size: usize, sign: bool) -> u64 {
    match (size, sign) {
        (1, false) => data as u8 as u64,
        (1, true) => data as i8 as u64,
        (2, false) => data as u16 as u64,
        (2, true) => data as i16 as u64,
        (4, false) => data as u32 as u64,
        (4, true) => data as i32 as u64,
        _ => data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cpu_with_prog(words: &[u32]) -> Cpu {
        let ram = GuestRam::new_owned(0, 64 * 1024);
        for (i, w) in words.iter().enumerate() {
            ram.store(i as u64 * 4, 4, *w as u64).unwrap();
        }
        Cpu::new(0, ram, Clock::Deterministic { shift: 0 })
    }

    #[test]
    fn arith_and_instr_limit() {
        // addi x1, x0, 5; add x2, x1, x1; jal x0, 0
        let mut cpu = cpu_with_prog(&[0x0050_0093, 0x0010_8133, 0x0000_006f]);
        assert_eq!(cpu.run(10), VmExit::InstrLimit);
        assert_eq!(cpu.xregs[1], 5);
        assert_eq!(cpu.xregs[2], 10);
        assert_eq!(cpu.instret(), 10);
        assert_eq!(cpu.pc, 8);
    }

    #[test]
    fn load_sign_extension() {
        // lb x3, 0x100(x0); lbu x4, 0x100(x0); jal x0, 0
        let mut cpu = cpu_with_prog(&[0x1000_0183, 0x1000_4203, 0x0000_006f]);
        cpu.ram.store(0x100, 1, 0x80).unwrap();
        cpu.run(2);
        assert_eq!(cpu.xregs[3], 0x80u64 as i8 as u64);
        assert_eq!(cpu.xregs[4], 0x80);
    }

    #[test]
    fn mmio_read_protocol() {
        // lui x5, 0x100; lw x6, 0(x5)
        let mut cpu = cpu_with_prog(&[0x0010_02b7, 0x0002_a303]);
        assert_eq!(
            cpu.run(10),
            VmExit::MmioRead {
                addr: 0x10_0000,
                size: 4
            }
        );
        // pc must sit on the unretired load.
        assert_eq!(cpu.pc, 4);
        assert_eq!(cpu.instret(), 1);
        cpu.complete_mmio_read(0xdead_beef);
        assert_eq!(cpu.xregs[6], 0xffff_ffff_dead_beef);
        assert_eq!(cpu.pc, 8);
        assert_eq!(cpu.instret(), 2);
    }

    #[test]
    fn mmio_write_exit() {
        // lui x5, 0x100; sw x5, 4(x5)
        let mut cpu = cpu_with_prog(&[0x0010_02b7, 0x0052_a223]);
        assert_eq!(
            cpu.run(10),
            VmExit::MmioWrite {
                addr: 0x10_0004,
                size: 4,
                data: 0x10_0000
            }
        );
        assert_eq!(cpu.pc, 8);
    }

    #[test]
    fn csr_roundtrip() {
        // csrrwi x0, mscratch, 5; csrrs x7, mscratch, x0; jal x0, 0
        let mut cpu = cpu_with_prog(&[0x3402_d073, 0x3400_23f3, 0x0000_006f]);
        cpu.run(2);
        assert_eq!(cpu.xregs[7], 5);
        // misa: writes ignored, reads back RV64 + extensions.
        let misa = cpu.read_csr(0x301).unwrap();
        cpu.write_csr(0x301, 0).unwrap();
        assert_eq!(cpu.read_csr(0x301).unwrap(), misa);
        assert_eq!(misa >> 62, 2);
        assert_ne!(misa & (1 << 8), 0); // I
        assert_ne!(misa & (1 << 20), 0); // U
    }

    #[test]
    fn ecall_traps_to_mtvec() {
        // lui x1, 0x1; csrrw x0, mtvec, x1; ecall
        // handler at 0x1000: csrrs x7, mcause, x0; jal x0, 0
        let mut cpu = cpu_with_prog(&[0x0000_10b7, 0x3050_9073, 0x0000_0073]);
        cpu.ram.store(0x1000, 4, 0x3420_23f3).unwrap();
        cpu.ram.store(0x1004, 4, 0x0000_006f).unwrap();
        cpu.run(4);
        assert_eq!(cpu.xregs[7], 11); // ecall from M
        assert_eq!(cpu.read_csr(0x341).unwrap(), 8); // mepc
        assert_eq!(cpu.pc, 0x1004);
    }

    #[test]
    fn illegal_instruction_sets_mtval() {
        let mut cpu = cpu_with_prog(&[0xffff_ffff]);
        cpu.write_csr(0x305, 0x1000).unwrap();
        cpu.run(1);
        assert_eq!(cpu.pc, 0x1000);
        assert_eq!(cpu.read_csr(0x342).unwrap(), 2); // mcause
        assert_eq!(cpu.read_csr(0x343).unwrap(), 0xffff_ffff); // mtval
    }

    #[test]
    fn mret_restores_mode() {
        let mut cpu = cpu_with_prog(&[0x3020_0073]); // mret
        cpu.write_csr(0x341, 0x100).unwrap(); // mepc
        cpu.run(1);
        assert_eq!(cpu.pc, 0x100);
        assert_eq!(cpu.priv_mode, PrivMode::U); // MPP was 0
    }

    #[test]
    fn wfi_and_interrupt() {
        let mut cpu = cpu_with_prog(&[0x1050_0073, 0x0000_006f]); // wfi; loop
        assert_eq!(cpu.run(10), VmExit::Wfi);
        assert_eq!(cpu.pc, 4);

        // Pending-and-enabled MTIP turns wfi into a nop.
        cpu.set_irq(IrqLine::MTimer, true);
        cpu.write_csr(0x304, 0x80).unwrap(); // mie.MTIE
        cpu.pc = 0;
        assert_eq!(cpu.run(2), VmExit::InstrLimit);

        // With mstatus.MIE the interrupt is actually taken.
        cpu.write_csr(0x305, 0x2000).unwrap(); // mtvec
        cpu.ram.store(0x2000, 4, 0x0000_006f).unwrap();
        cpu.write_csr(0x300, 0x8).unwrap(); // mstatus.MIE
        cpu.pc = 0;
        cpu.run(1);
        assert_eq!(cpu.pc, 0x2000);
        assert_eq!(cpu.read_csr(0x342).unwrap(), (1 << 63) | 7);
    }

    /// The decode cache is validated against the raw word re-read from
    /// memory, so code rewritten in place is picked up with no fence.i.
    #[test]
    fn self_modifying_code_is_not_cached() {
        // addi x1, x0, 5; jal x0, 0
        let mut cpu = cpu_with_prog(&[0x0050_0093, 0x0000_006f]);
        cpu.run(4);
        assert_eq!(cpu.xregs[1], 5);
        // Rewrite as addi x1, x0, 9 and re-enter at the same pc.
        cpu.ram.store(0, 4, 0x0090_0093).unwrap();
        cpu.pc = 0;
        cpu.run(4);
        assert_eq!(cpu.xregs[1], 9);
    }

    #[test]
    fn kick_interrupts_run() {
        let mut cpu = cpu_with_prog(&[0x0000_006f]);
        cpu.kick();
        assert_eq!(cpu.run(1000), VmExit::Interrupted);
        assert_eq!(cpu.run(1), VmExit::InstrLimit);
    }

    #[test]
    fn tohost_store_exits_shutdown() {
        // addi x1, x0, 1; sw x1, 0x200(x0)
        let mut cpu = cpu_with_prog(&[0x0010_0093, 0x2010_2023]);
        cpu.set_tohost(Some(0x200));
        assert_eq!(cpu.run(10), VmExit::Shutdown { code: 1 });
        assert_eq!(cpu.pc, 8);
    }

    /// AMO/LR/SC encoder: funct5, rs2, rs1, funct3 (2 = .w, 3 = .d), rd.
    fn amo(f5: u32, rs2: u32, rs1: u32, f3: u32, rd: u32) -> u32 {
        (f5 << 27) | (rs2 << 20) | (rs1 << 15) | (f3 << 12) | (rd << 7) | 0x2f
    }

    #[test]
    fn lr_sc_roundtrip() {
        let mut cpu = cpu_with_prog(&[
            0x1000_0293,            // addi x5, x0, 0x100
            amo(0x02, 0, 5, 2, 6),  // lr.w x6, (x5)
            0x0013_0313,            // addi x6, x6, 1
            amo(0x03, 6, 5, 2, 7),  // sc.w x7, x6, (x5) -> succeeds
            amo(0x03, 6, 5, 2, 28), // sc.w x28, x6, (x5) -> no reservation
            0x0000_006f,            // loop
        ]);
        cpu.ram.store(0x100, 4, 41).unwrap();
        cpu.run(5);
        assert_eq!(cpu.xregs[6], 42);
        assert_eq!(cpu.xregs[7], 0);
        assert_eq!(cpu.ram.load(0x100, 4), Some(42));
        assert_eq!(cpu.xregs[28], 1);
    }

    #[test]
    fn store_clears_reservation() {
        let mut cpu = cpu_with_prog(&[
            0x1000_0293,           // addi x5, x0, 0x100
            amo(0x02, 0, 5, 2, 6), // lr.w x6, (x5)
            0x0062_a423,           // sw x6, 8(x5)
            amo(0x03, 6, 5, 2, 7), // sc.w x7, x6, (x5) -> fails
            0x0000_006f,
        ]);
        cpu.ram.store(0x100, 4, 9).unwrap();
        cpu.run(4);
        assert_eq!(cpu.xregs[7], 1);
        assert_eq!(cpu.ram.load(0x100, 4), Some(9));
    }

    #[test]
    fn sc_wrong_address_or_width_fails() {
        let mut cpu = cpu_with_prog(&[
            0x1000_0293,            // addi x5, x0, 0x100
            amo(0x02, 0, 5, 3, 6),  // lr.d x6, (x5)
            amo(0x03, 6, 5, 2, 7),  // sc.w to the same addr, wrong width
            amo(0x02, 0, 5, 2, 6),  // lr.w x6, (x5)
            0x0082_8293,            // addi x5, x5, 8
            amo(0x03, 6, 5, 2, 28), // sc.w to a different addr
            0x0000_006f,
        ]);
        cpu.run(6);
        assert_eq!(cpu.xregs[7], 1);
        assert_eq!(cpu.xregs[28], 1);
    }

    #[test]
    fn amo_ops_and_sign_extension() {
        let mut cpu = cpu_with_prog(&[
            0x1000_0293,            // addi x5, x0, 0x100
            0x0070_0313,            // addi x6, x0, 7
            amo(0x00, 6, 5, 2, 7),  // amoadd.w x7, x6, (x5)
            amo(0x14, 6, 5, 2, 28), // amomax.w x28, x6, (x5)
            0x0000_006f,
        ]);
        cpu.ram.store(0x100, 4, (-5i32) as u32 as u64).unwrap();
        cpu.run(4);
        // Old value -5 sign-extends into rd; memory holds -5 + 7 = 2.
        assert_eq!(cpu.xregs[7], (-5i64) as u64);
        // amomax.w: old 2 into rd; max(2, 7) = 7 stored.
        assert_eq!(cpu.xregs[28], 2);
        assert_eq!(cpu.ram.load(0x100, 4), Some(7));
    }

    #[test]
    fn amo_misaligned_and_mmio_fault() {
        // Misaligned AMO -> store/AMO address-misaligned (mcause 6).
        let mut cpu = cpu_with_prog(&[
            0x1020_0293,           // addi x5, x0, 0x102
            amo(0x00, 6, 5, 2, 7), // amoadd.w
        ]);
        cpu.write_csr(0x305, 0x1000).unwrap();
        cpu.run(2);
        assert_eq!(cpu.pc, 0x1000);
        assert_eq!(cpu.read_csr(0x342).unwrap(), 6);
        assert_eq!(cpu.read_csr(0x343).unwrap(), 0x102);

        // Aligned AMO outside RAM -> store/AMO access fault (mcause 7),
        // never an MMIO exit.
        let mut cpu = cpu_with_prog(&[
            0x0010_02b7,           // lui x5, 0x100
            amo(0x00, 6, 5, 2, 7), // amoadd.w
        ]);
        cpu.write_csr(0x305, 0x1000).unwrap();
        assert_eq!(cpu.run(2), VmExit::InstrLimit);
        assert_eq!(cpu.pc, 0x1000);
        assert_eq!(cpu.read_csr(0x342).unwrap(), 7);
        assert_eq!(cpu.read_csr(0x343).unwrap(), 0x10_0000);
    }

    #[test]
    fn compressed_pc_stepping() {
        // c.li a0, 5; c.addi a0, 1; c.j 0 (self-loop)
        let mut cpu = cpu_with_prog(&[0x0505_4515, 0x0000_a001]);
        assert_eq!(cpu.run(3), VmExit::InstrLimit);
        assert_eq!(cpu.xregs[10], 6);
        assert_eq!(cpu.pc, 4);
        assert_eq!(cpu.instret(), 3);
    }

    #[test]
    fn mixed_width_fetch() {
        // c.li a0, 3; addi x11, x10, 100 (32-bit at offset 2); c.j 0
        let mut cpu = cpu_with_prog(&[0x0593_450d, 0xa001_0645]);
        cpu.run(3);
        assert_eq!(cpu.xregs[10], 3);
        assert_eq!(cpu.xregs[11], 103);
        assert_eq!(cpu.pc, 6);
    }

    #[test]
    fn illegal_compressed_sets_mtval() {
        // 0x8000 is a reserved RVC encoding; mtval gets the 16-bit bits
        // zero-extended.
        let mut cpu = cpu_with_prog(&[0x0000_8000]);
        cpu.write_csr(0x305, 0x1000).unwrap();
        cpu.run(1);
        assert_eq!(cpu.pc, 0x1000);
        assert_eq!(cpu.read_csr(0x342).unwrap(), 2);
        assert_eq!(cpu.read_csr(0x343).unwrap(), 0x8000);
    }

    #[test]
    fn deterministic_time_csr() {
        // csrrs x7, time, x0 twice around some nops
        let mut cpu = cpu_with_prog(&[
            0x0000_0013, // nop
            0x0000_0013,
            0x0000_0013,
            0x0000_0013,
            0xc010_23f3, // csrrs x7, time, x0
            0x0000_006f,
        ]);
        cpu.run(5);
        assert_eq!(cpu.xregs[7], 4); // instret was 4 at the read, shift 0
    }
}
