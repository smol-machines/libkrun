//! Epoll-like I/O event polling for Windows, backed by I/O Completion Ports.
//!
//! Uses an IOCP as the central event multiplexer with
//! `NtAssociateWaitCompletionPacket` to bridge waitable kernel handles (like
//! Windows Event objects used by `EventFd`) into the completion port.  This
//! gives true O(1) wake-up with no handle-count limitations.
//!
//! ## How it works
//!
//! 1. [`Epoll::new`] creates an I/O Completion Port.
//! 2. [`Epoll::ctl`] with [`ControlOperation::Add`] heap-allocates a [`Watch`]
//!    struct, creates a *Wait Completion Packet* (WCP) via the NT native API,
//!    and associates the caller's waitable handle with the IOCP.  The raw
//!    `Watch` pointer is used as the completion key.
//! 3. [`Epoll::wait`] calls `GetQueuedCompletionStatusEx` which blocks until
//!    one or more packets arrive (or timeout).  The returned completion key is
//!    cast back to a `Watch` pointer, and event metadata is read through
//!    atomics -- **no locks are acquired on this path**.
//! 4. After delivering each event, `wait` **re-associates** (if level-triggered)
//!    the WCP so the next signal produces another packet.
//! 5. [`Epoll::ctl`] with [`ControlOperation::Delete`] marks the watch as
//!    inactive, closes the WCP handle, and moves the `Watch` allocation to a
//!    zombie list.  A time-based GC sweep in the same method frees zombies
//!    older than 5 seconds, ensuring any in-flight packets referencing the
//!    `Watch` memory have been drained.

use log::debug;
use std::collections::HashMap;
use std::io;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bitflags::bitflags;

use super::bindings::*;
use super::{AsRawFd, RawFd};

use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT as WAIT_TIMEOUT_CODE,
};
use windows_sys::Win32::Networking::WinSock::{
    FD_ACCEPT, FD_CLOSE, FD_CONNECT, FD_READ, FD_WRITE, SOCKET, WSACloseEvent, WSACreateEvent,
    WSAEVENT, WSAEnumNetworkEvents, WSAEventSelect, WSANETWORKEVENTS,
};
use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_PIPE, GetFileType};
use windows_sys::Win32::System::IO::{
    CreateIoCompletionPort, GetQueuedCompletionStatusEx, OVERLAPPED_ENTRY,
};
use windows_sys::Win32::System::Threading::INFINITE;

/// Heuristic: a Winsock SOCKET reports as `FILE_TYPE_PIPE`, whereas the Windows
/// Event objects backing `EventFd` report `FILE_TYPE_UNKNOWN`. We only ever
/// register those two kinds of handle, so this cleanly distinguishes them.
fn is_socket(h: HANDLE) -> bool {
    unsafe { GetFileType(h) == FILE_TYPE_PIPE }
}

/// Translate an [`EventSet`] interest mask into the Winsock `WSAEventSelect`
/// network-event mask. `FD_CLOSE` is always selected so peer closes surface.
fn wsa_mask(events: EventSet) -> i32 {
    let mut mask = FD_CLOSE;
    if events.contains(EventSet::IN) {
        mask |= FD_READ | FD_ACCEPT;
    }
    if events.contains(EventSet::OUT) {
        mask |= FD_WRITE | FD_CONNECT;
    }
    mask as i32
}

/// Translate the network events reported by `WSAEnumNetworkEvents` into an
/// [`EventSet`]. `FD_CLOSE` maps to `IN | HANG_UP` so the reader drains any
/// buffered data before the connection is torn down.
fn wsa_events_to_set(network_events: i32) -> EventSet {
    let ne = network_events as u32;
    let mut set = EventSet::empty();
    if ne & (FD_READ | FD_ACCEPT) != 0 {
        set |= EventSet::IN;
    }
    if ne & (FD_WRITE | FD_CONNECT) != 0 {
        set |= EventSet::OUT;
    }
    if ne & FD_CLOSE != 0 {
        set |= EventSet::IN | EventSet::HANG_UP;
    }
    set
}

// Generic access mask requesting all permissions the caller is allowed.
// https://learn.microsoft.com/en-us/windows/win32/secauthz/access-mask
const MAXIMUM_ALLOWED: u32 = 0x0200_0000;
const GC_THRESHOLD: Duration = Duration::from_secs(5);

#[repr(i32)]
pub enum ControlOperation {
    Add,
    Modify,
    Delete,
}

bitflags! {
    /// Bitmask of I/O readiness event types.
    ///
    /// Bit values are intentionally identical to the macOS implementation so
    /// that device code using these constants is portable across all three
    /// supported platforms.
    pub struct EventSet: u32 {
        /// The handle is ready for reading.
        const IN = 0b00000001;
        /// The handle is ready for writing.
        const OUT = 0b00000010;
        /// Hang-up (peer closed its end).
        const HANG_UP = 0b00000100;
        /// Read hang-up (peer shut down its write side).
        const READ_HANG_UP = 0b00001000;
        /// Request edge-triggered notification. The WCP is not re-armed
        /// after delivery; use [`ControlOperation::Modify`] to re-register.
        const EDGE_TRIGGERED = 0b00010000;
    }
}

/// Carrier for a readiness event, mirroring `libc::epoll_event` on Linux.
#[derive(Clone, Copy, Default)]
pub struct EpollEvent {
    pub events: u32,
    u64: u64,
}

impl std::fmt::Debug for EpollEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{{ events: {}, data: {} }}", self.events(), self.data())
    }
}

impl EpollEvent {
    /// Create a new event with the given readiness mask and user data.
    pub fn new(events: EventSet, data: u64) -> Self {
        EpollEvent {
            events: events.bits(),
            u64: data,
        }
    }

    pub fn events(&self) -> u32 {
        self.events
    }

    pub fn event_set(&self) -> EventSet {
        // This unwrap is safe because `epoll_events` can only be user created or
        // initialized by the kernel. We trust the kernel to only send us valid
        // events. The user can only initialize `epoll_events` using valid events.
        EventSet::from_bits(self.events()).unwrap()
    }

    pub fn data(&self) -> u64 {
        debug!("EpollEvent data: {}", self.u64);
        self.u64
    }

    pub fn fd(&self) -> RawFd {
        self.u64 as RawFd
    }
}

/// A watched handle and its associated metadata.
///
/// Heap-allocated so its address can serve as the IOCP completion key.  
/// This lets [`Epoll::wait`] access `Watch` fields through raw
/// pointers and atomics with no locking.
struct Watch {
    /// User-facing key passed to [`Epoll::ctl`] (an EventFd handle or a SOCKET).
    /// Retained for diagnostics/identity; readiness uses `target`/`socket`.
    #[allow(dead_code)]
    fd: HANDLE,
    /// Waitable handle associated with the WCP. For an EventFd this is `fd`
    /// itself; for a socket it is the `WSAEventSelect` event (`wsa_event`).
    target: HANDLE,
    /// The SOCKET, when this watch tracks a Winsock socket; else null.
    socket: HANDLE,
    /// The `WSACreateEvent` handle for a socket watch; else null. Equals
    /// `target` when set; tracked separately for cleanup.
    wsa_event: HANDLE,
    wcp: HANDLE,
    events: AtomicU32,
    data: AtomicU64,
    /// Cleared by [`ControlOperation::Delete`] so that in-flight completion
    /// packets already queued by the kernel are silently ignored.
    is_active: AtomicBool,
}

/// The I/O Completion Port and the set of handles it is watching.
struct CompletionPort {
    handle: HANDLE,
    watches: Mutex<HashMap<HANDLE, *mut Watch>>,
    /// When you call ctl(Delete), we tell the kernel to cancel the WCP.
    /// However, there is a possible race condition where
    /// the kernel already signaled the handle and put the completion packet in the IOCP queue,
    /// but the vCPU thread hasn't popped it out.
    /// If we drop Watch immediately during ctl(Delete),
    /// the VCPU will have a pointer to a freed resource resulting in a segfault.
    /// Instead, we use this vector to maintain a list of Watch pointers and when
    /// they were added so we can drop the Watch safely.
    zombies: Mutex<Vec<(Instant, *mut Watch)>>,
}

// All raw `*mut Watch` pointers are either behind a `Mutex` (in
// `watches` / `zombies`) or read-only in the lock-free `wait` path where
// the pointed-to fields are atomics.  Windows `HANDLE` values are valid
// across threads.
unsafe impl Send for CompletionPort {}
unsafe impl Sync for CompletionPort {}

impl Drop for CompletionPort {
    fn drop(&mut self) {
        for (_, ptr) in self.watches.get_mut().unwrap().drain() {
            unsafe {
                let w = Box::from_raw(ptr);
                let _ = NtCancelWaitCompletionPacket(w.wcp, 1);
                CloseHandle(w.wcp);
                if !w.socket.is_null() {
                    WSAEventSelect(w.socket as SOCKET, 0, 0);
                    WSACloseEvent(w.wsa_event as WSAEVENT);
                }
            }
        }
        for (_, ptr) in self.zombies.get_mut().unwrap().drain(..) {
            unsafe {
                let _ = Box::from_raw(ptr);
            }
        }
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

/// Associate a Wait Completion Packet with the given IOCP and target handle.
///
/// When `fd` becomes signaled, the kernel pushes a completion packet to
/// `iocp` carrying `key` as the completion key (a raw pointer to the
/// corresponding [`Watch`]).
fn associate_wcp(
    wcp: HANDLE,
    iocp: HANDLE,
    fd: HANDLE,
    key: *mut std::ffi::c_void,
) -> io::Result<()> {
    let mut already_signaled: u8 = 0;
    let status = unsafe {
        NtAssociateWaitCompletionPacket(
            wcp,
            iocp,
            fd,
            key,
            ptr::null_mut(),
            0,
            0,
            &mut already_signaled,
        )
    };
    if !nt_success(status) {
        return Err(nt_status_err(status));
    }

    // `NtAssociateWaitCompletionPacket` re-arms the wait even when the target
    // is already signaled at association time: it sets `already_signaled` for
    // information and still queues a completion packet for the pending signal.
    // We therefore must NOT post a packet ourselves — doing so double-delivers
    // every event, and because `wait` re-associates before the handler drains
    // the eventfd (so the event is still signaled), it would post on *every*
    // re-arm, flooding the IOCP with spurious wakeups (a busy-loop of
    // `WouldBlock` reads that starves the host under load).
    let _ = already_signaled;
    Ok(())
}

/// Epoll-compatible polling abstraction backed by an I/O Completion Port.
pub struct Epoll {
    iocp: Arc<CompletionPort>,
    entries: Vec<OVERLAPPED_ENTRY>,
}

impl Clone for Epoll {
    fn clone(&self) -> Self {
        Epoll {
            iocp: self.iocp.clone(),
            entries: Vec::new(),
        }
    }
}

// The `entries` scratch buffer holds `OVERLAPPED_ENTRY` values (with raw
// `*mut OVERLAPPED` pointers) only transiently inside `wait`; it is cleared
// between calls and never shared. The `iocp` is `Send + Sync`. Moving an
// `Epoll` to another thread (e.g. the vsock muxer thread) is therefore sound.
unsafe impl Send for Epoll {}

impl Epoll {
    /// Create a new polling instance backed by a fresh I/O Completion Port.
    pub fn new() -> io::Result<Self> {
        let handle =
            unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 0) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Epoll {
            iocp: Arc::new(CompletionPort {
                handle,
                watches: Mutex::new(HashMap::new()),
                zombies: Mutex::new(Vec::new()),
            }),
            entries: Vec::with_capacity(32),
        })
    }

    /// Add, modify, or remove a handle in the interest set.
    ///
    /// * `fd` – the waitable handle (as [`RawFd`] / `HANDLE`).
    /// * `event` – carries the desired [`EventSet`] mask and a `u64` data
    ///   payload that will be returned by [`wait`](Self::wait) when this
    ///   handle becomes ready.
    pub fn ctl(
        &self,
        operation: ControlOperation,
        fd: RawFd,
        event: &EpollEvent,
    ) -> io::Result<()> {
        let mut watches = self.iocp.watches.lock().unwrap();
        match operation {
            ControlOperation::Add => {
                if watches.contains_key(&fd) {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "handle already registered",
                    ));
                }

                // For a Winsock socket, bridge an auto-signaling event into the
                // WCP path: WSAEventSelect signals `wsa_event` on the requested
                // network events, and that event is what the WCP waits on.
                let (target, socket, wsa_event) = if is_socket(fd) {
                    // WSACreateEvent returns a `WSAEVENT` (isize); 0 is the
                    // failure sentinel. It is a real waitable Event handle, so we
                    // store it as a HANDLE for the WCP association.
                    let ev = unsafe { WSACreateEvent() };
                    if ev == 0 {
                        return Err(io::Error::last_os_error());
                    }
                    let interest = EventSet::from_bits_truncate(event.events());
                    if unsafe { WSAEventSelect(fd as SOCKET, ev, wsa_mask(interest)) } != 0 {
                        unsafe {
                            WSACloseEvent(ev);
                        }
                        return Err(io::Error::last_os_error());
                    }
                    (ev as HANDLE, fd, ev as HANDLE)
                } else {
                    (fd, ptr::null_mut(), ptr::null_mut())
                };

                let mut wcp: HANDLE = ptr::null_mut();
                let status =
                    unsafe { NtCreateWaitCompletionPacket(&mut wcp, MAXIMUM_ALLOWED, ptr::null()) };
                if !nt_success(status) {
                    if !wsa_event.is_null() {
                        unsafe {
                            WSACloseEvent(wsa_event as WSAEVENT);
                        }
                    }
                    return Err(nt_status_err(status));
                }

                let watch_ptr = Box::into_raw(Box::new(Watch {
                    fd,
                    target,
                    socket,
                    wsa_event,
                    wcp,
                    events: AtomicU32::new(event.events()),
                    data: AtomicU64::new(event.data()),
                    is_active: AtomicBool::new(true),
                }));

                if let Err(e) = associate_wcp(wcp, self.iocp.handle, target, watch_ptr as *mut _) {
                    unsafe {
                        CloseHandle(wcp);
                        if !wsa_event.is_null() {
                            WSACloseEvent(wsa_event as WSAEVENT);
                        }
                        let _ = Box::from_raw(watch_ptr);
                    }
                    return Err(e);
                }

                watches.insert(fd, watch_ptr);
            }
            ControlOperation::Modify => {
                let &watch_ptr = watches.get(&fd).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "handle not registered")
                })?;

                let watch = unsafe { &*watch_ptr };
                watch.events.store(event.events(), Ordering::Release);
                watch.data.store(event.data(), Ordering::Release);

                // Re-arm the socket's network-event selection with the new mask.
                if !watch.socket.is_null() {
                    let interest = EventSet::from_bits_truncate(event.events());
                    unsafe {
                        WSAEventSelect(
                            watch.socket as SOCKET,
                            watch.wsa_event as WSAEVENT,
                            wsa_mask(interest),
                        );
                    }
                }

                unsafe {
                    let _ = NtCancelWaitCompletionPacket(watch.wcp, 1);
                }
                associate_wcp(
                    watch.wcp,
                    self.iocp.handle,
                    watch.target,
                    watch_ptr as *mut _,
                )?;
            }
            ControlOperation::Delete => {
                let watch_ptr = watches.remove(&fd).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "handle not registered")
                })?;

                let watch = unsafe { &*watch_ptr };
                watch.is_active.store(false, Ordering::Release);
                unsafe {
                    let _ = NtCancelWaitCompletionPacket(watch.wcp, 1);
                    CloseHandle(watch.wcp);
                    if !watch.socket.is_null() {
                        // Cancel network-event selection and free the WSA event.
                        WSAEventSelect(watch.socket as SOCKET, 0, 0);
                        WSACloseEvent(watch.wsa_event as WSAEVENT);
                    }
                }

                // Add the Watch to the zombies list with the current time
                // so we can drop it safely after some delay.
                let mut zombies = self.iocp.zombies.lock().unwrap();
                zombies.push((Instant::now(), watch_ptr));

                zombies.retain(|(deleted_at, ptr)| {
                    if deleted_at.elapsed() > GC_THRESHOLD {
                        // Free the Watch from the heap
                        unsafe {
                            let _ = Box::from_raw(*ptr);
                        }
                        false // Remove from the vector
                    } else {
                        true // Keep in the vector
                    }
                });
            }
        }
        Ok(())
    }

    /// Block until at least one registered handle is signaled, or until
    /// `timeout` milliseconds have elapsed.  Pass `-1` to wait indefinitely.
    ///
    /// This is the lock-free hot path: no `Mutex` is acquired and no heap
    /// allocation is performed.  The completion key returned by the kernel
    /// is the raw `Watch` pointer set during [`ctl`](Self::ctl), so we can
    /// read event metadata through atomics without any table lookup.
    pub fn wait(
        &mut self,
        max_events: usize,
        timeout: i32,
        events: &mut [EpollEvent],
    ) -> io::Result<usize> {
        let iocp_handle = self.iocp.handle;

        let capacity = events.len().min(max_events).min(i32::MAX as usize);
        if capacity == 0 {
            return Ok(0);
        }

        self.entries.clear();
        if self.entries.capacity() < capacity {
            self.entries.reserve_exact(capacity);
        }

        let mut count: u32 = 0;
        let win_timeout: u32 = if timeout < 0 {
            INFINITE
        } else {
            timeout as u32
        };

        let ok = unsafe {
            GetQueuedCompletionStatusEx(
                iocp_handle,
                self.entries.spare_capacity_mut().as_mut_ptr() as *mut _,
                capacity as u32,
                &mut count,
                win_timeout,
                0,
            )
        };

        if ok == 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(WAIT_TIMEOUT_CODE as i32) {
                return Ok(0);
            }
            return Err(err);
        }

        // Update vector length based on how many packets the kernel actually wrote.
        unsafe { self.entries.set_len(count as usize) }
        let mut result_count = 0;

        for entry in &self.entries {
            let watch_ptr = entry.lpCompletionKey as *const Watch;
            if watch_ptr.is_null() {
                continue;
            }

            let watch = unsafe { &*watch_ptr };

            if !watch.is_active.load(Ordering::Acquire) {
                continue;
            }

            let current_events = watch.events.load(Ordering::Acquire);
            let event_set = EventSet::from_bits_truncate(current_events);

            // For a socket, the actual readiness comes from the network-event
            // record (which also resets `wsa_event` so the next event re-signals);
            // for an EventFd we report the registered interest mask as before.
            let ready = if !watch.socket.is_null() {
                let mut nev: WSANETWORKEVENTS = unsafe { std::mem::zeroed() };
                let rc = unsafe {
                    WSAEnumNetworkEvents(
                        watch.socket as SOCKET,
                        watch.wsa_event as WSAEVENT,
                        &mut nev,
                    )
                };
                if rc != 0 {
                    EventSet::empty()
                } else {
                    wsa_events_to_set(nev.lNetworkEvents)
                }
            } else {
                event_set & (EventSet::IN | EventSet::OUT)
            };

            events[result_count] = EpollEvent {
                events: ready.bits(),
                u64: watch.data.load(Ordering::Acquire),
            };
            result_count += 1;

            if !event_set.contains(EventSet::EDGE_TRIGGERED) {
                // Level-triggered: re-associate the WCP so the next signal
                // on this handle produces another completion packet.
                //
                // KNOWN RACE: there is a race between this call and
                // `CloseHandle(watch.wcp)` in `Epoll::ctl(Delete)`.  Another
                // thread may delete the watch (marking it inactive and closing
                // the WCP handle) between our `is_active` check above and the
                // `associate_wcp` call here.  In that case:
                //
                //  1. The WCP handle is already closed and
                //     `NtAssociateWaitCompletionPacket` returns
                //     `STATUS_INVALID_HANDLE` -- harmless, we ignore the error.
                //  2. Windows recycles the handle value for a non-WCP object --
                //     `NtAssociateWaitCompletionPacket` returns
                //     `STATUS_OBJECT_TYPE_MISMATCH` -- also harmless.
                //  3. Windows recycles the handle value for a *new* WCP created
                //     by a third thread.  The associate succeeds on the wrong
                //     WCP.  When that third thread later tries to associate its
                //     own WCP it will fail and delete its handle, leaving the
                //     kernel to drop the WCP once its refcount reaches zero
                //     (the original delete already closed the userspace handle
                //     and the event was never queued to the IOCP).  The only
                //     consequence is a lost event for the third thread, which
                //     should be re-queued on the next iteration.
                //
                // We should try to remove the GC mechanism but for now
                // this is acceptable.
                let _ = associate_wcp(watch.wcp, iocp_handle, watch.target, watch_ptr as *mut _);
            }
        }

        Ok(result_count)
    }
}

impl AsRawFd for Epoll {
    fn as_raw_fd(&self) -> RawFd {
        self.iocp.handle
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent, SetEvent};

    /// Create a manual-reset, initially non-signaled Windows Event object.
    fn create_event() -> HANDLE {
        let h = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
        assert!(h != std::ptr::null_mut(), "CreateEventW failed");
        h
    }

    fn signal(handle: HANDLE) {
        assert_ne!(unsafe { SetEvent(handle) }, 0, "SetEvent failed");
    }

    fn reset(handle: HANDLE) {
        assert_ne!(unsafe { ResetEvent(handle) }, 0, "ResetEvent failed");
    }

    fn close(handle: HANDLE) {
        unsafe {
            CloseHandle(handle);
        }
    }

    #[test]
    fn test_event_ops() {
        let mut event = EpollEvent::default();
        assert_eq!(event.events(), 0);
        assert_eq!(event.data(), 0);

        event = EpollEvent::new(EventSet::IN, 42);
        assert_eq!(event.events(), EventSet::IN.bits());
        assert_eq!(event.event_set(), EventSet::IN);
        assert_eq!(event.data(), 42);
        assert_eq!(event.fd(), 42 as RawFd);
    }

    #[test]
    fn test_events_debug() {
        let event = EpollEvent::new(EventSet::IN, 42);
        assert_eq!(format!("{:?}", event), "{ events: 1, data: 42 }");
    }

    #[test]
    fn test_ctl_add_modify_delete() {
        let epoll = Epoll::new().unwrap();
        let ev = create_event();
        let event = EpollEvent::new(EventSet::IN, ev as u64);

        epoll.ctl(ControlOperation::Add, ev, &event).unwrap();
        assert!(epoll.ctl(ControlOperation::Add, ev, &event).is_err());

        let event2 = EpollEvent::new(EventSet::OUT, ev as u64);
        epoll.ctl(ControlOperation::Modify, ev, &event2).unwrap();

        epoll
            .ctl(ControlOperation::Delete, ev, &EpollEvent::default())
            .unwrap();
        assert!(
            epoll
                .ctl(ControlOperation::Delete, ev, &EpollEvent::default())
                .is_err()
        );
        assert!(
            epoll
                .ctl(ControlOperation::Modify, ev, &EpollEvent::default())
                .is_err()
        );

        close(ev);
    }

    #[test]
    fn test_clone_shares_state() {
        let epoll = Epoll::new().unwrap();
        let epoll2 = epoll.clone();
        let ev = create_event();
        let event = EpollEvent::new(EventSet::IN, ev as u64);

        epoll.ctl(ControlOperation::Add, ev, &event).unwrap();
        assert!(epoll2.ctl(ControlOperation::Add, ev, &event).is_err());

        epoll2
            .ctl(ControlOperation::Delete, ev, &EpollEvent::default())
            .unwrap();
        assert!(
            epoll
                .ctl(ControlOperation::Delete, ev, &EpollEvent::default())
                .is_err()
        );

        close(ev);
    }

    #[test]
    fn test_wait_returns_signaled_event() {
        let mut epoll = Epoll::new().unwrap();
        let ev = create_event();
        let event = EpollEvent::new(EventSet::IN, ev as u64);
        epoll.ctl(ControlOperation::Add, ev, &event).unwrap();

        signal(ev);

        let mut ready = vec![EpollEvent::default(); 8];
        let n = epoll.wait(8, 1000, &mut ready).unwrap();
        assert_eq!(n, 1);
        assert_eq!(ready[0].fd(), ev);
        assert_eq!(ready[0].event_set(), EventSet::IN);

        epoll
            .ctl(ControlOperation::Delete, ev, &EpollEvent::default())
            .unwrap();
        close(ev);
    }

    #[test]
    fn test_wait_timeout_no_signal() {
        let mut epoll = Epoll::new().unwrap();
        let ev = create_event();
        let event = EpollEvent::new(EventSet::IN, ev as u64);
        epoll.ctl(ControlOperation::Add, ev, &event).unwrap();

        let mut ready = vec![EpollEvent::default(); 8];
        let n = epoll.wait(8, 50, &mut ready).unwrap();
        assert_eq!(n, 0);

        epoll
            .ctl(ControlOperation::Delete, ev, &EpollEvent::default())
            .unwrap();
        close(ev);
    }

    #[test]
    fn test_wait_multiple_handles() {
        let mut epoll = Epoll::new().unwrap();
        let ev1 = create_event();
        let ev2 = create_event();

        epoll
            .ctl(
                ControlOperation::Add,
                ev1,
                &EpollEvent::new(EventSet::IN, ev1 as u64),
            )
            .unwrap();
        epoll
            .ctl(
                ControlOperation::Add,
                ev2,
                &EpollEvent::new(EventSet::IN, ev2 as u64),
            )
            .unwrap();

        signal(ev1);
        signal(ev2);

        let mut ready = vec![EpollEvent::default(); 8];
        let n = epoll.wait(8, 1000, &mut ready).unwrap();
        assert_eq!(n, 2);

        let handles: Vec<HANDLE> = ready[..n].iter().map(|e| e.fd()).collect();
        assert!(handles.contains(&ev1));
        assert!(handles.contains(&ev2));

        epoll
            .ctl(ControlOperation::Delete, ev1, &EpollEvent::default())
            .unwrap();
        epoll
            .ctl(ControlOperation::Delete, ev2, &EpollEvent::default())
            .unwrap();
        close(ev1);
        close(ev2);
    }

    #[test]
    fn test_level_triggered_redelivers() {
        let mut epoll = Epoll::new().unwrap();
        let ev = create_event();
        let event = EpollEvent::new(EventSet::IN, ev as u64);
        epoll.ctl(ControlOperation::Add, ev, &event).unwrap();

        signal(ev);

        let mut ready = vec![EpollEvent::default(); 8];
        let n = epoll.wait(8, 1000, &mut ready).unwrap();
        assert_eq!(n, 1);

        // Handle is still signaled (manual-reset event), so a second wait
        // should deliver it again (level-triggered semantics).
        let n = epoll.wait(8, 1000, &mut ready).unwrap();
        assert_eq!(n, 1);
        assert_eq!(ready[0].fd(), ev);

        epoll
            .ctl(ControlOperation::Delete, ev, &EpollEvent::default())
            .unwrap();
        close(ev);
    }

    // -- Edge-triggered tests -----------------------------------------------

    #[test]
    fn test_edge_triggered_no_redelivery() {
        let mut epoll = Epoll::new().unwrap();
        let ev = create_event();
        let event = EpollEvent::new(EventSet::IN | EventSet::EDGE_TRIGGERED, ev as u64);
        epoll.ctl(ControlOperation::Add, ev, &event).unwrap();

        signal(ev);

        let mut ready = vec![EpollEvent::default(); 8];
        let n = epoll.wait(8, 1000, &mut ready).unwrap();
        assert_eq!(n, 1);
        assert_eq!(ready[0].fd(), ev);

        // Even though the handle is still signaled, edge-triggered mode
        // should NOT re-deliver because the WCP was not re-armed.
        let n = epoll.wait(8, 100, &mut ready).unwrap();
        assert_eq!(n, 0);

        epoll
            .ctl(ControlOperation::Delete, ev, &EpollEvent::default())
            .unwrap();
        close(ev);
    }

    #[test]
    fn test_edge_triggered_rearm_via_modify() {
        let mut epoll = Epoll::new().unwrap();
        let ev = create_event();
        let event = EpollEvent::new(EventSet::IN | EventSet::EDGE_TRIGGERED, ev as u64);
        epoll.ctl(ControlOperation::Add, ev, &event).unwrap();

        signal(ev);

        let mut ready = vec![EpollEvent::default(); 8];
        let n = epoll.wait(8, 1000, &mut ready).unwrap();
        assert_eq!(n, 1);

        // Simulate the caller draining data, then re-registering interest.
        reset(ev);
        epoll.ctl(ControlOperation::Modify, ev, &event).unwrap();

        // No signal yet — should time out.
        let n = epoll.wait(8, 100, &mut ready).unwrap();
        assert_eq!(n, 0);

        // Signal again — now we should get the event.
        signal(ev);
        let n = epoll.wait(8, 1000, &mut ready).unwrap();
        assert_eq!(n, 1);
        assert_eq!(ready[0].fd(), ev);

        epoll
            .ctl(ControlOperation::Delete, ev, &EpollEvent::default())
            .unwrap();
        close(ev);
    }
}
