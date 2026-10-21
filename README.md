# tiermem

Software-managed object memory for embedded Rust, with typed access, transparent residency, and pluggable
storage backends.

## TODO

NOR flash backend, async, generic over `embedded_storage_async::nor_flash::NorFlash`:

- [ ] Decide on the SPI NOR driver: use an existing one or write a generic one.
  Existing: [`w25`](https://github.com/tweedegolf/w25) (Winbond W25 Q/X series, embedded-hal-async 1.0,
  implements the async `NorFlash` traits). Normal read only (≤ 50 MHz), no timeouts on BUSY, does not handle
  write protection, 3-byte addresses only.
- [ ] Reuse a store crate (`sequential-storage`, `ekv`, `littlefs2`) or write one
- [ ] On-flash format: sector header, record layout, CRC, commit marker, format version
- [ ] Index and mount: RAM budget, max object count, mount time
- [ ] Garbage collection and wear leveling
- [ ] Power-loss safety and the durability guarantee of `write` vs. `flush`
- [ ] Roles of the NOR and FRAM tiers
- [ ] Protection policy at mount: clear or fail
- [ ] Max object size, flash region used by tiermem, `ObjectId` allocation
- [ ] Schema versioning and migration of stored types
- [ ] Host tests with a simulated `NorFlash` (power cuts, several geometries), `cargo test` in CI
- [ ] Hardware tests on the MSeq PCB through [mseq_pcb_tb](https://github.com/MF-Room/mseq_pcb_tb)
