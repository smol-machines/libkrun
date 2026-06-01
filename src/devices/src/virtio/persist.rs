// Copyright 2026. SPDX-License-Identifier: Apache-2.0
//
//! VM-level device-state aggregation for checkpoint/fork.
//!
//! Composes the per-device `*State` snapshots (built on `Queue::save_state` and
//! each device's `save_state`/`restore_state`) into a single [`VmDevicesState`].
//!
//! The device manager holds devices polymorphically as `&dyn VirtioDevice`, so
//! this module downcasts (via the `AsAny` supertrait) to the concrete device
//! types to capture/restore them. That keeps the `VirtioDevice` trait itself
//! unaware of snapshots, and devices without a Persist impl yet (balloon, rng,
//! fs, gpu, snd, input) are simply skipped.

#[cfg(feature = "blk")]
use crate::virtio::{block::BlockState, Block};
#[cfg(feature = "net")]
use crate::virtio::{net::NetState, Net};
use crate::virtio::{Console, ConsoleState, VirtioDevice, Vsock, VsockState};
#[cfg(not(any(feature = "tee", feature = "aws-nitro")))]
use crate::virtio::{Fs, FsState};
#[cfg(not(feature = "tee"))]
use crate::virtio::{Rng, RngState};

/// Snapshot of a single virtio device's runtime state.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DeviceSnapshot {
    Console(ConsoleState),
    Vsock(VsockState),
    #[cfg(not(any(feature = "tee", feature = "aws-nitro")))]
    Fs(FsState),
    #[cfg(not(feature = "tee"))]
    Rng(RngState),
    #[cfg(feature = "blk")]
    Block(BlockState),
    #[cfg(feature = "net")]
    Net(NetState),
}

impl DeviceSnapshot {
    /// The virtio device type id (`TYPE_*`) this snapshot belongs to — used to
    /// match a snapshot to its transport when restoring a fresh clone.
    pub fn device_type(&self) -> u32 {
        use crate::virtio::*;
        match self {
            DeviceSnapshot::Console(_) => TYPE_CONSOLE,
            DeviceSnapshot::Vsock(_) => TYPE_VSOCK,
            #[cfg(not(any(feature = "tee", feature = "aws-nitro")))]
            DeviceSnapshot::Fs(_) => TYPE_FS,
            #[cfg(not(feature = "tee"))]
            DeviceSnapshot::Rng(_) => TYPE_RNG,
            #[cfg(feature = "blk")]
            DeviceSnapshot::Block(_) => TYPE_BLOCK,
            #[cfg(feature = "net")]
            DeviceSnapshot::Net(_) => TYPE_NET,
        }
    }

    /// Negotiated feature bits to restore before re-activation.
    pub fn acked_features(&self) -> u64 {
        match self {
            DeviceSnapshot::Console(s) => s.acked_features,
            DeviceSnapshot::Vsock(s) => s.acked_features,
            #[cfg(not(any(feature = "tee", feature = "aws-nitro")))]
            DeviceSnapshot::Fs(s) => s.acked_features,
            #[cfg(not(feature = "tee"))]
            DeviceSnapshot::Rng(s) => s.acked_features,
            #[cfg(feature = "blk")]
            DeviceSnapshot::Block(s) => s.acked_features,
            #[cfg(feature = "net")]
            DeviceSnapshot::Net(s) => s.acked_features,
        }
    }

    /// Per-queue saved state, in queue index order, for reconstructing the
    /// transport's queues on re-activation.
    pub fn queue_states(&self) -> Vec<Option<crate::virtio::queue::QueueState>> {
        match self {
            DeviceSnapshot::Console(s) => s.queues.clone(),
            DeviceSnapshot::Vsock(s) => vec![s.queue_rx.clone(), s.queue_tx.clone()],
            #[cfg(not(any(feature = "tee", feature = "aws-nitro")))]
            DeviceSnapshot::Fs(s) => s.queues.clone(),
            #[cfg(not(feature = "tee"))]
            DeviceSnapshot::Rng(s) => vec![s.queue.clone()],
            #[cfg(feature = "blk")]
            DeviceSnapshot::Block(s) => vec![s.queue.clone()],
            #[cfg(feature = "net")]
            DeviceSnapshot::Net(s) => vec![s.queue_rx.clone(), s.queue_tx.clone()],
        }
    }
}

/// Aggregate of all snapshot-supporting devices in a VM, for checkpoint/fork.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VmDevicesState {
    pub devices: Vec<DeviceSnapshot>,
}

impl VmDevicesState {
    /// Serialize to bytes for cross-process fork / on-disk hibernate. Uses JSON
    /// (the state is small — queue indices, features, ids — and human-readable
    /// output is convenient for debugging snapshots). No cross-version
    /// compatibility is promised.
    pub fn to_bytes(&self) -> std::result::Result<Vec<u8>, String> {
        serde_json::to_vec(self).map_err(|e| format!("serialize device state: {e}"))
    }

    /// Reconstruct from bytes produced by [`Self::to_bytes`].
    pub fn from_bytes(bytes: &[u8]) -> std::result::Result<Self, String> {
        serde_json::from_slice(bytes).map_err(|e| format!("deserialize device state: {e}"))
    }
}

/// Capture one device's state, if it supports Persist. Returns `None` for
/// device types without a snapshot impl yet.
pub fn snapshot_device(dev: &dyn VirtioDevice) -> Option<DeviceSnapshot> {
    let any = dev.as_any();
    if let Some(d) = any.downcast_ref::<Console>() {
        return Some(DeviceSnapshot::Console(d.save_state()));
    }
    if let Some(d) = any.downcast_ref::<Vsock>() {
        return Some(DeviceSnapshot::Vsock(d.save_state()));
    }
    #[cfg(not(any(feature = "tee", feature = "aws-nitro")))]
    if let Some(d) = any.downcast_ref::<Fs>() {
        return Some(DeviceSnapshot::Fs(d.save_state()));
    }
    #[cfg(not(feature = "tee"))]
    if let Some(d) = any.downcast_ref::<Rng>() {
        return Some(DeviceSnapshot::Rng(d.save_state()));
    }
    #[cfg(feature = "blk")]
    if let Some(d) = any.downcast_ref::<Block>() {
        return Some(DeviceSnapshot::Block(d.save_state()));
    }
    #[cfg(feature = "net")]
    if let Some(d) = any.downcast_ref::<Net>() {
        return Some(DeviceSnapshot::Net(d.save_state()));
    }
    None
}

/// Restore one device's state, matching the snapshot variant to the concrete
/// device type. Errors on a snapshot/device type mismatch.
pub fn restore_device(dev: &mut dyn VirtioDevice, snap: &DeviceSnapshot) -> Result<(), String> {
    let any = dev.as_mut_any();
    match snap {
        DeviceSnapshot::Console(s) => any
            .downcast_mut::<Console>()
            .ok_or_else(|| "snapshot/device mismatch: expected Console".to_string())?
            .restore_state(s),
        DeviceSnapshot::Vsock(s) => any
            .downcast_mut::<Vsock>()
            .ok_or_else(|| "snapshot/device mismatch: expected Vsock".to_string())?
            .restore_state(s),
        #[cfg(not(any(feature = "tee", feature = "aws-nitro")))]
        DeviceSnapshot::Fs(s) => any
            .downcast_mut::<Fs>()
            .ok_or_else(|| "snapshot/device mismatch: expected Fs".to_string())?
            .restore_state(s),
        #[cfg(not(feature = "tee"))]
        DeviceSnapshot::Rng(s) => any
            .downcast_mut::<Rng>()
            .ok_or_else(|| "snapshot/device mismatch: expected Rng".to_string())?
            .restore_state(s),
        #[cfg(feature = "blk")]
        DeviceSnapshot::Block(s) => any
            .downcast_mut::<Block>()
            .ok_or_else(|| "snapshot/device mismatch: expected Block".to_string())?
            .restore_state(s),
        #[cfg(feature = "net")]
        DeviceSnapshot::Net(s) => any
            .downcast_mut::<Net>()
            .ok_or_else(|| "snapshot/device mismatch: expected Net".to_string())?
            .restore_state(s),
    }
}

impl VmDevicesState {
    /// Capture all snapshot-supporting devices from an iterator of device refs.
    /// The device manager calls this over its activated virtio devices.
    pub fn capture<'a, I>(devices: I) -> Self
    where
        I: IntoIterator<Item = &'a dyn VirtioDevice>,
    {
        VmDevicesState {
            devices: devices.into_iter().filter_map(snapshot_device).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtio::queue::QueueState;
    use crate::virtio::{ConsoleState, VsockState};

    #[test]
    fn test_device_state_serialization_roundtrip() {
        let qs = QueueState {
            size: 256,
            ready: true,
            desc_table: 0x1000,
            avail_ring: 0x2000,
            used_ring: 0x3000,
            next_avail: 42,
            next_used: 41,
            event_idx_enabled: true,
            num_added: 7,
        };
        let state = VmDevicesState {
            devices: vec![
                DeviceSnapshot::Console(ConsoleState {
                    acked_features: 0xABCD,
                    activated: true,
                    queues: vec![Some(qs.clone()), None],
                }),
                DeviceSnapshot::Vsock(VsockState {
                    cid: 7,
                    acked_features: 0x1234,
                    activated: true,
                    queue_rx: Some(qs.clone()),
                    queue_tx: Some(qs),
                }),
            ],
        };

        let bytes = state.to_bytes().expect("serialize");
        let restored = VmDevicesState::from_bytes(&bytes).expect("deserialize");
        assert_eq!(
            state, restored,
            "device state must round-trip through bytes"
        );
    }
}
