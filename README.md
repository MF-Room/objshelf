# tiermem

Software-managed object memory for embedded Rust, with typed access, transparent residency, and pluggable
storage backends.

Objects have a name and a type. They are read from flash on first use and kept in RAM within a budget.
Changes stay in RAM until you flush them, so the slow flash writes only happen when and where you choose.

## Quick start

You need:

* a heap (`alloc`): a global allocator, for example [`embedded-alloc`](https://crates.io/crates/embedded-alloc);
* [RTIC 2](https://rtic.rs), or any lock that implements its `Mutex` trait (re-exported as `tiermem::Mutex`);
* a backend. The NOR backend runs on any blocking `embedded_storage::nor_flash::NorFlash` driver with
  `READ_SIZE` and `WRITE_SIZE` of 1 and an `ERASE_SIZE` that is a multiple of 256: an SPI NOR driver,
  internal MCU flash, or a RAM simulation for tests.

```toml
[dependencies]
tiermem = { git = "https://github.com/MF-Room/tiermem" }
serde = { version = "1", default-features = false, features = ["derive"] }
```

The memory is an RTIC `#[shared]` resource. Every function takes it as its first argument, and takes the
lock itself, for short steps only:

```rust
// Inside your `#[rtic::app]` module. `Flash` is your blocking `NorFlash` driver.
use serde::{Deserialize, Serialize};
use tiermem::backend::nor::{NorBackend, NorConfig};
use tiermem::{Handle, TierMem};

type Mem = TierMem<NorBackend<Flash>>;

// Any `Serialize + Deserialize` type can be stored.
#[derive(Default, Serialize, Deserialize)]
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
    // Region of the flash used by tiermem: aligned to the erase size, at least 2 sectors.
    let config = NorConfig { flash, range: 0..64 * 4096 };
    // Blocks while the flash is mounted. Objects in RAM take about 16 KiB. The closure runs when
    // unflushed changes fill that budget: here it starts the `save` task.
    let mut mem = Mem::mount(config, 16 * 1024, || {
        save::spawn().ok();
    })
    .unwrap();
    // Finds the object after a reboot, or creates it on the first boot. Before `mem` is shared,
    // the functions take it directly.
    let settings = tiermem::get_or_insert_with(&mut mem, "settings", Settings::default)
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
    // Reads the object from flash if it is not in RAM.
    let Some(brightness) = tiermem::with(mem, settings, |s| s.brightness).unwrap() else {
        // Not in RAM, and a flush has the flash: try again later.
        return;
    };
    // Changes RAM only, until the next flush.
    match tiermem::with_mut(mem, settings, |s| s.brightness = brightness + 1).unwrap() {
        // Writes the settings soon, and only them.
        Some(()) => {
            save_settings::spawn(*settings).ok();
        }
        // A flush has the flash, or unflushed changes fill the budget (`on_full` spawned
        // `save`): try again later.
        None => {}
    }
}

// The tasks that write to the flash, at the lowest priority: every task above it keeps running
// meanwhile. With the same priority, neither interrupts the other, so neither gets `None`.

// Writes all the changes. Spawned by `on_full`: only a full flush frees the whole budget.
#[task(priority = 1, shared = [mem])]
async fn save(mut cx: save::Context) {
    tiermem::flush(&mut cx.shared.mem).unwrap();
}

// Writes only the settings: the other changes stay in RAM until `save` runs. A handle is `Copy`, so
// it can be a spawn argument.
#[task(priority = 1, shared = [mem])]
async fn save_settings(mut cx: save_settings::Context, settings: Handle<Settings>) {
    tiermem::flush_object(&mut cx.shared.mem, &settings).unwrap();
}
```

No function waits, so any task can call any of them. When a call can't be done now, it returns `Ok(None)`
and does nothing: try again later. Tasks that don't share `mem` are never blocked by tiermem, whatever their
priority.

## API

In a task, `mem` is `&mut cx.shared.mem`. In `init`, before the memory is shared, it is `&mut mem` on the
owned `TierMem`.

| Function | What it does |
|---|---|
| `TierMem::mount(config, cache_bytes, on_full)` | Mounts the backend and reads the names of the stored objects, blocking (for `init`). `cache_bytes` is the RAM budget, `on_full` is called when unflushed changes fill it. |
| `TierMem::unmount(self)` | Flushes, then unmounts the backend. |
| `list(mem)` | Names of all objects. |
| `get::<T, _>(mem, name)` | Handle to the object `name`, of type `T`. `NotFound` if it doesn't exist. |
| `insert(mem, name, value)` | Adds a new object and returns its handle. `AlreadyExists` if the name is taken. |
| `get_or_insert_with(mem, name, f)` | Handle to the object `name`, inserting `f()` first if it doesn't exist (first boot). |
| `remove(mem, handle)` | Removes the object. |
| `with(mem, &handle, \|v\| ...)` | Runs the closure on the object and returns its result. Reads the object from flash first if it is not in RAM. |
| `with_mut(mem, &handle, \|v\| ...)` | Same, with mutable access. The change stays in RAM until the next flush. |
| `usage(mem)` | Space of the backend: capacity, used, free, reclaimable by garbage collection, number of objects. |
| `flush(mem)` | Writes the changes to the flash: changed and new objects, then the names, then the removals. |
| `flush_object(mem, &handle)` | Writes this object if it changed, then the names and removals if there are any (a new object needs its name stored). The other changed objects stay in RAM. |

`with`, `with_mut`, `insert`, `get_or_insert_with`, `flush` and `flush_object` return `Ok(None)`, having
done nothing, when the call can't be done now:

* **Busy:** a flush has the flash, and the call needs it. That is `with` and `with_mut` on an object that is
  not in RAM or is being written, and the flushes themselves. Only a task of a higher priority than the
  flush, which interrupts it, can get it.
* **Full:** unflushed changes fill the RAM budget, for `with_mut` and `insert` (and `get_or_insert_with`
  when it inserts). `on_full` was called to start a flush.

The errors are `NotFound`, `AlreadyExists`, `WrongType` (the handle's type is not the object's) and
`Backend` (the flash failed).

## How it works

* **Writes.** Only `flush`, `flush_object` and `unmount` write to the flash. A flush takes a few ms per
  object, and hundreds of ms when NOR garbage collection erases a sector. It runs in the caller's task:
  call it from a task of the lowest priority, so that every other task keeps running.
* **Locking.** Each function takes the lock of `mem` for short steps only: never call them inside
  `cx.shared.mem.lock(...)`. The lock blocks the tasks that share `mem`, up to the highest priority among
  them, and no other task. It is held for RAM work, for the closures given to `with` and `with_mut` (keep
  them short), and for the read of an object not in RAM (about 85 µs per 256-byte page of SPI NOR at
  25 MHz). The writes of a flush run outside of it.
* **Busy flash.** While a flush has the flash, objects in RAM keep working; `with` and `with_mut` return
  `None` for the others. A task at the flush's priority or below never sees it: it can't run during the
  flush.
* **RAM budget.** Each object in RAM counts its size plus its serialized size, an estimate of what it owns
  on the heap. Over budget, the least recently used unchanged objects are dropped: they are read again
  when needed. Changed objects stay until a flush; when one more would not fit, the call calls `on_full`
  and returns `None`.
* **Durability.** Changes, inserts and removals reach the flash on `flush` or `unmount`, or on
  `flush_object` for one object: a reset loses the rest. A power cut during a flush leaves each object
  either as it was or as written, and the next mount cleans up. Objects that must stay consistent with
  each other should be flushed together, with `flush`.
* **Names and types.** The names are kept in RAM and rewritten whole by the flush after an `insert` or a
  `remove`: keep them short. An object must be read with the type it was inserted with. A cached object of
  another type gives `WrongType`, but the backend does not check types yet (see TODO).

Details: the `TierMem` documentation in [`src/api.rs`](src/api.rs), the public API. The engine behind it is
in [`src/memory`](src/memory/mod.rs).

## NOR flash backend

`src/backend/nor`: object store on any blocking `embedded_storage::nor_flash::NorFlash` with 1-byte writes
(chips with internal ECC are not supported for now). Every operation blocks until done.

* Objects are serialized with postcard and split in 256-byte pages (20-byte header, 235-byte payload,
  1-byte obsolete marker). Page 0 of each sector holds a sector header with its erase count.
* RAM index: `ObjectId` → current version and page list (`hashbrown::HashMap`, needs a heap).
* Each write gets a new global version. A version counts only once all its pages are on flash, so a power
  cut leaves either the old or the new value. Replaced and removed pages are then marked obsolete.
* Every byte is programmed once between erases, and only from `0xFF`.
* Garbage collection relocates the live pages of the sector with the most garbage and erases it; one free
  sector is kept for it. Ties go to the least worn sector.
* Mount reads the whole region and rebuilds the index (2 MiB at 25 MHz: about 0.7 s).

Requirements, guarantees, time and space complexity: module documentation in
[`src/backend/nor/mod.rs`](src/backend/nor/mod.rs) (`cargo doc --document-private-items`).

## TODO

- [x] Reuse a store crate (`sequential-storage`, `ekv`, `littlefs2`) or write one: own store
- [ ] When to run garbage collection. Today it only runs inside a `write` that lacks space, so that write
  also pays for relocations and erases (up to 400 ms per sector). Options: run it in idle time,
  pre-erase free sectors, trigger on a garbage threshold, bound the work done per call.
- [ ] Wear leveling. Today: the least worn free sector is opened next, and ties between GC victims go to
  the least worn sector. Missing: static wear leveling (sectors holding data that never changes are
  never erased), an erase count kept across a sector header lost to an interrupted erase (it restarts
  at 0), a lifetime estimate for the expected write rate.
- [ ] Format version in the sector header, migration of the on-flash format
- [ ] Mount time: it reads the whole region. Bound it, or persist a checkpoint of the index
- [ ] Edge cases:
  - after a flash error, the RAM state may not match the flash: define the rule (remount?)
  - a page that decays after its write (weak bits after a cut, bit rot) only shows up as `Corrupt` on
    read: the previous version is already marked obsolete, so there is no fallback; optional read-back
    verify after writing
  - a corrupted sector header makes mount ignore the whole sector and its objects
  - skipped dirty pages can eat into the GC reserve; GC refuses a victim whose live pages don't fit
  - peak RAM: `write` and `read` hold the whole serialized object, plus the page list
  - limits: at most 65536 pages in the region (16 MiB with 256-byte pages), `u16` pages per object,
    `u32` version counter (wraps after 4 billion writes)
  - first use of a fresh chip erases each sector when it is first opened (one erase per sector)
  - power cut during mount, which can write (stale pages marked obsolete): not tested yet
- [ ] Protection policy at mount: clear or fail
- [x] Implement `TierMem` (`src/api.rs`, `src/memory`): RTIC shared resource with short locks, directory
  object (id 0), LRU read cache with a RAM budget, changes kept in RAM until a flush
- [ ] RAM accounting: the size of an object in RAM is estimated from its serialized size (spare `Vec`
  capacity is not counted)
- [ ] An object bigger than the whole RAM budget gets `None` forever from `with_mut` and `insert`, since
  no flush can make room: return a real error instead, such as `Error::TooBig`
- [ ] One object that fails to write at every flush (for example with `NoSpace`) blocks all other writes,
  the names included, since a flush stops at the first error: write the others and the names anyway,
  and return the first error at the end
- [ ] The directory is a single point of failure: if it is lost (corrupted sector header), mount removes
  every object
- [ ] Type check on read: a stored type tag or schema hash in the backend, instead of a postcard error
  or a wrong decode
- [ ] Max name length; the directory is rewritten whole on every change
- [ ] Max object size, flash region used by tiermem
- [ ] Schema versioning and migration of stored types
- [x] Host tests with a simulated `NorFlash` (power cuts, two geometries), `cargo test` in CI
