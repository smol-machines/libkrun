//! Module containing abstracts for dealing with contiguous regions of guest memory

use crate::bitmap::{Bitmap, BS};
use crate::guest_memory::Result;
use crate::{
    Address, AtomicAccess, Bytes, FileOffset, GuestAddress, GuestMemory, GuestMemoryError,
    GuestUsize, MemoryRegionAddress, ReadVolatile, VolatileSlice, WriteVolatile,
};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};

/// Represents a continuous region of guest physical memory.
///
/// Note that the [`Bytes`] super trait requirement can be satisfied by implementing
/// [`GuestMemoryRegionBytes`], which provides a default implementation of `Bytes`
/// for memory regions that are backed by physical RAM:
///
/// ```
/// ///
/// use vm_memory::bitmap::BS;
/// use vm_memory::{GuestAddress, GuestMemoryRegion, GuestMemoryRegionBytes, GuestUsize};
///
/// struct MyRegion;
///
/// impl GuestMemoryRegion for MyRegion {
///     type B = ();
///     fn len(&self) -> GuestUsize {
///         todo!()
///     }
///     fn start_addr(&self) -> GuestAddress {
///         todo!()
///     }
///     fn bitmap(&self) {
///         todo!()
///     }
/// }
///
/// impl GuestMemoryRegionBytes for MyRegion {}
/// ```
#[allow(clippy::len_without_is_empty)]
pub trait GuestMemoryRegion: Bytes<MemoryRegionAddress, E = GuestMemoryError> {
    /// Type used for dirty memory tracking.
    type B: Bitmap;

    /// Returns the size of the region.
    fn len(&self) -> GuestUsize;

    /// Returns the minimum (inclusive) address managed by the region.
    fn start_addr(&self) -> GuestAddress;

    /// Returns the maximum (inclusive) address managed by the region.
    fn last_addr(&self) -> GuestAddress {
        // unchecked_add is safe as the region bounds were checked when it was created.
        self.start_addr().unchecked_add(self.len() - 1)
    }

    /// Borrow the associated `Bitmap` object.
    fn bitmap(&self) -> BS<'_, Self::B>;

    /// Returns the given address if it is within this region.
    fn check_address(&self, addr: MemoryRegionAddress) -> Option<MemoryRegionAddress> {
        if self.address_in_range(addr) {
            Some(addr)
        } else {
            None
        }
    }

    /// Returns `true` if the given address is within this region.
    fn address_in_range(&self, addr: MemoryRegionAddress) -> bool {
        addr.raw_value() < self.len()
    }

    /// Returns the address plus the offset if it is in this region.
    fn checked_offset(
        &self,
        base: MemoryRegionAddress,
        offset: usize,
    ) -> Option<MemoryRegionAddress> {
        base.checked_add(offset as u64)
            .and_then(|addr| self.check_address(addr))
    }

    /// Tries to convert an absolute address to a relative address within this region.
    ///
    /// Returns `None` if `addr` is out of the bounds of this region.
    fn to_region_addr(&self, addr: GuestAddress) -> Option<MemoryRegionAddress> {
        addr.checked_offset_from(self.start_addr())
            .and_then(|offset| self.check_address(MemoryRegionAddress(offset)))
    }

    /// Returns the host virtual address corresponding to the region address.
    ///
    /// Some [`GuestMemory`](trait.GuestMemory.html) implementations, like `GuestMemoryMmap`,
    /// have the capability to mmap guest address range into host virtual address space for
    /// direct access, so the corresponding host virtual address may be passed to other subsystems.
    ///
    /// # Note
    /// The underlying guest memory is not protected from memory aliasing, which breaks the
    /// Rust memory safety model. It's the caller's responsibility to ensure that there's no
    /// concurrent accesses to the underlying guest memory.
    fn get_host_address(&self, _addr: MemoryRegionAddress) -> Result<*mut u8> {
        Err(GuestMemoryError::HostAddressNotAvailable)
    }

    /// Returns information regarding the file and offset backing this memory region.
    fn file_offset(&self) -> Option<&FileOffset> {
        None
    }

    /// Returns a [`VolatileSlice`](struct.VolatileSlice.html) of `count` bytes starting at
    /// `offset`.
    #[allow(unused_variables)]
    fn get_slice(
        &self,
        offset: MemoryRegionAddress,
        count: usize,
    ) -> Result<VolatileSlice<BS<Self::B>>> {
        Err(GuestMemoryError::HostAddressNotAvailable)
    }

    /// Gets a slice of memory for the entire region that supports volatile access.
    ///
    /// # Examples (uses the `backend-mmap` feature)
    ///
    /// ```
    /// # #[cfg(feature = "backend-mmap")]
    /// # {
    /// # use vm_memory::{GuestAddress, MmapRegion, GuestRegionMmap, GuestMemoryRegion};
    /// # use vm_memory::volatile_memory::{VolatileMemory, VolatileSlice, VolatileRef};
    /// #
    /// let region = GuestRegionMmap::<()>::from_range(GuestAddress(0x0), 0x400, None)
    ///     .expect("Could not create guest memory");
    /// let slice = region
    ///     .as_volatile_slice()
    ///     .expect("Could not get volatile slice");
    ///
    /// let v = 42u32;
    /// let r = slice
    ///     .get_ref::<u32>(0x200)
    ///     .expect("Could not get reference");
    /// r.store(v);
    /// assert_eq!(r.load(), v);
    /// # }
    /// ```
    fn as_volatile_slice(&self) -> Result<VolatileSlice<BS<Self::B>>> {
        self.get_slice(MemoryRegionAddress(0), self.len() as usize)
    }

    /// Show if the region is based on the `HugeTLBFS`.
    /// Returns Some(true) if the region is backed by hugetlbfs.
    /// None represents that no information is available.
    ///
    /// # Examples (uses the `backend-mmap` feature)
    ///
    /// ```
    /// # #[cfg(feature = "backend-mmap")]
    /// # {
    /// #   use vm_memory::{GuestAddress, GuestMemory, GuestMemoryMmap, GuestRegionMmap};
    /// let addr = GuestAddress(0x1000);
    /// let mem = GuestMemoryMmap::<()>::from_ranges(&[(addr, 0x1000)]).unwrap();
    /// let r = mem.find_region(addr).unwrap();
    /// assert_eq!(r.is_hugetlbfs(), None);
    /// # }
    /// ```
    #[cfg(target_os = "linux")]
    fn is_hugetlbfs(&self) -> Option<bool> {
        None
    }
}

/// Errors that can occur when dealing with [`GuestRegionCollection`]s
#[derive(Debug, thiserror::Error)]
pub enum GuestRegionCollectionError {
    /// No memory region found.
    #[error("No memory region found")]
    NoMemoryRegion,
    /// Some of the memory regions intersect with each other.
    #[error("Some of the memory regions intersect with each other")]
    MemoryRegionOverlap,
    /// The provided memory regions haven't been sorted.
    #[error("The provided memory regions haven't been sorted")]
    UnsortedMemoryRegions,
    /// Shared append-only growth has not been enabled for this collection.
    #[error("Shared memory growth is not enabled")]
    GrowthNotEnabled,
    /// A previous writer failed while publishing a memory region.
    #[error("Shared memory growth lock is poisoned")]
    GrowthPoisoned,
}

#[derive(Debug)]
struct AppendedRegion<R> {
    region: Arc<R>,
    next: OnceLock<Box<AppendedRegion<R>>>,
}

#[derive(Debug)]
struct SharedGrowth<R> {
    head: OnceLock<Box<AppendedRegion<R>>>,
    writer: Mutex<()>,
}

/// [`GuestMemory`](trait.GuestMemory.html) implementation based on a homogeneous collection
/// of [`GuestMemoryRegion`] implementations.
///
/// Represents a sorted set of non-overlapping physical guest memory regions.
#[derive(Debug)]
pub struct GuestRegionCollection<R> {
    regions: Vec<Arc<R>>,
    growth: Option<Arc<SharedGrowth<R>>>,
}

impl<R> Default for GuestRegionCollection<R> {
    fn default() -> Self {
        Self {
            regions: Vec::new(),
            growth: None,
        }
    }
}

impl<R> Clone for GuestRegionCollection<R> {
    fn clone(&self) -> Self {
        GuestRegionCollection {
            regions: self.regions.iter().map(Arc::clone).collect(),
            growth: self.growth.clone(),
        }
    }
}

impl<R: GuestMemoryRegion> GuestRegionCollection<R> {
    /// Enable shared, append-only growth before cloning this map into devices.
    /// Existing regions never move or disappear, so borrowed slices remain
    /// valid. Later clones observe appended regions without a stale map cache.
    ///
    /// The caller must serialize whole-map operations (snapshotting, hypervisor
    /// registration) against append. This is not a coherent multi-call snapshot.
    pub fn with_shared_growth(mut self) -> Self {
        if self.growth.is_none() {
            self.growth = Some(Arc::new(SharedGrowth {
                head: OnceLock::new(),
                writer: Mutex::new(()),
            }));
        }
        self
    }

    fn appended_regions(&self) -> impl Iterator<Item = &Arc<R>> {
        let first = self
            .growth
            .as_ref()
            .and_then(|growth| growth.head.get())
            .map(Box::as_ref);
        std::iter::successors(first, |node| node.next.get().map(Box::as_ref))
            .map(|node| &node.region)
    }

    fn all_regions(&self) -> impl Iterator<Item = &Arc<R>> {
        self.regions.iter().chain(self.appended_regions())
    }

    /// Publish one region above every existing GPA range to all shared views.
    /// The hypervisor mapping must already be ready before publication and the
    /// guest must not be told to use the memory until publication completes.
    /// Rejects overlap and insertion below the current highest region.
    pub fn append_shared_region(
        &self,
        region: Arc<R>,
    ) -> std::result::Result<(), GuestRegionCollectionError> {
        let growth = self
            .growth
            .as_ref()
            .ok_or(GuestRegionCollectionError::GrowthNotEnabled)?;
        let _writer = growth
            .writer
            .lock()
            .map_err(|_| GuestRegionCollectionError::GrowthPoisoned)?;
        let mut slot = &growth.head;
        let mut last = self.regions.last().map(|region| region.last_addr());
        while let Some(node) = slot.get() {
            last = Some(node.region.last_addr());
            slot = &node.next;
        }
        if last.is_some_and(|last| last >= region.start_addr()) {
            return Err(GuestRegionCollectionError::MemoryRegionOverlap);
        }
        slot.set(Box::new(AppendedRegion {
            region,
            next: OnceLock::new(),
        }))
        .map_err(|_| GuestRegionCollectionError::MemoryRegionOverlap)
    }

    /// Creates an empty `GuestMemoryMmap` instance.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a new [`GuestRegionCollection`] from a vector of regions.
    ///
    /// # Arguments
    ///
    /// * `regions` - The vector of regions.
    ///   The regions shouldn't overlap, and they should be sorted
    ///   by the starting address.
    pub fn from_regions(
        mut regions: Vec<R>,
    ) -> std::result::Result<Self, GuestRegionCollectionError> {
        Self::from_arc_regions(regions.drain(..).map(Arc::new).collect())
    }

    /// Creates a new [`GuestRegionCollection`] from a vector of Arc regions.
    ///
    /// Similar to the constructor `from_regions()` as it returns a
    /// [`GuestRegionCollection`]. The need for this constructor is to provide a way for
    /// consumer of this API to create a new [`GuestRegionCollection`] based on existing
    /// regions coming from an existing [`GuestRegionCollection`] instance.
    ///
    /// # Arguments
    ///
    /// * `regions` - The vector of `Arc` regions.
    ///   The regions shouldn't overlap and they should be sorted
    ///   by the starting address.
    pub fn from_arc_regions(
        regions: Vec<Arc<R>>,
    ) -> std::result::Result<Self, GuestRegionCollectionError> {
        if regions.is_empty() {
            return Err(GuestRegionCollectionError::NoMemoryRegion);
        }

        for window in regions.windows(2) {
            let prev = &window[0];
            let next = &window[1];

            if prev.start_addr() > next.start_addr() {
                return Err(GuestRegionCollectionError::UnsortedMemoryRegions);
            }

            if prev.last_addr() >= next.start_addr() {
                return Err(GuestRegionCollectionError::MemoryRegionOverlap);
            }
        }

        Ok(Self {
            regions,
            growth: None,
        })
    }

    /// Insert a region into the `GuestMemoryMmap` object and return a new `GuestMemoryMmap`.
    ///
    /// # Arguments
    /// * `region`: the memory region to insert into the guest memory object.
    pub fn insert_region(
        &self,
        region: Arc<R>,
    ) -> std::result::Result<GuestRegionCollection<R>, GuestRegionCollectionError> {
        let mut regions: Vec<_> = self.all_regions().cloned().collect();
        regions.push(region);
        regions.sort_by_key(|x| x.start_addr());

        Self::from_arc_regions(regions)
    }

    /// Remove a region from the [`GuestRegionCollection`] object and return a new `GuestRegionCollection`
    /// on success, together with the removed region.
    ///
    /// # Arguments
    /// * `base`: base address of the region to be removed
    /// * `size`: size of the region to be removed
    pub fn remove_region(
        &self,
        base: GuestAddress,
        size: GuestUsize,
    ) -> std::result::Result<(GuestRegionCollection<R>, Arc<R>), GuestRegionCollectionError> {
        let mut regions: Vec<_> = self.all_regions().cloned().collect();
        if let Ok(region_index) = regions.binary_search_by_key(&base, |x| x.start_addr()) {
            if regions.get(region_index).unwrap().len() == size {
                let region = regions.remove(region_index);
                return Ok((
                    Self {
                        regions,
                        growth: None,
                    },
                    region,
                ));
            }
        }

        Err(GuestRegionCollectionError::NoMemoryRegion)
    }
}

impl<R: GuestMemoryRegion> GuestMemory for GuestRegionCollection<R> {
    type R = R;

    fn num_regions(&self) -> usize {
        self.regions.len() + self.appended_regions().count()
    }

    fn find_region(&self, addr: GuestAddress) -> Option<&R> {
        let index = match self.regions.binary_search_by_key(&addr, |x| x.start_addr()) {
            Ok(x) => Some(x),
            // Within the closest region with starting address < addr
            Err(x) if (x > 0 && addr <= self.regions[x - 1].last_addr()) => Some(x - 1),
            _ => None,
        };
        index.map(|x| self.regions[x].as_ref()).or_else(|| {
            self.appended_regions()
                .find(|region| region.start_addr() <= addr && addr <= region.last_addr())
                .map(AsRef::as_ref)
        })
    }

    fn iter(&self) -> impl Iterator<Item = &Self::R> {
        self.all_regions().map(AsRef::as_ref)
    }
}

/// A marker trait that if implemented on a type `R` makes available a default
/// implementation of `Bytes<MemoryRegionAddress>` for `R`, based on the assumption
/// that the entire `GuestMemoryRegion` is just traditional memory without any
/// special access requirements.
pub trait GuestMemoryRegionBytes: GuestMemoryRegion {}

impl<R: GuestMemoryRegionBytes> Bytes<MemoryRegionAddress> for R {
    type E = GuestMemoryError;

    /// # Examples
    /// * Write a slice at guest address 0x1200.
    ///
    /// ```
    /// # #[cfg(feature = "backend-mmap")]
    /// # use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};
    /// #
    /// # #[cfg(feature = "backend-mmap")]
    /// # {
    /// # let start_addr = GuestAddress(0x1000);
    /// # let mut gm = GuestMemoryMmap::<()>::from_ranges(&vec![(start_addr, 0x400)])
    /// #    .expect("Could not create guest memory");
    /// #
    /// let res = gm
    ///     .write(&[1, 2, 3, 4, 5], GuestAddress(0x1200))
    ///     .expect("Could not write to guest memory");
    /// assert_eq!(5, res);
    /// # }
    /// ```
    fn write(&self, buf: &[u8], addr: MemoryRegionAddress) -> Result<usize> {
        let maddr = addr.raw_value() as usize;
        self.as_volatile_slice()?
            .write(buf, maddr)
            .map_err(Into::into)
    }

    /// # Examples
    /// * Read a slice of length 16 at guestaddress 0x1200.
    ///
    /// ```
    /// # #[cfg(feature = "backend-mmap")]
    /// # use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};
    /// #
    /// # #[cfg(feature = "backend-mmap")]
    /// # {
    /// # let start_addr = GuestAddress(0x1000);
    /// # let mut gm = GuestMemoryMmap::<()>::from_ranges(&vec![(start_addr, 0x400)])
    /// #    .expect("Could not create guest memory");
    /// #
    /// let buf = &mut [0u8; 16];
    /// let res = gm
    ///     .read(buf, GuestAddress(0x1200))
    ///     .expect("Could not read from guest memory");
    /// assert_eq!(16, res);
    /// # }
    /// ```
    fn read(&self, buf: &mut [u8], addr: MemoryRegionAddress) -> Result<usize> {
        let maddr = addr.raw_value() as usize;
        self.as_volatile_slice()?
            .read(buf, maddr)
            .map_err(Into::into)
    }

    fn write_slice(&self, buf: &[u8], addr: MemoryRegionAddress) -> Result<()> {
        let maddr = addr.raw_value() as usize;
        self.as_volatile_slice()?
            .write_slice(buf, maddr)
            .map_err(Into::into)
    }

    fn read_slice(&self, buf: &mut [u8], addr: MemoryRegionAddress) -> Result<()> {
        let maddr = addr.raw_value() as usize;
        self.as_volatile_slice()?
            .read_slice(buf, maddr)
            .map_err(Into::into)
    }

    fn read_volatile_from<F>(
        &self,
        addr: MemoryRegionAddress,
        src: &mut F,
        count: usize,
    ) -> Result<usize>
    where
        F: ReadVolatile,
    {
        self.as_volatile_slice()?
            .read_volatile_from(addr.0 as usize, src, count)
            .map_err(Into::into)
    }

    fn read_exact_volatile_from<F>(
        &self,
        addr: MemoryRegionAddress,
        src: &mut F,
        count: usize,
    ) -> Result<()>
    where
        F: ReadVolatile,
    {
        self.as_volatile_slice()?
            .read_exact_volatile_from(addr.0 as usize, src, count)
            .map_err(Into::into)
    }

    fn write_volatile_to<F>(
        &self,
        addr: MemoryRegionAddress,
        dst: &mut F,
        count: usize,
    ) -> Result<usize>
    where
        F: WriteVolatile,
    {
        self.as_volatile_slice()?
            .write_volatile_to(addr.0 as usize, dst, count)
            .map_err(Into::into)
    }

    fn write_all_volatile_to<F>(
        &self,
        addr: MemoryRegionAddress,
        dst: &mut F,
        count: usize,
    ) -> Result<()>
    where
        F: WriteVolatile,
    {
        self.as_volatile_slice()?
            .write_all_volatile_to(addr.0 as usize, dst, count)
            .map_err(Into::into)
    }

    fn store<T: AtomicAccess>(
        &self,
        val: T,
        addr: MemoryRegionAddress,
        order: Ordering,
    ) -> Result<()> {
        self.as_volatile_slice().and_then(|s| {
            s.store(val, addr.raw_value() as usize, order)
                .map_err(Into::into)
        })
    }

    fn load<T: AtomicAccess>(&self, addr: MemoryRegionAddress, order: Ordering) -> Result<T> {
        self.as_volatile_slice()
            .and_then(|s| s.load(addr.raw_value() as usize, order).map_err(Into::into))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::region::{GuestMemoryRegionBytes, GuestRegionCollectionError};
    use crate::{
        Address, GuestAddress, GuestMemory, GuestMemoryRegion, GuestRegionCollection, GuestUsize,
    };
    use matches::assert_matches;
    use std::sync::Arc;

    #[derive(Debug, PartialEq, Eq)]
    pub(crate) struct MockRegion {
        pub(crate) start: GuestAddress,
        pub(crate) len: GuestUsize,
    }

    impl GuestMemoryRegion for MockRegion {
        type B = ();

        fn len(&self) -> GuestUsize {
            self.len
        }

        fn start_addr(&self) -> GuestAddress {
            self.start
        }

        fn bitmap(&self) {}
    }

    impl GuestMemoryRegionBytes for MockRegion {}

    pub(crate) type Collection = GuestRegionCollection<MockRegion>;

    fn check_guest_memory_mmap(
        maybe_guest_mem: Result<Collection, GuestRegionCollectionError>,
        expected_regions_summary: &[(GuestAddress, u64)],
    ) {
        assert!(maybe_guest_mem.is_ok());

        let guest_mem = maybe_guest_mem.unwrap();
        assert_eq!(guest_mem.num_regions(), expected_regions_summary.len());
        let maybe_last_mem_reg = expected_regions_summary.last();
        if let Some((region_addr, region_size)) = maybe_last_mem_reg {
            let mut last_addr = region_addr.unchecked_add(*region_size);
            if last_addr.raw_value() != 0 {
                last_addr = last_addr.unchecked_sub(1);
            }
            assert_eq!(guest_mem.last_addr(), last_addr);
        }
        for ((region_addr, region_size), mmap) in
            expected_regions_summary.iter().zip(guest_mem.iter())
        {
            assert_eq!(region_addr, &mmap.start);
            assert_eq!(region_size, &mmap.len);

            assert!(guest_mem.find_region(*region_addr).is_some());
        }
    }

    pub(crate) fn new_guest_memory_collection_from_regions(
        regions_summary: &[(GuestAddress, u64)],
    ) -> Result<Collection, GuestRegionCollectionError> {
        Collection::from_regions(
            regions_summary
                .iter()
                .map(|&(start, len)| MockRegion { start, len })
                .collect(),
        )
    }

    fn new_guest_memory_collection_from_arc_regions(
        regions_summary: &[(GuestAddress, u64)],
    ) -> Result<Collection, GuestRegionCollectionError> {
        Collection::from_arc_regions(
            regions_summary
                .iter()
                .map(|&(start, len)| Arc::new(MockRegion { start, len }))
                .collect(),
        )
    }

    #[test]
    fn test_no_memory_region() {
        let regions_summary = [];

        assert_matches!(
            new_guest_memory_collection_from_regions(&regions_summary).unwrap_err(),
            GuestRegionCollectionError::NoMemoryRegion
        );
        assert_matches!(
            new_guest_memory_collection_from_arc_regions(&regions_summary).unwrap_err(),
            GuestRegionCollectionError::NoMemoryRegion
        );
    }

    #[test]
    fn test_overlapping_memory_regions() {
        let regions_summary = [(GuestAddress(0), 100), (GuestAddress(99), 100)];

        assert_matches!(
            new_guest_memory_collection_from_regions(&regions_summary).unwrap_err(),
            GuestRegionCollectionError::MemoryRegionOverlap
        );
        assert_matches!(
            new_guest_memory_collection_from_arc_regions(&regions_summary).unwrap_err(),
            GuestRegionCollectionError::MemoryRegionOverlap
        );
    }

    #[test]
    fn test_unsorted_memory_regions() {
        let regions_summary = [(GuestAddress(100), 100), (GuestAddress(0), 100)];

        assert_matches!(
            new_guest_memory_collection_from_regions(&regions_summary).unwrap_err(),
            GuestRegionCollectionError::UnsortedMemoryRegions
        );
        assert_matches!(
            new_guest_memory_collection_from_arc_regions(&regions_summary).unwrap_err(),
            GuestRegionCollectionError::UnsortedMemoryRegions
        );
    }

    #[test]
    fn test_valid_memory_regions() {
        let regions_summary = [(GuestAddress(0), 100), (GuestAddress(100), 100)];

        let guest_mem = Collection::new();
        assert_eq!(guest_mem.num_regions(), 0);

        check_guest_memory_mmap(
            new_guest_memory_collection_from_regions(&regions_summary),
            &regions_summary,
        );

        check_guest_memory_mmap(
            new_guest_memory_collection_from_arc_regions(&regions_summary),
            &regions_summary,
        );
    }

    #[test]
    fn test_mmap_insert_region() {
        let region_size = 0x1000;
        let regions = vec![
            (GuestAddress(0x0), region_size),
            (GuestAddress(0x10_0000), region_size),
        ];
        let mem_orig = new_guest_memory_collection_from_regions(&regions).unwrap();
        let mut gm = mem_orig.clone();
        assert_eq!(mem_orig.num_regions(), 2);

        let new_regions = [
            (GuestAddress(0x8000), 0x1000),
            (GuestAddress(0x4000), 0x1000),
            (GuestAddress(0xc000), 0x1000),
        ];

        for (start, len) in new_regions {
            gm = gm
                .insert_region(Arc::new(MockRegion { start, len }))
                .unwrap();
        }

        gm.insert_region(Arc::new(MockRegion {
            start: GuestAddress(0xc000),
            len: 0x1000,
        }))
        .unwrap_err();

        assert_eq!(mem_orig.num_regions(), 2);
        assert_eq!(gm.num_regions(), 5);

        let regions = gm.iter().collect::<Vec<_>>();

        assert_eq!(regions[0].start_addr(), GuestAddress(0x0000));
        assert_eq!(regions[1].start_addr(), GuestAddress(0x4000));
        assert_eq!(regions[2].start_addr(), GuestAddress(0x8000));
        assert_eq!(regions[3].start_addr(), GuestAddress(0xc000));
        assert_eq!(regions[4].start_addr(), GuestAddress(0x10_0000));
    }

    #[test]
    fn test_mmap_remove_region() {
        let region_size = 0x1000;
        let regions = vec![
            (GuestAddress(0x0), region_size),
            (GuestAddress(0x10_0000), region_size),
        ];
        let mem_orig = new_guest_memory_collection_from_regions(&regions).unwrap();
        let gm = mem_orig.clone();
        assert_eq!(mem_orig.num_regions(), 2);

        gm.remove_region(GuestAddress(0), 128).unwrap_err();
        gm.remove_region(GuestAddress(0x4000), 128).unwrap_err();
        let (gm, region) = gm.remove_region(GuestAddress(0x10_0000), 0x1000).unwrap();

        assert_eq!(mem_orig.num_regions(), 2);
        assert_eq!(gm.num_regions(), 1);

        assert_eq!(gm.iter().next().unwrap().start_addr(), GuestAddress(0x0000));
        assert_eq!(region.start_addr(), GuestAddress(0x10_0000));
    }

    #[test]
    fn test_iter() {
        let region_size = 0x400;
        let regions = vec![
            (GuestAddress(0x0), region_size),
            (GuestAddress(0x1000), region_size),
        ];
        let mut iterated_regions = Vec::new();
        let gm = new_guest_memory_collection_from_regions(&regions).unwrap();

        for region in gm.iter() {
            assert_eq!(region.len(), region_size as GuestUsize);
        }

        for region in gm.iter() {
            iterated_regions.push((region.start_addr(), region.len()));
        }
        assert_eq!(regions, iterated_regions);

        assert!(regions
            .iter()
            .map(|x| (x.0, x.1))
            .eq(iterated_regions.iter().copied()));

        let mmap_regions = gm.iter().collect::<Vec<_>>();

        assert_eq!(mmap_regions[0].start, regions[0].0);
        assert_eq!(mmap_regions[1].start, regions[1].0);
    }

    #[test]
    fn test_address_in_range() {
        let start_addr1 = GuestAddress(0x0);
        let start_addr2 = GuestAddress(0x800);
        let guest_mem =
            new_guest_memory_collection_from_regions(&[(start_addr1, 0x400), (start_addr2, 0x400)])
                .unwrap();

        assert!(guest_mem.address_in_range(GuestAddress(0x200)));
        assert!(!guest_mem.address_in_range(GuestAddress(0x600)));
        assert!(guest_mem.address_in_range(GuestAddress(0xa00)));
        assert!(!guest_mem.address_in_range(GuestAddress(0xc00)));
    }

    #[test]
    fn test_check_address() {
        let start_addr1 = GuestAddress(0x0);
        let start_addr2 = GuestAddress(0x800);
        let guest_mem =
            new_guest_memory_collection_from_regions(&[(start_addr1, 0x400), (start_addr2, 0x400)])
                .unwrap();

        assert_eq!(
            guest_mem.check_address(GuestAddress(0x200)),
            Some(GuestAddress(0x200))
        );
        assert_eq!(guest_mem.check_address(GuestAddress(0x600)), None);
        assert_eq!(
            guest_mem.check_address(GuestAddress(0xa00)),
            Some(GuestAddress(0xa00))
        );
        assert_eq!(guest_mem.check_address(GuestAddress(0xc00)), None);
    }

    #[test]
    fn test_checked_offset() {
        let start_addr1 = GuestAddress(0);
        let start_addr2 = GuestAddress(0x800);
        let start_addr3 = GuestAddress(0xc00);
        let guest_mem = new_guest_memory_collection_from_regions(&[
            (start_addr1, 0x400),
            (start_addr2, 0x400),
            (start_addr3, 0x400),
        ])
        .unwrap();

        assert_eq!(
            guest_mem.checked_offset(start_addr1, 0x200),
            Some(GuestAddress(0x200))
        );
        assert_eq!(
            guest_mem.checked_offset(start_addr1, 0xa00),
            Some(GuestAddress(0xa00))
        );
        assert_eq!(
            guest_mem.checked_offset(start_addr2, 0x7ff),
            Some(GuestAddress(0xfff))
        );
        assert_eq!(guest_mem.checked_offset(start_addr2, 0xc00), None);
        assert_eq!(guest_mem.checked_offset(start_addr1, usize::MAX), None);

        assert_eq!(guest_mem.checked_offset(start_addr1, 0x400), None);
        assert_eq!(
            guest_mem.checked_offset(start_addr1, 0x400 - 1),
            Some(GuestAddress(0x400 - 1))
        );
    }
}
