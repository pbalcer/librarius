use crate::utils::unsafe_utils;
use crate::vos::{ObjectSize, UntypedPointer};
use crate::Result;
use crate::Transaction;
use crate::{Librarius, LibrariusBuilder};
use std::marker::PhantomData;
use std::mem::size_of;

pub trait Persistent {
    fn size() -> ObjectSize;
}

impl Persistent for UntypedPointer {
    fn size() -> ObjectSize {
        ObjectSize::new_with_usize(size_of::<UntypedPointer>(), 0)
    }
}

pub struct PersistentPointer<T: Persistent> {
    raw: UntypedPointer,
    phantom: PhantomData<T>,
}

impl<T: Persistent> PersistentPointer<T> {
    fn from_raw(raw: UntypedPointer) -> Self {
        PersistentPointer {
            raw,
            phantom: PhantomData,
        }
    }

    fn from_raw_ref(raw: &UntypedPointer) -> &Self {
        unsafe { std::mem::transmute(raw) }
    }

    fn as_raw(&self) -> &UntypedPointer {
        unsafe { std::mem::transmute(self) }
    }

    pub fn new_none() -> Self {
        PersistentPointer {
            raw: UntypedPointer::new_none(),
            phantom: PhantomData,
        }
    }
}

pub trait TypedLibrariusBuilder<'root> {
    fn create_with_typed<T: Persistent, TC>(self, f: TC) -> Self
    where
        TC: Fn() -> T + 'root;
}

impl<'data, 'root> TypedLibrariusBuilder<'root> for LibrariusBuilder<'data, 'root> {
    fn create_with_typed<T: Persistent, TC>(self, tc: TC) -> Self
    where
        TC: Fn() -> T + 'root,
    {
        self.create_with(T::size(), move |data| {
            let typed = unsafe_utils::any_from_slice_mut(data);
            *typed = tc();
            Ok(())
        })
    }
}

pub trait TypedTransaction<'tx> {
    fn write_typed<T: Persistent>(
        &mut self,
        pointer: &'tx PersistentPointer<T>,
    ) -> Result<&'tx mut T>;
    fn read_typed<T: Persistent>(&mut self, pointer: &'tx PersistentPointer<T>) -> Result<&'tx T>;
    fn root_typed<T: Persistent>(&mut self) -> &'tx PersistentPointer<T>;
    fn alloc_typed<T: Persistent, F>(&mut self, f: F) -> Result<PersistentPointer<T>>
    where
        F: Fn() -> T;
}

impl<'tx, 'data> TypedTransaction<'tx> for Transaction<'tx, 'data> {
    fn write_typed<T: Persistent>(
        &mut self,
        pointer: &'tx PersistentPointer<T>,
    ) -> Result<&'tx mut T> {
        let data = self.write(pointer.as_raw(), &T::size())?;
        Ok(unsafe_utils::any_from_slice_mut(data))
    }

    fn read_typed<T: Persistent>(&mut self, pointer: &'tx PersistentPointer<T>) -> Result<&'tx T> {
        let data = self.read(pointer.as_raw(), &T::size())?;
        Ok(unsafe_utils::any_from_slice(data))
    }

    fn root_typed<T: Persistent>(&mut self) -> &'tx PersistentPointer<T> {
        let raw = self.root();
        PersistentPointer::from_raw_ref(raw)
    }

    fn alloc_typed<T: Persistent, F>(&mut self, f: F) -> Result<PersistentPointer<T>>
    where
        F: Fn() -> T,
    {
        let (raw, data) = self.alloc(T::size())?;

        let data = unsafe_utils::any_from_slice_mut(data);
        *data = f();

        Ok(PersistentPointer::from_raw(raw))
    }
}

pub fn deserialize<'tx, T: Persistent + 'tx>(data: &'tx [u8]) -> &'tx T {
    unsafe_utils::any_from_slice(data)
}

pub fn serialize<'tx, T: Persistent>(anyref: &'tx T) -> &'tx [u8] {
    unsafe_utils::any_as_slice(anyref)
}
