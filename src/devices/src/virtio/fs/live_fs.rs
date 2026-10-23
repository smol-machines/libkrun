// Live shares for virtio-fs.
//
// A live virtio-fs device starts empty and serves host directories that are
// attached and detached while the VM runs, so a host directory can be shared
// with a running guest without restarting it. Each share appears as a child of
// the device root under the name it was attached with; the guest bind-mounts it
// wherever it is wanted.
//
// `LiveFs<T>` wraps the passthrough filesystem serving the device. The device's
// own root is an empty host directory that is never written: a lookup in it only
// resolves attached share names, each through the passthrough's absolute
// host-path lookup. Every inode reached below a share is recorded as belonging
// to it, and every operation on an inode checks that its share is still
// attached, so detaching a share revokes the guest's access to everything under
// it at once, including files it already holds open.
//
// The read-only variant wraps `PassthroughFsRo`, so a read-only share is
// enforced by the host and not by a guest mount flag. Live devices have no DAX
// window: a mapping into guest memory could not be revoked on detach.

#[cfg(any(target_os = "macos", target_os = "windows"))]
use crossbeam_channel::Sender;
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicI32;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

#[cfg(any(target_os = "macos", target_os = "windows"))]
use utils::worker_message::WorkerMessage;

use super::device::{FuseServerState, LiveShareSnap};
use super::filesystem::{
    Context, DirEntry, Entry, Extensions, FileSystem, FsOptions, GetxattrReply, ListxattrReply,
    OpenOptions, SetattrValid, ZeroCopyReader, ZeroCopyWriter,
};
use super::fuse;
use super::passthrough::PassthroughFs;
use super::read_only::PassthroughFsRo;
use crate::virtio::bindings;
use crate::virtio::linux_errno;

type Inode = u64;
type Handle = u64;

/// Most shares one live device serves at once.
pub const MAX_LIVE_SHARES: usize = 64;

/// The passthrough filesystems a live device can wrap.
pub trait LiveBackend: FileSystem<Inode = Inode, Handle = Handle> {
    fn lookup_host_path(&self, path: &Path) -> io::Result<Entry>;
    fn host_path_of(&self, inode: Inode) -> Option<PathBuf>;
    fn snapshot(&self) -> FuseServerState;
    fn restore_with_roots(
        &self,
        state: &FuseServerState,
        share_roots: &HashMap<u64, PathBuf>,
    ) -> io::Result<()>;
}

impl LiveBackend for PassthroughFs {
    fn lookup_host_path(&self, path: &Path) -> io::Result<Entry> {
        PassthroughFs::lookup_host_path(self, path)
    }
    fn host_path_of(&self, inode: Inode) -> Option<PathBuf> {
        PassthroughFs::host_path_of(self, inode)
    }
    fn snapshot(&self) -> FuseServerState {
        PassthroughFs::snapshot(self)
    }
    fn restore_with_roots(
        &self,
        state: &FuseServerState,
        share_roots: &HashMap<u64, PathBuf>,
    ) -> io::Result<()> {
        PassthroughFs::restore_with_roots(self, state, share_roots)
    }
}

impl LiveBackend for PassthroughFsRo {
    fn lookup_host_path(&self, path: &Path) -> io::Result<Entry> {
        self.inner().lookup_host_path(path)
    }
    fn host_path_of(&self, inode: Inode) -> Option<PathBuf> {
        self.inner().host_path_of(inode)
    }
    fn snapshot(&self) -> FuseServerState {
        self.inner().snapshot()
    }
    fn restore_with_roots(
        &self,
        state: &FuseServerState,
        share_roots: &HashMap<u64, PathBuf>,
    ) -> io::Result<()> {
        self.inner().restore_with_roots(state, share_roots)
    }
}

struct Share {
    name: CString,
    /// Canonical host directory.
    host_path: PathBuf,
}

#[derive(Default)]
struct ShareTable {
    /// Ids are never reused, so an inode of a detached share stays revoked even
    /// if a share of the same name is attached again.
    next_id: u64,
    by_name: HashMap<CString, u64>,
    shares: HashMap<u64, Share>,
}

/// The shares attached to one live device. Shared between the device's FUSE
/// server and the control socket, which attaches and detaches them.
#[derive(Default)]
pub struct LiveShares {
    table: RwLock<ShareTable>,
}

fn valid_share_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

impl LiveShares {
    /// Attach `host_path` under `name`. The path must be an existing directory;
    /// it may not be, contain or lie inside a directory already shared, so every
    /// host inode belongs to at most one share.
    pub fn attach(&self, name: &str, host_path: &Path) -> Result<u64, String> {
        if !valid_share_name(name) {
            return Err(format!(
                "invalid share name {name:?}: use 1-64 of A-Z a-z 0-9 . _ -"
            ));
        }
        if !host_path.is_absolute() {
            return Err(format!(
                "share path {} is not absolute",
                host_path.display()
            ));
        }
        let host_path = std::fs::canonicalize(host_path)
            .map_err(|e| format!("share path {}: {e}", host_path.display()))?;
        if !host_path.is_dir() {
            return Err(format!(
                "share path {} is not a directory",
                host_path.display()
            ));
        }
        let c_name = CString::new(name).map_err(|_| format!("invalid share name {name:?}"))?;
        let mut table = self.table.write().unwrap();
        if table.by_name.contains_key(&c_name) {
            return Err(format!("a share named {name} is already attached"));
        }
        if table.shares.len() >= MAX_LIVE_SHARES {
            return Err(format!("at most {MAX_LIVE_SHARES} shares can be attached"));
        }
        if let Some(other) = table
            .shares
            .values()
            .find(|s| s.host_path.starts_with(&host_path) || host_path.starts_with(&s.host_path))
        {
            return Err(format!(
                "{} overlaps share {} ({})",
                host_path.display(),
                other.name.to_string_lossy(),
                other.host_path.display()
            ));
        }
        let id = table.next_id;
        table.next_id += 1;
        table.by_name.insert(c_name.clone(), id);
        table.shares.insert(
            id,
            Share {
                name: c_name,
                host_path,
            },
        );
        Ok(id)
    }

    /// Detach the share named `name`, revoking access to everything under it.
    pub fn detach(&self, name: &str) -> Result<(), String> {
        let c_name = CString::new(name).map_err(|_| format!("invalid share name {name:?}"))?;
        let mut table = self.table.write().unwrap();
        let id = table
            .by_name
            .remove(&c_name)
            .ok_or_else(|| format!("no share named {name} is attached"))?;
        table.shares.remove(&id);
        Ok(())
    }

    /// Attached shares as `(name, host path)`, sorted by name.
    pub fn list(&self) -> Vec<(String, PathBuf)> {
        let table = self.table.read().unwrap();
        let mut shares: Vec<_> = table
            .shares
            .values()
            .map(|s| (s.name.to_string_lossy().into_owned(), s.host_path.clone()))
            .collect();
        shares.sort();
        shares
    }

    fn by_name(&self, name: &CStr) -> Option<(u64, PathBuf)> {
        let table = self.table.read().unwrap();
        let id = *table.by_name.get(name)?;
        Some((id, table.shares.get(&id)?.host_path.clone()))
    }

    fn is_attached(&self, id: u64) -> bool {
        self.table.read().unwrap().shares.contains_key(&id)
    }

    fn snapshot(&self) -> Vec<LiveShareSnap> {
        let table = self.table.read().unwrap();
        let mut shares: Vec<_> = table
            .shares
            .iter()
            .map(|(id, s)| LiveShareSnap {
                id: *id,
                name: s.name.to_string_lossy().into_owned(),
                host_path: s.host_path.to_string_lossy().into_owned(),
            })
            .collect();
        shares.sort_by_key(|s| s.id);
        shares
    }

    /// Re-attach the shares of a snapshot under their original ids, so the
    /// inodes the guest already holds keep resolving.
    fn restore(&self, shares: &[LiveShareSnap]) -> HashMap<u64, PathBuf> {
        let mut table = self.table.write().unwrap();
        let mut roots = HashMap::new();
        for snap in shares {
            let Ok(name) = CString::new(snap.name.as_str()) else {
                continue;
            };
            let host_path = PathBuf::from(&snap.host_path);
            table.next_id = table.next_id.max(snap.id + 1);
            table.by_name.insert(name.clone(), snap.id);
            table.shares.insert(
                snap.id,
                Share {
                    name,
                    host_path: host_path.clone(),
                },
            );
            roots.insert(snap.id, host_path);
        }
        roots
    }
}

/// Live devices of this process by virtio-fs tag, for the control socket.
fn registry() -> &'static Mutex<HashMap<String, Arc<LiveShares>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Arc<LiveShares>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The shares of the live device tagged `tag`, if this process has one.
pub fn live_shares(tag: &str) -> Option<Arc<LiveShares>> {
    registry().lock().unwrap().get(tag).cloned()
}

/// Tags of this process's live devices.
pub fn live_tags() -> Vec<String> {
    let mut tags: Vec<_> = registry().lock().unwrap().keys().cloned().collect();
    tags.sort();
    tags
}

/// The filesystem a live virtio-fs device serves.
pub struct LiveFs<T> {
    inner: T,
    shares: Arc<LiveShares>,
    /// The share each inode below a share root was reached through.
    members: RwLock<HashMap<Inode, u64>>,
}

impl<T: LiveBackend> LiveFs<T> {
    /// Serve `inner` as the live device `tag`, registering its shares with the
    /// control socket of this process.
    pub fn new(inner: T, tag: &str) -> Self {
        let shares = Arc::new(LiveShares::default());
        registry()
            .lock()
            .unwrap()
            .insert(tag.to_owned(), shares.clone());
        Self {
            inner,
            shares,
            members: RwLock::new(HashMap::new()),
        }
    }

    pub fn inner(&self) -> &T {
        &self.inner
    }

    /// Snapshot the device: the passthrough state, with every inode reached
    /// through a share recorded relative to that share's host directory, and the
    /// shares themselves. Inodes of a detached share are left out.
    pub fn snapshot(&self) -> FuseServerState {
        let mut state = self.inner.snapshot();
        let members = self.members.read().unwrap();
        let shares = self.shares.snapshot();
        let roots: HashMap<u64, &str> = shares
            .iter()
            .map(|s| (s.id, s.host_path.as_str()))
            .collect();
        state.inodes.retain_mut(|snap| {
            let Some(id) = members.get(&snap.nodeid) else {
                return true;
            };
            let Some(root) = roots.get(id) else {
                return false;
            };
            let Some(path) = self.inner.host_path_of(snap.nodeid) else {
                return false;
            };
            match path.strip_prefix(root) {
                Ok(relative) => {
                    snap.share = Some(*id);
                    snap.relative_path = Some(relative.to_string_lossy().into_owned());
                    true
                }
                Err(_) => false,
            }
        });
        state.live_shares = shares;
        state
    }

    /// Rebuild a device from [`Self::snapshot`].
    pub fn restore(&self, state: &FuseServerState) -> io::Result<()> {
        let roots = self.shares.restore(&state.live_shares);
        {
            let mut members = self.members.write().unwrap();
            for snap in &state.inodes {
                if let Some(id) = snap.share {
                    members.insert(snap.nodeid, id);
                }
            }
        }
        self.inner.restore_with_roots(state, &roots)
    }

    /// Fail unless `inode` is the device root or below a share still attached.
    fn check(&self, inode: Inode) -> io::Result<()> {
        if inode == fuse::ROOT_ID {
            return Ok(());
        }
        match self.members.read().unwrap().get(&inode) {
            Some(id) if self.shares.is_attached(*id) => Ok(()),
            _ => Err(linux_errno::enoent()),
        }
    }

    /// The share `inode` belongs to, failing for the root (nothing is created
    /// in the device root) and for a detached share.
    fn share_of(&self, inode: Inode) -> io::Result<u64> {
        if inode == fuse::ROOT_ID {
            return Err(linux_errno::eacces());
        }
        match self.members.read().unwrap().get(&inode) {
            Some(id) if self.shares.is_attached(*id) => Ok(*id),
            _ => Err(linux_errno::enoent()),
        }
    }

    fn record(&self, share: u64, entry: &Entry) {
        if entry.inode != 0 {
            self.members.write().unwrap().insert(entry.inode, share);
        }
    }
}

impl<T: LiveBackend> FileSystem for LiveFs<T> {
    type Inode = Inode;
    type Handle = Handle;

    fn init(&self, capable: FsOptions) -> io::Result<FsOptions> {
        self.inner.init(capable)
    }

    fn destroy(&self) {
        self.inner.destroy()
    }

    fn lookup(&self, ctx: Context, parent: Inode, name: &CStr) -> io::Result<Entry> {
        if parent == fuse::ROOT_ID {
            let (id, host_path) = self.shares.by_name(name).ok_or_else(linux_errno::enoent)?;
            let mut entry = self.inner.lookup_host_path(&host_path)?;
            self.record(id, &entry);
            // The guest asks again on every use, so a detached share disappears
            // from the device root at once.
            entry.entry_timeout = Duration::ZERO;
            entry.attr_timeout = Duration::ZERO;
            return Ok(entry);
        }
        // `..` from a share root would name the host directory above it.
        if name.to_bytes() == b".." || name.to_bytes() == b"." {
            return Err(linux_errno::eacces());
        }
        let share = self.share_of(parent)?;
        let entry = self.inner.lookup(ctx, parent, name)?;
        self.record(share, &entry);
        Ok(entry)
    }

    fn forget(&self, ctx: Context, inode: Inode, count: u64) {
        self.inner.forget(ctx, inode, count)
    }

    fn batch_forget(&self, ctx: Context, requests: Vec<(Inode, u64)>) {
        self.inner.batch_forget(ctx, requests)
    }

    fn getattr(
        &self,
        ctx: Context,
        inode: Inode,
        handle: Option<Handle>,
    ) -> io::Result<(bindings::stat64, Duration)> {
        self.check(inode)?;
        self.inner.getattr(ctx, inode, handle)
    }

    fn setattr(
        &self,
        ctx: Context,
        inode: Inode,
        attr: bindings::stat64,
        handle: Option<Handle>,
        valid: SetattrValid,
    ) -> io::Result<(bindings::stat64, Duration)> {
        self.share_of(inode)?;
        self.inner.setattr(ctx, inode, attr, handle, valid)
    }

    fn readlink(&self, ctx: Context, inode: Inode) -> io::Result<Vec<u8>> {
        self.check(inode)?;
        self.inner.readlink(ctx, inode)
    }

    fn symlink(
        &self,
        ctx: Context,
        linkname: &CStr,
        parent: Inode,
        name: &CStr,
        extensions: Extensions,
    ) -> io::Result<Entry> {
        let share = self.share_of(parent)?;
        let entry = self
            .inner
            .symlink(ctx, linkname, parent, name, extensions)?;
        self.record(share, &entry);
        Ok(entry)
    }

    fn mknod(
        &self,
        ctx: Context,
        inode: Inode,
        name: &CStr,
        mode: u32,
        rdev: u32,
        umask: u32,
        extensions: Extensions,
    ) -> io::Result<Entry> {
        let share = self.share_of(inode)?;
        let entry = self
            .inner
            .mknod(ctx, inode, name, mode, rdev, umask, extensions)?;
        self.record(share, &entry);
        Ok(entry)
    }

    fn mkdir(
        &self,
        ctx: Context,
        parent: Inode,
        name: &CStr,
        mode: u32,
        umask: u32,
        extensions: Extensions,
    ) -> io::Result<Entry> {
        let share = self.share_of(parent)?;
        let entry = self
            .inner
            .mkdir(ctx, parent, name, mode, umask, extensions)?;
        self.record(share, &entry);
        Ok(entry)
    }

    fn unlink(&self, ctx: Context, parent: Inode, name: &CStr) -> io::Result<()> {
        self.share_of(parent)?;
        self.inner.unlink(ctx, parent, name)
    }

    fn rmdir(&self, ctx: Context, parent: Inode, name: &CStr) -> io::Result<()> {
        self.share_of(parent)?;
        self.inner.rmdir(ctx, parent, name)
    }

    fn rename(
        &self,
        ctx: Context,
        olddir: Inode,
        oldname: &CStr,
        newdir: Inode,
        newname: &CStr,
        flags: u32,
    ) -> io::Result<()> {
        if self.share_of(olddir)? != self.share_of(newdir)? {
            return Err(linux_errno::exdev());
        }
        self.inner
            .rename(ctx, olddir, oldname, newdir, newname, flags)
    }

    fn link(
        &self,
        ctx: Context,
        inode: Inode,
        newparent: Inode,
        newname: &CStr,
    ) -> io::Result<Entry> {
        let share = self.share_of(newparent)?;
        if self.share_of(inode)? != share {
            return Err(linux_errno::exdev());
        }
        let entry = self.inner.link(ctx, inode, newparent, newname)?;
        self.record(share, &entry);
        Ok(entry)
    }

    fn open(
        &self,
        ctx: Context,
        inode: Inode,
        kill_priv: bool,
        flags: u32,
    ) -> io::Result<(Option<Handle>, OpenOptions)> {
        self.check(inode)?;
        self.inner.open(ctx, inode, kill_priv, flags)
    }

    fn create(
        &self,
        ctx: Context,
        parent: Inode,
        name: &CStr,
        mode: u32,
        kill_priv: bool,
        flags: u32,
        umask: u32,
        extensions: Extensions,
    ) -> io::Result<(Entry, Option<Handle>, OpenOptions)> {
        let share = self.share_of(parent)?;
        let created = self
            .inner
            .create(ctx, parent, name, mode, kill_priv, flags, umask, extensions)?;
        self.record(share, &created.0);
        Ok(created)
    }

    fn read<W: io::Write + ZeroCopyWriter>(
        &self,
        ctx: Context,
        inode: Inode,
        handle: Handle,
        w: W,
        size: u32,
        offset: u64,
        lock_owner: Option<u64>,
        flags: u32,
    ) -> io::Result<usize> {
        self.check(inode)?;
        self.inner
            .read(ctx, inode, handle, w, size, offset, lock_owner, flags)
    }

    fn write<R: io::Read + ZeroCopyReader>(
        &self,
        ctx: Context,
        inode: Inode,
        handle: Handle,
        r: R,
        size: u32,
        offset: u64,
        lock_owner: Option<u64>,
        delayed_write: bool,
        kill_priv: bool,
        flags: u32,
    ) -> io::Result<usize> {
        self.check(inode)?;
        self.inner.write(
            ctx,
            inode,
            handle,
            r,
            size,
            offset,
            lock_owner,
            delayed_write,
            kill_priv,
            flags,
        )
    }

    fn flush(&self, ctx: Context, inode: Inode, handle: Handle, lock_owner: u64) -> io::Result<()> {
        self.check(inode)?;
        self.inner.flush(ctx, inode, handle, lock_owner)
    }

    fn fsync(&self, ctx: Context, inode: Inode, datasync: bool, handle: Handle) -> io::Result<()> {
        self.check(inode)?;
        self.inner.fsync(ctx, inode, datasync, handle)
    }

    fn fallocate(
        &self,
        ctx: Context,
        inode: Inode,
        handle: Handle,
        mode: u32,
        offset: u64,
        length: u64,
    ) -> io::Result<()> {
        self.check(inode)?;
        self.inner
            .fallocate(ctx, inode, handle, mode, offset, length)
    }

    fn release(
        &self,
        ctx: Context,
        inode: Inode,
        flags: u32,
        handle: Handle,
        flush: bool,
        flock_release: bool,
        lock_owner: Option<u64>,
    ) -> io::Result<()> {
        // Always let the guest give a handle back, even for a detached share.
        self.inner
            .release(ctx, inode, flags, handle, flush, flock_release, lock_owner)
    }

    fn statfs(&self, ctx: Context, inode: Inode) -> io::Result<bindings::statvfs64> {
        self.check(inode)?;
        self.inner.statfs(ctx, inode)
    }

    fn getxattr(
        &self,
        ctx: Context,
        inode: Inode,
        name: &CStr,
        size: u32,
    ) -> io::Result<GetxattrReply> {
        self.check(inode)?;
        self.inner.getxattr(ctx, inode, name, size)
    }

    fn listxattr(&self, ctx: Context, inode: Inode, size: u32) -> io::Result<ListxattrReply> {
        self.check(inode)?;
        self.inner.listxattr(ctx, inode, size)
    }

    fn setxattr(
        &self,
        ctx: Context,
        inode: Inode,
        name: &CStr,
        value: &[u8],
        flags: u32,
    ) -> io::Result<()> {
        self.share_of(inode)?;
        self.inner.setxattr(ctx, inode, name, value, flags)
    }

    fn removexattr(&self, ctx: Context, inode: Inode, name: &CStr) -> io::Result<()> {
        self.share_of(inode)?;
        self.inner.removexattr(ctx, inode, name)
    }

    fn opendir(
        &self,
        ctx: Context,
        inode: Inode,
        flags: u32,
    ) -> io::Result<(Option<Handle>, OpenOptions)> {
        self.check(inode)?;
        self.inner.opendir(ctx, inode, flags)
    }

    fn readdir<F>(
        &self,
        ctx: Context,
        inode: Inode,
        handle: Handle,
        size: u32,
        offset: u64,
        add_entry: F,
    ) -> io::Result<()>
    where
        F: FnMut(DirEntry) -> io::Result<usize>,
    {
        self.check(inode)?;
        self.inner
            .readdir(ctx, inode, handle, size, offset, add_entry)
    }

    fn readdirplus<F>(
        &self,
        ctx: Context,
        inode: Inode,
        handle: Handle,
        size: u32,
        offset: u64,
        mut add_entry: F,
    ) -> io::Result<()>
    where
        F: FnMut(DirEntry, Entry) -> io::Result<usize>,
    {
        if inode == fuse::ROOT_ID {
            // The device root lists empty: shares are looked up by name, and the
            // empty host directory under it is never handed out.
            return Ok(());
        }
        let share = self.share_of(inode)?;
        self.inner
            .readdirplus(ctx, inode, handle, size, offset, |dir, entry| {
                self.record(share, &entry);
                add_entry(dir, entry)
            })
    }

    fn fsyncdir(
        &self,
        ctx: Context,
        inode: Inode,
        datasync: bool,
        handle: Handle,
    ) -> io::Result<()> {
        self.check(inode)?;
        self.inner.fsyncdir(ctx, inode, datasync, handle)
    }

    fn releasedir(&self, ctx: Context, inode: Inode, flags: u32, handle: Handle) -> io::Result<()> {
        self.inner.releasedir(ctx, inode, flags, handle)
    }

    fn access(&self, ctx: Context, inode: Inode, mask: u32) -> io::Result<()> {
        self.check(inode)?;
        self.inner.access(ctx, inode, mask)
    }

    fn lseek(
        &self,
        ctx: Context,
        inode: Inode,
        handle: Handle,
        offset: u64,
        whence: u32,
    ) -> io::Result<u64> {
        self.check(inode)?;
        self.inner.lseek(ctx, inode, handle, offset, whence)
    }

    fn copyfilerange(
        &self,
        ctx: Context,
        inode_in: Inode,
        handle_in: Handle,
        offset_in: u64,
        inode_out: Inode,
        handle_out: Handle,
        offset_out: u64,
        len: u64,
        flags: u64,
    ) -> io::Result<usize> {
        self.check(inode_in)?;
        self.share_of(inode_out)?;
        self.inner.copyfilerange(
            ctx, inode_in, handle_in, offset_in, inode_out, handle_out, offset_out, len, flags,
        )
    }

    fn setupmapping(
        &self,
        _ctx: Context,
        _inode: Inode,
        _handle: Handle,
        _foffset: u64,
        _len: u64,
        _flags: u64,
        _moffset: u64,
        _host_shm_base: u64,
        _shm_size: u64,
        #[cfg(any(target_os = "macos", target_os = "windows"))] _map_sender: &Option<
            Sender<WorkerMessage>,
        >,
    ) -> io::Result<()> {
        // A mapping into guest memory could not be taken back on detach.
        Err(linux_errno::enosys())
    }

    fn ioctl(
        &self,
        ctx: Context,
        inode: Inode,
        handle: Handle,
        flags: u32,
        cmd: u32,
        arg: u64,
        in_size: u32,
        out_size: u32,
        exit_code: &Arc<AtomicI32>,
    ) -> io::Result<Vec<u8>> {
        self.check(inode)?;
        self.inner.ioctl(
            ctx, inode, handle, flags, cmd, arg, in_size, out_size, exit_code,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtio::fs::inode_alloc::InodeAllocator;
    use crate::virtio::fs::passthrough::Config;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    const CTX: Context = Context {
        uid: 0,
        gid: 0,
        pid: 0,
    };

    struct Dirs {
        base: PathBuf,
        root: PathBuf,
        a: PathBuf,
        b: PathBuf,
    }

    impl Drop for Dirs {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    fn dirs(test: &str) -> Dirs {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!(
            "libkrun-live-{test}-{}-{nonce}",
            std::process::id()
        ));
        let root = base.join("root");
        let a = base.join("a");
        let b = base.join("b");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(a.join("sub")).unwrap();
        fs::write(a.join("a.txt"), b"alpha").unwrap();
        fs::create_dir_all(&b).unwrap();
        Dirs {
            base: fs::canonicalize(&base).unwrap(),
            root: fs::canonicalize(&root).unwrap(),
            a: fs::canonicalize(&a).unwrap(),
            b: fs::canonicalize(&b).unwrap(),
        }
    }

    fn live(dirs: &Dirs, tag: &str) -> LiveFs<PassthroughFs> {
        let inner = PassthroughFs::new(
            Config {
                root_dir: dirs.root.to_string_lossy().into_owned(),
                ..Default::default()
            },
            Arc::new(InodeAllocator::new()),
        )
        .unwrap();
        let fs = LiveFs::new(inner, tag);
        fs.init(FsOptions::empty()).unwrap();
        fs
    }

    fn name(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    fn errno<T>(result: io::Result<T>) -> Option<i32> {
        match result {
            Ok(_) => panic!("expected an error"),
            Err(e) => e.raw_os_error(),
        }
    }

    #[test]
    fn attach_refuses_names_and_paths_it_cannot_serve_safely() {
        let d = dirs("attach");
        let shares = LiveShares::default();
        assert!(shares.attach("", &d.a).is_err());
        assert!(shares.attach("..", &d.a).is_err());
        assert!(shares.attach("a/b", &d.a).is_err());
        assert!(shares.attach("a", Path::new("relative/dir")).is_err());
        assert!(shares.attach("a", &d.base.join("missing")).is_err());
        assert!(
            shares.attach("a", &d.a.join("a.txt")).is_err(),
            "a file is not a share"
        );
        shares.attach("a", &d.a).unwrap();
        assert!(shares.attach("a", &d.b).is_err(), "names are unique");
        assert!(
            shares.attach("inner", &d.a.join("sub")).is_err(),
            "a share may not lie inside another"
        );
        assert!(shares.attach("outer", &d.base).is_err(), "nor contain one");
        shares.attach("b", &d.b).unwrap();
        assert_eq!(
            shares.list(),
            vec![("a".to_owned(), d.a.clone()), ("b".to_owned(), d.b.clone())]
        );
    }

    #[test]
    fn a_share_serves_its_tree_until_it_is_detached() {
        let d = dirs("detach");
        let fs = live(&d, "live-detach");
        assert_eq!(
            errno(fs.lookup(CTX, fuse::ROOT_ID, &name("a"))),
            linux_errno::enoent().raw_os_error()
        );

        live_shares("live-detach")
            .unwrap()
            .attach("a", &d.a)
            .unwrap();
        let share = fs.lookup(CTX, fuse::ROOT_ID, &name("a")).unwrap();
        assert_eq!(
            share.entry_timeout,
            Duration::ZERO,
            "the guest re-checks the share on every use"
        );
        let file = fs.lookup(CTX, share.inode, &name("a.txt")).unwrap();
        assert_eq!(fs.getattr(CTX, file.inode, None).unwrap().0.st_size, 5);

        live_shares("live-detach").unwrap().detach("a").unwrap();
        assert_eq!(
            errno(fs.getattr(CTX, file.inode, None)),
            linux_errno::enoent().raw_os_error()
        );
        assert_eq!(
            errno(fs.lookup(CTX, share.inode, &name("a.txt"))),
            linux_errno::enoent().raw_os_error()
        );
        assert_eq!(
            errno(fs.lookup(CTX, fuse::ROOT_ID, &name("a"))),
            linux_errno::enoent().raw_os_error()
        );

        // Attaching the same name again does not bring the old inodes back.
        live_shares("live-detach")
            .unwrap()
            .attach("a", &d.a)
            .unwrap();
        assert!(fs.getattr(CTX, file.inode, None).is_err());
        let again = fs.lookup(CTX, fuse::ROOT_ID, &name("a")).unwrap();
        let file = fs.lookup(CTX, again.inode, &name("a.txt")).unwrap();
        assert!(fs.getattr(CTX, file.inode, None).is_ok());
    }

    #[test]
    fn nothing_escapes_a_share_or_lands_in_the_device_root() {
        let d = dirs("escape");
        let fs = live(&d, "live-escape");
        let shares = live_shares("live-escape").unwrap();
        shares.attach("a", &d.a).unwrap();
        shares.attach("b", &d.b).unwrap();
        let a = fs.lookup(CTX, fuse::ROOT_ID, &name("a")).unwrap();
        let b = fs.lookup(CTX, fuse::ROOT_ID, &name("b")).unwrap();

        assert_eq!(
            errno(fs.lookup(CTX, a.inode, &name(".."))),
            linux_errno::eacces().raw_os_error()
        );
        assert_eq!(
            errno(fs.mkdir(
                CTX,
                fuse::ROOT_ID,
                &name("x"),
                0o755,
                0,
                Extensions::default()
            )),
            linux_errno::eacces().raw_os_error()
        );
        assert!(
            fs::read_dir(&d.root).unwrap().next().is_none(),
            "the device root stays empty"
        );
        assert_eq!(
            errno(fs.rename(CTX, a.inode, &name("a.txt"), b.inode, &name("a.txt"), 0)),
            linux_errno::exdev().raw_os_error()
        );
        assert!(d.a.join("a.txt").exists());
    }

    #[test]
    fn a_read_only_device_refuses_writes_in_its_shares() {
        let d = dirs("ro");
        let inner = PassthroughFsRo::new(
            Config {
                root_dir: d.root.to_string_lossy().into_owned(),
                ..Default::default()
            },
            Arc::new(InodeAllocator::new()),
        )
        .unwrap();
        let fs = LiveFs::new(inner, "live-ro");
        fs.init(FsOptions::empty()).unwrap();
        live_shares("live-ro").unwrap().attach("a", &d.a).unwrap();
        let a = fs.lookup(CTX, fuse::ROOT_ID, &name("a")).unwrap();
        assert!(
            fs.mkdir(CTX, a.inode, &name("new"), 0o755, 0, Extensions::default())
                .is_err()
        );
        assert!(!d.a.join("new").exists());
        assert!(fs.lookup(CTX, a.inode, &name("a.txt")).is_ok());
    }

    #[test]
    fn a_restored_device_serves_the_same_shares_and_inodes() {
        let d = dirs("restore");
        let fs = live(&d, "live-snap");
        let shares = live_shares("live-snap").unwrap();
        shares.attach("a", &d.a).unwrap();
        shares.attach("b", &d.b).unwrap();
        let a = fs.lookup(CTX, fuse::ROOT_ID, &name("a")).unwrap();
        let file = fs.lookup(CTX, a.inode, &name("a.txt")).unwrap();
        let b = fs.lookup(CTX, fuse::ROOT_ID, &name("b")).unwrap();
        shares.detach("b").unwrap();

        let state = fs.snapshot();
        assert_eq!(state.live_shares.len(), 1);
        assert!(
            state.inodes.iter().all(|s| s.nodeid != b.inode),
            "inodes of a detached share are not carried over"
        );

        let restored = live(&d, "live-snap-clone");
        restored.restore(&state).unwrap();
        assert_eq!(
            restored.getattr(CTX, file.inode, None).unwrap().0.st_size,
            5
        );
        assert!(restored.lookup(CTX, fuse::ROOT_ID, &name("a")).is_ok());
        assert!(restored.lookup(CTX, fuse::ROOT_ID, &name("b")).is_err());
        assert_eq!(
            live_shares("live-snap-clone").unwrap().list(),
            vec![("a".to_owned(), d.a.clone())]
        );
    }
}
