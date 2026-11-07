//! The public API: [`TierMem`], and [`TierMemExt`], its methods on anything that locks one. The
//! engine behind it is in `memory`.

use alloc::string::String;
use alloc::vec::Vec;

use rtic_core::Mutex;

use crate::backend::{Backend, Usage};
use crate::memory::{Core, Locked};
use crate::{Error, Handle, Storable};

/// Object memory on top of a storage [`Backend`], shared between RTIC tasks as a `#[shared]`
/// resource.
///
/// Objects have a name and a type. [`list`](TierMemExt::list) returns the names,
/// [`get`](TierMemExt::get), [`insert`](TierMemExt::insert) and
/// [`get_or_insert_with`](TierMemExt::get_or_insert_with) return a typed [`Handle`], and every
/// access to an object goes through a closure given to one of the `with*` methods, so no reference
/// to it escapes.
///
/// The methods are those of [`TierMemExt`], on RTIC's resource proxy (`cx.shared.mem`). An owned
/// `TierMem` is its own lock, so they also work on it directly, as in `init` before it is shared.
///
/// ```ignore
/// use tiermem::TierMemExt;
///
/// // in `init`: when the changes fill the RAM budget, the `save` task is spawned to flush them
/// let config = NorConfig { flash, range };
/// let mut mem = TierMem::<NorBackend<Flash>>::mount(config, 16 * 1024, || {
///     save::spawn().ok();
/// })?;
/// let settings: Handle<Settings> = mem.get_or_insert_with("settings", Settings::default)?;
///
/// // in a task with `shared = [mem]`
/// let bpm = cx.shared.mem.with(&settings, |s| s.bpm)?; // reads the object if it is not in RAM
/// cx.shared.mem.with_mut(&settings, |s| s.bpm = 120)?; // in RAM until the next flush
/// let bpm = cx.shared.mem.try_with(&settings, |s| s.bpm)?.unwrap_or(bpm); // never waits
/// if cx.shared.mem.try_with_mut(&settings, |s| s.bpm = 120)?.is_none() { /* try again later */ }
///
/// // in the `save` task, of the lowest priority
/// cx.shared.mem.flush()?; // the changes go to the backend
/// ```
///
/// # Writes
///
/// Only [`flush`](TierMemExt::flush) (and [`unmount`](Self::unmount)) writes to the backend.
/// Changes, [`insert`](TierMemExt::insert) and [`remove`](TierMemExt::remove) included, stay in
/// RAM until then. A flush runs in the caller's task: a few ms per object, and hundreds of ms when
/// NOR garbage collection runs (a sector erase is up to 400 ms on the W25Q16JV). Call it from a
/// task of the lowest priority, so that every other task keeps running meanwhile.
///
/// # Locking
///
/// Each method takes the RTIC lock of the resource for short steps only. Like every RTIC lock, it
/// blocks the tasks that share the memory and none of the others: tasks of a higher priority that
/// don't use it, such as a MIDI input interrupt, always keep running. The lock is held for:
///
/// - RAM work: names, changes, and the access to an object in RAM, with the closure given to the
///   `with*` method (keep closures short);
/// - the read of an object that is not in RAM: about 85 µs per 256-byte page on SPI NOR at
///   25 MHz.
///
/// The writes of a flush run outside of it. Meanwhile the storage is busy.
///
/// # Busy storage
///
/// Only the methods that read an object not in RAM need the storage. While a flush has it:
///
/// - [`try_with`](TierMemExt::try_with) and [`try_with_mut`](TierMemExt::try_with_mut) return
///   `Ok(None)` for an object that is not in RAM, or is the one being written (it is out of RAM
///   during its write). Objects in RAM keep working.
/// - [`with`](TierMemExt::with) and [`with_mut`](TierMemExt::with_mut) wait. On a single core,
///   that only happens when they preempt the flush, which cannot finish then: call `with`,
///   `with_mut` and `flush` from one priority level only (one task, or tasks of the same
///   priority).
///
/// The other methods never wait: call them from anywhere.
///
/// # RAM budget
///
/// Objects in RAM take about the `cache_bytes` given to [`mount`](Self::mount). Each one counts
/// its size plus its serialized size, an estimate of what it owns on the heap.
///
/// - Over budget, the least recently used unmodified objects are dropped, with no I/O: they are
///   read again when needed. The object just used stays, even alone over budget.
/// - Modified objects stay until a flush. When an object would become modified and they would
///   then take more than `cache_bytes`, [`with_mut`](TierMemExt::with_mut),
///   [`try_with_mut`](TierMemExt::try_with_mut) and [`insert`](TierMemExt::insert) call the
///   `on_full` callback given to [`mount`](Self::mount), to start a flush, and return
///   [`Error::Full`] without changing anything. An object already modified can still change, and
///   grow over the budget.
///
/// # Durability
///
/// Changes reach the backend on [`flush`](TierMemExt::flush) or [`unmount`](Self::unmount): a
/// reset loses the rest. A power cut during a flush leaves each object either as it was or as
/// written, and the next mount cleans up the names and objects the flush left half done.
pub struct TierMem<B: Backend> {
    pub(crate) core: Core<B>,
}

impl<B: Backend> TierMem<B> {
    /// Mounts the backend and reads the name directory, blocking until done. Objects in RAM take
    /// about `cache_bytes`, and `on_full` is called when the changes not flushed yet fill them
    /// (see [RAM budget](Self#ram-budget)). It runs inside the lock: keep it short, as spawning
    /// the task that flushes. `|| {}` does nothing.
    ///
    /// Meant for RTIC's `init`. The NOR backend reads the whole region (about 0.7 s for 2 MiB of
    /// SPI NOR at 25 MHz).
    pub fn mount(
        config: B::Config,
        cache_bytes: usize,
        on_full: fn(),
    ) -> Result<Self, Error<B::Error>> {
        Core::mount(config, cache_bytes, on_full).map(|core| Self { core })
    }

    /// Flushes, then unmounts the backend.
    pub fn unmount(mut self) -> Result<(), Error<B::Error>> {
        self.flush()?;
        self.core.unmount()
    }
}

/// With exclusive access, nothing else can reach the memory: locking it is free.
impl<B: Backend> Mutex for TierMem<B> {
    type T = Self;

    fn lock<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        f(self)
    }
}

/// The methods of a [`TierMem`], on anything that locks one: an RTIC shared resource
/// (`cx.shared.mem`), or a `TierMem` itself. See [`TierMem`] for the locking, busy storage, RAM
/// budget and durability.
pub trait TierMemExt {
    /// Errors of the backend.
    type Error;

    /// Names of all the stored objects, in no particular order. Never waits.
    fn list(&mut self) -> Result<Vec<String>, Error<Self::Error>>;

    /// Handle to the object `name`. Never waits.
    ///
    /// `T` must be the type the object was inserted with. Returns [`Error::NotFound`] if there is
    /// no object with this name.
    fn get<T: Storable>(&mut self, name: &str) -> Result<Handle<T>, Error<Self::Error>>;

    /// Inserts a new object `name` and returns its handle. Never waits: the object is in RAM,
    /// and reaches the backend at the next [`flush`](Self::flush).
    ///
    /// Returns [`Error::AlreadyExists`] if an object with this name exists, and [`Error::Full`]
    /// if the changes not flushed yet leave no room for it (see
    /// [RAM budget](TierMem#ram-budget)).
    fn insert<T: Storable>(
        &mut self,
        name: &str,
        value: T,
    ) -> Result<Handle<T>, Error<Self::Error>>;

    /// Handle to the object `name`. If there is no object with this name (as on the first boot),
    /// inserts the value returned by `f` first; `f` only runs in that case. Never waits.
    ///
    /// `T` must be the type the object was inserted with.
    fn get_or_insert_with<T: Storable>(
        &mut self,
        name: &str,
        f: impl FnOnce() -> T,
    ) -> Result<Handle<T>, Error<Self::Error>>;

    /// Removes the object of `handle`. Never waits: the removal reaches the backend at the next
    /// [`flush`](Self::flush).
    ///
    /// Returns [`Error::NotFound`] if it is already removed.
    fn remove<T: Storable>(&mut self, handle: Handle<T>) -> Result<(), Error<Self::Error>>;

    /// Runs `f` on the object of `handle` and returns its result. Reads the object if it is not
    /// in RAM, waiting if a flush has the storage (see [Busy storage](TierMem#busy-storage)).
    fn with<T: Storable, R>(
        &mut self,
        handle: &Handle<T>,
        f: impl FnOnce(&T) -> R,
    ) -> Result<R, Error<Self::Error>>;

    /// Runs `f` on the object of `handle` with mutable access and returns its result. Reads the
    /// object if it is not in RAM, waiting if a flush has the storage.
    ///
    /// The change is in RAM when this returns, and reaches the backend at the next
    /// [`flush`](Self::flush). Returns [`Error::Full`] without running `f` if the changes not
    /// flushed yet leave no room for this object (see [RAM budget](TierMem#ram-budget)).
    fn with_mut<T: Storable, R>(
        &mut self,
        handle: &Handle<T>,
        f: impl FnOnce(&mut T) -> R,
    ) -> Result<R, Error<Self::Error>>;

    /// Like [`with`](Self::with), but never waits: returns `Ok(None)` without running `f` if the
    /// object is not in RAM and a flush has the storage. Reads the object if it is not in RAM and
    /// the storage is free.
    ///
    /// Real errors still come as `Err`, so `?` and the `Option` methods replace a `match`:
    /// `mem.try_with(&h, |s| s.bpm)?.unwrap_or(last_bpm)`.
    fn try_with<T: Storable, R>(
        &mut self,
        handle: &Handle<T>,
        f: impl FnOnce(&T) -> R,
    ) -> Result<Option<R>, Error<Self::Error>>;

    /// Like [`with_mut`](Self::with_mut), but never waits: returns `Ok(None)` without running `f`
    /// (so without changing the object) if the object is not in RAM and a flush has the storage.
    /// Reads the object if it is not in RAM and the storage is free.
    fn try_with_mut<T: Storable, R>(
        &mut self,
        handle: &Handle<T>,
        f: impl FnOnce(&mut T) -> R,
    ) -> Result<Option<R>, Error<Self::Error>>;

    /// How the backend's space is used (see [`Usage`]), as of the last storage operation. Never
    /// waits.
    ///
    /// The changes not flushed yet are not counted: modified objects count with their stored
    /// size, and new ones not at all.
    fn usage(&mut self) -> Result<Usage, Error<Self::Error>>;

    /// Writes the changes to the backend: the modified and new objects, then the names, then the
    /// removals. Waits if another flush has the storage.
    ///
    /// Each object modified when it starts is written once, so it ends even if an interrupt keeps
    /// modifying objects. It stops at the first error: what is not written stays in RAM for the
    /// next flush.
    fn flush(&mut self) -> Result<(), Error<Self::Error>>;
}

impl<B: Backend, M: Mutex<T = TierMem<B>>> TierMemExt for M {
    type Error = B::Error;

    fn list(&mut self) -> Result<Vec<String>, Error<B::Error>> {
        Ok(Locked(self).list())
    }

    fn get<T: Storable>(&mut self, name: &str) -> Result<Handle<T>, Error<B::Error>> {
        let id = Locked(self).get(name).ok_or(Error::NotFound)?;
        Ok(Handle::new(id))
    }

    fn insert<T: Storable>(&mut self, name: &str, value: T) -> Result<Handle<T>, Error<B::Error>> {
        Locked(self).insert(name, value).map(Handle::new)
    }

    fn get_or_insert_with<T: Storable>(
        &mut self,
        name: &str,
        f: impl FnOnce() -> T,
    ) -> Result<Handle<T>, Error<B::Error>> {
        match self.get(name) {
            Err(Error::NotFound) => match self.insert(name, f()) {
                // Inserted by another task meanwhile.
                Err(Error::AlreadyExists) => self.get(name),
                result => result,
            },
            result => result,
        }
    }

    fn remove<T: Storable>(&mut self, handle: Handle<T>) -> Result<(), Error<B::Error>> {
        Locked(self).remove(handle.id)
    }

    fn with<T: Storable, R>(
        &mut self,
        handle: &Handle<T>,
        f: impl FnOnce(&T) -> R,
    ) -> Result<R, Error<B::Error>> {
        Locked(self).with(handle.id, false, |value: &mut T| f(value))
    }

    fn with_mut<T: Storable, R>(
        &mut self,
        handle: &Handle<T>,
        f: impl FnOnce(&mut T) -> R,
    ) -> Result<R, Error<B::Error>> {
        Locked(self).with(handle.id, true, f)
    }

    fn try_with<T: Storable, R>(
        &mut self,
        handle: &Handle<T>,
        f: impl FnOnce(&T) -> R,
    ) -> Result<Option<R>, Error<B::Error>> {
        Locked(self).try_with(handle.id, false, |value: &mut T| f(value))
    }

    fn try_with_mut<T: Storable, R>(
        &mut self,
        handle: &Handle<T>,
        f: impl FnOnce(&mut T) -> R,
    ) -> Result<Option<R>, Error<B::Error>> {
        Locked(self).try_with(handle.id, true, f)
    }

    fn usage(&mut self) -> Result<Usage, Error<B::Error>> {
        Ok(Locked(self).usage())
    }

    fn flush(&mut self) -> Result<(), Error<B::Error>> {
        Locked(self).flush()
    }
}
