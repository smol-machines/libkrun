// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// SPDX-License-Identifier: BSD-3-Clause

//! MMIO device manager for the Windows Hypervisor Platform backend.
//!
//! Mirrors `kvm/mmio.rs` (x86_64) but without KVM's `ioeventfd`/`irqfd`: WHP
//! traps every guest MMIO access into the vCPU run loop (so the notify-register
//! write reaches the bus directly, no ioeventfd), and device interrupts are
//! injected with `WhpVm::request_interrupt` rather than wired through an irqfd.
//!
//! The device→guest interrupt path is wired in `builder::attach_mmio_device`: a
//! per-device watcher thread waits on the device's `interrupt_evt` and raises the
//! IOAPIC line (`IrqChipDevice::set_irq`), which injects via `request_interrupt`.
//! Registration here records the IRQ line and places the device on the bus.

// Some manager methods (snapshot/restore/fork) are present for parity but not yet
// exercised on Windows; allow until the snapshot/fork path is ported.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::{fmt, io};

use devices::virtio::persist::{VmDevicesState, restore_device, snapshot_device};
use devices::{BusDevice, DeviceType};
use kernel::cmdline as kernel_cmdline;

/// Errors for the MMIO device manager.
#[allow(clippy::enum_variant_names)]
#[derive(Debug)]
pub enum Error {
    /// Failed to create MmioTransport.
    CreateMmioTransport(devices::virtio::CreateMmioTransportError),
    /// Failed to perform an operation on the bus.
    BusError(devices::BusError),
    /// Appending to kernel command line failed.
    Cmdline(kernel_cmdline::Error),
    /// Failure in creating or cloning an event fd.
    EventFd(io::Error),
    /// No more IRQs are available.
    IrqsExhausted,
    /// The device couldn't be found.
    DeviceNotFound,
    /// Failed to update the mmio device.
    UpdateFailed,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Error::CreateMmioTransport(ref e) => {
                write!(f, "failed to create mmio transport for the device {e}")
            }
            Error::BusError(ref e) => write!(f, "failed to perform bus operation: {e}"),
            Error::Cmdline(ref e) => {
                write!(f, "unable to add device to kernel command line: {e}")
            }
            Error::EventFd(ref e) => write!(f, "failed to create or clone event descriptor: {e}"),
            Error::IrqsExhausted => write!(f, "no more IRQs are available"),
            Error::DeviceNotFound => write!(f, "the device couldn't be found"),
            Error::UpdateFailed => write!(f, "failed to update the mmio device"),
        }
    }
}

impl From<devices::virtio::CreateMmioTransportError> for Error {
    fn from(e: devices::virtio::CreateMmioTransportError) -> Self {
        Self::CreateMmioTransport(e)
    }
}

type Result<T> = ::std::result::Result<T, Error>;

/// Size of the MMIO device window passed to the kernel cmdline (must exceed
/// 0x100 + the virtio config space; hardcoded to 4K like the other backends).
const MMIO_LEN: u64 = 0x1000;

/// Manages the complexities of registering an MMIO device on WHP.
pub struct MMIODeviceManager {
    pub bus: devices::Bus,
    mmio_base: u64,
    irq: u32,
    last_irq: u32,
    id_to_dev_info: HashMap<(DeviceType, String), MMIODeviceInfo>,
    /// Handles to registered virtio devices, kept so checkpoint/fork can
    /// capture/restore their state without downcasting through the bus.
    virtio_devices: Vec<Arc<Mutex<dyn devices::virtio::VirtioDevice>>>,
    /// MMIO transports, kept so a restored clone can re-activate each device
    /// from saved queue state.
    mmio_transports: Vec<Arc<Mutex<devices::virtio::MmioTransport>>>,
}

impl MMIODeviceManager {
    /// Create a new device manager for MMIO virtio devices.
    pub fn new(mmio_base: &mut u64, irq_interval: (u32, u32)) -> MMIODeviceManager {
        MMIODeviceManager {
            mmio_base: *mmio_base,
            irq: irq_interval.0,
            last_irq: irq_interval.1,
            bus: devices::Bus::new(),
            id_to_dev_info: HashMap::new(),
            virtio_devices: Vec::new(),
            mmio_transports: Vec::new(),
        }
    }

    /// Register an MMIO IOAPIC device (places its window on the bus).
    pub fn register_mmio_ioapic(
        &mut self,
        intc: Option<Arc<Mutex<devices::legacy::IrqChipDevice>>>,
    ) -> Result<()> {
        if let Some(intc) = intc {
            let (addr, size) = {
                let intc = intc.lock().unwrap();
                (intc.get_mmio_addr(), intc.get_mmio_size())
            };
            self.bus.insert(intc, addr, size).map_err(Error::BusError)?;
        }
        Ok(())
    }

    /// Register an already-created MMIO device behind an MMIO transport.
    ///
    /// Unlike KVM there is no `register_ioevent`/`register_irqfd`: WHP traps the
    /// guest's notify-register write into the run loop (delivered to the bus),
    /// and the IRQ is injected via `WhpVm::request_interrupt`. See the
    /// `TODO(whp-host)` at the top of this file.
    pub fn register_mmio_device(
        &mut self,
        mut mmio_device: devices::virtio::MmioTransport,
        type_id: u32,
        device_id: String,
    ) -> Result<(u64, u32)> {
        if self.irq > self.last_irq {
            return Err(Error::IrqsExhausted);
        }

        mmio_device.set_irq_line(self.irq);

        self.virtio_devices.push(mmio_device.device());
        let transport = Arc::new(Mutex::new(mmio_device));
        self.mmio_transports.push(transport.clone());

        self.bus
            .insert(transport, self.mmio_base, MMIO_LEN)
            .map_err(Error::BusError)?;
        let ret = (self.mmio_base, self.irq);
        self.id_to_dev_info.insert(
            (DeviceType::Virtio(type_id), device_id),
            MMIODeviceInfo {
                addr: self.mmio_base,
                _len: MMIO_LEN,
                _irq: self.irq,
            },
        );
        self.mmio_base += MMIO_LEN;
        self.irq += 1;

        Ok(ret)
    }

    /// Append a registered MMIO device to the kernel cmdline.
    pub fn add_device_to_cmdline(
        &mut self,
        cmdline: &mut kernel_cmdline::Cmdline,
        mmio_base: u64,
        irq: u32,
    ) -> Result<()> {
        // virtio_mmio.device=<size>K@<baseaddr>:<irq>
        cmdline
            .insert(
                "virtio_mmio.device",
                &format!("{}K@0x{:08x}:{}", MMIO_LEN / 1024, mmio_base, irq),
            )
            .map_err(Error::Cmdline)
    }

    /// Get the specified device off the bus.
    pub fn get_device(
        &self,
        device_type: DeviceType,
        device_id: &str,
    ) -> Option<&Mutex<dyn BusDevice>> {
        if let Some(dev_info) = self
            .id_to_dev_info
            .get(&(device_type, device_id.to_string()))
            && let Some((_, device)) = self.bus.get_device(dev_info.addr)
        {
            return Some(device);
        }
        None
    }

    /// Capture the runtime state of every snapshot-supporting virtio device.
    pub fn snapshot_devices(&self) -> VmDevicesState {
        let mut snapshots = Vec::new();
        for dev in &self.virtio_devices {
            let guard = dev.lock().expect("poisoned virtio device lock");
            if let Some(snap) = snapshot_device(&*guard) {
                snapshots.push(snap);
            }
        }
        VmDevicesState { devices: snapshots }
    }

    /// Re-activate devices on a freshly-built clone from a checkpoint.
    pub fn restore_activate_devices(
        &self,
        state: &VmDevicesState,
    ) -> std::result::Result<(), String> {
        let mut used = vec![false; self.mmio_transports.len()];
        for snap in &state.devices {
            let want = snap.device_type();
            let queue_states = snap.queue_states();
            let acked = snap.acked_features();
            let mut applied = false;
            for (i, transport) in self.mmio_transports.iter().enumerate() {
                if used[i] {
                    continue;
                }
                let mut t = transport.lock().expect("poisoned transport lock");
                if t.locked_device().device_type() != want {
                    continue;
                }
                restore_device(&mut *t.locked_device(), snap)?;
                t.restore_and_activate(&queue_states, acked)?;
                t.locked_device().finish_restore_activation();
                used[i] = true;
                applied = true;
                break;
            }
            if !applied {
                return Err(format!("no matching transport to restore device type {want}"));
            }
        }
        Ok(())
    }

    /// Quiesce every virtio device before snapshotting.
    pub fn quiesce_devices(&self) {
        for dev in &self.virtio_devices {
            dev.lock()
                .expect("poisoned virtio device lock")
                .quiesce_for_snapshot();
        }
    }

    /// Re-arm every virtio device quiesced by [`Self::quiesce_devices`].
    pub fn rearm_devices(&self) {
        for dev in &self.virtio_devices {
            dev.lock()
                .expect("poisoned virtio device lock")
                .rearm_after_snapshot();
        }
    }

    /// Restore device runtime state onto freshly-constructed devices.
    pub fn restore_devices(&self, state: &VmDevicesState) -> std::result::Result<(), String> {
        for snap in &state.devices {
            let mut applied = false;
            for dev in &self.virtio_devices {
                let mut guard = dev.lock().expect("poisoned virtio device lock");
                if restore_device(&mut *guard, snap).is_ok() {
                    applied = true;
                    break;
                }
            }
            if !applied {
                return Err(format!("no matching device to restore snapshot {snap:?}"));
            }
        }
        Ok(())
    }
}

/// Information about an MMIO device registered at some bus address.
#[derive(Clone, Debug)]
pub struct MMIODeviceInfo {
    addr: u64,
    _irq: u32,
    _len: u64,
}
