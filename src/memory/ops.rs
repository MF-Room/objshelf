//! Async functions: they lock the memory briefly, and wait for the runner between locks.

use core::future::poll_fn;

use rtic_core::Mutex;

use super::TierMem;
use crate::backend::Backend;
use crate::{Error, Handle, Storable};

/// Waits until the [`Runner`](crate::Runner) has mounted the memory. Returns
/// [`Error::Unavailable`] if the mount failed (the runner returns the error) or the memory is
/// unmounted.
pub async fn mounted<B, M>(mem: &mut M) -> Result<(), Error<B::Error>>
where
    B: Backend,
    M: Mutex<T = TierMem<B>>,
{
    poll_fn(|cx| mem.lock(|m| m.poll_mounted(cx.waker()))).await
}

/// Runs `f` on the object of `handle` and returns its result. If the object is not in RAM,
/// waits for the runner to load it. Waits for the mount too.
///
/// `f` runs inside the lock: keep it short.
pub async fn with<B, M, T, R>(
    mem: &mut M,
    handle: &Handle<T>,
    f: impl FnOnce(&T) -> R,
) -> Result<R, Error<B::Error>>
where
    B: Backend,
    M: Mutex<T = TierMem<B>>,
    T: Storable,
{
    let mut f = Some(f);
    poll_fn(|cx| mem.lock(|m| m.poll_with(handle.id, &mut f, Some(cx.waker())))).await
}

/// Like [`with`], with mutable access. The change is in RAM when this returns (see
/// [Durability](TierMem#durability)).
pub async fn with_mut<B, M, T, R>(
    mem: &mut M,
    handle: &Handle<T>,
    f: impl FnOnce(&mut T) -> R,
) -> Result<R, Error<B::Error>>
where
    B: Backend,
    M: Mutex<T = TierMem<B>>,
    T: Storable,
{
    let mut f = Some(f);
    poll_fn(|cx| mem.lock(|m| m.poll_with_mut(handle.id, &mut f, Some(cx.waker())))).await
}

/// Writes every object modified before this call to the backend, then the directory, then
/// applies the removals, and waits for it. Waits for the mount too.
///
/// On a backend error the other steps still run and the first error is returned. If several
/// tasks flush at the same time, one of them gets it.
pub async fn flush<B, M>(mem: &mut M) -> Result<(), Error<B::Error>>
where
    B: Backend,
    M: Mutex<T = TierMem<B>>,
{
    mounted(mem).await?;
    let generation = mem.lock(|m| {
        m.check_ready()?;
        Ok(m.queue_flush())
    })?;
    poll_fn(|cx| mem.lock(|m| m.poll_flush(generation, cx.waker()))).await
}

/// Flushes, then unmounts the backend; [`Runner::run`](crate::Runner::run) then returns. Every
/// other operation fails with [`Error::Unavailable`] from then on.
pub async fn unmount<B, M>(mem: &mut M) -> Result<(), Error<B::Error>>
where
    B: Backend,
    M: Mutex<T = TierMem<B>>,
{
    // Other tasks can modify objects during the flush: flush again until nothing is left.
    loop {
        flush(mem).await?;
        if mem.lock(|m| m.begin_unmount())? {
            break;
        }
    }
    poll_fn(|cx| mem.lock(|m| m.poll_unmounted(cx.waker()))).await
}
