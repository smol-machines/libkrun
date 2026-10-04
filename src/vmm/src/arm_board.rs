// SPDX-License-Identifier: Apache-2.0

//! Hypervisor-neutral description of an aarch64 guest's virtual board.
//!
//! A running guest remembers the board it booted on: where the GIC
//! distributor and redistributors live, which interrupt each device raises and
//! at which address it sits. The device tree that described it is not part of
//! a checkpoint, so a checkpoint restored by another hypervisor backend must
//! rebuild exactly that board. macOS records its board here at capture time;
//! the KVM backend reads it to restore a macOS checkpoint on Linux.

/// A non-virtio MMIO device, in the order the source registered it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LegacyDevice {
    pub kind: LegacyKind,
    pub addr: u64,
    /// The interrupt ID (INTID) the guest was told this device raises.
    pub intid: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LegacyKind {
    Serial,
    Rtc,
    Gpio,
}

impl LegacyKind {
    fn code(self) -> u8 {
        match self {
            LegacyKind::Serial => 1,
            LegacyKind::Rtc => 2,
            LegacyKind::Gpio => 3,
        }
    }

    fn from_code(code: u8) -> Result<Self, String> {
        match code {
            1 => Ok(LegacyKind::Serial),
            2 => Ok(LegacyKind::Rtc),
            3 => Ok(LegacyKind::Gpio),
            other => Err(format!("unknown legacy device kind {other}")),
        }
    }
}

/// The guest-visible board of a macOS (HVF) aarch64 machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoardLayout {
    pub gic_dist_base: u64,
    pub gic_dist_size: u64,
    pub gic_redist_base: u64,
    /// Size of the whole redistributor region (all CPU slots).
    pub gic_redist_size: u64,
    /// Number of CPU slots the GIC and device tree were sized for, which can
    /// exceed the vCPUs that exist when the guest may hot-add CPUs.
    pub cpu_slots: u32,
    /// Legacy MMIO devices in registration order. Virtio devices follow the
    /// last one, one MMIO slot and one interrupt each.
    pub legacy: Vec<LegacyDevice>,
    /// The first virtio device's MMIO address and interrupt ID.
    pub first_virtio_addr: u64,
    pub first_virtio_intid: u32,
}

const BOARD_LAYOUT_VERSION: u8 = 1;

impl BoardLayout {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64 + self.legacy.len() * 13);
        out.push(BOARD_LAYOUT_VERSION);
        for v in [
            self.gic_dist_base,
            self.gic_dist_size,
            self.gic_redist_base,
            self.gic_redist_size,
        ] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&self.cpu_slots.to_le_bytes());
        out.extend_from_slice(&(self.legacy.len() as u32).to_le_bytes());
        for dev in &self.legacy {
            out.push(dev.kind.code());
            out.extend_from_slice(&dev.addr.to_le_bytes());
            out.extend_from_slice(&dev.intid.to_le_bytes());
        }
        out.extend_from_slice(&self.first_virtio_addr.to_le_bytes());
        out.extend_from_slice(&self.first_virtio_intid.to_le_bytes());
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let mut r = Reader { bytes, pos: 0 };
        let version = r.u8()?;
        if version != BOARD_LAYOUT_VERSION {
            return Err(format!("unsupported board layout version {version}"));
        }
        let gic_dist_base = r.u64()?;
        let gic_dist_size = r.u64()?;
        let gic_redist_base = r.u64()?;
        let gic_redist_size = r.u64()?;
        let cpu_slots = r.u32()?;
        let count = r.u32()? as usize;
        if count > 64 {
            return Err(format!("board layout lists {count} legacy devices"));
        }
        let mut legacy = Vec::with_capacity(count);
        for _ in 0..count {
            let kind = LegacyKind::from_code(r.u8()?)?;
            let addr = r.u64()?;
            let intid = r.u32()?;
            legacy.push(LegacyDevice { kind, addr, intid });
        }
        let first_virtio_addr = r.u64()?;
        let first_virtio_intid = r.u32()?;
        Ok(BoardLayout {
            gic_dist_base,
            gic_dist_size,
            gic_redist_base,
            gic_redist_size,
            cpu_slots,
            legacy,
            first_virtio_addr,
            first_virtio_intid,
        })
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], String> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| "board layout truncated".to_string())?;
        let s = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn board_layout_round_trips() {
        let layout = BoardLayout {
            gic_dist_base: 0x09dd_0000,
            gic_dist_size: 0x1_0000,
            gic_redist_base: 0x09de_0000,
            gic_redist_size: 0x20_0000,
            cpu_slots: 16,
            legacy: vec![
                LegacyDevice {
                    kind: LegacyKind::Rtc,
                    addr: 0x0a00_1000,
                    intid: 32,
                },
                LegacyDevice {
                    kind: LegacyKind::Gpio,
                    addr: 0x0a00_2000,
                    intid: 33,
                },
            ],
            first_virtio_addr: 0x0a00_3000,
            first_virtio_intid: 34,
        };
        assert_eq!(BoardLayout::decode(&layout.encode()).unwrap(), layout);
    }

    #[test]
    fn truncated_board_layout_is_rejected() {
        let mut bytes = BoardLayout {
            gic_dist_base: 1,
            gic_dist_size: 2,
            gic_redist_base: 3,
            gic_redist_size: 4,
            cpu_slots: 1,
            legacy: Vec::new(),
            first_virtio_addr: 5,
            first_virtio_intid: 6,
        }
        .encode();
        bytes.pop();
        assert!(BoardLayout::decode(&bytes).is_err());
    }
}
