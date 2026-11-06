//! The object memory: named, typed objects cached in RAM and stored in a backend.

mod ops;
mod runner;
#[cfg(test)]
mod tests;

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::any::Any;
use core::future::Future;
use core::mem;
use core::pin::Pin;
use core::task::{Poll, Waker};

use get_size2::GetSize;
use hashbrown::{HashMap, HashSet};

use crate::backend::{Backend, Usage};
use crate::{Error, Handle, ObjectId, Storable};

pub use ops::{flush, mounted, unmount, with, with_mut};
pub use runner::Runner;

/// Object holding the name directory: the `(name, id)` pairs of the stored objects.
const DIRECTORY: ObjectId = ObjectId(0);

type Directory = Vec<(String, ObjectId)>;

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// A cached object, its type erased.
trait Cached<B: Backend>: Any + Send {
    fn write<'a>(&'a self, backend: &'a mut B, id: ObjectId)
    -> BoxFuture<'a, Result<(), B::Error>>;

    /// RAM taken by the object: its own size and what it owns on the heap.
    fn size(&self) -> usize;
}

impl<B: Backend, T: Storable> Cached<B> for T {
    fn write<'a>(
        &'a self,
        backend: &'a mut B,
        id: ObjectId,
    ) -> BoxFuture<'a, Result<(), B::Error>> {
        Box::pin(backend.write(id, self))
    }

    fn size(&self) -> usize {
        self.get_size()
    }
}

/// Reads an object as a `T`. The type is only known where the load is requested, so this is
/// stored as a function pointer until the runner does it.
type LoadFn<B> = for<'a> fn(
    &'a mut B,
    ObjectId,
) -> BoxFuture<'a, Result<Box<dyn Cached<B>>, <B as Backend>::Error>>;

fn load<'a, B: Backend, T: Storable>(
    backend: &'a mut B,
    id: ObjectId,
) -> BoxFuture<'a, Result<Box<dyn Cached<B>>, B::Error>> {
    Box::pin(async move {
        let value: T = backend.read(id).await?;
        Ok(Box::new(value) as Box<dyn Cached<B>>)
    })
}

struct Entry<B: Backend> {
    value: Box<dyn Cached<B>>,
    size: usize,
    /// Changed since it was last written to the backend (or never written).
    modified: bool,
    /// Loaded and not accessed yet: evicted last, so the task that asked for it gets it.
    fresh: bool,
    last_use: u64,
}

enum Slot<B: Backend> {
    Ready(Entry<B>),
    /// Waiting for the runner to load it.
    Queued(LoadFn<B>),
    /// The runner is loading or writing it.
    Busy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Mounting,
    Ready,
    Failed,
    Unmounting,
    Unmounted,
}

/// Flush in progress: the objects modified when it started, then the directory, then the
/// removals.
struct FlushPass<E> {
    generation: u64,
    queue: Vec<ObjectId>,
    directory_done: bool,
    /// First error, returned by `flush`. The other steps still run.
    error: Option<E>,
}

/// What follows the write-back of an evicted object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tidy {
    Idle,
    /// Write the directory, so it names the new object.
    Directory,
    /// Then remove the objects whose name is gone from the stored directory.
    Removals,
}

/// One storage operation for the runner, with what it needs taken out of the shared state.
enum Job<B: Backend> {
    Load {
        id: ObjectId,
        load: LoadFn<B>,
    },
    Write {
        id: ObjectId,
        entry: Entry<B>,
        evict: bool,
    },
    Directory {
        version: u64,
        complete: bool,
        entries: Directory,
    },
    Remove {
        id: ObjectId,
        version: u64,
    },
    Unmount,
}

/// Object memory on top of a storage [`Backend`], shared between RTIC tasks.
///
/// Objects have a name and a type. [`list`](Self::list) returns the names, [`get`](Self::get),
/// [`insert`](Self::insert) and [`get_or_insert_with`](Self::get_or_insert_with) return a typed
/// [`Handle`], and every access to an object goes through a closure, so no reference to it
/// escapes.
///
/// A `TierMem` does no storage I/O itself: it is the RAM side, an RTIC `#[shared]` resource. A
/// [`Runner`], the `#[local]` resource of a low-priority task, owns the backend and does all the
/// I/O: mount, loads, write-backs and flushes. Each lock is short (a few map operations, or the
/// closure given to a `with*` call), so higher-priority tasks and interrupts are never held up by
/// the storage.
///
/// ```ignore
/// #[shared]
/// struct Shared { mem: TierMem<NorBackend<Flash>> }
/// #[local]
/// struct Local { runner: Runner<NorBackend<Flash>> }
///
/// // in `init`: TierMem::new(16 * 1024), Runner::new(NorConfig { flash, range }), storage::spawn()
///
/// #[task(priority = 1, shared = [mem], local = [runner])]
/// async fn storage(mut cx: storage::Context) {
///     let result = cx.local.runner.run(&mut cx.shared.mem).await; // returns after unmount
/// }
///
/// #[task(priority = 2, shared = [mem])]
/// async fn app(mut cx: app::Context) {
///     tiermem::mounted(&mut cx.shared.mem).await?;
///     let settings = cx.shared.mem.lock(|m| m.get_or_insert_with("settings", Settings::default))?;
///     let bpm = tiermem::with(&mut cx.shared.mem, &settings, |s| s.bpm).await?; // loads if needed
///     tiermem::with_mut(&mut cx.shared.mem, &settings, |s| s.bpm = 120).await?;
///     tiermem::flush(&mut cx.shared.mem).await?; // modified objects go to the backend
/// }
///
/// #[task(binds = TIM2, priority = 3, shared = [mem])]
/// fn tick(mut cx: tick::Context) {
///     // `None` if the object is not in RAM: its load is queued, try again later.
///     let bpm = cx.shared.mem.lock(|m| m.try_with(&SETTINGS, |s| s.bpm));
/// }
/// ```
///
/// # Locks
///
/// Methods take `&mut self` and never wait: they run inside an RTIC `lock`. Closures given to
/// [`try_with`](Self::try_with), [`try_with_mut`](Self::try_with_mut), [`with`](crate::with) and
/// [`with_mut`](crate::with_mut) run inside it too, so keep them short.
///
/// # Durability
///
/// [`insert`](Self::insert), [`remove`](Self::remove) and the changes made by `with_mut` and
/// `try_with_mut` are in RAM when the call returns. They reach the backend when the object is
/// evicted, on [`flush`](crate::flush) or on [`unmount`](crate::unmount).
///
/// # Names
///
/// The whole directory of names is kept in RAM and rewritten to the backend when it changes, so
/// keep names short.
///
/// # Types
///
/// `T` must be the type the object was inserted with. A cached object of another type gives
/// [`Error::WrongType`], but the backend does not check the type of what it reads: postcard may
/// decode the bytes as another type, or fail with a backend error (TODO in the backend: store a
/// type tag).
pub struct TierMem<B: Backend> {
    state: State,
    directory: BTreeMap<String, ObjectId>,
    /// Ids named in `directory`.
    live: HashSet<ObjectId>,
    /// Ids the backend holds, the directory left out.
    stored: HashSet<ObjectId>,
    next_id: u32,
    /// Incremented on each change of `directory`.
    directory_version: u64,
    /// Version of the last directory written to the backend.
    directory_written: u64,
    /// The last directory written named all the objects: none was missing from the backend.
    directory_complete: bool,
    /// Objects to remove from the backend once a directory without them is written: `(id,
    /// directory version)`.
    removals: Vec<(ObjectId, u64)>,
    /// Removals that failed, retried on the next flush.
    failed_removals: Vec<(ObjectId, u64)>,
    cache: HashMap<ObjectId, Slot<B>>,
    cache_bytes: usize,
    /// RAM taken by the cached objects, including the ones the runner is writing.
    used: usize,
    tick: u64,
    loads: VecDeque<ObjectId>,
    load_errors: HashMap<ObjectId, B::Error>,
    /// Objects whose write-back failed on eviction: not evicted again until the next flush.
    failed_writes: HashSet<ObjectId>,
    flush_requested: u64,
    flush_done: u64,
    flush: Option<FlushPass<B::Error>>,
    flush_error: Option<(u64, B::Error)>,
    tidy: Tidy,
    unmount_error: Option<B::Error>,
    usage: Usage,
    runner_waker: Option<Waker>,
    /// Tasks waiting for the runner, woken after each of its steps.
    waiters: Vec<Waker>,
}

impl<B: Backend> TierMem<B> {
    /// Empty memory, mounted by [`Runner::run`]. Cached objects take up to `cache_bytes` of heap
    /// (sizes from [`GetSize`]). The budget can be exceeded for a short time: objects are
    /// evicted by the runner, between its storage operations.
    pub fn new(cache_bytes: usize) -> Self {
        Self {
            state: State::Mounting,
            directory: BTreeMap::new(),
            live: HashSet::new(),
            stored: HashSet::new(),
            next_id: 1,
            directory_version: 0,
            directory_written: 0,
            directory_complete: true,
            removals: Vec::new(),
            failed_removals: Vec::new(),
            cache: HashMap::new(),
            cache_bytes,
            used: 0,
            tick: 0,
            loads: VecDeque::new(),
            load_errors: HashMap::new(),
            failed_writes: HashSet::new(),
            flush_requested: 0,
            flush_done: 0,
            flush: None,
            flush_error: None,
            tidy: Tidy::Idle,
            unmount_error: None,
            usage: Usage::default(),
            runner_waker: None,
            waiters: Vec::new(),
        }
    }

    /// `true` once mounted, until unmounted. Methods return [`Error::Unavailable`] otherwise;
    /// [`mounted`](crate::mounted) waits for it.
    pub fn is_mounted(&self) -> bool {
        self.state == State::Ready
    }

    /// Names of all the objects, in alphabetical order.
    pub fn list(&self) -> Result<impl Iterator<Item = &str> + '_, Error<B::Error>> {
        self.check_ready()?;
        Ok(self.directory.keys().map(String::as_str))
    }

    /// Handle to the object `name`. Returns [`Error::NotFound`] if there is no object with this
    /// name.
    ///
    /// `T` must be the type the object was inserted with (see [Types](Self#types)).
    pub fn get<T: Storable>(&self, name: &str) -> Result<Handle<T>, Error<B::Error>> {
        self.check_ready()?;
        let id = self.directory.get(name).ok_or(Error::NotFound)?;
        Ok(Handle::new(*id))
    }

    /// Adds a new object `name` and returns its handle. It is in RAM, and reaches the backend
    /// like a modified object (see [Durability](Self#durability)).
    ///
    /// Returns [`Error::AlreadyExists`] if an object with this name exists, and
    /// [`Error::TooLarge`] if `value` takes more RAM than the whole cache.
    pub fn insert<T: Storable>(
        &mut self,
        name: &str,
        value: T,
    ) -> Result<Handle<T>, Error<B::Error>> {
        self.check_ready()?;
        if self.directory.contains_key(name) {
            return Err(Error::AlreadyExists);
        }
        let size = value.get_size();
        if size > self.cache_bytes {
            return Err(Error::TooLarge);
        }

        let id = ObjectId(self.next_id);
        self.next_id += 1;
        self.directory.insert(name.to_string(), id);
        self.live.insert(id);
        self.directory_version += 1;
        let last_use = self.next_tick();
        let entry = Entry {
            value: Box::new(value),
            size,
            modified: true,
            fresh: false,
            last_use,
        };
        self.cache.insert(id, Slot::Ready(entry));
        self.add_used(size);
        Ok(Handle::new(id))
    }

    /// Handle to the object `name`. If there is no object with this name (as on the first boot),
    /// inserts the value returned by `f` first; `f` only runs in that case.
    ///
    /// `T` must be the type the object was inserted with (see [Types](Self#types)).
    pub fn get_or_insert_with<T: Storable>(
        &mut self,
        name: &str,
        f: impl FnOnce() -> T,
    ) -> Result<Handle<T>, Error<B::Error>> {
        match self.get(name) {
            Err(Error::NotFound) => self.insert(name, f()),
            result => result,
        }
    }

    /// Removes the object of `handle`. The removal reaches the backend on the next flush (see
    /// [Durability](Self#durability)).
    pub fn remove<T>(&mut self, handle: Handle<T>) -> Result<(), Error<B::Error>> {
        self.check_ready()?;
        let id = handle.id;
        if !self.live.remove(&id) {
            return Err(Error::NotFound);
        }
        self.directory.retain(|_, named| *named != id);
        self.directory_version += 1;
        // A `Busy` slot is the runner's: it accounts for it when its operation ends.
        if let Some(Slot::Ready(entry)) = self.cache.remove(&id) {
            self.used -= entry.size;
        }
        self.load_errors.remove(&id);
        self.failed_writes.remove(&id);
        if self.stored.contains(&id) {
            self.removals.push((id, self.directory_version));
        }
        Ok(())
    }

    /// Runs `f` on the object of `handle` and returns its result, if the object is in RAM.
    /// Otherwise returns `Ok(None)` without running `f`, and queues a load: try again later, or
    /// use [`with`](crate::with), which waits for it.
    ///
    /// Also `Ok(None)` while the runner writes the object back. Real errors still come as `Err`,
    /// so `?` and the `Option` methods replace a `match`:
    /// `m.try_with(&h, |s| s.bpm)?.unwrap_or(last_bpm)`.
    pub fn try_with<T: Storable, R>(
        &mut self,
        handle: &Handle<T>,
        f: impl FnOnce(&T) -> R,
    ) -> Result<Option<R>, Error<B::Error>> {
        match self.poll_with(handle.id, &mut Some(f), None) {
            Poll::Ready(result) => result.map(Some),
            Poll::Pending => Ok(None),
        }
    }

    /// Like [`try_with`](Self::try_with), with mutable access. The change is in RAM when this
    /// returns (see [Durability](Self#durability)).
    pub fn try_with_mut<T: Storable, R>(
        &mut self,
        handle: &Handle<T>,
        f: impl FnOnce(&mut T) -> R,
    ) -> Result<Option<R>, Error<B::Error>> {
        match self.poll_with_mut(handle.id, &mut Some(f), None) {
            Poll::Ready(result) => result.map(Some),
            Poll::Pending => Ok(None),
        }
    }

    /// How the backend's space is used (see [`Usage`]), as of the runner's last storage
    /// operation. Objects modified in RAM count with their stored size.
    pub fn usage(&self) -> Result<Usage, Error<B::Error>> {
        self.check_ready()?;
        Ok(self.usage)
    }

    /// Asks the runner to write every modified object back, without waiting for it, as from an
    /// interrupt. [`flush`](crate::flush) waits and returns the result.
    pub fn request_flush(&mut self) -> Result<(), Error<B::Error>> {
        self.check_ready()?;
        self.queue_flush();
        Ok(())
    }

    fn check_ready(&self) -> Result<(), Error<B::Error>> {
        match self.state {
            State::Ready => Ok(()),
            _ => Err(Error::Unavailable),
        }
    }

    fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    fn add_used(&mut self, size: usize) {
        self.used += size;
        if self.used > self.cache_bytes {
            self.wake_runner();
        }
    }

    fn register(&mut self, waker: &Waker) {
        if !self.waiters.iter().any(|w| w.will_wake(waker)) {
            self.waiters.push(waker.clone());
        }
    }

    fn wake_waiters(&mut self) {
        for waker in self.waiters.drain(..) {
            waker.wake();
        }
    }

    fn wake_runner(&mut self) {
        if let Some(waker) = self.runner_waker.take() {
            waker.wake();
        }
    }

    fn queue_flush(&mut self) -> u64 {
        self.flush_requested += 1;
        self.wake_runner();
        self.flush_requested
    }

    fn directory_dirty(&self) -> bool {
        self.directory_written != self.directory_version || !self.directory_complete
    }

    /// Nothing left to write or remove.
    fn is_clean(&self) -> bool {
        !self.directory_dirty()
            && self.removals.is_empty()
            && self.failed_removals.is_empty()
            && self.cache.values().all(|slot| match slot {
                Slot::Ready(entry) => !entry.modified,
                Slot::Queued(_) => true,
                Slot::Busy => false,
            })
    }

    /// `Ready(Ok(()))` once the object `id` is cached. Queues its load if needed. Without a
    /// `waker` (the `try_*` methods), a memory not mounted yet is an error rather than a wait.
    fn poll_cached<T: Storable>(
        &mut self,
        id: ObjectId,
        waker: Option<&Waker>,
    ) -> Poll<Result<(), Error<B::Error>>> {
        match (self.state, waker) {
            (State::Ready, _) => {}
            (State::Mounting, Some(waker)) => {
                self.register(waker);
                return Poll::Pending;
            }
            _ => return Poll::Ready(Err(Error::Unavailable)),
        }
        if !self.live.contains(&id) {
            return Poll::Ready(Err(Error::NotFound));
        }
        if let Some(error) = self.load_errors.remove(&id) {
            return Poll::Ready(Err(Error::Backend(error)));
        }
        match self.cache.get(&id) {
            Some(Slot::Ready(_)) => return Poll::Ready(Ok(())),
            Some(_) => {}
            None => {
                self.cache.insert(id, Slot::Queued(load::<B, T>));
                self.loads.push_back(id);
                self.wake_runner();
            }
        }
        if let Some(waker) = waker {
            self.register(waker);
        }
        Poll::Pending
    }

    /// The cached entry of `id`, marked as used. Only after `poll_cached` returned it ready.
    fn touch(&mut self, id: ObjectId) -> &mut Entry<B> {
        let tick = self.next_tick();
        let Some(Slot::Ready(entry)) = self.cache.get_mut(&id) else {
            unreachable!("object {id:?} is not cached");
        };
        entry.last_use = tick;
        entry.fresh = false;
        entry
    }

    fn poll_with<T: Storable, R>(
        &mut self,
        id: ObjectId,
        f: &mut Option<impl FnOnce(&T) -> R>,
        waker: Option<&Waker>,
    ) -> Poll<Result<R, Error<B::Error>>> {
        core::task::ready!(self.poll_cached::<T>(id, waker))?;
        let entry = self.touch(id);
        let value: &dyn Any = &*entry.value;
        let Some(value) = value.downcast_ref::<T>() else {
            return Poll::Ready(Err(Error::WrongType));
        };
        let f = f.take().expect("polled after completion");
        Poll::Ready(Ok(f(value)))
    }

    fn poll_with_mut<T: Storable, R>(
        &mut self,
        id: ObjectId,
        f: &mut Option<impl FnOnce(&mut T) -> R>,
        waker: Option<&Waker>,
    ) -> Poll<Result<R, Error<B::Error>>> {
        core::task::ready!(self.poll_cached::<T>(id, waker))?;
        let entry = self.touch(id);
        let value: &mut dyn Any = &mut *entry.value;
        let Some(value) = value.downcast_mut::<T>() else {
            return Poll::Ready(Err(Error::WrongType));
        };
        let f = f.take().expect("polled after completion");
        let result = f(value);
        let size = value.get_size();
        let old = mem::replace(&mut entry.size, size);
        entry.modified = true;
        self.used -= old;
        self.add_used(size);
        Poll::Ready(Ok(result))
    }

    fn poll_mounted(&mut self, waker: &Waker) -> Poll<Result<(), Error<B::Error>>> {
        match self.state {
            State::Mounting => {
                self.register(waker);
                Poll::Pending
            }
            State::Ready => Poll::Ready(Ok(())),
            _ => Poll::Ready(Err(Error::Unavailable)),
        }
    }

    fn poll_flush(&mut self, generation: u64, waker: &Waker) -> Poll<Result<(), Error<B::Error>>> {
        if self.flush_done >= generation {
            let error = self.flush_error.take_if(|(failed, _)| *failed >= generation);
            return Poll::Ready(error.map_or(Ok(()), |(_, error)| Err(Error::Backend(error))));
        }
        if self.state != State::Ready {
            return Poll::Ready(Err(Error::Unavailable));
        }
        self.register(waker);
        Poll::Pending
    }

    /// Starts unmounting if there is nothing left to write. New operations then fail with
    /// [`Error::Unavailable`].
    fn begin_unmount(&mut self) -> Result<bool, Error<B::Error>> {
        self.check_ready()?;
        if !self.is_clean() {
            return Ok(false);
        }
        self.state = State::Unmounting;
        self.wake_runner();
        self.wake_waiters();
        Ok(true)
    }

    fn poll_unmounted(&mut self, waker: &Waker) -> Poll<Result<(), Error<B::Error>>> {
        match self.state {
            State::Unmounting => {
                self.register(waker);
                Poll::Pending
            }
            State::Unmounted => Poll::Ready(match self.unmount_error.take() {
                Some(error) => Err(Error::Backend(error)),
                None => Ok(()),
            }),
            _ => Poll::Ready(Err(Error::Unavailable)),
        }
    }

    // Runner side.

    fn set_mounted(&mut self, directory: Directory, stored: Vec<ObjectId>, usage: Usage) {
        let named = directory.len();
        self.next_id = stored.iter().map(|id| id.0 + 1).max().unwrap_or(1).max(1);
        self.stored = stored.into_iter().collect();
        self.directory = directory
            .into_iter()
            .filter(|(_, id)| self.stored.contains(id))
            .collect();
        self.live = self.directory.values().copied().collect();
        if self.directory.len() != named {
            // Names of missing objects were dropped: store the directory without them.
            self.directory_version += 1;
        }
        self.usage = usage;
        self.state = State::Ready;
        self.wake_waiters();
    }

    fn set_failed(&mut self) {
        self.state = State::Failed;
        self.wake_waiters();
    }

    fn poll_job(&mut self, waker: &Waker) -> Poll<Job<B>> {
        match self.next_job() {
            Some(job) => Poll::Ready(job),
            None => {
                self.runner_waker = Some(waker.clone());
                Poll::Pending
            }
        }
    }

    /// The runner's next operation: flush, then loads, then what follows an eviction, then
    /// evictions while over budget.
    fn next_job(&mut self) -> Option<Job<B>> {
        match self.state {
            State::Ready => {}
            State::Unmounting => return Some(Job::Unmount),
            _ => return None,
        }
        if let Some(job) = self.flush_job() {
            return Some(job);
        }
        while let Some(id) = self.loads.pop_front() {
            if let Some(slot @ Slot::Queued(_)) = self.cache.get_mut(&id) {
                let Slot::Queued(load) = mem::replace(slot, Slot::Busy) else {
                    unreachable!();
                };
                return Some(Job::Load { id, load });
            }
        }
        if let Some(job) = self.tidy_job() {
            return Some(job);
        }
        self.evict_job()
    }

    fn flush_job(&mut self) -> Option<Job<B>> {
        loop {
            if self.flush.is_none() {
                if self.flush_done == self.flush_requested {
                    return None;
                }
                self.failed_writes.clear();
                self.removals.append(&mut self.failed_removals);
                let queue = self
                    .cache
                    .iter()
                    .filter(|(_, slot)| matches!(slot, Slot::Ready(entry) if entry.modified))
                    .map(|(&id, _)| id)
                    .collect();
                self.flush = Some(FlushPass {
                    generation: self.flush_requested,
                    queue,
                    directory_done: false,
                    error: None,
                });
            }
            while let Some(id) = self.flush.as_mut().and_then(|pass| pass.queue.pop()) {
                if let Some(job) = self.take_write(id, false) {
                    return Some(job);
                }
            }
            let pass = self.flush.as_mut()?;
            if !pass.directory_done {
                pass.directory_done = true;
                if self.directory_dirty() {
                    return Some(self.directory_job());
                }
            }
            if let Some(job) = self.removal_job() {
                return Some(job);
            }
            let pass = self.flush.take()?;
            self.flush_done = pass.generation;
            if let Some(error) = pass.error {
                self.flush_error = Some((pass.generation, error));
            }
            self.wake_waiters();
        }
    }

    fn tidy_job(&mut self) -> Option<Job<B>> {
        if self.tidy == Tidy::Directory {
            self.tidy = Tidy::Removals;
            if self.directory_dirty() {
                return Some(self.directory_job());
            }
        }
        if self.tidy == Tidy::Removals {
            if let Some(job) = self.removal_job() {
                return Some(job);
            }
            self.tidy = Tidy::Idle;
        }
        None
    }

    /// Evicts least recently used objects until under budget: unmodified ones are dropped, a
    /// modified one is written back first.
    fn evict_job(&mut self) -> Option<Job<B>> {
        while self.used > self.cache_bytes {
            let (_, _, id) = self
                .cache
                .iter()
                .filter_map(|(&id, slot)| match slot {
                    Slot::Ready(entry) if !self.failed_writes.contains(&id) => {
                        Some((entry.fresh, entry.last_use, id))
                    }
                    _ => None,
                })
                .min()?;
            if matches!(self.cache.get(&id), Some(Slot::Ready(entry)) if entry.modified) {
                return self.take_write(id, true);
            }
            if let Some(Slot::Ready(entry)) = self.cache.remove(&id) {
                self.used -= entry.size;
            }
        }
        None
    }

    /// Takes a modified object out of the cache for the runner to write it.
    fn take_write(&mut self, id: ObjectId, evict: bool) -> Option<Job<B>> {
        let slot = self.cache.get_mut(&id)?;
        if !matches!(slot, Slot::Ready(entry) if entry.modified) {
            return None;
        }
        let Slot::Ready(entry) = mem::replace(slot, Slot::Busy) else {
            unreachable!();
        };
        Some(Job::Write { id, entry, evict })
    }

    /// The directory to store: only names whose object is in the backend, so that after a reboot
    /// every name can be loaded.
    fn directory_job(&self) -> Job<B> {
        let entries: Directory = self
            .directory
            .iter()
            .filter(|(_, id)| self.stored.contains(*id))
            .map(|(name, &id)| (name.clone(), id))
            .collect();
        Job::Directory {
            version: self.directory_version,
            complete: entries.len() == self.directory.len(),
            entries,
        }
    }

    /// A removal whose name is already gone from the stored directory.
    fn removal_job(&mut self) -> Option<Job<B>> {
        let at = self
            .removals
            .iter()
            .position(|&(_, version)| version <= self.directory_written)?;
        let (id, version) = self.removals.swap_remove(at);
        Some(Job::Remove { id, version })
    }

    /// Records a failure of a flush step, if the step belongs to a flush.
    fn flush_failed(&mut self, error: B::Error) {
        if let Some(pass) = &mut self.flush {
            pass.error.get_or_insert(error);
        }
    }

    fn finish_load(
        &mut self,
        id: ObjectId,
        result: Result<Box<dyn Cached<B>>, B::Error>,
        usage: Usage,
    ) {
        self.usage = usage;
        // Otherwise removed during the load.
        if matches!(self.cache.get(&id), Some(Slot::Busy)) {
            match result {
                Ok(value) => {
                    let size = value.size();
                    let last_use = self.next_tick();
                    let entry = Entry {
                        value,
                        size,
                        modified: false,
                        fresh: true,
                        last_use,
                    };
                    self.cache.insert(id, Slot::Ready(entry));
                    self.used += size;
                }
                Err(error) => {
                    self.cache.remove(&id);
                    self.load_errors.insert(id, error);
                }
            }
        }
        self.wake_waiters();
    }

    fn finish_write(
        &mut self,
        id: ObjectId,
        mut entry: Entry<B>,
        evict: bool,
        result: Result<(), B::Error>,
        usage: Usage,
    ) {
        self.usage = usage;
        if result.is_ok() && self.stored.insert(id) && !self.live.contains(&id) {
            // Removed while it was written for the first time.
            self.removals.push((id, self.directory_version));
        }
        if !self.cache.contains_key(&id) {
            // Removed during the write.
            self.used -= entry.size;
        } else {
            match result {
                Ok(()) if evict => {
                    self.cache.remove(&id);
                    self.used -= entry.size;
                    self.tidy = Tidy::Directory;
                }
                Ok(()) => {
                    entry.modified = false;
                    self.cache.insert(id, Slot::Ready(entry));
                }
                Err(error) => {
                    self.cache.insert(id, Slot::Ready(entry));
                    if evict {
                        self.failed_writes.insert(id);
                    } else {
                        self.flush_failed(error);
                    }
                }
            }
        }
        self.wake_waiters();
    }

    fn finish_directory(
        &mut self,
        version: u64,
        complete: bool,
        result: Result<(), B::Error>,
        usage: Usage,
    ) {
        self.usage = usage;
        match result {
            Ok(()) => {
                self.directory_written = version;
                self.directory_complete = complete;
            }
            Err(error) => self.flush_failed(error),
        }
        self.wake_waiters();
    }

    fn finish_remove(&mut self, id: ObjectId, version: u64, result: Result<(), B::Error>, usage: Usage) {
        self.usage = usage;
        match result {
            Ok(()) => {
                self.stored.remove(&id);
            }
            Err(error) => {
                self.failed_removals.push((id, version));
                self.flush_failed(error);
            }
        }
        self.wake_waiters();
    }

    fn finish_unmount(&mut self, result: Result<(), B::Error>) {
        self.state = State::Unmounted;
        self.unmount_error = result.err();
        self.cache.clear();
        self.used = 0;
        self.wake_waiters();
    }
}
