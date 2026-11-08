//! Cached objects with their type erased, so that one cache holds objects of any `Storable` type.

use core::any::Any;
use core::mem;

use super::cache::Entry;
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

/// RAM counted for `value` in the cache: its own size, what it owns on the heap (`GetSize`,
/// spare capacity included), and its entry in the cache. Only the allocator's own overhead is
/// left out.
pub(super) fn size<B: Backend, T: Storable>(value: &T) -> usize {
    mem::size_of::<(ObjectId, Entry<B>)>() + value.get_size()
}
