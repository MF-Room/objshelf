#![no_std]

pub mod backend;

use serde::{Deserialize, Serialize};

/// Identifier of an object stored in a [`backend::Backend`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ObjectId(pub u32);
