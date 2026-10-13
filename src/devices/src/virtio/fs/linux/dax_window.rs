//! Keeps the DAX window coherent with the size of the files mapped into it.
//!
//! The guest maps file chunks into the window with FUSE_SETUPMAPPING, and the
//! window holds a shared mapping of the host file there. A page of that mapping
//! past the file's end has no host page: KVM cannot resolve it, and its async
//! page fault then retries the guest's access forever, so a guest process that
//! touches such a page spins at that instruction. The guest only touches pages
//! below the size it last heard from us, but the host can shrink a file under
//! it at any time.
//!
//! So the part of a mapping past the file's end is never the file: it is
//! anonymous zero pages from setup on, and again whenever an inotify watch
//! sees the file shrink, which also frees a guest already spinning there. When
//! the file grows, the file is mapped there again. That must happen before the
//! guest can learn the new size, so [`DaxWindow::heal`] runs before every FUSE
//! reply: the growth's inotify event is queued by the write itself, so draining
//! the queue there sees it.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::ffi::CString;
use std::fs::File;
use std::io;
use std::mem::size_of;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::ptr::read_unaligned;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, Weak};
use std::thread;

use super::super::fuse;
use super::super::FuseDaxMapSnap;

/// One mapped file: the descriptor its mappings were made from, the inotify
/// watch on it, and how many window mappings use it.
struct MappedFile {
    file: Arc<File>,
    writable: bool,
    wd: Option<i32>,
    users: usize,
}

#[derive(Default)]
struct Coherence {
    files: HashMap<u64, MappedFile>,
    nodes_by_wd: HashMap<i32, u64>,
    /// Window offset of a mapping -> file offset where its zero pages begin.
    holes: BTreeMap<u64, u64>,
    inotify: Option<Arc<OwnedFd>>,
}

pub(crate) struct DaxWindow {
    /// Active window mappings, keyed by window offset. Serialized in the fork
    /// snapshot so a clone can replay them into its window (the window is
    /// anonymous memory and does not survive a cross-process fork).
    pub(crate) maps: RwLock<BTreeMap<u64, FuseDaxMapSnap>>,
    /// Held across every change to the window, so a mapping and its zero
    /// pages never interleave with another change.
    coherence: Mutex<Coherence>,
    has_holes: AtomicBool,
    page_size: u64,
}

/// A change to the window, made under the coherence lock.
pub(crate) struct WindowGuard<'a> {
    window: &'a DaxWindow,
    state: MutexGuard<'a, Coherence>,
}

impl DaxWindow {
    pub(crate) fn new() -> Arc<Self> {
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        Arc::new(DaxWindow {
            maps: RwLock::new(BTreeMap::new()),
            coherence: Mutex::new(Coherence::default()),
            has_holes: AtomicBool::new(false),
            page_size: if page_size > 0 {
                page_size as u64
            } else {
                4096
            },
        })
    }

    pub(crate) fn lock(&self) -> WindowGuard<'_> {
        WindowGuard {
            window: self,
            state: self.coherence.lock().unwrap(),
        }
    }

    /// Map the file back wherever it grew under zero pages, before a FUSE reply
    /// can tell the guest about the new size.
    pub(crate) fn heal(&self, host_shm_base: u64) {
        if !self.has_holes.load(Ordering::Acquire) {
            return;
        }
        // Only a file with a queued event can have grown.
        self.lock().drain_events(host_shm_base);
    }

    fn spawn_watcher(self: &Arc<Self>, inotify: Arc<OwnedFd>, host_shm_base: u64) {
        let window: Weak<DaxWindow> = Arc::downgrade(self);
        let spawned = thread::Builder::new()
            .name("fs-dax-watch".into())
            .spawn(move || loop {
                let mut pfd = libc::pollfd {
                    fd: inotify.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                let ready = unsafe { libc::poll(&mut pfd, 1, -1) };
                if ready > 0 && pfd.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
                    return;
                }
                if ready < 0 {
                    if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    error!("fs dax watch: poll failed: {}", io::Error::last_os_error());
                    return;
                }
                let Some(window) = window.upgrade() else {
                    return;
                };
                window.lock().drain_events(host_shm_base);
            });
        if let Err(e) = spawned {
            error!("fs dax watch: cannot start the watcher: {e}");
        }
    }
}

impl WindowGuard<'_> {
    /// Record a mapping the caller just made of `file` at window offset
    /// `moffset`, watch the file, and replace the part past its end with zero
    /// pages.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn mapped(
        &mut self,
        window: &Arc<DaxWindow>,
        nodeid: u64,
        file: File,
        writable: bool,
        snap: FuseDaxMapSnap,
        host_shm_base: u64,
    ) {
        let end = snap.moffset + snap.len;
        let replaced: Vec<FuseDaxMapSnap> = {
            let mut maps = self.window.maps.write().unwrap();
            let replaced = maps
                .values()
                .filter(|m| m.moffset < end && m.moffset + m.len > snap.moffset)
                .copied()
                .collect();
            maps.retain(|_, m| m.moffset + m.len <= snap.moffset || m.moffset >= end);
            maps.insert(snap.moffset, snap);
            replaced
        };
        for old in replaced {
            self.forget(&old);
        }

        let inotify = match &self.state.inotify {
            Some(fd) => Some(fd.clone()),
            None => match new_inotify() {
                Ok(fd) => {
                    let fd = Arc::new(fd);
                    self.state.inotify = Some(fd.clone());
                    window.spawn_watcher(fd.clone(), host_shm_base);
                    Some(fd)
                }
                Err(e) => {
                    warn!("fs dax: no inotify, a host truncation can stall the guest: {e}");
                    None
                }
            },
        };
        let entry = match self.state.files.entry(nodeid) {
            Entry::Occupied(e) => {
                let e = e.into_mut();
                // Mapping the file back over zero pages must allow what a
                // writable mapping of it allows.
                if writable && !e.writable {
                    e.file = Arc::new(file);
                    e.writable = true;
                }
                e
            }
            Entry::Vacant(v) => v.insert(MappedFile {
                file: Arc::new(file),
                writable,
                wd: None,
                users: 0,
            }),
        };
        entry.users += 1;
        let mut added = None;
        if let (None, Some(inotify)) = (entry.wd, &inotify) {
            match add_watch(inotify.as_raw_fd(), entry.file.as_raw_fd()) {
                Ok(wd) => {
                    entry.wd = Some(wd);
                    added = Some(wd);
                }
                Err(e) => warn!("fs dax: cannot watch nodeid {nodeid}: {e}"),
            }
        }
        let watched = entry.wd.is_some();
        if let Some(wd) = added {
            self.state.nodes_by_wd.insert(wd, nodeid);
        }
        // Without a watch, growth would go unseen under zero pages: leave
        // such a file mapped past its end, as it was before.
        if watched {
            self.reconcile_map(snap.moffset, host_shm_base);
        }
    }

    /// Forget the mappings overlapping `[moffset, end)` the caller just removed.
    pub(crate) fn unmapped(&mut self, moffset: u64, end: u64) {
        let removed: Vec<FuseDaxMapSnap> = {
            let mut maps = self.window.maps.write().unwrap();
            let removed = maps
                .values()
                .filter(|m| m.moffset < end && m.moffset + m.len > moffset)
                .copied()
                .collect();
            maps.retain(|_, m| m.moffset + m.len <= moffset || m.moffset >= end);
            removed
        };
        for old in removed {
            self.forget(&old);
        }
    }

    fn forget(&mut self, map: &FuseDaxMapSnap) {
        self.state.holes.remove(&map.moffset);
        self.window
            .has_holes
            .store(!self.state.holes.is_empty(), Ordering::Release);
        let Some(entry) = self.state.files.get_mut(&map.nodeid) else {
            return;
        };
        entry.users = entry.users.saturating_sub(1);
        if entry.users > 0 {
            return;
        }
        let removed = self.state.files.remove(&map.nodeid);
        if let Some(wd) = removed.and_then(|entry| entry.wd) {
            self.state.nodes_by_wd.remove(&wd);
            if let Some(inotify) = &self.state.inotify {
                unsafe { libc::inotify_rm_watch(inotify.as_raw_fd(), wd) };
            }
        }
    }

    /// Read every queued inotify event and bring the mappings of each changed
    /// file in line with its size.
    fn drain_events(&mut self, host_shm_base: u64) {
        let Some(inotify) = self.state.inotify.clone() else {
            return;
        };
        let mut changed = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = unsafe {
                libc::read(
                    inotify.as_raw_fd(),
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                )
            };
            if n <= 0 {
                break;
            }
            let mut at = 0usize;
            let header = size_of::<libc::inotify_event>();
            while at + header <= n as usize {
                // SAFETY: the kernel wrote a whole event at `at`.
                let event: libc::inotify_event =
                    unsafe { read_unaligned(buf.as_ptr().add(at) as *const _) };
                if event.mask & libc::IN_Q_OVERFLOW != 0 {
                    // Events were dropped: any mapped file may have changed.
                    changed.extend(self.state.files.keys().copied());
                } else if let Some(nodeid) = self.state.nodes_by_wd.get(&event.wd) {
                    changed.push(*nodeid);
                }
                at += header + event.len as usize;
            }
        }
        changed.sort_unstable();
        changed.dedup();
        for nodeid in changed {
            let maps: Vec<u64> = self
                .window
                .maps
                .read()
                .unwrap()
                .values()
                .filter(|m| m.nodeid == nodeid)
                .map(|m| m.moffset)
                .collect();
            for moffset in maps {
                self.reconcile_map(moffset, host_shm_base);
            }
        }
    }

    /// Make the mapping at `moffset` the file below the file's end and zero
    /// pages past it.
    fn reconcile_map(&mut self, moffset: u64, host_shm_base: u64) {
        let Some(map) = self.window.maps.read().unwrap().get(&moffset).copied() else {
            self.state.holes.remove(&moffset);
            return;
        };
        let Some(entry) = self.state.files.get(&map.nodeid) else {
            return;
        };
        let size = match entry.file.metadata() {
            Ok(m) => m.len(),
            Err(e) => {
                warn!("fs dax: cannot stat nodeid {}: {e}", map.nodeid);
                return;
            }
        };
        let page = self.window.page_size;
        let end = map.foffset + map.len;
        let valid = size
            .div_ceil(page)
            .saturating_mul(page)
            .clamp(map.foffset, end);
        let zero_from = self.state.holes.get(&moffset).copied().unwrap_or(end);
        let writable = (map.flags & fuse::SetupmappingFlags::WRITE.bits()) != 0;
        let prot = if writable {
            libc::PROT_READ | libc::PROT_WRITE
        } else {
            libc::PROT_READ
        };
        let at = |foffset: u64| host_shm_base + map.moffset + (foffset - map.foffset);
        let result = if valid < zero_from {
            map_fixed(at(valid), zero_from - valid, prot, None)
        } else if valid > zero_from {
            map_fixed(
                at(zero_from),
                valid - zero_from,
                prot,
                Some((entry.file.as_raw_fd(), zero_from)),
            )
        } else {
            Ok(())
        };
        match result {
            Ok(()) if valid < end => {
                self.state.holes.insert(moffset, valid);
            }
            Ok(()) => {
                self.state.holes.remove(&moffset);
            }
            Err(e) => warn!(
                "fs dax: cannot resize the mapping at {moffset:#x} of nodeid {}: {e}",
                map.nodeid
            ),
        }
        self.window
            .has_holes
            .store(!self.state.holes.is_empty(), Ordering::Release);
    }
}

fn new_inotify() -> io::Result<OwnedFd> {
    let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn add_watch(inotify: RawFd, file: RawFd) -> io::Result<i32> {
    let path = CString::new(format!("/proc/self/fd/{file}")).unwrap();
    // IN_MODIFY covers writes and truncation; IN_ATTRIB a size set by setattr.
    let wd = unsafe {
        libc::inotify_add_watch(inotify, path.as_ptr(), libc::IN_MODIFY | libc::IN_ATTRIB)
    };
    if wd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(wd)
}

/// Replace `[addr, addr + len)` of the window with the file at `foffset`, or
/// with zero pages.
fn map_fixed(addr: u64, len: u64, prot: i32, file: Option<(RawFd, u64)>) -> io::Result<()> {
    let (flags, fd, off) = match file {
        Some((fd, foffset)) => (
            libc::MAP_SHARED | libc::MAP_FIXED,
            fd,
            foffset as libc::off_t,
        ),
        None => (
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED,
            -1,
            0,
        ),
    };
    let ret = unsafe {
        libc::mmap(
            addr as *mut libc::c_void,
            len as usize,
            prot,
            flags,
            fd,
            off,
        )
    };
    if std::ptr::eq(ret, libc::MAP_FAILED) {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::FileExt;

    fn read_byte(addr: u64) -> u8 {
        unsafe { std::ptr::read_volatile(addr as *const u8) }
    }

    #[test]
    fn mapping_past_the_files_end_is_zeros_until_the_file_grows() {
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
        let window_len = 8 * page;
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                window_len as usize,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert!(!std::ptr::eq(base, libc::MAP_FAILED));
        let base = base as u64;

        let path = std::env::temp_dir().join(format!("dax-window-test-{}", std::process::id()));
        let mut file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        file.write_all(&vec![7u8; (2 * page) as usize]).unwrap();
        let _ = std::fs::remove_file(&path);

        // What setupmapping does: map four pages of a two-page file.
        let len = 4 * page;
        map_fixed(base, len, libc::PROT_READ, Some((file.as_raw_fd(), 0))).unwrap();
        let window = DaxWindow::new();
        window.lock().mapped(
            &window,
            1,
            file.try_clone().unwrap(),
            false,
            FuseDaxMapSnap {
                moffset: 0,
                nodeid: 1,
                foffset: 0,
                len,
                flags: 0,
            },
            base,
        );
        assert_eq!(read_byte(base), 7);
        assert_eq!(read_byte(base + 3 * page), 0, "past the end is zero pages");

        // The host truncates the file under the mapping.
        file.set_len(page).unwrap();
        window.heal(base);
        assert_eq!(read_byte(base), 7);
        assert_eq!(read_byte(base + page), 0, "truncated away is zero pages");

        // The file grows again: the mapping shows it before any reply.
        file.write_all_at(&vec![9u8; (3 * page) as usize], page)
            .unwrap();
        window.heal(base);
        assert_eq!(read_byte(base + page), 9);
        assert_eq!(read_byte(base + 3 * page), 9);

        window.lock().unmapped(0, len);
        assert!(!window.has_holes.load(Ordering::Acquire));
        unsafe { libc::munmap(base as *mut libc::c_void, window_len as usize) };
    }
}
