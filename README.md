# objshelf

A portable object store that caches plain Rust values on the heap, over pluggable backends from bare-metal to
hosted systems.

Objects have a name and a type. They are read from the storage backend on first use and kept in RAM within a
budget. Changes stay in RAM until you flush them, so the writes to storage only happen when and where you
choose.

## Quick start

You need:

* a heap (`alloc`): a global allocator, for example [`embedded-alloc`](https://crates.io/crates/embedded-alloc);
* [RTIC 2](https://rtic.rs), or any lock that implements its `Mutex` trait (re-exported as `objshelf::Mutex`);
* a backend. The NOR backend runs on any blocking `embedded_storage::nor_flash::NorFlash` driver with
  `READ_SIZE` and `WRITE_SIZE` of 1 and an `ERASE_SIZE` that is a multiple of 256: an SPI NOR driver,
  internal MCU flash, or a RAM simulation for tests. The file system backend (feature `fs`) keeps one file
  per object in a directory, on Linux and other Unix systems.

```toml
[dependencies]
objshelf = { git = "https://github.com/MF-Room/objshelf" }
serde = { version = "1", default-features = false, features = ["derive"] }
get-size2 = { version = "0.11", default-features = false, features = ["alloc", "derive"] }
```

`get-size2` measures the RAM an object takes, its heap included, for the RAM budget. Its derive refers to
it by name, so it must be a direct dependency, with the same version as objshelf's (`0.11`).

The memory is an RTIC `#[shared]` resource. Every function takes it as its first argument, and takes the
lock itself, for short steps only:

```rust
// Inside your `#[rtic::app]` module. `Flash` is your blocking `NorFlash` driver.
use get_size2::GetSize;
use serde::{Deserialize, Serialize};
use objshelf::backend::nor::{NorBackend, NorConfig};
use objshelf::{Handle, ObjShelf};

type Mem = ObjShelf<NorBackend<Flash>>;

// Any `Serialize + Deserialize + GetSize` type can be stored. For a field of a type without `GetSize`,
// use `#[get_size(ignore)]` if it owns no heap (a `heapless` collection, for example), or
// `#[get_size(size_fn = f)]`.
#[derive(Default, Serialize, Deserialize, GetSize)]
struct Settings {
    brightness: u8,
    name: String,
}

#[shared]
struct Shared {
    mem: Mem,
}

#[local]
struct Local {
    // A handle is a `Copy` id of the object.
    settings: Handle<Settings>,
}

#[init]
fn init(cx: init::Context) -> (Shared, Local) {
    let flash = Flash::new(/* SPI bus, chip select, ... */);
    // Region of the flash used by objshelf: aligned to the erase size, at least 2 sectors.
    let config = NorConfig { flash, range: 0..64 * 4096 };
    // Blocks while the flash is mounted. Objects in RAM take about 16 KiB. The closure runs when
    // unflushed changes fill that budget: here it starts the `save` task.
    let mut mem = Mem::mount(config, 16 * 1024, || {
        save::spawn().ok();
    })
    .unwrap();
    // Finds the object after a reboot, or creates it on the first boot. Before `mem` is shared,
    // the functions take it directly.
    let settings = objshelf::get_or_insert_with(&mut mem, "settings", Settings::default)
        .unwrap()
        .expect("fits in the RAM budget");
    app::spawn().unwrap();
    (Shared { mem }, Local { settings })
}

// Above the tasks that write: it keeps running during a flush, so it can get `None` (see below).
#[task(priority = 2, shared = [mem], local = [settings])]
async fn app(mut cx: app::Context) {
    let settings = cx.local.settings;
    let mem = &mut cx.shared.mem;
    // Reads the object from the backend if it is not in RAM.
    let Some(brightness) = objshelf::with(mem, settings, |s| s.brightness).unwrap() else {
        // Not in RAM, and a flush has the backend: try again later.
        return;
    };
    // Changes RAM only, until the next flush.
    match objshelf::with_mut(mem, settings, |s| s.brightness = brightness + 1).unwrap() {
        // Writes the settings soon, and only them.
        Some(()) => {
            save_settings::spawn(*settings).ok();
        }
        // A flush has the backend, or unflushed changes fill the budget (`on_full` spawned
        // `save`): try again later.
        None => {}
    }
}

// The tasks that write to the backend, at the lowest priority: every task above it keeps running
// meanwhile. With the same priority, neither interrupts the other, so neither gets `None`.

// Writes all the changes. Spawned by `on_full`: only a full flush frees the whole budget.
#[task(priority = 1, shared = [mem])]
async fn save(mut cx: save::Context) {
    objshelf::flush(&mut cx.shared.mem).unwrap();
}

// Writes only the settings: the other changes stay in RAM until `save` runs. A handle is `Copy`, so
// it can be a spawn argument.
#[task(priority = 1, shared = [mem])]
async fn save_settings(mut cx: save_settings::Context, settings: Handle<Settings>) {
    objshelf::flush_object(&mut cx.shared.mem, &settings).unwrap();
}
```

No function waits, so any task can call any of them. When a call can't be done now, it returns `Ok(None)`
and does nothing: try again later. Tasks that don't share `mem` are never blocked by objshelf, whatever their
priority.

## API

In a task, `mem` is `&mut cx.shared.mem`. In `init`, before the memory is shared, it is `&mut mem` on the
owned `ObjShelf`.

| Function | What it does |
|---|---|
| `ObjShelf::mount(config, cache_bytes, on_full)` | Mounts the backend and reads the names of the stored objects, blocking (for `init`). `cache_bytes` is the RAM budget, `on_full` is called when unflushed changes fill it. |
| `ObjShelf::unmount(self)` | Flushes, then unmounts the backend. |
| `list(mem)` | Names of all objects. |
| `get::<T, _>(mem, name)` | Handle to the object `name`, of type `T`. `NotFound` if it doesn't exist. |
| `insert(mem, name, value)` | Adds a new object and returns its handle. `AlreadyExists` if the name is taken, `NameTooLong` over 64 bytes (`MAX_NAME_LEN`). |
| `get_or_insert_with(mem, name, f)` | Handle to the object `name`, inserting `f()` first if it doesn't exist (first boot). |
| `remove(mem, handle)` | Removes the object. |
| `with(mem, &handle, \|v\| ...)` | Runs the closure on the object and returns its result. Reads the object from the backend first if it is not in RAM. |
| `with_mut(mem, &handle, \|v\| ...)` | Same, with mutable access. The change stays in RAM until the next flush. |
| `usage(mem)` | Space of the backend: capacity, used, free, reclaimable by garbage collection, number of objects. |
| `wear(mem)` | Wear of the storage (NOR flash): sectors, erases of the most worn one, all erases added up. `None` for storage that doesn't wear out. |
| `flush(mem)` | Writes the changes to the backend: changed and new objects, each with its name, then the removals. |
| `flush_object(mem, &handle)` | Writes this object if it changed, with its name. The other changes, removals included, stay in RAM. |

`with`, `with_mut`, `insert`, `get_or_insert_with`, `flush` and `flush_object` return `Ok(None)`, having
done nothing, when the call can't be done now:

* **Busy:** a flush has the backend, and the call needs it. That is `with` and `with_mut` on an object that
  is not in RAM or is being written, and the flushes themselves. Only a task of a higher priority than the
  flush, which interrupts it, can get it.
* **Full:** unflushed changes fill the RAM budget, for `with_mut` and `insert` (and `get_or_insert_with`
  when it inserts). `on_full` was called to start a flush.

The errors are `NotFound`, `AlreadyExists`, `NameTooLong` (over `MAX_NAME_LEN`), `WrongType` (the
handle's type is not the object's), `TooBig` (the object alone is bigger than the RAM budget, so
`with_mut` and `insert` can never take it) and `Backend` (the backend failed). When an object fails to
write, `flush` still writes the others and does the removals, and returns the first error: the object
stays in RAM for the next flush.

## How it works

* **Writes.** Only `flush`, `flush_object` and `unmount` write to the backend. On NOR flash, a flush takes a
  few ms per object, and hundreds of ms when garbage collection erases a sector. It runs in the caller's task:
  call it from a task of the lowest priority, so that every other task keeps running.
* **Locking.** Each function takes the lock of `mem` for short steps only: never call them inside
  `cx.shared.mem.lock(...)`. The lock blocks the tasks that share `mem`, up to the highest priority among
  them, and no other task. It is held for RAM work, for the closures given to `with` and `with_mut` (keep
  them short), and for the read of an object not in RAM (about 85 µs per 256-byte page of SPI NOR at
  25 MHz). The writes of a flush run outside of it.
* **Busy backend.** While a flush has the backend, objects in RAM keep working; `with` and `with_mut` return
  `None` for the others. A task at the flush's priority or below never sees it: it can't run during the
  flush.
* **RAM budget.** Each object in RAM counts its size, what it owns on the heap (measured with `GetSize`,
  spare `Vec` capacity included) and its entry in the cache; only the allocator's own overhead is left
  out. Over budget, the least recently used unchanged objects are dropped: they are read again
  when needed. Changed objects stay until a flush; when one more would not fit, the call calls `on_full`
  and returns `None`. An object bigger than the whole budget gets `TooBig` instead: no flush can make
  room for it.
* **Durability.** Changes, inserts and removals reach the backend on `flush` or `unmount`, or on
  `flush_object` for one object: a reset loses the rest. A power cut during a flush leaves each object
  either as it was or as written, with its name. An object replaced by a new one of the same name
  (`remove`, then `insert`) stays stored until the new one is written, and mount keeps the newer one if
  both are left: the name never ends up with no object or two. Objects that must stay consistent with
  each other should be flushed together, with `flush`.
* **Names and types.** Each object is stored with its name, and mount reads the names from the objects:
  there is no directory to lose, and an `insert` or a `remove` writes only that object. A name is at
  most 64 bytes (`MAX_NAME_LEN`), as it is written again with each write of its object. An object must
  be read with the type it was inserted with, or `with` and `with_mut` give `WrongType`. This is checked in
  RAM, and by the backend with a 4-byte tag stored with each object, a hash of the type's name
  (`type_name`). Renaming a type or moving it to another module changes the tag, as can a new compiler:
  the objects stored before then give `WrongType`. Changing the fields of a type doesn't: after a firmware
  update, converting the stored data is up to the app.
* **Limits.** An object must fit in the RAM budget (`TooBig` otherwise) and, serialized with its name and
  type tag, in the free space of the backend: at most 65535 pages of 235 bytes (about 15 MiB) on NOR. An
  object that doesn't fit fails its flush with `NoSpace`, and the flush goes on with the others. A read or
  write also holds the whole serialized object on the heap while it runs. The NOR region is aligned to the
  erase size, has at least 2 sectors (one stays free for garbage collection) and at most 16 MiB, and is
  used by nothing else. Each write of an object gets the next number of a `u32` counter, and mount keeps
  the copy with the highest one. The flash wears out before that counter can wrap: each write programs at
  least one page, and a W25Q16JV takes about 768 million page writes in its life (512 sectors × 15 data
  pages × 100 000 erases), against 4.3 billion numbers. Only a region of about 11 MiB or more could get
  there.

Details: the `ObjShelf` documentation in [`src/api.rs`](src/api.rs), the public API. The engine behind it is
in [`src/memory`](src/memory/mod.rs).

## NOR flash backend

`src/backend/nor`: object store on any blocking `embedded_storage::nor_flash::NorFlash` with 1-byte writes
(chips with internal ECC are not supported for now). Every operation blocks until done.

* Objects are serialized with postcard, their name and type tag first, and split in 256-byte pages
  (20-byte header, 235-byte payload, 1-byte obsolete marker). Page 0 of each sector holds a sector header
  with the format version and the erase count. A sector of another format version makes mount fail with
  `UnsupportedFormat`, instead of being erased as unformatted.
* RAM index: `ObjectId` → current version and page list (`hashbrown::HashMap`, needs a heap).
* Each write gets a new global version. A version counts only once all its pages are on flash, so a power
  cut leaves either the old or the new value. Replaced and removed pages are then marked obsolete.
* A page whose write was cut can read right and lose weak bits later. When the cut hit an update or a
  garbage collection, the old copy is still there at mount, which then writes the object again.
* Every byte is programmed once between erases, and only from `0xFF`.
* Garbage collection relocates the live pages of the sector with the most garbage and erases it; one free
  sector is kept for it. Ties go to the least worn sector.
* Wear: new pages go to the least worn free sector. Data that never changes would keep its sector out
  of the rotation, so when the least worn sector holding data is 100 erases behind the most worn one,
  garbage collection moves its data to the most worn free sector and erases it (static wear
  leveling). A sector found without a header at mount (an erase cut by a power loss) gets the average
  erase count of the others instead of 0, so it is not reused first again and again.
* Each page and sector header is read back right after its write, while the data is still in RAM. A page
  that reads back wrong is marked obsolete and written again on the next free page; a sector header
  that reads back wrong leaves the sector unformatted, and another sector is opened. After 3 tries the
  write fails with `VerifyFailed`, and the object keeps its previous value.
* Mount reads the whole region and rebuilds the index (2 MiB at 25 MHz: about 0.7 s).

### Lifetime

A sector endures a limited number of erases (100 000 for the W25Q16JV), and wear leveling spreads
them over all the sectors, so:

```text
lifetime ≈ sectors × endurance × pages written per erase / pages written per day
```

Each flush writes every modified object once, in pages of 235 bytes (name and value serialized).
Pages written per erase depend on how full the flash is: a fuller flash leaves less garbage in each
sector, so garbage collection moves more live pages for each erase. Measured with 4 KiB sectors and
one-page objects updated at random (`cargo test --release -- --ignored --nocapture pages_per_erase`):

| Flash full | Pages per erase | 1 page/s, whole W25Q16JV | 1 page/min, whole W25Q16JV |
|---|---|---|---|
| 25% or less | 15.0 | 24 years | 1 400 years |
| 50% | 12.5 | 20 years | 1 200 years |
| 75% | 7.5 | 12 years | 730 years |
| 90% | 3.7 | 6 years | 360 years |

So the region size and how full it is matter as much as how often you flush. At runtime,
`wear(mem)` gives the erases so far, stored with the sectors, so they count across reboots. Read
`total_erases` twice, some time apart, for the erase rate, then `remaining_erases(endurance)` / rate
is the time left. When the sectors wear out, writes fail with `VerifyFailed` or `NoSpace` and what
was written stays readable: on a simulated flash that wears out, the store used 87% of the page
writes the flash could take before its first failed write.

Requirements, guarantees, time and space complexity: module documentation in
[`src/backend/nor/mod.rs`](src/backend/nor/mod.rs) (`cargo doc --document-private-items`).

## File system backend

`src/backend/fs`, with the `fs` feature: one file per object in a directory, on Linux and other Unix
systems (it needs `std`).

```toml
objshelf = { git = "https://github.com/MF-Room/objshelf", features = ["fs"] }
```

```rust
use objshelf::backend::fs::{FsBackend, FsConfig};

let config = FsConfig { dir: "/var/lib/app/objects".into(), capacity: 64 << 20, direct_io: true };
let mut mem = ObjShelf::<FsBackend>::mount(config, 1 << 20, || {})?;
```

* Each object is in `{id:08x}.obj`: a 16-byte header (format version, payload length, CRC32), then the
  object serialized with postcard, its name and type tag first, padded to whole 4 KiB blocks.
* `ObjShelf` already caches the objects in RAM, so on Linux the files are read and written with
  `O_DIRECT`, past the page cache, which would hold a second copy. If the file system refuses it
  (`EINVAL`), the backend falls back to buffered I/O for the rest of the mount: `direct_io()` tells which
  is in use, and `direct_io: false` turns it off.
* A write goes to `{id:08x}.tmp`, which is synced, renamed over the object's file, then the directory is
  synced: after a crash, each object holds its old or its new value, and mount deletes the leftover `.tmp`
  files. `O_DIRECT` skips the page cache, not the syncs.
* `capacity` is a quota on the files, headers and padding included: a write past it fails with `NoSpace`,
  and the object keeps its previous value. `usage` counts whole blocks, with nothing to reclaim.
* Mount lists the directory and reads the first block of each file, which holds the name. A file whose
  header or name is damaged is skipped and left on disk, so it never blocks the mount; a file of another
  format version makes mount fail with `UnsupportedFormat`. `read` checks the CRC: `Corrupt` if it
  doesn't match.
* A lock file makes a second mount of the directory, from any process, fail with `Locked`. Other files in
  the directory are ignored.
* Limits: at least 4 KiB on disk per object, and less than 4 GiB serialized. A read or write holds the
  whole file in RAM, plus 4 KiB to align it.

Requirements, guarantees, time and space complexity: module documentation in
[`src/backend/fs/mod.rs`](src/backend/fs/mod.rs) (`cargo doc --features fs --document-private-items`).

## TODO

- [x] Reuse a store crate (`sequential-storage`, `ekv`, `littlefs2`) or write one: own store
- [x] When to run garbage collection: only a flush writes, so it only runs inside a flush, in the
  lowest-priority task. Its erases (up to 400 ms per sector) never block a higher-priority task: objects in
  RAM keep working, and the others return `None` until the flush ends.
- [x] Wear leveling: the least worn free sector is opened next, ties between GC victims go to the least
  worn sector, static wear leveling in the GC (data that never changes moves once its sector is 100
  erases behind), and a sector header lost to an interrupted erase gets the average erase count
- [x] Lifetime estimate: the formula and measured pages per erase (Lifetime), `wear(mem)` at
  runtime, and an end-of-life test on a simulated flash that wears out
- [x] Read back each page and sector header after its write, and write it again elsewhere if it reads
  back wrong (`VerifyFailed` after 3 tries)
- [x] Format version in the sector header: mount fails on another version instead of erasing it
- [ ] Edge cases:
  - after a flash error, the RAM state may not match the flash: define the rule (remount?)
  - a page that decays after its write only shows up as `Corrupt` on read, with no fallback: bit rot,
    or weak bits after a cut at the very end of an object's first write (an update or a garbage
    collection cut that way is written again at mount)
  - a corrupted sector header makes mount ignore the whole sector and its objects
  - skipped dirty pages, and pages that read back wrong, can eat into the GC reserve; GC refuses a
    victim whose live pages don't fit
  - peak RAM: `write` and `read` hold the whole serialized object, plus the page list
- [x] Implement `ObjShelf` (`src/api.rs`, `src/memory`): RTIC shared resource with short locks, names
  stored with the objects, LRU read cache with a RAM budget, changes kept in RAM until a flush
- [x] RAM accounting: an object counts its heap with `get-size2`, spare capacity included, instead of a
  serialized-size estimate
- [x] `Error::TooBig` from `with_mut` and `insert` for an object bigger than the whole RAM budget, instead
  of `None` forever
- [x] A flush writes the other objects and does the removals when one object fails to write (for example
  with `NoSpace`), and returns the first error at the end, so one failing object does not block every
  write
- [x] Names stored with the objects instead of in a directory object, which was a single point of
  failure (lost, mount removed every object) and was rewritten whole on every `insert` or `remove`
- [x] Type check on read: each object is stored with a 4-byte hash of its type's name, checked before
  decoding, so a wrong type gives `WrongType` instead of a postcard error or a wrong decode. Changes to a
  type's fields are not detected: firmware updates are up to the app
- [x] Max name length: `MAX_NAME_LEN`, 64 bytes, `NameTooLong` from `insert`
- [x] Max object size and flash region used by objshelf: documented (Limits)
- [x] Host tests with a simulated `NorFlash` (power cuts, worn pages, wear-out, two geometries), `cargo test`
  in CI
- [x] File system backend (feature `fs`): one file per object, replaced by a rename, `O_DIRECT` with a
  buffered fallback, a quota, a lock against a second mount
