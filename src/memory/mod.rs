//! The engine behind the public API of `api.rs`: the state of a `TierMem` (`Core`), and the code
//! that moves objects between RAM and the backend, reaching that state through the lock of the
//! `TierMem` (`Locked`).
//!
//! - `object`: cached objects with their type erased.
//! - `cache`: cached objects, their use by the `with*` and `try_*` methods, and their reads.
//! - `names`: the names, `insert` and `remove`.
//! - `write`: `flush`, the only code that writes to the backend.
//! - `lifecycle`: mount, with the name directory (object 0), and unmount.
//!
//! The lock is the one of the RTIC resource: it blocks the tasks that share the `TierMem`, and
//! only them. The work falls in three kinds:
//!
//! - Changes (`with_mut`, `insert`, `remove`): in RAM only, inside the lock. They stay there
//!   until a flush.
//! - Reads of objects that are not in RAM: inside the lock too, so the tasks that share the memory
//!   wait for them, and none of them ever finds a read in progress.
//! - Writes, by `flush` only: the backend and the object to write are taken out of `Core` inside
//!   the lock, written outside of it, and put back inside the lock. While the backend is out, the
//!   storage is busy: a `try_*` call that needs it returns `None`, a blocking call waits.

mod cache;
mod lifecycle;
mod names;
mod object;
#[cfg(test)]
mod tests;
mod write;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use hashbrown::HashMap;
use rtic_core::Mutex;

use crate::backend::{Backend, Usage};
use crate::{Error, ObjectId, TierMem};
use cache::Entry;

/// The state of a `TierMem`.
pub(crate) struct Core<B: Backend> {
    /// The backend, `None` while a write uses it: the storage is busy.
    backend: Option<B>,
    /// How the backend's space is used, as of the last storage operation.
    usage: Usage,

    // Names (`names.rs`).
    directory: BTreeMap<String, ObjectId>,
    /// The directory differs from the one stored as object 0 (`lifecycle.rs`).
    directory_modified: bool,
    /// Removed objects that the backend may still hold.
    removed: Vec<ObjectId>,
    next_id: u32,

    // Cached objects (`cache.rs`).
    cache: HashMap<ObjectId, Entry<B>>,
    /// RAM budget of the cached objects.
    cache_bytes: usize,
    /// RAM taken by the cached objects, including the one being written.
    used: usize,
    /// RAM taken by the modified objects, including the one being written.
    modified_bytes: usize,
    /// Called when the modified objects fill the budget, to start a flush.
    on_full: fn(),
    /// Clock of the last uses, for the LRU drop.
    tick: u64,
}

/// A `TierMem` reached through its lock `M`.
pub(crate) struct Locked<'a, M>(pub(crate) &'a mut M);

impl<B: Backend, M: Mutex<T = TierMem<B>>> Locked<'_, M> {
    /// Runs `f` on the core, inside the lock.
    fn lock<R>(&mut self, f: impl FnOnce(&mut Core<B>) -> R) -> R {
        self.0.lock(|mem| f(&mut mem.core))
    }

    /// Runs `f` inside the lock until it returns `Some`: `None` means that a flush has the
    /// backend. The lock is released in between. On a single core, that flush was preempted by
    /// the caller and cannot finish: `TierMem` documents the rule that avoids it.
    fn wait<R>(&mut self, mut f: impl FnMut(&mut Core<B>) -> Option<R>) -> R {
        loop {
            if let Some(result) = self.lock(&mut f) {
                return result;
            }
            core::hint::spin_loop();
        }
    }

    pub(crate) fn usage(&mut self) -> Usage {
        self.lock(|core| core.usage)
    }
}

impl<B: Backend> Core<B> {
    fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    /// Whether `size` more bytes of modified objects fit in the budget.
    fn fits_modified(&self, size: usize) -> bool {
        self.modified_bytes + size <= self.cache_bytes
    }

    /// Calls `on_full` and returns `Error::Full`.
    fn full<E>(&self) -> Error<E> {
        (self.on_full)();
        Error::Full
    }

    /// Puts the backend back after a write.
    fn restore(&mut self, backend: B) {
        self.usage = backend.usage();
        self.backend = Some(backend);
    }
}
