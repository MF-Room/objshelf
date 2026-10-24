# tiermem

Software-managed object memory for embedded Rust, with typed access, transparent residency, and pluggable
storage backends.

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
- [ ] Roles of the NOR and FRAM tiers, residency (loading, eviction, write-back)
- [ ] Protection policy at mount: clear or fail
- [ ] Max object size, flash region used by tiermem, `ObjectId` allocation
- [ ] Schema versioning and migration of stored types
- [x] Host tests with a simulated `NorFlash` (power cuts, two geometries), `cargo test` in CI
- [ ] Hardware tests on the MSeq PCB through [mseq_pcb_tb](https://github.com/MF-Room/mseq_pcb_tb)
