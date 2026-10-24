//! Storage backends: where objects live when they are not in RAM.

mod nor;

use serde::{Serialize, de::DeserializeOwned};

use crate::ObjectId;

/// Persistent store of serialized objects, addressed by `ObjectId`.
///
/// One operation runs at a time (`&mut self`). Storage futures must not be cancelled: a dropped
/// operation can leave the backend's RAM state out of sync with the storage.
pub(crate) trait Backend: Sized {
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

    /// Closes the storage. Nothing needs saving: writes are already durable.
    async fn unmount(self) -> Result<(), Self::Error> {
        Ok(())
    }
}
