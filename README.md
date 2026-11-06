# tiermem

Software-managed object memory for embedded Rust, with typed access, transparent residency, and pluggable
storage backends.

## Quick start

You need:

* a heap (`alloc`): a global allocator, for example [`embedded-alloc`](https://crates.io/crates/embedded-alloc);
* an async executor with shared resources, written for [RTIC 2](https://rtic.rs): `TierMem` is a
  `#[shared]` resource, accessed through any `rtic::Mutex` (`tiermem::Mutex`);
* a backend. The NOR backend runs on any `embedded_storage_async::nor_flash::NorFlash` implementation with
  `READ_SIZE` and `WRITE_SIZE` of 1 and an `ERASE_SIZE` that is a multiple of 256: an SPI NOR driver,
  internal MCU flash, or a RAM simulation for tests.

```toml
[dependencies]
tiermem = { git = "https://github.com/MF-Room/tiermem" }
```

Two parts:

* `TierMem`, the RAM side: names, cached objects, RAM budget. A `#[shared]` resource. Its methods never
  wait and never do I/O, so every lock is short.
* `Runner`, the `#[local]` resource of a low-priority task: it owns the backend and does all the I/O
  (mount, loads, write-backs, flushes), locking `TierMem` only between operations.

```rust
use tiermem::backend::nor::{NorBackend, NorConfig};
use tiermem::{Handle, Runner, TierMem, storable};

// Serialize, Deserialize and GetSize (RAM accounting). Put it before `#[derive]`.
#[storable]
#[derive(Default)]
struct Settings {
    bpm: u16,
    name: String,
}

#[shared]
struct Shared {
    mem: TierMem<NorBackend<Flash>>,
}

#[local]
struct Local {
    runner: Runner<NorBackend<Flash>>,
}

#[init]
fn init(cx: init::Context) -> (Shared, Local) {
    // Region used by tiermem: aligned to the erase size, at least 2 sectors.
    let config = NorConfig { flash, range: 0..64 * 4096 };
    storage::spawn().unwrap();
    // 16 KiB of RAM for cached objects.
    (Shared { mem: TierMem::new(16 * 1024) }, Local { runner: Runner::new(config) })
}

#[task(priority = 1, shared = [mem], local = [runner])]
async fn storage(mut cx: storage::Context) {
    // Mounts, then serves loads, write-backs and flushes. Returns after `unmount`, or on a mount error.
    let result = cx.local.runner.run(&mut cx.shared.mem).await;
}

#[task(priority = 2, shared = [mem])]
async fn app(mut cx: app::Context) {
    tiermem::mounted(&mut cx.shared.mem).await?;
    // Find the object again after a reboot, or create it the first time.
    let settings: Handle<Settings> =
        cx.shared.mem.lock(|m| m.get_or_insert_with("settings", Settings::default))?;
    // Waits for the load if the object is not in RAM.
    let bpm = tiermem::with(&mut cx.shared.mem, &settings, |s| s.bpm).await?;
    tiermem::with_mut(&mut cx.shared.mem, &settings, |s| s.bpm = 120).await?;
    // Modified objects are written on eviction, `flush` or `unmount`.
    tiermem::flush(&mut cx.shared.mem).await?;
}

#[task(binds = TIM2, priority = 3, shared = [mem])]
fn tick(mut cx: tick::Context) {
    // Never waits: `Ok(None)` if the object is not in RAM; its load is queued, try again later.
    let bpm = cx.shared.mem.lock(|m| m.try_with(&SETTINGS, |s| s.bpm));
}
```

* `list()` returns the names of all objects, `usage()` the backend capacity and the space used, free and
  reclaimable by garbage collection. `get` fails with `NotFound` if the object does not exist, `insert`
  with `AlreadyExists` if it does, and with `TooLarge` if it doesn't fit the RAM budget. Handles are
  `Copy` and can be stored anywhere.
* Closures given to `with`, `with_mut`, `try_with` and `try_with_mut` run inside the lock: keep them short.
* Durability: changes are in RAM when the call returns. They reach the backend when the object is evicted
  (over the RAM budget, least recently used first), on `flush` or on `unmount`. A reset loses the rest.
* Names: the whole directory is kept in RAM and rewritten when it changes, so keep names short.
* Types: an object must be read with the type it was inserted with. A cached object of another type gives
  `WrongType`, but the backend does not check types yet (see TODO).
* `#[storable]` field options: `#[storable(ignore)]`, `#[storable(heap_size = expr)]`,
  `#[storable(heap_size_fn = path)]` to adjust the heap size counted for a field.

Details: `TierMem` documentation in [`src/memory/mod.rs`](src/memory/mod.rs).

## NOR flash backend

`src/backend/nor`: async object store on any `embedded_storage_async::nor_flash::NorFlash` with 1-byte
writes (chips with internal ECC are not supported for now).

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
- [x] Implement `TierMem` (`src/memory`): RTIC shared resource + `Runner` task, directory object (id 0),
  LRU cache with a RAM budget and write-back, `#[storable]` macro
- [ ] Type check on read: a stored type tag or schema hash in the backend, instead of a postcard error
  or a wrong decode
- [ ] Max name length; the directory is rewritten whole on every change
- [ ] Max object size, flash region used by tiermem
- [ ] Schema versioning and migration of stored types
- [x] Host tests with a simulated `NorFlash` (power cuts, two geometries), `cargo test` in CI
