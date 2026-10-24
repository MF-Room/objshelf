#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod backend;

use core::marker::PhantomData;

use serde::{Deserialize, Serialize};

/// Identifier of an object stored in a `backend::Backend`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(crate) struct ObjectId(u32);

/// Typed handle to an object of type `T`.
pub struct Handle<T> {
    pub(crate) id: ObjectId,
    _marker: PhantomData<T>,
}
