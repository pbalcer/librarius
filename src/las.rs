use crate::error::{Error, Result};
use crate::source::{Page, Source, SourceAllocator};
use crate::utils::{crc, crc_slice, math, unsafe_utils, OptionExt};
use memoffset::offset_of;
use parking_lot::RwLock;
use std::collections::BTreeMap;
use std::mem::size_of;
use std::ops::{Bound::Included, Deref, DerefMut};
use std::sync::Arc;

pub type LogicalAddress = usize;

#[derive(Copy, Clone)]
pub struct LogicalSlice {
    offset: LogicalAddress,
    len: usize,
}

impl LogicalSlice {
    pub fn new(offset: LogicalAddress, len: usize) -> Self {
        LogicalSlice { offset, len }
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

    pub fn address(&self) -> LogicalAddress {
        self.offset
    }

    pub fn len(&self) -> usize {
        self.len
    }
}

struct PageHeader {}

impl PageHeader {
    fn new() -> Self {
        PageHeader {}
    }

    fn init(&mut self) {
        *self = PageHeader::new();
    }
}

struct MetaData {
    slice: LogicalSlice,
}

pub const ROOT_SIZE: usize = 8;
const ROOT_NONE: [u8; ROOT_SIZE] = [0; ROOT_SIZE];

struct Meta {
    hdr: PageHeader,
    data: MetaData,
    crc: u32,
    root: [u8; ROOT_SIZE],
    root_crc: u32,
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
            root_crc: 0,
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

pub struct LogicalAddressSpace<'data> {
    sources: RwLock<BTreeMap<LogicalAddress, Arc<SourceAllocator<'data>>>>,
    pagesize: usize,
    root: RwLock<Option<LogicalSlice>>,
}

impl<'data> LogicalAddressSpace<'data> {
    pub fn new(pagesize: usize) -> Self {
        LogicalAddressSpace {
            sources: RwLock::new(BTreeMap::new()),
            pagesize,
            root: RwLock::new(None),
        }
    }

    pub fn boxed_page_alloc<'tx>(&'tx self) -> PageAlloc<'tx> {
        Box::new(move || self.alloc())
    }

    fn allocate_address_space(&mut self, len: usize) -> LogicalSlice {
        let sources = self.sources.write();

        let last = sources.iter().next_back();

        let offset = last.map_or(0, |(offset, allocator)| offset + allocator.length());

        LogicalSlice::new(offset, len)
    }

    pub fn attach<F>(&mut self, source: impl Source + 'data, valid: F) -> Result<()>
    where
        F: Fn(&[u8]) -> bool,
    {
        let allocator = SourceAllocator::new(source, self.pagesize, |data| valid(data))?;

        let metapage = allocator.get_meta()?;

        let (slice, root) = {
            let mut data = vec![0; self.pagesize];
            allocator.read_into(&metapage, 0, &mut data)?;
            let metap: &mut Meta = unsafe_utils::any_from_slice_mut(data.as_mut_slice());
            if metap.is_valid() {
                let root = if metap.root == ROOT_NONE {
                    None
                } else {
                    Some(metap.root)
                };
                (metap.slice().clone(), root)
            } else {
                let meta = Meta::new(self.allocate_address_space(allocator.length()));
                let data = unsafe_utils::any_as_slice(&meta);

                allocator.write_from(&metapage, 0, data)?;

                (meta.slice().clone(), None)
            }
        };

        if let Some(root) = root {
            let mut root_location = self.root.write();
            if root_location.is_some() {
                return Err(Error::RootExists {});
            }

            *root_location = Some(LogicalSlice::new(
                slice.offset + offset_of!(Meta, root),
                ROOT_SIZE,
            ));
        }

        let mut sources = self.sources.write();
        let start = slice.offset;
        let end = start + slice.len - 1;
        if sources.range(start..end).count() != 0 {
            /* XXX: this has to check if there isn't any earlier mapping */
            Err(Error::InvalidLogicalAddress {})
        } else {
            sources.insert(start, Arc::new(allocator));
            Ok(())
        }
    }

    fn page_valid(bytes: &[u8]) -> bool {
        false
    }

    fn get_best_source<F>(&self, f: F) -> Option<(usize, Arc<SourceAllocator<'data>>)>
    where
        F: Fn(&Arc<SourceAllocator>) -> bool,
    {
        self.sources
            .read()
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

    pub fn root_location(&self) -> Result<LogicalSlice> {
        if let Some(root_location) = self.root.read().as_ref() {
            return Ok(root_location.clone());
        }

        let mut root = self.root.write();

        let root_location = root.get_or_insert_with_result(|| {
            let (base_offset, source) = self
                .get_best_byte_addressable()
                .ok_or(Error::NoAvailableMemory {})?;

            let metapage = source.get_meta()?;

            Ok(LogicalSlice::new(
                base_offset + metapage.offset() + offset_of!(Meta, root),
                ROOT_SIZE,
            ))
        })?;

        Ok(root_location.clone())
    }

    pub fn flush_root(&self) -> Result<()> {
        let root = self.root.read().unwrap();
        let root_data = self.read(&root)?;

        let (base_offset, source) = self
            .get_best_persistent()
            .ok_or(Error::NoAvailableMemory {})?;

        let metapage = source.get_meta()?;

        let mut data = vec![0; self.pagesize];
        source.read_into(&metapage, 0, &mut data)?;
        let metap: &mut Meta = unsafe_utils::any_from_slice_mut(data.as_mut_slice());
        metap.root.copy_from_slice(root_data);
        metap.root_crc = crc_slice(root_data);

        source.write_from(&metapage, 0, data.as_slice())?;

        Ok(())
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
        let sources = self.sources.read();

        let (base_offset, source) = sources
            .range((Included(&0), Included(&slice.offset)))
            .next_back()
            .ok_or(Error::InvalidLogicalAddress {})?;

        f(*base_offset, source.clone())
    }

    pub fn cancel(&self, mref: LogicalMutRef<'data>) {
        let err = self.with_source(&mref.slice, |base_offset, source| {
            source.free_page(mref.slice.to_page(self.pagesize, base_offset))
        });
        debug_assert!(err.is_ok())
    }

    pub fn flush(&self, slice: &LogicalSlice) -> Result<()> {
        todo!()
    }

    fn read_from_storage(&self, slice: &LogicalSlice) -> Result<LogicalSlice> {
        todo!()
    }

    pub fn read(&self, slice: &LogicalSlice) -> Result<&'data [u8]> {
        self.with_source(slice, |base_offset, source| {
            let page = slice.to_page(self.pagesize, base_offset);

            let data: Option<&'data [u8]> = source.get_bytes(&page)?;

            if let Some(data) = data {
                let start = slice.page_offset(page, base_offset);
                let end = start + slice.len();
                let data = &data[start..end];

                Ok(data)
            } else {
                todo!()
            }
        })
    }

    pub fn write(&self, slice: &LogicalSlice) -> Result<LogicalMutRef<'data>> {
        self.with_source(slice, |base_offset, source| {
            let page = slice.to_page(self.pagesize, base_offset);

            let data: Option<&'data mut [u8]> = source.get_bytes_mut(&page)?;

            if let Some(data) = data {
                let (hdrp, datap) = data.split_at_mut(size_of::<PageHeader>());

                let start = slice.page_offset(page, base_offset) - size_of::<PageHeader>();
                let end = start + slice.len();
                let data = &mut datap[start..end];

                Ok(LogicalMutRef::new(data, slice.clone()))
            } else {
                todo!()
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::MemorySource;

    #[test]
    fn basic_test() -> Result<()> {
        let mut las = LogicalAddressSpace::new(4096);
        las.attach(MemorySource::new(1 << 20)?, |data| false)?;

        let root = las.root_location()?;

        let slice = las.read(&root)?;

        let data = [0 as u8; ROOT_SIZE];

        assert_eq!(data, slice.deref());

        Ok(())
    }
}
