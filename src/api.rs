//! The public API: [`TierMem`], and the functions that use it through anything that locks one.
//! The engine behind it is in `memory`.

use alloc::string::String;
use alloc::vec::Vec;

use rtic_core::Mutex;

use crate::backend::{Backend, Usage};
use crate::memory::{Core, Locked};
use crate::{Error, Handle, Storable};

/// Object memory on top of a storage [`Backend`], shared between RTIC tasks as a `#[shared]`
/// resource.
///
/// Objects have a name and a type. [`list`] returns the names, [`get`], [`insert`] and
/// [`get_or_insert_with`] return a typed [`Handle`], and every access to an object goes through a
/// closure given to [`with`] or [`with_mut`], so no reference to it escapes.
///
/// The functions take the memory as `mem`: anything that locks a `TierMem`, such as RTIC's
/// resource proxy (`&mut cx.shared.mem`). An owned `TierMem` is its own lock, so they also take
/// `&mut mem` directly, as in `init` before it is shared.
///
/// ```ignore
/// // in `init`: when the changes fill the RAM budget, the `save` task is spawned to flush them
/// let config = NorConfig { flash, range };
/// let mut mem = TierMem::<NorBackend<Flash>>::mount(config, 16 * 1024, || {
///     save::spawn().ok();
/// })?;
/// let handle: Handle<Settings> =
///     tiermem::get_or_insert_with(&mut mem, "settings", Settings::default)?.expect("fits");
///
/// // in a task with `shared = [mem]`: `None` means "not now, try again later"
/// let mem = &mut cx.shared.mem;
/// if let Some(brightness) = tiermem::with(mem, &handle, |s| s.brightness)? { /* ... */ }
/// if tiermem::with_mut(mem, &handle, |s| s.brightness = 3)?.is_none() { /* try again later */ }
///
/// // in the `save` task, of the lowest priority
/// tiermem::flush(&mut cx.shared.mem)?; // the changes go to the backend
/// ```
///
/// # Writes
///
/// Only [`flush`], [`flush_object`] (which writes one object) and [`unmount`](Self::unmount) write
/// to the backend. Changes, [`insert`] and [`remove`] included, stay in RAM until then. A flush
/// runs in the caller's task: a few ms per object, and hundreds of ms when NOR garbage collection
/// runs (a sector erase can take 400 ms on SPI NOR). Call it from a task of the lowest priority,
/// so that every other task keeps running meanwhile.
///
/// # Locking
///
/// Each function takes the lock for short steps only: never wrap a call in `lock` yourself. Like
/// every RTIC lock, it blocks the tasks that share the memory and none of the others: tasks of a
/// higher priority that don't use it always keep running. The lock is held for:
///
/// - RAM work: names, changes, and the access to an object in RAM, with the closure given to
///   [`with`] or [`with_mut`] (keep closures short);
/// - the read of an object that is not in RAM: about 85 µs per 256-byte page on SPI NOR at
///   25 MHz.
///
/// The writes of a flush run outside of it. Meanwhile the storage is busy.
///
/// # Not now: `Ok(None)`
///
/// No function waits, so any task can call any of them. Instead, [`with`], [`with_mut`],
/// [`insert`], [`get_or_insert_with`], [`flush`] and [`flush_object`] return `Ok(None)`, having
/// done nothing, when the call cannot be done now:
///
/// - Busy: a flush has the storage, and the call needs it. That is [`with`] and [`with_mut`] on an
///   object that is not in RAM, or is the one being written (it is out of RAM during its write),
///   and the flushes themselves. Objects in RAM keep working. Only a task that preempts the
///   flush, of a higher priority, can find the storage busy.
/// - Full: see [RAM budget](#ram-budget).
///
/// Try again later. Real errors still come as `Err`.
///
/// # RAM budget
///
/// Objects in RAM take about the `cache_bytes` given to [`mount`](Self::mount). Each one counts
/// its size, what it owns on the heap (measured with [`GetSize`](get_size2::GetSize), spare
/// capacity included), and its entry in the cache. Only the allocator's own overhead is not
/// counted.
///
/// - Over budget, the least recently used unmodified objects are dropped, with no I/O: they are
///   read again when needed. The object just used stays, even alone over budget.
/// - Modified objects stay until a flush. When an object would become modified and they would
///   then take more than `cache_bytes`, [`with_mut`] and [`insert`] call the `on_full` callback
///   given to [`mount`](Self::mount), to start a flush, and return `Ok(None)` without changing
///   anything. An object already modified can still change, and grow over the budget.
/// - An object that alone takes more than `cache_bytes` can never be inserted or become
///   modified, as no flush can make room for it: [`with_mut`] and [`insert`] return
///   [`Error::TooBig`]. It can still be read.
///
/// # Durability
///
/// Changes reach the backend on [`flush`], [`flush_object`] for that object, or
/// [`unmount`](Self::unmount): a reset loses the rest. Each object is stored with its name. A
/// power cut during a flush leaves each object either as it was or as written, with its name: an
/// object replaced by a new one of the same name ([`remove`], then [`insert`]) stays until the new
/// one is written, and the next mount keeps the newer one if both are left.
pub struct TierMem<B: Backend> {
    pub(crate) core: Core<B>,
}

impl<B: Backend> TierMem<B> {
    /// Mounts the backend and reads the names of the objects, blocking until done. Objects in RAM take
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
        // Owned: no other flush can have the storage.
        flush(&mut self)?;
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

/// Names of all the stored objects, in no particular order.
pub fn list<B: Backend>(mem: &mut impl Mutex<T = TierMem<B>>) -> Vec<String> {
    Locked(mem).list()
}

/// Handle to the object `name`.
///
/// `T` must be the type the object was inserted with. Returns [`Error::NotFound`] if there is no
/// object with this name.
pub fn get<T: Storable, B: Backend>(
    mem: &mut impl Mutex<T = TierMem<B>>,
    name: &str,
) -> Result<Handle<T>, Error<B::Error>> {
    let id = Locked(mem).get(name).ok_or(Error::NotFound)?;
    Ok(Handle::new(id))
}

/// Inserts a new object `name` and returns its handle. The object is in RAM, and reaches the
/// backend at the next [`flush`].
///
/// Returns [`Error::AlreadyExists`] if an object with this name exists, `Ok(None)` if the changes
/// not flushed yet leave no room for it, and [`Error::TooBig`] if it is bigger than the whole
/// budget (see [RAM budget](TierMem#ram-budget)).
pub fn insert<T: Storable, B: Backend>(
    mem: &mut impl Mutex<T = TierMem<B>>,
    name: &str,
    value: T,
) -> Result<Option<Handle<T>>, Error<B::Error>> {
    Ok(Locked(mem).insert(name, value)?.map(Handle::new))
}

/// Handle to the object `name`. If there is no object with this name (as on the first boot),
/// inserts the value returned by `f` first; `f` only runs in that case, and `Ok(None)` or
/// [`Error::TooBig`] mean it did not fit, as for [`insert`].
///
/// `T` must be the type the object was inserted with.
pub fn get_or_insert_with<T: Storable, B: Backend>(
    mem: &mut impl Mutex<T = TierMem<B>>,
    name: &str,
    f: impl FnOnce() -> T,
) -> Result<Option<Handle<T>>, Error<B::Error>> {
    match get(mem, name) {
        Err(Error::NotFound) => match insert(mem, name, f()) {
            // Inserted by another task meanwhile.
            Err(Error::AlreadyExists) => get(mem, name).map(Some),
            result => result,
        },
        result => result.map(Some),
    }
}

/// Removes the object of `handle`. The removal reaches the backend at the next [`flush`].
///
/// Returns [`Error::NotFound`] if it is already removed.
pub fn remove<T: Storable, B: Backend>(
    mem: &mut impl Mutex<T = TierMem<B>>,
    handle: Handle<T>,
) -> Result<(), Error<B::Error>> {
    Locked(mem).remove(handle.id)
}

/// Runs `f` on the object of `handle` and returns its result. Reads the object if it is not in
/// RAM, or returns `Ok(None)` without running `f` if a flush has the storage (see
/// [Not now](TierMem#not-now-oknone)).
pub fn with<T: Storable, R, B: Backend>(
    mem: &mut impl Mutex<T = TierMem<B>>,
    handle: &Handle<T>,
    f: impl FnOnce(&T) -> R,
) -> Result<Option<R>, Error<B::Error>> {
    Locked(mem).with(handle.id, false, |value: &mut T| f(value))
}

/// Runs `f` on the object of `handle` with mutable access and returns its result. Reads the
/// object if it is not in RAM. The change is in RAM when this returns, and reaches the backend at
/// the next [`flush`].
///
/// Returns `Ok(None)` without running `f` if a flush has the storage and the object is not in
/// RAM, or if the changes not flushed yet leave no room for this object, and [`Error::TooBig`]
/// if it is bigger than the whole budget (see [RAM budget](TierMem#ram-budget)).
pub fn with_mut<T: Storable, R, B: Backend>(
    mem: &mut impl Mutex<T = TierMem<B>>,
    handle: &Handle<T>,
    f: impl FnOnce(&mut T) -> R,
) -> Result<Option<R>, Error<B::Error>> {
    Locked(mem).with(handle.id, true, f)
}

/// How the backend's space is used (see [`Usage`]), as of the last storage operation.
///
/// The changes not flushed yet are not counted: modified objects count with their stored size,
/// and new ones not at all.
pub fn usage<B: Backend>(mem: &mut impl Mutex<T = TierMem<B>>) -> Usage {
    Locked(mem).usage()
}

/// Writes the changes to the backend: the modified and new objects, each with its name, then the
/// removals. Returns `Ok(None)` if another flush has the storage (this one preempted it).
///
/// Each object modified when it starts is written once, so it ends even if an interrupt keeps
/// modifying objects. An object that fails to write stays in RAM for the next flush, and the
/// others and the removals are still done, so that one object failing at every flush does not
/// block them: the first error is returned at the end.
pub fn flush<B: Backend>(
    mem: &mut impl Mutex<T = TierMem<B>>,
) -> Result<Option<()>, Error<B::Error>> {
    Locked(mem).flush()
}

/// Writes the object of `handle` to the backend if it is modified, with its name. The other
/// changes, removals included, stay in RAM until [`flush`].
///
/// Returns [`Error::NotFound`] if the object is removed, and `Ok(None)` if another flush has the
/// storage, as [`flush`] does. Objects that must stay consistent with each other should be
/// flushed together, with [`flush`].
pub fn flush_object<T: Storable, B: Backend>(
    mem: &mut impl Mutex<T = TierMem<B>>,
    handle: &Handle<T>,
) -> Result<Option<()>, Error<B::Error>> {
    Locked(mem).flush_object(handle.id)
}
