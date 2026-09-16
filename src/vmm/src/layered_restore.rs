// Copyright 2026. SPDX-License-Identifier: Apache-2.0
//! Immutable layered RAM generations for Linux restore and branching.
//! Complete immutable backing consists of a base plus sealed modified-page
//! extents. Every mapping is private and kernel-faultable without a pager thread.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use vm_memory::mmap::MmapRegion;
use vm_memory::{Bytes, GuestAddress, GuestMemory, GuestMemoryMmap, GuestRegionMmap};

const PAGE: usize = 4096;
/// Same-host live generation format, distinct from a portable checkpoint.
pub const MANIFEST_MAGIC: [u8; 8] = *b"SMOLLAY1";

/// A complete logical region, independent of the number of file extents.
#[derive(Clone, Debug)]
pub struct RegionDescription {
    pub gpa: u64,
    pub len: u64,
    pub extents: Vec<ExtentDescription>,
}

/// Borrowed file descriptor in the generation owner's process. The owner must
/// retain its generation until receivers have opened their own descriptors.
#[derive(Clone, Debug)]
pub struct ExtentDescription {
    pub start: u64,
    pub len: u64,
    pub fd: i32,
    pub offset: u64,
}

/// Owns every immutable backing needed to restore or export this generation.
/// Dropping ancestors or unlinking their paths cannot invalidate these handles.
#[derive(Clone)]
pub struct Generation {
    regions: Vec<Image>,
}

impl Generation {
    pub fn publish_manifest(
        &self,
        socket: &std::path::Path,
    ) -> io::Result<(Vec<u8>, crate::retained_fds::RetainedFiles)> {
        use std::os::unix::ffi::OsStrExt;
        let mut bytes = self.encode_manifest()?;
        let mut files = std::collections::BTreeMap::new();
        for region in &self.regions {
            for extent in &region.extents {
                files.insert(extent.file.as_raw_fd(), extent.file.clone());
            }
        }
        let (service, token) = crate::retained_fds::RetainedFiles::start(socket, files)?;
        bytes[12..16].copy_from_slice(&1u32.to_le_bytes());
        let path = socket.as_os_str().as_bytes();
        bytes.extend_from_slice(&(path.len() as u16).to_le_bytes());
        bytes.extend_from_slice(path);
        bytes.extend_from_slice(&token);
        Ok((bytes, service))
    }
    /// Encode references to this process's retained immutable descriptors.
    /// Keep this generation alive until importers acquire their own handles.
    pub fn encode_manifest(&self) -> io::Result<Vec<u8>> {
        self.check_limits()?;
        let pid = std::process::id();
        let start = process_start_time(pid)?;
        let descriptions = self.descriptions();
        let mut output = Vec::new();
        output.extend_from_slice(&MANIFEST_MAGIC);
        output.extend_from_slice(&pid.to_le_bytes());
        output.extend_from_slice(&0_u32.to_le_bytes());
        output.extend_from_slice(&start.to_le_bytes());
        output.extend_from_slice(&(descriptions.len() as u32).to_le_bytes());
        for region in descriptions {
            output.extend_from_slice(&region.gpa.to_le_bytes());
            output.extend_from_slice(&region.len.to_le_bytes());
            output.extend_from_slice(&(region.extents.len() as u32).to_le_bytes());
            for extent in region.extents {
                output.extend_from_slice(&extent.start.to_le_bytes());
                output.extend_from_slice(&extent.len.to_le_bytes());
                output.extend_from_slice(&extent.fd.to_le_bytes());
                output.extend_from_slice(&extent.offset.to_le_bytes());
            }
        }
        Ok(output)
    }

    pub fn decode_manifest(input: &[u8]) -> io::Result<Self> {
        fn take<const N: usize>(input: &mut &[u8]) -> io::Result<[u8; N]> {
            let bytes = input
                .get(..N)
                .ok_or_else(|| invalid("truncated layered RAM manifest"))?;
            let value = bytes.try_into().unwrap();
            *input = &input[N..];
            Ok(value)
        }
        if input.len() > 2 * 1024 * 1024 {
            return Err(invalid("layered RAM manifest too large"));
        }
        let mut remaining = input;
        if take::<8>(&mut remaining)? != MANIFEST_MAGIC {
            return Err(invalid("invalid layered RAM manifest magic"));
        }
        let pid = u32::from_le_bytes(take(&mut remaining)?);
        let transport = u32::from_le_bytes(take(&mut remaining)?);
        if pid == 0 || pid > i32::MAX as u32 || transport > 1 {
            return Err(invalid("invalid layered RAM owner/header"));
        }
        let start = u64::from_le_bytes(take(&mut remaining)?);
        let count = u32::from_le_bytes(take(&mut remaining)?) as usize;
        if count == 0 || count > 256 {
            return Err(invalid("invalid RAM region count"));
        }
        let mut descriptions = Vec::with_capacity(count);
        let mut total = 0;
        for _ in 0..count {
            let gpa = u64::from_le_bytes(take(&mut remaining)?);
            let len = u64::from_le_bytes(take(&mut remaining)?);
            let count = u32::from_le_bytes(take(&mut remaining)?) as usize;
            total += count;
            if count == 0 || total > 65536 || count > remaining.len() / 28 {
                return Err(invalid("invalid layered RAM extent count"));
            }
            let mut extents = Vec::with_capacity(count);
            for _ in 0..count {
                extents.push(ExtentDescription {
                    start: u64::from_le_bytes(take(&mut remaining)?),
                    len: u64::from_le_bytes(take(&mut remaining)?),
                    fd: i32::from_le_bytes(take(&mut remaining)?),
                    offset: u64::from_le_bytes(take(&mut remaining)?),
                });
            }
            descriptions.push(RegionDescription { gpa, len, extents });
        }
        let handoff = if transport == 1 {
            use std::os::unix::ffi::OsStrExt;
            let len = u16::from_le_bytes(take(&mut remaining)?) as usize;
            if len == 0 || len > 100 {
                return Err(invalid("invalid checkpoint handoff path length"));
            }
            let path = remaining
                .get(..len)
                .ok_or_else(|| invalid("truncated checkpoint handoff path"))?;
            let path = std::path::PathBuf::from(std::ffi::OsStr::from_bytes(path));
            if !path.is_absolute() {
                return Err(invalid("checkpoint handoff path must be absolute"));
            }
            remaining = &remaining[len..];
            Some((path, take::<32>(&mut remaining)?))
        } else {
            None
        };
        if !remaining.is_empty() {
            return Err(invalid("trailing layered RAM manifest data"));
        }
        if process_start_time(pid)? != start {
            return Err(invalid("layered RAM owner identity changed"));
        }
        let generation = if let Some((path, token)) = handoff {
            let keys: Vec<_> = descriptions
                .iter()
                .flat_map(|region| region.extents.iter().map(|extent| extent.fd))
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            let files = crate::retained_fds::receive(&path, &token, pid, &keys)?;
            Self::from_descriptions_with_files(
                pid as i32,
                &descriptions,
                files.into_iter().collect(),
            )?
        } else {
            Self::from_descriptions(pid as i32, &descriptions)?
        };
        if process_start_time(pid)? != start {
            return Err(invalid("layered RAM owner changed during import"));
        }
        Ok(generation)
    }
    /// The caller must retain the immutable-file contract: a read-only handle
    /// prevents writes through this handle, not through other host handles.
    pub fn from_immutable_file(
        descs: &[crate::snapshot::MemoryRegionDesc],
        file: &File,
    ) -> io::Result<Self> {
        let mut descriptions = Vec::new();
        let mut offset = 0_u64;
        for desc in descs {
            descriptions.push(RegionDescription {
                gpa: desc.gpa,
                len: desc.len,
                extents: vec![ExtentDescription {
                    start: 0,
                    len: desc.len,
                    fd: file.as_raw_fd(),
                    offset,
                }],
            });
            offset = offset
                .checked_add(desc.len)
                .ok_or_else(|| invalid("RAM size overflow"))?;
        }
        if file.metadata()?.len() != offset {
            return Err(invalid("RAM file length mismatch"));
        }
        // Restore can run after the launcher has dropped to the VM's UID.
        // The validated, pre-opened snapshot remains readable through its fd,
        // but reopening /proc/self/fd would repeat pathname permission checks
        // against a cache inode owned by the service. Preserve that capability
        // rather than requiring broader access to the checkpoint cache.
        let files =
            std::collections::HashMap::from([(file.as_raw_fd(), Arc::new(file.try_clone()?))]);
        Self::from_descriptions_with_files(std::process::id() as i32, &descriptions, files)
    }

    /// Import immutable handles from a trusted, authenticated generation owner.
    /// Validate the complete layout before creating any guest mapping.
    pub fn from_descriptions(owner: i32, descriptions: &[RegionDescription]) -> io::Result<Self> {
        Self::from_descriptions_with_files(owner, descriptions, std::collections::HashMap::new())
    }

    fn from_descriptions_with_files(
        owner: i32,
        descriptions: &[RegionDescription],
        mut files: std::collections::HashMap<i32, Arc<File>>,
    ) -> io::Result<Self> {
        if owner <= 0 || descriptions.is_empty() || descriptions.len() > 256 {
            return Err(invalid("invalid layered RAM owner or region count"));
        }
        let mut regions = Vec::new();
        let mut previous_end = 0;
        let mut extent_count = 0_usize;
        for region in descriptions {
            let end = region
                .gpa
                .checked_add(region.len)
                .ok_or_else(|| invalid("RAM address overflow"))?;
            if region.gpa < previous_end
                || !region.gpa.is_multiple_of(PAGE as u64)
                || region.len == 0
                || !region.len.is_multiple_of(PAGE as u64)
            {
                return Err(invalid("invalid or overlapping RAM region"));
            }
            let len = usize::try_from(region.len)
                .map_err(|_| invalid("RAM region exceeds address space"))?;
            let mut cursor = 0_u64;
            let mut extents = Vec::new();
            for extent in &region.extents {
                extent_count += 1;
                if extent_count > 65536 {
                    return Err(invalid("too many layered RAM extents"));
                }
                if extent.fd < 0
                    || extent.start != cursor
                    || extent.len == 0
                    || !extent.len.is_multiple_of(PAGE as u64)
                    || !extent.offset.is_multiple_of(PAGE as u64)
                {
                    return Err(invalid("invalid layered RAM extent"));
                }
                cursor = cursor
                    .checked_add(extent.len)
                    .ok_or_else(|| invalid("extent overflow"))?;
                let file_end = extent
                    .offset
                    .checked_add(extent.len)
                    .ok_or_else(|| invalid("file extent overflow"))?;
                if cursor > region.len || file_end > i64::MAX as u64 {
                    return Err(invalid("extent exceeds its region or file offset range"));
                }
                let file = if let Some(file) = files.get(&extent.fd) {
                    file.clone()
                } else {
                    if files.len() >= 1024 {
                        return Err(invalid("too many layered RAM files"));
                    }
                    let file = Arc::new(
                        File::open(format!("/proc/{owner}/fd/{}", extent.fd)).map_err(|error| {
                            io::Error::new(error.kind(), format!(
                                "open retained RAM descriptor {} from generation owner {owner}: {error}",
                                extent.fd
                            ))
                        })?,
                    );
                    files.insert(extent.fd, file.clone());
                    file
                };
                if !file.metadata()?.is_file() || file.metadata()?.len() < file_end {
                    return Err(invalid("layered RAM backing is truncated or not a file"));
                }
                extents.push(Extent {
                    start: extent.start as usize,
                    end: cursor as usize,
                    offset: extent.offset,
                    file,
                });
            }
            if cursor != region.len {
                return Err(invalid("layered RAM has uncovered pages"));
            }
            regions.push(Image {
                gpa: region.gpa,
                len,
                extents,
            });
            previous_end = end;
        }
        Ok(Self { regions })
    }

    pub fn descriptions(&self) -> Vec<RegionDescription> {
        self.regions
            .iter()
            .map(|region| RegionDescription {
                gpa: region.gpa,
                len: region.len as u64,
                extents: region
                    .extents
                    .iter()
                    .map(|extent| ExtentDescription {
                        start: extent.start as u64,
                        len: (extent.end - extent.start) as u64,
                        fd: extent.file.as_raw_fd(),
                        offset: extent.offset,
                    })
                    .collect(),
            })
            .collect()
    }

    fn check_limits(&self) -> io::Result<()> {
        let mut count = 0;
        let mut files = std::collections::HashSet::new();
        for image in &self.regions {
            count += image.extents.len();
            if count > 65536 {
                return Err(invalid("layered RAM extent budget exceeded"));
            }
            for extent in &image.extents {
                files.insert(extent.file.as_raw_fd());
                if files.len() > 1024 {
                    return Err(invalid("layered RAM file budget exceeded"));
                }
            }
        }
        Ok(())
    }

    /// Logical regions remain unchanged even when the backing has many extents;
    /// a KVM slot is not consumed for each extent.
    pub fn restore(&self) -> io::Result<GuestMemoryMmap> {
        let mut regions = Vec::new();
        for image in &self.regions {
            let instance = image.restore()?;
            let (_, region) = instance
                .memory
                .remove_region(GuestAddress(image.gpa), image.len as u64)
                .map_err(|error| io::Error::other(format!("extract RAM region: {error:?}")))?;
            regions.push(region);
        }
        GuestMemoryMmap::from_arc_regions(regions)
            .map_err(|error| io::Error::other(format!("assemble layered RAM: {error:?}")))
    }

    pub(crate) fn validate_memory(
        &self,
        memory: &GuestMemoryMmap,
        excluded: &[u64],
    ) -> io::Result<()> {
        use vm_memory::{Address, GuestMemoryRegion};
        if memory.num_regions() != self.regions.len()
            || memory.iter().zip(&self.regions).any(|(actual, expected)| {
                actual.start_addr().raw_value() != expected.gpa
                    || actual.len() != expected.len as u64
            })
        {
            return Err(invalid("layered RAM mapping/layout mismatch"));
        }
        let mut regions = Vec::new();
        for image in &self.regions {
            if !excluded.contains(&image.gpa) {
                let (_, region) = memory
                    .remove_region(GuestAddress(image.gpa), image.len as u64)
                    .map_err(|error| io::Error::other(format!("select private RAM: {error:?}")))?;
                regions.push(region);
            }
        }
        let private = GuestMemoryMmap::from_arc_regions(regions)
            .map_err(|error| io::Error::other(format!("private RAM regions: {error:?}")))?;
        crate::generation_guardian::validate_private_memory_mappings(&private)
    }

    /// All vCPUs and device writers must be quiesced for the entire operation.
    pub fn capture_quiesced(&self, memory: &GuestMemoryMmap) -> io::Result<(Self, usize)> {
        self.capture_quiesced_excluding(memory, &[])
    }

    /// Device-owned shared windows are restored by their device snapshot, not
    /// by inspecting their page-table entries as if they were private RAM.
    pub(crate) fn capture_quiesced_excluding(
        &self,
        memory: &GuestMemoryMmap,
        excluded: &[u64],
    ) -> io::Result<(Self, usize)> {
        self.validate_memory(memory, excluded)?;
        let mut regions = Vec::new();
        let mut copied = 0_usize;
        for image in &self.regions {
            if excluded.contains(&image.gpa) {
                regions.push(image.clone());
                continue;
            }
            let instance = Instance {
                memory: memory.clone(),
                image: image.clone(),
            };
            let (next, bytes) = instance.capture_quiesced()?;
            copied = copied
                .checked_add(bytes)
                .ok_or_else(|| invalid("copied RAM size overflow"))?;
            regions.push(next);
        }
        let next = Self { regions };
        next.check_limits()?;
        Ok((next, copied))
    }

    /// Call only while every writer is stopped. On error the caller MUST NOT
    /// resume without recovering the mapping; some extents may already change.
    pub fn rebase_quiesced(&self, memory: &GuestMemoryMmap) -> io::Result<()> {
        self.rebase_quiesced_excluding(memory, &[])
    }

    pub(crate) fn rebase_quiesced_excluding(
        &self,
        memory: &GuestMemoryMmap,
        excluded: &[u64],
    ) -> io::Result<()> {
        self.validate_memory(memory, excluded)?;
        for image in &self.regions {
            if excluded.contains(&image.gpa) {
                continue;
            }
            let mut instance = Instance {
                memory: memory.clone(),
                image: image.clone(),
            };
            instance.rebase_quiesced(image.clone())?;
        }
        Ok(())
    }

    /// Export logical bytes directly from retained backing, including explicit
    /// zero overwrites; never mistake a sparse delta file for a complete image.
    pub fn write_to<W: io::Write>(&self, output: &mut W) -> io::Result<()> {
        let mut buffer = vec![0; 64 * 1024];
        for image in &self.regions {
            for extent in &image.extents {
                let mut offset = 0;
                while offset < extent.end - extent.start {
                    let len = (extent.end - extent.start - offset).min(buffer.len());
                    extent
                        .file
                        .read_exact_at(&mut buffer[..len], extent.offset + offset as u64)?;
                    output.write_all(&buffer[..len])?;
                    offset += len;
                }
            }
        }
        Ok(())
    }

    pub fn memory_regions(&self) -> Vec<crate::snapshot::MemoryRegionDesc> {
        self.regions
            .iter()
            .map(|image| crate::snapshot::MemoryRegionDesc {
                gpa: image.gpa,
                len: image.len as u64,
            })
            .collect()
    }

    pub fn write_sparse_to(&self, output: &mut File) -> io::Result<()> {
        use std::io::Seek;
        output.set_len(0)?;
        let mut buffer = vec![0; 64 * 1024];
        let mut logical = 0_u64;
        for image in &self.regions {
            for extent in &image.extents {
                let mut offset = 0;
                while offset < extent.end - extent.start {
                    let len = (extent.end - extent.start - offset).min(buffer.len());
                    extent
                        .file
                        .read_exact_at(&mut buffer[..len], extent.offset + offset as u64)?;
                    if buffer[..len].iter().any(|byte| *byte != 0) {
                        output.write_all_at(&buffer[..len], logical)?;
                    }
                    logical = logical
                        .checked_add(len as u64)
                        .ok_or_else(|| invalid("RAM export size overflow"))?;
                    offset += len;
                }
            }
        }
        output.set_len(logical)?;
        output.seek(io::SeekFrom::Start(logical))?;
        Ok(())
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn process_start_time(pid: u32) -> io::Result<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    stat.rsplit_once(") ")
        .and_then(|(_, fields)| fields.split_whitespace().nth(19))
        .ok_or_else(|| invalid("missing RAM owner identity"))?
        .parse()
        .map_err(|_| invalid("invalid RAM owner identity"))
}

#[derive(Clone)]
struct Extent {
    start: usize,
    end: usize,
    offset: u64,
    file: Arc<File>,
}

#[derive(Clone)]
struct Image {
    gpa: u64,
    len: usize,
    extents: Vec<Extent>,
}

struct Instance {
    memory: GuestMemoryMmap,
    image: Image,
}

impl Image {
    // The caller owns the immutability contract for the original snapshot.
    // Holding a descriptor protects against unlink, not in-place modification.
    #[cfg(test)]
    fn from_immutable_file(file: File, len: usize) -> io::Result<Self> {
        if len == 0 || !len.is_multiple_of(PAGE) || file.metadata()?.len() != len as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "RAM image size mismatch",
            ));
        }
        Ok(Self {
            gpa: 0,
            len,
            extents: vec![Extent {
                start: 0,
                end: len,
                offset: 0,
                file: Arc::new(file),
            }],
        })
    }

    fn restore(&self) -> io::Result<Instance> {
        let prot = libc::PROT_READ | libc::PROT_WRITE;
        let region = MmapRegion::build(
            None,
            self.len,
            prot,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
        )
        .map_err(|error| io::Error::other(format!("reserve layered RAM: {error:?}")))?;
        let region = GuestRegionMmap::new(region, GuestAddress(self.gpa))
            .ok_or_else(|| io::Error::other("RAM address overflow"))?;
        let memory = GuestMemoryMmap::from_regions(vec![region])
            .map_err(|error| io::Error::other(format!("layered RAM regions: {error:?}")))?;
        let base = memory.get_host_address(GuestAddress(self.gpa)).unwrap();
        for extent in &self.extents {
            // SAFETY: each aligned extent lies exclusively inside the reserved
            // mapping owned above. On failure its RAII owner unmaps the whole
            // reservation, including already-installed file views.
            let result = unsafe {
                libc::mmap(
                    base.add(extent.start).cast(),
                    extent.end - extent.start,
                    prot,
                    libc::MAP_PRIVATE | libc::MAP_FIXED | libc::MAP_NORESERVE,
                    extent.file.as_raw_fd(),
                    extent.offset as libc::off_t,
                )
            };
            if result == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Instance {
            memory,
            image: self.clone(),
        })
    }
}

// Pagemap reveals file/shared-anon versus private pages without needing PFNs.
// Swapped entries are conservatively included and read back through the valid
// mapping, never interpreted as zero or silently omitted.
fn private_or_swapped(entry: u64) -> bool {
    entry & (1 << 62) != 0 || (entry & (1 << 63) != 0 && entry & (1 << 61) == 0)
}

impl Instance {
    // Requires stopped vCPUs AND devices. Keep the original host addresses so
    // KVM slots stay valid. If a mapping fails the caller must NOT resume;
    // production integration needs explicit fail-closed/rollback handling.
    fn rebase_quiesced(&mut self, image: Image) -> io::Result<()> {
        if image.len != self.image.len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "rebase size mismatch",
            ));
        }
        let base = self
            .memory
            .get_host_address(GuestAddress(self.image.gpa))
            .unwrap();
        for extent in &image.extents {
            // SAFETY: this instance owns the full reservation; all writers are
            // stopped and each backing contains the captured bytes for its run.
            let result = unsafe {
                libc::mmap(
                    base.add(extent.start).cast(),
                    extent.end - extent.start,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_FIXED | libc::MAP_NORESERVE,
                    extent.file.as_raw_fd(),
                    extent.offset as libc::off_t,
                )
            };
            if result == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
        }
        self.image = image;
        Ok(())
    }

    // Requires all writers to be quiesced. This is deliberately test-only until
    // device mappings, admission accounting, capture and ABI ownership are wired.
    fn capture_quiesced(&self) -> io::Result<(Image, usize)> {
        let pagemap = File::open("/proc/self/pagemap")?;
        let base = self
            .memory
            .get_host_address(GuestAddress(self.image.gpa))
            .unwrap() as u64;
        let mut dirty = Vec::<(usize, usize)>::new();
        let mut entries = vec![0_u8; 8192 * 8];
        for first in (0..self.image.len / PAGE).step_by(8192) {
            let count = (self.image.len / PAGE - first).min(8192);
            pagemap.read_exact_at(
                &mut entries[..count * 8],
                (base / PAGE as u64 + first as u64) * 8,
            )?;
            for index in 0..count {
                let entry =
                    u64::from_ne_bytes(entries[index * 8..index * 8 + 8].try_into().unwrap());
                if private_or_swapped(entry) {
                    let start = (first + index) * PAGE;
                    if let Some(last) = dirty.last_mut().filter(|last| last.1 == start) {
                        last.1 += PAGE;
                    } else {
                        dirty.push((start, start + PAGE));
                    }
                }
            }
        }
        if dirty.is_empty() {
            return Ok((self.image.clone(), 0));
        }
        let delta =
            crate::builder::create_guest_ram_memfd(self.image.len).map_err(io::Error::other)?;
        let mut buffer = vec![0; 64 * 1024];
        let mut copied = 0;
        for &(start, end) in &dirty {
            for offset in (start..end).step_by(buffer.len()) {
                let len = (end - offset).min(buffer.len());
                self.memory
                    .read_slice(
                        &mut buffer[..len],
                        GuestAddress(self.image.gpa + offset as u64),
                    )
                    .map_err(|error| io::Error::other(format!("read private RAM: {error:?}")))?;
                delta.write_all_at(&buffer[..len], offset as u64)?;
                copied += len;
            }
        }
        let seals =
            libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
        if unsafe { libc::fcntl(delta.as_raw_fd(), libc::F_ADD_SEALS, seals) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let delta = Arc::new(delta);
        let mut extents = Vec::new();
        // Flatten the mapping index now; a restore never walks a delta chain.
        let mut first_dirty = 0;
        for old in &self.image.extents {
            let mut cursor = old.start;
            while first_dirty < dirty.len() && dirty[first_dirty].1 <= old.start {
                first_dirty += 1;
            }
            for &(start, end) in &dirty[first_dirty..] {
                if end <= cursor {
                    continue;
                }
                if start >= old.end {
                    break;
                }
                if start > cursor {
                    extents.push(Extent {
                        start: cursor,
                        end: start,
                        offset: old.offset + (cursor - old.start) as u64,
                        file: old.file.clone(),
                    });
                }
                cursor = end.min(old.end);
                if cursor == old.end {
                    break;
                }
            }
            if cursor < old.end {
                extents.push(Extent {
                    start: cursor,
                    end: old.end,
                    offset: old.offset + (cursor - old.start) as u64,
                    file: old.file.clone(),
                });
            }
        }
        for (start, end) in dirty {
            extents.push(Extent {
                start,
                end,
                offset: start as u64,
                file: delta.clone(),
            });
        }
        extents.sort_unstable_by_key(|extent| extent.start);
        let mut merged: Vec<Extent> = Vec::with_capacity(extents.len());
        for extent in extents {
            if let Some(last) = merged.last_mut().filter(|last| {
                last.end == extent.start
                    && Arc::ptr_eq(&last.file, &extent.file)
                    && last.offset + (last.end - last.start) as u64 == extent.offset
            }) {
                last.end = extent.end;
            } else {
                merged.push(extent);
            }
        }
        let extents = merged;
        let mut cursor = 0;
        for extent in &extents {
            assert_eq!(extent.start, cursor);
            cursor = extent.end;
        }
        assert_eq!(cursor, self.image.len);
        Ok((
            Image {
                gpa: self.image.gpa,
                len: self.image.len,
                extents,
            },
            copied,
        ))
    }
}

#[test]
fn swapped_pages_are_not_discarded() {
    assert!(!private_or_swapped(0));
    assert!(!private_or_swapped((1 << 63) | (1 << 61)));
    assert!(private_or_swapped(1 << 63));
    assert!(private_or_swapped(1 << 62));
}

#[test]
fn restore_keeps_preopened_descriptor_when_path_access_is_revoked() {
    use crate::snapshot::MemoryRegionDesc;
    use std::os::unix::fs::PermissionsExt;
    let file = crate::builder::create_guest_ram_memfd(PAGE).unwrap();
    file.write_all_at(&[0x6d; PAGE], 0).unwrap();
    let readonly = File::open(format!("/proc/self/fd/{}", file.as_raw_fd())).unwrap();
    file.set_permissions(std::fs::Permissions::from_mode(0o000))
        .unwrap();
    if unsafe { libc::geteuid() } != 0 {
        assert!(File::open(format!("/proc/self/fd/{}", readonly.as_raw_fd())).is_err());
    }
    let generation = Generation::from_immutable_file(
        &[MemoryRegionDesc {
            gpa: 0,
            len: PAGE as u64,
        }],
        &readonly,
    )
    .unwrap();
    drop(readonly);
    drop(file);
    let memory = generation.restore().unwrap();
    assert_eq!(memory.read_obj::<u8>(GuestAddress(0)).unwrap(), 0x6d);
}

#[test]
fn multi_region_import_capture_export_and_rebase() {
    use crate::snapshot::MemoryRegionDesc;
    use vm_memory::GuestMemoryRegion;
    let file = crate::builder::create_guest_ram_memfd(5 * PAGE).unwrap();
    file.write_all_at(&[0x11; 2 * PAGE], 0).unwrap();
    file.write_all_at(&[0x73; 3 * PAGE], (2 * PAGE) as u64)
        .unwrap();
    let descs = [
        MemoryRegionDesc {
            gpa: 0,
            len: (2 * PAGE) as u64,
        },
        MemoryRegionDesc {
            gpa: 1 << 32,
            len: (3 * PAGE) as u64,
        },
    ];
    let generation = Generation::from_immutable_file(&descs, &file).unwrap();
    drop(file);
    let memory = generation.restore().unwrap();
    assert_eq!(memory.num_regions(), 2);
    assert_eq!(
        memory.iter().map(|region| region.len()).collect::<Vec<_>>(),
        vec![(2 * PAGE) as u64, (3 * PAGE) as u64]
    );
    memory.write_slice(&[0; PAGE], GuestAddress(0)).unwrap();
    memory
        .write_slice(&[0x91; PAGE], GuestAddress((1 << 32) + (2 * PAGE) as u64))
        .unwrap();
    let (captured, copied) = generation.capture_quiesced(&memory).unwrap();
    assert_eq!(copied, 2 * PAGE);
    let descriptions = captured.descriptions();
    assert_eq!(
        descriptions
            .iter()
            .map(|region| region.extents.len())
            .sum::<usize>(),
        4
    );
    let imported = Generation::from_descriptions(std::process::id() as i32, &descriptions).unwrap();
    let mut bytes = Vec::new();
    imported.write_to(&mut bytes).unwrap();
    let mut expected = vec![0; PAGE];
    expected.extend_from_slice(&[0x11; PAGE]);
    expected.extend_from_slice(&[0x73; 2 * PAGE]);
    expected.extend_from_slice(&[0x91; PAGE]);
    assert_eq!(bytes, expected);
    captured.rebase_quiesced(&memory).unwrap();
    let (_, copied) = captured.capture_quiesced(&memory).unwrap();
    assert_eq!(copied, 0);
    drop(generation);
    drop(captured);
    drop(memory);
    let restored = imported.restore().unwrap();
    assert_eq!(restored.num_regions(), 2);
    assert_eq!(restored.read_obj::<u8>(GuestAddress(0)).unwrap(), 0);
    assert_eq!(
        restored.read_obj::<u8>(GuestAddress(1 << 32)).unwrap(),
        0x73
    );
    assert_eq!(
        restored
            .read_obj::<u8>(GuestAddress((1 << 32) + (2 * PAGE) as u64))
            .unwrap(),
        0x91
    );
}

#[test]
fn manifest_handoff_retains_private_checkpoint_after_owner_drops() {
    use crate::snapshot::MemoryRegionDesc;
    use std::os::unix::fs::PermissionsExt;
    let file = crate::builder::create_guest_ram_memfd(3 * PAGE).unwrap();
    file.write_all_at(&[0x79; 3 * PAGE], 0).unwrap();
    let readonly = File::open(format!("/proc/self/fd/{}", file.as_raw_fd())).unwrap();
    file.set_permissions(std::fs::Permissions::from_mode(0o000))
        .unwrap();
    let base = Generation::from_immutable_file(
        &[MemoryRegionDesc {
            gpa: 0,
            len: (3 * PAGE) as u64,
        }],
        &readonly,
    )
    .unwrap();
    let parent = base.restore().unwrap();
    parent
        .write_slice(&[0; PAGE], GuestAddress(PAGE as u64))
        .unwrap();
    let (snapshot, copied) = base.capture_quiesced(&parent).unwrap();
    assert_eq!(copied, PAGE);
    let socket = std::env::temp_dir().join(format!("krun-generation-fds-{}", std::process::id()));
    let (manifest, service) = snapshot.publish_manifest(&socket).unwrap();
    let imported = Generation::decode_manifest(&manifest).unwrap();
    drop(service);
    drop(snapshot);
    drop(base);
    drop(parent);
    drop(file);
    drop(readonly);
    let child = imported.restore().unwrap();
    assert_eq!(child.read_obj::<u8>(GuestAddress(0)).unwrap(), 0x79);
    assert_eq!(child.read_obj::<u8>(GuestAddress(PAGE as u64)).unwrap(), 0);
    assert_eq!(
        child
            .read_obj::<u8>(GuestAddress((2 * PAGE) as u64))
            .unwrap(),
        0x79
    );
    assert!(Generation::decode_manifest(&manifest).is_err());
}

#[cfg(target_arch = "x86_64")]
#[test]
fn deferred_checkpoint_preserves_boundary_after_source_continues() {
    use crate::snapshot::{DeferredMemorySave, MemoryRegionDesc};
    let file = crate::builder::create_guest_ram_memfd(3 * PAGE).unwrap();
    file.write_all_at(&[0x31; 3 * PAGE], 0).unwrap();
    let base = Generation::from_immutable_file(
        &[MemoryRegionDesc {
            gpa: 0,
            len: (3 * PAGE) as u64,
        }],
        &file,
    )
    .unwrap();
    let source = base.restore().unwrap();
    source
        .write_slice(&[0; PAGE], GuestAddress(PAGE as u64))
        .unwrap();
    let (saved, copied) = base.capture_quiesced(&source).unwrap();
    assert_eq!(copied, PAGE);
    saved.rebase_quiesced(&source).unwrap();
    let stream = DeferredMemorySave::from_layered(saved.clone());
    let sparse = DeferredMemorySave::from_layered(saved);
    source
        .write_slice(&[0x99; 3 * PAGE], GuestAddress(0))
        .unwrap();
    drop(base);
    drop(file);
    let mut expected = vec![0x31; 3 * PAGE];
    expected[PAGE..2 * PAGE].fill(0);
    let mut wire = Vec::new();
    let regions = stream.finish_stream(&mut wire).unwrap();
    assert_eq!(regions.len(), 1);
    assert_eq!(&wire[..8], b"SMOLRAM1");
    assert_eq!(
        u64::from_le_bytes(wire[8..16].try_into().unwrap()),
        (3 * PAGE) as u64
    );
    assert_eq!(&wire[16..], expected);
    let mut output = crate::builder::create_guest_ram_memfd(4 * PAGE).unwrap();
    output.write_all_at(&[0x77; 4 * PAGE], 0).unwrap();
    sparse.finish(&mut output).unwrap();
    assert_eq!(output.metadata().unwrap().len(), (3 * PAGE) as u64);
    let mut bytes = vec![0; 3 * PAGE];
    output.read_exact_at(&mut bytes, 0).unwrap();
    assert_eq!(bytes, expected);
    assert_eq!(source.read_obj::<u8>(GuestAddress(0)).unwrap(), 0x99);
}

#[test]
fn malformed_generation_descriptions_are_rejected_before_mapping() {
    use crate::snapshot::MemoryRegionDesc;
    let file = crate::builder::create_guest_ram_memfd(2 * PAGE).unwrap();
    let generation = Generation::from_immutable_file(
        &[MemoryRegionDesc {
            gpa: 0,
            len: (2 * PAGE) as u64,
        }],
        &file,
    )
    .unwrap();
    let valid = generation.descriptions();
    let mut cases = Vec::new();
    let mut bad = valid.clone();
    bad[0].extents[0].start = PAGE as u64;
    cases.push(bad);
    let mut bad = valid.clone();
    bad[0].extents[0].len = PAGE as u64;
    cases.push(bad);
    let mut bad = valid.clone();
    bad[0].extents[0].offset = PAGE as u64;
    cases.push(bad);
    let mut bad = valid.clone();
    bad[0].extents[0].fd = -1;
    cases.push(bad);
    let mut bad = valid.clone();
    bad[0].gpa = u64::MAX;
    cases.push(bad);
    let mut bad = valid.clone();
    bad.push(bad[0].clone());
    cases.push(bad);
    let mut bad = valid.clone();
    bad[0].extents.clear();
    cases.push(bad);
    let mut bad = valid.clone();
    bad[0].extents[0].len = 1;
    cases.push(bad);
    let mut bad = valid.clone();
    bad[0].extents[0].offset = 1;
    cases.push(bad);
    for bad in cases {
        assert!(Generation::from_descriptions(std::process::id() as i32, &bad).is_err());
    }
    assert!(Generation::from_descriptions(0, &valid).is_err());
    assert!(Generation::from_descriptions(std::process::id() as i32, &[]).is_err());
}

#[test]
fn manifest_rejects_truncation_trailing_data_and_stale_owner() {
    let file = crate::builder::create_guest_ram_memfd(PAGE).unwrap();
    let generation = Generation::from_immutable_file(
        &[crate::snapshot::MemoryRegionDesc {
            gpa: 0,
            len: PAGE as u64,
        }],
        &file,
    )
    .unwrap();
    let bytes = generation.encode_manifest().unwrap();
    for len in 0..bytes.len() {
        assert!(
            Generation::decode_manifest(&bytes[..len]).is_err(),
            "prefix {len}"
        );
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(Generation::decode_manifest(&trailing).is_err());
    let mut stale = bytes.clone();
    stale[16] ^= 1;
    assert!(Generation::decode_manifest(&stale).is_err());
    let imported = Generation::decode_manifest(&bytes).unwrap();
    drop(generation);
    drop(file);
    assert_eq!(
        imported
            .restore()
            .unwrap()
            .read_obj::<u8>(GuestAddress(0))
            .unwrap(),
        0
    );
}

#[test]
fn manifest_imports_backing_in_an_independent_process() {
    const INPUT: &str = "LIBKRUN_LAYERED_TEST_MANIFEST";
    if let Some(path) = std::env::var_os(INPUT) {
        let bytes = std::fs::read(path).unwrap();
        let image = Generation::decode_manifest(&bytes).unwrap();
        let memory = image.restore().unwrap();
        assert_eq!(memory.read_obj::<u8>(GuestAddress(0)).unwrap(), 0x42);
        assert_eq!(
            memory.read_obj::<u8>(GuestAddress(PAGE as u64)).unwrap(),
            0x73
        );
        memory.write_slice(&[0x91], GuestAddress(0)).unwrap();
        let (next, _) = image.capture_quiesced(&memory).unwrap();
        let grandchild = next.restore().unwrap();
        assert_eq!(grandchild.read_obj::<u8>(GuestAddress(0)).unwrap(), 0x91);
        return;
    }
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let file = crate::builder::create_guest_ram_memfd(2 * PAGE).unwrap();
    file.write_all_at(&[0x73; PAGE], PAGE as u64).unwrap();
    let image = Generation::from_immutable_file(
        &[crate::snapshot::MemoryRegionDesc {
            gpa: 0,
            len: (2 * PAGE) as u64,
        }],
        &file,
    )
    .unwrap();
    let memory = image.restore().unwrap();
    memory.write_slice(&[0x42; PAGE], GuestAddress(0)).unwrap();
    let (captured, _) = image.capture_quiesced(&memory).unwrap();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path =
        std::env::temp_dir().join(format!("layered-manifest-{}-{nonce}", std::process::id()));
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    output
        .write_all(&captured.encode_manifest().unwrap())
        .unwrap();
    drop(output);
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "layered_restore::manifest_imports_backing_in_an_independent_process",
            "--nocapture",
        ])
        .env(INPUT, &path)
        .status()
        .unwrap();
    std::fs::remove_file(path).unwrap();
    assert!(status.success());
    assert_eq!(memory.read_obj::<u8>(GuestAddress(0)).unwrap(), 0x42);
}

#[test]
fn unlinked_readonly_file_outlives_parents_and_generations() {
    use std::os::unix::fs::OpenOptionsExt;
    const LEN: usize = 32 * PAGE;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("layered-ram-{}-{nonce}", std::process::id()));
    let file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    file.write_all_at(&[0x73; LEN], 0).unwrap();
    drop(file);
    let file = File::open(&path).unwrap();
    let image = Image::from_immutable_file(file, LEN).unwrap();
    let parent = image.restore().unwrap();
    std::fs::remove_file(&path).unwrap();
    parent
        .memory
        .write_slice(&[0x42; PAGE], GuestAddress(0))
        .unwrap();
    let (first, _) = parent.capture_quiesced().unwrap();
    let child = first.restore().unwrap();
    child
        .memory
        .write_slice(&[0; PAGE], GuestAddress(PAGE as u64))
        .unwrap();
    let (second, _) = child.capture_quiesced().unwrap();
    drop(parent);
    drop(child);
    drop(image);
    drop(first);
    let grandchild = second.restore().unwrap();
    drop(second);
    assert_eq!(
        grandchild.memory.read_obj::<u8>(GuestAddress(0)).unwrap(),
        0x42
    );
    assert_eq!(
        grandchild
            .memory
            .read_obj::<u8>(GuestAddress(PAGE as u64))
            .unwrap(),
        0
    );
    assert_eq!(
        grandchild
            .memory
            .read_obj::<u8>(GuestAddress((LEN - PAGE) as u64))
            .unwrap(),
        0x73
    );
}

#[test]
fn repeated_capture_reuses_previous_private_pages() {
    let file = crate::builder::create_guest_ram_memfd(16 * PAGE).unwrap();
    let image = Image::from_immutable_file(file, 16 * PAGE).unwrap();
    let mut instance = image.restore().unwrap();
    instance
        .memory
        .write_slice(&[0x73; PAGE], GuestAddress(0))
        .unwrap();
    let (first, copied) = instance.capture_quiesced().unwrap();
    assert_eq!(copied, PAGE);
    let address = instance.memory.get_host_address(GuestAddress(0)).unwrap();
    instance.rebase_quiesced(first.clone()).unwrap();
    assert_eq!(
        instance.memory.get_host_address(GuestAddress(0)).unwrap(),
        address
    );
    let (_, copied) = instance.capture_quiesced().unwrap();
    assert_eq!(copied, 0);
    instance
        .memory
        .write_slice(&[0x91; PAGE], GuestAddress(PAGE as u64))
        .unwrap();
    let (second, copied) = instance.capture_quiesced().unwrap();
    assert_eq!(copied, PAGE);
    instance.rebase_quiesced(second.clone()).unwrap();
    let before = first.restore().unwrap();
    let after = second.restore().unwrap();
    assert_eq!(before.memory.read_obj::<u8>(GuestAddress(0)).unwrap(), 0x73);
    assert_eq!(
        before
            .memory
            .read_obj::<u8>(GuestAddress(PAGE as u64))
            .unwrap(),
        0
    );
    assert_eq!(after.memory.read_obj::<u8>(GuestAddress(0)).unwrap(), 0x73);
    assert_eq!(
        after
            .memory
            .read_obj::<u8>(GuestAddress(PAGE as u64))
            .unwrap(),
        0x91
    );
}

#[test]
fn sixty_four_generations_match_full_memory_model() {
    const LEN: usize = 128 * PAGE;
    let file = crate::builder::create_guest_ram_memfd(LEN).unwrap();
    let mut image = Image::from_immutable_file(file, LEN).unwrap();
    let mut expected = vec![0; LEN];
    let mut seed = 0x713_4179_u64;
    for generation in 0..64_u8 {
        let mut instance = image.restore().unwrap();
        for _ in 0..7 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let offset = (seed as usize % 128) * PAGE;
            let value = if generation % 3 == 0 { 0 } else { generation };
            expected[offset..offset + PAGE].fill(value);
            instance
                .memory
                .write_slice(
                    &expected[offset..offset + PAGE],
                    GuestAddress(offset as u64),
                )
                .unwrap();
        }
        let (next, _) = instance.capture_quiesced().unwrap();
        instance.rebase_quiesced(next.clone()).unwrap();
        let mut actual = vec![0; LEN];
        instance
            .memory
            .read_slice(&mut actual, GuestAddress(0))
            .unwrap();
        assert_eq!(actual, expected, "generation {generation}");
        let restored = next.restore().unwrap();
        restored
            .memory
            .read_slice(&mut actual, GuestAddress(0))
            .unwrap();
        assert_eq!(actual, expected, "restored generation {generation}");
        image = next;
    }
}

#[test]
#[ignore = "8 GiB address-space test, not an 8 GiB resident application"]
fn eight_gib_address_space_branches_without_full_backing_copy() {
    use std::time::Instant;
    const LEN: usize = 8 * 1024 * 1024 * 1024;
    let file = crate::builder::create_guest_ram_memfd(LEN).unwrap();
    file.write_all_at(&[0x73; PAGE], (LEN - PAGE) as u64)
        .unwrap();
    let image = Image::from_immutable_file(file, LEN).unwrap();
    let begin = Instant::now();
    let parent = image.restore().unwrap();
    let map_us = begin.elapsed().as_micros();
    parent
        .memory
        .write_slice(&[0x42; PAGE], GuestAddress(0))
        .unwrap();
    let begin = Instant::now();
    let (first, copied) = parent.capture_quiesced().unwrap();
    let capture_us = begin.elapsed().as_micros();
    assert_eq!(copied, PAGE);
    let child = first.restore().unwrap();
    child
        .memory
        .write_slice(&[0x91; PAGE], GuestAddress(PAGE as u64))
        .unwrap();
    let (second, nested_copied) = child.capture_quiesced().unwrap();
    assert_eq!(nested_copied, PAGE);
    drop(parent);
    drop(child);
    drop(image);
    drop(first);
    let grandchild = second.restore().unwrap();
    for (offset, value) in [(0, 0x42), (PAGE, 0x91), (LEN - PAGE, 0x73)] {
        assert_eq!(
            grandchild
                .memory
                .read_obj::<u8>(GuestAddress(offset as u64))
                .unwrap(),
            value
        );
    }
    println!(
        "layered_address_space bytes={LEN} map_us={map_us} capture_us={capture_us} copied={copied} nested_copied={nested_copied}"
    );
}

#[test]
fn nested_layers_preserve_modified_unread_and_zero_pages() {
    const LEN: usize = 32 * 1024 * 1024;
    let file = crate::builder::create_guest_ram_memfd(LEN).unwrap();
    file.write_all_at(&[0x73; PAGE], (LEN - PAGE) as u64)
        .unwrap();
    file.write_all_at(&[0x11; PAGE], PAGE as u64).unwrap();
    let image = Image::from_immutable_file(file, LEN).unwrap();
    let parent = image.restore().unwrap();
    parent
        .memory
        .write_slice(&[0x42; PAGE], GuestAddress(0))
        .unwrap();
    let (first, copied) = parent.capture_quiesced().unwrap();
    assert_eq!(copied, PAGE);
    let child = first.restore().unwrap();
    let sibling = first.restore().unwrap();
    child
        .memory
        .write_slice(&[0; PAGE], GuestAddress(PAGE as u64))
        .unwrap();
    child
        .memory
        .write_slice(&[0x91; PAGE], GuestAddress(0))
        .unwrap();
    let (second, copied) = child.capture_quiesced().unwrap();
    assert_eq!(copied, 2 * PAGE);
    drop(parent);
    drop(child);
    drop(first);
    drop(image);
    let grandchild = second.restore().unwrap();
    let mut bytes = [0; PAGE];
    for (offset, value) in [(0, 0x91), (PAGE, 0), (LEN - PAGE, 0x73)] {
        grandchild
            .memory
            .read_slice(&mut bytes, GuestAddress(offset as u64))
            .unwrap();
        assert_eq!(bytes, [value; PAGE]);
    }
    sibling
        .memory
        .read_slice(&mut bytes, GuestAddress(0))
        .unwrap();
    assert_eq!(bytes, [0x42; PAGE]);
    sibling
        .memory
        .read_slice(&mut bytes, GuestAddress(PAGE as u64))
        .unwrap();
    assert_eq!(bytes, [0x11; PAGE]);
    let (_, copied) = grandchild.capture_quiesced().unwrap();
    assert_eq!(copied, 0, "read-only faults must not become copied deltas");
}

#[cfg(target_arch = "x86_64")]
#[test]
fn kvm_writes_are_preserved_in_nested_layers() {
    use kvm_bindings::{kvm_regs, kvm_userspace_memory_region};
    use kvm_ioctls::{Kvm, VcpuExit};
    fn run(instance: &Instance, value: u8) {
        let kvm = Kvm::new().unwrap();
        let vm = kvm.create_vm().unwrap();
        // SAFETY: the instance retains the full backing reservation until
        // after every vCPU exits and the VM descriptor is dropped.
        unsafe {
            vm.set_user_memory_region(kvm_userspace_memory_region {
                slot: 0,
                guest_phys_addr: 0,
                memory_size: instance.image.len as u64,
                userspace_addr: instance.memory.get_host_address(GuestAddress(0)).unwrap() as u64,
                flags: 0,
            })
        }
        .unwrap();
        std::thread::scope(|scope| {
            for id in 0..8 {
                let mut cpu = vm.create_vcpu(id).unwrap();
                let mut sregs = cpu.get_sregs().unwrap();
                sregs.cs.base = 0;
                sregs.cs.selector = 0;
                sregs.ds.base = 0;
                sregs.ds.selector = 0;
                cpu.set_sregs(&sregs).unwrap();
                cpu.set_regs(&kvm_regs {
                    rip: 0,
                    rflags: 2,
                    rax: value as u64,
                    rbx: 0x4000 + id * PAGE as u64,
                    ..Default::default()
                })
                .unwrap();
                scope.spawn(move || assert!(matches!(cpu.run().unwrap(), VcpuExit::Hlt)));
            }
        });
    }
    let file = crate::builder::create_guest_ram_memfd(2 * 1024 * 1024).unwrap();
    // 16-bit real mode: mov [bx], al; hlt. Code remains shared/read-only.
    file.write_all_at(&[0x88, 0x07, 0xf4], 0).unwrap();
    let image = Image::from_immutable_file(file, 2 * 1024 * 1024).unwrap();
    let first = image.restore().unwrap();
    run(&first, 0x5A);
    let (generation, copied) = first.capture_quiesced().unwrap();
    assert_eq!(copied, 8 * PAGE);
    let second = generation.restore().unwrap();
    run(&second, 0x91);
    let (nested, copied) = second.capture_quiesced().unwrap();
    assert_eq!(copied, 8 * PAGE);
    let third = nested.restore().unwrap();
    let sibling = generation.restore().unwrap();
    drop(image);
    drop(first);
    drop(second);
    drop(generation);
    drop(nested);
    for id in 0..8 {
        let address = GuestAddress(0x4000 + id * PAGE as u64);
        assert_eq!(third.memory.read_obj::<u8>(address).unwrap(), 0x91);
        assert_eq!(sibling.memory.read_obj::<u8>(address).unwrap(), 0x5A);
    }
}
