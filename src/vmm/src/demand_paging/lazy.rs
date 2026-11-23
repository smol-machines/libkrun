//! Guest RAM restored lazily from a smolvm stored checkpoint.
//!
//! smolvm writes `memory.bin.lazy` into the restore directory instead of the
//! RAM image: a descriptor naming the stored checkpoint (its `checkpoint.json`
//! index and `objects/` directory) and the file within it that is the image.
//! The image is rebuilt in a memfd as the guest touches it. Guest RAM maps the
//! memfd privately, so the memfd stays the checkpoint's RAM: the immutable base
//! a layered generation branches from, never written by the guest.
//!
//! Every mapping is registered for missing-page faults. A fault is resolved by
//! writing the 1 MiB chunk holding the page into the memfd (verified against
//! its SHA-256 name, as lazy disks do) and waking the faulting thread, whose
//! retry then finds the page in the memfd's page cache. Nothing is copied into
//! private memory, so a touched page is held once, shared with any branch. A
//! page of a chunk of zeros gets the shared zero page, leaving the memfd's hole.
//! Workers fill every other chunk in the background.
//!
//! The memfd is the checkpoint's RAM only once every chunk is in; it is then
//! sealed against writes, as the immutable generation a branch receives.
//! Anything that reads it other than through the guest's own mappings (a save,
//! a branch) must first call [`wait_complete`].

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use devices::virtio::block::lazy::{StoredFile, read_object};

use super::{
    DemandPager, PAGE_SIZE, PagerStatus, UserfaultfdMode, create_userfaultfd, read_fault_message,
    record_failure, register_range,
};

/// Descriptor smolvm writes into the restore directory.
pub const DESCRIPTOR: &str = "memory.bin.lazy";

/// Threads resolving guest faults.
const FAULT_THREADS: usize = 4;
/// Threads filling the rest in the background.
const FILL_THREADS: usize = 4;
/// How long a chunk the store cannot supply yet is waited for.
const FETCH_DEADLINE: Duration = Duration::from_secs(300);

const IOC_READ: u32 = 2;
const fn ior(nr: u32, size: usize) -> libc::c_ulong {
    ((IOC_READ << 30) | ((size as u32) << 16) | (0xaa << 8) | nr) as libc::c_ulong
}
const fn iowr(nr: u32, size: usize) -> libc::c_ulong {
    ((3 << 30) | ((size as u32) << 16) | (0xaa << 8) | nr) as libc::c_ulong
}
const UFFDIO_WAKE: libc::c_ulong = ior(0x02, std::mem::size_of::<WakeRange>());
const UFFDIO_ZEROPAGE: libc::c_ulong = iowr(0x04, std::mem::size_of::<ZeroPage>());

#[repr(C)]
struct WakeRange {
    start: u64,
    len: u64,
}

#[repr(C)]
struct ZeroPage {
    range: WakeRange,
    mode: u64,
    zeropage: i64,
}

struct Descriptor {
    index: PathBuf,
    file: String,
    objects: PathBuf,
    /// Created once every chunk is in, for smolvm to see from outside.
    complete: Option<PathBuf>,
}

impl Descriptor {
    /// Parse a version-1 descriptor; any field it does not know is refused.
    fn parse(body: &[u8], path: &Path) -> io::Result<Self> {
        let bad = |what: &str| invalid(format!("{}: {what}", path.display()));
        let value: serde_json::Value =
            serde_json::from_slice(body).map_err(|e| bad(&e.to_string()))?;
        let object = value.as_object().ok_or_else(|| bad("not an object"))?;
        if let Some(key) = object
            .keys()
            .find(|k| !["version", "index", "file", "objects", "complete"].contains(&k.as_str()))
        {
            return Err(bad(&format!("unknown field {key}")));
        }
        if object.get("version").and_then(|v| v.as_u64()) != Some(1) {
            return Err(bad("unsupported version"));
        }
        let text = |key: &str| {
            object
                .get(key)
                .and_then(|v| v.as_str())
                .map(str::to_owned)
                .ok_or_else(|| bad(&format!("missing {key}")))
        };
        Ok(Self {
            index: PathBuf::from(text("index")?),
            file: text("file")?,
            objects: PathBuf::from(text("objects")?),
            complete: match object.get("complete") {
                None | Some(serde_json::Value::Null) => None,
                Some(_) => Some(PathBuf::from(text("complete")?)),
            },
        })
    }
}

const MISSING: u8 = 0;
const FETCHING: u8 = 1;
const PRESENT: u8 = 2;

struct Shared {
    memfd: File,
    stored: StoredFile,
    objects: PathBuf,
    label: PathBuf,
    state: Mutex<Vec<u8>>,
    changed: Condvar,
    missing: AtomicUsize,
    stop: AtomicBool,
    failure: Mutex<Option<String>>,
    /// Every chunk is in and the memfd is sealed against writes.
    sealed: AtomicBool,
    complete: Option<PathBuf>,
}

/// RAM being rebuilt from a stored checkpoint.
pub struct LazyRam {
    shared: Arc<Shared>,
}

/// The lazy RAM of this VMM, for the control commands that must wait for it.
static ACTIVE: Mutex<Option<Arc<Shared>>> = Mutex::new(None);

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl LazyRam {
    /// The lazy RAM `dir` describes, `len` bytes long, when smolvm asked for it.
    pub fn open(dir: &Path, len: u64) -> io::Result<Option<Self>> {
        let path = dir.join(DESCRIPTOR);
        let body = match std::fs::read(&path) {
            Ok(body) => body,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let d = Descriptor::parse(&body, &path)?;
        let stored = StoredFile::read(&d.index, &d.file)?;
        if stored.size != len {
            return Err(invalid(format!(
                "lazy RAM image is {} bytes, the manifest says {len}",
                stored.size
            )));
        }
        // Sealable and unsealed, as a restore base file reads to the snapshot
        // code: a memfd that forbids sealing is taken for an older source's
        // raw RAM, which restores differently.
        let fd = unsafe {
            libc::memfd_create(
                c"smolvm-lazy-ram".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let memfd = unsafe { File::from_raw_fd(fd) };
        memfd.set_len(len)?;
        // Chunks of zeros are already what the memfd holds.
        let state: Vec<u8> = stored
            .chunks
            .iter()
            .map(|c| if c.is_some() { MISSING } else { PRESENT })
            .collect();
        let missing = state.iter().filter(|s| **s != PRESENT).count();
        Ok(Some(Self {
            shared: Arc::new(Shared {
                memfd,
                stored,
                objects: d.objects,
                label: path,
                state: Mutex::new(state),
                changed: Condvar::new(),
                missing: AtomicUsize::new(missing),
                stop: AtomicBool::new(false),
                failure: Mutex::new(None),
                sealed: AtomicBool::new(false),
                complete: d.complete,
            }),
        }))
    }

    /// The memfd the RAM is rebuilt in. Map it privately, never shared.
    pub fn file(&self) -> &File {
        &self.shared.memfd
    }

    /// Register the guest's mappings of the memfd and start resolving their
    /// faults and filling the rest. `mappings` are `(host address, length,
    /// offset in the memfd)`. The returned pager stops it all when dropped.
    pub fn start(
        self,
        mappings: &[(u64, u64, u64)],
        mode: UserfaultfdMode,
    ) -> io::Result<DemandPager> {
        let uffd = create_userfaultfd(mode)?;
        for &(start, len, _) in mappings {
            register_range(uffd.as_raw_fd(), start, len)?;
        }
        let stop_fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if stop_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let stop_fd = unsafe { OwnedFd::from_raw_fd(stop_fd) };
        let status = Arc::new(Mutex::new(PagerStatus {
            failure: None,
            notifier: None,
        }));
        *lock(&ACTIVE) = Some(self.shared.clone());
        if self.shared.missing.load(Ordering::Acquire) == 0 {
            finished(&self.shared);
        }
        let shared = self.shared;
        let mappings = mappings.to_vec();
        let thread_status = status.clone();
        let raw_stop = stop_fd.as_raw_fd();
        let handler = thread::Builder::new()
            .name("smolvm-lazy-ram".into())
            .spawn(move || supervise(shared, uffd, raw_stop, mappings, thread_status))?;
        Ok(DemandPager {
            stop_fd,
            handler: Some(handler),
            status,
        })
    }
}

/// Wait until this VMM's lazy RAM, if any, holds every chunk: its memfd is
/// then the checkpoint's RAM. Fails if filling it failed.
pub fn wait_complete() -> io::Result<()> {
    let Some(shared) = lock(&ACTIVE).clone() else {
        return Ok(());
    };
    let mut state = lock(&shared.state);
    loop {
        if let Some(failure) = lock(&shared.failure).clone() {
            return Err(io::Error::other(format!("lazy RAM failed: {failure}")));
        }
        if shared.sealed.load(Ordering::Acquire) {
            return Ok(());
        }
        state = shared
            .changed
            .wait_timeout(state, Duration::from_millis(100))
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
}

fn supervise(
    shared: Arc<Shared>,
    uffd: OwnedFd,
    stop_fd: RawFd,
    mappings: Vec<(u64, u64, u64)>,
    status: Arc<Mutex<PagerStatus>>,
) {
    let uffd = Arc::new(uffd);
    let next = Arc::new(AtomicUsize::new(0));
    let mut workers = Vec::new();
    for _ in 0..FAULT_THREADS {
        let (shared, uffd, mappings, status) = (
            shared.clone(),
            uffd.clone(),
            mappings.clone(),
            status.clone(),
        );
        workers.push(thread::spawn(move || {
            if let Err(error) = resolve_faults(&shared, uffd.as_raw_fd(), stop_fd, &mappings) {
                fail(&shared, &status, error);
            }
        }));
    }
    for _ in 0..FILL_THREADS {
        let (shared, next, status) = (shared.clone(), next.clone(), status.clone());
        workers.push(thread::spawn(move || {
            while !shared.stop.load(Ordering::Relaxed) {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= shared.stored.chunks.len() {
                    return;
                }
                if let Err(error) = ensure(&shared, i) {
                    // A fault for this chunk retries it; the background fill
                    // giving up is fatal only once nothing can retry.
                    fail(&shared, &status, error);
                    return;
                }
            }
        }));
    }
    // Run until the pager is dropped.
    let mut poll_fd = libc::pollfd {
        fd: stop_fd,
        events: libc::POLLIN,
        revents: 0,
    };
    while unsafe { libc::poll(&mut poll_fd, 1, -1) } < 0 {}
    shared.stop.store(true, Ordering::Relaxed);
    shared.changed.notify_all();
    for worker in workers {
        let _ = worker.join();
    }
    let mut active = lock(&ACTIVE);
    if active.as_ref().is_some_and(|a| Arc::ptr_eq(a, &shared)) {
        *active = None;
    }
}

fn fail(shared: &Shared, status: &Arc<Mutex<PagerStatus>>, error: io::Error) {
    if shared.stop.load(Ordering::Relaxed) {
        return;
    }
    *lock(&shared.failure) = Some(error.to_string());
    shared.changed.notify_all();
    record_failure(
        status,
        format!("lazy RAM {}: {error}", shared.label.display()),
    );
}

/// Resolve faults on the guest's mappings until stopped.
fn resolve_faults(
    shared: &Shared,
    uffd: RawFd,
    stop_fd: RawFd,
    mappings: &[(u64, u64, u64)],
) -> io::Result<()> {
    let mut poll_fds = [
        libc::pollfd {
            fd: uffd,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: stop_fd,
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    loop {
        if unsafe { libc::poll(poll_fds.as_mut_ptr(), 2, -1) } < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if poll_fds[1].revents & libc::POLLIN != 0 {
            return Ok(());
        }
        if poll_fds[0].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(io::Error::other("userfaultfd poll failure"));
        }
        // Several threads read the one userfaultfd; another may have taken it.
        while let Some(address) = read_fault_message(uffd)? {
            let page = address & !(PAGE_SIZE as u64 - 1);
            let &(start, _, offset) = mappings
                .iter()
                .find(|(start, len, _)| page >= *start && page < start + len)
                .ok_or_else(|| invalid(format!("fault at {address:#x} is outside guest RAM")))?;
            let file_offset = offset + (page - start);
            let chunk = (file_offset / shared.stored.chunk_size) as usize;
            if shared.stored.chunks[chunk].is_none() {
                // A page of a chunk of zeros: the memfd keeps its hole, and
                // this private view maps the shared zero page there.
                let mut zero = ZeroPage {
                    range: WakeRange {
                        start: page,
                        len: PAGE_SIZE as u64,
                    },
                    mode: 0,
                    zeropage: 0,
                };
                if unsafe { libc::ioctl(uffd, UFFDIO_ZEROPAGE as _, &mut zero) } < 0 {
                    let error = io::Error::last_os_error();
                    // Another thread resolved the same page first.
                    if error.raw_os_error() != Some(libc::EEXIST) {
                        return Err(error);
                    }
                }
                continue;
            }
            ensure(shared, chunk)?;
            let mut range = WakeRange {
                start: page,
                len: PAGE_SIZE as u64,
            };
            if unsafe { libc::ioctl(uffd, UFFDIO_WAKE as _, &mut range) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
}

/// Make chunk `i` present in the memfd. Concurrent callers wait for the first.
fn ensure(shared: &Shared, i: usize) -> io::Result<()> {
    {
        let mut state = lock(&shared.state);
        loop {
            match state[i] {
                PRESENT => return Ok(()),
                FETCHING => {
                    state = shared
                        .changed
                        .wait(state)
                        .unwrap_or_else(|e| e.into_inner())
                }
                _ => {
                    state[i] = FETCHING;
                    break;
                }
            }
        }
    }
    let result = fetch(shared, i);
    let mut state = lock(&shared.state);
    state[i] = if result.is_ok() { PRESENT } else { MISSING };
    drop(state);
    if result.is_ok() && shared.missing.fetch_sub(1, Ordering::AcqRel) == 1 {
        finished(shared);
    }
    shared.changed.notify_all();
    result
}

fn fetch(shared: &Shared, i: usize) -> io::Result<()> {
    let hash = shared.stored.chunks[i]
        .as_deref()
        .expect("only chunks with objects are fetched");
    let chunk = read_object(
        &shared.objects,
        hash,
        shared.stored.chunk_len(i),
        FETCH_DEADLINE,
        &shared.stop,
        &shared.label,
    )?;
    shared
        .memfd
        .write_all_at(&chunk, i as u64 * shared.stored.chunk_size)
}

/// Every chunk is in: seal the memfd so it can stand as an immutable
/// generation a branch receives, then say so.
fn finished(shared: &Shared) {
    let seals = libc::F_SEAL_WRITE | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL;
    if unsafe { libc::fcntl(shared.memfd.as_raw_fd(), libc::F_ADD_SEALS, seals) } < 0 {
        let error = io::Error::last_os_error();
        *lock(&shared.failure) = Some(format!("seal the restored RAM: {error}"));
        shared.changed.notify_all();
        return;
    }
    shared.sealed.store(true, Ordering::Release);
    shared.changed.notify_all();
    if let Some(marker) = &shared.complete
        && let Err(error) = std::fs::write(marker, b"1\n")
    {
        log::warn!(
            "lazy RAM {}: could not record that it is complete: {error}",
            shared.label.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use vm_memory::{Bytes, GuestAddress, GuestMemory, GuestMemoryMmap};

    const CS: usize = 1024 * 1024;

    /// The tests share this process's one active lazy RAM.
    static SERIAL: Mutex<()> = Mutex::new(());

    /// A stored checkpoint holding `chunks` (Some(fill byte) or None for zeros)
    /// as `memory.bin`, and its lazy RAM descriptor in `dir`.
    fn stored(dir: &Path, chunks: &[Option<u8>]) -> Vec<u8> {
        let objects = dir.join("objects");
        std::fs::create_dir_all(&objects).unwrap();
        let mut data = Vec::new();
        let mut names = Vec::new();
        for chunk in chunks {
            let bytes = vec![chunk.unwrap_or(0); CS];
            data.extend_from_slice(&bytes);
            names.push(chunk.map(|_| {
                let hash = hex(&Sha256::digest(&bytes));
                std::fs::write(
                    objects.join(&hash),
                    zstd::bulk::compress(&bytes, 1).unwrap(),
                )
                .unwrap();
                hash
            }));
        }
        let index = serde_json::json!({
            "version": 1, "chunk_size": CS,
            "files": [{"path": "checkpoint/memory.bin", "size": data.len(), "chunks": names}],
        });
        std::fs::write(dir.join("checkpoint.json"), index.to_string()).unwrap();
        let descriptor = serde_json::json!({
            "version": 1,
            "index": dir.join("checkpoint.json"),
            "file": "checkpoint/memory.bin",
            "objects": objects,
            "complete": dir.join("complete"),
        });
        std::fs::write(dir.join(DESCRIPTOR), descriptor.to_string()).unwrap();
        data
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Map `file` privately as one region, as layered restore does.
    fn map_private(file: &File, len: usize) -> GuestMemoryMmap {
        let pointer = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_NORESERVE,
                file.as_raw_fd(),
                0,
            )
        };
        assert_ne!(pointer, libc::MAP_FAILED);
        let region = unsafe {
            vm_memory::MmapRegion::build_raw_owned(
                pointer.cast(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_NORESERVE,
            )
        }
        .unwrap();
        GuestMemoryMmap::from_regions(vec![
            vm_memory::GuestRegionMmap::new(region, GuestAddress(0)).unwrap(),
        ])
        .unwrap()
    }

    fn start(dir: &Path, len: usize) -> (GuestMemoryMmap, DemandPager, File) {
        let ram = LazyRam::open(dir, len as u64).unwrap().unwrap();
        let file = ram.file().try_clone().unwrap();
        let memory = map_private(&file, len);
        let region = memory.iter().next().unwrap();
        let host = region.as_ptr() as u64;
        let pager = ram
            .start(&[(host, len as u64, 0)], UserfaultfdMode::UserModeTest)
            .unwrap();
        (memory, pager, file)
    }

    #[test]
    fn a_missing_descriptor_means_ram_is_not_lazy() {
        let dir = tempfile::tempdir().unwrap();
        assert!(LazyRam::open(dir.path(), 4096).unwrap().is_none());
    }

    #[test]
    fn guest_reads_see_the_checkpoint_and_the_memfd_becomes_its_ram() {
        let _serial = lock(&SERIAL);
        let dir = tempfile::tempdir().unwrap();
        let data = stored(dir.path(), &[Some(3), None, Some(7), Some(9)]);
        let (memory, _pager, file) = start(dir.path(), data.len());
        // Guest reads across every chunk, including the one of zeros.
        for (i, chunk) in data.chunks(CS).enumerate() {
            let mut page = [0_u8; 64];
            memory
                .read_slice(&mut page, GuestAddress((i * CS + 4096) as u64))
                .unwrap();
            assert_eq!(page[..], chunk[4096..4096 + 64], "chunk {i}");
        }
        // A guest write stays private; the memfd keeps the checkpoint.
        memory.write_slice(&[0xee; 8], GuestAddress(0)).unwrap();
        wait_complete().unwrap();
        assert!(dir.path().join("complete").exists());
        let seals = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) };
        assert!(seals & libc::F_SEAL_WRITE != 0, "sealed once complete");
        let mut base = vec![0_u8; data.len()];
        file.read_exact_at(&mut base, 0).unwrap();
        assert_eq!(base, data);
    }

    #[test]
    fn a_layered_generation_over_lazy_ram_sees_the_checkpoint() {
        let _serial = lock(&SERIAL);
        let dir = tempfile::tempdir().unwrap();
        let data = stored(dir.path(), &[Some(4), None, Some(6)]);
        let ram = LazyRam::open(dir.path(), data.len() as u64)
            .unwrap()
            .unwrap();
        let descs = [crate::snapshot::MemoryRegionDesc {
            gpa: 0,
            len: data.len() as u64,
        }];
        let generation =
            crate::layered_restore::Generation::from_immutable_file(&descs, ram.file()).unwrap();
        // The memfd is all holes yet; a sparse mapping would miss it entirely.
        let memory = generation.restore_dense().unwrap();
        let region = memory.iter().next().unwrap();
        let _pager = ram
            .start(
                &[(region.as_ptr() as u64, data.len() as u64, 0)],
                UserfaultfdMode::UserModeTest,
            )
            .unwrap();
        let mut read = vec![0_u8; data.len()];
        memory.read_slice(&mut read, GuestAddress(0)).unwrap();
        assert_eq!(read, data);
    }

    #[test]
    fn a_corrupt_object_fails_instead_of_mapping_bad_memory() {
        let _serial = lock(&SERIAL);
        let dir = tempfile::tempdir().unwrap();
        let data = stored(dir.path(), &[Some(1)]);
        for entry in std::fs::read_dir(dir.path().join("objects")).unwrap() {
            let path = entry.unwrap().path();
            std::fs::write(&path, zstd::bulk::compress(&[5_u8; CS], 1).unwrap()).unwrap();
        }
        let (_memory, pager, _file) = start(dir.path(), data.len());
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while pager.failure().is_none() {
            assert!(std::time::Instant::now() < deadline, "no failure recorded");
            thread::sleep(Duration::from_millis(10));
        }
        assert!(pager.failure().unwrap().contains("does not match its hash"));
        assert!(wait_complete().is_err());
    }
}
