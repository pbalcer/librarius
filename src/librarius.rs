use crate::error::{Error, Result};
use crate::las::LogicalAddressSpace;
use crate::source::Source;
use crate::tx::Transaction;
use crate::utils::unsafe_utils;
use crate::vos::{UntypedPointer, Version, VersionedObjectStore};

pub struct Librarius<'data> {
    las: LogicalAddressSpace<'data>,
    vos: VersionedObjectStore<'data>,
}

impl<'data> Librarius<'data> {
    pub fn new(pagesize: usize) -> Librarius<'data> {
        let las = LogicalAddressSpace::new(pagesize);
        let vos = VersionedObjectStore::new();
        Librarius { las, vos }
    }

    pub fn attach(&mut self, source: impl Source + 'data) -> Result<()> {
        self.las.attach(source)?;
        Ok(())
    }

    pub fn root_alloc_if_none<F>(&mut self, size: usize, f: F) -> Result<()>
    where
        F: Fn(&mut [u8]) -> Result<()>,
    {
        let root_location = self.las.root_location()?;
        let ptr_location = UntypedPointer::new_byte_addressable(root_location.address());

        let data = self.las.read(&root_location)?;
        let ptr_root: &UntypedPointer = unsafe_utils::any_from_slice(data);

        let mut allocator = self.vos.new_object_allocator(self.las.boxed_page_alloc());
        let (root, data) = allocator.alloc_new(size, Version::new_base())?;

        f(data)?;

        ptr_root.compare_and_swap(UntypedPointer::new_none(), root);

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
        let mut librarius = Librarius::new(4096);
        librarius.attach(MemorySource::new(1 << 20)?)?;

        let root_size = std::mem::size_of::<usize>();
        let nthreads = 10;

        librarius.root_alloc_if_none(root_size, |data| {
            let counter: &mut usize = unsafe_utils::any_from_slice_mut(data);
            *counter = 0;

            Ok(())
        })?;

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

    use crate::typed::{Persistent, PersistentPointer, TypedLibrarius, TypedTransaction};

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
        let mut librarius = Librarius::new(4096);
        librarius.attach(MemorySource::new(1 << 30)?)?;

        librarius.root_typed_alloc_if_none(|| Root::new())?;
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
