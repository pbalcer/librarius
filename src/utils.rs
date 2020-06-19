pub mod unsafe_utils {
    use std::{mem, slice};

    /* poor's man zero-copy deserialization & serialization */
    pub fn any_as_slice<'a, T>(anyref: &'a T) -> &'a [u8] {
        unsafe { slice::from_raw_parts((anyref as *const T) as *const u8, mem::size_of::<T>()) }
    }

    pub fn any_from_slice_mut<'a, T>(data: &'a mut [u8]) -> &'a mut T {
        unsafe { mem::transmute(data.as_mut_ptr()) }
    }

    pub fn any_from_slice<'a, T>(data: &'a [u8]) -> &'a T {
        unsafe { mem::transmute(data.as_ptr()) }
    }
}
pub mod math {
    use core::ops::{Add, BitAnd, Not, Sub};

    pub fn align_up<T>(value: T, alignment: T) -> T
    where
        T: BitAnd<Output = T>
            + Not<Output = T>
            + Add<Output = T>
            + Sub<Output = T>
            + Copy
            + Copy
            + From<u8>,
    {
        ((value + alignment) - T::from(1)) & !(alignment - T::from(1)) // ! is a bitwise not (~)
    }

    pub fn align_down<T>(value: T, alignment: T) -> T
    where
        T: BitAnd<Output = T>
            + Not<Output = T>
            + Add<Output = T>
            + Sub<Output = T>
            + Copy
            + Copy
            + From<u8>,
    {
        value & !(alignment - T::from(1))
    }
}

pub trait OptionExt<T, E, F> {
    fn get_or_insert_with_result(&mut self, func: F) -> Result<&mut T, E>
    where
        F: FnMut() -> Result<T, E>;
}

impl<T, E, F> OptionExt<T, E, F> for Option<T> {
    fn get_or_insert_with_result(&mut self, mut f: F) -> Result<&mut T, E>
    where
        F: FnMut() -> Result<T, E>,
    {
        if let None = *self {
            *self = Some(f()?);
        }

        match *self {
            Some(ref mut v) => Ok(v),
            None => unsafe { std::hint::unreachable_unchecked() },
        }
    }
}

pub fn crc<T>(anyref: &T) -> u32 {
    let bytes = unsafe_utils::any_as_slice(anyref);

    let mut hasher = crc32fast::Hasher::new();
    hasher.update(bytes);
    hasher.finalize()
}

pub fn crc_slice(buf: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(buf);
    hasher.finalize()
}
