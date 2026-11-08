//! Storage backends: where objects live when they are not in RAM.

pub mod nor;

use alloc::string::String;
use alloc::vec::Vec;

use serde::{Serialize, de::DeserializeOwned};

use crate::ObjectId;

/// Persistent store of serialized objects, addressed by `ObjectId`. Each object holds its name,
/// written with it, so the names are stored nowhere else.
///
/// One operation runs at a time (`&mut self`), blocking until done.
// `'static` because `TierMem` erases the types of the objects it caches, which names the backend.
pub trait Backend: Sized + 'static {
    /// Errors of the backend and of its storage driver.
    type Error;
    /// What `mount` needs: the storage driver and its settings.
    type Config;

    /// Opens the storage and rebuilds the backend's state from what it holds.
    fn mount(config: Self::Config) -> Result<Self, Self::Error>;

    /// Stores `value` under `id`, with its `name`, replacing any previous value. It is durable when
    /// this returns. The name and the value are written at once: an object is never stored without
    /// its name.
    fn write<T: Serialize>(
        &mut self,
        id: ObjectId,
        name: &str,
        value: &T,
    ) -> Result<(), Self::Error>;

    /// Reads the value stored under `id`. `T` must be the type it was written with.
    fn read<T: DeserializeOwned>(&mut self, id: ObjectId) -> Result<T, Self::Error>;

    /// Reads the name stored with the object `id`, without its value.
    fn name(&mut self, id: ObjectId) -> Result<String, Self::Error>;

    /// Removes the object `id`. The removal is durable when this returns.
    fn remove(&mut self, id: ObjectId) -> Result<(), Self::Error>;

    /// How the storage space is used. Computed from the backend's RAM state, without I/O.
    fn usage(&self) -> Usage;

    /// Ids of all the stored objects, in no particular order. Computed from the backend's RAM
    /// state, without I/O.
    fn ids(&self) -> Vec<ObjectId>;

    /// Closes the storage. Nothing needs saving: writes are already durable.
    fn unmount(self) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// Space of a backend, in bytes of serialized objects.
///
/// A backend can store data in fixed-size units (pages on NOR flash), so a small object takes a
/// whole unit: `used` counts whole units, and an object of `n` bytes may need more than `n` of
/// `free`. `used + free + reclaimable` is at most `capacity`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    /// Bytes the backend can hold, without its metadata and reserved space.
    pub capacity: usize,
    /// Bytes taken by the stored objects.
    pub used: usize,
    /// Bytes writable now.
    pub free: usize,
    /// Bytes taken by replaced or removed objects, writable once reclaimed (on NOR flash, by
    /// garbage collection, which erases sectors and can take hundreds of ms).
    pub reclaimable: usize,
    /// Number of stored objects.
    pub objects: usize,
}
