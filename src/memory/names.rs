//! The names of the objects, `insert` and `remove`. They change RAM only, inside the lock, and
//! never wait: `flush` writes the objects, each with its name, then removes the removed objects
//! (`write.rs`).

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use rtic_core::Mutex;

use super::object;
use super::{Core, Locked};
use crate::backend::Backend;
use crate::{Error, MAX_NAME_LEN, ObjShelf, ObjectId, Storable};

impl<B: Backend, M: Mutex<T = ObjShelf<B>>> Locked<'_, M> {
    pub(crate) fn list(&mut self) -> Vec<String> {
        self.lock(|core| core.directory.keys().cloned().collect())
    }

    pub(crate) fn get(&mut self, name: &str) -> Option<ObjectId> {
        self.lock(|core| core.directory.get(name).copied())
    }

    /// Caches `value` as a new modified object named `name`. `None` if it does not fit with the
    /// other modified objects, `TooBig` if it never fits.
    pub(crate) fn insert<T: Storable>(
        &mut self,
        name: &str,
        value: T,
    ) -> Result<Option<ObjectId>, Error<B::Error>> {
        if name.len() > MAX_NAME_LEN {
            return Err(Error::NameTooLong);
        }
        self.lock(|core| {
            if core.directory.contains_key(name) {
                return Err(Error::AlreadyExists);
            }
            let size = object::size::<B, _>(&value);
            if size > core.cache_bytes {
                return Err(Error::TooBig);
            }
            if !core.fits_modified(size) {
                return Ok(core.full());
            }
            let id = ObjectId(core.next_id);
            core.next_id += 1;
            core.directory.insert(name.to_string(), id);
            core.cache(id, value, true);
            core.drop_clean(id);
            Ok(Some(id))
        })
    }

    /// Removes the name of the object `id` and the object from RAM. If it is being written, the
    /// flush drops it once written (`write::write_back`).
    pub(crate) fn remove(&mut self, id: ObjectId) -> Result<(), Error<B::Error>> {
        self.lock(|core| {
            let name = core.name_of(id).ok_or(Error::NotFound)?.to_string();
            core.directory.remove(&name);
            core.uncache(id);
            core.removed.push((id, name));
            Ok(())
        })
    }
}

impl<B: Backend> Core<B> {
    /// Name of the object `id`, `None` if it is removed.
    pub(super) fn name_of(&self, id: ObjectId) -> Option<&str> {
        let mut names = self.directory.iter();
        let (name, _) = names.find(|&(_, &named)| named == id)?;
        Some(name)
    }

    pub(super) fn is_named(&self, id: ObjectId) -> bool {
        self.name_of(id).is_some()
    }
}
