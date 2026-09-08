// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

pub mod device;
mod worker;

pub use self::device::{Block, BlockState, CacheType, create_overlay};
pub use self::worker::{PreparedAsyncIo, discard_prepared_async_io, prepare_async_io};

use vm_memory::GuestMemoryError;

use super::QueueConfig;

pub const CONFIG_SPACE_SIZE: usize = 8;
pub const SECTOR_SHIFT: u8 = 9;
pub const SECTOR_SIZE: u64 = (0x01_u64) << SECTOR_SHIFT;
const QUEUE_SIZE: u16 = 256;
pub const NUM_QUEUES: usize = 1;
pub static QUEUE_CONFIG: [QueueConfig; NUM_QUEUES] = [QueueConfig::new(QUEUE_SIZE)];

#[derive(Debug)]
pub enum Error {
    /// Guest gave us too few descriptors in a descriptor chain.
    DescriptorChainTooShort,
    /// Guest gave us a descriptor that was too short to use.
    DescriptorLengthTooSmall,
    /// Getting a block's metadata fails for any reason.
    GetFileMetadata(std::io::Error),
    /// Guest gave us bad memory addresses.
    GuestMemory(GuestMemoryError),
    /// The requested operation would cause a seek beyond disk end.
    InvalidOffset,
    /// Guest gave us a read only descriptor that protocol says to write to.
    UnexpectedReadOnlyDescriptor,
    /// Guest gave us a write only descriptor that protocol says to read from.
    UnexpectedWriteOnlyDescriptor,
}

/// Supported disk image formats
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageType {
    Raw,
    Qcow2,
    Vmdk,
}

/// Host I/O engine used by the virtio block worker.
///
/// `Sync` preserves the historical, single-worker behavior. `Async` lets the
/// worker submit multiple raw-disk reads through io_uring. Buffered writes remain inline.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BlockIoEngine {
    #[default]
    Sync,
    Async,
}

impl TryFrom<u32> for BlockIoEngine {
    type Error = ();

    fn try_from(io_engine: u32) -> Result<Self, Self::Error> {
        match io_engine {
            0 => Ok(Self::Sync),
            1 => Ok(Self::Async),
            _ => Err(()),
        }
    }
}

impl TryFrom<u32> for ImageType {
    type Error = ();

    fn try_from(disk_format: u32) -> Result<Self, Self::Error> {
        match disk_format {
            0 => Ok(ImageType::Raw),
            1 => Ok(ImageType::Qcow2),
            2 => Ok(ImageType::Vmdk),
            _ => {
                // Do not continue if the user cannot specify a valid disk format
                Err(())
            }
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SyncMode {
    None,
    Relaxed,
    #[default]
    Full,
}

impl TryFrom<u32> for SyncMode {
    type Error = ();

    fn try_from(sync_mode: u32) -> Result<Self, Self::Error> {
        match sync_mode {
            0 => Ok(SyncMode::None),
            1 => Ok(SyncMode::Relaxed),
            2 => Ok(SyncMode::Full),
            _ => {
                // Do not continue if the user cannot specify a valid sync mode
                Err(())
            }
        }
    }
}
