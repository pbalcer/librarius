use crate::error::{Error, Result};
use std::{fs, io::{prelude::*, SeekFrom}};
use crate::source::Source;

pub struct FileSource {
    file: std::fs::File,
}

impl FileSource {
    pub fn new(path: &str, len: usize) -> Result<Self> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|err| Error::SourceError { })?;

        file.set_len(len as u64)
            .map_err(|err| Error::FileIO { err })?;

        Ok(FileSource { file })
    }
}

impl Source for FileSource {
    fn is_byte_addressable(&self) -> bool {
        false
    }

    fn close(&mut self) {}

    fn read(&mut self, offset: usize, data: &mut [u8]) -> Result<()> {
        self.file
            .seek(SeekFrom::Start(offset as u64))
            .map_err(|err| Error::FileIO { err })?;
        let n = self.file.read(data).map_err(|err| Error::FileIO { err })?;
        if n == data.len() {
            Ok(())
        } else {
            Err(Error::PartialIO {})
        }
    }

    fn write(&mut self, offset: usize, data: &[u8]) -> Result<()> {
        self.file
            .seek(SeekFrom::Start(offset as u64))
            .map_err(|err| Error::FileIO { err })?;
        let n = self.file.write(data).map_err(|err| Error::FileIO { err })?;
        if n == data.len() {
            Ok(())
        } else {
            Err(Error::PartialIO {})
        }
    }

    fn flush(&mut self) -> Result<()> {
        self.file.flush().map_err(|err| Error::FileIO { err })
    }

    fn offset(&mut self, _ptr: *const u8) -> Result<usize> {
        Err(Error::NotByteAddressable {})
    }

    fn flush_slice(&self, _slice: &[u8]) -> Result<()> {
        Err(Error::NotByteAddressable {})
    }

    fn length(&self) -> Result<usize> {
        let m = self.file.metadata().map_err(|err| Error::FileIO { err })?;
        Ok(m.len() as usize)
    }

    fn perf_level(&self) -> usize {
        0
    }

    fn is_persistent(&self) -> bool {
        true
    }
    fn at(&self, _offset: usize, _len: usize) -> Result<&[u8]> {
        Err(Error::NotByteAddressable {})
    }
    fn at_mut(&mut self, _offset: usize, _len: usize) -> Result<&mut [u8]> {
        Err(Error::NotByteAddressable {})
    }
}
