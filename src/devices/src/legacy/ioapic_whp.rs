// Copyright 2026 Red Hat, Inc.
// SPDX-License-Identifier: Apache-2.0

//! WHP IOAPIC backend.
//!
//! WHP emulates the LAPIC but NOT the IOAPIC. This backend provides
//! interrupt injection through `WHvRequestInterrupt`, plugging into the
//! common IOAPIC register emulation in [`super::ioapic`].

use std::io;
use std::sync::Arc;

use whp::{InterruptDestinationMode, InterruptRequest, InterruptTriggerMode, InterruptType, WhpVm};

use crate::Error as DeviceError;
use utils::eventfd::EventFd;

use super::ioapic::{
    IOAPIC_DM_EXTINT, IOAPIC_DM_MASK, IOAPIC_LVT_DELIV_MODE_SHIFT, IOAPIC_LVT_DEST_MODE_SHIFT,
    IOAPIC_LVT_MASKED_SHIFT, IOAPIC_NUM_PINS, IOAPIC_VECTOR_MASK, IoApicBackend, IoApicRegs,
    Ioapic,
};

const IOAPIC_LVT_DEST_IDX_SHIFT: u64 = 56;

pub struct WhpIoapicBackend {
    vm: Arc<WhpVm>,
}

impl WhpIoapicBackend {
    fn service(regs: &mut IoApicRegs, vm: &WhpVm) {
        for i in 0..IOAPIC_NUM_PINS {
            let mask = 1u32 << i;
            if regs.irr & mask == 0 {
                continue;
            }

            let entry = regs.ioredtbl[i];
            if (entry >> IOAPIC_LVT_MASKED_SHIFT) & 1 != 0 {
                continue;
            }

            let vector = (entry & IOAPIC_VECTOR_MASK) as u32;
            let dest = ((entry >> IOAPIC_LVT_DEST_IDX_SHIFT) & 0xff) as u32;
            let dest_mode = ((entry >> IOAPIC_LVT_DEST_MODE_SHIFT) & 1) as u8;
            let deliv_mode = ((entry >> IOAPIC_LVT_DELIV_MODE_SHIFT) & IOAPIC_DM_MASK) as u8;

            if deliv_mode as u64 == IOAPIC_DM_EXTINT {
                error!("ioapic: ExtINT delivery mode not supported (pin {i})");
                continue;
            }

            // Clear IRR after injecting (both edge and level). WHP emulates the
            // LAPIC and handles guest EOI natively WITHOUT notifying this software
            // IOAPIC, so the remote-IRR/EOI handshake for level-triggered lines
            // cannot be tracked here: setting REMOTE_IRR and waiting for an EOI
            // that never arrives would block all further interrupts on the pin
            // (this stalled virtio-fs after FUSE_INIT on real WHP hardware).
            // Instead, inject once per device assertion (`set_irq`) and rely on
            // the device re-asserting for the next interrupt — virtio devices
            // signal once per batch of completions, so every batch is delivered
            // without an interrupt storm.
            regs.irr &= !mask;

            let req = InterruptRequest {
                interrupt_type: match deliv_mode {
                    1 => InterruptType::LowestPriority,
                    4 => InterruptType::Nmi,
                    5 => InterruptType::Init,
                    6 => InterruptType::Sipi,
                    _ => InterruptType::Fixed,
                },
                destination_mode: if dest_mode == 0 {
                    InterruptDestinationMode::Physical
                } else {
                    InterruptDestinationMode::Logical
                },
                // Always inject as edge-triggered, even when the guest programmed
                // the redirection entry as level-triggered. This backend uses a
                // once-per-assertion model: `service` injects a single interrupt
                // per `set_irq` and immediately clears IRR (it does not track
                // remote-IRR, because WHP services guest EOIs natively in the
                // LAPIC and never notifies this software IOAPIC). Requesting a
                // *level* interrupt would tell WHP the line is held high with no
                // matching de-assertion, so the LAPIC re-fires after every EOI —
                // an interrupt storm that spins the guest. An edge pulse delivers
                // exactly one interrupt per device assertion, which is what the
                // virtio devices signal (once per batch of completions).
                trigger_mode: InterruptTriggerMode::Edge,
                destination: dest,
                vector,
            };

            if let Err(e) = vm.request_interrupt(&req) {
                error!("ioapic: WHvRequestInterrupt failed for pin {i}: {e}");
            }
        }
    }
}

impl IoApicBackend for WhpIoapicBackend {
    fn on_entry_changed(&mut self, regs: &mut IoApicRegs, _index: usize) {
        Self::service(regs, &self.vm);
    }

    fn on_eoi(&mut self, regs: &mut IoApicRegs) {
        Self::service(regs, &self.vm);
    }

    fn set_irq(
        &mut self,
        irq_line: Option<u32>,
        _interrupt_evt: Option<&EventFd>,
        regs: &mut IoApicRegs,
    ) -> Result<(), DeviceError> {
        let irq = irq_line.ok_or_else(|| {
            DeviceError::FailedSignalingUsedQueue(io::Error::new(
                io::ErrorKind::InvalidData,
                "IRQ line not configured",
            ))
        })?;

        if irq as usize >= IOAPIC_NUM_PINS {
            return Err(DeviceError::FailedSignalingUsedQueue(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("IRQ {irq} out of IOAPIC pin range"),
            )));
        }

        regs.irr |= 1 << irq;
        Self::service(regs, &self.vm);
        Ok(())
    }
}

pub type WhpIoapic = Ioapic<WhpIoapicBackend>;

impl Ioapic<WhpIoapicBackend> {
    pub fn new(vm: Arc<WhpVm>) -> Self {
        Ioapic::from_backend(WhpIoapicBackend { vm })
    }
}
