use crate::error::{Error, Result};
use crate::las::{LogicalAddress, LogicalAddressSpace, LogicalMutRef, LogicalSlice, PageAlloc};
use crate::utils::unsafe_utils;
use parking_lot::RwLock;
use std::marker::PhantomData;
use std::mem::size_of;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

#[derive(Debug)]
pub struct UntypedPointer {
    address: AtomicUsize,
}

impl Clone for UntypedPointer {
    fn clone(&self) -> Self {
        UntypedPointer {
            address: AtomicUsize::new(self.address_internal()),
        }
    }
}

impl UntypedPointer {
    const POINTER_TYPE_MASK: usize = 0b11 << 54;
    const POINTER_REFCOUNT_MASK: usize = 0b11111111 << 56;

    const POINTER_BYTE_ADDRESSABLE: usize = 0b00 << 54;
    const POINTER_BLOCK: usize = 0b01 << 54;
    const POINTER_LOG: usize = 0b10 << 54;

    const POINTER_ADDRESS_MASK: usize = !(Self::POINTER_TYPE_MASK | Self::POINTER_REFCOUNT_MASK);

    fn type_bytes(&self) -> usize {
        self.address_internal() & Self::POINTER_TYPE_MASK
    }

    fn from_raw(data: usize) -> Self {
        UntypedPointer {
            address: AtomicUsize::new(data),
        }
    }

    pub(crate) fn new_byte_addressable(address: LogicalAddress) -> Self {
        UntypedPointer {
            address: AtomicUsize::new(address | Self::POINTER_BYTE_ADDRESSABLE),
        }
    }

    pub(crate) fn is_some(&self) -> bool {
        self.address() != 0
    }

    fn new_storage(address: LogicalAddress) -> Self {
        UntypedPointer {
            address: AtomicUsize::new(address | Self::POINTER_BLOCK),
        }
    }

    fn new_log(address: LogicalAddress) -> Self {
        UntypedPointer {
            address: AtomicUsize::new(address | Self::POINTER_LOG),
        }
    }

    pub(crate) fn new_none() -> Self {
        UntypedPointer {
            address: AtomicUsize::new(0),
        }
    }

    fn is_byte_addressable(&self) -> bool {
        self.type_bytes() == Self::POINTER_BYTE_ADDRESSABLE
    }

    fn is_block(&self) -> bool {
        self.type_bytes() == Self::POINTER_BLOCK
    }

    fn address_internal(&self) -> usize {
        self.address.load(Ordering::SeqCst)
    }

    pub(crate) fn address(&self) -> LogicalAddress {
        self.address_internal() & Self::POINTER_ADDRESS_MASK
    }

    pub fn refcount(&self) -> &AtomicU8 {
        let bytes = unsafe {
            let data = std::mem::transmute(&self.address);
            std::slice::from_raw_parts(data, size_of::<AtomicUsize>())
        };

        &bytes[0]
    }

    pub fn compare_and_swap(&self, current: UntypedPointer, new: UntypedPointer) -> bool {
        let current = current.address_internal();
        let new = new.address_internal();

        let old = self
            .address
            .compare_and_swap(current, new, Ordering::SeqCst);
        old == current
    }
}

pub struct Version {
    version: AtomicUsize,
}

impl Clone for Version {
    fn clone(&self) -> Self {
        Version {
            version: AtomicUsize::new(self.version.load(Ordering::SeqCst)),
        }
    }
}

impl Version {
    const VERSION_TYPE_MASK: usize = 0b1 << 63;
    const VERSION_DATA_MASK: usize = !(Self::VERSION_TYPE_MASK);

    const VERSION_TYPE_DIRECT: usize = 0b0 << 63;
    const VERSION_TYPE_INDIRECT: usize = 0b1 << 63;

    pub fn new() -> Self {
        Version {
            version: AtomicUsize::new(0),
        }
    }

    pub fn new_base() -> Self {
        Version {
            version: AtomicUsize::new(1),
        }
    }

    fn type_bytes(&self) -> usize {
        let version = self.version.load(Ordering::SeqCst);
        version & Self::VERSION_TYPE_MASK
    }

    fn commit(&self, new_version: usize, las: &LogicalAddressSpace) -> Result<()> {
        if self.type_bytes() == Self::VERSION_TYPE_DIRECT {
            self.version
                .store(new_version | Self::VERSION_TYPE_DIRECT, Ordering::SeqCst);

            Ok(())
        } else {
            let data = self.version.load(Ordering::SeqCst) & Self::VERSION_DATA_MASK;

            let ptr = UntypedPointer::from_raw(data);
            let slice = LogicalSlice::new(ptr.address(), size_of::<Version>());
            let data = las.read(&slice)?;

            let real_version = unsafe_utils::any_from_slice::<Version>(data);

            real_version.commit(new_version, las)
        }
    }

    fn new_indirect(real_version: UntypedPointer) -> Self {
        assert_eq!(real_version.address_internal() & Self::VERSION_TYPE_MASK, 0);

        Version {
            version: AtomicUsize::new(
                real_version.address_internal() | Self::VERSION_TYPE_INDIRECT,
            ),
        }
    }

    fn read(&self, las: &LogicalAddressSpace) -> Result<usize> {
        let data = self.version.load(Ordering::SeqCst) & Self::VERSION_DATA_MASK;

        if self.type_bytes() == Self::VERSION_TYPE_DIRECT {
            Ok(data)
        } else {
            let ptr = UntypedPointer::from_raw(data);
            let slice = LogicalSlice::new(ptr.address(), size_of::<Version>());
            let data = las.read(&slice)?;

            let real_version = unsafe_utils::any_from_slice::<Version>(data);

            real_version.read(las)
        }
    }
}

struct GenericAllocator<'tx> {
    active: Option<LogicalMutRef<'tx>>,
    page_alloc: PageAlloc<'tx>,
}

impl<'tx> GenericAllocator<'tx> {
    fn new(page_alloc: PageAlloc<'tx>) -> Self {
        GenericAllocator {
            active: None,
            page_alloc,
        }
    }

    pub fn alloc(&mut self, size: usize) -> Result<(LogicalSlice, &'tx mut [u8])> {
        let mut page_full = false;
        Ok(loop {
            if self.active.is_none() {
                self.active = Some((self.page_alloc)()?);
                page_full = true;
            }
            let mref = self.active.as_mut().unwrap();

            match mref.try_consume_bytes(size, size) {
                Some(it) => break it,
                _ => {
                    if page_full {
                        return Err(Error::LogEntryTooLarge {});
                    }
                    self.active = None;
                    continue;
                }
            }
        })
    }
}

struct ObjectHeader {
    size: usize,
    version: Version,
    parent: UntypedPointer,
    other: UntypedPointer,
}

impl ObjectHeader {
    pub fn new(size: usize, version: Version, other: UntypedPointer) -> Self {
        ObjectHeader {
            size,
            version,
            parent: UntypedPointer::new_none(),
            other,
        }
    }

    fn from_slice(data: &[u8]) -> &Self {
        unsafe_utils::any_from_slice(data)
    }

    fn from_slice_mut(data: &mut [u8]) -> &mut Self {
        unsafe_utils::any_from_slice_mut(data)
    }
}

pub struct TransactionalObjectAllocator<'tx> {
    generic: GenericAllocator<'tx>,
}

impl<'tx> TransactionalObjectAllocator<'tx> {
    fn new(page_alloc: PageAlloc<'tx>) -> Self {
        TransactionalObjectAllocator {
            generic: GenericAllocator::new(page_alloc),
        }
    }

    pub fn alloc_new(
        &mut self,
        size: usize,
        version: Version,
    ) -> Result<(UntypedPointer, &'tx mut [u8])> {
        self.alloc(size, version, UntypedPointer::new_none())
    }

    pub fn alloc(
        &mut self,
        size: usize,
        version: Version,
        other: UntypedPointer,
    ) -> Result<(UntypedPointer, &'tx mut [u8])> {
        let (slice, data) = self.generic.alloc(size + size_of::<ObjectHeader>())?;

        let (hdr, userdata) = data.split_at_mut(size_of::<ObjectHeader>());

        let hdrp = ObjectHeader::from_slice_mut(hdr);

        *hdrp = ObjectHeader::new(size, version, other);

        let (_, userslice) = slice.split_at(size_of::<ObjectHeader>());

        Ok((
            UntypedPointer::new_byte_addressable(userslice.address()),
            userdata,
        ))
    }
}

struct LogSegmentHeader {}

struct LogEntryHeader {
    slice: LogicalSlice,
}

impl LogEntryHeader {
    pub fn new(slice: LogicalSlice) -> Self {
        LogEntryHeader { slice }
    }

    fn from_slice(data: &[u8]) -> &Self {
        unsafe_utils::any_from_slice(data)
    }

    fn from_slice_mut(data: &mut [u8]) -> &mut Self {
        unsafe_utils::any_from_slice_mut(data)
    }
}

const LOG_ENTRY_OVERHEAD: usize = size_of::<LogEntryHeader>();

pub struct TransactionalLogAllocator<'tx> {
    generic: GenericAllocator<'tx>,
}

impl<'tx> TransactionalLogAllocator<'tx> {
    fn new(page_alloc: PageAlloc<'tx>) -> Self {
        TransactionalLogAllocator {
            generic: GenericAllocator::new(page_alloc),
        }
    }

    pub fn new_indirect_version(&mut self) -> Result<Version> {
        let (slice, data) = self.generic.alloc(size_of::<Version>())?;

        let versionp = unsafe_utils::any_from_slice_mut::<Version>(data);
        *versionp = Version::new();

        let ptr = UntypedPointer::new_byte_addressable(slice.address());

        Ok(Version::new_indirect(ptr))
    }

    pub fn copy(&mut self, dest: &'tx [u8], src: &[u8]) -> Result<LogicalSlice> {
        todo!()

        /*        let (slice, data) = self
                .generic
                .alloc(src.len() + size_of::<LogEntryHeader>())?;

            let (hdrp, logdata) = data.split_at_mut(size_of::<LogEntryHeader>());
            let hdr = LogEntryHeader::from_slice_mut(hdrp);
            *hdr = LogEntryHeader::new(dest);
            logdata.copy_from_slice(src);

            Ok(slice)
        */
    }
}

pub struct VersionedReader<'tx, 'data> {
    version: usize,
    las: &'tx LogicalAddressSpace<'data>,
    phantom: PhantomData<&'tx u8>,
}

impl<'tx, 'data> VersionedReader<'tx, 'data> {
    pub fn new(version: usize, las: &'tx LogicalAddressSpace<'data>) -> Self {
        VersionedReader {
            version,
            las,
            phantom: PhantomData,
        }
    }

    pub fn read(
        &self,
        pointer: &UntypedPointer,
        len: usize,
        abort_on_conflict: bool,
    ) -> Result<&'tx [u8]> {
        if !pointer.is_some() {
            return Err(Error::InvalidLogicalAddress {});
        }

        let real_offset = pointer.address() - size_of::<ObjectHeader>();
        let real_len = len + size_of::<ObjectHeader>();

        let slice = LogicalSlice::new(real_offset, real_len);

        let data = self.las.read(&slice)?;

        let (hdr, userdata) = data.split_at(size_of::<ObjectHeader>());

        let hdrp = ObjectHeader::from_slice(hdr);
        let version = hdrp.version.read(self.las)?;
        if version == 0 || version > self.version {
            if abort_on_conflict {
                Err(Error::TxAborted {})
            } else {
                self.read(&hdrp.other, len, abort_on_conflict)
            }
        } else {
            Ok(userdata)
        }
    }
}

pub struct VersionedObjectStore<'data> {
    phantom: PhantomData<&'data u8>,
    version: RwLock<usize>,
}

impl<'data> VersionedObjectStore<'data> {
    pub fn new() -> Self {
        VersionedObjectStore {
            phantom: PhantomData,
            version: RwLock::new(1),
        }
    }

    pub fn new_object_allocator<'tx>(
        &self,
        page_alloc: PageAlloc<'tx>,
    ) -> TransactionalObjectAllocator<'tx> {
        TransactionalObjectAllocator::new(page_alloc)
    }

    pub fn new_log_allocator<'tx>(
        &self,
        page_alloc: PageAlloc<'tx>,
    ) -> TransactionalLogAllocator<'tx> {
        TransactionalLogAllocator::new(page_alloc)
    }

    pub fn new_versioned_reader<'tx>(
        &self,
        las: &'tx LogicalAddressSpace<'data>,
    ) -> VersionedReader<'tx, 'data> {
        VersionedReader::new(*self.version.read(), las)
    }

    pub fn valid_page(data: &[u8]) -> bool {
        let header = ObjectHeader::from_slice(data);
        header.size != 0
    }

    pub fn commit_version<F>(
        &self,
        version: &Version,
        las: &LogicalAddressSpace,
        validate: F,
    ) -> Result<()>
    where
        F: FnOnce() -> Result<()>,
    {
        let mut new_version = self.version.write();
        *new_version += 1;

        validate()?;

        version.commit(*new_version, las)
    }
}
