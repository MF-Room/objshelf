//! The names of the objects, `insert` and `remove`. They change RAM only, inside the lock, and
//! never wait: `flush` writes the objects, then the directory, then removes the removed objects
//! (`write.rs`).

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use rtic_core::Mutex;

use super::object;
use super::{Core, Locked};
use crate::backend::Backend;
use crate::{Error, ObjectId, Storable, TierMem};

impl<B: Backend, M: Mutex<T = TierMem<B>>> Locked<'_, M> {
    pub(crate) fn list(&mut self) -> Vec<String> {
        self.lock(|core| core.directory.keys().cloned().collect())
    }

    pub(crate) fn get(&mut self, name: &str) -> Option<ObjectId> {
        self.lock(|core| core.directory.get(name).copied())
    }

    /// Caches `value` as a new modified object named `name`. `None` if it does not fit with the
    /// other modified objects.
    pub(crate) fn insert<T: Storable>(
        &mut self,
        name: &str,
        value: T,
    ) -> Result<Option<ObjectId>, Error<B::Error>> {
        self.lock(|core| {
            if core.directory.contains_key(name) {
                return Err(Error::AlreadyExists);
            }
            if !core.fits_modified(object::size(&value)) {
                return Ok(core.full());
            }
            let id = ObjectId(core.next_id);
            core.next_id += 1;
            core.directory.insert(name.to_string(), id);
            core.directory_modified = true;
            core.cache(id, value, true);
            core.drop_clean(id);
            Ok(Some(id))
        })
    }

    /// Removes the name of the object `id` and the object from RAM. If it is being written, the
    /// flush drops it once written (`write::write_back`).
    pub(crate) fn remove(&mut self, id: ObjectId) -> Result<(), Error<B::Error>> {
        self.lock(|core| {
            if !core.is_named(id) {
                return Err(Error::NotFound);
            }
            core.directory.retain(|_, &mut named| named != id);
            core.directory_modified = true;
            core.uncache(id);
            core.removed.push(id);
            Ok(())
        })
    }
}

impl<B: Backend> Core<B> {
    pub(super) fn is_named(&self, id: ObjectId) -> bool {
        self.directory.values().any(|&named| named == id)
    }
}
