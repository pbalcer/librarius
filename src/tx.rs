use crate::error::{Error, Result};
use crate::las::{LogicalAddress, LogicalAddressSpace};
use crate::utils::unsafe_utils;
use crate::vos::{
    TransactionalLogAllocator, TransactionalObjectAllocator, UntypedPointer, Version,
    VersionedObjectStore, VersionedReader,
};

struct TransactionWrite<'tx> {
    dst: &'tx UntypedPointer,
    current: LogicalAddress,
    new: UntypedPointer,
}

impl<'tx> TransactionWrite<'tx> {
    pub fn new(dst: &'tx UntypedPointer, current: LogicalAddress, new: UntypedPointer) -> Self {
        TransactionWrite { dst, current, new }
    }

    pub fn perform(&self) -> bool {
        self.dst.compare_and_swap(
            UntypedPointer::new_byte_addressable(self.current),
            self.new.clone(),
        )
    }

    pub fn rollback(&self) {
        let success = self.dst.compare_and_swap(
            self.new.clone(),
            UntypedPointer::new_byte_addressable(self.current),
        );
        assert!(success)
    }
}

struct TransactionRead<'tx> {
    pointer: &'tx UntypedPointer,
}

impl<'tx> TransactionRead<'tx> {
    pub fn new(pointer: &'tx UntypedPointer) -> Self {
        TransactionRead { pointer }
    }
}

pub struct Transaction<'tx, 'data: 'tx> {
    las: &'tx LogicalAddressSpace<'data>,
    vos: &'tx VersionedObjectStore<'data>,

    object_allocator: TransactionalObjectAllocator<'tx>,
    log_allocator: TransactionalLogAllocator<'tx>,
    reader: VersionedReader<'tx, 'data>,
    version: Option<Version>,

    writeset: Vec<TransactionWrite<'tx>>,
    readset: Vec<TransactionRead<'tx>>,
}

impl<'tx, 'data: 'tx> Transaction<'tx, 'data> {
    pub fn new(
        las: &'tx LogicalAddressSpace<'data>,
        vos: &'tx VersionedObjectStore<'data>,
    ) -> Self {
        let object_allocator = vos.new_object_allocator(las.boxed_page_alloc());
        let log_allocator = vos.new_log_allocator(las.boxed_page_alloc());
        let reader = vos.new_versioned_reader(las);

        Transaction {
            vos,
            las,
            object_allocator,
            log_allocator,
            reader,
            version: None,
            writeset: Vec::new(),
            readset: Vec::new(),
        }
    }

    pub fn read(&mut self, pointer: &'tx UntypedPointer, size: usize) -> Result<&'tx [u8]> {
        self.reader.read(pointer, size, false)
    }

    pub fn read_for_write(
        &mut self,
        pointer: &'tx UntypedPointer,
        size: usize,
    ) -> Result<&'tx [u8]> {
        self.readset.push(TransactionRead::new(pointer));
        self.reader.read(pointer, size, true)
    }

    pub fn root(&mut self) -> Result<&'tx UntypedPointer> {
        let root_location = self.las.root_location();
        let ptr = UntypedPointer::new_byte_addressable(root_location.address());
        let data = self.las.read(&root_location)?;

        let root: &UntypedPointer = unsafe_utils::any_from_slice(data);

        Ok(root)
    }

    pub fn write(&mut self, pointer: &'tx UntypedPointer, size: usize) -> Result<&'tx mut [u8]> {
        let read_pointer = pointer.clone();
        let address = read_pointer.address();

        let version = self.write_version()?;

        let src = self.reader.read(&read_pointer, size, true)?;
        let (dstptr, dst) = self.object_allocator.alloc(size, version, read_pointer)?;

        dst.copy_from_slice(src);

        let write = TransactionWrite::new(pointer, address, dstptr);

        if !write.perform() {
            Err(Error::TxAborted {})
        } else {
            self.writeset.push(write);

            Ok(dst)
        }
    }

    fn write_version(&mut self) -> Result<Version> {
        if self.version.is_some() {
            Ok(self.version.clone().unwrap())
        } else {
            self.version = Some(self.log_allocator.new_indirect_version()?);
            self.write_version()
        }
    }

    pub fn alloc(&mut self, size: usize) -> Result<(UntypedPointer, &'tx mut [u8])> {
        let version = self.write_version()?;
        self.object_allocator.alloc_new(size, version)
    }

    pub fn set(&mut self, owner: &UntypedPointer, offset: usize, src: &'tx [u8]) -> Result<()> {
        todo!()
    }

    pub fn abort(&mut self) {
        for w in &self.writeset {
            w.rollback();
        }
    }

    pub fn commit(&mut self) -> Result<()> {
        if let Some(version) = &self.version {
            if self
                .vos
                .commit_version(version, self.las, || {
                    for read in &self.readset {
                        self.reader.read(read.pointer, 0, true)?;
                    }
                    Ok(())
                })
                .is_err()
            {
                println!("validate failed");
                self.abort();
                Err(Error::TxAborted {})
            } else {
                Ok(())
            }
        } else {
            Ok(())
        }
    }
}
