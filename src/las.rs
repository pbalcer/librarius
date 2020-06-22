use crate::error::{Error, Result};
use crate::source::{Page, Source, SourceAllocator};
use crate::utils::{crc, crc_slice, math, unsafe_utils, OptionExt};
use memoffset::offset_of;
use parking_lot::RwLock;
use std::collections::{hash_map::Entry, BTreeMap, HashMap};
use std::mem::size_of;
use std::ops::{Bound::Included, Deref, DerefMut};
use std::{fmt::Debug, sync::Arc};

pub type LogicalAddress = usize;

#[derive(Copy, Clone, Debug)]
pub struct LogicalSlice {
    offset: LogicalAddress,
    len: usize,
}

impl LogicalSlice {
    pub fn new(offset: LogicalAddress, len: usize) -> Self {
        LogicalSlice { offset, len }
    }

    pub fn none() -> Self {
        LogicalSlice::new(0, 0)
    }

    pub fn split_at(self, mid: usize) -> (Self, Self) {
        let left = LogicalSlice {
            offset: self.offset,
            len: mid,
        };
        let right = LogicalSlice {
            offset: self.offset + mid,
            len: self.len - mid,
        };
        (left, right)
    }

    fn from_page(page: Page, base_offset: LogicalAddress) -> Self {
        LogicalSlice {
            offset: page.offset() + base_offset + size_of::<PageHeader>(),
            len: page.len() - size_of::<PageHeader>(),
        }
    }

    fn from_page_raw(page: Page, base_offset: LogicalAddress) -> Self {
        LogicalSlice {
            offset: page.offset() + base_offset,
            len: page.len(),
        }
    }

    fn page_offset(&self, page: Page, base_offset: LogicalAddress) -> LogicalAddress {
        self.offset - base_offset - page.offset()
    }

    fn to_page(&self, pagesize: usize, base_offset: LogicalAddress) -> Page {
        let offset = math::align_down(self.offset - base_offset, pagesize);
        let len = math::align_up(self.len, pagesize);
        assert_eq!(len, pagesize); //a single page can't span two locations

        Page::new(offset, len)
    }

    fn page_aligned(&self, pagesize: usize) -> Self {
        let offset = math::align_down(self.offset, pagesize);
        let len = math::align_up(self.len, pagesize);
        LogicalSlice::new(offset, len)
    }

    pub fn address(&self) -> LogicalAddress {
        self.offset
    }

    pub fn len(&self) -> usize {
        self.len
    }
}

#[derive(Debug)]
struct PageHeader {}

impl PageHeader {
    fn new() -> Self {
        PageHeader {}
    }

    fn init(&mut self) {
        *self = PageHeader::new();
    }
}

#[derive(Debug)]
struct MetaData {
    slice: LogicalSlice,
}

pub const ROOT_SIZE: usize = 64;

struct Meta {
    hdr: PageHeader,
    data: MetaData,
    crc: u32,
    root: [u8; ROOT_SIZE],
}

impl Debug for Meta {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Meta")
            .field("data", &self.data)
            .field("crc", &self.crc)
            .field("root", &self.root[8])
            .finish()
    }
}

impl Meta {
    fn new(slice: LogicalSlice) -> Self {
        let data = MetaData { slice };
        let crc = crc(&data);

        Meta {
            hdr: PageHeader::new(),
            data,
            crc,
            root: [0; ROOT_SIZE],
        }
    }

    pub fn slice(&self) -> &LogicalSlice {
        &self.data.slice
    }

    pub fn is_valid(&self) -> bool {
        self.crc == crc(&self.data)
    }
}

pub struct LogicalMutRef<'data> {
    data: &'data mut [u8],
    slice: LogicalSlice,
}

impl<'data> LogicalMutRef<'data> {
    fn new(data: &'data mut [u8], slice: LogicalSlice) -> Self {
        LogicalMutRef { data, slice }
    }

    pub fn try_consume_bytes(
        &mut self,
        size: usize,
        min: usize,
    ) -> Option<(LogicalSlice, &'data mut [u8])> {
        let len = std::cmp::min(self.slice.len, size);
        if len < min {
            return None;
        }

        let slice = LogicalSlice::new(self.slice.offset, len);

        self.slice.offset += len;
        self.slice.len -= len;

        let (new, old) = unsafe { std::mem::transmute(self.data.split_at_mut(len)) };
        self.data = old;

        Some((slice, new))
    }
}

impl<'data> Deref for LogicalMutRef<'data> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

impl<'data> DerefMut for LogicalMutRef<'data> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.data
    }
}

const CONTEXT_SIZE: usize = 16;

pub type PageAlloc<'tx> = Box<dyn Fn() -> Result<LogicalMutRef<'tx>> + 'tx>;

#[derive(Copy, Clone, Debug)]
pub struct ByteLogicalSlice(pub LogicalSlice);

#[derive(Copy, Clone, Debug)]
pub struct BlockLogicalSlice(pub LogicalSlice);

#[derive(Copy, Clone, Debug)]
pub enum StoredLogicalSlice {
    Block(BlockLogicalSlice),
    Byte(ByteLogicalSlice),
}

impl StoredLogicalSlice {
    pub fn new_byte(slice: LogicalSlice) -> Self {
        StoredLogicalSlice::Byte(ByteLogicalSlice(slice))
    }

    pub fn new_block(slice: LogicalSlice) -> Self {
        StoredLogicalSlice::Block(BlockLogicalSlice(slice))
    }

    pub fn new(slice: LogicalSlice, byte_addressable: bool) -> Self {
        match byte_addressable {
            true => Self::new_byte(slice),
            false => Self::new_block(slice),
        }
    }

    pub fn raw(&self) -> &LogicalSlice {
        match self {
            StoredLogicalSlice::Block(slice) => &slice.0,
            StoredLogicalSlice::Byte(slice) => &slice.0,
        }
    }

    pub fn unwrap_byte(self) -> ByteLogicalSlice {
        match self {
            StoredLogicalSlice::Block(_) => panic!("unwrap byte on block"),
            StoredLogicalSlice::Byte(b) => b,
        }
    }

    pub fn unwrap_block(self) -> BlockLogicalSlice {
        match self {
            StoredLogicalSlice::Block(b) => b,
            StoredLogicalSlice::Byte(_) => panic!("unwrap block on byte"),
        }
    }
}

pub struct LogicalAddressSpace<'data> {
    sources: BTreeMap<LogicalAddress, Arc<SourceAllocator<'data>>>,
    pagesize: usize,
    root: StoredLogicalSlice,
    root_bytes: ByteLogicalSlice,
    backing: RwLock<HashMap<LogicalAddress, StoredLogicalSlice>>,
}

impl<'data> LogicalAddressSpace<'data> {
    pub fn new<F>(
        pagesize: usize,
        raw_sources: impl Iterator<Item = Box<dyn Source + 'data>>,
        valid: F,
        create: bool,
    ) -> Result<Self>
    where
        F: Fn(&[u8]) -> bool,
    {
        let mut sources = BTreeMap::new();
        let mut unallocated = Vec::new();
        let mut root = None;

        for source in raw_sources {
            let allocator = SourceAllocator::new(source, pagesize, |data| valid(data))?;
            let metapage = allocator.get_meta()?;

            let mut data = vec![0; pagesize];
            allocator.read_into(&metapage, 0, &mut data)?;
            let metap: &mut Meta = unsafe_utils::any_from_slice_mut(data.as_mut_slice());
            if metap.is_valid() {
                if !metap.root.iter().all(|v| *v == 0) {
                    if root.is_some() {
                        return Err(Error::RootExists {});
                    }
                    let slice =
                        LogicalSlice::new(metap.slice().offset + offset_of!(Meta, root), ROOT_SIZE);
                    root = Some(StoredLogicalSlice::new(
                        slice,
                        allocator.is_byte_addressable(),
                    ));
                };
                let start = metap.slice().offset;
                let end = start + metap.slice().len - 1;
                if sources.range(start..end).count() != 0 {
                    /* XXX: this has to check if there isn't any earlier mapping */
                    return Err(Error::InvalidLogicalAddress {});
                } else {
                    sources.insert(start, Arc::new(allocator));
                }
            } else {
                if !create {
                    return Err(Error::OpenOnUninitialized {});
                }
                unallocated.push(allocator);
            }
        }

        for source in unallocated {
            let last = sources.iter().next_back();
            let offset = last.map_or(0, |(offset, allocator)| offset + allocator.length());

            let slice = LogicalSlice::new(offset, source.length());

            let meta = Meta::new(slice);
            let data = unsafe_utils::any_as_slice(&meta);

            let metapage = source.get_meta()?;

            source.write_from(&metapage, 0, data)?;

            sources.insert(meta.slice().offset, Arc::new(source));
        }

        let mut las = LogicalAddressSpace {
            sources,
            pagesize,
            root: StoredLogicalSlice::new_byte(LogicalSlice::none()),
            root_bytes: ByteLogicalSlice(LogicalSlice::none()),
            backing: RwLock::new(HashMap::new()),
        };

        if root.is_none() {
            assert!(create);
            println!("root none");

            let (base_offset, source) = las
                .get_best_persistent()
                .or_else(|| las.get_best_byte_addressable())
                .ok_or(Error::NoAvailableMemory {})?;

            let metapage = source.get_meta()?;
            let slice = LogicalSlice::new(
                base_offset + metapage.offset() + offset_of!(Meta, root),
                ROOT_SIZE,
            );
            let slice = StoredLogicalSlice::new(slice, source.is_byte_addressable());
            root = Some(slice);
        }
        las.root = root.unwrap();

        if let StoredLogicalSlice::Block(block) = &las.root {
            let slice_aligned = block.0.page_aligned(pagesize);
            let slice = StoredLogicalSlice::new_block(slice_aligned);

            let root_bytes = las.fetch(&slice)?;
            {
                let data = las.read(&root_bytes)?;
                let metap = unsafe_utils::any_from_slice::<Meta>(data);
                println!("fetched {:?} {:?}", root_bytes, metap);
            }

            println!("inserting {:?} {:?}", root_bytes.0.address(), slice);
            las.backing.write().insert(root_bytes.0.address(), slice);

            let slice = LogicalSlice::new(
                root_bytes.0.address() + offset_of!(Meta, root),
                las.root.raw().len,
            );
            las.root_bytes = StoredLogicalSlice::new_byte(slice).unwrap_byte();
        } else {
            las.root_bytes = las.root.unwrap_byte().clone();
        }

        Ok(las)
    }

    pub fn boxed_page_alloc<'tx>(&'tx self) -> PageAlloc<'tx> {
        Box::new(move || self.alloc())
    }

    fn page_valid(bytes: &[u8]) -> bool {
        false
    }

    fn get_best_source<F>(&self, f: F) -> Option<(usize, Arc<SourceAllocator<'data>>)>
    where
        F: Fn(&Arc<SourceAllocator>) -> bool,
    {
        self.sources
            .iter()
            .find(|(_, s)| f(s))
            .map(|(base_offset, source)| (*base_offset, source.clone()))
    }

    fn get_best_persistent(&self) -> Option<(usize, Arc<SourceAllocator<'data>>)> {
        self.get_best_source(|s| s.is_persistent())
    }

    fn get_best_byte_addressable(&self) -> Option<(usize, Arc<SourceAllocator<'data>>)> {
        self.get_best_source(|s| s.is_byte_addressable())
    }

    pub fn root_location(&self) -> &ByteLogicalSlice {
        &self.root_bytes
    }

    pub fn get_backing(&self, slice: &ByteLogicalSlice) -> Result<Option<StoredLogicalSlice>> {
        let slice_aligned = slice.0.page_aligned(self.pagesize);
        self.with_source(&slice_aligned, |base_offset, source| {
            let page = slice_aligned.to_page(self.pagesize, base_offset);
            let offset = slice.0.page_offset(page, base_offset);

            if source.is_persistent() {
                return Ok(Some(StoredLogicalSlice::Byte(slice.clone())));
            }
            let backing = self.backing.read().get(&slice_aligned.address()).copied();
            if let Some(backing) = backing {
                let slice = LogicalSlice::new(backing.raw().address() + offset, slice.0.len);
                Ok(Some(match backing {
                    StoredLogicalSlice::Block(_) => StoredLogicalSlice::new_block(slice),
                    StoredLogicalSlice::Byte(_) => StoredLogicalSlice::new_byte(slice),
                }))
            } else {
                Ok(None)
            }
        })
    }

    fn allocate_backing(&self, key: LogicalAddress) -> Result<()> {
        match self.backing.write().entry(key) {
            Entry::Occupied(_) => {}
            Entry::Vacant(v) => {
                let (base_offset, allocator) = self
                    .get_best_persistent()
                    .ok_or(Error::NoAvailableMemory {})?;
                let page = allocator.allocate_page()?;
                let slice_new = LogicalSlice::from_page(page, base_offset);
                v.insert(StoredLogicalSlice::new(
                    slice_new,
                    allocator.is_byte_addressable(),
                ));
            }
        }
        Ok(())
    }

    pub fn flush(&self, slice: &ByteLogicalSlice) -> Result<StoredLogicalSlice> {
        let slice_aligned = slice.0.page_aligned(self.pagesize);

        /* XXX: this is really inefficient and always flushes the entire page... */
        self.with_source(&slice_aligned, |base_offset, source| {
            assert!(source.is_byte_addressable());
            let page = slice_aligned.to_page(self.pagesize, base_offset);
            let data = source.get_bytes(&page)?.unwrap();
            let offset = slice.0.page_offset(page, base_offset);

            if source.is_persistent() {
                source.flush_partial(data)?;
                Ok(StoredLogicalSlice::Byte(slice.clone()))
            } else {
                let backing = self.backing.read().get(&slice_aligned.address()).copied();
                if let Some(backing) = backing {
                    println!("flushing {:?} {:?}", slice_aligned.address(), backing);
                    self.with_source(&backing.raw(), |dst_base_offset, dst_source| {
                        assert!(dst_source.is_persistent());

                        let dst_page = backing.raw().to_page(self.pagesize, dst_base_offset);
                        println!("writing data... {:?}", dst_page);
                        if dst_page.offset() == 4096 {
                            let metap = unsafe_utils::any_from_slice::<Meta>(data);
                            println!("flushing {:?} {:?}", slice_aligned, metap);
                        }
                        dst_source.write_from(&dst_page, 0, data)?;

                        Ok(())
                    })?;

                    let slice = LogicalSlice::new(backing.raw().address() + offset, slice.0.len);
                    Ok(match backing {
                        StoredLogicalSlice::Block(_) => StoredLogicalSlice::new_block(slice),
                        StoredLogicalSlice::Byte(_) => StoredLogicalSlice::new_byte(slice),
                    })
                } else {
                    self.allocate_backing(slice_aligned.address())?;
                    self.flush(slice)
                }
            }
        })
    }

    pub fn alloc<'tx>(&'tx self) -> Result<LogicalMutRef<'tx>>
    where
        'data: 'tx,
    {
        let (base_offset, source) = self
            .get_best_byte_addressable()
            .ok_or(Error::NoAvailableMemory {})?;

        let page = source.allocate_page()?;

        let data = source.get_bytes_mut(&page)?.unwrap();

        let slice = LogicalSlice::from_page(page, base_offset);
        let page_data_offset = slice.page_offset(page, base_offset);

        let (hdr, udata) = data.split_at_mut(page_data_offset);

        let hdr = unsafe_utils::any_from_slice_mut::<PageHeader>(hdr);
        hdr.init();

        Ok(LogicalMutRef::new(udata, slice))
    }

    pub fn publish(&self, mref: LogicalMutRef<'data>) -> LogicalSlice {
        mref.slice
    }

    fn with_source<F, R>(&self, slice: &LogicalSlice, f: F) -> Result<R>
    where
        F: FnOnce(usize, Arc<SourceAllocator<'data>>) -> Result<R>,
    {
        let (base_offset, source) = self
            .sources
            .range((Included(&0), Included(&slice.offset)))
            .next_back()
            .ok_or(Error::InvalidLogicalAddress {})?;

        f(*base_offset, source.clone())
    }

    pub fn read(&self, slice: &ByteLogicalSlice) -> Result<&'data [u8]> {
        let raw = &slice.0;
        self.with_source(raw, |base_offset, source| {
            assert!(source.is_byte_addressable());
            let page = raw.to_page(self.pagesize, base_offset);

            let data = source.get_bytes(&page)?.unwrap();

            let start = raw.page_offset(page, base_offset);
            let end = start + raw.len();
            let data = &data[start..end];

            Ok(data)
        })
    }

    pub fn fetch(&self, slice: &StoredLogicalSlice) -> Result<ByteLogicalSlice> {
        let raw = slice.raw();
        let mut src_data = vec![0 as u8; self.pagesize];

        let offset = self.with_source(raw, |base_offset, source| {
            let page = raw.to_page(self.pagesize, base_offset);

            source.read_into(&page, 0, src_data.as_mut_slice())?;

            Ok(raw.page_offset(page, base_offset))
        })?;

        let mut page = self.alloc()?;

        page.copy_from_slice(src_data.as_slice());

        println!("fetch with {}", offset);

        let slice = LogicalSlice::new(page.slice.address() + offset, raw.len);

        Ok(ByteLogicalSlice(slice))
    }

    pub fn write(&self, slice: &ByteLogicalSlice) -> Result<&'data mut [u8]> {
        let raw = &slice.0;
        self.with_source(raw, |base_offset, source| {
            assert!(source.is_byte_addressable());
            let page = raw.to_page(self.pagesize, base_offset);

            let data = source.get_bytes_mut(&page)?.unwrap();

            let start = raw.page_offset(page, base_offset);
            let end = start + raw.len();
            let data = &mut data[start..end];

            Ok(data)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::is_enum_variant;
    use crate::source::MemorySource;

    use std::iter;

    #[test]
    fn basic_test() -> Result<()> {
        let source: Box<dyn Source> = Box::new(MemorySource::new(1 << 20)?);
        let las = LogicalAddressSpace::new(4096, iter::once(source), |data| false, true)?;

        let root = las.root_location();

        let slice = las.read(&root)?;
        let data = [0 as u8; ROOT_SIZE];

        let result = las.flush(root);
        assert!(is_enum_variant!(
            result.unwrap_err(),
            Error::NoAvailableMemory {}
        ));

        Ok(())
    }
}
