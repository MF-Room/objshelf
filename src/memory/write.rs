//! `flush` and `flush_object`, the only code that writes to the backend. Each write takes the
//! backend out of the core inside the lock (`None` if another flush has it), writes outside of
//! the lock, and puts the backend back inside it.

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
    /// flush, written with the directory but not yet themselves, and those of new objects whose
    /// write failed.
    ///
    /// An object that fails to write stays modified, and the others and the names are still
    /// written, so that one object failing at every flush does not block them: the first error is
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
        let names = self.write_names();
        match first_error {
            Some(error) => Err(error),
            None => names,
        }
    }

    /// Writes the object `id` if it is modified, then the directory if it changed, as a new object
    /// needs its name stored, and removes the removed objects. The directory may then name new
    /// objects not written yet: the next mount drops those names if they never are. `None` if
    /// another flush has the backend.
    pub(crate) fn flush_object(&mut self, id: ObjectId) -> Result<Option<()>, Error<B::Error>> {
        if !self.lock(|core| core.is_named(id)) {
            return Err(Error::NotFound);
        }
        if self.write_back(id)?.is_none() {
            return Ok(None);
        }
        self.write_names()
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
            let entry = core.cache.remove(&id).expect("modified, so cached");
            Some(Some((backend, entry)))
        });
        let (mut backend, mut entry) = match taken {
            None => return Ok(None),
            Some(None) => return Ok(Some(())),
            Some(Some(taken)) => taken,
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
        result.map(Some).map_err(Error::Backend)
    }

    /// Writes the directory if it changed, then removes the removed objects that the backend
    /// holds. `None` if another flush has the backend.
    fn write_names(&mut self) -> Result<Option<()>, Error<B::Error>> {
        let taken = self.lock(|core| {
            if !core.directory_modified {
                return Some(None);
            }
            let backend = core.backend.take()?;
            core.directory_modified = false;
            let removed = mem::take(&mut core.removed);
            Some(Some((backend, core.stored_directory(), removed)))
        });
        let (mut backend, directory, removed) = match taken {
            None => return Ok(None),
            Some(None) => return Ok(Some(())),
            Some(Some(taken)) => taken,
        };
        let result = store_names(&mut backend, &directory, &removed);
        self.lock(|core| {
            core.restore(backend);
            if result.is_err() {
                core.directory_modified = true;
                core.removed.extend(removed);
            }
        });
        result.map(Some).map_err(Error::Backend)
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
