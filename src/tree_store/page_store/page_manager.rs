use crate::db::WriteStrategy;
use crate::transaction_tracker::TransactionId;
use crate::tree_store::btree_base::Checksum;
use crate::tree_store::page_store::base::{PageHint, PhysicalStorage};
use crate::tree_store::page_store::bitmap::{BtreeBitmap, BtreeBitmapMut};
use crate::tree_store::page_store::buddy_allocator::BuddyAllocator;
use crate::tree_store::page_store::cached_file::PagedCachedFile;
use crate::tree_store::page_store::header::{DatabaseHeader, DB_HEADER_SIZE, MAGICNUMBER};
use crate::tree_store::page_store::layout::DatabaseLayout;
use crate::tree_store::page_store::mmap::Mmap;
use crate::tree_store::page_store::region::{RegionHeaderAccessor, RegionHeaderMutator};
use crate::tree_store::page_store::utils::is_page_aligned;
use crate::tree_store::page_store::{hash128_with_seed, PageImpl, PageMut};
use crate::tree_store::PageNumber;
use crate::Error;
use crate::Result;
use std::cmp;
use std::cmp::max;
#[cfg(debug_assertions)]
use std::collections::HashMap;
use std::collections::HashSet;
use std::convert::TryInto;
use std::fs::File;
#[cfg(unix)]
use std::io;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

// Regions have a maximum size of 4GiB. A `4GiB - overhead` value is the largest that can be represented,
// because the leaf node format uses 32bit offsets
const MAX_USABLE_REGION_SPACE: u64 = 4 * 1024 * 1024 * 1024;
// TODO: remove this constant?
pub(crate) const MAX_MAX_PAGE_ORDER: usize = 20;
pub(super) const MIN_USABLE_PAGES: usize = 10;
const MIN_DESIRED_USABLE_BYTES: usize = 1024 * 1024;

// TODO: allocate more tracker space when it becomes exhausted, and remove this hard coded 1000 regions
const NUM_REGIONS: u32 = 1000;

// TODO: set to 1, when version 1.0 is released
pub(crate) const FILE_FORMAT_VERSION: u8 = 109;

fn ceil_log2(x: usize) -> usize {
    if x.is_power_of_two() {
        x.trailing_zeros() as usize
    } else {
        x.next_power_of_two().trailing_zeros() as usize
    }
}

#[derive(Debug, PartialEq, Copy, Clone)]
pub(crate) enum ChecksumType {
    Unused, // No checksum is calculated. Stores arbitrary data
    XXH3_128,
}

impl ChecksumType {
    pub(crate) fn checksum(&self, data: &[u8]) -> Checksum {
        match self {
            ChecksumType::Unused => 0,
            ChecksumType::XXH3_128 => hash128_with_seed(data, 0),
        }
    }
}

impl From<WriteStrategy> for ChecksumType {
    fn from(strategy: WriteStrategy) -> Self {
        match strategy {
            WriteStrategy::Checksum => ChecksumType::XXH3_128,
            WriteStrategy::TwoPhase => ChecksumType::Unused,
        }
    }
}

impl From<u8> for ChecksumType {
    fn from(x: u8) -> Self {
        match x {
            1 => ChecksumType::Unused,
            2 => ChecksumType::XXH3_128,
            _ => unimplemented!(),
        }
    }
}

#[allow(clippy::from_over_into)]
impl Into<u8> for ChecksumType {
    fn into(self) -> u8 {
        match self {
            ChecksumType::Unused => 1,
            ChecksumType::XXH3_128 => 2,
        }
    }
}

// Tracks the page orders that MAY BE free in each region. This data structure is optimistic, so
// a region may not actually have a page free for a given order
//
// Format:
// num_allocators: u32 number of allocators
// allocator_len: u32 length of each allocator
// data: BtreeBitmap data for each order
pub(crate) struct RegionTracker<'a> {
    data: &'a mut [u8],
}

impl<'a> RegionTracker<'a> {
    pub(crate) fn new(data: &'a mut [u8]) -> Self {
        Self { data }
    }

    pub(crate) fn required_bytes(regions: u32, orders: usize) -> usize {
        2 * size_of::<u32>() + orders * BtreeBitmapMut::required_space(regions.try_into().unwrap())
    }

    pub(crate) fn init_new(regions: u32, orders: usize, data: &'a mut [u8]) -> Self {
        assert!(data.len() >= Self::required_bytes(regions, orders));
        data[..4].copy_from_slice(&u32::try_from(orders).unwrap().to_le_bytes());
        data[4..8].copy_from_slice(
            &u32::try_from(BtreeBitmapMut::required_space(regions.try_into().unwrap()))
                .unwrap()
                .to_le_bytes(),
        );

        let mut result = Self { data };
        for i in 0..orders {
            BtreeBitmapMut::init_new(result.get_order_mut(i), regions as usize);
        }

        result
    }

    pub(crate) fn find_free(&self, order: usize) -> Option<u64> {
        let mem = self.get_order(order);
        let accessor = BtreeBitmap::new(mem);
        accessor.find_first_unset()
    }

    pub(crate) fn mark_free(&mut self, order: usize, region: u64) {
        assert!(order < self.suballocators());
        for i in 0..=order {
            let start = 8 + i * self.suballocator_len();
            let end = start + self.suballocator_len();
            let mem = &mut self.data[start..end];
            let mut accessor = BtreeBitmapMut::new(mem);
            accessor.clear(region);
        }
    }

    pub(crate) fn mark_full(&mut self, order: usize, region: u64) {
        assert!(order < self.suballocators());
        for i in order..self.suballocators() {
            let start = 8 + i * self.suballocator_len();
            let end = start + self.suballocator_len();
            let mem = &mut self.data[start..end];
            let mut accessor = BtreeBitmapMut::new(mem);
            accessor.set(region);
        }
    }

    fn suballocator_len(&self) -> usize {
        u32::from_le_bytes(self.data[4..8].try_into().unwrap()) as usize
    }

    fn suballocators(&self) -> usize {
        u32::from_le_bytes(self.data[..4].try_into().unwrap()) as usize
    }

    fn get_order_mut(&mut self, order: usize) -> &mut [u8] {
        assert!(order < self.suballocators());
        let start = 8 + order * self.suballocator_len();
        let end = start + self.suballocator_len();
        &mut self.data[start..end]
    }

    fn get_order(&self, order: usize) -> &[u8] {
        assert!(order < self.suballocators());
        let start = 8 + order * self.suballocator_len();
        let end = start + self.suballocator_len();
        &self.data[start..end]
    }
}

enum AllocationOp {
    Allocate(PageNumber),
    Free(PageNumber),
    FreeUncommitted(PageNumber),
}

// The current layout for the active transaction.
// May include uncommitted changes to the database layout, if it grew or shrank
// TODO: this should probably be merged into InMemoryState
struct InProgressLayout {
    layout: DatabaseLayout,
    tracker_page: PageNumber,
}

struct Allocators {
    region_header_size: u32,
    region_tracker: Vec<u8>,
    region_headers: Vec<Vec<u8>>,
}

impl Allocators {
    fn new(layout: DatabaseLayout) -> Self {
        let page_size = layout.full_region_layout().page_size() as usize;
        // Round up to page size
        let region_tracker_required_pages =
            (RegionTracker::required_bytes(NUM_REGIONS, MAX_MAX_PAGE_ORDER + 1) + page_size - 1)
                / page_size;
        let required_bytes = page_size * region_tracker_required_pages.next_power_of_two();
        let mut region_tracker_bytes = vec![0; required_bytes];
        RegionTracker::init_new(
            NUM_REGIONS,
            MAX_MAX_PAGE_ORDER + 1,
            &mut region_tracker_bytes,
        );
        let mut region_tracker = RegionTracker::new(&mut region_tracker_bytes);
        let mut region_headers = vec![];
        let region_header_size: u32 = layout
            .full_region_layout()
            .data_section()
            .start
            .try_into()
            .unwrap();
        for i in 0..layout.num_regions() {
            let region_layout = layout.region_layout(i);
            let mut region_header_bytes = vec![0; region_header_size as usize];
            let mut region = RegionHeaderMutator::new(&mut region_header_bytes);
            region.initialize(
                region_layout.num_pages(),
                layout.full_region_layout().num_pages(),
            );
            let max_order = region.allocator_mut().get_max_order();
            region_tracker.mark_free(max_order, i as u64);
            region_headers.push(region_header_bytes);
        }
        drop(region_tracker);

        Self {
            region_header_size,
            region_tracker: region_tracker_bytes,
            region_headers,
        }
    }

    fn from_bytes(header: &DatabaseHeader, storage: &dyn PhysicalStorage) -> Result<Self> {
        let page_size = header.page_size();
        let region_header_size = header.region_header_pages() * page_size;
        let region_size =
            header.region_max_data_pages() as u64 * page_size as u64 + region_header_size as u64;
        let range = header.primary_slot().region_tracker.address_range(
            page_size as u64,
            region_size,
            region_header_size as u64,
            page_size,
        );
        let len: usize = (range.end - range.start).try_into().unwrap();
        let region_tracker = storage.read_direct(range.start, len)?;
        let mut region_headers = vec![];
        let layout = header.primary_slot().layout;
        for i in 0..layout.num_regions() {
            let base = layout.region_base_address(i);
            let len: usize = layout
                .region_layout(i)
                .data_section()
                .start
                .try_into()
                .unwrap();

            let mem = storage.read_direct(base, len)?;
            region_headers.push(mem);
        }

        Ok(Self {
            region_header_size,
            region_tracker,
            region_headers,
        })
    }

    fn flush_to(
        &self,
        region_tracker_page: PageNumber,
        layout: DatabaseLayout,
        storage: &mut Box<dyn PhysicalStorage>,
    ) -> Result {
        let page_size = layout.full_region_layout().page_size();
        let region_header_size =
            (layout.full_region_layout().get_header_pages() * page_size) as u64;
        let region_size =
            layout.full_region_layout().num_pages() as u64 * page_size as u64 + region_header_size;
        // Safety: we have a mutable reference to the Mmap, so no one else can have a reference this memory
        let mut region_tracker_bytes = unsafe {
            let range = region_tracker_page.address_range(
                page_size as u64,
                region_size,
                region_header_size,
                page_size,
            );
            let len: usize = (range.end - range.start).try_into().unwrap();
            storage.write(range.start, len)?
        };
        region_tracker_bytes
            .as_mut()
            .copy_from_slice(&self.region_tracker);

        assert_eq!(self.region_headers.len(), layout.num_regions() as usize);
        for i in 0..layout.num_regions() {
            let base = layout.region_base_address(i);
            let len: usize = layout
                .region_layout(i)
                .data_section()
                .start
                .try_into()
                .unwrap();

            // Safety: we have a mutable reference to the storage, so no one else can have a reference this memory
            let mut mem = unsafe { storage.write(base, len)? };
            mem.as_mut()
                .copy_from_slice(&self.region_headers[i as usize]);
        }

        Ok(())
    }

    fn resize_to(&mut self, new_layout: DatabaseLayout) {
        let shrink = match (new_layout.num_regions() as usize).cmp(&self.region_headers.len()) {
            cmp::Ordering::Less => true,
            cmp::Ordering::Equal => {
                let region = RegionHeaderAccessor::new(self.region_headers.last().unwrap());
                let allocator = region.allocator();
                let last_region = new_layout
                    .trailing_region_layout()
                    .unwrap_or_else(|| new_layout.full_region_layout());
                match (last_region.num_pages() as usize).cmp(&allocator.len()) {
                    cmp::Ordering::Less => true,
                    cmp::Ordering::Equal => {
                        // No-op
                        return;
                    }
                    cmp::Ordering::Greater => false,
                }
            }
            cmp::Ordering::Greater => false,
        };

        let mut region_tracker = RegionTracker::new(&mut self.region_tracker);
        if shrink {
            // Drop all regions that were removed
            for i in (new_layout.num_regions() as u64)..(self.region_headers.len() as u64) {
                region_tracker.mark_full(0, i);
            }
            self.region_headers
                .drain((new_layout.num_regions() as usize)..);

            // Resize the last region
            let last_region = new_layout
                .trailing_region_layout()
                .unwrap_or_else(|| new_layout.full_region_layout());
            let mut region = RegionHeaderMutator::new(self.region_headers.last_mut().unwrap());
            let mut allocator = region.allocator_mut();
            if allocator.len() > last_region.num_pages() as usize {
                allocator.resize(last_region.num_pages() as usize);
            }
        } else {
            let old_num_regions = self.region_headers.len();
            for i in 0..new_layout.num_regions() {
                let new_region = new_layout.region_layout(i);
                if (i as usize) < old_num_regions {
                    let mut region = RegionHeaderMutator::new(&mut self.region_headers[i as usize]);
                    assert!(new_region.num_pages() as usize >= region.allocator_mut().len());
                    if new_region.num_pages() as usize != region.allocator_mut().len() {
                        let mut allocator = region.allocator_mut();
                        allocator.resize(new_region.num_pages() as usize);
                        let highest_free = allocator.highest_free_order().unwrap();
                        region_tracker.mark_free(highest_free, i as u64);
                    }
                } else {
                    // brand new region
                    // TODO: check that region_tracker has enough space and grow it if needed
                    let mut new_region_bytes = vec![0; self.region_header_size as usize];
                    let mut region = RegionHeaderMutator::new(&mut new_region_bytes);
                    region.initialize(
                        new_region.num_pages(),
                        new_layout.full_region_layout().num_pages(),
                    );
                    let highest_free = region.allocator_mut().highest_free_order().unwrap();
                    region_tracker.mark_free(highest_free, i as u64);
                    self.region_headers.push(new_region_bytes);
                }
            }
        }
    }
}

struct InMemoryState {
    header: DatabaseHeader,
    allocators: Allocators,
}

impl InMemoryState {
    fn from_bytes(header: DatabaseHeader, file: &dyn PhysicalStorage) -> Result<Self> {
        let allocators = Allocators::from_bytes(&header, file)?;
        Ok(Self { header, allocators })
    }

    fn get_region(&self, region: u32) -> RegionHeaderAccessor {
        RegionHeaderAccessor::new(&self.allocators.region_headers[region as usize])
    }

    fn get_region_mut(&mut self, region: u32) -> RegionHeaderMutator {
        RegionHeaderMutator::new(&mut self.allocators.region_headers[region as usize])
    }

    fn get_region_tracker_mut(&mut self) -> RegionTracker {
        RegionTracker::new(&mut self.allocators.region_tracker)
    }
}

pub(crate) struct TransactionalMemory {
    // Pages allocated since the last commit
    allocated_since_commit: Mutex<HashSet<PageNumber>>,
    log_since_commit: Mutex<Vec<AllocationOp>>,
    // True if the allocator state was corrupted when the file was opened
    needs_recovery: bool,
    // TODO: should be a compile-time type parameter
    storage: Box<dyn PhysicalStorage>,
    state: Mutex<InMemoryState>,
    // The current layout for the active transaction.
    // May include uncommitted changes to the database layout, if it grew or shrank
    layout: Mutex<InProgressLayout>,
    // The number of PageMut which are outstanding
    #[cfg(debug_assertions)]
    open_dirty_pages: Mutex<HashSet<PageNumber>>,
    // Reference counts of PageImpls that are outstanding
    #[cfg(debug_assertions)]
    read_page_ref_counts: Mutex<HashMap<PageNumber, u64>>,
    // Indicates that a non-durable commit has been made, so reads should be served from the secondary meta page
    read_from_secondary: AtomicBool,
    page_size: u32,
    // We store these separately from the layout because they're static, and accessed on the get_page()
    // code path where there is no locking
    region_size: u64,
    region_header_with_padding_size: u64,
    #[allow(dead_code)]
    pages_are_os_page_aligned: bool,
    #[allow(dead_code)]
    use_mmap: bool,
}

impl TransactionalMemory {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        file: File,
        use_mmap: bool,
        page_size: usize,
        requested_region_size: Option<usize>,
        initial_size: Option<u64>,
        read_cache_size_bytes: usize,
        write_cache_size_bytes: usize,
        write_strategy: Option<WriteStrategy>,
    ) -> Result<Self> {
        assert!(page_size.is_power_of_two() && page_size >= DB_HEADER_SIZE);

        let region_size = requested_region_size
            .map(|x| x as u64)
            .unwrap_or(MAX_USABLE_REGION_SPACE);
        assert!(region_size.is_power_of_two());

        // TODO: allocate more tracker space when it becomes exhausted, and remove this hard coded 1000 regions
        let region_tracker_required_bytes =
            RegionTracker::required_bytes(NUM_REGIONS, MAX_MAX_PAGE_ORDER + 1);
        let starting_size = if let Some(size) = initial_size {
            size
        } else {
            // Make sure that there is enough room to allocate the region tracker into a page
            let size: u64 = max(MIN_DESIRED_USABLE_BYTES, page_size * MIN_USABLE_PAGES)
                .try_into()
                .unwrap();
            let tracker_space =
                (page_size * ((region_tracker_required_bytes + page_size - 1) / page_size)) as u64;
            size + tracker_space
        };
        let layout = DatabaseLayout::calculate(
            starting_size,
            (region_size / u64::try_from(page_size).unwrap())
                .try_into()
                .unwrap(),
            page_size.try_into().unwrap(),
        )?;

        {
            let file_len = file.metadata()?.len();

            if file_len < layout.len() {
                file.set_len(layout.len())?;
            }
        }

        let mut storage: Box<dyn PhysicalStorage> = if use_mmap {
            Box::new(Mmap::new(file)?)
        } else {
            Box::new(PagedCachedFile::new(
                file.try_clone().unwrap(),
                page_size as u64,
                read_cache_size_bytes,
                write_cache_size_bytes,
            )?)
        };

        let magic_number: [u8; MAGICNUMBER.len()] = storage
            .read_direct(0, MAGICNUMBER.len())?
            .try_into()
            .unwrap();

        if magic_number != MAGICNUMBER {
            let mut allocators = Allocators::new(layout);

            // Allocate the region tracker in the zeroth region
            let tracker_page = {
                let mut region = RegionHeaderMutator::new(&mut allocators.region_headers[0]);
                let tracker_required_pages =
                    (allocators.region_tracker.len() + page_size - 1) / page_size;
                let required_order = ceil_log2(tracker_required_pages);
                let page_number = region.allocator_mut().alloc(required_order).unwrap();
                PageNumber::new(
                    0,
                    page_number.try_into().unwrap(),
                    required_order.try_into().unwrap(),
                )
            };

            let checksum_type = match write_strategy.unwrap_or(WriteStrategy::Checksum) {
                WriteStrategy::Checksum => ChecksumType::XXH3_128,
                WriteStrategy::TwoPhase => ChecksumType::Unused,
            };
            let mut header =
                DatabaseHeader::new(layout, checksum_type, TransactionId(0), tracker_page);

            header.recovery_required = false;
            // Safety: we own the storage object and have no other references to this memory
            unsafe {
                storage
                    .write(0, DB_HEADER_SIZE)?
                    .as_mut()
                    .copy_from_slice(&header.to_bytes(false, false));
            }
            allocators.flush_to(tracker_page, layout, &mut storage)?;

            storage.flush()?;
            // Write the magic number only after the data structure is initialized and written to disk
            // to ensure that it's crash safe
            // Safety: we own the storage object and have no other references to this memory
            unsafe {
                storage
                    .write(0, DB_HEADER_SIZE)?
                    .as_mut()
                    .copy_from_slice(&header.to_bytes(true, false));
            }
            storage.flush()?;
        }
        let header_bytes = storage.read_direct(0, DB_HEADER_SIZE)?;
        let (mut header, repair_info) = DatabaseHeader::from_bytes(&header_bytes);

        if let Some(requested_strategy) = write_strategy {
            let checksum_type: ChecksumType = requested_strategy.into();
            assert_eq!(checksum_type, header.primary_slot().checksum_type);
        }

        assert_eq!(header.page_size() as usize, page_size);
        let version = header.primary_slot().version;
        if version > FILE_FORMAT_VERSION {
            return Err(Error::Corrupted(format!(
                "Expected file format version {FILE_FORMAT_VERSION}, found {version}",
            )));
        }
        if version < FILE_FORMAT_VERSION {
            return Err(Error::UpgradeRequired(version));
        }
        let version = header.secondary_slot().version;
        if version > FILE_FORMAT_VERSION {
            return Err(Error::Corrupted(format!(
                "Expected file format version {FILE_FORMAT_VERSION}, found {version}",
            )));
        }
        if version < FILE_FORMAT_VERSION {
            return Err(Error::UpgradeRequired(version));
        }

        let needs_recovery = header.recovery_required;
        if needs_recovery {
            if repair_info.primary_corrupted {
                header.swap_primary_slot();
            } else {
                // If the secondary is a valid commit, verify that the primary is newer. This handles an edge case where:
                // * the primary bit is flipped to the secondary
                // * a crash occurs during fsync, such that no other data is written out to the secondary. meaning that it contains a valid, but out of date transaction
                let secondary_newer =
                    header.secondary_slot().transaction_id > header.primary_slot().transaction_id;
                if secondary_newer && !repair_info.secondary_corrupted {
                    header.swap_primary_slot();
                }
            }
            assert!(!repair_info.invalid_magic_number);
            // Safety: we own the storage object and have no other references to this memory
            unsafe {
                storage
                    .write(0, DB_HEADER_SIZE)?
                    .as_mut()
                    .copy_from_slice(&header.to_bytes(true, false));
            }
            storage.flush()?;
        }

        let layout = header.primary_slot().layout;
        let tracker_page = header.primary_slot().region_tracker;
        let region_size = layout.full_region_layout().len();
        let region_header_size = layout.full_region_layout().data_section().start;

        let state = InMemoryState::from_bytes(header, storage.as_ref())?;

        assert!(page_size >= DB_HEADER_SIZE);

        Ok(Self {
            allocated_since_commit: Mutex::new(HashSet::new()),
            log_since_commit: Mutex::new(vec![]),
            needs_recovery,
            storage,
            layout: Mutex::new(InProgressLayout {
                layout,
                tracker_page,
            }),
            state: Mutex::new(state),
            #[cfg(debug_assertions)]
            open_dirty_pages: Mutex::new(HashSet::new()),
            #[cfg(debug_assertions)]
            read_page_ref_counts: Mutex::new(HashMap::new()),
            read_from_secondary: AtomicBool::new(false),
            page_size: page_size.try_into().unwrap(),
            region_size,
            region_header_with_padding_size: region_header_size,
            pages_are_os_page_aligned: is_page_aligned(page_size),
            use_mmap,
        })
    }

    pub(crate) fn begin_writable(&self) -> Result {
        let mut state = self.state.lock().unwrap();
        assert!(!state.header.recovery_required);
        state.header.recovery_required = true;
        unsafe {
            self.write_header(&state.header, false)?;
        }
        self.storage.flush()
    }

    pub(crate) fn needs_repair(&self) -> Result<bool> {
        Ok(self.state.lock().unwrap().header.recovery_required)
    }

    pub(crate) fn needs_checksum_verification(&self) -> Result<bool> {
        Ok(self.checksum_type() == ChecksumType::XXH3_128)
    }

    pub(crate) fn checksum_type(&self) -> ChecksumType {
        self.state
            .lock()
            .unwrap()
            .header
            .primary_slot()
            .checksum_type
    }

    pub(crate) fn repair_primary_corrupted(&self) {
        let mut state = self.state.lock().unwrap();
        state.header.swap_primary_slot();
        let mut layout = self.layout.lock().unwrap();
        layout.layout = state.header.primary_slot().layout;
        layout.tracker_page = state.header.primary_slot().region_tracker;
    }

    pub(crate) fn begin_repair(&self) -> Result<()> {
        let mut state = self.state.lock().unwrap();

        let layout = self.layout.lock().unwrap();
        state.allocators = Allocators::new(layout.layout);
        let region_tracker_page = layout.tracker_page;

        // Mark the region tracker page as allocated
        state
            .get_region_mut(region_tracker_page.region)
            .allocator_mut()
            .record_alloc(
                region_tracker_page.page_index.into(),
                region_tracker_page.page_order as usize,
            );

        Ok(())
    }

    pub(crate) fn mark_pages_allocated(
        &self,
        allocated_pages: impl Iterator<Item = PageNumber>,
    ) -> Result<()> {
        let mut state = self.state.lock().unwrap();

        for page_number in allocated_pages {
            let region_index = page_number.region;
            let mut region = state.get_region_mut(region_index);
            region.allocator_mut().record_alloc(
                page_number.page_index as u64,
                page_number.page_order as usize,
            );
        }

        Ok(())
    }

    unsafe fn write_header(&self, header: &DatabaseHeader, swap_primary: bool) -> Result {
        self.storage
            .write(0, DB_HEADER_SIZE)?
            .as_mut()
            .copy_from_slice(&header.to_bytes(true, swap_primary));

        Ok(())
    }

    pub(crate) fn end_repair(&mut self) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        unsafe { self.write_header(&state.header, false)? };
        self.storage.flush()?;

        state.header.recovery_required = false;
        unsafe { self.write_header(&state.header, false)? };
        let result = self.storage.flush();
        self.needs_recovery = false;

        result
    }

    pub(crate) fn get_raw_allocator_states(&self) -> Vec<Vec<u8>> {
        let state = self.state.lock().unwrap();
        let layout = self.layout.lock().unwrap();

        let mut regional_allocators = vec![];
        for i in 0..layout.layout.num_regions() {
            regional_allocators.push(state.get_region(i).allocator_raw());
        }

        regional_allocators
    }

    // Diffs region_states, which must be the result of calling get_raw_allocator_states(), against
    // the currently allocated set of pages
    pub(crate) fn pages_allocated_since_raw_state(
        &self,
        region_states: &[Vec<u8>],
    ) -> Vec<PageNumber> {
        let mut result = vec![];
        let state = self.state.lock().unwrap();
        let layout = self.layout.lock().unwrap();

        assert!(region_states.len() <= layout.layout.num_regions() as usize);

        for i in 0..layout.layout.num_regions() {
            let region = state.get_region(i);
            let current_state = region.allocator();
            if let Some(old_state) = region_states.get(i as usize) {
                let old_allocated = BuddyAllocator::new(old_state).get_order0_allocated_pages(i);
                let new_allocated = current_state.get_order0_allocated_pages(i);
                result.extend(new_allocated.difference(&old_allocated));
            } else {
                // This region didn't exist, so everything is newly allocated
                result.extend(current_state.get_order0_allocated_pages(i));
            }
        }

        // TODO: it would be more efficient if we merged all the adjacent order0 pages together

        result
    }

    // Commit all outstanding changes and make them visible as the primary
    //
    // If new_checksum_type is provided, caller must ensure that all pages conform to the new checksum
    pub(crate) fn commit(
        &self,
        data_root: Option<(PageNumber, Checksum)>,
        freed_root: Option<(PageNumber, Checksum)>,
        transaction_id: TransactionId,
        eventual: bool,
        new_checksum_type: Option<ChecksumType>,
    ) -> Result {
        // All mutable pages must be dropped, this ensures that when a transaction completes
        // no more writes can happen to the pages it allocated. Thus it is safe to make them visible
        // to future read transactions
        #[cfg(debug_assertions)]
        debug_assert!(self.open_dirty_pages.lock().unwrap().is_empty());
        assert!(!self.needs_recovery);

        let mut state = self.state.lock().unwrap();
        let original_checksum_type = state.header.primary_slot().checksum_type;
        let checksum_type = new_checksum_type.unwrap_or(original_checksum_type);
        let mut layout = self.layout.lock().unwrap();

        // Trim surplus file space, before finalizing the commit
        let shrunk = self.try_shrink(&mut state, &mut layout)?;

        let mut secondary = state.header.secondary_slot_mut();
        secondary.checksum_type = checksum_type;
        secondary.transaction_id = transaction_id;
        secondary.root = data_root;
        secondary.freed_root = freed_root;
        secondary.layout = layout.layout;
        secondary.region_tracker = layout.tracker_page;
        unsafe { self.write_header(&state.header, false)? };

        // Use 2-phase commit, if checksums are disabled
        if matches!(checksum_type, ChecksumType::Unused) {
            if eventual {
                self.storage.eventual_flush()?;
            } else {
                self.storage.flush()?;
            }
        }

        // Swap the primary bit on-disk
        unsafe { self.write_header(&state.header, true)? };
        if eventual {
            self.storage.eventual_flush()?;
        } else {
            self.storage.flush()?;
        }
        // Only swap the in-memory primary bit after the fsync is successful
        state.header.swap_primary_slot();

        // Safety: try_shrink() only removes unallocated free pages at the end of the database file
        // references to unallocated pages are not allowed to exist, and we've now promoted the
        // shrunked layout to the primary
        if shrunk {
            unsafe {
                self.storage.resize(layout.layout.len())?;
            }
        }

        self.log_since_commit.lock().unwrap().clear();
        self.allocated_since_commit.lock().unwrap().clear();
        self.read_from_secondary.store(false, Ordering::Release);

        Ok(())
    }

    // Make changes visible, without a durability guarantee
    pub(crate) fn non_durable_commit(
        &self,
        data_root: Option<(PageNumber, Checksum)>,
        freed_root: Option<(PageNumber, Checksum)>,
        transaction_id: TransactionId,
    ) -> Result {
        // All mutable pages must be dropped, this ensures that when a transaction completes
        // no more writes can happen to the pages it allocated. Thus it is safe to make them visible
        // to future read transactions
        #[cfg(debug_assertions)]
        debug_assert!(self.open_dirty_pages.lock().unwrap().is_empty());
        assert!(!self.needs_recovery);

        let mut state = self.state.lock().unwrap();
        let checksum_type = state.header.primary_slot().checksum_type;
        let layout = self.layout.lock().unwrap();
        let mut secondary = state.header.secondary_slot_mut();
        secondary.checksum_type = checksum_type;
        secondary.transaction_id = transaction_id;
        secondary.root = data_root;
        secondary.freed_root = freed_root;
        secondary.layout = layout.layout;
        secondary.region_tracker = layout.tracker_page;

        self.log_since_commit.lock().unwrap().clear();
        self.allocated_since_commit.lock().unwrap().clear();
        self.storage.write_barrier()?;
        // TODO: maybe we can remove this flag and just update the in-memory DatabaseHeader state?
        self.read_from_secondary.store(true, Ordering::Release);

        Ok(())
    }

    pub(crate) fn rollback_uncommitted_writes(&self) -> Result {
        #[cfg(debug_assertions)]
        debug_assert!(self.open_dirty_pages.lock().unwrap().is_empty());
        let mut state = self.state.lock().unwrap();
        // The layout to restore
        let (restore, restore_tracker_page) = if self.read_from_secondary.load(Ordering::Acquire) {
            (
                state.header.secondary_slot().layout,
                state.header.secondary_slot().region_tracker,
            )
        } else {
            (
                state.header.primary_slot().layout,
                state.header.primary_slot().region_tracker,
            )
        };

        let mut layout = self.layout.lock().unwrap();
        for op in self.log_since_commit.lock().unwrap().drain(..).rev() {
            match op {
                AllocationOp::Allocate(page_number) => {
                    let region_index = page_number.region;
                    state
                        .get_region_tracker_mut()
                        .mark_free(page_number.page_order as usize, region_index as u64);
                    let mut region = state.get_region_mut(region_index);
                    region.allocator_mut().free(
                        page_number.page_index as u64,
                        page_number.page_order as usize,
                    );

                    let address = page_number.address_range(
                        self.page_size as u64,
                        self.region_size,
                        self.region_header_with_padding_size,
                        self.page_size,
                    );
                    let len: usize = (address.end - address.start).try_into().unwrap();
                    self.storage.invalidate_cache(address.start, len);
                    self.storage.cancel_pending_write(address.start, len);
                }
                AllocationOp::Free(page_number) | AllocationOp::FreeUncommitted(page_number) => {
                    let region_index = page_number.region;
                    let mut region = state.get_region_mut(region_index);
                    region.allocator_mut().record_alloc(
                        page_number.page_index as u64,
                        page_number.page_order as usize,
                    );
                }
            }
        }
        self.allocated_since_commit.lock().unwrap().clear();

        // Shrinking only happens during commit
        assert!(restore.len() <= layout.layout.len());
        // Reset the layout, in case it changed during the writes
        if restore.len() < layout.layout.len() {
            // Restore the size of the last region's allocator
            let last_region_index = restore.num_regions() - 1;
            let last_region = restore.region_layout(last_region_index);
            let mut region = state.get_region_mut(last_region_index);
            region
                .allocator_mut()
                .resize(last_region.num_pages() as usize);

            *layout = InProgressLayout {
                layout: restore,
                tracker_page: restore_tracker_page,
            };
            // Safety: we've rollbacked the transaction, so any data in that was written into
            // space that was grown during this transaction no longer exists
            unsafe {
                self.storage.resize(layout.layout.len())?;
            }
        }

        Ok(())
    }

    // TODO: make all callers explicitly provide a hint
    pub(crate) fn get_page(&self, page_number: PageNumber) -> Result<PageImpl> {
        self.get_page_extended(page_number, PageHint::None)
    }

    pub(crate) fn get_page_extended(
        &self,
        page_number: PageNumber,
        hint: PageHint,
    ) -> Result<PageImpl> {
        // We must not retrieve an immutable reference to a page which already has a mutable ref to it
        #[cfg(debug_assertions)]
        {
            debug_assert!(
                !self.open_dirty_pages.lock().unwrap().contains(&page_number),
                "{page_number:?}",
            );
            *(self
                .read_page_ref_counts
                .lock()
                .unwrap()
                .entry(page_number)
                .or_default()) += 1;
        }

        // Safety: we asserted that no mutable references are open
        let range = page_number.address_range(
            self.page_size as u64,
            self.region_size,
            self.region_header_with_padding_size,
            self.page_size,
        );
        let len: usize = (range.end - range.start).try_into().unwrap();
        let mem = unsafe { self.storage.read(range.start, len, hint)? };

        Ok(PageImpl {
            mem,
            page_number,
            #[cfg(debug_assertions)]
            open_pages: &self.read_page_ref_counts,
            #[cfg(not(debug_assertions))]
            _debug_lifetime: Default::default(),
        })
    }

    // Safety: the caller must ensure that no references to the memory in `page` exist
    pub(crate) unsafe fn get_page_mut(&self, page_number: PageNumber) -> Result<PageMut> {
        #[cfg(debug_assertions)]
        {
            assert!(!self
                .read_page_ref_counts
                .lock()
                .unwrap()
                .contains_key(&page_number));
            assert!(self.open_dirty_pages.lock().unwrap().insert(page_number));
        }

        let address_range = page_number.address_range(
            self.page_size as u64,
            self.region_size,
            self.region_header_with_padding_size,
            self.page_size,
        );
        let len: usize = (address_range.end - address_range.start)
            .try_into()
            .unwrap();
        let mem = self.storage.write(address_range.start, len)?;

        Ok(PageMut {
            mem,
            page_number,
            #[cfg(debug_assertions)]
            open_pages: &self.open_dirty_pages,
            #[cfg(not(debug_assertions))]
            _debug_lifetime: Default::default(),
        })
    }

    pub(crate) fn get_version(&self) -> u8 {
        let state = self.state.lock().unwrap();
        if self.read_from_secondary.load(Ordering::Acquire) {
            state.header.secondary_slot().version
        } else {
            state.header.primary_slot().version
        }
    }

    pub(crate) fn get_data_root(&self) -> Option<(PageNumber, Checksum)> {
        let state = self.state.lock().unwrap();
        if self.read_from_secondary.load(Ordering::Acquire) {
            state.header.secondary_slot().root
        } else {
            state.header.primary_slot().root
        }
    }

    pub(crate) fn get_freed_root(&self) -> Option<(PageNumber, Checksum)> {
        let state = self.state.lock().unwrap();
        if self.read_from_secondary.load(Ordering::Acquire) {
            state.header.secondary_slot().freed_root
        } else {
            state.header.primary_slot().freed_root
        }
    }

    pub(crate) fn get_last_committed_transaction_id(&self) -> Result<TransactionId> {
        let state = self.state.lock().unwrap();
        if self.read_from_secondary.load(Ordering::Acquire) {
            Ok(state.header.secondary_slot().transaction_id)
        } else {
            Ok(state.header.primary_slot().transaction_id)
        }
    }

    // Safety: the caller must ensure that no references to the memory in `page` exist
    pub(crate) unsafe fn free(&self, page: PageNumber) {
        let mut state = self.state.lock().unwrap();
        let region_index = page.region;
        // Free in the regional allocator
        let mut region = state.get_region_mut(region_index);
        region
            .allocator_mut()
            .free(page.page_index as u64, page.page_order as usize);
        // Ensure that the region is marked as having free space
        state
            .get_region_tracker_mut()
            .mark_free(page.page_order as usize, region_index as u64);
        self.log_since_commit
            .lock()
            .unwrap()
            .push(AllocationOp::Free(page));

        let address_range = page.address_range(
            self.page_size as u64,
            self.region_size,
            self.region_header_with_padding_size,
            self.page_size,
        );
        let len: usize = (address_range.end - address_range.start)
            .try_into()
            .unwrap();
        self.storage.invalidate_cache(address_range.start, len);
        self.storage.cancel_pending_write(address_range.start, len);
    }

    // Frees the page if it was allocated since the last commit. Returns true, if the page was freed
    // Safety: the caller must ensure that no references to the memory in `page` exist
    pub(crate) unsafe fn free_if_uncommitted(&self, page: PageNumber) -> bool {
        if self.allocated_since_commit.lock().unwrap().remove(&page) {
            let mut state = self.state.lock().unwrap();
            // Free in the regional allocator
            let mut region = state.get_region_mut(page.region);
            region
                .allocator_mut()
                .free(page.page_index as u64, page.page_order as usize);
            // Ensure that the region is marked as having free space
            state
                .get_region_tracker_mut()
                .mark_free(page.page_order as usize, page.region as u64);

            self.log_since_commit
                .lock()
                .unwrap()
                .push(AllocationOp::FreeUncommitted(page));

            let address_range = page.address_range(
                self.page_size as u64,
                self.region_size,
                self.region_header_with_padding_size,
                self.page_size,
            );
            let len: usize = (address_range.end - address_range.start)
                .try_into()
                .unwrap();
            self.storage.invalidate_cache(address_range.start, len);
            self.storage.cancel_pending_write(address_range.start, len);

            true
        } else {
            false
        }
    }

    // Page has not been committed
    pub(crate) fn uncommitted(&self, page: PageNumber) -> bool {
        self.allocated_since_commit.lock().unwrap().contains(&page)
    }

    pub(crate) unsafe fn mark_transaction(&self, id: TransactionId) {
        self.storage.mark_transaction(id)
    }

    pub(crate) unsafe fn mmap_gc(&self, oldest_live_id: TransactionId) -> Result {
        self.storage.gc(oldest_live_id)
    }

    fn allocate_helper(
        &self,
        state: &mut InMemoryState,
        required_order: usize,
    ) -> Result<Option<PageNumber>> {
        loop {
            let candidate_region =
                if let Some(candidate) = state.get_region_tracker_mut().find_free(required_order) {
                    candidate.try_into().unwrap()
                } else {
                    return Ok(None);
                };
            let mut region = state.get_region_mut(candidate_region);
            if let Some(page) = region.allocator_mut().alloc(required_order) {
                return Ok(Some(PageNumber::new(
                    candidate_region,
                    page.try_into().unwrap(),
                    required_order.try_into().unwrap(),
                )));
            } else {
                // Mark the region, if it's full
                state
                    .get_region_tracker_mut()
                    .mark_full(required_order, candidate_region as u64);
            }
        }
    }

    // Safety: caller must guarantee that no references to free pages at the end of the last region exist
    #[cfg_attr(windows, allow(unreachable_code))]
    #[cfg_attr(windows, allow(unused_variables))]
    fn try_shrink(
        &self,
        state: &mut InMemoryState,
        in_progress_layout: &mut InProgressLayout,
    ) -> Result<bool> {
        // TODO: enable shrinking on Windows
        #[cfg(windows)]
        {
            return Ok(false);
        }

        let (layout, tracker_page) = (
            &mut in_progress_layout.layout,
            &mut in_progress_layout.tracker_page,
        );
        let last_region_index = layout.num_regions() - 1;
        let last_region = layout.region_layout(last_region_index);
        let region = state.get_region(last_region_index);
        let last_allocator = region.allocator();
        let trailing_free = last_allocator.trailing_free_pages();
        let last_allocator_len = last_allocator.len();
        drop(last_allocator);
        if trailing_free < last_allocator_len / 2 {
            return Ok(false);
        }
        let reduce_to_pages = if layout.num_regions() > 1 && trailing_free == last_allocator_len {
            0
        } else {
            max(MIN_USABLE_PAGES, last_allocator_len - trailing_free / 2)
        };

        let new_usable_bytes = if reduce_to_pages == 0 {
            layout.usable_bytes() - last_region.usable_bytes()
        } else {
            layout.usable_bytes()
                - ((last_allocator_len - reduce_to_pages) as u64)
                    * (state.header.page_size() as u64)
        };

        let new_layout = DatabaseLayout::calculate(
            new_usable_bytes,
            state.header.region_max_data_pages(),
            self.page_size,
        )?;
        state.allocators.resize_to(new_layout);
        assert!(new_layout.len() <= layout.len());

        // TODO: try to shrink the region tracker and relocate it to a lower region, if it's in the last one

        *in_progress_layout = InProgressLayout {
            layout: new_layout,
            tracker_page: *tracker_page,
        };

        Ok(true)
    }

    fn grow(
        &self,
        state: &mut InMemoryState,
        layout: &mut InProgressLayout,
        required_order_allocation: usize,
    ) -> Result<()> {
        let layout = &mut layout.layout;

        let required_growth = 2u64.pow(required_order_allocation.try_into().unwrap())
            * state.header.page_size() as u64;
        let max_region_size =
            (state.header.region_max_data_pages() as u64) * (state.header.page_size() as u64);
        let next_desired_size = if layout.num_full_regions() > 0 {
            if let Some(trailing) = layout.trailing_region_layout() {
                if 2 * required_growth < max_region_size - trailing.usable_bytes() {
                    // Fill out the trailing region
                    layout.usable_bytes() + (max_region_size - trailing.usable_bytes())
                } else {
                    // Grow by 1 region
                    layout.usable_bytes() + max_region_size
                }
            } else {
                // Grow by 1 region
                layout.usable_bytes() + max_region_size
            }
        } else {
            max(
                layout.usable_bytes() * 2,
                layout.usable_bytes() + required_growth * 2,
            )
        };
        let new_layout = DatabaseLayout::calculate(
            next_desired_size,
            state.header.region_max_data_pages(),
            self.page_size,
        )?;
        assert!(new_layout.len() >= layout.len());

        // Safety: We're growing the storage
        unsafe {
            self.storage.resize(new_layout.len())?;
        }
        state.allocators.resize_to(new_layout);
        *layout = new_layout;
        Ok(())
    }

    pub(crate) fn allocate(&self, allocation_size: usize) -> Result<PageMut> {
        let required_pages = (allocation_size + self.get_page_size() - 1) / self.get_page_size();
        let required_order = ceil_log2(required_pages);

        let mut state = self.state.lock().unwrap();
        let mut layout = self.layout.lock().unwrap();

        let page_number =
            if let Some(page_number) = self.allocate_helper(&mut state, required_order)? {
                page_number
            } else {
                self.grow(&mut state, &mut layout, required_order)?;
                self.allocate_helper(&mut state, required_order)?.unwrap()
            };

        self.allocated_since_commit
            .lock()
            .unwrap()
            .insert(page_number);
        self.log_since_commit
            .lock()
            .unwrap()
            .push(AllocationOp::Allocate(page_number));
        #[cfg(debug_assertions)]
        {
            assert!(!self
                .read_page_ref_counts
                .lock()
                .unwrap()
                .contains_key(&page_number));
            assert!(self.open_dirty_pages.lock().unwrap().insert(page_number));
        }

        let address_range = page_number.address_range(
            self.page_size as u64,
            self.region_size,
            self.region_header_with_padding_size,
            self.page_size,
        );
        let len: usize = (address_range.end - address_range.start)
            .try_into()
            .unwrap();

        // Safety:
        // The address range we're returning was just allocated, so no other references exist
        #[allow(unused_mut)]
        let mut mem = unsafe { self.storage.write(address_range.start, len)? };
        debug_assert!(mem.as_ref().len() >= allocation_size);

        // TODO: move this into the mmap implementation
        #[cfg(unix)]
        if self.use_mmap {
            let len = mem.as_ref().len();
            // If this is a large page, hint that it should be paged in
            if self.pages_are_os_page_aligned && len > self.get_page_size() {
                let result = unsafe {
                    libc::madvise(
                        mem.as_mut().as_mut_ptr() as *mut libc::c_void,
                        len as libc::size_t,
                        libc::MADV_WILLNEED,
                    )
                };
                if result != 0 {
                    return Err(io::Error::last_os_error().into());
                }
            }
        }

        #[cfg(debug_assertions)]
        {
            // Poison the memory in debug mode to help detect uninitialized reads
            mem.as_mut().fill(0xFF);
        }

        Ok(PageMut {
            mem,
            page_number,
            #[cfg(debug_assertions)]
            open_pages: &self.open_dirty_pages,
            #[cfg(not(debug_assertions))]
            _debug_lifetime: Default::default(),
        })
    }

    pub(crate) fn count_allocated_pages(&self) -> Result<usize> {
        let state = self.state.lock().unwrap();
        let layout = self.layout.lock().unwrap();
        let mut count = 0;
        for i in 0..layout.layout.num_regions() {
            let region = state.get_region(i);
            count += region.allocator().count_allocated_pages();
        }

        Ok(count)
    }

    pub(crate) fn get_page_size(&self) -> usize {
        self.page_size.try_into().unwrap()
    }
}

impl Drop for TransactionalMemory {
    fn drop(&mut self) {
        // Commit any non-durable transactions that are outstanding
        if self.read_from_secondary.load(Ordering::Acquire) {
            if let Ok(non_durable_transaction_id) = self.get_last_committed_transaction_id() {
                let root = self.get_data_root();
                let freed_root = self.get_freed_root();
                if self
                    .commit(root, freed_root, non_durable_transaction_id, false, None)
                    .is_err()
                {
                    eprintln!(
                        "Failure while finalizing non-durable commit. Database may have rolled back"
                    );
                }
            } else {
                eprintln!(
                    "Failure while finalizing non-durable commit. Database may have rolled back"
                );
            }
        }
        let mut state = self.state.lock().unwrap();
        if state
            .allocators
            .flush_to(
                state.header.primary_slot().region_tracker,
                state.header.primary_slot().layout,
                &mut self.storage,
            )
            .is_err()
        {
            eprintln!("Failure while flushing allocator state. Repair required at restart.");
        }

        if self.storage.flush().is_ok() && !self.needs_recovery {
            state.header.recovery_required = false;
            let _ = unsafe { self.write_header(&state.header, false) };
            let _ = self.storage.flush();
        }
    }
}
