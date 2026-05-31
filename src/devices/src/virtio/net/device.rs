// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.
use crate::virtio::net::Error;
use crate::virtio::net::Result;
use crate::virtio::net::{NUM_QUEUES, QUEUE_CONFIG};
use crate::virtio::queue::Error as QueueError;
use crate::virtio::queue::QueueState;
use crate::virtio::{
    ActivateError, ActivateResult, DeviceQueue, DeviceState, InterruptTransport, QueueConfig,
    VirtioDevice, TYPE_NET,
};
use crate::Error as DeviceError;

use std::thread::JoinHandle;
use utils::eventfd::{EventFd, EFD_NONBLOCK};

use super::backend::{ReadError, WriteError};
use super::worker::NetWorker;

use std::cmp;
use std::io::Write;
use std::os::fd::RawFd;
use std::path::PathBuf;
use virtio_bindings::virtio_net::VIRTIO_NET_F_MAC;
use virtio_bindings::virtio_ring::VIRTIO_RING_F_EVENT_IDX;
use vm_memory::{ByteValued, GuestMemoryError, GuestMemoryMmap};

const VIRTIO_F_VERSION_1: u32 = 32;

#[derive(Debug)]
pub enum FrontendError {
    DescriptorChainTooSmall,
    EmptyQueue,
    GuestMemory(GuestMemoryError),
    QueueError(QueueError),
    ReadOnlyDescriptor,
}

#[derive(Debug)]
pub enum RxError {
    Backend(ReadError),
    DeviceError(DeviceError),
}

#[derive(Debug)]
pub enum TxError {
    Backend(WriteError),
    DeviceError(DeviceError),
    QueueError(QueueError),
}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C, packed)]
struct VirtioNetConfig {
    mac: [u8; 6],
    status: u16,
    max_virtqueue_pairs: u16,
}

// Safe because it only has data and has no implicit padding.
unsafe impl ByteValued for VirtioNetConfig {}

#[derive(Clone)]
pub enum VirtioNetBackend {
    UnixstreamFd(RawFd),
    UnixstreamPath(PathBuf),
    UnixgramFd(RawFd),
    UnixgramPath(PathBuf, bool),
    #[cfg(target_os = "linux")]
    Tap(String),
}

pub struct Net {
    id: String,
    pub cfg_backend: VirtioNetBackend,

    avail_features: u64,
    acked_features: u64,

    pub(crate) device_state: DeviceState,

    config: VirtioNetConfig,

    worker_thread: Option<JoinHandle<NetWorker>>,
    worker_stopfd: EventFd,
    /// Worker reclaimed by [`Self::quiesce_for_snapshot`] (stopped): holds the
    /// rx/tx queues + backend so their state can be snapshotted/restored and the
    /// worker re-armed. `None` during normal running.
    quiesced_worker: Option<NetWorker>,
}

impl Net {
    /// Create a new virtio network device using the backend
    pub fn new(
        id: String,
        cfg_backend: VirtioNetBackend,
        mac: [u8; 6],
        features: u32,
    ) -> Result<Self> {
        let avail_features = features as u64
            | (1 << VIRTIO_NET_F_MAC)
            | (1 << VIRTIO_RING_F_EVENT_IDX)
            | (1 << VIRTIO_F_VERSION_1);

        let config = VirtioNetConfig {
            mac,
            status: 0,
            max_virtqueue_pairs: 0,
        };

        Ok(Net {
            id,
            cfg_backend,

            avail_features,
            acked_features: 0u64,

            device_state: DeviceState::Inactive,
            config,
            worker_thread: None,
            worker_stopfd: EventFd::new(EFD_NONBLOCK).map_err(Error::EventFd)?,
            quiesced_worker: None,
        })
    }

    /// Provides the ID of this net device.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Provides the virtio-net backend of this net device.
    pub fn backend(&self) -> &VirtioNetBackend {
        &self.cfg_backend
    }
}

/// Serializable runtime state of the [`Net`] device, for VM checkpoint/fork.
///
/// Captures device-level negotiated state (acked features, activation). The MAC
/// config and the tap/backend are re-provided from the VM config on restore.
/// Like block, net's virtqueues live in its worker thread, so queue state and
/// the in-flight packet drain require the shared worker-quiesce step — not
/// captured here.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NetState {
    pub acked_features: u64,
    pub activated: bool,
    /// rx/tx virtqueue indices, captured after the worker is drained
    /// ([`Net::quiesce_for_snapshot`]). `None` if not activated/quiesced.
    pub queue_rx: Option<QueueState>,
    pub queue_tx: Option<QueueState>,
}

impl Net {
    /// Capture device-level + virtqueue runtime state for VM checkpoint/fork.
    /// The caller must have run [`Self::quiesce_for_snapshot`] first.
    pub fn save_state(&self) -> NetState {
        let (queue_rx, queue_tx) = match self.quiesced_worker.as_ref() {
            Some(w) => {
                let (rx, tx) = w.save_queue_states();
                (Some(rx), Some(tx))
            }
            None => (None, None),
        };
        NetState {
            acked_features: self.acked_features,
            activated: matches!(self.device_state, DeviceState::Activated(..)),
            queue_rx,
            queue_tx,
        }
    }

    /// Restore device-level + virtqueue state. The worker must be quiesced first;
    /// the restored indices take effect when [`Self::rearm_after_snapshot`] re-arms it.
    pub fn restore_state(&mut self, state: &NetState) -> std::result::Result<(), String> {
        self.acked_features = state.acked_features;
        if let (Some(worker), Some(rx), Some(tx)) = (
            self.quiesced_worker.as_mut(),
            state.queue_rx.as_ref(),
            state.queue_tx.as_ref(),
        ) {
            worker.restore_queue_states(rx, tx)?;
        }
        Ok(())
    }

    /// Stop + reclaim the worker so its rx/tx queue indices can be snapshotted at
    /// a clean boundary. Idempotent; no-op if not running. Device stays activated.
    fn quiesce_worker(&mut self) {
        if let Some(handle) = self.worker_thread.take() {
            let _ = self.worker_stopfd.write(1);
            match handle.join() {
                Ok(worker) => self.quiesced_worker = Some(worker),
                Err(e) => error!("net: error draining worker thread: {e:?}"),
            }
        }
    }

    /// Re-arm a worker reclaimed by [`Self::quiesce_worker`] from the (possibly
    /// restored) queue indices. No-op if not quiesced.
    fn rearm_worker(&mut self) {
        if let Some(worker) = self.quiesced_worker.take() {
            self.worker_thread = Some(worker.run());
        }
    }
}

impl VirtioDevice for Net {
    fn avail_features(&self) -> u64 {
        self.avail_features
    }

    fn acked_features(&self) -> u64 {
        self.acked_features
    }

    fn set_acked_features(&mut self, acked_features: u64) {
        self.acked_features = acked_features;
    }

    fn device_type(&self) -> u32 {
        TYPE_NET
    }

    fn device_name(&self) -> &str {
        "net"
    }

    fn queue_config(&self) -> &[QueueConfig] {
        &QUEUE_CONFIG
    }

    fn read_config(&self, offset: u64, mut data: &mut [u8]) {
        let config_slice = self.config.as_slice();
        let config_len = config_slice.len() as u64;
        if offset >= config_len {
            error!("Failed to read config space");
            return;
        }
        if let Some(end) = offset.checked_add(data.len() as u64) {
            // This write can't fail, offset and end are checked against config_len.
            data.write_all(&config_slice[offset as usize..cmp::min(end, config_len) as usize])
                .unwrap();
        }
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        log::warn!(
            "Net: guest driver attempted to write device config (offset={:x}, len={:x})",
            offset,
            data.len()
        );
    }

    fn activate(
        &mut self,
        mem: GuestMemoryMmap,
        interrupt: InterruptTransport,
        queues: Vec<DeviceQueue>,
    ) -> ActivateResult {
        let [rx_q, tx_q]: [_; NUM_QUEUES] = queues.try_into().map_err(|_| {
            error!("Cannot perform activate. Expected {} queue(s)", NUM_QUEUES);
            ActivateError::BadActivate
        })?;

        let stop_fd = match self.worker_stopfd.try_clone() {
            Ok(fd) => fd,
            Err(e) => {
                error!("virtio-net: cannot clone stop fd: {e:?}");
                return Err(ActivateError::BadActivate);
            }
        };

        match NetWorker::new(
            rx_q,
            tx_q,
            interrupt.clone(),
            mem.clone(),
            self.acked_features,
            self.cfg_backend.clone(),
            stop_fd,
        ) {
            Ok(worker) => {
                self.worker_thread = Some(worker.run());
                self.device_state = DeviceState::Activated(mem, interrupt);
                Ok(())
            }
            Err(err) => {
                error!(
                    "Error activating virtio-net ({}) backend: {err:?}",
                    self.id()
                );
                Err(ActivateError::BadActivate)
            }
        }
    }

    fn is_activated(&self) -> bool {
        self.device_state.is_activated()
    }

    fn reset(&mut self) -> bool {
        if let Some(worker) = self.worker_thread.take() {
            let _ = self.worker_stopfd.write(1);
            if let Err(e) = worker.join() {
                error!("net: error waiting for worker thread: {e:?}");
            }
        }
        self.quiesced_worker = None;
        self.device_state = DeviceState::Inactive;
        true
    }

    /// Quiesce for checkpoint/fork: drain + reclaim the worker's rx/tx queues.
    fn quiesce_for_snapshot(&mut self) {
        self.quiesce_worker();
    }

    /// Re-arm the worker after checkpoint/restore (resumes packet processing).
    fn rearm_after_snapshot(&mut self) {
        self.rearm_worker();
    }
}
