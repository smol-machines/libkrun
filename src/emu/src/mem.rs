// Copyright 2026 The libkrun Authors. Licensed under Apache-2.0.

use std::alloc::{self, Layout};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64};

/// Backing allocation for the owned constructor. Never exposed as a `&[u8]`,
/// so mutation through raw pointers stays within the aliasing rules.
struct OwnedMem {
    ptr: *mut u8,
    layout: Layout,
}

// SAFETY: OwnedMem is a plain heap allocation; ownership transfer and shared
// access across threads are governed by GuestRam's rules below.
unsafe impl Send for OwnedMem {}
unsafe impl Sync for OwnedMem {}

impl Drop for OwnedMem {
    fn drop(&mut self) {
        unsafe { alloc::dealloc(self.ptr, self.layout) };
    }
}

/// A single contiguous guest DRAM region mapped at `gpa_base`.
///
/// Cloning yields another handle to the same memory (harts share RAM).
#[derive(Clone)]
pub struct GuestRam {
    base: *mut u8,
    gpa_base: u64,
    len: u64,
    _owned: Option<Arc<OwnedMem>>,
}

// SAFETY: GuestRam hands out raw byte access to a region that is either owned
// (OwnedMem, freed with the last clone) or guaranteed valid for the program's
// lifetime by the from_raw_parts caller. Concurrent plain loads/stores from
// several harts can race, exactly as real DRAM does; the guest is responsible
// for its own synchronization, and torn values never break host memory safety
// because all accesses are byte-copies or aligned atomics within bounds.
unsafe impl Send for GuestRam {}
unsafe impl Sync for GuestRam {}

impl GuestRam {
    /// Allocate a zero-filled, owned region (tests and simple embedders).
    pub fn new_owned(gpa_base: u64, len: usize) -> Self {
        let layout = Layout::from_size_align(len, 16).expect("bad ram size");
        let ptr = unsafe { alloc::alloc_zeroed(layout) };
        assert!(!ptr.is_null(), "guest ram allocation failed");
        GuestRam {
            base: ptr,
            gpa_base,
            len: len as u64,
            _owned: Some(Arc::new(OwnedMem { ptr, layout })),
        }
    }

    /// Wrap an externally managed mapping.
    ///
    /// # Safety
    /// `base..base+len` must be valid for reads and writes for the lifetime
    /// of every clone of the returned region, and must not be accessed as a
    /// Rust reference (`&`/`&mut [u8]`) while the region is live.
    pub unsafe fn from_raw_parts(base: *mut u8, gpa_base: u64, len: u64) -> Self {
        GuestRam {
            base,
            gpa_base,
            len,
            _owned: None,
        }
    }

    pub fn gpa_base(&self) -> u64 {
        self.gpa_base
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Byte offset into the region if `gpa..gpa+size` lies entirely inside.
    #[inline]
    fn offset(&self, gpa: u64, size: u64) -> Option<usize> {
        let off = gpa.checked_sub(self.gpa_base)?;
        if off.checked_add(size)? <= self.len {
            Some(off as usize)
        } else {
            None
        }
    }

    #[inline]
    pub fn contains(&self, gpa: u64, size: u64) -> bool {
        self.offset(gpa, size).is_some()
    }

    /// Byte offset of the 4 KiB page holding `gpa`, if that whole page lies
    /// inside the region. Callers cache it and then read anywhere inside the
    /// page without re-checking bounds.
    #[inline]
    pub(crate) fn page_offset(&self, gpa: u64) -> Option<usize> {
        self.offset(gpa & !0xfff, 0x1000)
    }

    /// Unaligned little-endian read of 4 bytes at a byte offset the caller
    /// already bounds-checked (see [`GuestRam::page_offset`]).
    ///
    /// # Safety
    /// `off + 4` must lie inside the region.
    #[inline]
    pub(crate) unsafe fn read_u32_unchecked(&self, off: usize) -> u32 {
        unsafe { u32::from_le(self.base.add(off).cast::<u32>().read_unaligned()) }
    }

    /// Little-endian load of 1/2/4/8 bytes at a bounds-checked offset.
    /// Specialized per width, unlike [`GuestRam::load`], whose byte-composed
    /// form compiles to a variable-length copy.
    ///
    /// # Safety
    /// `off + size` must lie inside the region.
    #[inline]
    pub(crate) unsafe fn load_at(&self, off: usize, size: usize) -> u64 {
        unsafe {
            let p = self.base.add(off);
            match size {
                1 => u64::from(p.read()),
                2 => u64::from(u16::from_le(p.cast::<u16>().read_unaligned())),
                4 => u64::from(u32::from_le(p.cast::<u32>().read_unaligned())),
                _ => u64::from_le(p.cast::<u64>().read_unaligned()),
            }
        }
    }

    /// Little-endian store of 1/2/4/8 bytes at a bounds-checked offset.
    ///
    /// # Safety
    /// `off + size` must lie inside the region.
    #[inline]
    pub(crate) unsafe fn store_at(&self, off: usize, size: usize, val: u64) {
        unsafe {
            let p = self.base.add(off);
            match size {
                1 => p.write(val as u8),
                2 => p.cast::<u16>().write_unaligned((val as u16).to_le()),
                4 => p.cast::<u32>().write_unaligned((val as u32).to_le()),
                _ => p.cast::<u64>().write_unaligned(val.to_le()),
            }
        }
    }

    /// Little-endian load of 1/2/4/8 bytes, any alignment (byte-composed).
    #[inline]
    pub fn load(&self, gpa: u64, size: usize) -> Option<u64> {
        debug_assert!(matches!(size, 1 | 2 | 4 | 8));
        let off = self.offset(gpa, size as u64)?;
        let mut buf = [0u8; 8];
        unsafe { ptr::copy_nonoverlapping(self.base.add(off), buf.as_mut_ptr(), size) };
        Some(u64::from_le_bytes(buf))
    }

    /// Little-endian store of 1/2/4/8 bytes, any alignment.
    #[inline]
    pub fn store(&self, gpa: u64, size: usize, val: u64) -> Option<()> {
        debug_assert!(matches!(size, 1 | 2 | 4 | 8));
        let off = self.offset(gpa, size as u64)?;
        let buf = val.to_le_bytes();
        unsafe { ptr::copy_nonoverlapping(buf.as_ptr(), self.base.add(off), size) };
        Some(())
    }

    /// 16-byte little-endian load (future LR/SC.Q, FLD pairs).
    pub fn load_u128(&self, gpa: u64) -> Option<u128> {
        let off = self.offset(gpa, 16)?;
        let mut buf = [0u8; 16];
        unsafe { ptr::copy_nonoverlapping(self.base.add(off), buf.as_mut_ptr(), 16) };
        Some(u128::from_le_bytes(buf))
    }

    pub fn store_u128(&self, gpa: u64, val: u128) -> Option<()> {
        let off = self.offset(gpa, 16)?;
        let buf = val.to_le_bytes();
        unsafe { ptr::copy_nonoverlapping(buf.as_ptr(), self.base.add(off), 16) };
        Some(())
    }

    /// Aligned 4-byte atomic view for the A extension.
    pub fn atomic_u32(&self, gpa: u64) -> Option<&AtomicU32> {
        if gpa & 3 != 0 {
            return None;
        }
        let off = self.offset(gpa, 4)?;
        // SAFETY: in bounds and 4-byte aligned (region base is 16-aligned for
        // owned memory; from_raw_parts callers provide page-aligned mappings).
        Some(unsafe { &*(self.base.add(off) as *const AtomicU32) })
    }

    /// Aligned 8-byte atomic view for the A extension.
    pub fn atomic_u64(&self, gpa: u64) -> Option<&AtomicU64> {
        if gpa & 7 != 0 {
            return None;
        }
        let off = self.offset(gpa, 8)?;
        // SAFETY: as atomic_u32.
        Some(unsafe { &*(self.base.add(off) as *const AtomicU64) })
    }

    /// Bulk copy into RAM (ELF loading).
    pub fn write_slice(&self, gpa: u64, data: &[u8]) -> Option<()> {
        let off = self.offset(gpa, data.len() as u64)?;
        unsafe { ptr::copy_nonoverlapping(data.as_ptr(), self.base.add(off), data.len()) };
        Some(())
    }

    /// Bulk copy out of RAM.
    pub fn read_slice(&self, gpa: u64, out: &mut [u8]) -> Option<()> {
        let off = self.offset(gpa, out.len() as u64)?;
        unsafe { ptr::copy_nonoverlapping(self.base.add(off), out.as_mut_ptr(), out.len()) };
        Some(())
    }

    pub fn fill_zero(&self, gpa: u64, len: u64) -> Option<()> {
        let off = self.offset(gpa, len)?;
        unsafe { ptr::write_bytes(self.base.add(off), 0, len as usize) };
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_and_endianness() {
        let ram = GuestRam::new_owned(0x8000_0000, 4096);
        assert!(ram.load(0x7fff_ffff, 1).is_none());
        assert!(ram.load(0x8000_0ff9, 8).is_none());
        assert!(ram.store(0x8000_1000, 1, 0).is_none());

        ram.store(0x8000_0000, 8, 0x0102_0304_0506_0708).unwrap();
        assert_eq!(ram.load(0x8000_0000, 1), Some(0x08));
        assert_eq!(ram.load(0x8000_0007, 1), Some(0x01));
        // Misaligned byte-composed access.
        assert_eq!(ram.load(0x8000_0001, 4), Some(0x0405_0607));
        ram.store(0x8000_0003, 2, 0xbeef).unwrap();
        assert_eq!(ram.load(0x8000_0003, 2), Some(0xbeef));
    }

    #[test]
    fn atomics_require_alignment() {
        let ram = GuestRam::new_owned(0, 64);
        assert!(ram.atomic_u32(2).is_none());
        assert!(ram.atomic_u64(4).is_none());
        ram.atomic_u64(8)
            .unwrap()
            .store(7, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(ram.load(8, 8), Some(7));
    }
}
