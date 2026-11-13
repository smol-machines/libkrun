//! Process-wide budget for retained, handle-less filesystem descriptors.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

pub(super) struct DescriptorBudget {
    limit: usize,
    used: AtomicUsize,
}

impl DescriptorBudget {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            limit,
            used: AtomicUsize::new(0),
        }
    }

    pub(super) fn process() -> Arc<Self> {
        static BUDGET: OnceLock<Arc<DescriptorBudget>> = OnceLock::new();
        BUDGET
            .get_or_init(|| {
                let mut limit = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                // SAFETY: getrlimit writes only this initialized local value.
                let capacity = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0 {
                    capacity(limit.rlim_cur)
                } else {
                    0
                };
                Arc::new(Self::new(capacity))
            })
            .clone()
    }

    pub(super) fn reserve(self: &Arc<Self>) -> Option<Reservation> {
        let mut used = self.used.load(Ordering::Relaxed);
        while used < self.limit {
            match self.used.compare_exchange_weak(
                used,
                used + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(Reservation(self.clone())),
                Err(current) => used = current,
            }
        }
        None
    }
}

// Keep at least three quarters of the descriptor allowance for explicit
// handles, sockets, disks and other users of this embedding process. This is
// one shared allowance, not a separate allowance for every mounted directory.
fn capacity(soft_limit: libc::rlim_t) -> usize {
    (soft_limit / 4).min(8192) as usize
}

pub(super) struct Reservation(Arc<DescriptorBudget>);

impl Drop for Reservation {
    fn drop(&mut self) {
        self.0.used.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::thread;

    #[test]
    fn leaves_headroom_and_bounds_large_limits() {
        assert_eq!(capacity(0), 0);
        assert_eq!(capacity(256), 64);
        assert_eq!(capacity(10240), 2560);
        assert_eq!(capacity(libc::RLIM_INFINITY), 8192);
    }

    #[test]
    fn reservations_are_shared_and_returned_on_drop() {
        let first_mount = Arc::new(DescriptorBudget::new(2));
        let second_mount = first_mount.clone();
        let first = first_mount.reserve().unwrap();
        let second = second_mount.reserve().unwrap();
        assert!(first_mount.reserve().is_none());
        drop(first);
        let replacement = second_mount.reserve().unwrap();
        assert!(second_mount.reserve().is_none());
        drop((second, replacement));
        assert_eq!(first_mount.used.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn concurrent_mounts_cannot_exceed_the_budget() {
        let budget = Arc::new(DescriptorBudget::new(8));
        let acquired = AtomicUsize::new(0);
        let barrier = Barrier::new(32);
        thread::scope(|scope| {
            for _ in 0..32 {
                scope.spawn(|| {
                    let reservation = budget.reserve();
                    if reservation.is_some() {
                        acquired.fetch_add(1, Ordering::Relaxed);
                    }
                    barrier.wait();
                    drop(reservation);
                });
            }
        });
        assert_eq!(acquired.load(Ordering::Relaxed), 8);
        assert_eq!(budget.used.load(Ordering::Relaxed), 0);
    }
}
