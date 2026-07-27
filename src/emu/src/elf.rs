// Copyright 2026 The libkrun Authors. Licensed under Apache-2.0.
//
// Minimal ELF64 little-endian loader, just enough for riscv-tests and
// kernel-style static images. No relocation, no dynamic linking.

use crate::mem::GuestRam;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfError {
    Truncated,
    BadMagic,
    Unsupported,
    OutOfRam,
}

const PT_LOAD: u32 = 1;
const SHT_SYMTAB: u32 = 2;

fn r16(b: &[u8], off: usize) -> Option<u64> {
    Some(u16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?) as u64)
}

fn r32(b: &[u8], off: usize) -> Option<u64> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?) as u64)
}

fn r64(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(off..off + 8)?.try_into().ok()?))
}

fn check_header(image: &[u8]) -> Result<(), ElfError> {
    if image.len() < 64 {
        return Err(ElfError::Truncated);
    }
    if &image[0..4] != b"\x7fELF" {
        return Err(ElfError::BadMagic);
    }
    // ELFCLASS64, little-endian.
    if image[4] != 2 || image[5] != 1 {
        return Err(ElfError::Unsupported);
    }
    Ok(())
}

/// Copy all PT_LOAD segments into RAM by physical address; returns the entry
/// point.
pub fn load(image: &[u8], ram: &GuestRam) -> Result<u64, ElfError> {
    check_header(image)?;
    let entry = r64(image, 24).ok_or(ElfError::Truncated)?;
    let phoff = r64(image, 32).ok_or(ElfError::Truncated)? as usize;
    let phentsize = r16(image, 54).ok_or(ElfError::Truncated)? as usize;
    let phnum = r16(image, 56).ok_or(ElfError::Truncated)? as usize;

    for i in 0..phnum {
        let ph = phoff + i * phentsize;
        let p_type = r32(image, ph).ok_or(ElfError::Truncated)? as u32;
        if p_type != PT_LOAD {
            continue;
        }
        let p_offset = r64(image, ph + 8).ok_or(ElfError::Truncated)? as usize;
        let p_paddr = r64(image, ph + 24).ok_or(ElfError::Truncated)?;
        let p_filesz = r64(image, ph + 32).ok_or(ElfError::Truncated)? as usize;
        let p_memsz = r64(image, ph + 40).ok_or(ElfError::Truncated)?;
        let data = image
            .get(p_offset..p_offset + p_filesz)
            .ok_or(ElfError::Truncated)?;
        ram.write_slice(p_paddr, data).ok_or(ElfError::OutOfRam)?;
        if p_memsz > p_filesz as u64 {
            ram.fill_zero(p_paddr + p_filesz as u64, p_memsz - p_filesz as u64)
                .ok_or(ElfError::OutOfRam)?;
        }
    }
    Ok(entry)
}

/// Look up a symbol (e.g. "tohost") in the symtab; returns its value.
pub fn find_symbol(image: &[u8], name: &str) -> Option<u64> {
    check_header(image).ok()?;
    let shoff = r64(image, 40)? as usize;
    let shentsize = r16(image, 58)? as usize;
    let shnum = r16(image, 60)? as usize;

    for i in 0..shnum {
        let sh = shoff + i * shentsize;
        if r32(image, sh + 4)? as u32 != SHT_SYMTAB {
            continue;
        }
        let sym_off = r64(image, sh + 24)? as usize;
        let sym_size = r64(image, sh + 32)? as usize;
        let entsize = r64(image, sh + 56)? as usize;
        let strtab_idx = r32(image, sh + 40)? as usize;
        let str_sh = shoff + strtab_idx * shentsize;
        let str_off = r64(image, str_sh + 24)? as usize;
        let str_size = r64(image, str_sh + 32)? as usize;
        let strtab = image.get(str_off..str_off + str_size)?;

        if entsize == 0 {
            continue;
        }
        for s in (0..sym_size).step_by(entsize) {
            let st_name = r32(image, sym_off + s)? as usize;
            let st_value = r64(image, sym_off + s + 8)?;
            let bytes = strtab.get(st_name..)?;
            let end = bytes.iter().position(|&c| c == 0)?;
            if &bytes[..end] == name.as_bytes() {
                return Some(st_value);
            }
        }
    }
    None
}
