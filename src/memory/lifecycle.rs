//! Mount, with the name directory, and unmount.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use hashbrown::{HashMap, HashSet};

use super::Core;
use crate::backend::Backend;
use crate::{Error, ObjectId};

/// Object holding the name directory: the `(name, id)` pairs of the stored objects.
pub(super) const DIRECTORY: ObjectId = ObjectId(0);

/// The name directory, as stored in object `DIRECTORY`.
pub(super) type Directory = Vec<(String, ObjectId)>;

impl<B: Backend> Core<B> {
    /// Mounts the backend with `config`, blocking until done, and reads the directory.
    pub(crate) fn mount(
        config: B::Config,
        cache_bytes: usize,
        on_full: fn(),
    ) -> Result<Self, Error<B::Error>> {
        let (backend, directory) = mount_backend::<B>(config).map_err(Error::Backend)?;
        let next_id = directory.values().map(|id| id.0 + 1).max().unwrap_or(1);
        Ok(Self {
            usage: backend.usage(),
            backend: Some(backend),
            directory,
            directory_modified: false,
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

    /// The directory to store as object `DIRECTORY`.
    pub(super) fn stored_directory(&self) -> Directory {
        stored(&self.directory)
    }
}

fn stored(directory: &BTreeMap<String, ObjectId>) -> Directory {
    let pairs = directory.iter();
    pairs.map(|(name, &id)| (name.clone(), id)).collect()
}

/// Mounts the backend and reads the directory. Names of objects the backend does not hold are
/// dropped, and the objects without a name, left by an `insert` or a `remove` cut by a reset,
/// are removed.
fn mount_backend<B: Backend>(
    config: B::Config,
) -> Result<(B, BTreeMap<String, ObjectId>), B::Error> {
    let mut backend = B::mount(config)?;
    let ids: HashSet<ObjectId> = backend.ids().into_iter().collect();
    let names: Directory = if ids.contains(&DIRECTORY) {
        backend.read(DIRECTORY)?
    } else {
        Directory::new()
    };
    let directory: BTreeMap<String, ObjectId> = names
        .iter()
        .filter(|(_, id)| ids.contains(id))
        .cloned()
        .collect();
    if directory.len() != names.len() {
        // Objects lost by the backend (a corrupted sector): store the directory without them.
        backend.write(DIRECTORY, &stored(&directory))?;
    }
    let named: HashSet<ObjectId> = directory.values().copied().collect();
    for id in ids {
        if id != DIRECTORY && !named.contains(&id) {
            backend.remove(id)?;
        }
    }
    Ok((backend, directory))
}
