//! Storage backends: where objects live when they are not in RAM.

pub mod nor;

use serde::{Serialize, de::DeserializeOwned};

use crate::ObjectId;

/// Persistent store of serialized objects, addressed by `ObjectId`.
///
/// One operation runs at a time (`&mut self`). Storage futures must not be cancelled: a dropped
/// operation can leave the backend's RAM state out of sync with the storage. `TierMem` runs each
/// of them to completion with `block_on`, so this only concerns code that awaits a backend directly.
// The futures don't need to be `Send`: `TierMem` polls them in place.
#[allow(async_fn_in_trait)]
pub trait Backend: Sized {
    /// Errors of the backend and of its storage driver.
    type Error;
    /// What `mount` needs: the storage driver and its settings.
    type Config;

    /// Opens the storage and rebuilds the backend's state from what it holds.
    async fn mount(config: Self::Config) -> Result<Self, Self::Error>;

    /// Stores `value` under `id`, replacing any previous value. It is durable when this returns.
    async fn write<T: Serialize>(&mut self, id: ObjectId, value: &T) -> Result<(), Self::Error>;

    /// Reads the value stored under `id`. `T` must be the type it was written with.
    async fn read<T: DeserializeOwned>(&mut self, id: ObjectId) -> Result<T, Self::Error>;

    /// Removes the object `id`. The removal is durable when this returns.
    async fn remove(&mut self, id: ObjectId) -> Result<(), Self::Error>;

    /// How the storage space is used. Computed from the backend's RAM state, without I/O.
    fn usage(&self) -> Usage;

    /// Closes the storage. Nothing needs saving: writes are already durable.
    async fn unmount(self) -> Result<(), Self::Error> {
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
