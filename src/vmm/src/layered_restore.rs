// Copyright 2026. SPDX-License-Identifier: Apache-2.0
//! Experimental generation backend. Not connected to VM restore/admission.
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

#[derive(Clone)]
struct Extent {
    start: usize,
    end: usize,
    file: Arc<File>,
}

#[derive(Clone)]
struct Image {
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
    fn from_immutable_file(file: File, len: usize) -> io::Result<Self> {
        if len == 0 || !len.is_multiple_of(PAGE) || file.metadata()?.len() != len as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "RAM image size mismatch",
            ));
        }
        Ok(Self {
            len,
            extents: vec![Extent {
                start: 0,
                end: len,
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
        let region = GuestRegionMmap::new(region, GuestAddress(0))
            .ok_or_else(|| io::Error::other("RAM address overflow"))?;
        let memory = GuestMemoryMmap::from_regions(vec![region])
            .map_err(|error| io::Error::other(format!("layered RAM regions: {error:?}")))?;
        let base = memory.get_host_address(GuestAddress(0)).unwrap();
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
                    extent.start as libc::off_t,
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
        let base = self.memory.get_host_address(GuestAddress(0)).unwrap();
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
                    extent.start as libc::off_t,
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
        let base = self.memory.get_host_address(GuestAddress(0)).unwrap() as u64;
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
                    .read_slice(&mut buffer[..len], GuestAddress(offset as u64))
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
        for old in &self.image.extents {
            let mut cursor = old.start;
            for &(start, end) in &dirty {
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
                    file: old.file.clone(),
                });
            }
        }
        for (start, end) in dirty {
            extents.push(Extent {
                start,
                end,
                file: delta.clone(),
            });
        }
        extents.sort_unstable_by_key(|extent| extent.start);
        let mut cursor = 0;
        for extent in &extents {
            assert_eq!(extent.start, cursor);
            cursor = extent.end;
        }
        assert_eq!(cursor, self.image.len);
        Ok((
            Image {
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
