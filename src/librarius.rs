use crate::error::{Error, Result};
use crate::las::LogicalAddressSpace;
use crate::source::Source;
use crate::tx::Transaction;
use crate::utils::unsafe_utils;
use crate::vos::{UntypedPointer, Version, VersionedObjectStore};

pub struct LibrariusBuilder<'data, 'root> {
    sources: Vec<Box<dyn Source + 'data>>,
    pagesize: usize,
    root: Option<(usize, Box<dyn Fn(&mut [u8]) -> Result<()> + 'root>)>,
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
        root_size: usize,
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
}

impl<'data> Librarius<'data> {
    pub fn new<F>(
        pagesize: usize,
        sources: impl Iterator<Item = Box<dyn Source + 'data>>,
        root: Option<(usize, F)>,
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
        let mut librarius = Librarius { las, vos };

        if let Some((root_size, root_constr)) = root {
            librarius.root_alloc(root_size, root_constr)?;
        }

        Ok(librarius)
    }

    fn root_alloc<F>(&mut self, size: usize, f: F) -> Result<()>
    where
        F: Fn(&mut [u8]) -> Result<()>,
    {
        let root_location = self.las.root_location();
        let ptr_location = UntypedPointer::new_byte_addressable(root_location.address());

        let data = self.las.read(&root_location)?;
        let ptr_root: &UntypedPointer = unsafe_utils::any_from_slice(data);
        if !ptr_root.is_some() {
            let mut allocator = self.vos.new_object_allocator(self.las.boxed_page_alloc());
            let (root, data) = allocator.alloc_new(size, Version::new_base())?;

            f(data)?;

            ptr_root.compare_and_swap(UntypedPointer::new_none(), root);
        }

        Ok(())
    }

    pub fn run<R, TX>(&self, func: TX) -> Result<R>
    where
        TX: FnOnce(&mut Transaction) -> Result<R>,
    {
        let mut tx = Transaction::new(&self.las, &self.vos);
        let result = func(&mut tx);

        match result {
            Ok(_) => tx.commit()?,
            Err(_) => tx.abort(),
        }

        result
    }

    pub fn run_repeatedly<R, TX>(&self, transaction: TX) -> Result<R>
    where
        TX: Fn(&mut Transaction) -> Result<R>,
    {
        loop {
            match self.run(&transaction) {
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
    use std::sync::Arc;

    #[test]
    fn counter() -> Result<()> {
        let root_size = std::mem::size_of::<usize>();

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
                let result = lr.run_repeatedly(|tx| {
                    let root = tx.root()?;

                    let rootp = tx.write(root, root_size)?;
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
            let root = tx.root()?;

            let rootp = tx.read(root, root_size)?;
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
    impl Persistent for Tuple {}
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

    impl Persistent for Root {}

    #[test]
    fn switcharoo() -> Result<()> {
        let librarius = LibrariusBuilder::new()
            .create_with_typed(|| Root::new())
            .source(MemorySource::new(1 << 20)?)
            .open()?;
        let nthreads = 10;

        librarius.run_repeatedly(|tx| {
            let root = tx.root_typed::<Root>()?;
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
                let result = lr.run_repeatedly(|tx| {
                    let root = tx.root_typed::<Root>()?;

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
            let root = tx.root_typed::<Root>()?;
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
