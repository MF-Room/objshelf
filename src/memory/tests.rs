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
use crate::{
    Error, Handle, Mutex, TierMem, flush, get, get_or_insert_with, insert, list, remove, try_with,
    try_with_mut, usage, with, with_mut,
};

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
    let add = |i: u8| {
        let blob = insert(mem, &i.to_string(), Blob(vec![i; 100])).unwrap();
        flush(mem).unwrap();
        blob
    };
    (0..4).map(add).collect()
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
    let settings = insert(&mut mem, "settings", value).unwrap();
    assert_eq!(with(&mut mem, &settings, |s| s.brightness).unwrap(), 3);
    with_mut(&mut mem, &settings, |s| s.brightness = 4).unwrap();
    assert_eq!(
        try_with(&mut mem, &settings, |s| s.brightness).unwrap(),
        Some(4)
    );
    let pushed = try_with_mut(&mut mem, &settings, |s| s.name.push('b'));
    assert_eq!(pushed.unwrap(), Some(()));
    let expected = Settings {
        brightness: 4,
        name: "ab".to_string(),
    };
    assert_eq!(
        with(&mut mem, &settings, Settings::clone).unwrap(),
        expected
    );

    let again = insert(&mut mem, "settings", Settings::default());
    assert!(matches!(again, Err(Error::AlreadyExists)));
    assert!(matches!(
        get::<Settings, _>(&mut mem, "other"),
        Err(Error::NotFound)
    ));
    let found: Handle<Settings> = get(&mut mem, "settings").unwrap();
    assert_eq!(found.id, settings.id);

    // `f` only runs for a new object.
    let same = get_or_insert_with(&mut mem, "settings", || -> Settings { panic!("it exists") });
    assert_eq!(same.unwrap().id, settings.id);
    let blob = get_or_insert_with(&mut mem, "blob", || Blob(vec![1, 2])).unwrap();
    assert_eq!(with(&mut mem, &blob, |b| b.0.len()).unwrap(), 2);
    assert_eq!(list(&mut mem), ["blob", "settings"]);
}

#[test]
fn wrong_type() {
    let mut mem = mount(Flash::new(8), CACHE);
    insert(&mut mem, "settings", Settings::default()).unwrap();
    let wrong: Handle<Blob> = get(&mut mem, "settings").unwrap();
    assert!(matches!(
        with(&mut mem, &wrong, |b| b.0.len()),
        Err(Error::WrongType)
    ));
    let tried = try_with(&mut mem, &wrong, |b| b.0.len());
    assert!(matches!(tried, Err(Error::WrongType)));
}

#[test]
fn insert_and_remove_reach_the_flash_on_flush() {
    let mut mem = mount(Flash::new(8), CACHE);
    insert(&mut mem, "a", Blob(vec![1])).unwrap();
    let mut mem = mount(power_cut(mem), CACHE);
    assert!(list(&mut mem).is_empty(), "lost without a flush");

    let a = insert(&mut mem, "a", Blob(vec![1])).unwrap();
    insert(&mut mem, "b", Blob(vec![2])).unwrap();
    flush(&mut mem).unwrap();
    remove(&mut mem, a).unwrap();
    assert!(matches!(remove(&mut mem, a), Err(Error::NotFound)));
    assert!(matches!(with(&mut mem, &a, |_| ()), Err(Error::NotFound)));
    let mut mem = mount(power_cut(mem), CACHE);
    assert_eq!(list(&mut mem), ["a", "b"], "lost without a flush");

    let a: Handle<Blob> = get(&mut mem, "a").unwrap();
    remove(&mut mem, a).unwrap();
    // Never stored: nothing to remove from the flash.
    let c = insert(&mut mem, "c", 3u32).unwrap();
    remove(&mut mem, c).unwrap();
    flush(&mut mem).unwrap();
    let mut mem = mount(power_cut(mem), CACHE);
    assert_eq!(list(&mut mem), ["b"]);
    let b: Handle<Blob> = get(&mut mem, "b").unwrap();
    assert_eq!(with(&mut mem, &b, |b| b.0.clone()).unwrap(), [2]);
    // The directory and `b`.
    assert_eq!(usage(&mut mem).objects, 2);
}

#[test]
fn changes_reach_the_flash_on_flush() {
    let mut mem = mount(Flash::new(8), CACHE);
    let a = insert(&mut mem, "a", 1u32).unwrap();
    flush(&mut mem).unwrap();
    with_mut(&mut mem, &a, |a| *a = 2).unwrap();
    let mut mem = mount(power_cut(mem), CACHE);
    assert_eq!(
        with(&mut mem, &a, |a| *a).unwrap(),
        1,
        "lost without a flush"
    );

    with_mut(&mut mem, &a, |a| *a = 3).unwrap();
    flush(&mut mem).unwrap();
    assert!(!mem.core.cache[&a.id].modified);
    let mut mem = mount(power_cut(mem), CACHE);
    assert_eq!(with(&mut mem, &a, |a| *a).unwrap(), 3);
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
        assert_eq!(with(&mut mem, blob, |b| b.0[0]).unwrap(), i);
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
    with_mut(&mut mem, &blobs[0], |b| b.0[0] = 10).unwrap();
    with_mut(&mut mem, &blobs[1], |b| b.0[0] = 11).unwrap();

    // A third modified blob does not fit: nothing changes.
    let mut ran = false;
    let full = with_mut(&mut mem, &blobs[2], |_| ran = true);
    assert!(matches!(full, Err(Error::Full)));
    assert!(!ran);
    let full = try_with_mut(&mut mem, &blobs[2], |_| ());
    assert!(matches!(full, Err(Error::Full)));
    let full = insert(&mut mem, "new", Blob(vec![0; 100]));
    assert!(matches!(full, Err(Error::Full)));
    assert!(matches!(
        get::<Blob, _>(&mut mem, "new"),
        Err(Error::NotFound)
    ));
    assert_eq!(FULL.load(Ordering::Relaxed), 3);

    // Reads and the modified objects still work.
    assert_eq!(with(&mut mem, &blobs[2], |b| b.0[0]).unwrap(), 2);
    with_mut(&mut mem, &blobs[0], |b| b.0.push(0)).unwrap();

    flush(&mut mem).unwrap();
    assert_eq!(mem.core.modified_bytes, 0);
    with_mut(&mut mem, &blobs[2], |b| b.0[0] = 12).unwrap();
    let mut mem = mount(power_cut(mem), 300);
    let firsts: Vec<u8> = blobs
        .iter()
        .map(|blob| with(&mut mem, blob, |b| b.0[0]).unwrap())
        .collect();
    assert_eq!(firsts, [10, 11, 2, 3]);
}

#[test]
fn an_object_over_the_budget_stays_readable() {
    let mut mem = mount(Flash::new(8), CACHE);
    let big = insert(&mut mem, "big", Blob(vec![7; 200])).unwrap();
    flush(&mut mem).unwrap();
    let mut mem = mount(power_cut(mem), 50);
    for _ in 0..3 {
        assert_eq!(with(&mut mem, &big, |b| b.0.len()).unwrap(), 200);
        assert_eq!(try_with(&mut mem, &big, |b| b.0.len()).unwrap(), Some(200));
        assert!(is_cached(&mem, &big));
    }
    // But it cannot be modified.
    assert!(matches!(with_mut(&mut mem, &big, |_| ()), Err(Error::Full)));
}

#[test]
fn a_failed_flush_keeps_the_changes() {
    let mut mem = mount(Flash::new(8), CACHE);
    let a = insert(&mut mem, "a", 1u32).unwrap();
    let b = insert(&mut mem, "b", 2u32).unwrap();
    flush(&mut mem).unwrap();
    with_mut(&mut mem, &a, |a| *a = 10).unwrap();
    remove(&mut mem, b).unwrap();
    let flash = mem.core.backend.as_mut().unwrap().flash_mut();
    flash.power_cut();

    assert!(matches!(flush(&mut mem), Err(Error::Backend(_))));
    assert!(mem.core.cache[&a.id].modified);
    assert_eq!(mem.core.modified_bytes, mem.core.cache[&a.id].size);
    assert_eq!(with(&mut mem, &a, |a| *a).unwrap(), 10);
    assert!(mem.core.directory_modified);
    assert_eq!(mem.core.removed, [b.id]);
}

#[test]
fn try_with_reads_an_object_not_in_ram() {
    let mut mem = mount(Flash::new(8), CACHE);
    let a = insert(&mut mem, "a", 5u32).unwrap();
    flush(&mut mem).unwrap();
    mem.core.uncache(a.id);
    assert_eq!(try_with(&mut mem, &a, |a| *a).unwrap(), Some(5));
    assert!(is_cached(&mem, &a));
    assert_eq!(try_with_mut(&mut mem, &a, |a| *a += 1).unwrap(), Some(()));
    assert_eq!(with(&mut mem, &a, |a| *a).unwrap(), 6);
}

#[test]
fn try_with_returns_none_only_while_a_write_has_the_storage() {
    let mut mem = mount(Flash::new(8), CACHE);
    let cached = insert(&mut mem, "cached", 1u32).unwrap();
    let uncached = insert(&mut mem, "uncached", 2u32).unwrap();
    let written = insert(&mut mem, "written", 3u32).unwrap();
    flush(&mut mem).unwrap();
    mem.core.uncache(uncached.id);
    with_mut(&mut mem, &written, |w| *w = 30).unwrap();
    let mut mem = share(mem);

    // A write inside the lock would make the interrupt's lock panic.
    let seen = Arc::new(StdMutex::new(Vec::new()));
    let log = seen.clone();
    on_write(mem, move |mem| {
        log.lock().unwrap().push((
            try_with(mem, &cached, |v| *v).unwrap(),
            try_with(mem, &uncached, |v| *v).unwrap(),
            try_with_mut(mem, &written, |v| *v += 1).unwrap(),
            usage(mem).objects,
        ));
    });
    flush(&mut mem).unwrap();
    let seen = seen.lock().unwrap();
    assert!(!seen.is_empty());
    assert!(
        seen.iter().all(|&s| s == (Some(1), None, None, 4)),
        "{seen:?}"
    );

    // The storage is free again.
    assert_eq!(try_with(&mut mem, &uncached, |v| *v).unwrap(), Some(2));
    assert_eq!(with(&mut mem, &written, |v| *v).unwrap(), 30);
}

#[test]
fn flush_ends_while_an_interrupt_keeps_modifying() {
    let mut mem = mount(Flash::new(8), CACHE);
    let a = insert(&mut mem, "a", 0u32).unwrap();
    let b = insert(&mut mem, "b", 0u32).unwrap();
    flush(&mut mem).unwrap();
    with_mut(&mut mem, &a, |v| *v = 1).unwrap();
    with_mut(&mut mem, &b, |v| *v = 1).unwrap();
    let mut mem = share(mem);
    on_write(mem, move |mem| {
        // The object being written is out of RAM: only the other one changes.
        try_with_mut(mem, &a, |v| *v += 1).unwrap();
        try_with_mut(mem, &b, |v| *v += 1).unwrap();
    });
    flush(&mut mem).unwrap();
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
    let a = insert(&mut mem, "a", 1u32).unwrap();
    insert(&mut mem, "b", 2u32).unwrap();
    flush(&mut mem).unwrap();
    with_mut(&mut mem, &a, |v| *v = 10).unwrap();
    let mut mem = share(mem);
    // Removes `a` during its write. `remove` never waits, so it works from an interrupt.
    on_write(mem, move |mem| {
        let _ = remove(mem, a);
    });
    flush(&mut mem).unwrap();
    assert_eq!(list(&mut mem), ["b"]);
    let flash = mem.lock(|m| {
        assert!(!is_cached(m, &a));
        assert_eq!(m.core.modified_bytes, 0);
        let backend = m.core.backend.take().unwrap();
        Flash::with_data(backend.into_flash().data)
    });
    // Its removal was written in the same flush.
    let mut mem = mount(flash, CACHE);
    assert_eq!(list(&mut mem), ["b"]);
    assert_eq!(usage(&mut mem).objects, 2);
}

#[test]
fn power_cut_during_flush() {
    let mut mem = mount(Flash::new(8), CACHE);
    insert(&mut mem, "x", 1u32).unwrap();
    insert(&mut mem, "y", 2u32).unwrap();
    flush(&mut mem).unwrap();
    let flash = power_cut(mem);
    for op in 0.. {
        let cut = Flash::with_data(flash.data.clone()).cut_at(op, Cut::FirstHalf);
        let mut mem = mount(cut, CACHE);
        insert(&mut mem, "a", 3u32).unwrap();
        let y: Handle<u32> = get(&mut mem, "y").unwrap();
        remove(&mut mem, y).unwrap();
        let done = flush(&mut mem).is_ok();

        // Before or after, never in between: the mount removed the objects without a name.
        let mut mem = mount(power_cut(mem), CACHE);
        let names = list(&mut mem);
        let (name, value) = if names == ["a", "x"] {
            ("a", 3)
        } else {
            assert_eq!(names, ["x", "y"], "cut at {op}");
            ("y", 2)
        };
        let handle: Handle<u32> = get(&mut mem, name).unwrap();
        assert_eq!(
            with(&mut mem, &handle, |v| *v).unwrap(),
            value,
            "cut at {op}"
        );
        assert_eq!(usage(&mut mem).objects, 3, "cut at {op}");
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
