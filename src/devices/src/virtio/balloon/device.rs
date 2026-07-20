use std::cmp;
// TryInto is in the 2021 prelude; the explicit import is only needed on Unix.
#[cfg(not(target_os = "windows"))]
use std::convert::TryInto;
use std::io::Write;

use utils::eventfd::EventFd;
use vm_memory::{Address, ByteValued, Bytes, GuestMemory, GuestMemoryMmap};

use super::super::{
    ActivateError, ActivateResult, BalloonError, DeviceQueue, DeviceState, QueueConfig,
    VirtioDevice,
};
use super::{defs, defs::uapi};
use crate::virtio::InterruptTransport;
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::Memory::DiscardVirtualMemory;

// Inflate queue.
pub(crate) const IFQ_INDEX: usize = 0;
// Deflate queue.
pub(crate) const DFQ_INDEX: usize = 1;
// Stats queue.
pub(crate) const STQ_INDEX: usize = 2;
// Page-hinting queue.
pub(crate) const PHQ_INDEX: usize = 3;
// Free page reporting queue.
pub(crate) const FRQ_INDEX: usize = 4;

// Supported features.
pub(crate) const AVAIL_FEATURES: u64 = (1 << uapi::VIRTIO_F_VERSION_1 as u64)
    | (1 << uapi::VIRTIO_BALLOON_F_STATS_VQ as u64)
    | (1 << uapi::VIRTIO_BALLOON_F_FREE_PAGE_HINT as u64)
    | (1 << uapi::VIRTIO_BALLOON_F_REPORTING as u64);

#[derive(Copy, Clone, Debug, Default)]
#[repr(C, packed)]
pub struct VirtioBalloonConfig {
    /* Number of pages host wants Guest to give up. */
    num_pages: u32,
    /* Number of pages we've actually got in balloon. */
    actual: u32,
    /* Free page report command id, readonly by guest */
    free_page_report_cmd_id: u32,
    /* Stores PAGE_POISON if page poisoning is in use */
    poison_val: u32,
}

// Safe because it only has data and has no implicit padding.
unsafe impl ByteValued for VirtioBalloonConfig {}

/// Runtime state captured for checkpoint/fork (see `save_state`).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BalloonState {
    pub acked_features: u64,
    pub num_pages: u32,
    pub actual: u32,
    pub queues: Vec<Option<crate::virtio::queue::QueueState>>,
}

pub struct Balloon {
    pub(crate) queues: Option<Vec<DeviceQueue>>,
    pub(crate) avail_features: u64,
    pub(crate) acked_features: u64,
    pub(crate) activate_evt: EventFd,
    pub(crate) device_state: DeviceState,
    config: VirtioBalloonConfig,
}

impl Balloon {
    pub fn new() -> super::Result<Balloon> {
        Ok(Balloon {
            queues: None,
            avail_features: AVAIL_FEATURES,
            acked_features: 0,
            activate_evt: EventFd::new(utils::eventfd::EFD_NONBLOCK)
                .map_err(BalloonError::EventFd)?,
            device_state: DeviceState::Inactive,
            config: VirtioBalloonConfig::default(),
        })
    }

    pub fn id(&self) -> &str {
        defs::BALLOON_DEV_ID
    }

    /// Capture runtime state for checkpoint/fork. vCPUs must be paused, so
    /// the queues are at a clean boundary.
    pub fn save_state(&self) -> BalloonState {
        BalloonState {
            acked_features: self.acked_features,
            num_pages: self.config.num_pages,
            actual: self.config.actual,
            queues: match self.queues.as_ref() {
                Some(qs) => qs.iter().map(|q| Some(q.queue.save_state())).collect(),
                None => vec![None; defs::NUM_QUEUES],
            },
        }
    }

    /// Restore negotiated features + config onto a freshly-built,
    /// not-yet-activated Balloon; the queues are re-applied when the device is
    /// re-activated (`restore_and_activate`). Without this the restored guest
    /// keeps submitting free-page reports to a device that was never
    /// activated in the clone, so nothing is ever reclaimed.
    pub fn restore_state(&mut self, state: &BalloonState) -> Result<(), String> {
        self.acked_features = state.acked_features;
        self.config.num_pages = state.num_pages;
        self.config.actual = state.actual;
        Ok(())
    }

    /// Host-side target: ask the guest to inflate the balloon to `pages`
    /// 4 KiB pages (0 fully deflates). Takes effect via a config-change
    /// interrupt; the guest driver in/deflates toward the target and updates
    /// `actual` in config space as it goes.
    pub fn set_target_pages(&mut self, pages: u32) {
        self.config.num_pages = pages;
        if let DeviceState::Activated(_, ref interrupt) = self.device_state {
            interrupt.signal_config_change();
        }
    }

    /// (target, actual) balloon size in 4 KiB pages, as last written by the
    /// host and guest respectively.
    pub fn pages(&self) -> (u32, u32) {
        (self.config.num_pages, self.config.actual)
    }

    pub fn process_frq(&mut self) -> bool {
        debug!("balloon: process_frq()");
        let mem = match self.device_state {
            DeviceState::Activated(ref mem, _) => mem,
            // This should never happen, it's been already validated in the event handler.
            DeviceState::Inactive => unreachable!(),
        };

        let queues = self
            .queues
            .as_mut()
            .expect("queues should exist when activated");
        let mut have_used = false;

        while let Some(head) = queues[FRQ_INDEX].queue.pop(mem) {
            let index = head.index;
            for desc in head.into_iter() {
                let host_addr = mem.get_host_address(desc.addr).unwrap();
                debug!(
                    "balloon: should release guest_addr={:?} host_addr={:p} len={}",
                    desc.addr, host_addr, desc.len
                );
                // On macOS, pages mapped into the guest via hv_vm_map are
                // hypervisor-pinned and no madvise releases them; real reclaim
                // unmaps the range from stage-2 first and lazily remaps it on
                // the next guest fault (see hvf::balloon_reclaim_range). Must
                // happen before this report is acked via add_used below.
                #[cfg(target_os = "macos")]
                if hvf::balloon_reclaim_enabled() {
                    hvf::balloon_reclaim_range(desc.addr.0, host_addr as u64, desc.len as u64);
                    continue;
                }
                // On Linux MADV_DONTNEED decommits immediately (KVM MMU
                // notifiers invalidate stage-2). On macOS this fallback is
                // known-inert and kept only for the reclaim-disabled mode.
                #[cfg(target_os = "linux")]
                let advice = libc::MADV_DONTNEED;
                #[cfg(target_os = "macos")]
                let advice = libc::MADV_FREE_REUSABLE;
                #[cfg(unix)]
                unsafe {
                    libc::madvise(
                        host_addr as *mut libc::c_void,
                        desc.len.try_into().unwrap(),
                        advice,
                    )
                };
                #[cfg(target_os = "windows")]
                unsafe {
                    DiscardVirtualMemory(host_addr as *mut core::ffi::c_void, desc.len as usize)
                };
            }

            have_used = true;
            if let Err(e) = queues[FRQ_INDEX].queue.add_used(mem, index, 0) {
                error!("failed to add used elements to the queue: {e:?}");
            }
        }

        have_used
    }

    /// Inflate queue: the guest surrenders pages as arrays of little-endian
    /// u32 PFNs (4 KiB units). Coalesce consecutive PFNs into runs and release
    /// each run exactly like a free-page report — the guest will not touch
    /// these pages until it deflates them, and deflated pages come back
    /// zero-filled (refault path / fresh anonymous pages), which the driver
    /// tolerates because we do not negotiate MUST_TELL_HOST semantics beyond
    /// the queue itself.
    pub fn process_ifq(&mut self) -> bool {
        debug!("balloon: process_ifq()");
        let mem = match self.device_state {
            DeviceState::Activated(ref mem, _) => mem,
            DeviceState::Inactive => unreachable!(),
        };
        let queues = self
            .queues
            .as_mut()
            .expect("queues should exist when activated");
        let mut have_used = false;

        while let Some(head) = queues[IFQ_INDEX].queue.pop(mem) {
            let index = head.index;
            for desc in head.into_iter() {
                let count = desc.len as usize / 4;
                let mut run_start: u64 = 0;
                let mut run_len: u64 = 0;
                for i in 0..count {
                    let pfn: u32 = match mem.read_obj(desc.addr.unchecked_add(i as u64 * 4)) {
                        Ok(v) => v,
                        Err(e) => {
                            error!("balloon: bad inflate pfn buffer: {e:?}");
                            break;
                        }
                    };
                    let gpa = u64::from(pfn) << 12;
                    if run_len > 0 && gpa == run_start + run_len {
                        run_len += 4096;
                    } else {
                        if run_len > 0 {
                            release_guest_range(mem, run_start, run_len);
                        }
                        run_start = gpa;
                        run_len = 4096;
                    }
                }
                if run_len > 0 {
                    release_guest_range(mem, run_start, run_len);
                }
            }
            have_used = true;
            if let Err(e) = queues[IFQ_INDEX].queue.add_used(mem, index, 0) {
                error!("failed to add used elements to the inflate queue: {e:?}");
            }
        }
        have_used
    }

    /// Deflate queue: the guest reclaims pages from the balloon. Nothing to
    /// release — the pages materialize on access (refault remap on macOS,
    /// fresh anonymous pages on Linux) — so just ack the descriptors.
    pub fn process_dfq(&mut self) -> bool {
        debug!("balloon: process_dfq()");
        let mem = match self.device_state {
            DeviceState::Activated(ref mem, _) => mem,
            DeviceState::Inactive => unreachable!(),
        };
        let queues = self
            .queues
            .as_mut()
            .expect("queues should exist when activated");
        let mut have_used = false;

        while let Some(head) = queues[DFQ_INDEX].queue.pop(mem) {
            let index = head.index;
            have_used = true;
            if let Err(e) = queues[DFQ_INDEX].queue.add_used(mem, index, 0) {
                error!("failed to add used elements to the deflate queue: {e:?}");
            }
        }
        have_used
    }
}

/// Release a guest range surrendered by the balloon (free-page report or
/// inflate): unmap+purge on macOS when reclaim is enabled, otherwise the
/// per-OS madvise/discard fallback. Sub-host-page runs are silently skipped
/// by the reclaim path's inward alignment.
fn release_guest_range(mem: &GuestMemoryMmap, gpa: u64, len: u64) {
    use vm_memory::GuestAddress;
    let host_addr = match mem.get_host_address(GuestAddress(gpa)) {
        Ok(p) => p,
        Err(e) => {
            error!("balloon: inflate range outside guest memory: 0x{gpa:x}+{len}: {e:?}");
            return;
        }
    };
    #[cfg(target_os = "macos")]
    if hvf::balloon_reclaim_enabled() {
        hvf::balloon_reclaim_range(gpa, host_addr as u64, len);
        return;
    }
    #[cfg(target_os = "linux")]
    let advice = libc::MADV_DONTNEED;
    #[cfg(target_os = "macos")]
    let advice = libc::MADV_FREE_REUSABLE;
    #[cfg(unix)]
    unsafe {
        libc::madvise(host_addr as *mut libc::c_void, len as usize, advice)
    };
    #[cfg(target_os = "windows")]
    unsafe {
        DiscardVirtualMemory(host_addr as *mut core::ffi::c_void, len as usize)
    };
}

impl VirtioDevice for Balloon {
    fn avail_features(&self) -> u64 {
        self.avail_features
    }

    fn acked_features(&self) -> u64 {
        self.acked_features
    }

    fn set_acked_features(&mut self, acked_features: u64) {
        self.acked_features = acked_features
    }

    fn device_type(&self) -> u32 {
        uapi::VIRTIO_ID_BALLOON
    }

    fn device_name(&self) -> &str {
        "balloon"
    }

    fn queue_config(&self) -> &[QueueConfig] {
        &defs::QUEUE_CONFIG
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
        // The guest driver reports the balloon's current size by writing
        // `actual` (offset 4, 4 bytes LE). Everything else is host-owned.
        if offset == 4 && data.len() == 4 {
            self.config.actual = u32::from_le_bytes(data.try_into().unwrap());
            return;
        }
        warn!(
            "balloon: guest driver attempted to write device config (offset={:x}, len={:x})",
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
        if queues.len() != defs::NUM_QUEUES {
            error!(
                "Cannot perform activate. Expected {} queue(s), got {}",
                defs::NUM_QUEUES,
                queues.len()
            );
            return Err(ActivateError::BadActivate);
        }

        if self.activate_evt.write(1).is_err() {
            error!("Cannot write to activate_evt",);
            return Err(ActivateError::BadActivate);
        }

        self.queues = Some(queues);
        self.device_state = DeviceState::Activated(mem, interrupt);

        Ok(())
    }

    fn is_activated(&self) -> bool {
        self.device_state.is_activated()
    }
}
