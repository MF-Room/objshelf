#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod backend;
mod memory;

use core::marker::PhantomData;

use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub use memory::TierMem;

/// Identifier of an object stored in a [`backend::Backend`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ObjectId(u32);

/// Types that can be kept in a [`TierMem`]: serialized when they go to the backend.
pub trait Storable: Serialize + DeserializeOwned + 'static {}

impl<T: Serialize + DeserializeOwned + 'static> Storable for T {}

/// Typed handle to an object of type `T`, obtained from [`TierMem::get`] or [`TierMem::insert`].
///
/// A handle is a plain identifier: copying it is free, and it can be stored anywhere (statics, other
/// tasks). The object itself is only reached through the `with*` methods of [`TierMem`].
pub struct Handle<T> {
    #[expect(dead_code, reason = "read by TierMem once it is implemented")]
    pub(crate) id: ObjectId,
    // `fn() -> T`: the handle owns no `T`, so it is `Send` and `Sync` whatever `T` is.
    _marker: PhantomData<fn() -> T>,
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
    /// The storage is in use, or the object is not in RAM and loading it would need I/O. Only
    /// returned by the `try_*` methods; the others wait instead.
    Busy,
    /// No object with this name.
    NotFound,
    /// An object with this name already exists.
    AlreadyExists,
    /// The backend failed.
    Backend(E),
}
