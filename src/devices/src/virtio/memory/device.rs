use std::io::{Read, Write};
use std::os::fd::AsRawFd;

use polly::event_manager::{EventManager, Subscriber};
use utils::epoll::{EpollEvent, EventSet};
use utils::eventfd::{EFD_NONBLOCK, EventFd};
use vm_memory::GuestMemoryMmap;

use super::MemoryState;
use crate::virtio::descriptor_utils::{Reader, Writer};
use crate::virtio::{
    ActivateError, ActivateResult, DeviceQueue, DeviceState, InterruptTransport, QueueConfig,
    VirtioDevice,
};

const FEATURES: u64 = (1 << 32) | (1 << 1); // VERSION_1, UNPLUGGED_INACCESSIBLE
const QUEUES: [QueueConfig; 1] = [QueueConfig::new(128)];

pub struct MemoryDevice {
    state: MemoryState,
    acked: u64,
    queues: Vec<DeviceQueue>,
    activation: EventFd,
    device: DeviceState,
    quiesced: bool,
}

impl MemoryDevice {
    pub fn new(state: MemoryState) -> std::io::Result<Self> {
        Ok(Self {
            state,
            acked: 0,
            queues: Vec::new(),
            activation: EventFd::new(EFD_NONBLOCK)?,
            device: DeviceState::Inactive,
            quiesced: false,
        })
    }

    /// RAM must already be mapped and owned before notifying the guest.
    pub fn request_growth(&mut self, requested: u64, usable: u64) -> Result<(), String> {
        self.state.request_growth(requested, usable)?;
        if let DeviceState::Activated(_, interrupt) = &self.device {
            interrupt
                .try_signal_config_change()
                .map_err(|e| format!("RAM growth recorded but guest notification failed: {e:?}"))?;
        }
        Ok(())
    }

    pub fn plugged_size(&self) -> u64 {
        self.state.plugged_size()
    }

    pub fn geometry(&self) -> (u64, u64, u64) {
        (
            self.state.addr,
            self.state.usable_size,
            self.state.region_size,
        )
    }

    fn process_queue(&mut self) {
        if self.quiesced {
            return;
        }
        let DeviceState::Activated(memory, _) = &self.device else {
            return;
        };
        let queue = &mut self.queues[0].queue;
        let mut used = false;
        while let Some(head) = queue.pop(memory) {
            let index = head.index;
            let length = (|| {
                let mut reader = Reader::new(memory, head.clone()).ok()?;
                let mut writer = Writer::new(memory, head).ok()?;
                if reader.available_bytes() != 24 || writer.available_bytes() < 10 {
                    return None;
                }
                let mut request = [0; 24];
                reader.read_exact(&mut request).ok()?;
                // Validate/write the reply before committing plug accounting.
                // The device lock keeps configuration reads coherent with it.
                let mut next = self.state.clone();
                let response = next.request(&request);
                writer.write_all(&response).ok()?;
                self.state = next;
                Some(10)
            })()
            .unwrap_or(0);
            if let Err(error) = queue.add_used(memory, index, length) {
                error!("virtio-mem: failed to publish response: {error}");
                break;
            }
            used = true;
        }
        if used {
            self.device.signal_used_queue();
        }
    }

    fn kick(&self) {
        if let Some(queue) = self.queues.first()
            && let Err(error) = queue.event.write(1)
        {
            error!("virtio-mem: queue notification failed: {error}");
        }
    }
}

impl VirtioDevice for MemoryDevice {
    fn avail_features(&self) -> u64 {
        FEATURES
    }
    fn acked_features(&self) -> u64 {
        self.acked
    }
    fn set_acked_features(&mut self, features: u64) {
        self.acked = features & FEATURES;
    }
    fn device_type(&self) -> u32 {
        24
    }
    fn device_name(&self) -> &str {
        "virtio-mem"
    }
    fn queue_config(&self) -> &[QueueConfig] {
        &QUEUES
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        data.fill(0);
        let config = self.state.config();
        if let Ok(start) = usize::try_from(offset)
            && let Some(source) = config.get(start..)
        {
            let count = source.len().min(data.len());
            data[..count].copy_from_slice(&source[..count]);
        }
    }
    fn write_config(&mut self, _offset: u64, _data: &[u8]) {}
    fn activate(
        &mut self,
        memory: GuestMemoryMmap,
        interrupt: InterruptTransport,
        queues: Vec<DeviceQueue>,
    ) -> ActivateResult {
        if queues.len() != 1 || self.device.is_activated() {
            return Err(ActivateError::BadActivate);
        }
        self.activation
            .write(1)
            .map_err(|_| ActivateError::BadActivate)?;
        self.queues = queues;
        self.device = DeviceState::Activated(memory, interrupt);
        Ok(())
    }
    fn is_activated(&self) -> bool {
        self.device.is_activated()
    }
    fn quiesce_for_snapshot(&mut self) {
        self.quiesced = true;
    }
    fn rearm_after_snapshot(&mut self) {
        self.quiesced = false;
        self.kick();
    }
    fn finish_restore_activation(&mut self) {
        self.kick();
    }
    // Do not claim reset support until the event registrations and plugged
    // memory lifecycle can both be reset safely.
}

impl Subscriber for MemoryDevice {
    fn interest_list(&self) -> Vec<EpollEvent> {
        vec![EpollEvent::new(
            EventSet::IN,
            self.activation.as_raw_fd() as u64,
        )]
    }
    fn process(&mut self, event: &EpollEvent, manager: &mut EventManager) {
        if event.event_set() != EventSet::IN {
            return;
        }
        if event.fd() == self.activation.as_raw_fd() {
            if let Err(error) = self.activation.read() {
                error!("virtio-mem: activation notification failed: {error}");
                return;
            }
            let Some(queue) = self.queues.first() else {
                return;
            };
            let Ok(subscriber) = manager.subscriber(self.activation.as_raw_fd()) else {
                return;
            };
            let fd = queue.event.as_raw_fd();
            if let Err(error) =
                manager.register(fd, EpollEvent::new(EventSet::IN, fd as u64), subscriber)
            {
                error!("virtio-mem: queue registration failed: {error:?}");
                return;
            }
            self.kick();
        } else if self
            .queues
            .first()
            .is_some_and(|q| q.event.as_raw_fd() == event.fd())
        {
            if let Err(error) = self.queues[0].event.read() {
                error!("virtio-mem: request notification failed: {error}");
            } else {
                self.process_queue();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::legacy::DummyIrqChip;
    use crate::virtio::queue::tests::VirtQueue;
    use std::sync::Arc;
    use vm_memory::{Bytes, GuestAddress};

    #[test]
    fn memory_queue_validates_response_and_respects_snapshot_boundary() {
        for response_len in [9, 10] {
            let memory = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x20000)]).unwrap();
            let ring = VirtQueue::new(GuestAddress(0x1000), &memory, 8);
            let event = Arc::new(EventFd::new(EFD_NONBLOCK).unwrap());
            let mut state = MemoryState::new(0x1_0000_0000, 1 << 30, 2 << 20).unwrap();
            state.request_growth(2 << 20, 2 << 20).unwrap();
            let mut device = MemoryDevice::new(state).unwrap();
            device
                .activate(
                    memory.clone(),
                    InterruptTransport::new(DummyIrqChip::new().into(), "memory test".into())
                        .unwrap(),
                    vec![DeviceQueue::new(ring.create_queue(), event)],
                )
                .unwrap();
            let mut request = [0; 24];
            request[8..16].copy_from_slice(&0x1_0000_0000u64.to_le_bytes());
            request[16..18].copy_from_slice(&1u16.to_le_bytes());
            memory.write_slice(&request, GuestAddress(0x10000)).unwrap();
            ring.dtable[0].set(0x10000, 24, 1, 1);
            ring.dtable[1].set(0x11000, response_len, 2, 0);
            ring.avail.ring[0].set(0);
            ring.avail.idx.set(1);
            device.quiesce_for_snapshot();
            device.process_queue();
            assert_eq!(ring.used.idx.get(), 0);
            assert_eq!(device.plugged_size(), 0);
            device.rearm_after_snapshot();
            device.process_queue();
            assert_eq!(ring.used.idx.get(), 1);
            assert_eq!(
                device.plugged_size(),
                if response_len == 10 { 2 << 20 } else { 0 }
            );
            device.process_queue();
            assert_eq!(ring.used.idx.get(), 1, "request processed twice");
        }
    }
}
