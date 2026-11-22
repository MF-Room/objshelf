//! Object store on NOR flash.
//!
//! Objects are serialized with postcard, their name and the 4-byte tag of their type first (see
//! [`type_tag`]), and split in 256-byte pages (see `layout`). Each write of an object gets a new
//! global version, written to free pages. A version only counts once all its pages are on flash
//! and its last page is committed, so after a power cut an object holds either its old or its new
//! value.
//! Replaced and removed pages are marked obsolete. Garbage collection relocates the live pages of
//! the sector with the most garbage, then erases it.
//!
//! # Wear
//!
//! - New pages go to the least worn free sector (dynamic wear leveling).
//! - Static wear leveling: a sector whose data never changes is never garbage collected, so it
//!   stops wearing. When the least worn sector holding data is `WEAR_GAP` (100) erases behind the
//!   most worn sector, garbage collection moves its data to the most worn free sector and erases
//!   it. It is then the least worn free sector, so the next one opened. The erase counts of
//!   sectors holding data stay within about `WEAR_GAP` of each other.
//! - A sector found without a header at mount (an erase cut by a power loss, or never formatted)
//!   gets the average erase count of the others.
//! - [`Backend::wear`] reports the erase counts, for a lifetime estimate (README, Lifetime). It
//!   leaves out those averages, until each of these sectors is erased: they are estimates, and
//!   on a fresh chip they would add up to erases that never happened.
//!
//! # Requirements
//!
//! - **Heap**: `alloc` and a global allocator (index, page lists, serialized objects).
//! - **Flash**: a blocking `embedded_storage::nor_flash::NorFlash` driver with `WRITE_SIZE == 1` and
//!   `READ_SIZE == 1`, `ERASE_SIZE` a multiple of 256 and at least 512. `MultiwriteNorFlash` is not
//!   needed: every byte is programmed at most once between erases, and only from `0xFF`. Chips with
//!   internal ECC (`WRITE_SIZE > 1`) are not supported.
//! - **Region**: aligned to `ERASE_SIZE`, at least 3 sectors (two are kept free for garbage
//!   collection), at most 65536 pages (16 MiB), used by nothing else: a sector without an
//!   objshelf header is erased when it is needed.
//! - **Blocking**: each flash operation runs to completion when the driver is called, and so does
//!   each backend operation.
//! - **Values**: `Serialize` / `DeserializeOwned` (postcard).
//!
//! # Guarantees
//!
//! - A `write` or `remove` is on flash when it returns: nothing is buffered.
//! - Each object is updated atomically. There are no transactions over several objects.
//! - After a power cut at any point, mount restores a consistent state.
//! - A flash error that is not a power cut (the driver returns an error) fails the operation
//!   without changing what the backend reports, and a retry finishes it: a failed `write` keeps the
//!   object's old value, a failed `remove` keeps the object. After a reboot, the failed operation
//!   may be done or not, as after a power cut. The RAM index never gets ahead of the flash: a
//!   `remove` marks a page of the object obsolete before it forgets the object, and a page whose
//!   obsolete mark fails is kept in a list, marked before the next `write` or `remove`. Such a
//!   mark, for a version already replaced, doesn't fail the `write`.
//! - A program cut by a power loss can leave weak bits: they read right, then flip later. So a
//!   version only counts once its commit byte, in its last page, is programmed: a program of its
//!   own, done once all the pages of the version read back right. A write whose pages were cut is
//!   never taken for done, even if they read right. When the cut hit the commit byte, leaving it
//!   partly programmed, or a garbage collection copying a page, mount writes the object again.
//!   Left: a commit byte that the cut left programmed but weak; all 8 bits have to flip back for
//!   the version to stop counting.
//! - A sector header has a commit byte too, programmed once the header reads back right, and data
//!   only goes in after it. A sector whose header is valid but not committed, and holds no data,
//!   is erased again before use: a weak header could fail its CRC later and take the sector's
//!   data with it.
//! - Every page, commit byte and sector header is read back right after its write, while the data
//!   is still in RAM. A page or its commit byte that reads back wrong gets the page marked
//!   obsolete and written again on the next free page. A sector header or its commit byte that
//!   reads back wrong leaves its sector unformatted, and the least worn free sector is picked
//!   again. After 3 tries, the write fails with [`NorError::VerifyFailed`].
//! - A write never takes the 2 free sectors kept for garbage collection. A garbage collection
//!   moves at most one sector of live pages, so when pages that read back wrong in the first free
//!   sector make it need more room, it goes on in the second one. Pages that read back wrong can
//!   also make a write need more pages than it counted: when it runs out, it gives up the pages it
//!   wrote, collects garbage and starts again. After 3 tries, it fails with
//!   [`NorError::NoSpace`]. So does a write whose collections free no page 3 times in a row: the
//!   pages that read back wrong took all they freed, and more collections would move the same
//!   live pages round the sectors forever.
//! - A static wear leveling move is optional and tried once per write at most: if it runs out of
//!   pages anyway, or they keep reading back wrong, it stops and the write goes on. The sector it
//!   left half moved is collected later.
//! - Each sector header holds the version of the on-flash format: mount fails with
//!   [`NorError::UnsupportedFormat`] on a sector of another version, instead of erasing it as
//!   unformatted.
//!
//! # Time
//!
//! `N` pages in the region, `S` sectors, `P` pages per sector (16 with 4 KiB sectors), `k` pages
//! of the object (`ceil(len / 234)`, at least 1), `k'` pages of its previous version.
//!
//! | Operation | Flash | CPU |
//! |---|---|---|
//! | `mount` | reads all `N` pages; after a crash, one 1-byte read and write per stale page and a `write` of each object that had some | `O(N + S)` |
//! | `read` | `k` page reads | `O(len)` |
//! | `write`, no GC | `k` blank-check reads, `k` page writes, `k` read-backs, a 1-byte write and read-back (the commit), `k'` 1-byte reads and writes | `O(len + S)` |
//! | `remove` | `k'` 1-byte reads and writes | `O(k')` |
//! | `usage` | none | `O(S)` |
//! | GC, per sector reclaimed | up to `3 × (P - 1)` page reads, `P - 1` page writes, 1 erase, 3 small writes, 3 small reads | `O(S + P)` |
//!
//! A page or commit byte that reads back wrong costs one more page: a blank check, a write, a
//! read-back and a 1-byte read and write, plus the commit again for a last page. When that runs a
//! write out of pages, it starts again after a garbage collection: the pages written so far, once
//! more. A mark reads its byte first, as one retried after a flash error may be done already.
//! Hash map operations are expected `O(1)`.
//!
//! Garbage collection only runs inside a `write` that lacks space, possibly for several sectors.
//! When the wear gets uneven, its first collection also moves one sector of data that never
//! changes: the cost of reclaiming a sector, once more. Opening a sector without a header also
//! erases it, even if it reads blank: an erase cut by a power loss can leave cells that read
//! `0xFF` but are not fully erased, and nothing tells them apart from a sector never used. So
//! does opening a sector whose header is not committed. So on a fresh chip, the first writes
//! erase each sector once, and mount reads only page 0 of each sector.
//!
//! On the W25Q16JV at 25 MHz: page read 82 µs of SPI transfer, 0.23 ms with the blocking driver
//! of `examples/rtic_nor`; page write 0.4 ms (3 ms max), sector erase 45 ms (400 ms max), mount
//! of 2 MiB ≈ 1.9 s with that driver. The read-back adds a page read to each page write: 20% of a
//! typical program at the transfer speed, 60% with that driver.
//!
//! # Space
//!
//! RAM, 32-bit target:
//!
//! - index: about 24 B per object plus 2 B per page. Each page belongs to at most one object, so
//!   page lists total at most `2 × N` bytes (16 KiB for 2 MiB);
//! - sector table: 16 B per sector (8 KiB for 2 MiB with 4 KiB sectors);
//! - pages left to mark after a flash error: 2 B each, none otherwise;
//! - `read` and `write`: the whole serialized object, plus a 256 B page buffer on the stack;
//! - `mount`, temporarily: 4 B per page found, 8 B per sector.
//!
//! Flash: 234 payload bytes per 256-byte page, page 0 of each sector for its header, two sectors
//! kept free, at least one page per object. On a whole W25Q16JV (512 sectors) that is at most
//! 510 × 15 × 234 B ≈ 1.71 MiB of live data. An update also needs room for its new version before
//! the old one is released. An object, serialized with its name and type tag, takes at most 65535
//! pages (about 15 MiB) and must fit in the free space: `write` returns [`NorError::NoSpace`]
//! otherwise.
//!
//! Versions are a `u32` counter, and mount keeps the highest one: the flash wears out before it
//! can wrap. Each write programs at least one page, and a W25Q16JV takes about 768 million page
//! writes in its life (512 sectors × 15 data pages × 100 000 erases), against 4.3 billion
//! versions. Only a region of about 11 MiB or more could get there.

mod gc;
mod layout;
#[cfg(test)]
pub(crate) mod mock;
mod object;
mod scan;
mod space;
#[cfg(test)]
mod tests;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::ops::Range;

use embedded_storage::nor_flash::NorFlash;
use hashbrown::HashMap;
use serde::{Serialize, de::DeserializeOwned};

use super::{Backend, Usage, Wear, decode, type_tag};
use crate::ObjectId;
use layout::{MARKER_OFFSET, PAGE_SIZE, PAYLOAD_SIZE};

/// Free sectors kept so garbage collection always has room to relocate live pages: writes never
/// take them. One holds the live pages of any sector; the second takes the pages that the first
/// can't, as some of its pages read back wrong.
const RESERVED_SECTORS: usize = 2;

/// Static wear leveling: when the least worn sector holding data is this many erases behind the
/// most worn sector, garbage collection moves its data to the most worn free sector. A lower gap
/// spreads the wear more evenly, at the cost of more moves.
const WEAR_GAP: u32 = 100;

/// Pages tried for one page of data, and sectors for one sector header, when they or their commit
/// byte read back wrong after their write, before giving up with [`NorError::VerifyFailed`]. Also
/// the tries of a write that runs out of pages, and the collections in a row that free no page,
/// before giving up with [`NorError::NoSpace`].
const WRITE_ATTEMPTS: usize = 3;

/// Configuration of a [`NorBackend`].
pub struct NorConfig<F> {
    /// Flash driver.
    pub flash: F,
    /// Flash region used by the backend: aligned to `F::ERASE_SIZE`, at least 3 sectors.
    pub range: Range<u32>,
}

/// Errors of a [`NorBackend`].
#[derive(Debug)]
pub enum NorError<E> {
    /// The flash driver failed.
    Flash(E),
    /// Unsupported region or flash geometry (`WRITE_SIZE` and `READ_SIZE` must be 1).
    InvalidConfig,
    /// No object with this id.
    NotFound,
    /// Not enough free space, even after garbage collection: pages that read back wrong take
    /// space too.
    NoSpace,
    /// A page of the object failed its checks.
    Corrupt,
    /// The value could not be serialized or deserialized.
    Serialization(postcard::Error),
    /// A sector holds another version of the on-flash format, the one given, written by other
    /// firmware: the mount fails without erasing anything.
    UnsupportedFormat(u16),
    /// A page of data, or its commit byte, read back wrong right after its write on 3 pages in a
    /// row, or a sector header or its commit byte in 3 sectors: the flash is probably worn out.
    /// Nothing was replaced: the object keeps its previous value.
    VerifyFailed,
}

/// Current version of an object and its pages, in order.
struct Object {
    version: u32,
    pages: Vec<u16>,
}

#[derive(Debug, Clone, Copy)]
struct Sector {
    /// Erased with a valid header, committed or followed by data. Other sectors are erased before
    /// use.
    formatted: bool,
    erase_count: u32,
    /// `erase_count` is an estimate: the sector had no header at mount, so it got the average count
    /// of the others (see `scan`). Its real count is unknown, so `wear` leaves it out until the
    /// sector is erased again.
    estimated: bool,
    /// Next page to allocate. Page 0 holds the sector header.
    next_page: usize,
    /// Pages holding the current version of an object.
    live: usize,
}

/// Object store on any NOR flash with 1-byte writes.
pub struct NorBackend<F: NorFlash> {
    flash: F,
    start: u32,
    sectors: Vec<Sector>,
    objects: HashMap<ObjectId, Object>,
    /// Sector new pages are allocated from.
    active: Option<usize>,
    next_version: u32,
    /// Pages that hold no live data but may still hold a whole page of it, not marked obsolete
    /// as the flash failed: a version replaced or removed, a write or a relocation cut by the
    /// error. Marked before the next write or removal, so that no old version comes back at
    /// mount.
    unmarked: Vec<u16>,
}

impl<F: NorFlash> NorBackend<F> {
    const PAGES_PER_SECTOR: usize = F::ERASE_SIZE / PAGE_SIZE;
    const DATA_PAGES: usize = Self::PAGES_PER_SECTOR - 1;

    fn page_addr(&self, page: usize) -> u32 {
        self.start + (page * PAGE_SIZE) as u32
    }

    fn sector_addr(&self, sector: usize) -> u32 {
        self.start + (sector * F::ERASE_SIZE) as u32
    }

    fn sector_of(page: usize) -> usize {
        page / Self::PAGES_PER_SECTOR
    }

    /// Erase count of each sector of the region, in address order. A sector found without a header
    /// at mount (an erase cut by a power loss, or never formatted) gets the average count of the
    /// sectors with one, which [`Backend::wear`] leaves out.
    pub fn erase_counts(&self) -> impl ExactSizeIterator<Item = u32> + '_ {
        self.sectors.iter().map(|s| s.erase_count)
    }

    #[cfg(test)]
    pub(crate) fn flash_mut(&mut self) -> &mut F {
        &mut self.flash
    }

    /// The flash driver, to mount the same flash again as after a reboot.
    #[cfg(test)]
    pub(crate) fn into_flash(self) -> F {
        self.flash
    }

    fn read_page(&mut self, page: usize) -> Result<[u8; PAGE_SIZE], NorError<F::Error>> {
        let mut bytes = [0; PAGE_SIZE];
        let addr = self.page_addr(page);
        self.flash.read(addr, &mut bytes).map_err(NorError::Flash)?;
        Ok(bytes)
    }

    fn program(&mut self, addr: u32, bytes: &[u8]) -> Result<(), NorError<F::Error>> {
        self.flash.write(addr, bytes).map_err(NorError::Flash)
    }

    /// Programs `bytes`, within one page, then reads them back. `false` if they differ: cells
    /// that did not program, as in a worn sector.
    fn program_checked(&mut self, addr: u32, bytes: &[u8]) -> Result<bool, NorError<F::Error>> {
        self.program(addr, bytes)?;
        let mut back = [0; PAGE_SIZE];
        let back = &mut back[..bytes.len()];
        self.flash.read(addr, back).map_err(NorError::Flash)?;
        Ok(back == bytes)
    }

    fn mark_obsolete(&mut self, page: usize) -> Result<(), NorError<F::Error>> {
        self.program_marker(self.page_addr(page) + MARKER_OFFSET as u32)
    }

    /// Programs the marker byte at `addr`, unless it is already: a mark retried after a flash
    /// error may have been done, and a byte is only programmed once between erases.
    fn program_marker(&mut self, addr: u32) -> Result<(), NorError<F::Error>> {
        let mut marker = [0];
        self.flash
            .read(addr, &mut marker)
            .map_err(NorError::Flash)?;
        if marker[0] == 0xFF {
            self.program(addr, &[0])?;
        }
        Ok(())
    }

    /// Marks the pages of `unmarked` obsolete. Stops at the first flash error, keeping the pages
    /// left.
    fn mark_unmarked(&mut self) -> Result<(), NorError<F::Error>> {
        while let Some(&page) = self.unmarked.last() {
            self.mark_obsolete(usize::from(page))?;
            self.unmarked.pop();
        }
        Ok(())
    }
}

impl<F: NorFlash + 'static> Backend for NorBackend<F> {
    type Error = NorError<F::Error>;
    type Config = NorConfig<F>;

    fn mount(config: Self::Config) -> Result<Self, Self::Error> {
        let NorConfig { flash, range } = config;
        let erase_size = F::ERASE_SIZE as u32;
        let geometry_ok = F::WRITE_SIZE == 1
            && F::READ_SIZE == 1
            && F::ERASE_SIZE >= 2 * PAGE_SIZE
            && F::ERASE_SIZE.is_multiple_of(PAGE_SIZE);
        let range_ok = geometry_ok
            && range.start < range.end
            && range.start.is_multiple_of(erase_size)
            && range.end.is_multiple_of(erase_size)
            && range.end as usize <= flash.capacity();
        if !range_ok {
            return Err(NorError::InvalidConfig);
        }
        let sectors = ((range.end - range.start) / erase_size) as usize;
        if sectors <= RESERVED_SECTORS || sectors * Self::PAGES_PER_SECTOR > 1 << 16 {
            return Err(NorError::InvalidConfig);
        }

        let unformatted = Sector {
            formatted: false,
            erase_count: 0,
            estimated: false,
            next_page: 1,
            live: 0,
        };
        let mut backend = Self {
            flash,
            start: range.start,
            sectors: vec![unformatted; sectors],
            objects: HashMap::new(),
            active: None,
            next_version: 0,
            unmarked: Vec::new(),
        };
        for id in backend.scan()? {
            backend.rewrite(id)?;
        }
        Ok(backend)
    }

    fn write<T: Serialize>(
        &mut self,
        id: ObjectId,
        name: &str,
        value: &T,
    ) -> Result<(), Self::Error> {
        let tag = type_tag::<T>().to_le_bytes();
        let data = postcard::to_allocvec(&(name, tag, value)).map_err(NorError::Serialization)?;
        self.write_data(id, &data)
    }

    fn read<T: DeserializeOwned>(&mut self, id: ObjectId) -> Result<Option<T>, Self::Error> {
        let data = self.read_data(id)?;
        decode(&data).map_err(NorError::Serialization)
    }

    /// Reads the pages of the object until its name is whole: the first one only, unless the name
    /// is longer than about 230 bytes.
    fn name(&mut self, id: ObjectId) -> Result<String, Self::Error> {
        let count = self.objects.get(&id).ok_or(NorError::NotFound)?.pages.len();
        let mut data = Vec::new();
        for index in 0..count {
            self.read_payload(id, index, &mut data)?;
            match postcard::take_from_bytes::<&str>(&data) {
                Ok((name, _)) => return Ok(name.into()),
                Err(postcard::Error::DeserializeUnexpectedEnd) if index + 1 < count => {}
                Err(error) => return Err(NorError::Serialization(error)),
            }
        }
        unreachable!("an object has at least one page")
    }

    /// Marks the first page of the object obsolete, then releases the others. If the flash fails
    /// before the first page is marked, nothing changed: the object is still here.
    fn remove(&mut self, id: ObjectId) -> Result<(), Self::Error> {
        let first = self.objects.get(&id).ok_or(NorError::NotFound)?.pages[0];
        // An older version left unmarked would come back at mount once this one is gone.
        self.mark_unmarked()?;
        // Mount ignores a version with an obsolete page: the object is gone from flash, so it
        // can go from RAM.
        self.mark_obsolete(usize::from(first))?;
        let object = self.objects.remove(&id).expect("found above");
        self.sectors[Self::sector_of(usize::from(first))].live -= 1;
        self.release(&object.pages[1..]);
        Ok(())
    }

    /// In whole pages of 234 payload bytes. `capacity` leaves out the sector headers and the
    /// sector kept for garbage collection; `free` is what a write can use without collecting.
    fn usage(&self) -> Usage {
        let user_sectors = self.sectors.len() - RESERVED_SECTORS;
        let used: usize = self.sectors.iter().map(|s| s.live).sum();
        let garbage: usize = (0..self.sectors.len()).map(|s| self.garbage(s)).sum();
        Usage {
            capacity: user_sectors * Self::DATA_PAGES * PAYLOAD_SIZE,
            used: used * PAYLOAD_SIZE,
            free: self.user_free_pages() * PAYLOAD_SIZE,
            reclaimable: garbage * PAYLOAD_SIZE,
            objects: self.objects.len(),
        }
    }

    fn ids(&self) -> Vec<ObjectId> {
        self.objects.keys().copied().collect()
    }

    /// From the erase counts of the sectors (see `erase_counts`), without those of the sectors
    /// found without a header at mount and not erased since: they only hold an estimate.
    fn wear(&self) -> Option<Wear> {
        let counts = (self.sectors.iter())
            .filter(|s| !s.estimated)
            .map(|s| s.erase_count);
        Some(Wear {
            sectors: self.sectors.len() as u32,
            most_worn: counts.clone().max().unwrap_or(0),
            total_erases: counts.map(u64::from).sum(),
        })
    }
}
