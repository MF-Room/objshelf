//! Mount, which reads the names stored with the objects, and unmount.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use hashbrown::HashMap;

use super::Core;
use crate::backend::Backend;
use crate::{Error, ObjectId};

impl<B: Backend> Core<B> {
    /// Mounts the backend with `config`, blocking until done, and reads the names.
    pub(crate) fn mount(
        config: B::Config,
        cache_bytes: usize,
        on_full: fn(),
    ) -> Result<Self, Error<B::Error>> {
        let (backend, directory) = mount_backend::<B>(config).map_err(Error::Backend)?;
        // Above every stored id: a new object always has a higher id than the older ones.
        let next_id = directory.values().map(|id| id.0 + 1).max().unwrap_or(0);
        Ok(Self {
            usage: backend.usage(),
            backend: Some(backend),
            directory,
            removed: Vec::new(),
            next_id,
            cache: HashMap::new(),
            cache_bytes,
            used: 0,
            modified_bytes: 0,
            on_full,
            tick: 0,
        })
    }

    /// Unmounts the backend. The changes in RAM are dropped: flush them first.
    pub(crate) fn unmount(self) -> Result<(), Error<B::Error>> {
        let backend = self.backend.expect("no write runs: the memory is owned");
        backend.unmount().map_err(Error::Backend)
    }
}

/// Mounts the backend and reads the name of each object it holds. Two objects with the same name
/// are left by a flush cut after the write of a new object and before the removal of the old one
/// it replaces: the newer one, with the higher id, is kept and the other one removed.
fn mount_backend<B: Backend>(
    config: B::Config,
) -> Result<(B, BTreeMap<String, ObjectId>), B::Error> {
    let mut backend = B::mount(config)?;
    let mut ids = backend.ids();
    ids.sort_unstable();
    let mut directory = BTreeMap::new();
    let mut replaced = Vec::new();
    for id in ids {
        let name = backend.name(id)?;
        replaced.extend(directory.insert(name, id));
    }
    for id in replaced {
        backend.remove(id)?;
    }
    Ok((backend, directory))
}
