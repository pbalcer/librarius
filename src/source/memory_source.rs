use crate::error::{Error, Result};
use crate::source::Source;
use errno;
use libc;
use std::ptr;

struct MemoryMap<'a> {
    data: &'a mut [u8],
}

unsafe impl<'a> Send for MemoryMap<'a> {}
unsafe impl<'a> Sync for MemoryMap<'a> {}

impl<'a> MemoryMap<'a> {
    fn from_existing(data: &'a mut [u8]) -> Self {
        MemoryMap { data }
    }

    fn new(len: usize) -> Result<Self> {
        let ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len as libc::size_t,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_ANONYMOUS | libc::MAP_SHARED,
                -1,
                0,
            )
        };

        if ptr == libc::MAP_FAILED {
            Err(Error::MemoryAlloc {
                errno: errno::errno(),
            })
        } else {
            Ok(MemoryMap {
                data: unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, len) },
            })
        }
    }

    fn at(&self, offset: usize, len: usize) -> Option<&[u8]> {
        if offset + len > self.data.len() {
            return None;
        }

        let end = offset + len;
        Some(&self.data[offset..end])
    }

    fn at_mut(&mut self, offset: usize, len: usize) -> Option<&mut [u8]> {
        if offset + len > self.data.len() {
            return None;
        }

        let end = offset + len;
        Some(&mut self.data[offset..end])
    }

    fn offset(&self, ptr: *const u8) -> isize {
        let base = self.data.as_ptr() as isize;
        let offptr = ptr as isize;
        offptr - base
    }

    fn len(&self) -> usize {
        self.data.len()
    }
}

impl<'a> Drop for MemoryMap<'a> {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(
                self.data.as_mut_ptr() as *mut core::ffi::c_void,
                self.data.len(),
            );
        }
    }
}

pub struct MemorySource<'a> {
    map: MemoryMap<'a>,
    persistent: bool,
}

impl<'a> MemorySource<'a> {
    pub fn new(len: usize) -> Result<Self> {
        let map = MemoryMap::new(len)?;
        Ok(MemorySource {
            map,
            persistent: false,
        })
    }
}

impl<'a> Source for MemorySource<'a> {
    fn is_byte_addressable(&self) -> bool {
        true
    }
    fn is_persistent(&self) -> bool {
        self.persistent
    }
    fn perf_level(&self) -> usize {
        100
    }

    fn close(&mut self) {}

    fn length(&self) -> Result<usize> {
        Ok(self.map.len() as usize)
    }

    fn read(&mut self, offset: usize, dst: &mut [u8]) -> Result<()> {
        let len = dst.len();
        let src = self.map.at(offset, len).ok_or(Error::InvalidMemory {})?;

        dst.copy_from_slice(src);

        Ok(())
    }

    fn write(&mut self, offset: usize, src: &[u8]) -> Result<()> {
        let len = src.len();
        let dst = self
            .map
            .at_mut(offset, len)
            .ok_or(Error::InvalidMemory {})?;

        dst.copy_from_slice(src);

        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    fn at(&self, offset: usize, len: usize) -> Result<&[u8]> {
        self.map.at(offset, len).ok_or(Error::InvalidMemory {})
    }

    fn at_mut(&mut self, offset: usize, len: usize) -> Result<&mut [u8]> {
        self.map.at_mut(offset, len).ok_or(Error::InvalidMemory {})
    }

    fn offset(&mut self, ptr: *const u8) -> Result<usize> {
        let off = self.map.offset(ptr);
        if off > 0 {
            Ok(off as usize)
        } else {
            Err(Error::InvalidMemory {})
        }
    }

    fn flush_slice(&self, slice: &[u8]) -> Result<()> {
        Ok(())
    }
}
