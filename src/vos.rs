use crate::error::{Error, Result};
use crate::las::{
    BlockLogicalSlice, ByteLogicalSlice, LogicalAddress, LogicalAddressSpace, LogicalMutRef,
    LogicalSlice, PageAlloc, StoredLogicalSlice,
};
use crate::utils::{unsafe_utils, OptionExt};
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

    fn internal_clone(&self) -> Self {
        UntypedPointer {
            address: AtomicUsize::new(self.address_internal()),
        }
    }

    pub(crate) fn new_from_block(slice: &BlockLogicalSlice) -> Self {
        Self::new_block(slice.0.address())
    }

    pub(crate) fn new_from_byte(slice: &ByteLogicalSlice) -> Self {
        Self::new_byte(slice.0.address())
    }

    pub(crate) fn new_from_stored(slice: &StoredLogicalSlice) -> Self {
        match slice {
            StoredLogicalSlice::Block(slice) => Self::new_from_block(slice),
            StoredLogicalSlice::Byte(slice) => Self::new_from_byte(slice),
        }
    }

    pub(crate) fn new_byte(address: LogicalAddress) -> Self {
        UntypedPointer {
            address: AtomicUsize::new(address | Self::POINTER_BYTE_ADDRESSABLE),
        }
    }

    pub fn is_some(&self) -> bool {
        self.address() != 0
    }

    pub fn is_none(&self) -> bool {
        self.address() == 0
    }

    fn new_block(address: LogicalAddress) -> Self {
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

    fn into_stored_slice(&self, len: usize) -> StoredLogicalSlice {
        let slice = LogicalSlice::new(self.address(), len);
        StoredLogicalSlice::new(slice, self.is_byte_addressable())
    }

    fn into_stored_slice_offset(&self, len: usize, offset: usize) -> StoredLogicalSlice {
        let slice = LogicalSlice::new(self.address() - offset, len + offset);
        StoredLogicalSlice::new(slice, self.is_byte_addressable())
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
        self.version.load(Ordering::SeqCst) & Self::VERSION_TYPE_MASK
    }

    fn data_bytes(&self) -> usize {
        self.version.load(Ordering::SeqCst) & Self::VERSION_DATA_MASK
    }

    fn commit(&self, new_version: usize, las: &LogicalAddressSpace) -> Result<()> {
        if self.type_bytes() == Self::VERSION_TYPE_DIRECT {
            self.version
                .store(new_version | Self::VERSION_TYPE_DIRECT, Ordering::SeqCst);

            Ok(())
        } else {
            let data = self.data_bytes();
            let ptr = UntypedPointer::from_raw(data);

            let slice = ptr.into_stored_slice(size_of::<Version>());

            let data = las.read(&slice.unwrap_byte())?;

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

    pub fn newer(&self, other: &Version, las: &LogicalAddressSpace) -> Result<bool> {
        let s = self.read(las)?;
        let o = other.read(las)?;
        Ok(s > o)
    }

    fn read(&self, las: &LogicalAddressSpace) -> Result<usize> {
        let data = self.data_bytes();

        if self.type_bytes() == Self::VERSION_TYPE_DIRECT {
            Ok(data)
        } else {
            let ptr = UntypedPointer::from_raw(data);
            let slice = ptr.into_stored_slice(size_of::<Version>());
            let data = las.read(&slice.unwrap_byte())?;

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
                        return Err(Error::AllocationTooLarge {});
                    }
                    self.active = None;
                    continue;
                }
            }
        })
    }
}

#[derive(Copy, Clone, Debug)]
pub struct ObjectSize {
    pub pointers: u32,
    pub data: u32,
}

impl ObjectSize {
    pub fn new(pointers: u32, data: u32) -> Self {
        ObjectSize { pointers, data }
    }

    pub fn new_with_usize(pointers: usize, data: usize) -> Self {
        /* XXX: proper conversions... */
        ObjectSize {
            pointers: pointers as u32,
            data: data as u32,
        }
    }

    pub fn total(&self) -> usize {
        (self.pointers + self.data) as usize
    }
}

pub struct ObjectHeader {
    pub size: ObjectSize,
    version: Version,
    parent: UntypedPointer,
    other: UntypedPointer,
}

impl ObjectHeader {
    fn new(size: ObjectSize, version: Version, other: UntypedPointer) -> Self {
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
        size: ObjectSize,
        version: Version,
    ) -> Result<(UntypedPointer, &'tx mut [u8])> {
        self.alloc(size, version, UntypedPointer::new_none())
    }

    pub fn init_object(
        &mut self,
        data: &'tx mut [u8],
        size: ObjectSize,
        version: Version,
        other: UntypedPointer,
    ) -> &'tx mut [u8] {
        let (hdr, userdata) = data.split_at_mut(size_of::<ObjectHeader>());

        let hdrp = ObjectHeader::from_slice_mut(hdr);

        *hdrp = ObjectHeader::new(size, version, other);

        userdata
    }

    pub fn alloc(
        &mut self,
        size: ObjectSize,
        version: Version,
        other: UntypedPointer,
    ) -> Result<(UntypedPointer, &'tx mut [u8])> {
        let (slice, data) = self
            .generic
            .alloc(size.total() + size_of::<ObjectHeader>())?;

        let userdata = self.init_object(data, size, version, other);

        let (_, userslice) = slice.split_at(size_of::<ObjectHeader>());

        Ok((UntypedPointer::new_byte(userslice.address()), userdata))
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

        let ptr = UntypedPointer::new_byte(slice.address());

        Ok(Version::new_indirect(ptr))
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

    pub fn read_version(&self, ptr: &UntypedPointer) -> Result<&Version> {
        let slice = ptr.into_stored_slice_offset(0, size_of::<ObjectHeader>());
        if let StoredLogicalSlice::Block(block) = slice {
            todo!()
        }

        let slice = slice.unwrap_byte();

        let hdr = self.las.read(&slice)?;

        let hdrp = ObjectHeader::from_slice(hdr);

        Ok(&hdrp.version)
    }

    pub fn flush(&self, ptr: &UntypedPointer) -> Result<()> {
        let slice = ptr.into_stored_slice_offset(0, size_of::<ObjectHeader>());
        if let StoredLogicalSlice::Block(block) = slice {
            return Ok(());
        }

        let slice = slice.unwrap_byte();

        let hdr = self.las.read(&slice)?;

        let hdrp = ObjectHeader::from_slice(hdr);
        let npointers = hdrp.size.pointers as usize / size_of::<UntypedPointer>();

        let slice = ptr
            .into_stored_slice(hdrp.size.pointers as usize)
            .unwrap_byte();

        let data = self.las.read(&slice)?.as_ptr() as *const UntypedPointer;
        let pointers: &[UntypedPointer] = unsafe { std::slice::from_raw_parts(data, npointers) };

        for p in pointers.iter().filter(|p| p.is_some()) {
            let oldptr = p.internal_clone();
            if p.is_byte_addressable() {
                let stored_slice = p.into_stored_slice(1).unwrap_byte();
                let mut backing = self.las.get_backing(&stored_slice)?;
                let backing = backing.get_or_insert_with_result(|| {
                    self.las.flush(&stored_slice)?;
                    Ok(self.las.get_backing(&stored_slice)?.unwrap())
                })?;
                let newptr = UntypedPointer::new_from_stored(backing);
                if !p.compare_and_swap(oldptr, newptr) { /* XXX: leaking memory... */ }
            }
        }

        self.las.flush(&slice)?;

        Ok(())
    }

    pub fn read(
        &self,
        ptr: &UntypedPointer,
        size: &ObjectSize,
        abort_on_conflict: bool,
    ) -> Result<(&'tx [u8], &ObjectHeader)> {
        if !ptr.is_some() {
            return Err(Error::InvalidLogicalAddress {});
        }

        let oldptr = ptr.internal_clone();
        let slice = oldptr.into_stored_slice_offset(size.total(), size_of::<ObjectHeader>());
        if let StoredLogicalSlice::Block(block) = slice {
            let slice = oldptr.into_stored_slice(size.total());

            let bytes = self.las.fetch(&slice)?;
            let newptr = UntypedPointer::new_from_byte(&bytes);
            if !ptr.compare_and_swap(oldptr, newptr) { /* XXX: leaking memory... */ }
            return self.read(ptr, size, abort_on_conflict);
        }

        let slice = slice.unwrap_byte();

        let data = self.las.read(&slice)?;

        let (hdr, userdata) = data.split_at(size_of::<ObjectHeader>());

        let hdrp = ObjectHeader::from_slice(hdr);
        let version = hdrp.version.read(self.las)?;
        if version == 0 || version > self.version {
            if abort_on_conflict {
                Err(Error::TxAborted {})
            } else {
                self.read(&hdrp.other, size, abort_on_conflict)
            }
        } else {
            Ok((userdata, hdrp))
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
        header.size.total() != 0
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
