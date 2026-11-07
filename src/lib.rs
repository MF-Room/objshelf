#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod api;
pub mod backend;
mod memory;

use core::marker::PhantomData;

use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub use api::{
    TierMem, flush, get, get_or_insert_with, insert, list, remove, usage, with, with_mut,
};
/// The lock trait of RTIC 2 (`rtic::Mutex`): the functions of this crate take anything that
/// implements it. Implement it to share a `TierMem` without RTIC.
pub use rtic_core::Mutex;

/// Identifier of an object stored in a [`backend::Backend`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ObjectId(u32);

/// Types that can be kept in a [`TierMem`]: serialized when they go to the backend. `Send`, as
/// RTIC shares resources between tasks of different priorities.
pub trait Storable: Serialize + DeserializeOwned + Send + 'static {}

impl<T: Serialize + DeserializeOwned + Send + 'static> Storable for T {}

/// Typed handle to an object of type `T`, obtained from [`get`] or [`insert`].
///
/// A handle is a plain identifier: copying it is free, and it can be stored anywhere (statics, other
/// tasks). The object itself is only reached through the `with*` functions, such as [`with`].
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
    /// The object is cached with another type than the one of the handle.
    WrongType,
    /// The changes not flushed yet fill the RAM budget: nothing was changed. The callback given to
    /// [`TierMem::mount`] was called to start a flush; try again once it is done.
    Full,
    /// A flush has the storage, and the call needs it: the object is not in RAM, or is the one
    /// being written. Nothing was done; try again once the flush is done.
    Busy,
    /// The backend failed.
    Backend(E),
}
