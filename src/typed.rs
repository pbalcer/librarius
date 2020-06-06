use crate::utils::unsafe_utils;
use crate::vos::UntypedPointer;
use crate::Librarius;
use crate::Result;
use crate::Transaction;
use std::marker::PhantomData;
use std::mem::size_of;

pub trait Persistent {}

impl Persistent for UntypedPointer {}

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

pub trait TypedLibrarius {
    fn root_typed_alloc_if_none<T: Persistent, F>(&mut self, f: F) -> Result<()>
    where
        F: Fn() -> T;
}

impl<'data> TypedLibrarius for Librarius<'data> {
    fn root_typed_alloc_if_none<T: Persistent, F>(&mut self, f: F) -> Result<()>
    where
        F: Fn() -> T,
    {
        self.root_alloc_if_none(size_of::<T>(), |data| {
            let typed = unsafe_utils::any_from_slice_mut(data);
            *typed = f();
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
    fn root_typed<T: Persistent>(&mut self) -> Result<&'tx PersistentPointer<T>>;
    fn alloc_typed<T: Persistent, F>(&mut self, f: F) -> Result<PersistentPointer<T>>
    where
        F: Fn() -> T;
}

impl<'tx, 'data> TypedTransaction<'tx> for Transaction<'tx, 'data> {
    fn write_typed<T: Persistent>(
        &mut self,
        pointer: &'tx PersistentPointer<T>,
    ) -> Result<&'tx mut T> {
        let data = self.write(pointer.as_raw(), size_of::<T>())?;
        Ok(unsafe_utils::any_from_slice_mut(data))
    }

    fn read_typed<T: Persistent>(&mut self, pointer: &'tx PersistentPointer<T>) -> Result<&'tx T> {
        let data = self.read(pointer.as_raw(), size_of::<T>())?;
        Ok(unsafe_utils::any_from_slice(data))
    }

    fn root_typed<T: Persistent>(&mut self) -> Result<&'tx PersistentPointer<T>> {
        let raw = self.root()?;
        Ok(PersistentPointer::from_raw_ref(raw))
    }

    fn alloc_typed<T: Persistent, F>(&mut self, f: F) -> Result<PersistentPointer<T>>
    where
        F: Fn() -> T,
    {
        let (raw, data) = self.alloc(size_of::<T>())?;

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
