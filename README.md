# tiermem

Software-managed object memory for embedded Rust, with typed access, transparent residency, and pluggable
storage backends.

## Quick start

You need:

* a heap (`alloc`): a global allocator, for example [`embedded-alloc`](https://crates.io/crates/embedded-alloc);
* [RTIC 2](https://rtic.rs), or anything that implements its lock trait (`tiermem::Mutex`, which is
  `rtic::Mutex`);
* a backend. The NOR backend runs on any blocking `embedded_storage::nor_flash::NorFlash` driver with
  `READ_SIZE` and `WRITE_SIZE` of 1 and an `ERASE_SIZE` that is a multiple of 256: an SPI NOR driver,
  internal MCU flash, or a RAM simulation for tests. An async driver is not supported yet.

```toml
[dependencies]
tiermem = { git = "https://github.com/MF-Room/tiermem" }
serde = { version = "1", default-features = false, features = ["derive"] }
```

`TierMem` is an RTIC `#[shared]` resource. Its methods come from the `TierMemExt` trait, implemented for
RTIC's resource proxy (`cx.shared.mem`) and for an owned `TierMem` (as in `init`, before it is shared):

```rust
use serde::{Deserialize, Serialize};
use tiermem::backend::nor::{NorBackend, NorConfig};
use tiermem::{Handle, TierMem, TierMemExt};

type Mem = TierMem<NorBackend<Flash>>;

// Any `Serialize + Deserialize` type can be stored.
#[derive(Default, Serialize, Deserialize)]
struct Settings {
    bpm: u16,
    name: String,
}

#[shared]
struct Shared {
    mem: Mem,
}

#[local]
struct Local {
    // Handles are `Copy`: each task gets its own copy.
    ui_settings: Handle<Settings>,
    tick_settings: Handle<Settings>,
}

#[init]
fn init(cx: init::Context) -> (Shared, Local) {
    // Region used by tiermem: aligned to the erase size, at least 2 sectors.
    let config = NorConfig { flash, range: 0..64 * 4096 };
    // Blocks while the flash is mounted. Objects in RAM take about 16 KiB. When the changes not
    // flushed yet fill them, tiermem calls the closure, which starts the `save` task.
    let mut mem = TierMem::mount(config, 16 * 1024, || {
        save::spawn().ok();
    })
    .unwrap();
    // Finds the object again after a reboot, or creates it the first time.
    let settings = mem.get_or_insert_with("settings", Settings::default).unwrap();
    ui::spawn().unwrap();
    (Shared { mem }, Local { ui_settings: settings, tick_settings: settings })
}

// `with`, `with_mut` and `flush`: from one priority level only, the lowest.
#[task(priority = 1, shared = [mem], local = [ui_settings])]
async fn ui(mut cx: ui::Context) {
    let settings = *cx.local.ui_settings;
    // Reads the object if it is not in RAM.
    let bpm = cx.shared.mem.with(&settings, |s| s.bpm).unwrap();
    // In RAM only: `Err(Full)` if the changes not flushed yet fill the budget.
    if cx.shared.mem.with_mut(&settings, |s| s.bpm = 120).is_ok() {
        save::spawn().ok();
    }
}

// The only task that writes to the flash. Every task above priority 1 keeps running meanwhile.
#[task(priority = 1, shared = [mem])]
async fn save(mut cx: save::Context) {
    cx.shared.mem.flush().unwrap();
}

#[task(binds = RTC_WKUP, priority = 3, shared = [mem], local = [tick_settings])]
fn tick(mut cx: tick::Context) {
    // Never waits: `None` only if the object is not in RAM and a flush has the flash.
    if let Some(bpm) = cx.shared.mem.try_with(cx.local.tick_settings, |s| s.bpm).unwrap() {
        // ...
    }
}

// Doesn't use `mem`: never blocked by tiermem, even during a read.
#[task(binds = USART1, priority = 4)]
fn midi(cx: midi::Context) { /* ... */ }
```

* `list()` returns the names of all objects, `usage()` the backend capacity and the space used, free and
  reclaimable by garbage collection. `get` fails with `NotFound` if the object does not exist, `insert`
  with `AlreadyExists` if it does. Handles are `Copy` and can be stored anywhere.
* `mount` blocks: it reads the whole region (about 0.7 s for 2 MiB of SPI NOR at 25 MHz) and removes the
  objects a reset left without a name.
* Writes: only `flush` (and `unmount`) writes to the flash. Changes made with `with_mut`, `try_with_mut`,
  `insert` and `remove` stay in RAM until then. A flush runs in the caller's task, a few ms per object and
  hundreds of ms when NOR garbage collection runs: call it from a task of the lowest priority.
* Each method takes the RTIC lock of `mem` for short steps only. It blocks the tasks that share `mem` (up
  to the highest priority among them) and no other task: in the example, the MIDI interrupt always runs.
* Reads of objects not in RAM run inside the lock: about 85 µs per 256-byte page of SPI NOR at 25 MHz. So do
  the closures given to `with`, `with_mut`, `try_with` and `try_with_mut`: keep them short. The writes of a
  flush run outside of it.
* Priorities. While a flush has the flash, `try_with` and `try_with_mut` return `None` for an object that
  is not in RAM or is being written; objects in RAM keep working. `with` and `with_mut` wait instead, which
  on a single core never ends if they preempted the flush: call `with`, `with_mut` and `flush` from one
  priority level only. Every other method never waits: call it from anywhere.
* RAM budget: each object in RAM counts its size plus its serialized size, an estimate of what it owns on
  the heap. Over budget, the least recently used unmodified objects are dropped (they are read again when
  needed). Modified objects stay until a flush: when one more would not fit, `with_mut`, `try_with_mut` and
  `insert` call the `on_full` closure given to `mount` and return `Err(Full)` without changing anything.
* Durability: everything reaches the flash on `flush` or `unmount`; a reset loses the rest. A power cut
  during a flush leaves each object either as it was or as written, and the next mount cleans up.
* Names: the whole directory is kept in RAM and rewritten by a flush after each `insert` or `remove`, so
  keep names short.
* Types: an object must be read with the type it was inserted with. A cached object of another type gives
  `WrongType`, but the backend does not check types yet (see TODO).

Details: `TierMem` documentation in [`src/api.rs`](src/api.rs), the public API; the engine behind it is in
[`src/memory`](src/memory/mod.rs).

## NOR flash backend

`src/backend/nor`: object store on any blocking `embedded_storage::nor_flash::NorFlash` with 1-byte writes
(chips with internal ECC are not supported for now). It is written with the async flash traits, behind an
adapter, so that an async driver can be added later.

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
  - after a flash error, or a dropped (cancelled) future, the RAM state may not match the flash: define
    the rule (remount?) and document that storage futures must not be cancelled
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
- [ ] Async flash driver: flushes that let other tasks of the same priority run while the flash programs or
  erases, instead of holding the CPU
- [ ] RAM accounting: the size of an object in RAM is estimated from its serialized size (spare `Vec`
  capacity is not counted)
- [ ] The directory is a single point of failure: if it is lost (corrupted sector header), mount removes
  every object
- [ ] Type check on read: a stored type tag or schema hash in the backend, instead of a postcard error
  or a wrong decode
- [ ] Max name length; the directory is rewritten whole on every change
- [ ] Max object size, flash region used by tiermem
- [ ] Schema versioning and migration of stored types
- [x] Host tests with a simulated `NorFlash` (power cuts, two geometries), `cargo test` in CI
