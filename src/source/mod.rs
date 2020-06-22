use crate::{
    error::{Error, Result},
    utils::{crc, math, unsafe_utils},
};
use parking_lot::RwLock;
use std::collections::VecDeque;

pub mod file_source;
pub mod memory_source;

pub use file_source::FileSource;
pub use memory_source::MemorySource;

pub trait Source: Send + Sync {
    fn is_byte_addressable(&self) -> bool;
    fn is_persistent(&self) -> bool;
    fn perf_level(&self) -> usize;

    fn close(&mut self);

    fn length(&self) -> Result<usize>;

    fn read(&mut self, offset: usize, data: &mut [u8]) -> Result<()>;
    fn write(&mut self, offset: usize, data: &[u8]) -> Result<()>;
    fn flush(&mut self) -> Result<()>;

    fn at(&self, offset: usize, len: usize) -> Result<&[u8]>;
    fn at_mut(&mut self, offset: usize, len: usize) -> Result<&mut [u8]>;

    fn offset(&mut self, ptr: *const u8) -> Result<usize>;
    fn flush_slice(&self, slice: &[u8]) -> Result<()>;
}

const SOURCE_HEADER_MAGIC: u64 = 0xDEADBEEF;

struct SourceHeaderData {
    magic: u64,
    pagesize: u64,
}

struct SourceHeader {
    data: SourceHeaderData,
    crc32: u32,
}

impl SourceHeader {
    fn new(pagesize: usize) -> Self {
        let data = SourceHeaderData {
            magic: SOURCE_HEADER_MAGIC,
            pagesize: pagesize as u64,
        };
        let crc32 = crc(&data);

        SourceHeader { data, crc32 }
    }

    fn is_valid(&self) -> bool {
        self.data.magic == SOURCE_HEADER_MAGIC && self.crc32 == crc(&self.data)
    }

    fn pagesize(&self) -> usize {
        self.data.pagesize as usize
    }
}

#[derive(Copy, Clone, Debug)]
pub struct Page {
    offset: usize,
    len: usize,
}

impl Page {
    pub fn new(offset: usize, len: usize) -> Page {
        Page { offset, len: len }
    }

    fn split(&mut self, len: usize) -> Option<Page> {
        if self.len < len {
            return None;
        }
        let next = Page::new(self.offset, len);

        self.offset += len;
        self.len -= len;

        Some(next)
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn offset(&self) -> usize {
        self.offset
    }
}

// This is *very* ugly. Source trait needs to be changed to allow asynchronous
// I/O, and this implementation should follow.
pub struct SourceAllocator<'data> {
    source: RwLock<Box<dyn Source + 'data>>,
    freelist: RwLock<VecDeque<Page>>,
    pagesize: usize,
}

impl<'data> SourceAllocator<'data> {
    fn create(&mut self) -> Result<()> {
        let hdr = SourceHeader::new(self.pagesize);
        let data = unsafe_utils::any_as_slice(&hdr);

        let source = self.source.get_mut();

        source.write(0, data)?;
        source.flush()?;

        Ok(())
    }

    fn initialize<F>(&mut self, valid: F) -> Result<()>
    where
        F: Fn(&[u8]) -> bool,
    {
        let mut data = vec![0; self.pagesize];

        self.source.get_mut().read(0, &mut data)?;

        {
            let hdrp: &SourceHeader = unsafe_utils::any_from_slice(&data);
            if !hdrp.is_valid() {
                self.create()?;
            } else {
                if hdrp.pagesize() != self.pagesize {
                    return Err(Error::WrongPagesize {
                        is: hdrp.pagesize(),
                        expected: self.pagesize,
                    });
                }
            }
        }

        let mut base_offset = math::align_up(std::mem::size_of::<SourceHeader>(), self.pagesize);
        base_offset += self.pagesize; // metapage

        let base_size = self.length() - base_offset;

        let npages = base_size / self.pagesize;

        for n in 0..npages {
            let offset = base_offset + (n * self.pagesize);

            self.source.get_mut().read(offset, &mut data)?;

            if !valid(data.as_slice()) {
                self.free_page(Page::new(offset, self.pagesize))?;
            }
        }

        Ok(())
    }

    pub fn new<F>(source: Box<dyn Source + 'data>, pagesize: usize, valid: F) -> Result<Self>
    where
        F: Fn(&[u8]) -> bool,
    {
        let mut allocator = SourceAllocator {
            source: RwLock::new(source),
            freelist: RwLock::new(VecDeque::new()),
            pagesize,
        };

        allocator.initialize(valid)?;

        Ok(allocator)
    }

    pub fn get_meta(&self) -> Result<Page> {
        Ok(Page::new(self.pagesize, self.pagesize))
    }

    pub fn allocate_page(&self) -> Result<Page> {
        let mut freelist = self.freelist.write();

        let mut page = freelist.pop_front().ok_or(Error::NoAvailableMemory {})?;
        let allocated = page
            .split(self.pagesize)
            .ok_or(Error::NoAvailableMemory {})?;

        if page.len != 0 {
            freelist.push_front(page);
        }

        Ok(allocated)
    }

    pub fn get_bytes(&self, page: &Page) -> Result<Option<&'data [u8]>> {
        let source = self.source.read();

        if source.is_byte_addressable() {
            /*
             * XXX: unsafe
             */
            let bytes = unsafe { std::mem::transmute(source.at(page.offset, page.len)?) };
            Ok(Some(bytes))
        } else {
            Ok(None)
        }
    }

    pub fn get_bytes_mut(&self, page: &Page) -> Result<Option<&'data mut [u8]>> {
        let mut source = self.source.write();

        if source.is_byte_addressable() {
            /*
             * XXX: unsafe
             */
            let bytes = unsafe { std::mem::transmute(source.at_mut(page.offset, page.len)?) };
            Ok(Some(bytes))
        } else {
            Ok(None)
        }
    }

    pub fn read_into(&self, page: &Page, offset: usize, data: &mut [u8]) -> Result<()> {
        assert!(page.len >= data.len());

        self.source.write().read(page.offset, data)
    }

    pub fn write_from(&self, page: &Page, offset: usize, data: &[u8]) -> Result<()> {
        assert!(page.len >= data.len());
        let mut src = self.source.write();

        src.write(page.offset, data)?;
        src.flush()
    }

    pub fn flush(&self) -> Result<()> {
        self.source.write().flush()
    }

    pub fn flush_partial(&self, data: &[u8]) -> Result<()> {
        self.source.write().flush_slice(data)
    }

    pub fn free_page(&self, page: Page) -> Result<()> {
        self.freelist.write().push_back(page);
        Ok(())
    }

    pub fn is_byte_addressable(&self) -> bool {
        self.source.read().is_byte_addressable()
    }

    pub fn is_persistent(&self) -> bool {
        self.source.read().is_persistent()
    }

    pub fn length(&self) -> usize {
        math::align_down(self.source.read().length().unwrap(), self.pagesize)
    }
}
