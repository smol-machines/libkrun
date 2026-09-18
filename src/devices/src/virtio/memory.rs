//! Virtio-mem protocol state for live RAM growth.
//!
//! Layout follows Linux include/uapi/linux/virtio_mem.h. The VMM must register
//! backing RAM before increasing the usable/requested size. This module does
//! not allocate RAM or acknowledge durability; it tracks guest plug requests.

use std::collections::BTreeSet;
use std::ops::Range;

mod device;
pub use device::{MemoryDevice, MemoryDeviceState};

const ACK: u16 = 0;
const NACK: u16 = 1;
const ERROR: u16 = 3;
const PLUGGED: u16 = 0;
const UNPLUGGED: u16 = 1;
const MIXED: u16 = 2;

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryState {
    addr: u64,
    region_size: u64,
    block_size: u64,
    usable_size: u64,
    requested_size: u64,
    plugged: BTreeSet<u64>,
}

impl MemoryState {
    /// A restored device may only advertise usable memory that exists in the
    /// restored address space, including currently unplugged blocks.
    pub fn validate_backing(&self, memory: &vm_memory::GuestMemoryMmap) -> Result<(), String> {
        use vm_memory::{GuestAddress, GuestMemory};
        self.validate()?;
        let len =
            usize::try_from(self.usable_size).map_err(|_| "RAM backing exceeds host range")?;
        if len != 0 && !memory.check_range(GuestAddress(self.addr), len) {
            return Err("checkpoint is missing backing for the RAM device's usable range".into());
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<(), String> {
        let mut geometry = Self::new(self.addr, self.region_size, self.block_size)?;
        geometry.request_growth(self.requested_size, self.usable_size)?;
        if self.plugged.len() as u64 > self.requested_size / self.block_size
            || self
                .plugged
                .last()
                .is_some_and(|block| *block >= self.usable_size / self.block_size)
        {
            return Err("checkpoint has invalid plugged RAM blocks".into());
        }
        Ok(())
    }

    /// Reserve only a guest physical address aperture, not backing host RAM.
    pub fn new(addr: u64, region_size: u64, block_size: u64) -> Result<Self, String> {
        if !block_size.is_power_of_two()
            || block_size < 4096
            || !addr.is_multiple_of(block_size)
            || region_size == 0
            || !region_size.is_multiple_of(block_size)
            || addr.checked_add(region_size).is_none()
        {
            return Err("invalid virtio-mem address aperture or block size".into());
        }
        Ok(Self {
            addr,
            region_size,
            block_size,
            usable_size: 0,
            requested_size: 0,
            plugged: BTreeSet::new(),
        })
    }

    /// Call only after the full usable range has live, owned hypervisor RAM.
    /// Both values grow monotonically; reset/unplug never frees the backing.
    pub fn request_growth(&mut self, requested: u64, usable: u64) -> Result<(), String> {
        if requested < self.requested_size
            || usable < self.usable_size
            || requested > usable
            || usable > self.region_size
            || !requested.is_multiple_of(self.block_size)
            || !usable.is_multiple_of(self.block_size)
        {
            return Err("invalid virtio-mem growth request".into());
        }
        self.requested_size = requested;
        self.usable_size = usable;
        Ok(())
    }

    pub fn plugged_size(&self) -> u64 {
        self.plugged.len() as u64 * self.block_size
    }

    pub fn config(&self) -> [u8; 56] {
        let mut bytes = [0; 56];
        for (offset, value) in [
            (0, self.block_size),
            (16, self.addr),
            (24, self.region_size),
            (32, self.usable_size),
            (40, self.plugged_size()),
            (48, self.requested_size),
        ] {
            bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        }
        bytes
    }

    fn blocks(&self, addr: u64, count: u16) -> Option<Range<u64>> {
        let offset = addr.checked_sub(self.addr)?;
        let len = u64::from(count).checked_mul(self.block_size)?;
        if count == 0
            || !offset.is_multiple_of(self.block_size)
            || offset.checked_add(len)? > self.usable_size
        {
            return None;
        }
        let first = offset / self.block_size;
        Some(first..first + u64::from(count))
    }

    /// Process one fixed-size request; malformed input must not change state.
    pub fn request(&mut self, bytes: &[u8]) -> [u8; 10] {
        let mut response = [0; 10];
        let mut result = ERROR;
        let mut state = UNPLUGGED;
        if let Ok(bytes) = <&[u8; 24]>::try_from(bytes) {
            let kind = u16::from_le_bytes(bytes[0..2].try_into().unwrap());
            if kind == 2 {
                // UNPLUG_ALL is used by Linux during device initialization.
                // No backing is removed, and the requested growth survives.
                self.plugged.clear();
                result = ACK;
            } else {
                let addr = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
                let count = u16::from_le_bytes(bytes[16..18].try_into().unwrap());
                if let Some(blocks) = self.blocks(addr, count) {
                    let plugged = blocks.clone().filter(|b| self.plugged.contains(b)).count();
                    match kind {
                        0 if plugged == 0 => {
                            let extra = u64::from(count) * self.block_size;
                            if self
                                .plugged_size()
                                .checked_add(extra)
                                .is_none_or(|size| size > self.requested_size)
                            {
                                result = NACK;
                            } else {
                                self.plugged.extend(blocks);
                                result = ACK;
                            }
                        }
                        1 if plugged == usize::from(count) => {
                            for block in blocks {
                                self.plugged.remove(&block);
                            }
                            result = ACK;
                        }
                        3 => {
                            state = if plugged == 0 {
                                UNPLUGGED
                            } else if plugged == usize::from(count) {
                                PLUGGED
                            } else {
                                MIXED
                            };
                            result = ACK;
                        }
                        _ => {}
                    }
                }
            }
        }
        response[0..2].copy_from_slice(&result.to_le_bytes());
        response[8..10].copy_from_slice(&state.to_le_bytes());
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u64 = 0x1_0000_0000;
    const BLOCK: u64 = 2 << 20;

    fn req(kind: u16, addr: u64, count: u16) -> [u8; 24] {
        let mut bytes = [0; 24];
        bytes[0..2].copy_from_slice(&kind.to_le_bytes());
        bytes[8..16].copy_from_slice(&addr.to_le_bytes());
        bytes[16..18].copy_from_slice(&count.to_le_bytes());
        bytes
    }

    #[test]
    fn restored_memory_requires_backing_for_unplugged_usable_blocks_too() {
        use vm_memory::{GuestAddress, GuestMemoryMmap};
        let mut state = MemoryState::new(BASE, 0x10000, 0x1000).unwrap();
        state.request_growth(0x1000, 0x3000).unwrap();
        let missing = GuestMemoryMmap::<()>::from_ranges(&[
            (GuestAddress(BASE), 0x1000),
            (GuestAddress(BASE + 0x2000), 0x1000),
        ])
        .unwrap();
        assert!(state.validate_backing(&missing).is_err());
        let complete = GuestMemoryMmap::<()>::from_ranges(&[
            (GuestAddress(BASE), 0x1000),
            (GuestAddress(BASE + 0x1000), 0x2000),
        ])
        .unwrap();
        state.validate_backing(&complete).unwrap();
    }

    #[test]
    fn memory_growth_tracks_guest_plugs_without_preallocating_the_aperture() {
        let mut memory = MemoryState::new(BASE, 1 << 40, BLOCK).unwrap();
        assert!(memory.plugged.is_empty());
        assert_eq!(memory.request(&req(0, BASE, 1))[0], ERROR as u8);
        memory.request_growth(2 * BLOCK, 4 * BLOCK).unwrap();
        assert_eq!(memory.request(&req(0, BASE, 1))[0], ACK as u8);
        assert_eq!(memory.request(&req(3, BASE, 2))[8], MIXED as u8);
        assert_eq!(memory.request(&req(0, BASE + BLOCK, 2))[0], NACK as u8);
        assert_eq!(memory.plugged_size(), BLOCK);
        memory.request_growth(4 * BLOCK, 4 * BLOCK).unwrap();
        assert_eq!(memory.request(&req(0, BASE + BLOCK, 3))[0], ACK as u8);
        assert_eq!(memory.request(&req(3, BASE, 4))[8], PLUGGED as u8);
        assert_eq!(memory.request(&req(1, BASE, 1))[0], ACK as u8);
        assert_eq!(memory.plugged_size(), 3 * BLOCK);
        assert_eq!(memory.request(&req(2, 0, 0))[0], ACK as u8);
        assert_eq!(memory.plugged_size(), 0);
        assert_eq!(memory.requested_size, 4 * BLOCK);
    }

    #[test]
    fn invalid_memory_requests_do_not_mutate_plugged_or_requested_state() {
        let mut memory = MemoryState::new(BASE, 8 * BLOCK, BLOCK).unwrap();
        memory.request_growth(4 * BLOCK, 4 * BLOCK).unwrap();
        memory.request(&req(0, BASE, 1));
        let before = memory.clone();
        for request in [
            req(0, BASE, 1),
            req(0, BASE + 1, 1),
            req(0, BASE - BLOCK, 1),
            req(0, BASE + 4 * BLOCK, 1),
            req(1, BASE, 2),
            req(3, BASE, 0),
            req(99, BASE, 1),
            req(0, u64::MAX, u16::MAX),
        ] {
            assert_eq!(memory.request(&request)[0], ERROR as u8);
            assert_eq!(memory, before);
        }
        assert_eq!(memory.request(&[0; 23])[0], ERROR as u8);
        for (requested, usable) in [
            (BLOCK, 4 * BLOCK),
            (5 * BLOCK, 4 * BLOCK),
            (9 * BLOCK, 9 * BLOCK),
            (4 * BLOCK + 1, 8 * BLOCK),
        ] {
            assert!(memory.request_growth(requested, usable).is_err());
            assert_eq!(memory, before);
        }
    }
}
