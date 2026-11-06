//! The runner: the task that owns the backend and does all the storage I/O.

use alloc::vec::Vec;
use core::future::poll_fn;
use core::mem;

use hashbrown::HashSet;
use rtic_core::Mutex;

use super::{DIRECTORY, Directory, Job, TierMem};
use crate::backend::Backend;
use crate::{Error, ObjectId};

/// Does the storage I/O of a [`TierMem`]: run [`run`](Self::run) in a low-priority RTIC task,
/// with the runner as a `#[local]` resource of that task.
///
/// The runner mounts the backend, then waits for work: loads requested by `with*` calls, flushes,
/// and write-backs of the objects it evicts when the cache is over budget. It only locks the
/// memory to take a job and to store its result, never during I/O.
pub struct Runner<B: Backend> {
    state: State<B>,
}

enum State<B: Backend> {
    Config(B::Config),
    Mounted(B),
    Done,
}

impl<B: Backend> Runner<B> {
    /// Runner that mounts the backend with `config`.
    pub fn new(config: B::Config) -> Self {
        Self {
            state: State::Config(config),
        }
    }

    /// Mounts the backend, then serves `mem` until it is unmounted with
    /// [`unmount`](crate::unmount).
    ///
    /// Returns the mount error, if any: the memory is then unavailable. Later errors go to the
    /// calls that caused them. Runs once: a second call returns [`Error::Unavailable`]. The
    /// future must not be dropped before it returns, as the backend's own futures.
    pub async fn run<M>(&mut self, mem: &mut M) -> Result<(), Error<B::Error>>
    where
        M: Mutex<T = TierMem<B>>,
    {
        let config = match mem::replace(&mut self.state, State::Done) {
            State::Config(config) => config,
            state => {
                self.state = state;
                return Err(Error::Unavailable);
            }
        };
        let backend = match mount::<B>(config).await {
            Ok((backend, directory, stored)) => {
                let usage = backend.usage();
                mem.lock(|m| m.set_mounted(directory, stored, usage));
                self.state.insert(backend)
            }
            Err(error) => {
                mem.lock(|m| m.set_failed());
                return Err(Error::Backend(error));
            }
        };

        loop {
            let job = poll_fn(|cx| mem.lock(|m| m.poll_job(cx.waker()))).await;
            match job {
                Job::Load { id, load } => {
                    let result = load(backend, id).await;
                    let usage = backend.usage();
                    mem.lock(|m| m.finish_load(id, result, usage));
                }
                Job::Write { id, entry, evict } => {
                    let result = entry.value.write(backend, id).await;
                    let usage = backend.usage();
                    mem.lock(|m| m.finish_write(id, entry, evict, result, usage));
                }
                Job::Directory {
                    version,
                    complete,
                    entries,
                } => {
                    let result = backend.write(DIRECTORY, &entries).await;
                    let usage = backend.usage();
                    mem.lock(|m| m.finish_directory(version, complete, result, usage));
                }
                Job::Remove { id, version } => {
                    let result = backend.remove(id).await;
                    let usage = backend.usage();
                    mem.lock(|m| m.finish_remove(id, version, result, usage));
                }
                Job::Unmount => {
                    let State::Mounted(backend) = mem::replace(&mut self.state, State::Done)
                    else {
                        unreachable!();
                    };
                    let result = backend.unmount().await;
                    mem.lock(|m| m.finish_unmount(result));
                    return Ok(());
                }
            }
        }
    }

    /// The backend, if mounted and not unmounted: to inspect the storage after the run future
    /// was dropped.
    #[cfg(test)]
    pub(crate) fn into_backend(self) -> Option<B> {
        match self.state {
            State::Mounted(backend) => Some(backend),
            _ => None,
        }
    }
}

impl<B: Backend> State<B> {
    fn insert(&mut self, backend: B) -> &mut B {
        *self = State::Mounted(backend);
        let State::Mounted(backend) = self else {
            unreachable!();
        };
        backend
    }
}

/// Mounts the backend, reads the directory and removes the objects it doesn't name, left by a
/// write or a removal cut by a reset. Returns the directory and the ids of the stored objects.
async fn mount<B: Backend>(config: B::Config) -> Result<(B, Directory, Vec<ObjectId>), B::Error> {
    let mut backend = B::mount(config).await?;
    let ids = backend.ids();
    let directory: Directory = if ids.contains(&DIRECTORY) {
        backend.read(DIRECTORY).await?
    } else {
        Directory::new()
    };
    let named: HashSet<_> = directory.iter().map(|&(_, id)| id).collect();
    let mut stored = Vec::new();
    for id in ids {
        if id == DIRECTORY {
            continue;
        }
        if named.contains(&id) {
            stored.push(id);
        } else {
            backend.remove(id).await?;
        }
    }
    Ok((backend, directory, stored))
}
