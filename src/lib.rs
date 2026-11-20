#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod api;
pub mod backend;
mod memory;

use core::marker::PhantomData;

use get_size2::GetSize;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub use api::{
    ObjShelf, flush, flush_object, get, get_or_insert_with, insert, list, remove, usage, wear,
    with, with_mut,
};
/// The lock trait of RTIC 2 (`rtic::Mutex`): the functions of this crate take anything that
/// implements it. Implement it to share an `ObjShelf` without RTIC.
pub use rtic_core::Mutex;

/// Longest name of an object, in bytes of UTF-8. A name is written again with each write of its
/// object, and this keeps it in the first page of the object on NOR flash, so mount reads one page
/// per object to find the names.
pub const MAX_NAME_LEN: usize = 64;

/// Identifier of an object stored in a [`backend::Backend`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ObjectId(u32);

/// Types that can be kept in an [`ObjShelf`]: serialized when they go to the backend, and measured
/// in RAM with [`GetSize`] from the `get-size2` crate (`#[derive(GetSize)]`). `Send`, as RTIC
/// shares resources between tasks of different priorities.
pub trait Storable: Serialize + DeserializeOwned + GetSize + Send + 'static {}

impl<T: Serialize + DeserializeOwned + GetSize + Send + 'static> Storable for T {}

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

/// Errors of [`ObjShelf`], generic over the backend's error `E`.
#[derive(Debug)]
pub enum Error<E> {
    /// No object with this name, or the object of this handle was removed.
    NotFound,
    /// An object with this name already exists.
    AlreadyExists,
    /// The name is longer than [`MAX_NAME_LEN`] bytes.
    NameTooLong,
    /// The object was inserted with another type than the one of the handle. Checked in RAM, and
    /// by the backend with the [`type_tag`](backend::type_tag) stored with the object.
    WrongType,
    /// The object alone takes more RAM than the whole budget, so it can never be inserted or
    /// become modified: no flush can make room for it.
    TooBig,
    /// The backend failed.
    Backend(E),
}
