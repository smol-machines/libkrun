use std::sync::atomic::{AtomicU64, Ordering};

use super::fuse;

/// Allocates unique FUSE inode numbers.
///
/// FUSE inode numbers are opaque identifiers with two reserved values:
///   - `0` — invalid / negative-entry cache sentinel (never allocated)
///   - `1` (`ROOT_ID`) — the root directory of the filesystem
///
/// All other numbers are allocated sequentially from a random base chosen per
/// session, in `[FIRST_BASE, FIRST_BASE + BASE_SPAN)`. The guest keeps file
/// handles (nodeid + generation, and the generation is always 0) beyond one
/// session: overlayfs stores a lower file's handle in the upper layer as its
/// copy-up origin and decodes it again after a reboot. Numbering every boot
/// from `ROOT_ID + 1` gave that handle to whichever file the new session
/// looked up at the same position, and overlayfs then served the copied-up
/// file's contents under that other file's name. With a random base an
/// earlier session's nodeid is unknown to this one, so the guest finds no
/// inode for the handle and overlayfs ignores the origin. A restore keeps the
/// snapshotted counter, so restored guests keep their nodeids.
/// The allocator is `Send + Sync` and safe to share across threads.
pub struct InodeAllocator {
    next: AtomicU64,
}

/// Lowest base: above every nodeid a session numbered from `ROOT_ID + 1`
/// (older engines, older snapshots) will reach in practice.
const FIRST_BASE: u64 = 1 << 32;
/// Width of the base range; leaves more than 2^63 ids of headroom above it.
const BASE_SPAN: u64 = 1 << 62;

impl InodeAllocator {
    /// An allocator for a new session, numbering from a random base.
    pub fn new() -> Self {
        Self::starting_at(FIRST_BASE + rand::random::<u64>() % BASE_SPAN)
    }

    /// An allocator numbering from `ROOT_ID + 1`, for a device whose ids must
    /// be the same every session and that has no snapshot state to restore.
    pub fn sequential() -> Self {
        Self::starting_at(fuse::ROOT_ID + 1)
    }

    fn starting_at(first: u64) -> Self {
        Self {
            next: AtomicU64::new(first),
        }
    }

    /// Allocate the next inode number. Each call returns a unique value.
    pub fn next(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }

    /// Current value of the allocation counter (for checkpoint/snapshot).
    pub fn current(&self) -> u64 {
        self.next.load(Ordering::Relaxed)
    }

    /// Restore the allocation counter from a snapshot. Call before reopening
    /// handles so newly-allocated ids don't collide with restored ones.
    pub fn restore(&self, value: u64) {
        self.next.store(value, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_session_numbers_from_its_own_random_base() {
        let first = InodeAllocator::new();
        let second = InodeAllocator::new();
        let (a, b) = (first.next(), second.next());
        assert_ne!(a, b);
        for id in [a, b] {
            assert!((FIRST_BASE..FIRST_BASE + BASE_SPAN).contains(&id));
        }
        assert_eq!(first.next(), a + 1);
    }

    #[test]
    fn restore_continues_from_the_snapshotted_counter() {
        let source = InodeAllocator::new();
        source.next();
        let clone = InodeAllocator::new();
        clone.restore(source.current());
        assert_eq!(clone.next(), source.next());

        // A snapshot from an engine that numbered from ROOT_ID + 1.
        let old = InodeAllocator::new();
        old.restore(42);
        assert_eq!(old.next(), 42);
    }

    #[test]
    fn a_sequential_allocator_starts_after_the_root() {
        assert_eq!(InodeAllocator::sequential().next(), fuse::ROOT_ID + 1);
    }
}
