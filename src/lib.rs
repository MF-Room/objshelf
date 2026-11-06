#![cfg_attr(not(test), no_std)]

extern crate alloc;
// Lets `#[storable]`, which writes `::tiermem::..` paths, also work inside this crate.
extern crate self as tiermem;

pub mod backend;
mod memory;

use core::marker::PhantomData;

use get_size2::GetSize;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub use memory::{Runner, TierMem, flush, mounted, unmount, with, with_mut};
/// Lock of a resource shared between tasks: RTIC's `Mutex` (re-exported by `rtic` 2), which
/// the async functions of this crate take.
pub use rtic_core::Mutex;
pub use tiermem_macros::storable;

/// Paths used by the code `#[storable]` generates. Not public API.
#[doc(hidden)]
pub mod __private {
    pub use get_size2;
    pub use serde;
}

/// Identifier of an object stored in a [`backend::Backend`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ObjectId(u32);

/// Types that can be kept in a [`TierMem`]: serialized when they go to the backend, and sized
/// with [`GetSize`] to count the RAM they take in the cache. [`storable`] implements all of it.
pub trait Storable: Serialize + DeserializeOwned + GetSize + Send + 'static {}

impl<T: Serialize + DeserializeOwned + GetSize + Send + 'static> Storable for T {}

/// Typed handle to an object of type `T`, obtained from [`TierMem::get`] or [`TierMem::insert`].
///
/// A handle is a plain identifier: copying it is free, and it can be stored anywhere (statics, other
/// tasks). The object itself is only reached through the `with*` functions.
pub struct Handle<T> {
    pub(crate) id: ObjectId,
    // `fn() -> T`: the handle owns no `T`, so it is `Send` and `Sync` whatever `T` is.
    _marker: PhantomData<fn() -> T>,
}

impl<T> Handle<T> {
    pub(crate) fn new(id: ObjectId) -> Self {
        Self {
            id,
            _marker: PhantomData,
        }
    }
}

impl<T> Clone for Handle<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Handle<T> {}

/// Errors of [`TierMem`], generic over the backend's error `E`.
#[derive(Debug)]
pub enum Error<E> {
    /// No object with this name, or the object of this handle was removed.
    NotFound,
    /// An object with this name already exists.
    AlreadyExists,
    /// The object is larger than the whole RAM budget of the cache.
    TooLarge,
    /// The object is cached with another type than the one of the handle.
    WrongType,
    /// The memory is not mounted: mount not finished or failed, or unmounted.
    Unavailable,
    /// The backend failed.
    Backend(E),
}
