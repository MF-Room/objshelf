//! Cached objects: their use by the `with*` and `try_*` methods, reads of the ones not in RAM,
//! and the drop of unmodified ones over the budget. Nothing here writes.

use alloc::boxed::Box;
use core::any::Any;

use embassy_futures::block_on;
use rtic_core::Mutex;

use super::object::{self, Cached};
use super::{Core, Locked};
use crate::backend::Backend;
use crate::{Error, ObjectId, Storable, TierMem};

pub(super) struct Entry<B: Backend> {
    pub(super) value: Box<dyn Cached<B>>,
    /// RAM counted for the object (see `object::size`).
    pub(super) size: usize,
    /// Changed since it was last written to the backend, or never written. It stays in RAM
    /// until a flush.
    pub(super) modified: bool,
    pub(super) last_use: u64,
}

impl<B: Backend, M: Mutex<T = TierMem<B>>> Locked<'_, M> {
    /// Runs `f` on the object `id`, waiting while a flush has the backend if the object is not in
    /// RAM. With `modify`, the object is then modified.
    pub(crate) fn with<T: Storable, R>(
        &mut self,
        id: ObjectId,
        modify: bool,
        f: impl FnOnce(&mut T) -> R,
    ) -> Result<R, Error<B::Error>> {
        let mut f = Some(f);
        self.wait(|core| core.access(id, modify, &mut f))
    }

    /// Like `with`, but `None` instead of waiting.
    pub(crate) fn try_with<T: Storable, R>(
        &mut self,
        id: ObjectId,
        modify: bool,
        f: impl FnOnce(&mut T) -> R,
    ) -> Result<Option<R>, Error<B::Error>> {
        let mut f = Some(f);
        self.lock(|core| core.access(id, modify, &mut f))
            .transpose()
    }
}

impl<B: Backend> Core<B> {
    /// Runs `f` on the object `id`, read from the backend first if it is not in RAM. `None` if
    /// that needs the backend and a flush has it. With `modify`, the object is then modified, or
    /// `Full` without running `f` if it does not fit with the other modified objects. Unmodified
    /// objects over the budget are dropped, the one just used left out.
    fn access<T: Storable, R>(
        &mut self,
        id: ObjectId,
        modify: bool,
        f: &mut Option<impl FnOnce(&mut T) -> R>,
    ) -> Option<Result<R, Error<B::Error>>> {
        if let Err(error) = self.load::<T>(id)? {
            return Some(Err(error));
        }
        let entry = &self.cache[&id];
        let value: &dyn Any = &*entry.value;
        if !value.is::<T>() {
            return Some(Err(Error::WrongType));
        }
        if modify && !entry.modified && !self.fits_modified(entry.size) {
            return Some(Err(self.full()));
        }
        let tick = self.next_tick();
        let entry = self.cache.get_mut(&id).expect("loaded");
        entry.last_use = tick;
        let value: &mut dyn Any = &mut *entry.value;
        let value = value.downcast_mut::<T>().expect("checked above");
        let f = f.take().expect("runs once");
        let result = f(value);
        if modify {
            let size = object::size(value);
            self.used = self.used - entry.size + size;
            if entry.modified {
                self.modified_bytes -= entry.size;
            }
            self.modified_bytes += size;
            entry.size = size;
            entry.modified = true;
        }
        self.drop_clean(id);
        Some(Ok(result))
    }

    /// Makes sure the object `id` is in RAM, reading it inside the lock if needed. `None` if
    /// that needs the backend and a flush has it, the write of this object included.
    fn load<T: Storable>(&mut self, id: ObjectId) -> Option<Result<(), Error<B::Error>>> {
        if self.cache.contains_key(&id) {
            return Some(Ok(()));
        }
        if !self.is_named(id) {
            return Some(Err(Error::NotFound));
        }
        let backend = self.backend.as_mut()?;
        Some(match block_on(backend.read::<T>(id)) {
            Ok(value) => {
                self.cache(id, value, false);
                Ok(())
            }
            Err(error) => Err(Error::Backend(error)),
        })
    }

    /// Caches `value` as the object `id`: `modified` if the backend does not hold it.
    pub(super) fn cache<T: Storable>(&mut self, id: ObjectId, value: T, modified: bool) {
        let size = object::size(&value);
        let last_use = self.next_tick();
        let entry = Entry {
            value: Box::new(value),
            size,
            modified,
            last_use,
        };
        self.cache.insert(id, entry);
        self.used += size;
        if modified {
            self.modified_bytes += size;
        }
    }

    pub(super) fn uncache(&mut self, id: ObjectId) {
        if let Some(entry) = self.cache.remove(&id) {
            self.used -= entry.size;
            if entry.modified {
                self.modified_bytes -= entry.size;
            }
        }
    }

    /// Drops unmodified objects, least recently used first, while over budget. `keep` stays.
    pub(super) fn drop_clean(&mut self, keep: ObjectId) {
        while self.used > self.cache_bytes {
            let Some(id) = self.lru_clean(keep) else {
                return;
            };
            self.uncache(id);
        }
    }

    /// The least recently used unmodified object, `keep` left out.
    fn lru_clean(&self, keep: ObjectId) -> Option<ObjectId> {
        self.cache
            .iter()
            .filter(|&(&id, entry)| id != keep && !entry.modified)
            .min_by_key(|(_, entry)| entry.last_use)
            .map(|(&id, _)| id)
    }
}
