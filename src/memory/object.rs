//! Cached objects with their type erased, so that one cache holds objects of any `Storable` type.

use core::any::Any;
use core::mem;

use crate::backend::Backend;
use crate::{ObjectId, Storable};

/// A cached object, its type erased.
pub(super) trait Cached<B: Backend>: Any + Send {
    fn write(&self, backend: &mut B, id: ObjectId) -> Result<(), B::Error>;
}

impl<B: Backend, T: Storable> Cached<B> for T {
    fn write(&self, backend: &mut B, id: ObjectId) -> Result<(), B::Error> {
        backend.write(id, self)
    }
}

/// RAM counted for `value` in the cache: its own size, plus its serialized size as an estimate
/// of what it owns on the heap.
pub(super) fn size<T: Storable>(value: &T) -> usize {
    // A value that cannot be serialized cannot be written either: the write reports the error.
    mem::size_of::<T>() + postcard::experimental::serialized_size(value).unwrap_or(0)
}
