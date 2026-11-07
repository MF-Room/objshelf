//! `flush`, the only code that writes to the backend. Each write takes the backend out of the
//! core inside the lock (`Busy` if another flush has it), writes outside of the lock, and puts the
//! backend back inside it.

use alloc::vec::Vec;
use core::mem;

use rtic_core::Mutex;

use super::Locked;
use super::lifecycle::{DIRECTORY, Directory};
use crate::backend::Backend;
use crate::{Error, ObjectId, TierMem};

impl<B: Backend, M: Mutex<T = TierMem<B>>> Locked<'_, M> {
    /// Writes the objects modified when this starts, each once, so an interrupt that keeps
    /// modifying objects cannot keep it going. Then writes the directory and removes the removed
    /// objects. New objects are stored before their names and removed ones lose their names first,
    /// so after a power cut the next mount removes the objects left without a name. It also drops
    /// the names left without an object: those of objects inserted by an interrupt during the
    /// flush, written with the directory but not yet themselves. Stops at the first error: what is
    /// not written waits for the next flush.
    pub(crate) fn flush(&mut self) -> Result<(), Error<B::Error>> {
        let modified: Vec<ObjectId> = self.lock(|core| {
            let modified = core.cache.iter().filter(|(_, entry)| entry.modified);
            modified.map(|(&id, _)| id).collect()
        });
        for id in modified {
            self.write_back(id)?;
        }
        self.write_names()
    }

    /// Writes the object `id` back if it is modified. It is out of the cache meanwhile, so
    /// nothing changes it during the write. If it was removed meanwhile, it is then dropped.
    fn write_back(&mut self, id: ObjectId) -> Result<(), Error<B::Error>> {
        let taken = self.lock(|core| {
            if !core.cache.get(&id).is_some_and(|entry| entry.modified) {
                return Ok(None);
            }
            let backend = core.backend.take().ok_or(Error::Busy)?;
            let entry = core.cache.remove(&id).expect("modified, so cached");
            Ok(Some((backend, entry)))
        })?;
        let Some((mut backend, mut entry)) = taken else {
            return Ok(());
        };
        let result = entry.value.write(&mut backend, id);
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
        result.map_err(Error::Backend)
    }

    /// Writes the directory if it changed, then removes the removed objects that the backend
    /// holds.
    fn write_names(&mut self) -> Result<(), Error<B::Error>> {
        let taken = self.lock(|core| {
            if !core.directory_modified {
                return Ok(None);
            }
            let backend = core.backend.take().ok_or(Error::Busy)?;
            core.directory_modified = false;
            let removed = mem::take(&mut core.removed);
            Ok(Some((backend, core.stored_directory(), removed)))
        })?;
        let Some((mut backend, directory, removed)) = taken else {
            return Ok(());
        };
        let result = store_names(&mut backend, &directory, &removed);
        self.lock(|core| {
            core.restore(backend);
            if result.is_err() {
                core.directory_modified = true;
                core.removed.extend(removed);
            }
        });
        result.map_err(Error::Backend)
    }
}

/// Writes `directory`, then removes the objects of `removed` that the backend holds.
fn store_names<B: Backend>(
    backend: &mut B,
    directory: &Directory,
    removed: &[ObjectId],
) -> Result<(), B::Error> {
    backend.write(DIRECTORY, directory)?;
    let stored = backend.ids();
    for &id in removed.iter().filter(|id| stored.contains(id)) {
        backend.remove(id)?;
    }
    Ok(())
}
