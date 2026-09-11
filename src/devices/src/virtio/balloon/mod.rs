mod device;
mod event_handler;

pub use self::defs::uapi::VIRTIO_ID_BALLOON as TYPE_BALLOON;
pub use self::device::{Balloon, BalloonState};

mod defs {
    use super::super::QueueConfig;

    pub const BALLOON_DEV_ID: &str = "virtio_balloon";
    pub const NUM_QUEUES: usize = 5;
    pub const QUEUE_SIZE: u16 = 256;
    pub static QUEUE_CONFIG: [QueueConfig; NUM_QUEUES] = [QueueConfig::new(QUEUE_SIZE); NUM_QUEUES];

    pub mod uapi {
        pub const VIRTIO_F_VERSION_1: u32 = 32;
        pub const VIRTIO_ID_BALLOON: u32 = 5;
        pub const VIRTIO_BALLOON_F_STATS_VQ: u32 = 1;
        pub const VIRTIO_BALLOON_F_DEFLATE_ON_OOM: u32 = 2;
        pub const VIRTIO_BALLOON_F_FREE_PAGE_HINT: u32 = 3;
        pub const VIRTIO_BALLOON_F_REPORTING: u32 = 5;

        /// Tags for the entries the guest writes on the stats queue. Values are
        /// little-endian u64 and, for the memory tags, are in bytes.
        pub const VIRTIO_BALLOON_S_SWAP_IN: u16 = 0;
        pub const VIRTIO_BALLOON_S_SWAP_OUT: u16 = 1;
        pub const VIRTIO_BALLOON_S_MAJFLT: u16 = 2;
        pub const VIRTIO_BALLOON_S_MINFLT: u16 = 3;
        pub const VIRTIO_BALLOON_S_MEMFREE: u16 = 4;
        pub const VIRTIO_BALLOON_S_MEMTOT: u16 = 5;
        pub const VIRTIO_BALLOON_S_AVAIL: u16 = 6;
        pub const VIRTIO_BALLOON_S_CACHES: u16 = 7;
    }
}

/// What the guest says about its own memory, taken from the balloon stats
/// queue. All fields are bytes unless named as a count.
///
/// This is the only accurate account of a machine's memory. The host's
/// `phys_footprint` is not: the hypervisor charges guest RAM to the VMM at
/// roughly twice the pages actually backed, and keeps charging it after the
/// guest frees them. These numbers come from the guest's own allocator.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct BalloonStats {
    /// Total usable RAM the guest sees.
    pub mem_total: u64,
    /// Free RAM: never allocated, or released and not reused.
    pub mem_free: u64,
    /// Memory available for new allocations without swapping. This is the
    /// figure to report, because `mem_free` excludes reclaimable page cache.
    pub mem_available: u64,
    /// Page cache. Counted inside `mem_available`.
    pub caches: u64,
    pub swap_in: u64,
    pub swap_out: u64,
    /// Fault counts, not bytes.
    pub major_faults: u64,
    pub minor_faults: u64,
}

impl BalloonStats {
    /// Memory the guest cannot hand back on demand.
    pub fn in_use(&self) -> u64 {
        self.mem_total.saturating_sub(self.mem_available)
    }
}

#[derive(Debug)]
pub enum BalloonError {
    /// Failed to create event fd.
    EventFd(std::io::Error),
}

type Result<T> = std::result::Result<T, BalloonError>;
