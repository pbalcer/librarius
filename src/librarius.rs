use crate::error::{Error, Result};
use crate::las::LogicalAddressSpace;
use crate::source::Source;
use crate::tx::Transaction;
use crate::utils::unsafe_utils;
use crate::vos::{ObjectHeader, ObjectSize, UntypedPointer, Version, VersionedObjectStore};

pub struct LibrariusBuilder<'data, 'root> {
    sources: Vec<Box<dyn Source + 'data>>,
    pagesize: usize,
    root: Option<(ObjectSize, Box<dyn Fn(&mut [u8]) -> Result<()> + 'root>)>,
}

impl<'data, 'root> LibrariusBuilder<'data, 'root> {
    pub fn new() -> Self {
        LibrariusBuilder {
            sources: Vec::new(),
            pagesize: 4096,
            root: None,
        }
    }

    pub fn create_with(
        mut self,
        root_size: ObjectSize,
        f: impl Fn(&mut [u8]) -> Result<()> + 'root,
    ) -> Self {
        self.root = Some((root_size, Box::new(f)));
        self
    }

    pub fn source(mut self, source: impl Source + 'data) -> Self {
        self.sources.push(Box::new(source));
        self
    }

    pub fn pagesize(mut self, pagesize: usize) -> Self {
        self.pagesize = pagesize;
        self
    }

    pub fn open(self) -> Result<Librarius<'data>> {
        Librarius::new(self.pagesize, self.sources.into_iter(), self.root)
    }
}

pub struct Librarius<'data> {
    las: LogicalAddressSpace<'data>,
    vos: VersionedObjectStore<'data>,
    root: &'data UntypedPointer,
}

impl<'data> Librarius<'data> {
    pub fn new<F>(
        pagesize: usize,
        sources: impl Iterator<Item = Box<dyn Source + 'data>>,
        root: Option<(ObjectSize, F)>,
    ) -> Result<Librarius<'data>>
    where
        F: Fn(&mut [u8]) -> Result<()>,
    {
        let las = LogicalAddressSpace::new(
            pagesize,
            sources,
            VersionedObjectStore::valid_page,
            root.is_some(),
        )?;
        let vos = VersionedObjectStore::new();

        let root = if let Some((root_size, root_constr)) = root {
            Self::root_alloc(&las, &vos, root_size, root_constr)?
        } else {
            Self::root_read(&las, &vos)?
        };

        Ok(Librarius { las, vos, root })
    }

    fn root_read(
        las: &LogicalAddressSpace<'data>,
        vos: &VersionedObjectStore<'data>,
    ) -> Result<&'data UntypedPointer> {
        let root_location = las.root_location();

        let (_, data) = las
            .read(&root_location)?
            .split_at(std::mem::size_of::<ObjectHeader>());

        let ptr_root: &UntypedPointer = unsafe_utils::any_from_slice(data);

        Ok(ptr_root)
    }

    fn root_alloc<F>(
        las: &LogicalAddressSpace<'data>,
        vos: &VersionedObjectStore<'data>,
        size: ObjectSize,
        f: F,
    ) -> Result<&'data UntypedPointer>
    where
        F: Fn(&mut [u8]) -> Result<()>,
    {
        let mut allocator = vos.new_object_allocator(las.boxed_page_alloc());

        let root_location = las.root_location();
        {
            let root = Self::root_read(las, vos)?;
            if root.is_some() {
                return Ok(root);
            }
        }

        let owning = root_location.0.address() + std::mem::size_of::<ObjectHeader>();

        let ptr_owning = UntypedPointer::new_byte(owning);

        let internal_size = ObjectSize::new(8, 0);
        let data = las.write(&root_location)?;
        let userdata = allocator.init_object(
            data,
            internal_size,
            Version::new_base(),
            UntypedPointer::new_none(),
        );

        let ptr_root: &UntypedPointer = unsafe_utils::any_from_slice(userdata);

        let (root, data) = allocator.alloc_new(size, Version::new_base())?;

        f(data)?;

        let result = ptr_root.compare_and_swap(UntypedPointer::new_none(), root);
        assert!(result);
        let reader = vos.new_versioned_reader(las);
        if let Err(err) = reader.flush(&ptr_owning) {
            println!("flushing {:?}", err);
        }

        Self::root_read(las, vos)
    }

    pub fn run_once<R, TX>(&self, func: TX) -> Result<R>
    where
        TX: FnOnce(&mut Transaction) -> Result<R>,
    {
        let mut tx = Transaction::new(&self.las, &self.vos, self.root);
        let result = func(&mut tx);

        match result {
            Ok(_) => tx.commit()?,
            Err(_) => tx.abort(),
        }

        result
    }

    pub fn run<R, TX>(&self, transaction: TX) -> Result<R>
    where
        TX: Fn(&mut Transaction) -> Result<R>,
    {
        loop {
            match self.run_once(&transaction) {
                Ok(result) => return Ok(result),
                Err(Error::TxAborted {}) => {}
                Err(error) => return Err(error),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::MemorySource;
    use std::mem::size_of;
    use std::sync::Arc;

    struct BasicRoot {
        value: u64,
    }
    impl Persistent for BasicRoot {
        fn size() -> ObjectSize {
            ObjectSize::new_with_usize(0, size_of::<BasicRoot>())
        }
    }

    #[test]
    fn basic() -> Result<()> {
        let librarius = LibrariusBuilder::new()
            .create_with_typed(|| BasicRoot { value: 0 })
            .source(MemorySource::new(1 << 20)?)
            .open()?;

        Ok(())
    }

    #[test]
    fn counter() -> Result<()> {
        let root_size = ObjectSize::new_with_usize(0, std::mem::size_of::<usize>());

        let librarius = LibrariusBuilder::new()
            .create_with(root_size, |data| {
                let counter: &mut usize = unsafe_utils::any_from_slice_mut(data);
                *counter = 0;

                Ok(())
            })
            .source(MemorySource::new(1 << 20)?)
            .open()?;

        let nthreads = 10;

        let librarius = Arc::new(librarius);

        let mut threads = Vec::new();

        for i in 0..nthreads {
            let lr = librarius.clone();
            threads.push(std::thread::spawn(move || {
                let result = lr.run(|tx| {
                    let root = tx.root();

                    let rootp = tx.write(root, &root_size)?;
                    let counter: &mut usize = unsafe_utils::any_from_slice_mut(rootp);

                    *counter += 1;

                    Ok(*counter)
                })?;
                Ok(())
            }));
        }

        for th in threads {
            th.join().unwrap()?;
        }

        librarius.run(|tx| {
            let root = tx.root();

            let rootp = tx.read(root, &root_size)?;
            let counter: &usize = unsafe_utils::any_from_slice(rootp);
            assert_eq!(*counter, nthreads);

            Ok(())
        })?;

        Ok(())
    }

    use crate::typed::{Persistent, PersistentPointer, TypedLibrariusBuilder, TypedTransaction};

    struct Tuple {
        value: bool,
    }
    impl Persistent for Tuple {
        fn size() -> ObjectSize {
            ObjectSize::new_with_usize(0, size_of::<Tuple>())
        }
    }
    impl Tuple {
        fn new(value: bool) -> Self {
            Tuple { value }
        }
    }

    const NTUPLES: usize = 10;
    struct Root {
        arr: [PersistentPointer<Tuple>; NTUPLES],
    }

    impl Root {
        fn new() -> Self {
            Root {
                arr: [
                    PersistentPointer::new_none(),
                    PersistentPointer::new_none(),
                    PersistentPointer::new_none(),
                    PersistentPointer::new_none(),
                    PersistentPointer::new_none(),
                    PersistentPointer::new_none(),
                    PersistentPointer::new_none(),
                    PersistentPointer::new_none(),
                    PersistentPointer::new_none(),
                    PersistentPointer::new_none(),
                ],
            }
        }
    }

    impl Persistent for Root {
        fn size() -> ObjectSize {
            ObjectSize::new_with_usize(size_of::<Root>(), 0)
        }
    }

    #[test]
    fn switcharoo() -> Result<()> {
        let librarius = LibrariusBuilder::new()
            .create_with_typed(|| Root::new())
            .source(MemorySource::new(1 << 20)?)
            .open()?;
        let nthreads = 10;

        librarius.run(|tx| {
            let root = tx.root_typed::<Root>();
            let rootp = tx.write_typed(root)?;

            for n in 0..NTUPLES {
                rootp.arr[n] = tx.alloc_typed(|| Tuple::new(n % 2 == 0))?;
            }

            Ok(())
        })?;

        let librarius = Arc::new(librarius);

        let mut threads = Vec::new();

        for i in 0..nthreads {
            let lr = librarius.clone();
            threads.push(std::thread::spawn(move || {
                let result = lr.run(|tx| {
                    let root = tx.root_typed::<Root>();

                    let rootp = tx.read_typed(root)?;

                    let desired_value = i % 2 == 0;

                    for n in 0..NTUPLES {
                        let t = tx.write_typed(&rootp.arr[n])?;
                        if t.value != desired_value {
                            t.value = desired_value;
                        }
                    }

                    Ok(())
                });
            }));
        }

        for th in threads {
            th.join().unwrap()
        }

        let true_count = librarius.run(|tx| {
            let root = tx.root_typed::<Root>();
            let rootp = tx.read_typed(root)?;

            let mut true_count = 0;
            for n in 0..NTUPLES {
                let t = tx.read_typed(&rootp.arr[n])?;
                true_count += t.value as usize;
            }

            Ok(true_count)
        })?;

        assert!(true_count == NTUPLES || true_count == 0);

        Ok(())
    }
}
