//! Object store on NOR flash.
//!
//! Objects are serialized with postcard, their name first, and split in 256-byte pages (see
//! `layout`). Each write of an object gets a new global version, written to free pages. A version only counts once all its
//! pages are on flash, so after a power cut an object holds either its old or its new value.
//! Replaced and removed pages are marked obsolete. Garbage collection relocates the live pages of
//! the sector with the most garbage, then erases it.
//!
//! # Requirements
//!
//! - **Heap**: `alloc` and a global allocator (index, page lists, serialized objects).
//! - **Flash**: a blocking `embedded_storage::nor_flash::NorFlash` driver with `WRITE_SIZE == 1` and
//!   `READ_SIZE == 1`, `ERASE_SIZE` a multiple of 256 and at least 512. `MultiwriteNorFlash` is not
//!   needed: every byte is programmed at most once between erases, and only from `0xFF`. Chips with
//!   internal ECC (`WRITE_SIZE > 1`) are not supported.
//! - **Region**: aligned to `ERASE_SIZE`, at least 2 sectors (one is kept free for garbage
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
//! - A page write cut by a power loss can leave weak bits: the page reads right, then turns
//!   `Corrupt` later. When the cut hit an update or a garbage collection, mount sees it and writes
//!   the object again. When it hit the very end of an object's first write, nothing tells.
//! - Each sector header holds the version of the on-flash format: mount fails with
//!   [`NorError::UnsupportedFormat`] on a sector of another version, instead of erasing it as
//!   unformatted.
//!
//! # Time
//!
//! `N` pages in the region, `S` sectors, `P` pages per sector (16 with 4 KiB sectors), `k` pages
//! of the object (`ceil(len / 235)`, at least 1), `k'` pages of its previous version.
//!
//! | Operation | Flash | CPU |
//! |---|---|---|
//! | `mount` | reads all `N` pages; after a crash, one 1-byte write per stale page and a `write` of each object that had some | `O(N + S)` |
//! | `read` | `k` page reads | `O(len)` |
//! | `write`, no GC | `k` blank-check reads, `k` page writes, `k'` 1-byte writes | `O(len + S)` |
//! | `remove` | `k'` 1-byte writes | `O(k')` |
//! | `usage` | none | `O(S)` |
//! | GC, per sector reclaimed | up to `2 × (P - 1)` page reads, `P - 1` page writes, 1 erase, 2 small writes | `O(S + P)` |
//!
//! Hash map operations are expected `O(1)`. Garbage collection only runs inside a `write` that
//! lacks space, possibly for several sectors. Opening a sector without a header also erases it,
//! even if it reads blank: an erase cut by a power loss can leave cells that read `0xFF` but are
//! not fully erased, and nothing tells them apart from a sector never used. So on a fresh chip,
//! the first writes erase each sector once, and mount reads only page 0 of each sector.
//!
//! On the W25Q16JV at 25 MHz: page read ≈ 0.1 ms, page write 0.4 ms (3 ms max), sector erase
//! 45 ms (400 ms max), mount of 2 MiB ≈ 0.7 s of SPI reads.
//!
//! # Space
//!
//! RAM, 32-bit target:
//!
//! - index: about 24 B per object plus 2 B per page. Each page belongs to at most one object, so
//!   page lists total at most `2 × N` bytes (16 KiB for 2 MiB);
//! - sector table: 16 B per sector (8 KiB for 2 MiB with 4 KiB sectors);
//! - `read` and `write`: the whole serialized object, plus a 256 B page buffer on the stack;
//! - `mount`, temporarily: 4 B per page found, 8 B per sector.
//!
//! Flash: 235 payload bytes per 256-byte page, page 0 of each sector for its header, one sector
//! kept free, at least one page per object. On a whole W25Q16JV (512 sectors) that is at most
//! 511 × 15 × 235 B ≈ 1.72 MiB of live data. An update also needs room for its new version before
//! the old one is released. An object, serialized with its name, takes at most 65535 pages
//! (about 15 MiB) and must fit in the free space: `write` returns [`NorError::NoSpace`] otherwise.

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

use super::{Backend, Usage};
use crate::ObjectId;
use layout::{MARKER_OFFSET, PAGE_SIZE, PAYLOAD_SIZE};

/// Free sectors kept so garbage collection always has room to relocate live pages.
const RESERVED_SECTORS: usize = 1;

/// Configuration of a [`NorBackend`].
pub struct NorConfig<F> {
    /// Flash driver.
    pub flash: F,
    /// Flash region used by the backend: aligned to `F::ERASE_SIZE`, at least 2 sectors.
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
    /// Not enough free space, even after garbage collection.
    NoSpace,
    /// A page of the object failed its checks.
    Corrupt,
    /// The value could not be serialized or deserialized.
    Serialization(postcard::Error),
    /// A sector holds another version of the on-flash format, the one given, written by other
    /// firmware: the mount fails without erasing anything.
    UnsupportedFormat(u16),
}

/// Current version of an object and its pages, in order.
struct Object {
    version: u32,
    pages: Vec<u16>,
}

#[derive(Debug, Clone, Copy)]
struct Sector {
    /// Erased with a valid header. Other sectors are erased before use.
    formatted: bool,
    erase_count: u32,
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

    /// Erase count of each sector of the region, in address order. A sector whose header was lost
    /// (an erase cut by a power loss) restarts at 0.
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

    fn mark_obsolete(&mut self, page: usize) -> Result<(), NorError<F::Error>> {
        let addr = self.page_addr(page) + MARKER_OFFSET as u32;
        self.program(addr, &[0])
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
        let data = postcard::to_allocvec(&(name, value)).map_err(NorError::Serialization)?;
        self.write_data(id, &data)
    }

    fn read<T: DeserializeOwned>(&mut self, id: ObjectId) -> Result<T, Self::Error> {
        let data = self.read_data(id)?;
        let (_name, value): (&str, T) =
            postcard::from_bytes(&data).map_err(NorError::Serialization)?;
        Ok(value)
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

    fn remove(&mut self, id: ObjectId) -> Result<(), Self::Error> {
        let object = self.objects.remove(&id).ok_or(NorError::NotFound)?;
        self.release(&object.pages)
    }

    /// In whole pages of 235 payload bytes. `capacity` leaves out the sector headers and the
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
}
