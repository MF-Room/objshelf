# tiermem

Software-managed object memory for embedded Rust, with typed access, transparent residency, and pluggable
storage backends.

## Quick start

> Status: the `TierMem` API is defined, its implementation is not written yet (`todo!`).

You need:

* a heap (`alloc`): a global allocator, for example [`embedded-alloc`](https://crates.io/crates/embedded-alloc);
* a backend. The NOR backend runs on any `embedded_storage_async::nor_flash::NorFlash` implementation with
  `READ_SIZE` and `WRITE_SIZE` of 1 and an `ERASE_SIZE` that is a multiple of 256: an SPI NOR driver,
  internal MCU flash, or a RAM simulation for tests.

```toml
[dependencies]
tiermem = { git = "https://github.com/MF-Room/tiermem" }
embedded-storage-async = "0.4"
serde = { version = "1", default-features = false, features = ["derive"] }
```

```rust
use embedded_storage_async::nor_flash::NorFlash;
use serde::{Deserialize, Serialize};
use tiermem::backend::nor::{NorBackend, NorConfig, NorError};
use tiermem::{Error, Handle, TierMem};

// Any `Serialize + Deserialize` type can be stored.
#[derive(Default, Serialize, Deserialize)]
struct Settings {
    brightness: u8,
    timeout_s: u16,
}

fn run<F: NorFlash>(flash: F) -> Result<(), Error<NorError<F::Error>>> {
    // Region used by tiermem: aligned to `F::ERASE_SIZE`, at least 2 sectors.
    let range = 0..64 * F::ERASE_SIZE as u32;
    let mem = TierMem::<NorBackend<F>>::mount(NorConfig { flash, range })?;

    // Find the object again after a reboot, or create it the first time.
    let settings: Handle<Settings> = match mem.get("settings") {
        Ok(handle) => handle,
        Err(Error::NotFound) => mem.insert("settings", Settings::default())?,
        Err(error) => return Err(error),
    };

    // Blocking access: waits if the storage is busy.
    let brightness = mem.with(&settings, |s| s.brightness)?;
    mem.with_mut(&settings, |s| s.timeout_s = 30)?;

    // Non-waiting access, for time-critical code.
    match mem.try_with(&settings, |s| s.brightness) {
        Ok(brightness) => {}
        Err(Error::Busy) => { /* try again later */ }
        Err(error) => return Err(error),
    }

    // Modified objects are written to flash on eviction, `flush` or `unmount`.
    mem.flush()?;
    mem.unmount()
}
```

* `mem.list()` returns the names of all stored objects.
* To share the memory between tasks, put it in a `static` (for example with `static_cell`) and pass
  `&'static TierMem<..>`. Handles are `Copy` and can be stored anywhere.
* The API is blocking: storage operations run to completion and cannot be cancelled. A blocking call
  holds the CPU during storage I/O (up to hundreds of ms when NOR garbage collection runs). On a single
  core, call blocking methods from one priority level only, and `try_*` methods from interrupts and
  time-critical code; otherwise a blocking call can wait forever on a storage that lower-priority code
  holds. See the `TierMem` documentation.

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

- [ ] Decide on the SPI NOR driver: use an existing one or write a generic one.
  Existing: [`w25`](https://github.com/tweedegolf/w25) (Winbond W25 Q/X series, embedded-hal-async 1.0,
  implements the async `NorFlash` traits). Normal read only (≤ 50 MHz), no timeouts on BUSY, does not handle
  write protection, 3-byte addresses only.
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
- [ ] Implement `TierMem` (`src/memory.rs`, Marius): the public API is written, the bodies are `todo!`.
  Open points (`TODO(Marius)` in the code):
  - lock: a busy flag (`AtomicBool`) rather than a critical section, which would disable interrupts
    during storage I/O
  - name → `ObjectId` mapping: a persisted directory object (e.g. id 0) or a hash of the name; max name
    length
  - residency: when to load and evict objects, write-back of modified objects, RAM budget
  - type check on `get` (stored type tag or schema hash) instead of a postcard error
- [ ] Max object size, flash region used by tiermem
- [ ] Schema versioning and migration of stored types
- [x] Host tests with a simulated `NorFlash` (power cuts, two geometries), `cargo test` in CI
- [ ] Hardware tests on the MSeq PCB through [mseq_pcb_tb](https://github.com/MF-Room/mseq_pcb_tb)
