//! Storage backends: where objects live when they are not in RAM.

pub mod nor;

use alloc::string::String;
use alloc::vec::Vec;

use serde::{Serialize, de::DeserializeOwned};

use crate::ObjectId;

/// Persistent store of serialized objects, addressed by `ObjectId`. Each object holds its name,
/// written with it, so the names are stored nowhere else, and the [`type_tag`] of its type.
///
/// One operation runs at a time (`&mut self`), blocking until done.
// `'static` because `ObjShelf` erases the types of the objects it caches, which names the backend.
pub trait Backend: Sized + 'static {
    /// Errors of the backend and of its storage driver.
    type Error;
    /// What `mount` needs: the storage driver and its settings.
    type Config;

    /// Opens the storage and rebuilds the backend's state from what it holds.
    fn mount(config: Self::Config) -> Result<Self, Self::Error>;

    /// Stores `value` under `id`, with its `name` and `type_tag::<T>()`, replacing any previous
    /// value. It is durable when this returns. The name and the value are written at once: an
    /// object is never stored without its name.
    fn write<T: Serialize>(
        &mut self,
        id: ObjectId,
        name: &str,
        value: &T,
    ) -> Result<(), Self::Error>;

    /// Reads the value stored under `id`: `None` if it was written with another type than `T`,
    /// its stored tag not being `type_tag::<T>()`. The tag is checked before the value is
    /// decoded.
    fn read<T: DeserializeOwned>(&mut self, id: ObjectId) -> Result<Option<T>, Self::Error>;

    /// Reads the name stored with the object `id`, without its value.
    fn name(&mut self, id: ObjectId) -> Result<String, Self::Error>;

    /// Removes the object `id`. The removal is durable when this returns.
    fn remove(&mut self, id: ObjectId) -> Result<(), Self::Error>;

    /// How the storage space is used. Computed from the backend's RAM state, without I/O.
    fn usage(&self) -> Usage;

    /// How worn the storage is (see [`Wear`]). Computed from the backend's RAM state, without
    /// I/O. `None`, the default, for storage that doesn't wear out, or whose wear something else
    /// handles (the OS for a file system).
    fn wear(&self) -> Option<Wear> {
        None
    }

    /// Ids of all the stored objects, in no particular order. Computed from the backend's RAM
    /// state, without I/O.
    fn ids(&self) -> Vec<ObjectId>;

    /// Closes the storage. Nothing needs saving: writes are already durable.
    fn unmount(self) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// Tag of the type `T`, stored with each object so that reading it with another type is caught:
/// a 32-bit FNV-1a hash of `core::any::type_name::<T>()`.
///
/// It changes when the type is renamed or moved to another module, and the compiler doesn't
/// promise to keep type names the same across its versions: objects stored before such a change
/// then read as another type. Changes inside the type (its fields) don't change the tag.
pub fn type_tag<T: ?Sized>() -> u32 {
    let name = core::any::type_name::<T>();
    name.bytes().fold(0x811c_9dc5, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    })
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

/// Wear of a storage erased in sectors, each enduring a limited number of erases: NOR flash.
///
/// The endurance is in the chip's datasheet: 100 000 erases per sector for the W25Q16JV. Wear
/// leveling keeps the sectors within about 100 erases of each other, so the storage wears out
/// when `total_erases` nears `sectors × endurance`. The backend has no clock: to estimate the time
/// left, read `total_erases` twice, some time apart.
///
/// ```
/// use objshelf::backend::Wear;
///
/// // Read a day apart.
/// let yesterday = Wear { sectors: 512, most_worn: 120, total_erases: 51_000 };
/// let today = Wear { sectors: 512, most_worn: 121, total_erases: 51_480 };
/// let per_day = today.total_erases - yesterday.total_erases;
/// let days_left = today.remaining_erases(100_000) / per_day;
/// assert_eq!(days_left, 106_559); // about 290 years
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Wear {
    /// Number of sectors.
    pub sectors: u32,
    /// Erases of the most worn sector.
    pub most_worn: u32,
    /// Erases of all the sectors added up. The counts are stored with the sectors, so this keeps
    /// counting across reboots.
    pub total_erases: u64,
}

impl Wear {
    /// Erases left before every sector reaches `endurance`, the erases a sector is rated for.
    pub fn remaining_erases(&self, endurance: u32) -> u64 {
        (u64::from(self.sectors) * u64::from(endurance)).saturating_sub(self.total_erases)
    }
}
