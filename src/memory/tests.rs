use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex as StdMutex;

use serde::{Deserialize, Serialize};

use crate::backend::Backend;
use crate::backend::nor::mock::{Cut, MockFlash};
use crate::backend::nor::{NorBackend, NorConfig, NorError};
use crate::{Error, Handle, Mutex, TierMem, TierMemExt};

type Flash = MockFlash<1024>;
type Mem = TierMem<NorBackend<Flash>>;

const CACHE: usize = 4096;

fn config(flash: Flash) -> NorConfig<Flash> {
    let range = 0..flash.data.len() as u32;
    NorConfig { flash, range }
}

fn mount(flash: Flash, cache_bytes: usize) -> Mem {
    Mem::mount(config(flash), cache_bytes, || {}).unwrap()
}

/// Drops the memory as on a power cut, and returns the flash after a reboot.
fn power_cut(mem: Mem) -> Flash {
    let backend = mem.core.backend.expect("a write has the backend");
    Flash::with_data(backend.into_flash().data)
}

/// A memory shared like an RTIC resource, for the whole program. Its lock panics if it is
/// already held, as when an "interrupt" runs while a write wrongly holds it.
#[derive(Clone, Copy)]
struct Shared(&'static StdMutex<Mem>);

impl Mutex for Shared {
    type T = Mem;

    fn lock<R>(&mut self, f: impl FnOnce(&mut Mem) -> R) -> R {
        f(&mut self.0.try_lock().expect("the lock is already held"))
    }
}

fn share(mem: Mem) -> Shared {
    Shared(Box::leak(Box::new(StdMutex::new(mem))))
}

/// Runs `interrupt` at the start of each program operation of the flash, as an interrupt that
/// preempts a write.
fn on_write(mem: Shared, mut interrupt: impl FnMut(&mut Shared) + Send + 'static) {
    let mut lock = mem;
    lock.lock(|m| {
        let flash = m.core.backend.as_mut().unwrap().flash_mut();
        flash.on_write = Some(Box::new(move || interrupt(&mut { mem })));
    });
}

fn is_cached<T>(mem: &Mem, handle: &Handle<T>) -> bool {
    mem.core.cache.contains_key(&handle.id)
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
struct Settings {
    brightness: u8,
    name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Blob(Vec<u8>);

/// Inserts four flushed 100-byte blobs, which count 125 bytes each: two fit in 300 bytes.
fn blobs(mem: &mut Mem) -> Vec<Handle<Blob>> {
    let insert = |i: u8| {
        let blob = mem.insert(&i.to_string(), Blob(vec![i; 100])).unwrap();
        mem.flush().unwrap();
        blob
    };
    (0..4).map(insert).collect()
}

#[test]
fn the_memory_can_be_an_rtic_resource() {
    fn resource<T: Send>() {}
    resource::<Mem>();
}

#[test]
fn insert_get_and_access() {
    let mut mem = mount(Flash::new(8), CACHE);
    let value = Settings {
        brightness: 3,
        name: "a".to_string(),
    };
    let settings = mem.insert("settings", value).unwrap();
    assert_eq!(mem.with(&settings, |s| s.brightness).unwrap(), 3);
    mem.with_mut(&settings, |s| s.brightness = 4).unwrap();
    assert_eq!(mem.try_with(&settings, |s| s.brightness).unwrap(), Some(4));
    let pushed = mem.try_with_mut(&settings, |s| s.name.push('b'));
    assert_eq!(pushed.unwrap(), Some(()));
    let expected = Settings {
        brightness: 4,
        name: "ab".to_string(),
    };
    assert_eq!(mem.with(&settings, Settings::clone).unwrap(), expected);

    let again = mem.insert("settings", Settings::default());
    assert!(matches!(again, Err(Error::AlreadyExists)));
    assert!(matches!(mem.get::<Settings>("other"), Err(Error::NotFound)));
    let found: Handle<Settings> = mem.get("settings").unwrap();
    assert_eq!(found.id, settings.id);

    // `f` only runs for a new object.
    let same = mem.get_or_insert_with("settings", || -> Settings { panic!("it exists") });
    assert_eq!(same.unwrap().id, settings.id);
    let blob = mem.get_or_insert_with("blob", || Blob(vec![1, 2])).unwrap();
    assert_eq!(mem.with(&blob, |b| b.0.len()).unwrap(), 2);
    assert_eq!(mem.list().unwrap(), ["blob", "settings"]);
}

#[test]
fn wrong_type() {
    let mut mem = mount(Flash::new(8), CACHE);
    mem.insert("settings", Settings::default()).unwrap();
    let wrong: Handle<Blob> = mem.get("settings").unwrap();
    assert!(matches!(
        mem.with(&wrong, |b| b.0.len()),
        Err(Error::WrongType)
    ));
    let tried = mem.try_with(&wrong, |b| b.0.len());
    assert!(matches!(tried, Err(Error::WrongType)));
}

#[test]
fn insert_and_remove_reach_the_flash_on_flush() {
    let mut mem = mount(Flash::new(8), CACHE);
    mem.insert("a", Blob(vec![1])).unwrap();
    let mut mem = mount(power_cut(mem), CACHE);
    assert!(mem.list().unwrap().is_empty(), "lost without a flush");

    let a = mem.insert("a", Blob(vec![1])).unwrap();
    mem.insert("b", Blob(vec![2])).unwrap();
    mem.flush().unwrap();
    mem.remove(a).unwrap();
    assert!(matches!(mem.remove(a), Err(Error::NotFound)));
    assert!(matches!(mem.with(&a, |_| ()), Err(Error::NotFound)));
    let mut mem = mount(power_cut(mem), CACHE);
    assert_eq!(mem.list().unwrap(), ["a", "b"], "lost without a flush");

    let a: Handle<Blob> = mem.get("a").unwrap();
    mem.remove(a).unwrap();
    // Never stored: nothing to remove from the flash.
    let c = mem.insert("c", 3u32).unwrap();
    mem.remove(c).unwrap();
    mem.flush().unwrap();
    let mut mem = mount(power_cut(mem), CACHE);
    assert_eq!(mem.list().unwrap(), ["b"]);
    let b: Handle<Blob> = mem.get("b").unwrap();
    assert_eq!(mem.with(&b, |b| b.0.clone()).unwrap(), [2]);
    // The directory and `b`.
    assert_eq!(mem.usage().unwrap().objects, 2);
}

#[test]
fn changes_reach_the_flash_on_flush() {
    let mut mem = mount(Flash::new(8), CACHE);
    let a = mem.insert("a", 1u32).unwrap();
    mem.flush().unwrap();
    mem.with_mut(&a, |a| *a = 2).unwrap();
    let mut mem = mount(power_cut(mem), CACHE);
    assert_eq!(mem.with(&a, |a| *a).unwrap(), 1, "lost without a flush");

    mem.with_mut(&a, |a| *a = 3).unwrap();
    mem.flush().unwrap();
    assert!(!mem.core.cache[&a.id].modified);
    let mut mem = mount(power_cut(mem), CACHE);
    assert_eq!(mem.with(&a, |a| *a).unwrap(), 3);
    mem.unmount().unwrap();
}

#[test]
fn unmodified_objects_leave_ram_over_budget() {
    let budget = 300;
    let mut mem = mount(Flash::new(16), budget);
    let blobs = blobs(&mut mem);
    let stored = |mem: &Mem| mem.core.backend.as_ref().unwrap().usage();
    let before = stored(&mem);
    for (i, blob) in (0..).zip(&blobs) {
        assert_eq!(mem.with(blob, |b| b.0[0]).unwrap(), i);
        assert!(mem.core.used <= budget);
    }
    assert_eq!(stored(&mem), before, "nothing written");
}

#[test]
fn full_until_flush() {
    static FULL: AtomicUsize = AtomicUsize::new(0);
    let on_full = || {
        FULL.fetch_add(1, Ordering::Relaxed);
    };
    let mut mem = Mem::mount(config(Flash::new(16)), 300, on_full).unwrap();
    let blobs = blobs(&mut mem);
    mem.with_mut(&blobs[0], |b| b.0[0] = 10).unwrap();
    mem.with_mut(&blobs[1], |b| b.0[0] = 11).unwrap();

    // A third modified blob does not fit: nothing changes.
    let mut ran = false;
    let full = mem.with_mut(&blobs[2], |_| ran = true);
    assert!(matches!(full, Err(Error::Full)));
    assert!(!ran);
    let full = mem.try_with_mut(&blobs[2], |_| ());
    assert!(matches!(full, Err(Error::Full)));
    let full = mem.insert("new", Blob(vec![0; 100]));
    assert!(matches!(full, Err(Error::Full)));
    assert!(matches!(mem.get::<Blob>("new"), Err(Error::NotFound)));
    assert_eq!(FULL.load(Ordering::Relaxed), 3);

    // Reads and the modified objects still work.
    assert_eq!(mem.with(&blobs[2], |b| b.0[0]).unwrap(), 2);
    mem.with_mut(&blobs[0], |b| b.0.push(0)).unwrap();

    mem.flush().unwrap();
    assert_eq!(mem.core.modified_bytes, 0);
    mem.with_mut(&blobs[2], |b| b.0[0] = 12).unwrap();
    let mut mem = mount(power_cut(mem), 300);
    let firsts: Vec<u8> = blobs
        .iter()
        .map(|blob| mem.with(blob, |b| b.0[0]).unwrap())
        .collect();
    assert_eq!(firsts, [10, 11, 2, 3]);
}

#[test]
fn an_object_over_the_budget_stays_readable() {
    let mut mem = mount(Flash::new(8), CACHE);
    let big = mem.insert("big", Blob(vec![7; 200])).unwrap();
    mem.flush().unwrap();
    let mut mem = mount(power_cut(mem), 50);
    for _ in 0..3 {
        assert_eq!(mem.with(&big, |b| b.0.len()).unwrap(), 200);
        assert_eq!(mem.try_with(&big, |b| b.0.len()).unwrap(), Some(200));
        assert!(is_cached(&mem, &big));
    }
    // But it cannot be modified.
    assert!(matches!(mem.with_mut(&big, |_| ()), Err(Error::Full)));
}

#[test]
fn a_failed_flush_keeps_the_changes() {
    let mut mem = mount(Flash::new(8), CACHE);
    let a = mem.insert("a", 1u32).unwrap();
    let b = mem.insert("b", 2u32).unwrap();
    mem.flush().unwrap();
    mem.with_mut(&a, |a| *a = 10).unwrap();
    mem.remove(b).unwrap();
    let flash = mem.core.backend.as_mut().unwrap().flash_mut();
    flash.power_cut();

    assert!(matches!(mem.flush(), Err(Error::Backend(_))));
    assert!(mem.core.cache[&a.id].modified);
    assert_eq!(mem.core.modified_bytes, mem.core.cache[&a.id].size);
    assert_eq!(mem.with(&a, |a| *a).unwrap(), 10);
    assert!(mem.core.directory_modified);
    assert_eq!(mem.core.removed, [b.id]);
}

#[test]
fn try_with_reads_an_object_not_in_ram() {
    let mut mem = mount(Flash::new(8), CACHE);
    let a = mem.insert("a", 5u32).unwrap();
    mem.flush().unwrap();
    mem.core.uncache(a.id);
    assert_eq!(mem.try_with(&a, |a| *a).unwrap(), Some(5));
    assert!(is_cached(&mem, &a));
    assert_eq!(mem.try_with_mut(&a, |a| *a += 1).unwrap(), Some(()));
    assert_eq!(mem.with(&a, |a| *a).unwrap(), 6);
}

#[test]
fn try_with_returns_none_only_while_a_write_has_the_storage() {
    let mut mem = mount(Flash::new(8), CACHE);
    let cached = mem.insert("cached", 1u32).unwrap();
    let uncached = mem.insert("uncached", 2u32).unwrap();
    let written = mem.insert("written", 3u32).unwrap();
    mem.flush().unwrap();
    mem.core.uncache(uncached.id);
    mem.with_mut(&written, |w| *w = 30).unwrap();
    let mut mem = share(mem);

    // A write inside the lock would make the interrupt's lock panic.
    let seen = Arc::new(StdMutex::new(Vec::new()));
    let log = seen.clone();
    on_write(mem, move |mem| {
        log.lock().unwrap().push((
            mem.try_with(&cached, |v| *v).unwrap(),
            mem.try_with(&uncached, |v| *v).unwrap(),
            mem.try_with_mut(&written, |v| *v += 1).unwrap(),
            mem.usage().unwrap().objects,
        ));
    });
    mem.flush().unwrap();
    let seen = seen.lock().unwrap();
    assert!(!seen.is_empty());
    assert!(
        seen.iter().all(|&s| s == (Some(1), None, None, 4)),
        "{seen:?}"
    );

    // The storage is free again.
    assert_eq!(mem.try_with(&uncached, |v| *v).unwrap(), Some(2));
    assert_eq!(mem.with(&written, |v| *v).unwrap(), 30);
}

#[test]
fn flush_ends_while_an_interrupt_keeps_modifying() {
    let mut mem = mount(Flash::new(8), CACHE);
    let a = mem.insert("a", 0u32).unwrap();
    let b = mem.insert("b", 0u32).unwrap();
    mem.flush().unwrap();
    mem.with_mut(&a, |v| *v = 1).unwrap();
    mem.with_mut(&b, |v| *v = 1).unwrap();
    let mut mem = share(mem);
    on_write(mem, move |mem| {
        // The object being written is out of RAM: only the other one changes.
        mem.try_with_mut(&a, |v| *v += 1).unwrap();
        mem.try_with_mut(&b, |v| *v += 1).unwrap();
    });
    mem.flush().unwrap();
    // The first one written was modified again during the write of the second.
    let modified = mem.lock(|m| {
        let modified = m.core.cache.values().filter(|entry| entry.modified);
        modified.count()
    });
    assert_eq!(modified, 1);
}

#[test]
fn remove_during_flush() {
    let mut mem = mount(Flash::new(8), CACHE);
    let a = mem.insert("a", 1u32).unwrap();
    mem.insert("b", 2u32).unwrap();
    mem.flush().unwrap();
    mem.with_mut(&a, |v| *v = 10).unwrap();
    let mut mem = share(mem);
    // Removes `a` during its write. `remove` never waits, so it works from an interrupt.
    on_write(mem, move |mem| {
        let _ = mem.remove(a);
    });
    mem.flush().unwrap();
    assert_eq!(mem.list().unwrap(), ["b"]);
    let flash = mem.lock(|m| {
        assert!(!is_cached(m, &a));
        assert_eq!(m.core.modified_bytes, 0);
        let backend = m.core.backend.take().unwrap();
        Flash::with_data(backend.into_flash().data)
    });
    // Its removal was written in the same flush.
    let mut mem = mount(flash, CACHE);
    assert_eq!(mem.list().unwrap(), ["b"]);
    assert_eq!(mem.usage().unwrap().objects, 2);
}

#[test]
fn power_cut_during_flush() {
    let mut mem = mount(Flash::new(8), CACHE);
    mem.insert("x", 1u32).unwrap();
    mem.insert("y", 2u32).unwrap();
    mem.flush().unwrap();
    let flash = power_cut(mem);
    for op in 0.. {
        let cut = Flash::with_data(flash.data.clone()).cut_at(op, Cut::FirstHalf);
        let mut mem = mount(cut, CACHE);
        mem.insert("a", 3u32).unwrap();
        let y: Handle<u32> = mem.get("y").unwrap();
        mem.remove(y).unwrap();
        let done = mem.flush().is_ok();

        // Before or after, never in between: the mount removed the objects without a name.
        let mut mem = mount(power_cut(mem), CACHE);
        let names = mem.list().unwrap();
        let (name, value) = if names == ["a", "x"] {
            ("a", 3)
        } else {
            assert_eq!(names, ["x", "y"], "cut at {op}");
            ("y", 2)
        };
        let handle: Handle<u32> = mem.get(name).unwrap();
        assert_eq!(mem.with(&handle, |v| *v).unwrap(), value, "cut at {op}");
        assert_eq!(mem.usage().unwrap().objects, 3, "cut at {op}");
        if done {
            assert_eq!(names, ["a", "x"]);
            break;
        }
    }
}

#[test]
fn mount_returns_backend_errors() {
    let config = NorConfig {
        flash: Flash::new(4),
        range: 512..4096,
    };
    let mounted = Mem::mount(config, CACHE, || {});
    assert!(matches!(
        mounted,
        Err(Error::Backend(NorError::InvalidConfig))
    ));
}
