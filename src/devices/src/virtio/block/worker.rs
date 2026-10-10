use crate::virtio::descriptor_utils::{Reader, Writer};
#[cfg(target_os = "windows")]
use std::fs::File;

use super::super::DeviceQueue;
use super::BlockIoEngine;
use super::device::{CacheType, DiskProperties};

use crate::virtio::InterruptTransport;
#[cfg(target_os = "linux")]
use io_uring::{IoUring, opcode, register::Restriction, squeue, types};
#[cfg(target_os = "linux")]
use std::collections::HashMap;
use std::io::{self, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::result;
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(target_os = "linux")]
use std::sync::{LazyLock, Mutex};
use std::thread;
use utils::epoll::{ControlOperation, Epoll, EpollEvent, EventSet};
#[cfg(target_os = "linux")]
use utils::eventfd::EFD_NONBLOCK;
use utils::eventfd::EventFd;
#[cfg(target_os = "windows")]
use utils::windows::AsRawFd;
use virtio_bindings::virtio_blk::*;
#[cfg(target_os = "linux")]
use vm_memory::VolatileSlice;
use vm_memory::{ByteValued, GuestMemoryMmap};

#[allow(dead_code)]
#[derive(Debug)]
pub enum RequestError {
    Discarding(io::Error),
    DiscardingToZero(io::Error),
    FlushingToDisk(io::Error),
    InvalidDataLength,
    ReadingFromDescriptor(io::Error),
    WritingToDescriptor(io::Error),
    WritingZeroes(io::Error),
    UnknownRequest,
}

/// The request header represents the mandatory fields of each block device request.
///
/// A request header contains the following fields:
///   * request_type: an u32 value mapping to a read, write or flush operation.
///   * reserved: 32 bits are reserved for future extensions of the Virtio Spec.
///   * sector: an u64 value representing the offset where a read/write is to occur.
///
/// The header simplifies reading the request from memory as all request follow
/// the same memory layout.
#[derive(Copy, Clone, Default)]
#[repr(C)]
pub struct RequestHeader {
    request_type: u32,
    _reserved: u32,
    sector: u64,
}
// Safe because RequestHeader only contains plain data.
unsafe impl ByteValued for RequestHeader {}

#[derive(Copy, Clone, Default)]
#[repr(C)]
pub struct DiscardWriteData {
    sector: u64,
    num_sectors: u32,
    flags: u32,
}
// Safe because DiscardWriteData only contains plain data.
unsafe impl ByteValued for DiscardWriteData {}

#[cfg(target_os = "linux")]
const MAX_ASYNC_REQUEST_BYTES: usize = 16 * 1024 * 1024;
#[cfg(target_os = "linux")]
const MAX_ASYNC_IN_FLIGHT_BYTES: usize = 64 * 1024 * 1024;
#[cfg(target_os = "linux")]
const MAX_ASYNC_IN_FLIGHT_REQUESTS: usize = 128;

/// Opaque handle for an io_uring created while the launcher can still perform
/// setup syscalls. The ring itself stays in this crate and can be consumed
/// exactly once when the block device is constructed after seccomp is active.
#[derive(Clone)]
pub struct PreparedAsyncIo(Arc<PreparedAsyncIoInner>);

struct PreparedAsyncIoInner(u64);

impl std::fmt::Debug for PreparedAsyncIo {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("PreparedAsyncIo")
            .field(&self.0.0)
            .finish()
    }
}

impl PartialEq for PreparedAsyncIo {
    fn eq(&self, other: &Self) -> bool {
        self.0.0 == other.0.0
    }
}

impl Eq for PreparedAsyncIo {}

impl Drop for PreparedAsyncIoInner {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        {
            PREPARED_ASYNC_IO.lock().unwrap().remove(&self.0);
        }
    }
}

#[cfg(target_os = "linux")]
static NEXT_PREPARED_ASYNC_IO: AtomicU64 = AtomicU64::new(1);
#[cfg(target_os = "linux")]
static PREPARED_ASYNC_IO: LazyLock<Mutex<HashMap<u64, AsyncIo>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn prepare_async_io(path: &str, writable: bool) -> io::Result<PreparedAsyncIo> {
    #[cfg(target_os = "linux")]
    {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(writable)
            .open(path)?;
        let ring = AsyncIo::new(&file)?;
        let token = NEXT_PREPARED_ASYNC_IO.fetch_add(1, Ordering::Relaxed);
        if token == 0 {
            return Err(io::Error::other(
                "async block I/O token namespace exhausted",
            ));
        }
        PREPARED_ASYNC_IO.lock().unwrap().insert(token, ring);
        Ok(PreparedAsyncIo(Arc::new(PreparedAsyncIoInner(token))))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (path, writable);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "async block I/O requires Linux",
        ))
    }
}

pub fn discard_prepared_async_io(prepared: PreparedAsyncIo) {
    drop(prepared);
}

pub(crate) fn take_prepared_async_io(prepared: PreparedAsyncIo) -> Option<AsyncIo> {
    #[cfg(target_os = "linux")]
    {
        return PREPARED_ASYNC_IO.lock().unwrap().remove(&prepared.0.0);
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = prepared;
        None
    }
}

#[cfg(target_os = "linux")]
struct AsyncJob {
    head_index: u16,
    destination: AsyncReadDestination,
    offset: u64,
    bytes: usize,
}

#[cfg(target_os = "linux")]
enum AsyncReadDestination {
    Guest {
        mem: GuestMemoryMmap,
        iovecs: Box<[libc::iovec]>,
    },
    #[cfg(test)]
    Owned(Vec<u8>),
}

#[derive(Debug)]
struct AsyncCompletion {
    head_index: u16,
    bytes: usize,
    result: io::Result<usize>,
    #[cfg(test)]
    owned_data: Option<Vec<u8>>,
}

#[cfg(target_os = "linux")]
enum PendingBuffer {
    Guest {
        _mem: GuestMemoryMmap,
        _iovecs: Box<[libc::iovec]>,
    },
    #[cfg(test)]
    Owned {
        _data: Vec<u8>,
        _iovecs: Box<[libc::iovec]>,
    },
}

// SAFETY: The raw pointers in the retained iovec array refer either to the
// GuestMemoryMmap stored in the same variant or to its owned Vec.  AsyncIo is
// moved into one block worker before submission and never accessed from a
// second userspace thread; the kernel stops using every pointer before the
// corresponding PendingBuffer is removed.
#[cfg(target_os = "linux")]
unsafe impl Send for PendingBuffer {}

#[cfg(target_os = "linux")]
struct PendingRequest {
    head_index: u16,
    bytes: usize,
    _buffer: PendingBuffer,
}

#[cfg(target_os = "linux")]
pub(crate) struct AsyncIo {
    ring: IoUring,
    pending: HashMap<u64, PendingRequest>,
    completion_fd: EventFd,
    next_id: u64,
}

#[cfg(target_os = "linux")]
impl AsyncIo {
    pub(crate) fn new(file: &std::fs::File) -> io::Result<Self> {
        let completion_fd = EventFd::new(EFD_NONBLOCK)?;
        let mut builder = IoUring::builder();
        builder.setup_r_disabled().setup_submit_all();
        let ring = builder.build(MAX_ASYNC_IN_FLIGHT_REQUESTS as u32)?;
        ring.submitter()
            .register_eventfd(completion_fd.as_raw_fd())?;
        ring.submitter().register_files(&[file.as_raw_fd()])?;
        let mut restrictions = [
            Restriction::sqe_op(opcode::Readv::CODE),
            Restriction::sqe_flags_required(squeue::Flags::FIXED_FILE.bits()),
        ];
        ring.submitter().register_restrictions(&mut restrictions)?;
        ring.submitter().register_enable_rings()?;
        Ok(Self {
            ring,
            pending: HashMap::new(),
            completion_fd,
            next_id: 1,
        })
    }

    fn submit_read(&mut self, job: AsyncJob) -> io::Result<()> {
        let (buffer, iovec_ptr, iovec_len) = match job.destination {
            AsyncReadDestination::Guest { mem, iovecs } => {
                let ptr = iovecs.as_ptr();
                let len = iovecs.len() as u32;
                (
                    PendingBuffer::Guest {
                        _mem: mem,
                        _iovecs: iovecs,
                    },
                    ptr,
                    len,
                )
            }
            #[cfg(test)]
            AsyncReadDestination::Owned(mut data) => {
                let iovecs: Box<[libc::iovec]> = vec![libc::iovec {
                    iov_base: data.as_mut_ptr().cast(),
                    iov_len: data.len(),
                }]
                .into_boxed_slice();
                let ptr = iovecs.as_ptr();
                let len = iovecs.len() as u32;
                (
                    PendingBuffer::Owned {
                        _data: data,
                        _iovecs: iovecs,
                    },
                    ptr,
                    len,
                )
            }
        };
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let entry = opcode::Readv::new(types::Fixed(0), iovec_ptr.cast_mut(), iovec_len)
            .offset(job.offset)
            .build()
            .user_data(id);
        self.pending.insert(
            id,
            PendingRequest {
                head_index: job.head_index,
                bytes: job.bytes,
                _buffer: buffer,
            },
        );
        // SAFETY: PendingRequest owns the iovec array and its backing guest or
        // test buffer until the corresponding completion is consumed.
        if unsafe { self.ring.submission().push(&entry) }.is_err() {
            self.pending.remove(&id);
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "io_uring submission queue is full",
            ));
        }
        Ok(())
    }

    fn submit(&self) -> io::Result<()> {
        self.ring.submit().map(|_| ())
    }

    fn completions(&mut self) -> Vec<AsyncCompletion> {
        self.ring
            .completion()
            .filter_map(|entry| {
                let request = self.pending.remove(&entry.user_data())?;
                #[cfg(test)]
                let owned_data = match request._buffer {
                    PendingBuffer::Owned { _data, .. } => Some(_data),
                    PendingBuffer::Guest { .. } => None,
                };
                let result = if entry.result() < 0 {
                    Err(io::Error::from_raw_os_error(-entry.result()))
                } else if entry.result() as usize != request.bytes {
                    Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!(
                            "short asynchronous block I/O: {} of {} bytes",
                            entry.result(),
                            request.bytes
                        ),
                    ))
                } else {
                    Ok(entry.result() as usize)
                };
                Some(AsyncCompletion {
                    head_index: request.head_index,
                    bytes: request.bytes,
                    result,
                    #[cfg(test)]
                    owned_data,
                })
            })
            .collect()
    }

    fn wait_all(&mut self) -> io::Result<Vec<AsyncCompletion>> {
        if self.pending.is_empty() {
            return Ok(Vec::new());
        }
        self.ring.submit_and_wait(self.pending.len())?;
        Ok(self.completions())
    }
}

#[cfg(target_os = "linux")]
fn guest_iovecs(slices: Vec<VolatileSlice<'_>>) -> Box<[libc::iovec]> {
    slices
        .iter()
        .map(|slice| libc::iovec {
            iov_base: slice.ptr_guard_mut().as_ptr().cast(),
            iov_len: slice.len(),
        })
        .collect()
}

#[cfg(not(target_os = "linux"))]
pub(crate) struct AsyncIo {
    completion_fd: EventFd,
}

#[cfg(not(target_os = "linux"))]
impl AsyncIo {
    fn completions(&mut self) -> Vec<AsyncCompletion> {
        unreachable!("async block I/O requires Linux")
    }

    fn wait_all(&mut self) -> io::Result<Vec<AsyncCompletion>> {
        unreachable!("async block I/O requires Linux")
    }
}

pub struct BlockWorker {
    device_queue: DeviceQueue,
    interrupt: InterruptTransport,
    mem: GuestMemoryMmap,
    disk: DiskProperties,
    stop_fd: EventFd,
    async_io: Option<AsyncIo>,
    in_flight: usize,
    in_flight_bytes: usize,
}

impl BlockWorker {
    pub fn new(
        device_queue: DeviceQueue,
        interrupt: InterruptTransport,
        mem: GuestMemoryMmap,
        disk: DiskProperties,
        stop_fd: EventFd,
        io_engine: BlockIoEngine,
    ) -> io::Result<Self> {
        let async_io = if io_engine == BlockIoEngine::Async && disk.backend().supports_async_io() {
            Some(
                disk.backend()
                    .take_async_io()
                    .ok_or_else(|| io::Error::other("async block ring was not prepared"))?,
            )
        } else {
            None
        };
        Ok(Self {
            device_queue,
            interrupt,
            mem,
            disk,
            stop_fd,
            async_io,
            in_flight: 0,
            in_flight_bytes: 0,
        })
    }

    /// Snapshot the worker's virtqueue indices (for checkpoint/fork). Call only
    /// while the worker is stopped (reclaimed), so there is no concurrent access.
    pub(crate) fn save_queue_state(&self) -> crate::virtio::queue::QueueState {
        self.device_queue.queue.save_state()
    }

    /// Restore virtqueue indices onto the reclaimed worker before re-arming it.
    pub(crate) fn restore_queue_state(
        &mut self,
        state: &crate::virtio::queue::QueueState,
    ) -> std::result::Result<(), String> {
        self.device_queue.queue.restore_state(state)
    }

    /// Replace the backing disk while this worker is reclaimed and stopped.
    ///
    /// `Block` owns the quiesce/re-arm lifecycle; keeping the swap here makes
    /// it impossible for an active worker thread to observe a half-pivoted
    /// device.
    pub(crate) fn replace_disk(&mut self, disk: DiskProperties) -> DiskProperties {
        if let Some(async_io) = self.async_io.take() {
            self.disk.backend().return_async_io(async_io);
        }
        let old = std::mem::replace(&mut self.disk, disk);
        self.async_io = self.disk.backend().take_async_io();
        old
    }

    /// Spawn the worker thread. On stop (a write to `stop_fd`) the thread drains
    /// any pending requests and **returns the `BlockWorker`**, so the device can
    /// reclaim the virtqueue (to snapshot its indices) and later re-arm the
    /// worker from it — the device-layer "quiesce to a clean boundary" step for
    /// checkpoint/fork.
    pub fn run(self) -> thread::JoinHandle<BlockWorker> {
        thread::Builder::new()
            .name("block worker".into())
            .spawn(|| self.work())
            .unwrap()
    }

    fn work(mut self) -> BlockWorker {
        let virtq_ev_fd = self.device_queue.event.as_raw_fd();
        let stop_ev_fd = self.stop_fd.as_raw_fd();
        let async_ev_fd = self
            .async_io
            .as_ref()
            .map(|io| io.completion_fd.as_raw_fd());

        let mut epoll = Epoll::new().unwrap();

        let _ = epoll.ctl(
            ControlOperation::Add,
            virtq_ev_fd,
            &EpollEvent::new(EventSet::IN, virtq_ev_fd as u64),
        );

        if let Some(fd) = async_ev_fd {
            let _ = epoll.ctl(
                ControlOperation::Add,
                fd,
                &EpollEvent::new(EventSet::IN, fd as u64),
            );
        }

        let _ = epoll.ctl(
            ControlOperation::Add,
            stop_ev_fd,
            &EpollEvent::new(EventSet::IN, stop_ev_fd as u64),
        );

        let mut epoll_events = vec![EpollEvent::new(EventSet::empty(), 0); 32];
        loop {
            match epoll.wait(epoll_events.len(), -1, epoll_events.as_mut_slice()) {
                Ok(ev_cnt) => {
                    for event in &epoll_events[0..ev_cnt] {
                        let source = event.fd();
                        let event_set = event.event_set();
                        match event_set {
                            EventSet::IN if source == virtq_ev_fd => {
                                self.process_queue_event();
                            }
                            EventSet::IN if source == stop_ev_fd => {
                                debug!("stopping worker thread");
                                let _ = self.stop_fd.read();
                                // Drain: complete any requests the guest made
                                // available before the stop so we yield the
                                // queue at a clean boundary (no in-flight I/O).
                                self.process_virtio_queues();
                                self.drain_async();
                                return self;
                            }
                            EventSet::IN if Some(source) == async_ev_fd => {
                                self.process_async_completions();
                            }
                            _ => {
                                log::warn!(
                                    "Received unknown event: {event_set:?} from fd: {source:?}"
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    debug!("failed to consume muxer epoll event: {e}");
                }
            }
        }
    }

    fn process_queue_event(&mut self) {
        if let Err(e) = self.device_queue.event.read() {
            // A drained eventfd reports WouldBlock on a spurious level-triggered
            // wakeup (common with the Windows epoll shim) — expected, not an error.
            if e.kind() != io::ErrorKind::WouldBlock {
                error!("Failed to get queue event: {e:?}");
            }
        } else {
            self.process_virtio_queues();
        }
    }

    /// Process device virtio queue(s).
    fn process_virtio_queues(&mut self) {
        let mem = self.mem.clone();
        loop {
            self.device_queue.queue.disable_notification(&mem).unwrap();

            self.process_queue(&mem);

            if !self.device_queue.queue.enable_notification(&mem).unwrap() {
                break;
            }
        }
    }

    fn process_queue(&mut self, mem: &GuestMemoryMmap) {
        if self.async_io.is_some() {
            self.process_queue_async(mem);
            return;
        }
        #[cfg(target_os = "windows")]
        if let Some(handles) = self.disk.parallel.clone() {
            self.process_queue_parallel(mem, &handles);
            return;
        }
        let mut signal_needed = false;
        while let Some(head) = self.device_queue.queue.pop(mem) {
            let mut reader = match Reader::new(mem, head.clone()) {
                Ok(r) => r,
                Err(e) => {
                    error!("invalid descriptor chain: {e:?}");
                    continue;
                }
            };
            let mut writer = match Writer::new(mem, head.clone()) {
                Ok(r) => r,
                Err(e) => {
                    error!("invalid descriptor chain: {e:?}");
                    continue;
                }
            };
            let request_header: RequestHeader = match reader.read_obj() {
                Ok(h) => h,
                Err(e) => {
                    error!("invalid request header: {e:?}");
                    continue;
                }
            };

            let req_started = blk_stats::start();
            let req_type = request_header.request_type;
            let req_bytes = reader.available_bytes() + writer.available_bytes();
            let processed = self.process_request(request_header, &mut reader, &mut writer);
            blk_stats::finish(req_type, req_bytes, req_started);
            let (status, len): (u8, usize) = match processed {
                Ok(l) => (VIRTIO_BLK_S_OK.try_into().unwrap(), l),
                Err(e) => {
                    error!("error processing request: {e:?}");
                    (VIRTIO_BLK_S_IOERR.try_into().unwrap(), 0)
                }
            };

            if let Err(e) = writer.write_obj(status) {
                error!("Failed to write virtio block status: {e:?}")
            }

            if let Err(e) = self
                .device_queue
                .queue
                .add_used(mem, head.index, len as u32)
            {
                error!("failed to add used elements to the queue: {e:?}");
            }

            if self.device_queue.queue.needs_notification(mem).unwrap() {
                signal_needed = true;
            }
        }
        // Signal once after draining all block requests rather than per-request,
        // avoiding redundant IRQ signals when multiple descriptors complete in
        // a single epoll wake-up.
        if signal_needed && let Err(e) = self.interrupt.try_signal_used_queue() {
            error!("error signalling queue: {e:?}");
        }
    }

    /// Windows raw-image path: drain the ring, run consecutive reads and
    /// writes concurrently across independent file handles, and keep flush,
    /// discard, write-zeroes and every other request as an ordered barrier
    /// through the regular path. Completions are published in ring order.
    #[cfg(target_os = "windows")]
    fn process_queue_parallel(&mut self, mem: &GuestMemoryMmap, handles: &[File]) {
        struct Pending<'a> {
            index: u16,
            header: RequestHeader,
            reader: Reader<'a>,
            writer: Writer<'a>,
        }

        let mut batch: Vec<Pending> = Vec::new();
        let mut signal_needed = false;
        let nsectors = self.disk.nsectors();

        // Run one request against a raw handle. Bounds are checked here
        // because a plain file handle, unlike the format accessor, would
        // happily extend the image past its advertised size.
        fn run_data(file: &File, p: &mut Pending, nsectors: u64) -> (u8, usize) {
            let started = blk_stats::start();
            let bytes = p.reader.available_bytes() + p.writer.available_bytes();
            let ty = p.header.request_type;
            let r = run_data_inner(file, p, nsectors);
            blk_stats::finish(ty, bytes, started);
            r
        }
        fn run_data_inner(file: &File, p: &mut Pending, nsectors: u64) -> (u8, usize) {
            let ok = VIRTIO_BLK_S_OK as u8;
            let err = VIRTIO_BLK_S_IOERR as u8;
            let off = p.header.sector.saturating_mul(512);
            let (len, res) = if p.header.request_type == VIRTIO_BLK_T_IN {
                let len = p.writer.available_bytes().saturating_sub(1);
                (len, None)
            } else {
                (p.reader.available_bytes(), Some(()))
            };
            if !len.is_multiple_of(512)
                || off.saturating_add(len as u64) > nsectors.saturating_mul(512)
            {
                return (err, 0);
            }
            let r = match res {
                None => p.writer.write_from_at(file, len, off),
                Some(()) => p.reader.read_to_at(file, len, off),
            };
            match r {
                Ok(n) => (ok, n),
                Err(e) => {
                    error!("parallel block request failed: {e:?}");
                    (err, 0)
                }
            }
        }

        fn flush_batch(
            batch: &mut Vec<Pending<'_>>,
            worker: &mut BlockWorker,
            mem: &GuestMemoryMmap,
            handles: &[File],
            nsectors: u64,
            signal: &mut bool,
        ) {
            if batch.is_empty() {
                return;
            }
            let mut results = vec![(0u8, 0usize); batch.len()];
            if batch.len() == 1 {
                results[0] = run_data(&handles[0], &mut batch[0], nsectors);
            } else {
                let lanes = handles.len().min(batch.len());
                // Each request's descriptor reader/writer holds raw pointers
                // into guest memory, which is mapped for the whole scope, and
                // each request is handed to exactly one lane.
                struct Lane<'b, 'a>(Vec<(usize, &'b mut Pending<'a>)>);
                unsafe impl Send for Lane<'_, '_> {}
                let mut per_lane: Vec<Lane> = (0..lanes).map(|_| Lane(Vec::new())).collect();
                for (i, p) in batch.iter_mut().enumerate() {
                    per_lane[i % lanes].0.push((i, p));
                }
                let lane_results: Vec<Vec<(usize, (u8, usize))>> = std::thread::scope(|scope| {
                    let joins: Vec<_> = per_lane
                        .into_iter()
                        .enumerate()
                        .map(|(lane, items)| {
                            let file = &handles[lane];
                            scope.spawn(move || {
                                let items = items;
                                items
                                    .0
                                    .into_iter()
                                    .map(|(i, p)| (i, run_data(file, p, nsectors)))
                                    .collect::<Vec<_>>()
                            })
                        })
                        .collect();
                    joins.into_iter().map(|j| j.join().unwrap()).collect()
                });
                for lane in lane_results {
                    for (i, r) in lane {
                        results[i] = r;
                    }
                }
            }
            for (p, (status, len)) in batch.iter_mut().zip(results) {
                if let Err(e) = p.writer.write_obj(status) {
                    error!("Failed to write virtio block status: {e:?}");
                }
                if let Err(e) = worker.device_queue.queue.add_used(mem, p.index, len as u32) {
                    error!("failed to add used elements to the queue: {e:?}");
                }
                if worker.device_queue.queue.needs_notification(mem).unwrap() {
                    *signal = true;
                }
            }
            batch.clear();
        }

        while let Some(head) = self.device_queue.queue.pop(mem) {
            let index = head.index;
            let (mut reader, writer) = match (
                Reader::new(mem, head.clone()),
                Writer::new(mem, head.clone()),
            ) {
                (Ok(r), Ok(w)) => (r, w),
                _ => {
                    error!("invalid descriptor chain");
                    continue;
                }
            };
            let header: RequestHeader = match reader.read_obj() {
                Ok(h) => h,
                Err(e) => {
                    error!("invalid request header: {e:?}");
                    continue;
                }
            };
            if header.request_type == VIRTIO_BLK_T_IN || header.request_type == VIRTIO_BLK_T_OUT {
                batch.push(Pending {
                    index,
                    header,
                    reader,
                    writer,
                });
                continue;
            }
            // Barrier: finish everything before it, then run it in order.
            flush_batch(&mut batch, self, mem, handles, nsectors, &mut signal_needed);
            let mut writer = writer;
            let started = blk_stats::start();
            let processed = self.process_request(header, &mut reader, &mut writer);
            blk_stats::finish(header.request_type, 0, started);
            let (status, len): (u8, usize) = match processed {
                Ok(l) => (VIRTIO_BLK_S_OK as u8, l),
                Err(e) => {
                    error!("error processing request: {e:?}");
                    (VIRTIO_BLK_S_IOERR as u8, 0)
                }
            };
            if let Err(e) = writer.write_obj(status) {
                error!("Failed to write virtio block status: {e:?}");
            }
            if let Err(e) = self.device_queue.queue.add_used(mem, index, len as u32) {
                error!("failed to add used elements to the queue: {e:?}");
            }
            if self.device_queue.queue.needs_notification(mem).unwrap() {
                signal_needed = true;
            }
        }
        flush_batch(&mut batch, self, mem, handles, nsectors, &mut signal_needed);

        if signal_needed && let Err(e) = self.interrupt.try_signal_used_queue() {
            error!("error signalling queue: {e:?}");
        }
    }

    #[cfg(target_os = "linux")]
    fn process_queue_async(&mut self, mem: &GuestMemoryMmap) {
        let mut signal_needed = false;
        while let Some(head) = self.device_queue.queue.pop(mem) {
            let mut reader = match Reader::new(mem, head.clone()) {
                Ok(reader) => reader,
                Err(error) => {
                    error!("invalid descriptor chain: {error:?}");
                    continue;
                }
            };
            let mut writer = match Writer::new(mem, head.clone()) {
                Ok(writer) => writer,
                Err(error) => {
                    error!("invalid descriptor chain: {error:?}");
                    continue;
                }
            };
            let request_header: RequestHeader = match reader.read_obj() {
                Ok(header) => header,
                Err(error) => {
                    error!("invalid request header: {error:?}");
                    continue;
                }
            };

            let async_job = match request_header.request_type {
                VIRTIO_BLK_T_IN => {
                    let Some(offset) = request_header.sector.checked_mul(512) else {
                        self.complete_inline(
                            mem,
                            head.index,
                            &mut writer,
                            Err(RequestError::InvalidDataLength),
                            &mut signal_needed,
                        );
                        continue;
                    };
                    let Some(data_len) = writer.available_bytes().checked_sub(1) else {
                        self.complete_inline(
                            mem,
                            head.index,
                            &mut writer,
                            Err(RequestError::InvalidDataLength),
                            &mut signal_needed,
                        );
                        continue;
                    };
                    if !data_len.is_multiple_of(512) || data_len > MAX_ASYNC_REQUEST_BYTES {
                        self.complete_inline(
                            mem,
                            head.index,
                            &mut writer,
                            Err(RequestError::InvalidDataLength),
                            &mut signal_needed,
                        );
                        continue;
                    }
                    if self.in_flight >= MAX_ASYNC_IN_FLIGHT_REQUESTS
                        || self.in_flight_bytes.saturating_add(data_len) > MAX_ASYNC_IN_FLIGHT_BYTES
                    {
                        self.drain_async();
                    }
                    let _status = match writer.split_at(data_len) {
                        Ok(status) => status,
                        Err(error) => {
                            self.complete_inline(
                                mem,
                                head.index,
                                &mut writer,
                                Err(RequestError::WritingToDescriptor(io::Error::other(error))),
                                &mut signal_needed,
                            );
                            continue;
                        }
                    };
                    Some(AsyncJob {
                        head_index: head.index,
                        destination: AsyncReadDestination::Guest {
                            mem: mem.clone(),
                            iovecs: guest_iovecs(writer.remaining_volatile_slices()),
                        },
                        offset,
                        bytes: data_len,
                    })
                }
                VIRTIO_BLK_T_OUT => {
                    // Buffered pwritev normally completes into the host page cache. Queueing it
                    // through io_uring adds completion overhead without overlapping storage I/O,
                    // and measured worse at higher queue depths. Keep it on the historical inline
                    // path; flush/discard/write-zero requests below remain explicit barriers.
                    let result = self.process_request(request_header, &mut reader, &mut writer);
                    self.complete_inline(mem, head.index, &mut writer, result, &mut signal_needed);
                    continue;
                }
                _ => None,
            };

            if let Some(job) = async_job {
                let job_bytes = job.bytes;
                let submitted = self
                    .async_io
                    .as_mut()
                    .expect("async engine missing")
                    .submit_read(job);
                let submission_error = match submitted {
                    Ok(()) => {
                        self.in_flight += 1;
                        self.in_flight_bytes += job_bytes;
                        continue;
                    }
                    Err(error) => error,
                };
                // The read path narrowed `writer` to the data descriptors before
                // submission. Reconstruct it and advance to the trailing status
                // descriptor so an admission failure cannot corrupt guest data.
                let mut status_writer = match Writer::new(mem, head.clone()) {
                    Ok(writer) => writer,
                    Err(error) => {
                        error!("async submission error has an invalid descriptor: {error:?}");
                        continue;
                    }
                };
                status_writer = match status_writer.split_at(job_bytes) {
                    Ok(writer) => writer,
                    Err(error) => {
                        error!(
                            "async submission error has an invalid status descriptor: {error:?}"
                        );
                        continue;
                    }
                };
                self.complete_inline(
                    mem,
                    head.index,
                    &mut status_writer,
                    Err(RequestError::WritingToDescriptor(submission_error)),
                    &mut signal_needed,
                );
                continue;
            }

            // Flush/discard/write-zeroes are barriers with respect to earlier
            // asynchronous requests. Drain first, execute the format-aware
            // operation synchronously, then continue admitting later requests.
            if self.in_flight > 0 {
                self.drain_async();
            }
            let result = self.process_request(request_header, &mut reader, &mut writer);
            self.complete_inline(mem, head.index, &mut writer, result, &mut signal_needed);
        }
        if self.in_flight > 0
            && let Some(async_io) = self.async_io.as_ref()
            && let Err(error) = async_io.submit()
        {
            error!("failed to submit asynchronous block requests: {error:?}");
            self.drain_async();
        }
        if signal_needed && let Err(error) = self.interrupt.try_signal_used_queue() {
            error!("error signalling queue: {error:?}");
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn process_queue_async(&mut self, _mem: &GuestMemoryMmap) {
        unreachable!("async block I/O requires Linux")
    }

    #[cfg(target_os = "linux")]
    fn complete_inline(
        &mut self,
        mem: &GuestMemoryMmap,
        head_index: u16,
        writer: &mut Writer,
        result: result::Result<usize, RequestError>,
        signal_needed: &mut bool,
    ) {
        let (status, len) = match result {
            Ok(len) => (VIRTIO_BLK_S_OK as u8, len),
            Err(error) => {
                error!("error processing request: {error:?}");
                (VIRTIO_BLK_S_IOERR as u8, 0)
            }
        };
        if let Err(error) = writer.write_obj(status) {
            error!("failed to write virtio block status: {error:?}");
        }
        if let Err(error) = self
            .device_queue
            .queue
            .add_used(mem, head_index, len as u32)
        {
            error!("failed to add used elements to the queue: {error:?}");
        }
        if self
            .device_queue
            .queue
            .needs_notification(mem)
            .unwrap_or(false)
        {
            *signal_needed = true;
        }
    }

    fn process_async_completions(&mut self) {
        let Some(async_io) = self.async_io.as_mut() else {
            return;
        };
        let _ = async_io.completion_fd.read();
        let completions = async_io.completions();
        let mut signal_needed = false;
        for completion in completions {
            signal_needed |= self.finish_async_completion(completion);
        }
        if signal_needed && let Err(error) = self.interrupt.try_signal_used_queue() {
            error!("error signalling queue: {error:?}");
        }
    }

    fn drain_async(&mut self) {
        if self.in_flight == 0 {
            return;
        }
        let completions = match self.async_io.as_mut() {
            Some(async_io) => match async_io.wait_all() {
                Ok(completions) => completions,
                Err(error) => {
                    error!("failed to drain asynchronous block requests: {error:?}");
                    return;
                }
            },
            None => return,
        };
        let mut signal_needed = false;
        for completion in completions {
            signal_needed |= self.finish_async_completion(completion);
        }
        if signal_needed && let Err(error) = self.interrupt.try_signal_used_queue() {
            error!("error signalling queue: {error:?}");
        }
    }

    fn finish_async_completion(&mut self, completion: AsyncCompletion) -> bool {
        self.in_flight = self.in_flight.saturating_sub(1);
        self.in_flight_bytes = self.in_flight_bytes.saturating_sub(completion.bytes);
        let Some(head) = crate::virtio::queue::DescriptorChain::checked_new(
            &self.mem,
            self.device_queue.queue.desc_table,
            self.device_queue.queue.actual_size(),
            completion.head_index,
        ) else {
            error!("async completion has an invalid descriptor chain");
            return false;
        };
        let mut writer = match Writer::new(&self.mem, head) {
            Ok(writer) => writer,
            Err(error) => {
                error!("async completion writer is invalid: {error:?}");
                return false;
            }
        };
        writer = match writer.split_at(completion.bytes) {
            Ok(status_writer) => status_writer,
            Err(error) => {
                error!("async read completion has an invalid status descriptor: {error:?}");
                return false;
            }
        };
        let mut len = 0;
        let mut status = VIRTIO_BLK_S_IOERR as u8;
        match completion.result {
            Ok(count) => {
                status = VIRTIO_BLK_S_OK as u8;
                len = count;
            }
            Err(error) => error!("asynchronous block request failed: {error:?}"),
        }
        if let Err(error) = writer.write_obj(status) {
            error!("failed to write async block status: {error:?}");
        }
        if let Err(error) =
            self.device_queue
                .queue
                .add_used(&self.mem, completion.head_index, len as u32)
        {
            error!("failed to add async used element: {error:?}");
        }
        self.device_queue
            .queue
            .needs_notification(&self.mem)
            .unwrap_or(false)
    }

    fn process_request(
        &mut self,
        request_header: RequestHeader,
        reader: &mut Reader,
        writer: &mut Writer,
    ) -> result::Result<usize, RequestError> {
        match request_header.request_type {
            VIRTIO_BLK_T_IN => {
                let data_len = writer.available_bytes() - 1;
                if !data_len.is_multiple_of(512) {
                    Err(RequestError::InvalidDataLength)
                } else {
                    writer
                        .write_from_at(&self.disk, data_len, request_header.sector * 512)
                        .map_err(RequestError::WritingToDescriptor)
                }
            }
            VIRTIO_BLK_T_OUT => {
                let data_len = reader.available_bytes();
                if !data_len.is_multiple_of(512) {
                    Err(RequestError::InvalidDataLength)
                } else {
                    reader
                        .read_to_at(&self.disk, data_len, request_header.sector * 512)
                        .map_err(RequestError::ReadingFromDescriptor)
                }
            }
            VIRTIO_BLK_T_FLUSH => match self.disk.cache_type() {
                CacheType::Writeback => {
                    self.disk
                        .file
                        .flush()
                        .map_err(RequestError::FlushingToDisk)?;
                    self.disk
                        .file
                        .sync()
                        .map_err(RequestError::FlushingToDisk)?;
                    Ok(0)
                }
                CacheType::Unsafe => Ok(0),
            },
            VIRTIO_BLK_T_GET_ID => {
                let data_len = writer.available_bytes();
                let disk_id = self.disk.image_id();
                if data_len < disk_id.len() {
                    Err(RequestError::InvalidDataLength)
                } else {
                    writer
                        .write_all(disk_id)
                        .map_err(RequestError::WritingToDescriptor)?;
                    Ok(disk_id.len())
                }
            }
            VIRTIO_BLK_T_DISCARD => {
                let discard_write_data: DiscardWriteData = reader
                    .read_obj()
                    .map_err(RequestError::ReadingFromDescriptor)?;
                self.disk
                    .file
                    .discard_to_any(
                        discard_write_data.sector * 512,
                        discard_write_data.num_sectors as u64 * 512,
                    )
                    .map_err(RequestError::Discarding)?;
                Ok(0)
            }
            VIRTIO_BLK_T_WRITE_ZEROES => {
                let discard_write_data: DiscardWriteData = reader
                    .read_obj()
                    .map_err(RequestError::ReadingFromDescriptor)?;
                let unmap = (discard_write_data.flags & VIRTIO_BLK_WRITE_ZEROES_FLAG_UNMAP) != 0;
                if unmap {
                    self.disk
                        .file
                        .discard_to_zero(
                            discard_write_data.sector * 512,
                            discard_write_data.num_sectors as u64 * 512,
                        )
                        .map_err(RequestError::DiscardingToZero)?;
                } else {
                    self.disk
                        .file
                        .write_zeroes(
                            discard_write_data.sector * 512,
                            discard_write_data.num_sectors as u64 * 512,
                        )
                        .map_err(RequestError::WritingZeroes)?;
                }
                Ok(0)
            }
            _ => Err(RequestError::UnknownRequest),
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn submit_reads(io: &mut AsyncIo, requests: u16) {
        for index in 0..requests {
            io.submit_read(AsyncJob {
                head_index: index,
                destination: AsyncReadDestination::Owned(vec![0; 4096]),
                offset: index as u64 * 4096,
                bytes: 4096,
            })
            .unwrap();
        }
        io.submit().unwrap();
        let completions = io.wait_all().unwrap();
        assert_eq!(completions.len(), requests as usize);
        for completion in completions {
            assert_eq!(completion.result.unwrap(), 4096);
            assert_eq!(
                completion.owned_data.unwrap(),
                vec![completion.head_index as u8; 4096]
            );
        }
    }

    #[test]
    fn async_io_overlaps_raw_requests_and_preserves_payloads() {
        let backing = utils::tempfile::TempFile::new().unwrap();
        backing.as_file().set_len(1024 * 1024).unwrap();
        for index in 0..32 {
            std::os::unix::fs::FileExt::write_all_at(
                backing.as_file(),
                &vec![index as u8; 4096],
                index as u64 * 4096,
            )
            .unwrap();
        }
        let (disk, _) = super::super::device::open_disk_format(
            backing.as_path().to_str().unwrap(),
            super::super::ImageType::Raw,
            true,
            false,
            false,
            BlockIoEngine::Async,
            Some(
                super::super::prepare_async_io(backing.as_path().to_str().unwrap(), true).unwrap(),
            ),
        )
        .unwrap();
        assert!(disk.supports_async_io());
        let mut io = disk.take_async_io().unwrap();
        submit_reads(&mut io, 32);
    }

    #[test]
    fn async_engine_keeps_qcow2_on_the_synchronous_format_path() {
        let base = utils::tempfile::TempFile::new().unwrap();
        base.as_file().set_len(1024 * 1024).unwrap();
        let overlay = base.as_path().with_extension("qcow2");
        super::super::device::create_overlay(
            overlay.to_str().unwrap(),
            base.as_path().to_str().unwrap(),
            super::super::ImageType::Raw,
        )
        .unwrap();
        let (disk, _) = super::super::device::open_disk_format(
            overlay.to_str().unwrap(),
            super::super::ImageType::Qcow2,
            true,
            false,
            false,
            BlockIoEngine::Async,
            None,
        )
        .unwrap();
        assert!(!disk.supports_async_io());
        std::fs::remove_file(overlay).unwrap();
    }
}

/// Opt-in per-request-type accounting (`SMOLVM_BLK_STATS=<dir>`): count, total
/// bytes and total host time, written to `<dir>/blk-<pid>.txt` while running.
mod blk_stats {
    use std::sync::{Mutex, OnceLock};
    use std::time::Instant;

    struct Stats {
        dir: std::path::PathBuf,
        types: [(u64, u64, u64, u64); 32],
        n: u64,
    }

    fn stats() -> Option<&'static Mutex<Stats>> {
        static S: OnceLock<Option<Mutex<Stats>>> = OnceLock::new();
        S.get_or_init(|| {
            std::env::var_os("SMOLVM_BLK_STATS").map(|d| {
                Mutex::new(Stats {
                    dir: d.into(),
                    types: [(0, 0, 0, 0); 32],
                    n: 0,
                })
            })
        })
        .as_ref()
    }

    pub fn start() -> Option<Instant> {
        stats().map(|_| Instant::now())
    }

    pub fn finish(req_type: u32, bytes: usize, started: Option<Instant>) {
        let (Some(t), Some(m)) = (started, stats()) else {
            return;
        };
        let us = t.elapsed().as_micros() as u64;
        let mut s = m.lock().unwrap();
        let e = &mut s.types[(req_type as usize).min(31)];
        e.0 += 1;
        e.1 += bytes as u64;
        e.2 += us;
        e.3 = e.3.max(us);
        s.n += 1;
        if s.n % 50 == 0 {
            let mut out = String::new();
            for (ty, (c, b, us, max)) in s.types.iter().enumerate() {
                if *c > 0 {
                    out.push_str(&format!(
                        "type{ty} count {c} MiB {} host_ms {} max_us {max}\n",
                        b >> 20,
                        us / 1000
                    ));
                }
            }
            let _ = std::fs::create_dir_all(&s.dir);
            let _ = std::fs::write(s.dir.join(format!("blk-{}.txt", std::process::id())), out);
        }
    }
}
