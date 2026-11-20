use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex as StdMutex;

use get_size2::GetSize;
use serde::{Deserialize, Serialize};

use super::object;
use crate::backend::Backend;
use crate::backend::nor::mock::{Cut, MockError, MockFlash};
use crate::backend::nor::{NorBackend, NorConfig, NorError};
use crate::{
    Error, Handle, MAX_NAME_LEN, Mutex, ObjShelf, ObjectId, Storable, flush, flush_object, get,
    get_or_insert_with, insert, list, remove, usage, wear, with, with_mut,
};

type Flash = MockFlash<1024>;
type Mem = ObjShelf<NorBackend<Flash>>;

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

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize, GetSize)]
struct Settings {
    brightness: u8,
    name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, GetSize)]
struct Blob(Vec<u8>);

/// RAM counted for `value` in the cache.
fn ram<T: Storable>(value: &T) -> usize {
    object::size::<NorBackend<Flash>, T>(value)
}

/// A budget that two of the blobs of `blobs` fit in, but not three.
fn two_blobs() -> usize {
    let blob = ram(&Blob(vec![0; 100]));
    2 * blob + blob / 2
}

/// Inserts four flushed 100-byte blobs.
fn blobs(mem: &mut Mem) -> Vec<Handle<Blob>> {
    let add = |i: u8| {
        let blob = insert(mem, &i.to_string(), Blob(vec![i; 100]))
            .unwrap()
            .unwrap();
        flush(mem).unwrap().unwrap();
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
    let settings = insert(&mut mem, "settings", value).unwrap().unwrap();
    assert_eq!(
        with(&mut mem, &settings, |s| s.brightness)
            .unwrap()
            .unwrap(),
        3
    );
    with_mut(&mut mem, &settings, |s| s.brightness = 4)
        .unwrap()
        .unwrap();
    assert_eq!(
        with(&mut mem, &settings, |s| s.brightness)
            .unwrap()
            .unwrap(),
        4
    );
    with_mut(&mut mem, &settings, |s| s.name.push('b'))
        .unwrap()
        .unwrap();
    let expected = Settings {
        brightness: 4,
        name: "ab".to_string(),
    };
    assert_eq!(
        with(&mut mem, &settings, Settings::clone).unwrap().unwrap(),
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
    assert_eq!(same.unwrap().unwrap().id, settings.id);
    let blob = get_or_insert_with(&mut mem, "blob", || Blob(vec![1, 2]))
        .unwrap()
        .unwrap();
    assert_eq!(with(&mut mem, &blob, |b| b.0.len()).unwrap().unwrap(), 2);
    assert_eq!(list(&mut mem), ["blob", "settings"]);
}

#[test]
fn names_are_at_most_max_name_len_bytes() {
    let mut mem = mount(Flash::new(8), CACHE);
    let longest = "n".repeat(MAX_NAME_LEN);
    insert(&mut mem, &longest, 1u32).unwrap().unwrap();
    // Counted in bytes: each `é` takes 2.
    let too_long = "é".repeat(MAX_NAME_LEN / 2 + 1);
    let inserted = insert(&mut mem, &too_long, 2u32);
    assert!(matches!(inserted, Err(Error::NameTooLong)));
    let inserted = get_or_insert_with(&mut mem, &too_long, || 2u32);
    assert!(matches!(inserted, Err(Error::NameTooLong)));
    flush(&mut mem).unwrap().unwrap();
    let mut mem = mount(power_cut(mem), CACHE);
    assert_eq!(list(&mut mem), [longest]);
}

#[test]
fn wrong_type() {
    let mut mem = mount(Flash::new(8), CACHE);
    insert(&mut mem, "settings", Settings::default())
        .unwrap()
        .unwrap();
    let wrong: Handle<Blob> = get(&mut mem, "settings").unwrap();
    assert!(matches!(
        with(&mut mem, &wrong, |b| b.0.len()),
        Err(Error::WrongType)
    ));
    let changed = with_mut(&mut mem, &wrong, |b| b.0.clear());
    assert!(matches!(changed, Err(Error::WrongType)));
}

/// After a reboot the object is not in RAM: the backend checks its type, so the handle used first
/// doesn't decide. Without the check, `Settings::default()` decodes as an empty `Blob`.
#[test]
fn wrong_type_after_a_reboot() {
    let mut mem = mount(Flash::new(8), CACHE);
    insert(&mut mem, "settings", Settings::default())
        .unwrap()
        .unwrap();
    flush(&mut mem).unwrap().unwrap();
    let mut mem = mount(power_cut(mem), CACHE);
    let wrong: Handle<Blob> = get(&mut mem, "settings").unwrap();
    assert!(matches!(
        with(&mut mem, &wrong, |b| b.0.len()),
        Err(Error::WrongType)
    ));
    assert!(!is_cached(&mem, &wrong));
    let right: Handle<Settings> = get(&mut mem, "settings").unwrap();
    let read = with(&mut mem, &right, |s| s.clone()).unwrap();
    assert_eq!(read, Some(Settings::default()));
    let changed = with_mut(&mut mem, &wrong, |b| b.0.clear());
    assert!(matches!(changed, Err(Error::WrongType)));
}

#[test]
fn insert_and_remove_reach_the_flash_on_flush() {
    let mut mem = mount(Flash::new(8), CACHE);
    insert(&mut mem, "a", Blob(vec![1])).unwrap().unwrap();
    let mut mem = mount(power_cut(mem), CACHE);
    assert!(list(&mut mem).is_empty(), "lost without a flush");

    let a = insert(&mut mem, "a", Blob(vec![1])).unwrap().unwrap();
    insert(&mut mem, "b", Blob(vec![2])).unwrap().unwrap();
    flush(&mut mem).unwrap().unwrap();
    remove(&mut mem, a).unwrap();
    assert!(matches!(remove(&mut mem, a), Err(Error::NotFound)));
    assert!(matches!(with(&mut mem, &a, |_| ()), Err(Error::NotFound)));
    let mut mem = mount(power_cut(mem), CACHE);
    assert_eq!(list(&mut mem), ["a", "b"], "lost without a flush");

    let a: Handle<Blob> = get(&mut mem, "a").unwrap();
    remove(&mut mem, a).unwrap();
    // Never stored: nothing to remove from the flash.
    let c = insert(&mut mem, "c", 3u32).unwrap().unwrap();
    remove(&mut mem, c).unwrap();
    flush(&mut mem).unwrap().unwrap();
    let mut mem = mount(power_cut(mem), CACHE);
    assert_eq!(list(&mut mem), ["b"]);
    let b: Handle<Blob> = get(&mut mem, "b").unwrap();
    assert_eq!(with(&mut mem, &b, |b| b.0.clone()).unwrap().unwrap(), [2]);
    assert_eq!(usage(&mut mem).objects, 1);
}

#[test]
fn changes_reach_the_flash_on_flush() {
    let mut mem = mount(Flash::new(8), CACHE);
    let a = insert(&mut mem, "a", 1u32).unwrap().unwrap();
    flush(&mut mem).unwrap().unwrap();
    with_mut(&mut mem, &a, |a| *a = 2).unwrap().unwrap();
    let mut mem = mount(power_cut(mem), CACHE);
    assert_eq!(
        with(&mut mem, &a, |a| *a).unwrap().unwrap(),
        1,
        "lost without a flush"
    );

    with_mut(&mut mem, &a, |a| *a = 3).unwrap().unwrap();
    flush(&mut mem).unwrap().unwrap();
    assert!(!mem.core.cache[&a.id].modified);
    let mut mem = mount(power_cut(mem), CACHE);
    assert_eq!(with(&mut mem, &a, |a| *a).unwrap().unwrap(), 3);
    mem.unmount().unwrap();
}

#[test]
fn wear_follows_the_flushes() {
    let mut mem = mount(Flash::new(4), CACHE);
    // A fresh flash: no sector erased yet.
    assert_eq!(wear(&mut mem).unwrap().total_erases, 0);
    let a = insert(&mut mem, "a", vec![0u8; 100]).unwrap().unwrap();
    let mut erases = 0;
    for round in 0..30 {
        with_mut(&mut mem, &a, |a| a[0] = round).unwrap().unwrap();
        flush(&mut mem).unwrap().unwrap();
        let worn = wear(&mut mem).unwrap();
        assert_eq!(worn.sectors, 4);
        assert!(worn.total_erases >= erases);
        erases = worn.total_erases;
    }
    // Each sector holds 3 pages: 30 writes erased sectors again.
    assert!(erases > 4, "{erases} erases");
    let mut mem = mount(power_cut(mem), CACHE);
    assert_eq!(wear(&mut mem).unwrap().total_erases, erases);
}

#[test]
fn flush_object_writes_only_that_object() {
    let mut mem = mount(Flash::new(8), CACHE);
    let a = insert(&mut mem, "a", 1u32).unwrap().unwrap();
    let b = insert(&mut mem, "b", 2u32).unwrap().unwrap();
    flush(&mut mem).unwrap().unwrap();
    with_mut(&mut mem, &a, |a| *a = 10).unwrap().unwrap();
    with_mut(&mut mem, &b, |b| *b = 20).unwrap().unwrap();
    flush_object(&mut mem, &a).unwrap().unwrap();
    assert!(!mem.core.cache[&a.id].modified);
    assert!(mem.core.cache[&b.id].modified);
    let mut mem = mount(power_cut(mem), CACHE);
    assert_eq!(with(&mut mem, &a, |a| *a).unwrap().unwrap(), 10);
    assert_eq!(
        with(&mut mem, &b, |b| *b).unwrap().unwrap(),
        2,
        "lost without a flush"
    );

    let stored = |mem: &Mem| mem.core.backend.as_ref().unwrap().usage();
    let before = stored(&mem);
    flush_object(&mut mem, &a).unwrap().unwrap();
    assert_eq!(stored(&mem), before, "unmodified: nothing written");
    remove(&mut mem, b).unwrap();
    assert!(matches!(flush_object(&mut mem, &b), Err(Error::NotFound)));
}

#[test]
fn a_new_object_flushed_alone_survives_a_power_cut() {
    let mut mem = mount(Flash::new(8), CACHE);
    let a = insert(&mut mem, "a", 1u32).unwrap().unwrap();
    insert(&mut mem, "b", 2u32).unwrap().unwrap();
    flush_object(&mut mem, &a).unwrap().unwrap();
    // `a` was stored with its name, `b` not at all.
    let mut mem = mount(power_cut(mem), CACHE);
    assert_eq!(list(&mut mem), ["a"]);
    assert_eq!(with(&mut mem, &a, |a| *a).unwrap().unwrap(), 1);
    assert_eq!(usage(&mut mem).objects, 1);

    let b = insert(&mut mem, "b", 2u32).unwrap().unwrap();
    insert(&mut mem, "c", 3u32).unwrap().unwrap();
    flush_object(&mut mem, &b).unwrap().unwrap();
    flush(&mut mem).unwrap().unwrap();
    let mut mem = mount(power_cut(mem), CACHE);
    assert_eq!(list(&mut mem), ["a", "b", "c"]);
    let c: Handle<u32> = get(&mut mem, "c").unwrap();
    assert_eq!(with(&mut mem, &c, |c| *c).unwrap().unwrap(), 3);
}

#[test]
fn ram_counts_spare_capacity() {
    let mut mem = mount(Flash::new(8), CACHE);
    let notes = insert(&mut mem, "notes", Vec::<u32>::new())
        .unwrap()
        .unwrap();
    let before = mem.core.used;
    let capacity = with_mut(&mut mem, &notes, |n| {
        n.reserve_exact(100);
        n.capacity()
    });
    let capacity = capacity.unwrap().unwrap();
    // Empty, but 4 bytes per element of capacity: a serialized estimate would count 0.
    assert_eq!(mem.core.used, before + capacity * 4);
}

#[test]
fn unmodified_objects_leave_ram_over_budget() {
    let budget = two_blobs();
    let mut mem = mount(Flash::new(16), budget);
    let blobs = blobs(&mut mem);
    let stored = |mem: &Mem| mem.core.backend.as_ref().unwrap().usage();
    let before = stored(&mem);
    for (i, blob) in (0..).zip(&blobs) {
        assert_eq!(with(&mut mem, blob, |b| b.0[0]).unwrap().unwrap(), i);
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
    let mut mem = Mem::mount(config(Flash::new(16)), two_blobs(), on_full).unwrap();
    let blobs = blobs(&mut mem);
    with_mut(&mut mem, &blobs[0], |b| b.0[0] = 10)
        .unwrap()
        .unwrap();
    with_mut(&mut mem, &blobs[1], |b| b.0[0] = 11)
        .unwrap()
        .unwrap();

    // A third modified blob does not fit: nothing changes.
    let mut ran = false;
    let full = with_mut(&mut mem, &blobs[2], |_| ran = true);
    assert!(matches!(full, Ok(None)));
    assert!(!ran);
    let full = insert(&mut mem, "new", Blob(vec![0; 100]));
    assert!(matches!(full, Ok(None)));
    assert!(matches!(
        get::<Blob, _>(&mut mem, "new"),
        Err(Error::NotFound)
    ));
    assert_eq!(FULL.load(Ordering::Relaxed), 2);

    // Reads and the modified objects still work.
    assert_eq!(with(&mut mem, &blobs[2], |b| b.0[0]).unwrap().unwrap(), 2);
    with_mut(&mut mem, &blobs[0], |b| b.0.push(0))
        .unwrap()
        .unwrap();

    flush(&mut mem).unwrap().unwrap();
    assert_eq!(mem.core.modified_bytes, 0);
    with_mut(&mut mem, &blobs[2], |b| b.0[0] = 12)
        .unwrap()
        .unwrap();
    let mut mem = mount(power_cut(mem), two_blobs());
    let firsts: Vec<u8> = blobs
        .iter()
        .map(|blob| with(&mut mem, blob, |b| b.0[0]).unwrap().unwrap())
        .collect();
    assert_eq!(firsts, [10, 11, 2, 3]);
}

#[test]
fn an_object_over_the_budget_stays_readable() {
    let mut mem = mount(Flash::new(8), CACHE);
    let big = insert(&mut mem, "big", Blob(vec![7; 200]))
        .unwrap()
        .unwrap();
    flush(&mut mem).unwrap().unwrap();
    // `big` alone is over this budget, an empty blob is not.
    let mut mem = mount(power_cut(mem), ram(&Blob(vec![0; 50])));
    for _ in 0..3 {
        assert_eq!(with(&mut mem, &big, |b| b.0.len()).unwrap().unwrap(), 200);
        assert!(is_cached(&mem, &big));
    }
    // But it can never become modified, as no flush can make room for it.
    let mut ran = false;
    let too_big = with_mut(&mut mem, &big, |_| ran = true);
    assert!(matches!(too_big, Err(Error::TooBig)));
    assert!(!ran);
    let too_big = insert(&mut mem, "other", Blob(vec![7; 200]));
    assert!(matches!(too_big, Err(Error::TooBig)));

    // An object can grow over the budget while modified, then it is too big.
    let small = insert(&mut mem, "small", Blob(vec![])).unwrap().unwrap();
    with_mut(&mut mem, &small, |b| b.0.resize(100, 0))
        .unwrap()
        .unwrap();
    flush(&mut mem).unwrap().unwrap();
    let too_big = with_mut(&mut mem, &small, |_| ());
    assert!(matches!(too_big, Err(Error::TooBig)));
}

#[test]
fn a_failed_flush_keeps_the_changes() {
    let mut mem = mount(Flash::new(8), CACHE);
    let a = insert(&mut mem, "a", 1u32).unwrap().unwrap();
    let b = insert(&mut mem, "b", 2u32).unwrap().unwrap();
    flush(&mut mem).unwrap().unwrap();
    with_mut(&mut mem, &a, |a| *a = 10).unwrap().unwrap();
    remove(&mut mem, b).unwrap();
    let flash = mem.core.backend.as_mut().unwrap().flash_mut();
    flash.power_cut();

    assert!(matches!(flush(&mut mem), Err(Error::Backend(_))));
    assert!(mem.core.cache[&a.id].modified);
    assert_eq!(mem.core.modified_bytes, mem.core.cache[&a.id].size);
    assert_eq!(with(&mut mem, &a, |a| *a).unwrap().unwrap(), 10);
    assert_eq!(mem.core.removed, [(b.id, "b".to_string())]);
}

#[test]
fn an_object_that_fails_to_write_does_not_block_the_others() {
    let mut mem = mount(Flash::new(8), 16 * 1024);
    let a = insert(&mut mem, "a", 1u32).unwrap().unwrap();
    flush(&mut mem).unwrap().unwrap();
    // Bigger than the flash: its write fails at every flush.
    let big = insert(&mut mem, "big", Blob(vec![0; 6000]))
        .unwrap()
        .unwrap();
    with_mut(&mut mem, &a, |a| *a = 10).unwrap().unwrap();
    let b = insert(&mut mem, "b", 2u32).unwrap().unwrap();
    let flushed = flush(&mut mem);
    assert!(matches!(flushed, Err(Error::Backend(NorError::NoSpace))));
    assert!(mem.core.cache[&big.id].modified);
    assert_eq!(mem.core.modified_bytes, mem.core.cache[&big.id].size);

    let mut mem = mount(power_cut(mem), CACHE);
    assert_eq!(list(&mut mem), ["a", "b"]);
    assert_eq!(with(&mut mem, &a, |a| *a).unwrap().unwrap(), 10);
    assert_eq!(with(&mut mem, &b, |b| *b).unwrap().unwrap(), 2);
    assert_eq!(usage(&mut mem).objects, 2);
}

#[test]
fn a_replaced_object_stays_until_its_successor_is_written() {
    let mut mem = mount(Flash::new(8), 16 * 1024);
    let old = insert(&mut mem, "a", Blob(vec![1])).unwrap().unwrap();
    flush(&mut mem).unwrap().unwrap();
    // The new `a` is bigger than the flash: its write fails, so the old one is not removed.
    remove(&mut mem, old).unwrap();
    let new = insert(&mut mem, "a", Blob(vec![2; 6000])).unwrap().unwrap();
    let flushed = flush(&mut mem);
    assert!(matches!(flushed, Err(Error::Backend(NorError::NoSpace))));
    assert_eq!(mem.core.removed, [(old.id, "a".to_string())]);
    let flash = mem.core.backend.as_mut().unwrap().flash_mut();
    let mut after_cut = mount(Flash::with_data(flash.data.clone()), CACHE);
    assert_eq!(
        contents_of::<Blob>(&mut after_cut),
        (vec![("a".into(), Blob(vec![1]))], 1)
    );

    // Once the new `a` is written, the old one goes.
    with_mut(&mut mem, &new, |b| b.0.truncate(1))
        .unwrap()
        .unwrap();
    flush(&mut mem).unwrap().unwrap();
    assert!(mem.core.removed.is_empty());
    let mut mem = mount(power_cut(mem), CACHE);
    assert_eq!(
        contents_of::<Blob>(&mut mem),
        (vec![("a".into(), Blob(vec![2]))], 1)
    );
}

#[test]
fn the_mount_keeps_the_newer_of_two_objects_with_the_same_name() {
    // As a flush cut between the write of a new `a` and the removal of the old one leaves it.
    let mut backend = NorBackend::mount(config(Flash::new(8))).unwrap();
    backend.write(ObjectId(3), "a", &1u32).unwrap();
    backend.write(ObjectId(7), "a", &2u32).unwrap();
    backend.write(ObjectId(5), "b", &3u32).unwrap();
    let mut mem = mount(Flash::with_data(backend.into_flash().data), CACHE);
    let expected = vec![("a".into(), 2), ("b".into(), 3)];
    assert_eq!(contents(&mut mem), (expected, 2));
    // A new object gets a higher id than every stored one.
    let c = insert(&mut mem, "c", 4u32).unwrap().unwrap();
    assert_eq!(c.id, ObjectId(8));
}

#[test]
fn with_reads_an_object_not_in_ram() {
    let mut mem = mount(Flash::new(8), CACHE);
    let a = insert(&mut mem, "a", 5u32).unwrap().unwrap();
    flush(&mut mem).unwrap().unwrap();
    mem.core.uncache(a.id);
    assert_eq!(with(&mut mem, &a, |a| *a).unwrap().unwrap(), 5);
    assert!(is_cached(&mem, &a));
    mem.core.uncache(a.id);
    with_mut(&mut mem, &a, |a| *a += 1).unwrap().unwrap();
    assert_eq!(with(&mut mem, &a, |a| *a).unwrap().unwrap(), 6);
}

#[test]
fn busy_only_while_a_flush_has_the_storage() {
    let mut mem = mount(Flash::new(8), CACHE);
    let cached = insert(&mut mem, "cached", 1u32).unwrap().unwrap();
    let uncached = insert(&mut mem, "uncached", 2u32).unwrap().unwrap();
    let written = insert(&mut mem, "written", 3u32).unwrap().unwrap();
    flush(&mut mem).unwrap().unwrap();
    mem.core.uncache(uncached.id);
    with_mut(&mut mem, &written, |w| *w = 30).unwrap().unwrap();
    let mut mem = share(mem);

    // A write inside the lock would make the interrupt's lock panic.
    let seen = Arc::new(StdMutex::new(Vec::new()));
    let log = seen.clone();
    on_write(mem, move |mem| {
        log.lock().unwrap().push((
            with(mem, &cached, |v| *v).unwrap(),
            with(mem, &uncached, |v| *v).unwrap(),
            with_mut(mem, &written, |v| *v += 1).unwrap(),
            usage(mem).objects,
        ));
    });
    flush(&mut mem).unwrap().unwrap();
    let seen = seen.lock().unwrap();
    assert!(!seen.is_empty());
    assert!(
        seen.iter().all(|&s| s == (Some(1), None, None, 3)),
        "{seen:?}"
    );

    // The storage is free again.
    assert_eq!(with(&mut mem, &uncached, |v| *v).unwrap().unwrap(), 2);
    assert_eq!(with(&mut mem, &written, |v| *v).unwrap().unwrap(), 30);
}

#[test]
fn flush_ends_while_an_interrupt_keeps_modifying() {
    let mut mem = mount(Flash::new(8), CACHE);
    let a = insert(&mut mem, "a", 0u32).unwrap().unwrap();
    let b = insert(&mut mem, "b", 0u32).unwrap().unwrap();
    flush(&mut mem).unwrap().unwrap();
    with_mut(&mut mem, &a, |v| *v = 1).unwrap().unwrap();
    with_mut(&mut mem, &b, |v| *v = 1).unwrap().unwrap();
    let mut mem = share(mem);
    on_write(mem, move |mem| {
        // The object being written is out of RAM: only the other one changes.
        for handle in [&a, &b] {
            let changed = with_mut(mem, handle, |v| *v += 1);
            assert!(matches!(changed, Ok(Some(()) | None)));
        }
    });
    flush(&mut mem).unwrap().unwrap();
    // The first one written was modified again during the write of the second.
    let modified = mem.lock(|m| {
        let modified = m.core.cache.values().filter(|entry| entry.modified);
        modified.count()
    });
    assert_eq!(modified, 1);
}

#[test]
fn a_flush_during_a_flush_is_busy() {
    let mut mem = mount(Flash::new(8), CACHE);
    let a = insert(&mut mem, "a", 0u32).unwrap().unwrap();
    let b = insert(&mut mem, "b", 0u32).unwrap().unwrap();
    flush(&mut mem).unwrap().unwrap();
    with_mut(&mut mem, &b, |v| *v = 1).unwrap().unwrap();
    let mut mem = share(mem);
    let seen = Arc::new(StdMutex::new(Vec::new()));
    let log = seen.clone();
    // During the write of `b`, `a` changes, and a second flush finds the storage busy.
    on_write(mem, move |mem| {
        with_mut(mem, &a, |v| *v += 1).unwrap().unwrap();
        log.lock().unwrap().push(matches!(flush(mem), Ok(None)));
    });
    flush(&mut mem).unwrap().unwrap();
    let seen = seen.lock().unwrap();
    assert!(
        !seen.is_empty() && seen.iter().all(|&busy| busy),
        "{seen:?}"
    );
    let modified = mem.lock(|m| m.core.cache[&a.id].modified);
    assert!(modified, "left for the next flush");
}

#[test]
fn remove_during_flush() {
    let mut mem = mount(Flash::new(8), CACHE);
    let a = insert(&mut mem, "a", 1u32).unwrap().unwrap();
    insert(&mut mem, "b", 2u32).unwrap().unwrap();
    flush(&mut mem).unwrap().unwrap();
    with_mut(&mut mem, &a, |v| *v = 10).unwrap().unwrap();
    let mut mem = share(mem);
    // Removes `a` during its write. `remove` never waits, so it works from an interrupt.
    on_write(mem, move |mem| {
        let _ = remove(mem, a);
    });
    flush(&mut mem).unwrap().unwrap();
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
    assert_eq!(usage(&mut mem).objects, 1);
}

/// Inserts `a`, replaces `x` with a new object of the same name, and removes `y`.
fn replace_x(mem: &mut Mem) {
    insert(mem, "a", 3u32).unwrap().unwrap();
    let x: Handle<u32> = get(mem, "x").unwrap();
    remove(mem, x).unwrap();
    insert(mem, "x", 10u32).unwrap().unwrap();
    let y: Handle<u32> = get(mem, "y").unwrap();
    remove(mem, y).unwrap();
}

#[test]
fn power_cut_during_flush() {
    let mut mem = mount(Flash::new(8), CACHE);
    insert(&mut mem, "x", 1u32).unwrap().unwrap();
    insert(&mut mem, "y", 2u32).unwrap().unwrap();
    flush(&mut mem).unwrap().unwrap();
    let flash = power_cut(mem);
    let mut two_x = false;
    for op in 0.. {
        let cut = Flash::with_data(flash.data.clone()).cut_at(op, Cut::FirstHalf);
        let mut mem = mount(cut, CACHE);
        replace_x(&mut mem);
        let done = matches!(flush(&mut mem), Ok(Some(())));
        let left = power_cut(mem).data;
        let stored = NorBackend::mount(config(Flash::with_data(left.clone()))).unwrap();
        let stored = stored.usage().objects;

        // Each object as it was or as written, the new ones before the removals. `x` is the old
        // or the new one, never none, and the mount removed the old one if both were left.
        let (found, objects) = contents(&mut mount(Flash::with_data(left), CACHE));
        let value = |name: &str| found.iter().find(|(n, _)| n == name).map(|&(_, v)| v);
        assert!(matches!(value("x"), Some(1 | 10)), "cut at {op}: {found:?}");
        assert!(
            matches!(value("a"), None | Some(3)),
            "cut at {op}: {found:?}"
        );
        if value("y").is_none() {
            assert_eq!(value("a"), Some(3), "cut at {op}");
            assert_eq!(value("x"), Some(10), "cut at {op}");
        }
        assert_eq!(objects, found.len(), "cut at {op}");
        two_x |= stored > found.len();
        if done {
            assert_eq!(found, [("a".into(), 3), ("x".into(), 10)]);
            break;
        }
    }
    assert!(two_x, "some cut left two objects named x");
}

/// The names, values and object count a mount finds, all objects being of type `T`.
fn contents_of<T: Storable + Clone>(mem: &mut Mem) -> (Vec<(String, T)>, usize) {
    let names = list(mem);
    let read = |name: String| {
        let handle: Handle<T> = get(mem, &name).unwrap();
        let value = with(mem, &handle, T::clone).unwrap().unwrap();
        (name, value)
    };
    let values = names.into_iter().map(read).collect();
    (values, usage(mem).objects)
}

fn contents(mem: &mut Mem) -> (Vec<(String, u32)>, usize) {
    contents_of(mem)
}

#[test]
fn power_cut_during_mount() {
    let mut mem = mount(Flash::new(8), CACHE);
    insert(&mut mem, "x", 1u32).unwrap().unwrap();
    insert(&mut mem, "y", 2u32).unwrap().unwrap();
    flush(&mut mem).unwrap().unwrap();
    let flash = power_cut(mem);
    let mut cut_mounts = 0;
    for flush_op in 0.. {
        // A flush cut at each of its operations: the next mount marks stale pages obsolete, and
        // removes the old `x` if the new one is stored too.
        let cut = Flash::with_data(flash.data.clone()).cut_at(flush_op, Cut::FirstHalf);
        let mut mem = mount(cut, CACHE);
        replace_x(&mut mem);
        let done = matches!(flush(&mut mem), Ok(Some(())));
        let left = power_cut(mem).data;
        let expected = contents(&mut mount(Flash::with_data(left.clone()), CACHE));

        // That mount cut at each of its operations: the next one finds the same.
        for cut in [Cut::FirstHalf, Cut::SecondHalf] {
            for mount_op in 0.. {
                let copy = Arc::new(StdMutex::new(Vec::new()));
                let flash = Flash::with_data(left.clone())
                    .cut_at(mount_op, cut)
                    .copy_on_cut(copy.clone());
                let at = format!("flush cut at {flush_op}, mount cut at {mount_op} ({cut:?})");
                match Mem::mount(config(flash), CACHE, || {}) {
                    // Done before the cut.
                    Ok(mut mem) => {
                        assert_eq!(contents(&mut mem), expected, "{at}");
                        break;
                    }
                    Err(error) => {
                        let power_cut = Error::Backend(NorError::Flash(MockError::PowerCut));
                        assert_eq!(format!("{error:?}"), format!("{power_cut:?}"), "{at}");
                    }
                }
                cut_mounts += 1;
                let after = copy.lock().unwrap().clone();
                let mut mem = mount(Flash::with_data(after), CACHE);
                assert_eq!(contents(&mut mem), expected, "{at}");
            }
        }
        if done {
            break;
        }
    }
    assert!(cut_mounts > 0, "some mounts write");
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
