//! The object memory: named, typed objects kept in RAM and stored in a backend.

use alloc::string::String;
use alloc::vec::Vec;
use core::marker::PhantomData;

use crate::backend::Backend;
use crate::{Error, Handle, Storable};

/// Object memory on top of a storage [`Backend`].
///
/// Objects have a name and a type. [`list`](Self::list) returns the names, [`get`](Self::get),
/// [`insert`](Self::insert) and [`get_or_insert_with`](Self::get_or_insert_with) return a typed [`Handle`], and
/// every access to an object goes through a closure given to one of the `with*` methods, so no
/// reference to it escapes.
///
/// ```ignore
/// let mem = TierMem::<NorBackend<Flash>>::mount(NorConfig { flash, range })?;
/// for name in mem.list()? { /* ... */ }
/// let settings: Handle<Settings> = mem.get_or_insert_with("settings", Settings::default)?;
/// let bpm = mem.with(&settings, |s| s.bpm)?; // waits if the storage is busy
/// mem.with_mut(&settings, |s| s.bpm = 120)?;
/// let bpm = mem.try_with(&settings, |s| s.bpm)?.unwrap_or(bpm); // `None` if busy, never waits
/// if mem.try_with_mut(&settings, |s| s.bpm = 120)?.is_none() { /* try again later */ }
/// mem.flush()?; // modified objects go to the backend
/// ```
///
/// # Blocking
///
/// The API is not async: each call runs the backend's futures to completion with `block_on`, so an
/// operation can never be dropped halfway and leave the backend out of sync with its storage. The
/// price is that blocking calls hold the CPU during storage I/O: loading an object takes a few ms,
/// and a write that needs garbage collection on NOR flash takes hundreds of ms (a sector erase is up
/// to 400 ms on the W25Q16JV). Time-critical code uses the `try_*` methods, which return
/// `Ok(None)` instead of waiting while the storage is busy with another operation. They still load
/// an object that is not in RAM.
///
/// # Deadlock
///
/// Blocking methods wait for the storage to be free. On a single core, a blocking call from an
/// interrupt (or a higher-priority executor) while lower-priority code holds the storage waits
/// forever, since that code cannot run to release it. Call blocking methods from one priority level
/// only, and `try_*` methods everywhere else. For the same reason, a closure must not call back into
/// the same `TierMem`: a blocking call deadlocks and a `try_*` call returns `Ok(None)`.
///
/// # Durability
///
/// A change made by [`with_mut`](Self::with_mut) or [`try_with_mut`](Self::try_with_mut) is in RAM
/// when the call returns. It reaches the backend when the object is evicted, or on
/// [`flush`](Self::flush) or [`unmount`](Self::unmount). [`insert`](Self::insert) and
/// [`remove`](Self::remove) are durable when they return.
pub struct TierMem<B: Backend> {
    // TODO(Marius): state. The backend, the name directory (name -> `ObjectId`), the objects
    // resident in RAM (with a modified flag), and the lock: a busy flag (`AtomicBool`) rather than
    // a critical section, which would disable interrupts during storage I/O.
    _backend: PhantomData<B>,
}

impl<B: Backend> TierMem<B> {
    /// Mounts the backend and loads the name directory.
    pub fn mount(config: B::Config) -> Result<Self, Error<B::Error>> {
        let _ = config;
        todo!("Marius: block_on(B::mount(config)), then load the name directory")
    }

    /// Writes every modified object back, then unmounts the backend.
    pub fn unmount(self) -> Result<(), Error<B::Error>> {
        todo!("Marius: flush, then block_on(backend.unmount())")
    }

    /// Names of all the stored objects, in no particular order. Waits if the storage is busy.
    pub fn list(&self) -> Result<Vec<String>, Error<B::Error>> {
        todo!("Marius: names from the directory")
    }

    /// Handle to the object `name`. Waits if the storage is busy.
    ///
    /// `T` must be the type the object was inserted with. Returns [`Error::NotFound`] if there is no
    /// object with this name.
    pub fn get<T: Storable>(&self, name: &str) -> Result<Handle<T>, Error<B::Error>> {
        let _ = name;
        todo!("Marius: look up the name in the directory")
    }

    /// Stores a new object `name` and returns its handle. It is durable when this returns. Waits if
    /// the storage is busy.
    ///
    /// Returns [`Error::AlreadyExists`] if an object with this name exists.
    pub fn insert<T: Storable>(&self, name: &str, value: T) -> Result<Handle<T>, Error<B::Error>> {
        let _ = (name, value);
        todo!("Marius: allocate an ObjectId, write the value and the directory entry")
    }

    /// Handle to the object `name`. If there is no object with this name (as on the first boot),
    /// stores the value returned by `f` first; `f` only runs in that case. Waits if the storage is
    /// busy.
    ///
    /// `T` must be the type the object was inserted with.
    pub fn get_or_insert_with<T: Storable>(
        &self,
        name: &str,
        f: impl FnOnce() -> T,
    ) -> Result<Handle<T>, Error<B::Error>> {
        let _ = (name, f);
        todo!(
            "Marius: get, or insert f() if NotFound, under one lock so no other insert comes between"
        )
    }

    /// Removes the object of `handle`. It is durable when this returns. Waits if the storage is
    /// busy.
    pub fn remove<T: Storable>(&self, handle: Handle<T>) -> Result<(), Error<B::Error>> {
        let _ = handle;
        todo!("Marius: remove the object and its directory entry")
    }

    /// Runs `f` on the object of `handle` and returns its result. Waits if the storage is busy, and
    /// loads the object if it is not in RAM.
    pub fn with<T: Storable, R>(
        &self,
        handle: &Handle<T>,
        f: impl FnOnce(&T) -> R,
    ) -> Result<R, Error<B::Error>> {
        let _ = (handle, f);
        todo!("Marius: lock (wait), load if needed, run f")
    }

    /// Runs `f` on the object of `handle` with mutable access and returns its result. Waits if the
    /// storage is busy, and loads the object if it is not in RAM.
    ///
    /// The change is in RAM when this returns (see [Durability](Self#durability)).
    pub fn with_mut<T: Storable, R>(
        &self,
        handle: &Handle<T>,
        f: impl FnOnce(&mut T) -> R,
    ) -> Result<R, Error<B::Error>> {
        let _ = (handle, f);
        todo!("Marius: lock (wait), load if needed, run f, mark modified")
    }

    /// Like [`with`](Self::with), but never waits for the storage: returns `Ok(None)` without
    /// running `f` if the storage is busy with another operation (for example a write or an erase).
    /// Loads the object if it is not in RAM.
    ///
    /// Real errors still come as `Err`, so `?` and the `Option` methods replace a `match`:
    /// `mem.try_with(&h, |s| s.bpm)?.unwrap_or(last_bpm)`.
    pub fn try_with<T: Storable, R>(
        &self,
        handle: &Handle<T>,
        f: impl FnOnce(&T) -> R,
    ) -> Result<Option<R>, Error<B::Error>> {
        let _ = (handle, f);
        todo!("Marius: try to lock (None if locked), load if needed, run f")
    }

    /// Like [`with_mut`](Self::with_mut), but never waits for the storage: returns `Ok(None)`
    /// without running `f` (so without changing the object) if the storage is busy with another
    /// operation (for example a write or an erase). Loads the object if it is not in RAM.
    pub fn try_with_mut<T: Storable, R>(
        &self,
        handle: &Handle<T>,
        f: impl FnOnce(&mut T) -> R,
    ) -> Result<Option<R>, Error<B::Error>> {
        let _ = (handle, f);
        todo!("Marius: try to lock (None if locked), load if needed, run f, mark modified")
    }

    /// Writes every modified object back to the backend. Waits if the storage is busy.
    pub fn flush(&self) -> Result<(), Error<B::Error>> {
        todo!("Marius: block_on(backend.write(..)) for each modified object")
    }
}
