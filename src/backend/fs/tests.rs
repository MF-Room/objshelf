use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::file::{BLOCK, FORMAT_VERSION, HEADER_SIZE};
use super::{FsBackend, FsConfig, FsError, open_options};
use crate::backend::{Backend, Usage};
use crate::{ObjShelf, ObjectId, flush, get, insert, list, with, with_mut};

/// A directory of its own for one test, under the system's temporary directory, removed when
/// dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let name = format!("objshelf-fs-{}-{n}", std::process::id());
        let path = std::env::temp_dir().join(name);
        let _ = fs::remove_dir_all(&path);
        Self(path)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const CAPACITY: usize = 64 * BLOCK;
const X: ObjectId = ObjectId(0x2a);
const Y: ObjectId = ObjectId(7);

fn config(dir: &TempDir, direct_io: bool) -> FsConfig {
    FsConfig {
        dir: dir.0.clone(),
        capacity: CAPACITY,
        direct_io,
    }
}

fn mount(dir: &TempDir) -> FsBackend {
    FsBackend::mount(config(dir, true)).unwrap()
}

fn sorted_ids(backend: &FsBackend) -> Vec<ObjectId> {
    let mut ids = backend.ids();
    ids.sort_unstable();
    ids
}

/// Changes the file `name` with `change`, as a damaged disk would.
fn damage(dir: &TempDir, name: &str, change: impl FnOnce(&mut Vec<u8>)) {
    let path = dir.file(name);
    let mut bytes = fs::read(&path).unwrap();
    change(&mut bytes);
    fs::write(&path, bytes).unwrap();
}

#[test]
fn round_trip() {
    let dir = TempDir::new();
    let mut backend = mount(&dir);
    let big: Vec<u8> = (0..10_000).map(|i| i as u8).collect();
    backend.write(X, "small", &5u32).unwrap();
    backend.write(Y, "big", &big).unwrap();
    assert_eq!(backend.read::<u32>(X).unwrap(), Some(5));
    assert_eq!(backend.read::<Vec<u8>>(Y).unwrap(), Some(big));
    let len = |name| fs::metadata(dir.file(name)).unwrap().len();
    assert_eq!(len("0000002a.obj"), BLOCK as u64);
    assert_eq!(len("00000007.obj"), 3 * BLOCK as u64);
}

#[test]
#[cfg(target_os = "linux")]
fn o_direct_is_used() {
    let dir = TempDir::new();
    let mut backend = mount(&dir);
    backend.write(X, "x", &1u32).unwrap();
    assert_eq!(backend.read::<u32>(X).unwrap(), Some(1));
    let message = "the temporary directory's file system refuses O_DIRECT (tmpfs on old kernels?)";
    assert!(backend.direct_io(), "{message}");
}

/// A file system that refuses `O_DIRECT` answers `EINVAL`. This kernel's tmpfs accepts it, so
/// the error is injected.
#[test]
#[cfg(target_os = "linux")]
fn a_refused_o_direct_falls_back_to_buffered_io() {
    let dir = TempDir::new();
    let mut backend = mount(&dir);
    let missing = dir.file("missing");
    let mut runs = Vec::new();
    let open = backend.io(|direct| {
        runs.push(direct);
        open_options(direct).read(true).open(&missing)
    });
    // Another error is not retried, and O_DIRECT stays on.
    assert_eq!(open.unwrap_err().kind(), io::ErrorKind::NotFound);
    assert_eq!(runs, [true]);
    assert!(backend.direct_io());

    let mut runs = Vec::new();
    let result = backend.io(|direct| {
        runs.push(direct);
        match direct {
            true => Err(io::Error::from_raw_os_error(libc::EINVAL)),
            false => Ok(5),
        }
    });
    assert_eq!(result.unwrap(), 5);
    assert_eq!(runs, [true, false]);
    assert!(!backend.direct_io());
    // Buffered from then on.
    let mut runs = Vec::new();
    backend
        .io(|direct| {
            runs.push(direct);
            Ok(())
        })
        .unwrap();
    assert_eq!(runs, [false]);
    backend.write(X, "x", &1u32).unwrap();
    assert_eq!(backend.read::<u32>(X).unwrap(), Some(1));
}

#[test]
fn reading_with_another_type_gives_none() {
    let dir = TempDir::new();
    let mut backend = mount(&dir);
    backend.write(X, "x", &5u32).unwrap();
    assert_eq!(backend.read::<i32>(X).unwrap(), None);
    assert_eq!(backend.read::<u32>(X).unwrap(), Some(5));
}

#[test]
fn remount_finds_the_objects() {
    let dir = TempDir::new();
    let mut backend = mount(&dir);
    backend.write(X, "x", &1u32).unwrap();
    backend.write(Y, "y", &"why".to_string()).unwrap();
    backend.unmount().unwrap();

    let mut backend = mount(&dir);
    assert_eq!(sorted_ids(&backend), [Y, X]);
    // Kept from mount, then read from the file.
    assert_eq!(backend.name(X).unwrap(), "x");
    assert_eq!(backend.name(X).unwrap(), "x");
    assert_eq!(backend.name(Y).unwrap(), "y");
    assert_eq!(backend.read::<String>(Y).unwrap().unwrap(), "why");
    assert!(matches!(backend.name(ObjectId(3)), Err(FsError::NotFound)));
}

#[test]
fn replace_then_remove() {
    let dir = TempDir::new();
    let mut backend = mount(&dir);
    backend.write(X, "x", &vec![0u8; 5000]).unwrap();
    assert_eq!(backend.usage().used, 2 * BLOCK);
    backend.write(X, "x", &1u32).unwrap();
    assert_eq!(backend.read::<u32>(X).unwrap(), Some(1));
    let usage = Usage {
        capacity: CAPACITY,
        used: BLOCK,
        free: CAPACITY - BLOCK,
        reclaimable: 0,
        objects: 1,
    };
    assert_eq!(backend.usage(), usage);

    backend.remove(X).unwrap();
    assert!(!dir.file("0000002a.obj").exists());
    assert!(matches!(backend.read::<u32>(X), Err(FsError::NotFound)));
    assert!(matches!(backend.remove(X), Err(FsError::NotFound)));
    assert_eq!(backend.usage().used, 0);
    assert_eq!(backend.usage().objects, 0);
}

#[test]
fn a_write_cut_by_a_crash_leaves_the_old_value() {
    let dir = TempDir::new();
    let mut backend = mount(&dir);
    backend.write(X, "x", &1u32).unwrap();
    backend.unmount().unwrap();
    // The new value, written and synced, but not renamed yet.
    let other = TempDir::new();
    let mut backend = mount(&other);
    backend.write(X, "x", &2u32).unwrap();
    fs::copy(other.file("0000002a.obj"), dir.file("0000002a.tmp")).unwrap();
    // A first write cut sooner.
    fs::write(dir.file("00000003.tmp"), b"cut").unwrap();

    let mut backend = mount(&dir);
    assert!(!dir.file("0000002a.tmp").exists());
    assert!(!dir.file("00000003.tmp").exists());
    assert_eq!(backend.ids(), [X]);
    assert_eq!(backend.read::<u32>(X).unwrap(), Some(1));
}

#[test]
fn other_files_are_ignored() {
    let dir = TempDir::new();
    let mut backend = mount(&dir);
    backend.write(X, "x", &1u32).unwrap();
    backend.unmount().unwrap();
    let object = fs::read(dir.file("0000002a.obj")).unwrap();
    let others = ["._0000002a.obj", "0000002A.obj", "notes.txt"];
    for name in others {
        fs::write(dir.file(name), &object).unwrap();
    }
    fs::create_dir(dir.file("00000005.obj")).unwrap();

    let backend = mount(&dir);
    assert_eq!(backend.ids(), [X]);
    assert_eq!(backend.usage().used, BLOCK);
    for name in others {
        assert!(dir.file(name).exists(), "{name}");
    }
}

#[test]
fn a_damaged_payload_reads_corrupt() {
    let dir = TempDir::new();
    let mut backend = mount(&dir);
    backend.write(X, "x", &1u32).unwrap();
    backend.unmount().unwrap();
    // After the name (2 bytes) and the type tag (4 bytes): the value.
    damage(&dir, "0000002a.obj", |bytes| bytes[HEADER_SIZE + 6] ^= 1);

    let mut backend = mount(&dir);
    assert_eq!(backend.ids(), [X]);
    assert!(matches!(backend.read::<u32>(X), Err(FsError::Corrupt)));
}

#[test]
fn mount_skips_damaged_files() {
    let dir = TempDir::new();
    let mut backend = mount(&dir);
    backend.write(X, "x", &1u32).unwrap();
    backend.write(Y, "y", &2u32).unwrap();
    backend.write(ObjectId(1), "z", &3u32).unwrap();
    backend.unmount().unwrap();
    damage(&dir, "0000002a.obj", |bytes| bytes[0] ^= 1);
    damage(&dir, "00000001.obj", |bytes| bytes.truncate(100));

    let mut backend = mount(&dir);
    assert_eq!(backend.ids(), [Y]);
    assert_eq!(backend.read::<u32>(Y).unwrap(), Some(2));
    assert!(dir.file("0000002a.obj").exists());
}

#[test]
fn another_format_version_fails_the_mount() {
    let dir = TempDir::new();
    let mut backend = mount(&dir);
    backend.write(X, "x", &1u32).unwrap();
    backend.unmount().unwrap();
    let version = FORMAT_VERSION + 1;
    damage(&dir, "0000002a.obj", |bytes| {
        bytes[4..6].copy_from_slice(&version.to_le_bytes());
    });
    fs::write(dir.file("00000003.tmp"), b"cut").unwrap();

    let mount = FsBackend::mount(config(&dir, true));
    assert!(matches!(mount, Err(FsError::UnsupportedFormat(v)) if v == version));
    // Nothing changed.
    assert!(dir.file("00000003.tmp").exists());
}

#[test]
fn writes_past_the_capacity_fail() {
    let dir = TempDir::new();
    let config = FsConfig {
        capacity: 3 * BLOCK,
        ..config(&dir, true)
    };
    let mut backend = FsBackend::mount(config).unwrap();
    let two_blocks = vec![1u8; 5000];
    backend.write(X, "x", &two_blocks).unwrap();
    assert!(matches!(
        backend.write(Y, "y", &two_blocks),
        Err(FsError::NoSpace)
    ));
    backend.write(Y, "y", &1u32).unwrap();
    // Three blocks, in place of two: one more than is free.
    let three_blocks = vec![2u8; 9000];
    assert!(matches!(
        backend.write(X, "x", &three_blocks),
        Err(FsError::NoSpace)
    ));
    assert_eq!(backend.read::<Vec<u8>>(X).unwrap(), Some(two_blocks));
    assert!(!dir.file("0000002a.tmp").exists());
    let usage = Usage {
        capacity: 3 * BLOCK,
        used: 3 * BLOCK,
        free: 0,
        reclaimable: 0,
        objects: 2,
    };
    assert_eq!(backend.usage(), usage);
    // Smaller.
    backend.write(X, "x", &2u32).unwrap();
    assert_eq!(backend.usage().used, 2 * BLOCK);
}

#[test]
fn a_second_mount_is_locked_out() {
    let dir = TempDir::new();
    let backend = mount(&dir);
    assert!(matches!(
        FsBackend::mount(config(&dir, true)),
        Err(FsError::Locked)
    ));
    backend.unmount().unwrap();
    mount(&dir);
}

#[test]
fn buffered_and_direct_io_read_each_other() {
    let dir = TempDir::new();
    let big = vec![3u8; 9000];
    let mut buffered = FsBackend::mount(config(&dir, false)).unwrap();
    assert!(!buffered.direct_io());
    buffered.write(X, "x", &big).unwrap();
    buffered.unmount().unwrap();

    let mut direct = mount(&dir);
    assert_eq!(direct.read::<Vec<u8>>(X).unwrap(), Some(big));
    direct.write(Y, "y", &1u32).unwrap();
    direct.unmount().unwrap();

    let mut buffered = FsBackend::mount(config(&dir, false)).unwrap();
    assert_eq!(buffered.read::<u32>(Y).unwrap(), Some(1));
}

type Mem = ObjShelf<FsBackend>;

#[test]
fn obj_shelf_on_files() {
    let dir = TempDir::new();
    let mut mem = Mem::mount(config(&dir, true), 4096, || {}).unwrap();
    let count = insert(&mut mem, "count", 1u32).unwrap().unwrap();
    with_mut(&mut mem, &count, |n| *n += 1).unwrap().unwrap();
    flush(&mut mem).unwrap().unwrap();
    mem.unmount().unwrap();

    let mut mem = Mem::mount(config(&dir, true), 4096, || {}).unwrap();
    let count = get::<u32, _>(&mut mem, "count").unwrap();
    assert_eq!(with(&mut mem, &count, |n| *n).unwrap(), Some(2));
}

/// A flush cut between the write of a new object and the removal of the old one of the same
/// name: mount keeps the newer one, with the higher id, and removes the file of the other.
#[test]
fn obj_shelf_keeps_the_newer_of_two_objects_with_one_name() {
    let dir = TempDir::new();
    let mut backend = mount(&dir);
    backend.write(ObjectId(1), "x", &1u32).unwrap();
    backend.write(ObjectId(2), "x", &2u32).unwrap();
    backend.unmount().unwrap();

    let mut mem = Mem::mount(config(&dir, true), 4096, || {}).unwrap();
    assert_eq!(list(&mut mem), ["x"]);
    let x = get::<u32, _>(&mut mem, "x").unwrap();
    assert_eq!(with(&mut mem, &x, |n| *n).unwrap(), Some(2));
    assert!(!dir.file("00000001.obj").exists());
}
