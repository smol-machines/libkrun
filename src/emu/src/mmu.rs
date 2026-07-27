// Copyright 2026 The libkrun Authors. Licensed under Apache-2.0.
//
// Sv39/Sv48 address translation with a small direct-mapped TLB, plus PMP
// (NAPOT/NA4/TOR) checks on the resulting physical addresses.

use crate::PrivMode;
use crate::cpu::{Cpu, DPAGE_SIZE, DataPage};
use crate::csr::{MSTATUS_MPRV, MSTATUS_MXR, MSTATUS_SUM};
use crate::trap::Exception;

pub const PTE_V: u64 = 1 << 0;
pub const PTE_R: u64 = 1 << 1;
pub const PTE_W: u64 = 1 << 2;
pub const PTE_X: u64 = 1 << 3;
pub const PTE_U: u64 = 1 << 4;
pub const PTE_A: u64 = 1 << 6;
pub const PTE_D: u64 = 1 << 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
    Fetch,
}

pub(crate) enum LoadResult {
    /// Zero-extended raw value read from RAM.
    Ram(u64),
    /// Physical MMIO address; the caller builds the exit.
    Mmio(u64),
}

pub(crate) enum StoreResult {
    /// Store completed in RAM at this physical address.
    Ram(u64),
    /// Physical MMIO address; the caller builds the exit.
    Mmio(u64),
}

const TLB_SIZE: usize = 64;

/// One cached leaf translation for a single 4 KiB virtual page (superpage
/// walks are cached per 4 KiB page too). Permissions are re-checked on every
/// hit against the current privilege, SUM/MXR, and access type, so privilege
/// or mstatus changes never require a flush; satp writes and sfence.vma do.
#[derive(Clone, Copy)]
struct TlbEntry {
    /// Virtual page number; u64::MAX marks an empty slot.
    vpn: u64,
    /// Physical page base (4 KiB-aligned).
    pa_base: u64,
    /// Leaf PTE bits (R/W/X/U/A/D).
    pte: u64,
}

pub(crate) struct Tlb {
    entries: [TlbEntry; TLB_SIZE],
}

impl Tlb {
    pub(crate) fn new() -> Self {
        Tlb {
            entries: [TlbEntry {
                vpn: u64::MAX,
                pa_base: 0,
                pte: 0,
            }; TLB_SIZE],
        }
    }

    pub(crate) fn flush(&mut self) {
        for e in &mut self.entries {
            e.vpn = u64::MAX;
        }
    }
}

fn page_fault(access: Access, va: u64) -> Exception {
    match access {
        Access::Read => Exception::LoadPageFault(va),
        Access::Write => Exception::StorePageFault(va),
        Access::Fetch => Exception::InstrPageFault(va),
    }
}

fn access_fault(access: Access, va: u64) -> Exception {
    match access {
        Access::Read => Exception::LoadAccessFault(va),
        Access::Write => Exception::StoreAccessFault(va),
        Access::Fetch => Exception::InstrAccessFault(va),
    }
}

fn crosses_page(va: u64, size: usize) -> bool {
    (va & 0xfff) + size as u64 > 0x1000
}

impl Cpu {
    /// Flush this hart's address-translation caches. Called by `sfence.vma`,
    /// by accepted `satp` writes, and by the embedder when a remote fence
    /// (SBI RFENCE) targets this hart.
    pub fn flush_tlb(&mut self) {
        self.tlb.flush();
        self.invalidate_fetch_page();
        self.invalidate_data_pages();
    }

    pub(crate) fn invalidate_data_pages(&mut self) {
        for e in &mut self.load_pages {
            e.tag = u64::MAX;
        }
        for e in &mut self.store_pages {
            e.tag = u64::MAX;
        }
    }

    /// Everything a leaf permission check consults besides the PTE itself:
    /// the effective privilege for data accesses (mstatus.MPRV substitutes
    /// MPP) plus SUM and MXR. Carrying it in the data-page tag means a
    /// privilege or mstatus change retires the primed pages by missing,
    /// with no invalidation hook to forget.
    #[inline]
    fn data_ctx(&self) -> u64 {
        let m = self.csrs.mstatus;
        let prv = if m & MSTATUS_MPRV != 0 {
            (m >> 11) & 3
        } else {
            self.priv_mode as u64
        };
        prv | ((m >> 16) & 0xc)
    }

    #[inline]
    fn data_tag(&self, va: u64) -> u64 {
        ((va >> 8) & !0xf) | self.data_ctx()
    }

    /// RAM byte offset for a load that stays inside an already-primed page,
    /// or `None` to take the full [`Cpu::load_vaddr`] path.
    #[inline]
    pub(crate) fn fast_load_off(&self, va: u64, size: usize) -> Option<usize> {
        let e = self.load_pages[(va >> 12) as usize & (DPAGE_SIZE - 1)];
        let off = (va & 0xfff) as usize;
        if off + size <= 0x1000 && e.tag == self.data_tag(va) {
            Some(e.off + off)
        } else {
            None
        }
    }

    /// As [`Cpu::fast_load_off`], for stores.
    #[inline]
    pub(crate) fn fast_store_off(&self, va: u64, size: usize) -> Option<usize> {
        let e = self.store_pages[(va >> 12) as usize & (DPAGE_SIZE - 1)];
        let off = (va & 0xfff) as usize;
        if off + size <= 0x1000 && e.tag == self.data_tag(va) {
            Some(e.off + off)
        } else {
            None
        }
    }

    /// Prime the fast path for `va`, which just translated to `pa` under the
    /// current context. PMP can deny sub-page ranges, and the HTIF `tohost`
    /// hook must see every store, so neither is compatible with a page-wide
    /// pass.
    fn prime_data_page(&mut self, va: u64, pa: u64, write: bool) {
        if self.pmp_active || (write && self.tohost_addr().is_some()) {
            return;
        }
        if let Some(page) = self.ram.page_offset(pa) {
            let entry = DataPage {
                tag: self.data_tag(va),
                off: page,
            };
            let idx = (va >> 12) as usize & (DPAGE_SIZE - 1);
            if write {
                self.store_pages[idx] = entry;
            } else {
                self.load_pages[idx] = entry;
            }
        }
    }

    /// Effective privilege for data accesses: mstatus.MPRV substitutes MPP.
    pub(crate) fn data_priv(&self) -> PrivMode {
        if self.csrs.mstatus & MSTATUS_MPRV != 0 {
            match (self.csrs.mstatus >> 11) & 3 {
                0 => PrivMode::U,
                1 => PrivMode::S,
                _ => PrivMode::M,
            }
        } else {
            self.priv_mode
        }
    }

    /// Whether accesses at privilege `prv` go through page translation.
    pub(crate) fn vm_active(&self, prv: PrivMode) -> bool {
        prv != PrivMode::M && self.csrs.satp >> 60 != 0
    }

    /// Translate one virtual address for an access of `size` bytes that does
    /// not cross a page boundary: Sv39/Sv48 translation (when active for
    /// `prv`) followed by a PMP check on the physical address.
    pub(crate) fn translate(
        &mut self,
        va: u64,
        access: Access,
        prv: PrivMode,
        size: u64,
    ) -> Result<u64, Exception> {
        let pa = if self.vm_active(prv) {
            self.translate_mapped(va, access, prv)?
        } else {
            va
        };
        if self.pmp_active && !self.pmp_ok(pa, size, access, prv) {
            return Err(access_fault(access, va));
        }
        Ok(pa)
    }

    fn translate_mapped(
        &mut self,
        va: u64,
        access: Access,
        prv: PrivMode,
    ) -> Result<u64, Exception> {
        let vpn = va >> 12;
        let idx = (vpn as usize) & (TLB_SIZE - 1);
        let e = self.tlb.entries[idx];
        if e.vpn == vpn {
            self.check_leaf(e.pte, access, prv)
                .map_err(|()| page_fault(access, va))?;
            return Ok(e.pa_base | (va & 0xfff));
        }
        let (pa_base, pte) = self.walk(va, access, prv)?;
        self.tlb.entries[idx] = TlbEntry { vpn, pa_base, pte };
        Ok(pa_base | (va & 0xfff))
    }

    /// Sv39/Sv48 page-table walk. Returns the physical page base for `va`'s
    /// 4 KiB page and the leaf PTE. A and D are never updated in memory
    /// (Svade): software sees a page fault and sets them itself.
    fn walk(&mut self, va: u64, access: Access, prv: PrivMode) -> Result<(u64, u64), Exception> {
        let satp = self.csrs.satp;
        let levels: u32 = if satp >> 60 == 8 { 3 } else { 4 };
        let fault = || page_fault(access, va);

        // Bits above the VA space must replicate its most significant bit.
        let va_bits = 12 + 9 * levels;
        if ((va as i64) << (64 - va_bits)) >> (64 - va_bits) != va as i64 {
            return Err(fault());
        }

        let mut base = (satp & 0xfff_ffff_ffff) << 12;
        for lvl in (0..levels).rev() {
            let pte_addr = base + ((va >> (12 + 9 * lvl)) & 0x1ff) * 8;
            if self.pmp_active && !self.pmp_ok(pte_addr, 8, Access::Read, prv) {
                return Err(access_fault(access, va));
            }
            // A PTE outside RAM (PMA violation) is an access fault of the
            // original access type.
            let pte = self
                .ram
                .load(pte_addr, 8)
                .ok_or_else(|| access_fault(access, va))?;
            // Invalid, W-without-R, or use of the reserved/unimplemented
            // high bits (Svnapot N, Svpbmt PBMT, reserved 60:54).
            if pte & PTE_V == 0 || (pte & PTE_W != 0 && pte & PTE_R == 0) || pte >> 54 != 0 {
                return Err(fault());
            }
            let ppn = (pte >> 10) & 0xfff_ffff_ffff;
            if pte & (PTE_R | PTE_X) == 0 {
                // Pointer to the next level; a pointer at the last level is
                // invalid.
                if lvl == 0 {
                    return Err(fault());
                }
                base = ppn << 12;
                continue;
            }
            // Leaf. Superpages must be aligned to their own size.
            let low_mask = (1u64 << (9 * lvl)) - 1;
            if ppn & low_mask != 0 {
                return Err(fault());
            }
            self.check_leaf(pte, access, prv).map_err(|()| fault())?;
            return Ok((((ppn & !low_mask) | ((va >> 12) & low_mask)) << 12, pte));
        }
        unreachable!("walk terminates at level 0")
    }

    /// Leaf permission check: U/SUM/MXR rules, then R/W/X against the access
    /// type, then Svade A/D. `Err(())` means page fault.
    fn check_leaf(&self, pte: u64, access: Access, prv: PrivMode) -> Result<(), ()> {
        let mstatus = self.csrs.mstatus;
        if pte & PTE_U != 0 {
            // S touches user pages only for data, and only with SUM set.
            if prv == PrivMode::S && (access == Access::Fetch || mstatus & MSTATUS_SUM == 0) {
                return Err(());
            }
        } else if prv == PrivMode::U {
            return Err(());
        }
        let ok = match access {
            Access::Fetch => pte & PTE_X != 0,
            Access::Read => pte & PTE_R != 0 || (mstatus & MSTATUS_MXR != 0 && pte & PTE_X != 0),
            Access::Write => pte & PTE_W != 0,
        };
        if !ok || pte & PTE_A == 0 || (access == Access::Write && pte & PTE_D == 0) {
            return Err(());
        }
        Ok(())
    }

    pub(crate) fn load_vaddr(&mut self, va: u64, size: usize) -> Result<LoadResult, Exception> {
        let prv = self.data_priv();
        if crosses_page(va, size) && self.vm_active(prv) {
            // A misaligned access spanning two virtual pages accesses byte by
            // byte: the pages need not be physically contiguous.
            let mut val = 0;
            for k in 0..size as u64 {
                let b = va.wrapping_add(k);
                let pa = self.translate(b, Access::Read, prv, 1)?;
                let byte = self.ram.load(pa, 1).ok_or(Exception::LoadAccessFault(b))?;
                val |= byte << (8 * k);
            }
            return Ok(LoadResult::Ram(val));
        }
        let pa = self.translate(va, Access::Read, prv, size as u64)?;
        self.prime_data_page(va, pa, false);
        if let Some(v) = self.ram.load(pa, size) {
            return Ok(LoadResult::Ram(v));
        }
        if self.ram.contains(pa, 1) {
            // Starts in RAM but runs off the end.
            return Err(Exception::LoadAccessFault(va));
        }
        if pa & (size as u64 - 1) != 0 {
            // Misaligned accesses are byte-composed only in RAM; device
            // buses require natural alignment.
            return Err(Exception::LoadAddrMisaligned(va));
        }
        Ok(LoadResult::Mmio(pa))
    }

    pub(crate) fn store_vaddr(
        &mut self,
        va: u64,
        size: usize,
        val: u64,
    ) -> Result<StoreResult, Exception> {
        let prv = self.data_priv();
        if crosses_page(va, size) && self.vm_active(prv) {
            // Translate every byte before writing any, so a fault on the
            // second page leaves memory untouched.
            let mut pas = [0u64; 8];
            for (k, pa) in pas.iter_mut().enumerate().take(size) {
                let b = va.wrapping_add(k as u64);
                *pa = self.translate(b, Access::Write, prv, 1)?;
                if !self.ram.contains(*pa, 1) {
                    return Err(Exception::StoreAccessFault(b));
                }
            }
            for (k, pa) in pas.iter().enumerate().take(size) {
                self.ram.store(*pa, 1, (val >> (8 * k)) & 0xff).unwrap();
            }
            return Ok(StoreResult::Ram(pas[0]));
        }
        let pa = self.translate(va, Access::Write, prv, size as u64)?;
        self.prime_data_page(va, pa, true);
        if self.ram.store(pa, size, val).is_some() {
            return Ok(StoreResult::Ram(pa));
        }
        if self.ram.contains(pa, 1) {
            return Err(Exception::StoreAccessFault(va));
        }
        if pa & (size as u64 - 1) != 0 {
            return Err(Exception::StoreAddrMisaligned(va));
        }
        Ok(StoreResult::Mmio(pa))
    }

    // ---- PMP ----

    /// Recompute the cached "any PMP entry is active" flag; called on every
    /// pmpcfg write. With no active entries PMP checks are skipped entirely
    /// (accesses from any mode succeed), which keeps unconfigured embedders
    /// working; once an entry is active the spec rules apply, including
    /// no-match denial for S/U.
    pub(crate) fn update_pmp_active(&mut self) {
        self.pmp_active = (0..64).any(|i| (self.pmp_cfg_byte(i) >> 3) & 3 != 0);
        // A page primed while PMP was off may now be partly denied.
        self.invalidate_fetch_page();
        self.invalidate_data_pages();
    }

    fn pmp_cfg_byte(&self, i: usize) -> u64 {
        // RV64: even pmpcfg registers hold 8 entries each.
        (self.csrs.pmpcfg[(i / 8) * 2] >> (8 * (i % 8))) & 0xff
    }

    /// pmpaddr[i] is read-only when its own entry is locked, or when the
    /// next entry is a locked TOR (its base depends on pmpaddr[i]).
    pub(crate) fn pmpaddr_locked(&self, i: usize) -> bool {
        if self.pmp_cfg_byte(i) & 0x80 != 0 {
            return true;
        }
        if i + 1 < 64 {
            let next = self.pmp_cfg_byte(i + 1);
            if next & 0x80 != 0 && (next >> 3) & 3 == 1 {
                return true;
            }
        }
        false
    }

    /// True when `pa..pa+size` is permitted. Entries match in priority
    /// order; a partial overlap with the matching entry always fails.
    fn pmp_ok(&self, pa: u64, size: u64, access: Access, prv: PrivMode) -> bool {
        let end = pa + (size - 1);
        for i in 0..64 {
            let cfg = self.pmp_cfg_byte(i);
            let a = (cfg >> 3) & 3;
            if a == 0 {
                continue;
            }
            let (lo, hi) = self.pmp_range(i, a);
            if end < lo || pa >= hi {
                continue;
            }
            if pa < lo || end >= hi {
                return false;
            }
            if prv == PrivMode::M && cfg & 0x80 == 0 {
                // M-mode ignores unlocked entries' permissions.
                return true;
            }
            return match access {
                Access::Read => cfg & 1 != 0,
                Access::Write => cfg & 2 != 0,
                Access::Fetch => cfg & 4 != 0,
            };
        }
        // No entry matched: M succeeds, S/U fail (some entry is active).
        prv == PrivMode::M
    }

    /// [lo, hi) byte range of active entry `i` with address-matching mode
    /// `a` (1 = TOR, 2 = NA4, 3 = NAPOT).
    fn pmp_range(&self, i: usize, a: u64) -> (u64, u64) {
        let addr = self.csrs.pmpaddr[i];
        match a {
            1 => {
                let lo = if i == 0 {
                    0
                } else {
                    self.csrs.pmpaddr[i - 1] << 2
                };
                (lo, addr << 2)
            }
            2 => (addr << 2, (addr << 2) + 4),
            _ => {
                // NAPOT: trailing ones encode the region size.
                let t = (!addr).trailing_zeros();
                let base = (addr >> (t + 1) << (t + 1)) << 2;
                (base, base + (1u64 << (t + 3)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::GuestRam;
    use crate::{Clock, Cpu};

    const SUM: u64 = MSTATUS_SUM;
    const MXR: u64 = MSTATUS_MXR;

    fn cpu() -> Cpu {
        // RAM at physical 0 keeps the page-table arithmetic readable.
        let ram = GuestRam::new_owned(0, 1 << 20);
        Cpu::new(0, ram, Clock::Deterministic { shift: 0 })
    }

    fn pte(ppn: u64, flags: u64) -> u64 {
        (ppn << 10) | flags
    }

    const RWXAD: u64 = PTE_V | PTE_R | PTE_W | PTE_X | PTE_A | PTE_D;

    /// Root at 0x1000, next level at 0x2000, leaf table at 0x3000; VA 0x8000
    /// maps to PA 0x4000 with `flags`.
    fn map_sv39(cpu: &mut Cpu, flags: u64) {
        cpu.ram.store(0x1000, 8, pte(0x2, PTE_V)).unwrap();
        cpu.ram.store(0x2000, 8, pte(0x3, PTE_V)).unwrap();
        cpu.ram.store(0x3000 + 8 * 8, 8, pte(0x4, flags)).unwrap();
        cpu.write_csr(0x180, (8 << 60) | 1).unwrap();
        cpu.priv_mode = crate::PrivMode::S;
    }

    #[test]
    fn sv39_walk_and_perms() {
        let mut cpu = cpu();
        map_sv39(&mut cpu, RWXAD);
        cpu.ram.store(0x4010, 8, 0x1234).unwrap();
        assert_eq!(
            cpu.translate(0x8010, Access::Read, PrivMode::S, 8),
            Ok(0x4010)
        );
        assert_eq!(
            cpu.translate(0x8010, Access::Write, PrivMode::S, 8),
            Ok(0x4010)
        );
        assert_eq!(
            cpu.translate(0x8010, Access::Fetch, PrivMode::S, 2),
            Ok(0x4010)
        );
        // Unmapped VA and non-canonical VA page-fault.
        assert_eq!(
            cpu.translate(0x10000, Access::Read, PrivMode::S, 8),
            Err(Exception::LoadPageFault(0x10000))
        );
        assert_eq!(
            cpu.translate(1 << 40, Access::Write, PrivMode::S, 8),
            Err(Exception::StorePageFault(1 << 40))
        );
    }

    #[test]
    fn svade_a_and_d_faults() {
        let mut cpu = cpu();
        map_sv39(&mut cpu, PTE_V | PTE_R | PTE_W | PTE_D);
        // A clear: every access faults; hardware never sets A.
        assert_eq!(
            cpu.translate(0x8000, Access::Read, PrivMode::S, 8),
            Err(Exception::LoadPageFault(0x8000))
        );
        assert_eq!(cpu.ram.load(0x3000 + 8 * 8, 8).unwrap() & PTE_A, 0);
        // A set, D clear: loads work, stores fault, D stays clear.
        map_sv39(&mut cpu, PTE_V | PTE_R | PTE_W | PTE_A);
        cpu.flush_tlb();
        assert!(cpu.translate(0x8000, Access::Read, PrivMode::S, 8).is_ok());
        assert_eq!(
            cpu.translate(0x8000, Access::Write, PrivMode::S, 8),
            Err(Exception::StorePageFault(0x8000))
        );
        assert_eq!(cpu.ram.load(0x3000 + 8 * 8, 8).unwrap() & PTE_D, 0);
    }

    #[test]
    fn user_pages_sum_and_mxr() {
        let mut cpu = cpu();
        map_sv39(&mut cpu, RWXAD | PTE_U);
        // U-mode: full access to its own page.
        assert!(cpu.translate(0x8000, Access::Read, PrivMode::U, 8).is_ok());
        assert!(cpu.translate(0x8000, Access::Fetch, PrivMode::U, 2).is_ok());
        // S-mode: data denied without SUM, fetch denied always.
        assert!(cpu.translate(0x8000, Access::Read, PrivMode::S, 8).is_err());
        cpu.csrs.mstatus |= SUM;
        assert!(cpu.translate(0x8000, Access::Read, PrivMode::S, 8).is_ok());
        assert!(cpu.translate(0x8000, Access::Write, PrivMode::S, 8).is_ok());
        assert!(
            cpu.translate(0x8000, Access::Fetch, PrivMode::S, 2)
                .is_err()
        );

        // Non-U page: U-mode denied.
        map_sv39(&mut cpu, RWXAD);
        cpu.flush_tlb();
        assert!(cpu.translate(0x8000, Access::Read, PrivMode::U, 8).is_err());

        // X-only page: fetch ok, read only with MXR.
        map_sv39(&mut cpu, PTE_V | PTE_X | PTE_A);
        cpu.flush_tlb();
        assert!(cpu.translate(0x8000, Access::Fetch, PrivMode::S, 2).is_ok());
        assert!(cpu.translate(0x8000, Access::Read, PrivMode::S, 8).is_err());
        cpu.csrs.mstatus |= MXR;
        assert!(cpu.translate(0x8000, Access::Read, PrivMode::S, 8).is_ok());
    }

    #[test]
    fn superpage_alignment() {
        let mut cpu = cpu();
        // Root[1] is a misaligned 1 GiB leaf (ppn low bits set).
        cpu.ram.store(0x1008, 8, pte(0x3, RWXAD)).unwrap();
        // Root[0] -> level-1 table at 0x2000 with an aligned 2 MiB leaf.
        cpu.ram.store(0x1000, 8, pte(0x2, PTE_V)).unwrap();
        cpu.ram.store(0x2000, 8, pte(0, RWXAD)).unwrap();
        cpu.write_csr(0x180, (8 << 60) | 1).unwrap();
        cpu.priv_mode = crate::PrivMode::S;
        assert_eq!(
            cpu.translate(1 << 30, Access::Read, PrivMode::S, 8),
            Err(Exception::LoadPageFault(1 << 30))
        );
        // The 2 MiB megapage at VA 0 maps PA 0 with the VA's low VPN mixed in.
        assert_eq!(
            cpu.translate(0x5678, Access::Read, PrivMode::S, 8),
            Ok(0x5678)
        );
        assert_eq!(
            cpu.translate(0x1f_f123, Access::Read, PrivMode::S, 1),
            Ok(0x1f_f123)
        );
    }

    #[test]
    fn nonleaf_rwx_and_reserved_bits() {
        let mut cpu = cpu();
        // W-without-R leaf.
        cpu.ram.store(0x1000, 8, pte(0x2, PTE_V)).unwrap();
        cpu.ram.store(0x2000, 8, pte(0x3, PTE_V)).unwrap();
        cpu.ram
            .store(0x3000, 8, pte(0x4, PTE_V | PTE_W | PTE_A | PTE_D))
            .unwrap();
        cpu.write_csr(0x180, (8 << 60) | 1).unwrap();
        cpu.priv_mode = crate::PrivMode::S;
        assert!(cpu.translate(0, Access::Read, PrivMode::S, 8).is_err());
        // Pointer PTE at the last level.
        cpu.ram.store(0x3000, 8, pte(0x4, PTE_V)).unwrap();
        assert!(cpu.translate(0, Access::Read, PrivMode::S, 8).is_err());
        // Reserved high bits set on an otherwise valid leaf.
        cpu.ram
            .store(0x3000, 8, pte(0x4, RWXAD) | (1 << 60))
            .unwrap();
        assert!(cpu.translate(0, Access::Read, PrivMode::S, 8).is_err());
    }

    #[test]
    fn sv48_walk() {
        let mut cpu = cpu();
        // Four levels: 0x1000 -> 0x2000 -> 0x3000 -> 0x5000 -> PA 0x4000.
        cpu.ram.store(0x1000, 8, pte(0x2, PTE_V)).unwrap();
        cpu.ram.store(0x2000, 8, pte(0x3, PTE_V)).unwrap();
        cpu.ram.store(0x3000, 8, pte(0x5, PTE_V)).unwrap();
        cpu.ram.store(0x5000 + 8 * 8, 8, pte(0x4, RWXAD)).unwrap();
        cpu.write_csr(0x180, (9 << 60) | 1).unwrap();
        cpu.priv_mode = crate::PrivMode::S;
        assert_eq!(
            cpu.translate(0x8123, Access::Read, PrivMode::S, 1),
            Ok(0x4123)
        );
        // A VA canonical for Sv48 but not Sv39 walks fine (and misses).
        assert_eq!(
            cpu.translate(1 << 40, Access::Read, PrivMode::S, 8),
            Err(Exception::LoadPageFault(1 << 40))
        );
        // Non-canonical for Sv48.
        assert_eq!(
            cpu.translate(1 << 50, Access::Read, PrivMode::S, 8),
            Err(Exception::LoadPageFault(1 << 50))
        );
    }

    #[test]
    fn satp_warl_and_tlb_invalidation() {
        let mut cpu = cpu();
        map_sv39(&mut cpu, RWXAD);
        let satp = cpu.read_csr(0x180).unwrap();
        assert_eq!(satp, (8 << 60) | 1);
        // Unsupported modes leave satp unchanged; ASID is hard-wired to 0.
        cpu.write_csr(0x180, 10 << 60).unwrap();
        assert_eq!(cpu.read_csr(0x180).unwrap(), satp);
        cpu.write_csr(0x180, (8 << 60) | (0xbeef << 44) | 1)
            .unwrap();
        assert_eq!(cpu.read_csr(0x180).unwrap(), satp);

        // Prime the TLB, change the PTE: the stale entry still serves.
        assert_eq!(
            cpu.translate(0x8000, Access::Read, PrivMode::S, 8),
            Ok(0x4000)
        );
        cpu.ram.store(0x3000 + 8 * 8, 8, pte(0x5, RWXAD)).unwrap();
        assert_eq!(
            cpu.translate(0x8000, Access::Read, PrivMode::S, 8),
            Ok(0x4000)
        );
        // sfence.vma (flush_tlb) makes the new mapping visible.
        cpu.flush_tlb();
        assert_eq!(
            cpu.translate(0x8000, Access::Read, PrivMode::S, 8),
            Ok(0x5000)
        );
        // A satp rewrite also flushes.
        cpu.ram.store(0x3000 + 8 * 8, 8, pte(0x6, RWXAD)).unwrap();
        cpu.write_csr(0x180, (8 << 60) | 1).unwrap();
        assert_eq!(
            cpu.translate(0x8000, Access::Read, PrivMode::S, 8),
            Ok(0x6000)
        );
    }

    /// A primed data page is only reused under the exact permission context
    /// it was primed in, is never shared between loads and stores, and does
    /// not survive a TLB flush.
    #[test]
    fn data_fast_path_context_and_flush() {
        let mut cpu = cpu();
        map_sv39(&mut cpu, RWXAD | PTE_U);
        cpu.csrs.mstatus |= SUM;
        cpu.ram.store(0x4000, 8, 0x1111).unwrap();
        assert!(matches!(
            cpu.load_vaddr(0x8000, 8),
            Ok(LoadResult::Ram(0x1111))
        ));
        assert_eq!(cpu.fast_load_off(0x8000, 8), Some(0x4000));
        // A load-primed page never answers a store.
        assert_eq!(cpu.fast_store_off(0x8000, 8), None);
        // Clearing SUM would deny the access, so the entry must miss.
        cpu.csrs.mstatus &= !SUM;
        assert_eq!(cpu.fast_load_off(0x8000, 8), None);
        cpu.csrs.mstatus |= SUM;
        assert_eq!(cpu.fast_load_off(0x8000, 8), Some(0x4000));
        // An access that would leave the page falls back.
        assert_eq!(cpu.fast_load_off(0x8ffc, 8), None);
        cpu.flush_tlb();
        assert_eq!(cpu.fast_load_off(0x8000, 8), None);
    }

    /// The fetch fast path is tagged by privilege mode, so dropping to U
    /// without any flush cannot execute a page primed for S.
    #[test]
    fn fetch_fast_path_is_privilege_tagged() {
        let mut cpu = cpu();
        cpu.write_csr(0x305, 0x1000).unwrap(); // mtvec
        map_sv39(&mut cpu, RWXAD);
        cpu.ram.store(0x4000, 4, 0x0000_0013).unwrap(); // nop
        cpu.ram.store(0x4004, 4, 0x0000_006f).unwrap(); // jal x0, 0
        cpu.pc = 0x8000;
        cpu.run(3);
        assert_eq!(cpu.pc, 0x8004);

        cpu.priv_mode = PrivMode::U;
        cpu.pc = 0x8000;
        cpu.run(1);
        assert_eq!(cpu.pc, 0x1000);
        assert_eq!(cpu.csrs.mcause, 12); // instruction page fault
        assert_eq!(cpu.csrs.mtval, 0x8000);
    }

    #[test]
    fn mprv_uses_mpp_for_data() {
        let mut cpu = cpu();
        map_sv39(&mut cpu, RWXAD);
        cpu.priv_mode = crate::PrivMode::M;
        // Plain M-mode: bare.
        assert!(matches!(cpu.load_vaddr(0x8000, 8), Ok(LoadResult::Ram(_))));
        cpu.ram.store(0x4000, 8, 77).unwrap();
        // MPRV with MPP=S: loads translate.
        cpu.csrs.mstatus |= crate::csr::MSTATUS_MPRV | (1 << 11);
        match cpu.load_vaddr(0x8000, 8) {
            Ok(LoadResult::Ram(v)) => assert_eq!(v, 77),
            _ => panic!("expected translated RAM load"),
        }
    }

    #[test]
    fn misaligned_page_crossing_load_store() {
        let mut cpu = cpu();
        // Map VA 0x8000 -> PA 0x4000 and VA 0x9000 -> PA 0x6000 (not
        // physically contiguous).
        map_sv39(&mut cpu, RWXAD);
        cpu.ram.store(0x3000 + 9 * 8, 8, pte(0x6, RWXAD)).unwrap();
        cpu.ram.store(0x4ffc, 8, 0).unwrap();
        cpu.ram.store(0x4fff, 1, 0xaa).unwrap();
        cpu.ram.store(0x6000, 1, 0xbb).unwrap();
        match cpu.load_vaddr(0x8fff, 2) {
            Ok(LoadResult::Ram(v)) => assert_eq!(v, 0xbbaa),
            _ => panic!("expected split load"),
        }
        match cpu.store_vaddr(0x8fff, 2, 0x1122) {
            Ok(StoreResult::Ram(pa)) => assert_eq!(pa, 0x4fff),
            _ => panic!("expected split store"),
        }
        assert_eq!(cpu.ram.load(0x4fff, 1), Some(0x22));
        assert_eq!(cpu.ram.load(0x6000, 1), Some(0x11));
        // Second page unmapped: store faults before writing anything.
        cpu.ram.store(0x3000 + 9 * 8, 8, 0).unwrap();
        cpu.flush_tlb();
        assert!(matches!(
            cpu.store_vaddr(0x8fff, 2, 0x3344),
            Err(Exception::StorePageFault(0x9000))
        ));
        assert_eq!(cpu.ram.load(0x4fff, 1), Some(0x22));
    }

    #[test]
    fn pmp_napot_tor_na4() {
        let mut cpu = cpu();
        // Entry 0: NAPOT 8 bytes at 0x100, R only. addr = (0x100 >> 2) | 0.
        cpu.write_csr(0x3b0, 0x40).unwrap();
        cpu.write_csr(0x3a0, 0x19).unwrap(); // NAPOT | R
        assert!(cpu.pmp_ok(0x100, 8, Access::Read, PrivMode::S));
        assert!(!cpu.pmp_ok(0x100, 8, Access::Write, PrivMode::S));
        // Partial overlap fails even for M.
        assert!(!cpu.pmp_ok(0xfc, 8, Access::Read, PrivMode::M));
        // Unmatched: M ok, S denied.
        assert!(cpu.pmp_ok(0x200, 4, Access::Read, PrivMode::M));
        assert!(!cpu.pmp_ok(0x200, 4, Access::Read, PrivMode::S));
        // M-mode matching an unlocked entry ignores its permissions.
        assert!(cpu.pmp_ok(0x100, 8, Access::Write, PrivMode::M));

        // Entry 1: TOR [0x100+8, 0x1000) RW. Lower bound comes from
        // pmpaddr0.
        cpu.write_csr(0x3b1, 0x1000 >> 2).unwrap();
        let cfg = cpu.read_csr(0x3a0).unwrap();
        cpu.write_csr(0x3a0, cfg | (0x0b << 8)).unwrap(); // TOR | R | W
        assert!(cpu.pmp_ok(0x800, 8, Access::Write, PrivMode::S));
        assert!(!cpu.pmp_ok(0x800, 8, Access::Fetch, PrivMode::S));

        // Entry 2: NA4 at 0x2000, X only.
        cpu.write_csr(0x3b2, 0x2000 >> 2).unwrap();
        let cfg = cpu.read_csr(0x3a0).unwrap();
        cpu.write_csr(0x3a0, cfg | (0x14 << 16)).unwrap(); // NA4 | X
        assert!(cpu.pmp_ok(0x2000, 4, Access::Fetch, PrivMode::S));
        assert!(!cpu.pmp_ok(0x2000, 4, Access::Read, PrivMode::S));
        assert!(!cpu.pmp_ok(0x2004, 4, Access::Fetch, PrivMode::S));
    }

    #[test]
    fn pmp_locking_and_warl() {
        let mut cpu = cpu();
        // Locked NAPOT entry 0 without W: even M-mode stores fail.
        cpu.write_csr(0x3b0, 0x40).unwrap();
        cpu.write_csr(0x3a0, 0x99).unwrap(); // L | NAPOT | R
        assert!(cpu.pmp_ok(0x100, 8, Access::Read, PrivMode::M));
        assert!(!cpu.pmp_ok(0x100, 8, Access::Write, PrivMode::M));
        // Locked cfg byte and its pmpaddr are read-only now.
        cpu.write_csr(0x3a0, 0).unwrap();
        assert_eq!(cpu.read_csr(0x3a0).unwrap(), 0x99);
        cpu.write_csr(0x3b0, 0).unwrap();
        assert_eq!(cpu.read_csr(0x3b0).unwrap(), 0x40);
        // A locked TOR entry also locks the previous pmpaddr.
        cpu.write_csr(0x3b2, 0x123).unwrap();
        cpu.write_csr(0x3a0, 0x89 << 24).unwrap(); // entry 3: L | TOR | R
        cpu.write_csr(0x3b2, 0).unwrap();
        assert_eq!(cpu.read_csr(0x3b2).unwrap(), 0x123);
        // pmpaddr is 54 bits WARL.
        cpu.write_csr(0x3b4, u64::MAX).unwrap();
        assert_eq!(cpu.read_csr(0x3b4).unwrap(), (1u64 << 54) - 1);
        // Odd pmpcfg registers do not exist on RV64.
        assert!(cpu.read_csr(0x3a1).is_err());
        assert!(cpu.write_csr(0x3a1, 0).is_err());
    }
}
