// Copyright 2026. SPDX-License-Identifier: Apache-2.0
//
//! Guest-memory checkpoint: serialize/restore guest RAM to a byte stream.
//!
//! Platform-neutral by design — it operates only on the `GuestMemoryMmap` host
//! mapping and `std::io`, so the same code serves both the KVM (Linux) and HVF
//! (macOS) snapshot paths. The CoW-clone fast path (Linux `memfd` /
//! macOS `vm_remap(copy=TRUE)`) layers on top of these same region descriptors;
//! this eager byte-copy path is the correctness baseline validated in
//! `experiments/fork-poc` (Experiment 2: persist -> release -> restore,
//! bit-identical continuation).
//!
//! A full VM checkpoint composes three parts: this guest-memory image, the
//! paused-vCPU register state (`vstate` save_state), and the virtio device
//! state (`devices::virtio::persist`). See [`SnapshotManifest`] for the layout.

use std::io::{self, Read, Write};

use vm_memory::{Address, GuestAddress, GuestMemory, GuestMemoryRegion};

use crate::GuestMemoryMmap;

/// Magic at the head of a snapshot manifest: "SMOLSNAP".
pub const SNAPSHOT_MAGIC: u64 = 0x534d4f4c534e4150;
/// On-disk snapshot format version. Bumped on any layout change (no backward
/// compatibility is maintained — alpha project).
pub const SNAPSHOT_VERSION: u32 = 1;

/// Describes one guest-RAM region in a memory snapshot: where it maps in guest
/// physical address space and how many bytes it holds. The region bytes follow
/// in the memory stream in region order; the descriptors carry the lengths, so
/// the byte stream itself needs no per-region framing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryRegionDesc {
    /// Guest physical base address of the region.
    pub gpa: u64,
    /// Region length in bytes.
    pub len: u64,
}

/// Serialize all guest-memory regions to `out`, returning the region layout the
/// restore side needs (each region's guest address + length). Bytes are written
/// in region order with no per-region framing.
///
/// The caller must have paused the vCPUs (and drained device workers) first so
/// the bytes are captured at a stable, consistent boundary.
pub fn write_guest_memory<W: Write>(
    mem: &GuestMemoryMmap,
    out: &mut W,
) -> io::Result<Vec<MemoryRegionDesc>> {
    let mut descs = Vec::new();
    for region in mem.iter() {
        let gpa = region.start_addr();
        let len = region.len();
        let host = mem
            .get_host_address(gpa)
            .map_err(|e| io::Error::other(format!("get_host_address: {e:?}")))?;
        // Safety: `host` points to `len` bytes of live guest RAM owned by the
        // mmap region currently being iterated. The VM is paused, so the bytes
        // are stable for the duration of the copy.
        let bytes = unsafe { std::slice::from_raw_parts(host as *const u8, len as usize) };
        out.write_all(bytes)?;
        descs.push(MemoryRegionDesc {
            gpa: gpa.raw_value(),
            len,
        });
    }
    Ok(descs)
}

/// Load guest-memory bytes from `inp` back into `mem`, using the region layout
/// captured by [`write_guest_memory`]. Each region's bytes are read directly
/// into the live host mapping. `mem` must have been built with a layout that
/// covers every `desc.gpa..desc.gpa+desc.len` range (i.e. the same VM config).
///
/// Must be called before the restored vCPUs are resumed.
pub fn read_guest_memory_into<R: Read>(
    mem: &GuestMemoryMmap,
    descs: &[MemoryRegionDesc],
    inp: &mut R,
) -> io::Result<()> {
    for desc in descs {
        let host = mem
            .get_host_address(GuestAddress(desc.gpa))
            .map_err(|e| io::Error::other(format!("get_host_address: {e:?}")))?;
        // Safety: `host` points to `desc.len` bytes of guest RAM for this
        // region, and the VM is not yet running, so writing into it is sound.
        let dst = unsafe { std::slice::from_raw_parts_mut(host, desc.len as usize) };
        inp.read_exact(dst)?;
    }
    Ok(())
}

/// Total byte length of all regions in a descriptor list (the size of the
/// memory image stream).
pub fn memory_image_len(descs: &[MemoryRegionDesc]) -> u64 {
    descs.iter().map(|d| d.len).sum()
}

/// Copy every region's bytes from `src` guest memory into `dst` (same layout).
/// Used to restore a VM from an in-memory CoW snapshot (`cow_clone_guest_memory`)
/// back into the live, KVM-registered guest memory — the bounded counterpart to
/// streaming through [`write_guest_memory`]/[`read_guest_memory_into`].
pub fn copy_guest_memory(src: &GuestMemoryMmap, dst: &GuestMemoryMmap) -> io::Result<()> {
    let io_err = |m: String| io::Error::other(m);
    for region in src.iter() {
        let gpa = region.start_addr();
        let len = region.len() as usize;
        let s = src
            .get_host_address(gpa)
            .map_err(|e| io_err(format!("src get_host_address: {e:?}")))?;
        let d = dst
            .get_host_address(gpa)
            .map_err(|e| io_err(format!("dst get_host_address: {e:?}")))?;
        // Safety: both point to `len` bytes of guest RAM for this region; the VM
        // is paused so the bytes are stable, and the regions don't overlap.
        unsafe { std::ptr::copy_nonoverlapping(s as *const u8, d, len) };
    }
    Ok(())
}

/// Copy-on-write clone of a `memfd`-backed guest memory image — the core of
/// fast, dense VM **fork** (plan §4/§9a). Each `memfd`-backed RAM region is
/// re-mapped `MAP_PRIVATE`, so the clone shares the parent's clean physical
/// pages and only copies a page when it is written: fork latency is independent
/// of RAM size, and N clones of a golden browser-ready VM share one copy of the
/// (unwritten) working set → pool density. Anonymous regions (device SHM/GPU,
/// not part of the CoW image) get a fresh private copy.
///
/// The parent must be paused/frozen at the fork point (its live `memfd` is the
/// shared base; later parent writes to un-CoW'd pages would otherwise leak into
/// clones — see plan §4). Linux-only; the macOS analogue is
/// `vm_remap(copy=TRUE)` on the anonymous HVF region.
#[cfg(target_os = "linux")]
pub fn cow_clone_guest_memory(parent: &GuestMemoryMmap) -> std::io::Result<GuestMemoryMmap> {
    use std::os::fd::AsRawFd;
    use vm_memory::mmap::MmapRegion;
    use vm_memory::GuestRegionMmap;

    let prot = libc::PROT_READ | libc::PROT_WRITE;
    let io_err = |m: String| io::Error::other(m);

    let mut regions: Vec<GuestRegionMmap> = Vec::new();
    for region in parent.iter() {
        let gpa = region.start_addr();
        let size = region.len() as usize;

        let (ptr, flags) = match region.file_offset() {
            // memfd-backed RAM → CoW clone (MAP_PRIVATE of the same memfd).
            Some(fo) => {
                let flags = libc::MAP_PRIVATE;
                // Safety: mapping `size` bytes of the parent's memfd at its
                // region offset; ptr is checked against MAP_FAILED below.
                let ptr = unsafe {
                    libc::mmap(
                        std::ptr::null_mut(),
                        size,
                        prot,
                        flags,
                        fo.file().as_raw_fd(),
                        fo.start() as libc::off_t,
                    )
                };
                if ptr == libc::MAP_FAILED {
                    return Err(io::Error::last_os_error());
                }
                (ptr, flags)
            }
            // Anonymous region (device SHM/GPU): fresh private map + byte copy.
            None => {
                let flags = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS;
                let ptr = unsafe { libc::mmap(std::ptr::null_mut(), size, prot, flags, -1, 0) };
                if ptr == libc::MAP_FAILED {
                    return Err(io::Error::last_os_error());
                }
                let src = parent
                    .get_host_address(gpa)
                    .map_err(|e| io_err(format!("get_host_address: {e:?}")))?;
                // Safety: both regions are `size` bytes and non-overlapping
                // (fresh mapping); the parent is frozen during the clone.
                unsafe { std::ptr::copy_nonoverlapping(src as *const u8, ptr as *mut u8, size) };
                (ptr, flags)
            }
        };

        // Safety: `ptr` is a live mapping of `size` bytes we just created; the
        // resulting MmapRegion takes ownership and munmaps it on drop.
        let mmap_region = unsafe { MmapRegion::build_raw(ptr as *mut u8, size, prot, flags) }
            .map_err(|e| io_err(format!("build_raw: {e:?}")))?;
        let guest_region = GuestRegionMmap::new(mmap_region, gpa)
            .ok_or_else(|| io_err("guest region address overflow".to_string()))?;
        regions.push(guest_region);
    }

    GuestMemoryMmap::from_regions(regions).map_err(|e| io_err(format!("from_regions: {e:?}")))
}

/// Describes one guest-RAM region for cross-process CoW fork: its guest address,
/// length, and (for memfd-backed RAM) the owner process's fd number + offset so a
/// clone can open `/proc/<pid>/fd/<fd>` and `mmap(MAP_PRIVATE)` it. `fd < 0` marks
/// an anonymous region (device SHM/GPU) the clone cannot CoW-share — it gets a
/// fresh zeroed mapping.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemfdRegionDesc {
    pub gpa: u64,
    pub len: u64,
    pub fd: i32,
    pub offset: u64,
    /// macOS only: filesystem path of the backing guest-RAM file (recovered via
    /// `F_GETPATH`), used by a clone to open + `mmap(MAP_PRIVATE)` it. Empty on
    /// Linux, where the clone instead reaches the backing memfd through
    /// `/proc/<owner_pid>/fd/<fd>`. Empty path == anonymous region (no CoW).
    pub path: String,
}

/// Enumerate the guest-memory regions of a (memfd-backed) VM for fork: returns
/// each region's gpa/len + the backing memfd fd (this process's fd number) and
/// offset, or `fd = -1` for anonymous regions. The owning process must stay
/// alive (frozen) so a clone can reach the fds via `/proc/<pid>/fd`.
#[cfg(target_os = "linux")]
pub fn memfd_region_descs(mem: &GuestMemoryMmap) -> Vec<MemfdRegionDesc> {
    use std::os::fd::AsRawFd;
    mem.iter()
        .map(|region| {
            let (fd, offset) = match region.file_offset() {
                Some(fo) => (fo.file().as_raw_fd(), fo.start()),
                None => (-1, 0),
            };
            MemfdRegionDesc {
                gpa: region.start_addr().raw_value(),
                len: region.len(),
                fd,
                offset,
                path: String::new(),
            }
        })
        .collect()
}

/// macOS variant of [`memfd_region_descs`]: records each region's backing-file
/// path (recovered via `F_GETPATH` on the open fd) so a clone can open + CoW-map
/// it. macOS has no `/proc/<pid>/fd`, so cross-process sharing goes through the
/// file path instead; the owner must stay alive (frozen) and the file must
/// remain on disk for the clone's lifetime. Anonymous regions get an empty path.
#[cfg(target_os = "macos")]
pub fn memfd_region_descs(mem: &GuestMemoryMmap) -> Vec<MemfdRegionDesc> {
    use std::os::fd::AsRawFd;
    mem.iter()
        .map(|region| {
            let (path, offset) = match region.file_offset() {
                Some(fo) => {
                    let mut buf = [0i8; libc::PATH_MAX as usize];
                    let rc = unsafe {
                        libc::fcntl(fo.file().as_raw_fd(), libc::F_GETPATH, buf.as_mut_ptr())
                    };
                    let path = if rc == 0 {
                        unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }
                            .to_string_lossy()
                            .into_owned()
                    } else {
                        String::new()
                    };
                    (path, fo.start())
                }
                None => (String::new(), 0),
            };
            MemfdRegionDesc {
                gpa: region.start_addr().raw_value(),
                len: region.len(),
                fd: if path.is_empty() { -1 } else { 0 },
                offset,
                path,
            }
        })
        .collect()
}

/// Build a clone's guest memory as a CoW view of another process's memfd-backed
/// RAM: for each descriptor, open `/proc/<owner_pid>/fd/<fd>` and
/// `mmap(MAP_PRIVATE)` it (clean pages shared with the frozen owner → density;
/// writes copy on demand). Anonymous regions get a fresh zeroed private mapping.
/// The owner process must be alive and frozen for the duration.
#[cfg(target_os = "linux")]
pub fn open_cow_memory_from_pid(
    owner_pid: i32,
    descs: &[MemfdRegionDesc],
) -> io::Result<GuestMemoryMmap> {
    use std::os::fd::AsRawFd;
    use vm_memory::mmap::MmapRegion;
    use vm_memory::GuestRegionMmap;

    let prot = libc::PROT_READ | libc::PROT_WRITE;
    let io_err = |m: String| io::Error::other(m);
    let mut regions: Vec<GuestRegionMmap> = Vec::with_capacity(descs.len());

    for d in descs {
        let size = d.len as usize;
        let (ptr, flags) = if d.fd >= 0 {
            // Open the owner's memfd via /proc and CoW-map it.
            let path = format!("/proc/{owner_pid}/fd/{}", d.fd);
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .map_err(|e| io_err(format!("open {path}: {e}")))?;
            let flags = libc::MAP_PRIVATE;
            // Safety: mapping `size` bytes of the owner's memfd at `offset`; the
            // mapping holds its own reference, so `file` may be dropped after.
            let ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    size,
                    prot,
                    flags,
                    file.as_raw_fd(),
                    d.offset as libc::off_t,
                )
            };
            (ptr, flags)
        } else {
            let flags = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS;
            let ptr = unsafe { libc::mmap(std::ptr::null_mut(), size, prot, flags, -1, 0) };
            (ptr, flags)
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let mmap_region = unsafe { MmapRegion::build_raw(ptr as *mut u8, size, prot, flags) }
            .map_err(|e| io_err(format!("build_raw: {e:?}")))?;
        let guest_region = GuestRegionMmap::new(mmap_region, GuestAddress(d.gpa))
            .ok_or_else(|| io_err("guest region address overflow".to_string()))?;
        regions.push(guest_region);
    }

    GuestMemoryMmap::from_regions(regions).map_err(|e| io_err(format!("from_regions: {e:?}")))
}

/// macOS variant of [`open_cow_memory_from_pid`]: opens each region's backing
/// file *by path* and `mmap(MAP_PRIVATE)`s it (clean pages shared CoW with the
/// frozen owner → density; writes copy on demand). Anonymous regions (empty
/// path) get a fresh zeroed private mapping. The owner process must be alive and
/// frozen and the backing files must remain on disk for the clone's lifetime.
#[cfg(target_os = "macos")]
pub fn open_cow_memory_from_paths(descs: &[MemfdRegionDesc]) -> io::Result<GuestMemoryMmap> {
    use std::os::fd::AsRawFd;
    use vm_memory::mmap::MmapRegion;
    use vm_memory::GuestRegionMmap;

    let prot = libc::PROT_READ | libc::PROT_WRITE;
    let io_err = |m: String| io::Error::other(m);
    let mut regions: Vec<GuestRegionMmap> = Vec::with_capacity(descs.len());

    for d in descs {
        let size = d.len as usize;
        let (ptr, flags) = if !d.path.is_empty() {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&d.path)
                .map_err(|e| io_err(format!("open {}: {e}", d.path)))?;
            let flags = libc::MAP_PRIVATE;
            // Safety: mapping `size` bytes of the owner's guest-RAM file at
            // `offset`; the mapping holds its own reference, so `file` may drop.
            let ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    size,
                    prot,
                    flags,
                    file.as_raw_fd(),
                    d.offset as libc::off_t,
                )
            };
            (ptr, flags)
        } else {
            let flags = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS;
            let ptr = unsafe { libc::mmap(std::ptr::null_mut(), size, prot, flags, -1, 0) };
            (ptr, flags)
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let mmap_region = unsafe { MmapRegion::build_raw(ptr as *mut u8, size, prot, flags) }
            .map_err(|e| io_err(format!("build_raw: {e:?}")))?;
        let guest_region = GuestRegionMmap::new(mmap_region, GuestAddress(d.gpa))
            .ok_or_else(|| io_err("guest region address overflow".to_string()))?;
        regions.push(guest_region);
    }

    GuestMemoryMmap::from_regions(regions).map_err(|e| io_err(format!("from_regions: {e:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

    #[test]
    fn test_guest_memory_snapshot_roundtrip_single_region() {
        let size = 0x20000usize; // 128 KiB
        let src = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), size)]).unwrap();

        // Write a recognizable, non-trivial pattern across the whole region.
        let pattern: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        src.write_slice(&pattern, GuestAddress(0)).unwrap();

        // Dump to an in-memory stream.
        let mut buf = Vec::new();
        let descs = write_guest_memory(&src, &mut buf).unwrap();
        assert_eq!(descs.len(), 1);
        assert_eq!(descs[0].gpa, 0);
        assert_eq!(descs[0].len as usize, size);
        assert_eq!(buf.len(), size);
        assert_eq!(memory_image_len(&descs) as usize, size);

        // Restore into a fresh, zeroed guest memory.
        let dst = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), size)]).unwrap();
        read_guest_memory_into(&dst, &descs, &mut buf.as_slice()).unwrap();

        let mut got = vec![0u8; size];
        dst.read_slice(&mut got, GuestAddress(0)).unwrap();
        assert_eq!(got, pattern, "restored bytes must match the snapshot");
    }

    #[test]
    fn test_guest_memory_snapshot_roundtrip_multi_region() {
        // Two regions separated by a gap in guest physical space.
        let regions = [
            (GuestAddress(0), 0x10000usize),
            (GuestAddress(0x100000), 0x8000usize),
        ];
        let src = GuestMemoryMmap::from_ranges(&regions).unwrap();
        src.write_slice(&[0xAB; 0x10000], GuestAddress(0)).unwrap();
        src.write_slice(&[0xCD; 0x8000], GuestAddress(0x100000))
            .unwrap();

        let mut buf = Vec::new();
        let descs = write_guest_memory(&src, &mut buf).unwrap();
        assert_eq!(descs.len(), 2);
        assert_eq!(buf.len(), 0x10000 + 0x8000);

        let dst = GuestMemoryMmap::from_ranges(&regions).unwrap();
        read_guest_memory_into(&dst, &descs, &mut buf.as_slice()).unwrap();

        let mut got_lo = vec![0u8; 0x10000];
        let mut got_hi = vec![0u8; 0x8000];
        dst.read_slice(&mut got_lo, GuestAddress(0)).unwrap();
        dst.read_slice(&mut got_hi, GuestAddress(0x100000)).unwrap();
        assert_eq!(got_lo, vec![0xAB; 0x10000]);
        assert_eq!(got_hi, vec![0xCD; 0x8000]);
    }

    // The CoW fork primitive: a clone shares the parent's clean pages but is
    // isolated on write, in both directions — the density + safety property
    // that makes fast fork sound (plan §4). Validated on the real memfd-backed
    // vm-memory abstraction, not just the standalone PoC.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_cow_clone_isolation_and_sharing() {
        use crate::builder::create_guest_ram_memfd;
        use vm_memory::FileOffset;

        let size = 0x10000usize; // 64 KiB
        let memfd = create_guest_ram_memfd(size).expect("memfd");
        let parent = GuestMemoryMmap::from_ranges_with_files([(
            GuestAddress(0),
            size,
            Some(FileOffset::new(memfd, 0)),
        )])
        .expect("memfd-backed parent");

        // Base pattern in the parent.
        parent
            .write_slice(&[0xAA; 0x10000], GuestAddress(0))
            .unwrap();

        // CoW clone shares the parent's pages → sees the same bytes.
        let clone = cow_clone_guest_memory(&parent).expect("cow clone");
        let mut buf = vec![0u8; 16];
        clone.read_slice(&mut buf, GuestAddress(0)).unwrap();
        assert_eq!(buf, vec![0xAA; 16], "clone shares parent's clean pages");

        // Writing the clone copies-on-write: the parent is unaffected.
        clone.write_slice(&[0xBB; 16], GuestAddress(0)).unwrap();
        let mut p = vec![0u8; 16];
        parent.read_slice(&mut p, GuestAddress(0)).unwrap();
        assert_eq!(p, vec![0xAA; 16], "parent unchanged by clone write (CoW)");
        let mut c = vec![0u8; 16];
        clone.read_slice(&mut c, GuestAddress(0)).unwrap();
        assert_eq!(c, vec![0xBB; 16], "clone holds its own copy");

        // And a later parent write does not leak into the clone's CoW'd page.
        parent.write_slice(&[0xCC; 16], GuestAddress(0)).unwrap();
        let mut c2 = vec![0u8; 16];
        clone.read_slice(&mut c2, GuestAddress(0)).unwrap();
        assert_eq!(
            c2,
            vec![0xBB; 16],
            "clone isolated from later parent writes"
        );
    }

    // Pool density: N CoW clones of a faulted-in base must cost only the pages
    // each clone *writes*, not N full copies — the "extremely fast scaling"
    // property (plan §9a / PoC Exp 1), here on the real `cow_clone` primitive.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_cow_clone_density() {
        use crate::builder::create_guest_ram_memfd;
        use vm_memory::FileOffset;

        // Resident-set size of this process, in bytes (statm field 2 = pages).
        fn rss_bytes() -> u64 {
            let s = std::fs::read_to_string("/proc/self/statm").unwrap();
            let resident_pages: u64 = s.split_whitespace().nth(1).unwrap().parse().unwrap();
            resident_pages * 4096
        }

        let base = 64 * 1024 * 1024usize; // 64 MiB base image
        let memfd = create_guest_ram_memfd(base).expect("memfd");
        let parent = GuestMemoryMmap::from_ranges_with_files([(
            GuestAddress(0),
            base,
            Some(FileOffset::new(memfd, 0)),
        )])
        .expect("parent");
        // Fault the whole base in so it counts toward RSS.
        parent
            .write_slice(&vec![0x5A; base], GuestAddress(0))
            .unwrap();

        let rss_before = rss_bytes();

        // 8 clones; dirty 1 MiB in each (forces CoW of just those pages).
        const N: usize = 8;
        let mut clones = Vec::new();
        for _ in 0..N {
            let c = cow_clone_guest_memory(&parent).expect("clone");
            c.write_slice(&vec![0xA5; 1024 * 1024], GuestAddress(0))
                .unwrap();
            clones.push(c);
        }

        let added = rss_bytes().saturating_sub(rss_before);
        let naive = (N * base) as u64; // what 8 full copies would cost (512 MiB)
                                       // Clones should add only ~N MiB (their dirtied pages) + slop, far below
                                       // a naive 512 MiB. Generous bound to stay robust across machines.
        assert!(
            added < 64 * 1024 * 1024,
            "8 CoW clones of 64 MiB added {added} bytes (naive full copy = {naive}); \
             expected only the dirtied pages — pages are not being shared"
        );
        assert_eq!(clones.len(), N);
    }

    #[test]
    fn test_short_stream_is_an_error() {
        let size = 0x4000usize;
        let descs = [MemoryRegionDesc {
            gpa: 0,
            len: size as u64,
        }];
        let dst = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), size)]).unwrap();
        // Stream has fewer bytes than the descriptor claims -> read_exact errors.
        let truncated = vec![0u8; size - 1];
        let err = read_guest_memory_into(&dst, &descs, &mut truncated.as_slice()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }
}
