//! Disks served lazily from a stored checkpoint.
//!
//! A disk at `PATH` is lazy when smolvm wrote `PATH.lazydisk` beside it: a
//! descriptor naming a stored checkpoint (its `checkpoint.json` index and
//! `objects/` directory, the format in smolvm's `crates/smolvm-checkpoint/FORMAT.md`)
//! and the file within it this disk is. Nothing is copied before boot: a 1 MiB
//! chunk is read from the store the first time the guest touches it, verified
//! against its SHA-256 name, and kept in a local cache file that from then on
//! *is* the disk for that chunk. Guest writes go to the cache once the chunk
//! they touch is present.
//!
//! Which chunks the cache holds is persisted beside it only after the cache is
//! synced, so a crash never claims data the cache does not hold: it refetches.
//! It is persisted at once when the last chunk arrives, which is how smolvm
//! learns a running machine's disk is complete.
//!
//! The checkpoint fixes only the bytes it holds. A qcow2 layer the guest writes
//! grows past them as it allocates clusters: those bytes are local, never
//! fetched, and the disk's size is the cache file's.
//!
//! The VMM holds a shared lock on the descriptor for as long as it serves the
//! disk. smolvm fills a disk itself only holding that lock exclusively, so the
//! two never write the cache at once; a disk smolvm is filling cannot be
//! opened.
//!
//! While the guest runs, workers fetch ahead of it: the chunks after one it
//! asked for, the order an earlier restore of the same checkpoint asked for
//! them in (a trace), and optionally everything else, capped so the guest's own
//! reads keep the store's bandwidth.
//!
//! A chunk the store cannot supply yet is retried until a deadline rather than
//! failing at once: a guest I/O error turns its filesystem read-only. smolvm
//! may still be importing the checkpoint, writing each object whole under its
//! name; one it marked `.import-failed` will not bring the rest, so a missing
//! object fails at once there. A chunk
//! whose content does not match its hash fails at once; corrupt data is never
//! returned.

use std::collections::VecDeque;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use imago::format::gate::ImplicitOpenGate;
use imago::io_buffers::{IoVector, IoVectorMut};
use imago::storage::PreallocateMode;
use imago::storage::drivers::CommonStorageHelper;
use imago::{DynStorage, FormatAccess, FormatDriverBuilder, Storage, StorageOpenOptions};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Suffix of the descriptor that makes a disk lazy.
pub const DESCRIPTOR_SUFFIX: &str = ".lazydisk";

/// The descriptor that makes `path` lazy, when smolvm wrote one.
pub fn descriptor_for(path: &Path) -> Option<PathBuf> {
    let descriptor = PathBuf::from(format!("{}{DESCRIPTOR_SUFFIX}", path.display()));
    descriptor.is_file().then_some(descriptor)
}

/// `PATH.lazydisk`, written by smolvm.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Descriptor {
    version: u32,
    /// The stored checkpoint's `checkpoint.json`.
    index: PathBuf,
    /// The file within the index this disk is (`files[].path`).
    file: String,
    /// Directory holding the checkpoint's objects, named by SHA-256.
    objects: PathBuf,
    /// Local cache file: the disk's bytes as they become present.
    cache: PathBuf,
    /// Chunk indices an earlier restore fetched, in order, to prefetch.
    #[serde(default)]
    trace: Option<PathBuf>,
    /// Where to record the chunks this run fetches on demand, in order.
    #[serde(default)]
    trace_out: Option<PathBuf>,
    /// Chunks fetched after each one the guest asks for.
    #[serde(default = "default_readahead")]
    readahead: usize,
    /// Fetch workers.
    #[serde(default = "default_workers")]
    workers: usize,
    /// Also fetch every remaining chunk in the background.
    #[serde(default)]
    fill: bool,
    /// Background-fill fetches in flight at most.
    #[serde(default = "default_fill_max")]
    fill_max: usize,
    /// How long a chunk the store cannot supply is retried before the read fails.
    #[serde(default = "default_fetch_deadline")]
    fetch_deadline_secs: u64,
}

fn default_readahead() -> usize {
    16
}
fn default_workers() -> usize {
    16
}
fn default_fill_max() -> usize {
    4
}
fn default_fetch_deadline() -> u64 {
    300
}

/// The parts of a stored checkpoint's index this reads.
#[derive(Debug, Deserialize)]
struct Index {
    version: u32,
    chunk_size: u64,
    files: Vec<IndexFile>,
}

#[derive(Debug, Deserialize)]
struct IndexFile {
    path: String,
    size: u64,
    chunks: Vec<Option<String>>,
}

const MISSING: u8 = 0;
const FETCHING: u8 = 1;
const PRESENT: u8 = 2;

struct Shared {
    image: PathBuf,
    objects: PathBuf,
    chunk_size: u64,
    size: u64,
    chunks: Vec<Option<String>>,
    cache: File,
    present_path: PathBuf,
    state: Mutex<Vec<u8>>,
    changed: Condvar,
    trace_out: Option<Mutex<File>>,
    /// Workers' queues: `.0` readahead and trace (first), `.1` background fill.
    queue: Mutex<(VecDeque<usize>, VecDeque<usize>)>,
    queued: Condvar,
    readahead: usize,
    filling: AtomicUsize,
    fill_max: usize,
    fetch_deadline: Duration,
    stop: AtomicBool,
    /// The descriptor, locked shared until the last worker is gone.
    _lock: File,
    /// Chunks not present yet. When it reaches zero the presence record is
    /// written at once, so smolvm sees the disk is complete while it runs.
    missing: AtomicUsize,
}

/// A disk served from a stored checkpoint (see the module docs).
pub struct LazyStorage {
    shared: Arc<Shared>,
    helper: CommonStorageHelper,
}

impl fmt::Debug for LazyStorage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LazyStorage({})", self.shared.image.display())
    }
}

impl fmt::Display for LazyStorage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "lazy:{}", self.shared.image.display())
    }
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

impl LazyStorage {
    /// Open the lazy disk `descriptor` describes.
    pub fn open(descriptor: &Path) -> io::Result<Self> {
        let mut held = File::open(descriptor)?;
        held.try_lock_shared().map_err(|_| {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("{}: smolvm is completing this disk", descriptor.display()),
            )
        })?;
        let mut body = Vec::new();
        io::Read::read_to_end(&mut held, &mut body)?;
        let d: Descriptor = serde_json::from_slice(&body)
            .map_err(|e| invalid(format!("{}: {e}", descriptor.display())))?;
        if d.version != 1 {
            return Err(invalid(format!(
                "lazy disk descriptor version {} is not supported",
                d.version
            )));
        }
        let index: Index = serde_json::from_slice(&std::fs::read(&d.index)?)
            .map_err(|e| invalid(format!("{}: {e}", d.index.display())))?;
        if index.version != 1 || index.chunk_size == 0 {
            return Err(invalid(
                "stored checkpoint index version or chunk size is not supported",
            ));
        }
        let file = index
            .files
            .into_iter()
            .find(|f| f.path == d.file)
            .ok_or_else(|| invalid(format!("the checkpoint has no file {}", d.file)))?;
        if file.chunks.len() as u64 != file.size.div_ceil(index.chunk_size) {
            return Err(invalid(format!(
                "{}: chunk count does not match its size",
                d.file
            )));
        }
        if let Some(bad) = file.chunks.iter().flatten().find(|h| !is_sha256(h)) {
            return Err(invalid(format!(
                "{}: object name {bad} is not a SHA-256",
                d.file
            )));
        }

        // A cache whose presence record is intact resumes; anything else starts over.
        let present_path = PathBuf::from(format!("{}.present", d.cache.display()));
        let resumed = std::fs::read(&present_path)
            .ok()
            .filter(|p| p.len() == file.chunks.len())
            .filter(|_| std::fs::metadata(&d.cache).is_ok_and(|m| m.len() >= file.size));
        let cache = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(resumed.is_none())
            .open(&d.cache)?;
        // Never shorter than the checkpoint's file; a qcow2 layer may have
        // grown past it since, and that growth is kept.
        if cache.metadata()?.len() < file.size {
            cache.set_len(file.size)?;
        }
        let state: Vec<u8> = file
            .chunks
            .iter()
            .enumerate()
            .map(|(i, hash)| match (hash, &resumed) {
                (None, _) => PRESENT,
                (Some(_), Some(p)) if p[i] == 1 => PRESENT,
                _ => MISSING,
            })
            .collect();
        let trace_out = match &d.trace_out {
            Some(p) => Some(Mutex::new(
                OpenOptions::new().create(true).append(true).open(p)?,
            )),
            None => None,
        };
        let image = descriptor
            .to_str()
            .and_then(|s| s.strip_suffix(DESCRIPTOR_SUFFIX))
            .map(PathBuf::from)
            .unwrap_or_else(|| descriptor.to_path_buf());
        let missing = state.iter().filter(|st| **st != PRESENT).count();
        let shared = Arc::new(Shared {
            image,
            objects: d.objects,
            chunk_size: index.chunk_size,
            size: file.size,
            chunks: file.chunks,
            cache,
            present_path,
            state: Mutex::new(state),
            changed: Condvar::new(),
            trace_out,
            queue: Mutex::new(Default::default()),
            queued: Condvar::new(),
            readahead: d.readahead,
            filling: AtomicUsize::new(0),
            fill_max: d.fill_max,
            fetch_deadline: Duration::from_secs(d.fetch_deadline_secs),
            stop: AtomicBool::new(false),
            _lock: held,
            missing: AtomicUsize::new(missing),
        });
        {
            let mut q = lock(&shared.queue);
            if let Some(trace) = d
                .trace
                .as_ref()
                .and_then(|p| std::fs::read_to_string(p).ok())
            {
                q.0.extend(
                    trace
                        .split_whitespace()
                        .filter_map(|i| i.parse::<usize>().ok())
                        .filter(|i| *i < shared.chunks.len()),
                );
            }
            if d.fill {
                let state = lock(&shared.state);
                q.1.extend((0..shared.chunks.len()).filter(|i| state[*i] == MISSING));
            }
        }
        for _ in 0..d.workers.max(1) {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("lazy-disk".into())
                .spawn(move || worker(&shared))?;
        }
        shared.queued.notify_all();
        Ok(Self {
            shared,
            helper: CommonStorageHelper::default(),
        })
    }

    fn ensure_range(&self, offset: u64, len: u64) -> io::Result<()> {
        if len == 0 {
            return Ok(());
        }
        let cs = self.shared.chunk_size;
        // Chunks past the checkpoint's file hold only what was written here.
        let end = ((offset + len - 1) / cs + 1).min(self.shared.chunks.len() as u64);
        for chunk in offset / cs..end {
            ensure(&self.shared, chunk as usize, true)?;
        }
        Ok(())
    }

    /// Sync the cache, then record which chunks it holds.
    fn persist(&self) -> io::Result<()> {
        persist(&self.shared)
    }
}

impl Drop for LazyStorage {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.queued.notify_all();
        if let Err(e) = self.persist() {
            log::warn!(
                "lazy disk {}: could not record its cache: {e}",
                self.shared.image.display()
            );
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn is_sha256(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn persist(s: &Shared) -> io::Result<()> {
    s.cache.sync_data()?;
    let present: Vec<u8> = lock(&s.state)
        .iter()
        .map(|st| u8::from(*st == PRESENT))
        .collect();
    let tmp = PathBuf::from(format!("{}.tmp", s.present_path.display()));
    {
        let mut f = File::create(&tmp)?;
        f.write_all(&present)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &s.present_path)?;
    if let Some(out) = &s.trace_out {
        lock(out).flush()?;
    }
    Ok(())
}

/// Fetch queued chunks until the disk is closed: readahead and the trace before
/// background fill, which runs at most `fill_max` at a time.
fn worker(s: &Shared) {
    while !s.stop.load(Ordering::Relaxed) {
        let next = {
            let mut q = lock(&s.queue);
            let pick = q.0.pop_front().map(|i| (i, false)).or_else(|| {
                (s.filling.load(Ordering::Relaxed) < s.fill_max)
                    .then(|| q.1.pop_front().map(|i| (i, true)))
                    .flatten()
            });
            match pick {
                Some(p) => p,
                None => {
                    let _ = s.queued.wait_timeout(q, Duration::from_millis(50));
                    continue;
                }
            }
        };
        let (i, fill) = next;
        if fill {
            s.filling.fetch_add(1, Ordering::Relaxed);
        }
        if let Err(e) = ensure(s, i, false) {
            log::warn!(
                "lazy disk {}: chunk {i} not fetched ahead: {e}",
                s.image.display()
            );
        }
        if fill {
            s.filling.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// Make chunk `i` present in the cache. Concurrent callers for one chunk wait
/// for the first; a guest read (`on_demand`) records the chunk in the trace and
/// queues the chunks after it.
fn ensure(s: &Shared, i: usize, on_demand: bool) -> io::Result<()> {
    {
        let mut state = lock(&s.state);
        loop {
            match state[i] {
                PRESENT => return Ok(()),
                FETCHING => state = s.changed.wait(state).unwrap_or_else(|e| e.into_inner()),
                _ => {
                    state[i] = FETCHING;
                    break;
                }
            }
        }
    }
    let result = fetch_into_cache(s, i);
    let mut state = lock(&s.state);
    state[i] = if result.is_ok() { PRESENT } else { MISSING };
    s.changed.notify_all();
    drop(state);
    result?;
    if s.missing.fetch_sub(1, Ordering::AcqRel) == 1
        && let Err(e) = persist(s)
    {
        log::warn!(
            "lazy disk {}: could not record that it is complete: {e}",
            s.image.display()
        );
    }
    if on_demand {
        if let Some(out) = &s.trace_out {
            let _ = writeln!(lock(out), "{i}");
        }
        read_ahead(s, i);
    }
    Ok(())
}

fn read_ahead(s: &Shared, i: usize) {
    if s.readahead == 0 {
        return;
    }
    let end = (i + 1 + s.readahead).min(s.chunks.len());
    let wanted: Vec<usize> = {
        let state = lock(&s.state);
        (i + 1..end).filter(|j| state[*j] == MISSING).collect()
    };
    if wanted.is_empty() {
        return;
    }
    let mut q = lock(&s.queue);
    for j in wanted.into_iter().rev() {
        q.0.push_front(j);
    }
    drop(q);
    s.queued.notify_all();
}

/// Whether smolvm marked the checkpoint holding `objects` as an import that
/// stopped short: an object missing from it will not arrive.
fn import_failed(objects: &Path) -> bool {
    objects
        .parent()
        .is_some_and(|checkpoint| checkpoint.join(".import-failed").exists())
}

/// Read chunk `i`'s object, check it, and write it into the cache. An object the
/// store cannot supply is retried until the deadline; a corrupt one fails at once.
fn fetch_into_cache(s: &Shared, i: usize) -> io::Result<()> {
    let hash = s.chunks[i]
        .as_deref()
        .expect("a missing chunk names its object");
    let offset = i as u64 * s.chunk_size;
    let expected = (s.size - offset).min(s.chunk_size) as usize;
    let path = s.objects.join(hash);
    let started = Instant::now();
    let mut delay = Duration::from_millis(20);
    let mut warned = false;
    let compressed = loop {
        match std::fs::read(&path) {
            Ok(b) => break b,
            Err(e)
                if started.elapsed() < s.fetch_deadline
                    && !s.stop.load(Ordering::Relaxed)
                    && !import_failed(&s.objects) =>
            {
                if !warned && started.elapsed() > Duration::from_secs(5) {
                    warned = true;
                    log::warn!(
                        "lazy disk {}: object {hash} not available yet: {e}",
                        s.image.display()
                    );
                }
                std::thread::sleep(delay);
                // Short: an object still being imported is read soon after it lands.
                delay = (delay * 2).min(Duration::from_millis(100));
            }
            Err(e) => return Err(io::Error::new(e.kind(), format!("object {hash}: {e}"))),
        }
    };
    let chunk = zstd::bulk::decompress(&compressed, expected)
        .map_err(|e| invalid(format!("object {hash} does not decompress: {e}")))?;
    if chunk.len() != expected {
        return Err(invalid(format!(
            "object {hash} is {} bytes, not {expected}",
            chunk.len()
        )));
    }
    let digest = Sha256::digest(&chunk);
    let mut actual = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(actual, "{b:02x}");
    }
    if actual != hash {
        return Err(invalid(format!("object {hash} does not match its hash")));
    }
    s.cache.write_all_at(&chunk, offset)
}

impl Storage for LazyStorage {
    fn size(&self) -> io::Result<u64> {
        Ok(self.shared.cache.metadata()?.len())
    }

    fn get_filename(&self) -> Option<PathBuf> {
        Some(self.shared.image.clone())
    }

    fn resolve_relative_path<P: AsRef<Path>>(&self, relative: P) -> io::Result<PathBuf> {
        let relative = relative.as_ref();
        if relative.is_absolute() {
            return Ok(relative.to_path_buf());
        }
        Ok(self
            .shared
            .image
            .parent()
            .unwrap_or(Path::new("/"))
            .join(relative))
    }

    async unsafe fn pure_readv(&self, mut bufv: IoVectorMut<'_>, offset: u64) -> io::Result<()> {
        let len = bufv.len();
        self.ensure_range(offset, len)?;
        let mut data = vec![0u8; len as usize];
        self.shared.cache.read_exact_at(&mut data, offset)?;
        bufv.copy_from_slice(&data);
        Ok(())
    }

    async unsafe fn pure_writev(&self, bufv: IoVector<'_>, offset: u64) -> io::Result<()> {
        let len = bufv.len();
        self.ensure_range(offset, len)?;
        let mut data = vec![0u8; len as usize];
        bufv.copy_into_slice(&mut data);
        self.shared.cache.write_all_at(&data, offset)
    }

    async fn resize(&self, new_size: u64, _prealloc_mode: PreallocateMode) -> io::Result<()> {
        // Shrinking into the checkpoint's bytes would lose chunks the
        // presence record still claims.
        if new_size < self.shared.size {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "a lazy disk cannot shrink below its checkpoint's size",
            ));
        }
        self.shared.cache.set_len(new_size)
    }

    async fn flush(&self) -> io::Result<()> {
        Ok(())
    }

    async fn sync(&self) -> io::Result<()> {
        self.persist()
    }

    async unsafe fn invalidate_cache(&self) -> io::Result<()> {
        Ok(())
    }

    fn get_storage_helper(&self) -> &CommonStorageHelper {
        &self.helper
    }
}

/// Opens a qcow2 image's backing files lazily when smolvm wrote them a
/// descriptor, and as ordinary files otherwise.
#[derive(Clone, Copy, Debug, Default)]
pub struct LazyGate;

impl ImplicitOpenGate<Box<dyn DynStorage>> for LazyGate {
    async fn open_format<F: FormatDriverBuilder<Box<dyn DynStorage>>>(
        &mut self,
        builder: F,
    ) -> io::Result<FormatAccess<Box<dyn DynStorage>>> {
        let driver = Box::pin(builder.open(LazyGate)).await?;
        Ok(FormatAccess::new(driver))
    }

    async fn open_storage(&mut self, opts: StorageOpenOptions) -> io::Result<Box<dyn DynStorage>> {
        if let Some(descriptor) = opts.get_filename().and_then(descriptor_for) {
            return Ok(Box::new(LazyStorage::open(&descriptor)?));
        }
        <Box<dyn DynStorage> as Storage>::open(opts).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CS: usize = 1 << 20;

    /// A stored checkpoint holding one file of `chunks` (None = zeros), with
    /// its objects, index and a descriptor for it. Returns (descriptor, data).
    fn store(dir: &Path, chunks: &[Option<u8>], tail: usize, extra: &str) -> (PathBuf, Vec<u8>) {
        let objects = dir.join("objects");
        std::fs::create_dir_all(&objects).unwrap();
        let mut data = Vec::new();
        let mut names = Vec::new();
        for (n, c) in chunks.iter().enumerate() {
            let len = if n + 1 == chunks.len() && tail > 0 {
                tail
            } else {
                CS
            };
            let chunk: Vec<u8> = match c {
                None => vec![0; len],
                Some(seed) => (0..len)
                    .map(|i| (i as u8).wrapping_mul(*seed).wrapping_add(n as u8))
                    .collect(),
            };
            if c.is_some() {
                let hash = format!("{:x}", Sha256::digest(&chunk));
                std::fs::write(
                    objects.join(&hash),
                    zstd::bulk::compress(&chunk, 3).unwrap(),
                )
                .unwrap();
                names.push(serde_json::Value::String(hash));
            } else {
                names.push(serde_json::Value::Null);
            }
            data.extend_from_slice(&chunk);
        }
        let index = serde_json::json!({
            "version": 1, "chunk_size": CS, "manifest": {},
            "files": [{"path": "disk.qcow2", "size": data.len(), "mode": 0o644, "chunks": names}],
        });
        std::fs::write(dir.join("checkpoint.json"), index.to_string()).unwrap();
        let descriptor = dir.join(format!("disk.qcow2{DESCRIPTOR_SUFFIX}"));
        std::fs::write(
            &descriptor,
            format!(
                r#"{{"version":1,"index":"{}","file":"disk.qcow2","objects":"{}","cache":"{}","trace_out":"{}"{extra}}}"#,
                dir.join("checkpoint.json").display(),
                objects.display(),
                dir.join("cache").display(),
                dir.join("trace.out").display(),
            ),
        )
        .unwrap();
        (descriptor, data)
    }

    fn read(storage: &LazyStorage, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        storage.ensure_range(offset, len as u64)?;
        let mut out = vec![0u8; len];
        storage.shared.cache.read_exact_at(&mut out, offset)?;
        Ok(out)
    }

    #[test]
    fn reads_match_the_checkpoint_and_fetch_only_what_is_touched() {
        let dir = tempfile::tempdir().unwrap();
        let (descriptor, data) = store(
            dir.path(),
            &[Some(3), None, Some(7), Some(9)],
            4096,
            r#","readahead":0"#,
        );
        let disk = LazyStorage::open(&descriptor).unwrap();
        assert_eq!(disk.size().unwrap(), data.len() as u64);
        // Across a chunk boundary, then a zero chunk, then the short tail.
        let start = CS as u64 - 100;
        assert_eq!(read(&disk, start, 200).unwrap(), data[CS - 100..CS + 100]);
        let tail = data.len() - 10;
        assert_eq!(read(&disk, tail as u64, 10).unwrap(), data[tail..]);
        let state = lock(&disk.shared.state).clone();
        assert_eq!(
            state,
            [PRESENT, PRESENT, MISSING, PRESENT],
            "chunk 2 was never touched"
        );
        drop(disk);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("trace.out")).unwrap(),
            "0\n3\n"
        );
    }

    #[test]
    fn a_corrupt_object_fails_the_read_instead_of_returning_it() {
        let dir = tempfile::tempdir().unwrap();
        let (descriptor, _) = store(dir.path(), &[Some(5)], 0, r#","readahead":0"#);
        let object = std::fs::read_dir(dir.path().join("objects"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        std::fs::write(&object, zstd::bulk::compress(&vec![1u8; CS], 3).unwrap()).unwrap();
        let disk = LazyStorage::open(&descriptor).unwrap();
        let err = read(&disk, 0, 10).unwrap_err();
        assert!(err.to_string().contains("does not match its hash"), "{err}");
    }

    #[test]
    fn a_missing_object_is_retried_until_it_appears() {
        let dir = tempfile::tempdir().unwrap();
        let (descriptor, data) = store(
            dir.path(),
            &[Some(2)],
            0,
            r#","readahead":0,"fetch_deadline_secs":30"#,
        );
        let object = std::fs::read_dir(dir.path().join("objects"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let parked = dir.path().join("parked");
        std::fs::rename(&object, &parked).unwrap();
        let disk = LazyStorage::open(&descriptor).unwrap();
        let restore = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            std::fs::rename(parked, object).unwrap();
        });
        assert_eq!(
            read(&disk, 0, 64).unwrap(),
            data[..64],
            "the read waited for the object"
        );
        restore.join().unwrap();
    }

    #[test]
    fn writes_and_fetched_chunks_survive_a_reopen_without_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let (descriptor, data) = store(dir.path(), &[Some(4), Some(6)], 0, r#","readahead":0"#);
        let disk = LazyStorage::open(&descriptor).unwrap();
        disk.ensure_range(10, 5).unwrap();
        disk.shared.cache.write_all_at(b"hello", 10).unwrap();
        disk.persist().unwrap();
        drop(disk);
        // The store is gone; chunk 0 must come from the cache, with the write.
        std::fs::remove_dir_all(dir.path().join("objects")).unwrap();
        let disk = LazyStorage::open(&descriptor).unwrap();
        let state = lock(&disk.shared.state).clone();
        assert_eq!(state, [PRESENT, MISSING]);
        let mut expected = data[..CS].to_vec();
        expected[10..15].copy_from_slice(b"hello");
        assert_eq!(read(&disk, 0, CS).unwrap(), expected);
    }

    #[test]
    fn a_qcow2_layer_grows_past_its_checkpoint_and_keeps_the_growth() {
        let dir = tempfile::tempdir().unwrap();
        // A short last chunk, as a qcow2 file's size rarely is a whole chunk.
        let (descriptor, data) = store(dir.path(), &[Some(3), Some(5)], 100, r#","readahead":0"#);
        let disk = LazyStorage::open(&descriptor).unwrap();
        let end = data.len() as u64;
        assert_eq!(Storage::size(&disk).unwrap(), end);
        // A write straddling the old end, then one well past it: the guest
        // allocating clusters.
        disk.ensure_range(end - 10, 20).unwrap();
        disk.shared.cache.write_all_at(&[7; 20], end - 10).unwrap();
        disk.ensure_range(end + 5 * CS as u64, 4).unwrap();
        disk.shared
            .cache
            .write_all_at(b"tail", end + 5 * CS as u64)
            .unwrap();
        assert_eq!(Storage::size(&disk).unwrap(), end + 5 * CS as u64 + 4);
        disk.persist().unwrap();
        drop(disk);

        let disk = LazyStorage::open(&descriptor).unwrap();
        assert_eq!(Storage::size(&disk).unwrap(), end + 5 * CS as u64 + 4);
        assert_eq!(read(&disk, end - 10, 20).unwrap(), [7; 20]);
        assert_eq!(read(&disk, end + 5 * CS as u64, 4).unwrap(), b"tail");
        assert_eq!(read(&disk, 0, 16).unwrap(), data[..16]);
    }

    #[test]
    fn a_missing_object_of_a_failed_import_fails_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let (descriptor, _) = store(dir.path(), &[Some(7)], 0, r#","readahead":0"#);
        std::fs::remove_dir_all(dir.path().join("objects")).unwrap();
        std::fs::create_dir(dir.path().join("objects")).unwrap();
        std::fs::write(dir.path().join(".import-failed"), b"1\n").unwrap();
        let disk = LazyStorage::open(&descriptor).unwrap();
        let started = Instant::now();
        assert!(read(&disk, 0, 16).is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn a_disk_smolvm_is_completing_cannot_be_opened() {
        let dir = tempfile::tempdir().unwrap();
        let (descriptor, _) = store(dir.path(), &[Some(1)], 0, "");
        let held = File::open(&descriptor).unwrap();
        held.lock().unwrap();
        let error = LazyStorage::open(&descriptor).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        held.unlock().unwrap();
        let disk = LazyStorage::open(&descriptor).unwrap();
        // While the VMM serves it, smolvm cannot take it over.
        assert!(File::open(&descriptor).unwrap().try_lock().is_err());
        drop(disk);
    }

    #[test]
    fn a_recorded_trace_is_prefetched_and_fill_completes_the_disk() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("trace"), "2\n0\n").unwrap();
        let extra = format!(
            r#","trace":"{}","fill":true"#,
            dir.path().join("trace").display()
        );
        let (descriptor, _) = store(dir.path(), &[Some(1), Some(2), Some(3), Some(4)], 0, &extra);
        let disk = LazyStorage::open(&descriptor).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        // The record says so while the disk is still open: smolvm reads it
        // to know a running machine's disk is complete.
        while std::fs::read(&disk.shared.present_path).map_or(true, |p| p != [1, 1, 1, 1]) {
            assert!(
                Instant::now() < deadline,
                "the disk never recorded completion"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(lock(&disk.shared.state).iter().all(|s| *s == PRESENT));
    }
}
