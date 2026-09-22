//! Experimental Linux RAM growth with checkpointed boot layout and device state.

use std::sync::Arc;
use vm_memory::{FileOffset, GuestAddress, GuestRegionMmap};

use crate::{Vmm, VmmRunState, builder, vstate};

impl Vmm {
    /// Grow the hot-add area (MiB in addition to initial boot RAM).
    pub fn prototype_grow_memory(&mut self, additional_mib: u64) -> Result<(), String> {
        if self.run_state != VmmRunState::Running {
            return Err("RAM growth requires a running machine".into());
        }
        let device = self
            .prototype_memory
            .as_ref()
            .ok_or("RAM hot-add prototype is not enabled")?;
        let mut device = device.lock().map_err(|_| "RAM device lock poisoned")?;
        let (base, mapped, capacity) = device.geometry();
        let target = additional_mib
            .checked_mul(1 << 20)
            .ok_or("RAM size overflow")?;
        if target < mapped || target > capacity || !target.is_multiple_of(128 << 20) {
            return Err(
                "RAM hot-add area must grow in 128 MiB units within the platform aperture".into(),
            );
        }
        if target != mapped {
            let size =
                usize::try_from(target - mapped).map_err(|_| "RAM size exceeds host range")?;
            let (next_generation, region) = if let Some(generation) = &self.layered_ram {
                let (next, region) = generation
                    .with_zero_region(base + mapped, size)
                    .map_err(|error| format!("prepare added RAM generation: {error}"))?;
                (Some(next), region)
            } else {
                let backing = builder::create_guest_ram_memfd(size)?;
                let region = Arc::new(
                    GuestRegionMmap::from_range(
                        GuestAddress(base + mapped),
                        size,
                        Some(FileOffset::new(backing, 0)),
                    )
                    .map_err(|e| format!("map added RAM: {e}"))?,
                );
                (None, region)
            };
            let context =
                vstate::KvmContext::new().map_err(|e| format!("KVM capability check: {e}"))?;
            self.vm
                .append_guest_memory(&self.guest_memory, region, context.max_memslots())
                .map_err(|e| format!("register added RAM: {e}"))?;
            if let Some(generation) = next_generation {
                self.layered_ram = Some(generation);
            }
        }
        // Geometry commits before notification; a notification failure can be
        // retried without registering overlapping slots or forgetting backing.
        device.request_growth(target, target)
    }

    pub fn prototype_memory_status(&self) -> Result<(u64, u64), String> {
        let device = self
            .prototype_memory
            .as_ref()
            .ok_or("RAM hot-add prototype is not enabled")?
            .lock()
            .map_err(|_| "RAM device lock poisoned")?;
        Ok((device.geometry().1, device.plugged_size()))
    }

    /// Report original boot layout separately from live added RAM, so callers
    /// can reconcile an interrupted resize without trusting a stale VM record.
    pub fn prototype_memory_info(&self) -> Result<String, String> {
        let boot = self
            .memory_growth_topology
            .as_ref()
            .ok_or("RAM hot-add topology is unavailable")?
            .boot_memory_mib;
        let device = self
            .prototype_memory
            .as_ref()
            .ok_or("RAM hot-add prototype is not enabled")?
            .lock()
            .map_err(|_| "RAM device lock poisoned")?;
        let (base, mapped, capacity) = device.geometry();
        let plugged = device.plugged_size();
        Ok(format!(
            "OK boot_mib {boot} base {base} mapped {mapped} plugged {plugged} capacity {capacity}\n"
        ))
    }
}
