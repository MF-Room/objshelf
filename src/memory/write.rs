//! `flush` and `flush_object`, the only code that writes to the backend. Each write takes the
//! backend out of the core inside the lock (`None` if another flush has it), writes outside of
//! the lock, and puts the backend back inside it.

use alloc::string::String;
use alloc::vec::Vec;
use core::mem;

use hashbrown::HashSet;
use rtic_core::Mutex;

use super::Locked;
use crate::backend::Backend;
use crate::{Error, ObjectId, TierMem};

/// Removed objects, with their names.
type Removed = Vec<(ObjectId, String)>;

impl<B: Backend, M: Mutex<T = TierMem<B>>> Locked<'_, M> {
    /// Writes the objects modified when this starts, each once, so an interrupt that keeps
    /// modifying objects cannot keep it going. Each object is written with its name, so a new one
    /// is stored with it at once. Then removes the removed objects.
    ///
    /// An object that fails to write stays modified, and the others and the removals are still
    /// done, so that one object failing at every flush does not block them: the first error is
    /// returned at the end. Stops at `None` if another flush has the backend. What is not written
    /// waits for the next flush.
    pub(crate) fn flush(&mut self) -> Result<Option<()>, Error<B::Error>> {
        let modified: Vec<ObjectId> = self.lock(|core| {
            let modified = core.cache.iter().filter(|(_, entry)| entry.modified);
            modified.map(|(&id, _)| id).collect()
        });
        let mut first_error = None;
        for id in modified {
            match self.write_back(id) {
                Ok(Some(())) => {}
                Ok(None) => return Ok(None),
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        let removals = self.write_removals();
        match first_error {
            Some(error) => Err(error),
            None => removals,
        }
    }

    /// Writes the object `id` if it is modified, with its name. `None` if another flush has the
    /// backend.
    pub(crate) fn flush_object(&mut self, id: ObjectId) -> Result<Option<()>, Error<B::Error>> {
        if !self.lock(|core| core.is_named(id)) {
            return Err(Error::NotFound);
        }
        self.write_back(id)
    }

    /// Writes the object `id` back if it is modified, `None` if another flush has the backend. It
    /// is out of the cache meanwhile, so nothing changes it during the write. If it was removed
    /// meanwhile, it is then dropped.
    fn write_back(&mut self, id: ObjectId) -> Result<Option<()>, Error<B::Error>> {
        let taken = self.lock(|core| {
            if !core.cache.get(&id).is_some_and(|entry| entry.modified) {
                return Some(None);
            }
            let backend = core.backend.take()?;
            let name = String::from(core.name_of(id).expect("cached, so named"));
            let entry = core.cache.remove(&id).expect("modified, so cached");
            Some(Some((backend, name, entry)))
        });
        let (mut backend, name, mut entry) = match taken {
            None => return Ok(None),
            Some(None) => return Ok(Some(())),
            Some(Some(taken)) => taken,
        };
        let result = entry.value.write(&mut backend, id, &name);
        self.lock(|core| {
            core.restore(backend);
            if result.is_ok() {
                core.modified_bytes -= entry.size;
                entry.modified = false;
            }
            if core.is_named(id) {
                core.cache.insert(id, entry);
            } else {
                // Removed during the write: `removed` holds it.
                core.used -= entry.size;
                if entry.modified {
                    core.modified_bytes -= entry.size;
                }
            }
        });
        result.map(Some).map_err(Error::Backend)
    }

    /// Removes the removed objects that the backend holds. One whose name was given to a new
    /// object stays until the backend holds that one, as its write may have failed: after a power
    /// cut the name has the old object or the new one, never none, and the mount keeps the newer
    /// of two. `None` if another flush has the backend.
    fn write_removals(&mut self) -> Result<Option<()>, Error<B::Error>> {
        let taken = self.lock(|core| {
            if core.removed.is_empty() {
                return Some(None);
            }
            let backend = core.backend.take()?;
            let removed = mem::take(&mut core.removed);
            let successors = removed.iter();
            let successors = successors.map(|(_, name)| core.directory.get(name).copied());
            let successors = successors.collect();
            Some(Some((backend, removed, successors)))
        });
        let (mut backend, removed, successors) = match taken {
            None => return Ok(None),
            Some(None) => return Ok(Some(())),
            Some(Some(taken)) => taken,
        };
        let (kept, result) = remove_stored(&mut backend, removed, successors);
        self.lock(|core| {
            core.restore(backend);
            core.removed.extend(kept);
        });
        result.map(Some).map_err(Error::Backend)
    }
}

/// Removes the objects of `removed` that the backend holds, except those whose successor, the
/// object that has their name now, it does not hold yet. Returns those left, to retry.
fn remove_stored<B: Backend>(
    backend: &mut B,
    removed: Removed,
    successors: Vec<Option<ObjectId>>,
) -> (Removed, Result<(), B::Error>) {
    let stored: HashSet<ObjectId> = backend.ids().into_iter().collect();
    let mut kept = Vec::new();
    let mut removals = removed.into_iter().zip(successors);
    while let Some(((id, name), successor)) = removals.next() {
        if !stored.contains(&id) {
            // Never written.
            continue;
        }
        if successor.is_some_and(|successor| !stored.contains(&successor)) {
            kept.push((id, name));
            continue;
        }
        if let Err(error) = backend.remove(id) {
            kept.push((id, name));
            kept.extend(removals.map(|(removal, _)| removal));
            return (kept, Err(error));
        }
    }
    (kept, Ok(()))
}
