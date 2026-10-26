//! Object store on NOR flash.
//!
//! Objects are serialized with postcard and split in 256-byte pages (see `layout`). Each write of
//! an object gets a new global version, written to free pages. A version only counts once all its
//! pages are on flash, so after a power cut an object holds either its old or its new value.
//! Replaced and removed pages are marked obsolete. Garbage collection relocates the live pages of
//! the sector with the most garbage, then erases it.
//!
//! # Requirements
//!
//! - **Heap**: `alloc` and a global allocator (index, page lists, serialized objects).
//! - **Flash**: `embedded_storage_async::nor_flash::NorFlash` with `WRITE_SIZE == 1` and
//!   `READ_SIZE == 1`, `ERASE_SIZE` a multiple of 256 and at least 512. `MultiwriteNorFlash` is not
//!   needed: every byte is programmed at most once between erases, and only from `0xFF`. Chips with
//!   internal ECC (`WRITE_SIZE > 1`) are not supported.
//! - **Region**: aligned to `ERASE_SIZE`, at least 2 sectors (one is kept free for garbage
//!   collection), at most 65536 pages (16 MiB), used by nothing else.
//! - **Async**: an executor. Storage futures must not be cancelled: a dropped write leaves the RAM
//!   index out of sync with the flash until the next mount.
//! - **Values**: `Serialize` / `DeserializeOwned` (postcard).
//!
//! # Guarantees
//!
//! - A `write` or `remove` is on flash when it returns: nothing is buffered.
//! - Each object is updated atomically. There are no transactions over several objects.
//! - After a power cut at any point, mount restores a consistent state.
//!
//! # Time
//!
//! `N` pages in the region, `S` sectors, `P` pages per sector (16 with 4 KiB sectors), `k` pages
//! of the object (`ceil(len / 235)`, at least 1), `k'` pages of its previous version.
//!
//! | Operation | Flash | CPU |
//! |---|---|---|
//! | `mount` | reads all `N` pages; after a crash, one 1-byte write per stale page | `O(N + S)` |
//! | `read` | `k` page reads | `O(len)` |
//! | `write`, no GC | `k` blank-check reads, `k` page writes, `k'` 1-byte writes | `O(len + S)` |
//! | `remove` | `k'` 1-byte writes | `O(k')` |
//! | `usage` | none | `O(S)` |
//! | GC, per sector reclaimed | up to `2 × (P - 1)` page reads, `P - 1` page writes, 1 erase, 2 small writes | `O(S + P)` |
//!
//! Hash map operations are expected `O(1)`. Garbage collection only runs inside a `write` that
//! lacks space, possibly for several sectors. Opening a sector never used before also erases it.
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
//! the old one is released.

mod layout;
#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;

use alloc::vec;
use alloc::vec::Vec;
use core::cmp::Reverse;
use core::ops::Range;

use embedded_storage_async::nor_flash::NorFlash;
use hashbrown::HashMap;
use serde::{Serialize, de::DeserializeOwned};

use super::{Backend, Usage};
use crate::ObjectId;
use layout::{
    MARKER_OFFSET, PAGE_SIZE, PAYLOAD_SIZE, PageHeader, SectorHeader, is_blank, is_obsolete,
};

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

/// Pages of one version found while mounting.
struct Candidate {
    version: u32,
    pages: Vec<Option<u16>>,
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

    /// Allocated pages that hold no live data.
    fn garbage(&self, sector: usize) -> usize {
        let sector = &self.sectors[sector];
        sector.next_page - 1 - sector.live
    }

    fn is_free(&self, sector: usize) -> bool {
        let s = &self.sectors[sector];
        self.active != Some(sector) && (!s.formatted || s.next_page == 1)
    }

    fn free_sectors(&self) -> usize {
        (0..self.sectors.len()).filter(|&s| self.is_free(s)).count()
    }

    fn active_free_pages(&self) -> usize {
        self.active
            .map_or(0, |s| Self::PAGES_PER_SECTOR - self.sectors[s].next_page)
    }

    /// Pages a write can use while keeping `RESERVED_SECTORS` free.
    fn user_free_pages(&self) -> usize {
        match self.free_sectors().checked_sub(RESERVED_SECTORS) {
            Some(spare) => self.active_free_pages() + spare * Self::DATA_PAGES,
            None => 0,
        }
    }

    /// Erase count of each sector of the region, in address order. A sector whose header was lost
    /// (an erase cut by a power loss) restarts at 0.
    pub fn erase_counts(&self) -> impl ExactSizeIterator<Item = u32> + '_ {
        self.sectors.iter().map(|s| s.erase_count)
    }

    async fn read_page(&mut self, page: usize) -> Result<[u8; PAGE_SIZE], NorError<F::Error>> {
        let mut bytes = [0; PAGE_SIZE];
        let addr = self.page_addr(page);
        self.flash
            .read(addr, &mut bytes)
            .await
            .map_err(NorError::Flash)?;
        Ok(bytes)
    }

    async fn program(&mut self, addr: u32, bytes: &[u8]) -> Result<(), NorError<F::Error>> {
        self.flash.write(addr, bytes).await.map_err(NorError::Flash)
    }

    async fn mark_obsolete(&mut self, page: usize) -> Result<(), NorError<F::Error>> {
        let addr = self.page_addr(page) + MARKER_OFFSET as u32;
        self.program(addr, &[0]).await
    }

    /// Marks the pages of a replaced or removed version obsolete.
    async fn release(&mut self, pages: &[u16]) -> Result<(), NorError<F::Error>> {
        for &page in pages {
            let page = usize::from(page);
            self.sectors[Self::sector_of(page)].live -= 1;
            self.mark_obsolete(page).await?;
        }
        Ok(())
    }

    /// Erases a sector and writes its header.
    async fn erase_sector(&mut self, sector: usize) -> Result<(), NorError<F::Error>> {
        let addr = self.sector_addr(sector);
        let erase_count = self.sectors[sector].erase_count.saturating_add(1);
        self.sectors[sector] = Sector {
            formatted: false,
            erase_count,
            next_page: 1,
            live: 0,
        };
        self.flash
            .erase(addr, addr + F::ERASE_SIZE as u32)
            .await
            .map_err(NorError::Flash)?;
        self.program(addr, &SectorHeader::encode(erase_count))
            .await?;
        self.sectors[sector].formatted = true;
        Ok(())
    }

    /// Makes the least worn free sector the active one.
    async fn open_sector(&mut self) -> Result<usize, NorError<F::Error>> {
        let sector = (0..self.sectors.len())
            .filter(|&s| self.is_free(s))
            .min_by_key(|&s| self.sectors[s].erase_count)
            .ok_or(NorError::NoSpace)?;
        if !self.sectors[sector].formatted {
            self.erase_sector(sector).await?;
        }
        self.active = Some(sector);
        Ok(sector)
    }

    /// Returns a fully erased page. Pages are allocated in order inside a sector and never
    /// reused before the sector is erased.
    async fn alloc_page(&mut self) -> Result<usize, NorError<F::Error>> {
        loop {
            let sector = match self.active {
                Some(s) if self.sectors[s].next_page < Self::PAGES_PER_SECTOR => s,
                _ => self.open_sector().await?,
            };
            let page = sector * Self::PAGES_PER_SECTOR + self.sectors[sector].next_page;
            self.sectors[sector].next_page += 1;
            // A write cut by a power loss can leave a page partly programmed: skip it.
            if is_blank(&self.read_page(page).await?) {
                return Ok(page);
            }
        }
    }

    async fn ensure_space(&mut self, pages: usize) -> Result<(), NorError<F::Error>> {
        while self.user_free_pages() < pages {
            if !self.collect().await? {
                return Err(NorError::NoSpace);
            }
        }
        Ok(())
    }

    /// Reclaims the sector with the most garbage, the least worn one on a tie. Returns `false` if
    /// no sector can be reclaimed.
    async fn collect(&mut self) -> Result<bool, NorError<F::Error>> {
        let Some(victim) = (0..self.sectors.len())
            .filter(|&s| self.sectors[s].formatted && self.garbage(s) > 0)
            .max_by_key(|&s| (self.garbage(s), Reverse(self.sectors[s].erase_count)))
        else {
            return Ok(false);
        };
        let active_free = if self.active == Some(victim) {
            0
        } else {
            self.active_free_pages()
        };
        if self.sectors[victim].live > active_free + self.free_sectors() * Self::DATA_PAGES {
            return Ok(false);
        }
        if self.active == Some(victim) {
            self.active = None;
        }

        for index in 1..self.sectors[victim].next_page {
            let page = victim * Self::PAGES_PER_SECTOR + index;
            let bytes = self.read_page(page).await?;
            let Some(header) = PageHeader::decode(&bytes) else {
                continue;
            };
            let id = ObjectId(header.object_id);
            let slot = usize::from(header.index);
            let live = !is_obsolete(&bytes)
                && self.objects.get(&id).is_some_and(|object| {
                    object.version == header.version
                        && object.pages.get(slot) == Some(&(page as u16))
                });
            if !live {
                continue;
            }
            // Same bytes, header included: until the victim is erased, mount sees two identical
            // copies and keeps one.
            let new = self.alloc_page().await?;
            let addr = self.page_addr(new);
            self.program(addr, &bytes[..MARKER_OFFSET]).await?;
            if let Some(object) = self.objects.get_mut(&id) {
                object.pages[slot] = new as u16;
            }
            self.sectors[Self::sector_of(new)].live += 1;
        }

        // Retire the sector before erasing it, so a cut erase is detected at mount.
        let addr = self.sector_addr(victim) + MARKER_OFFSET as u32;
        self.program(addr, &[0]).await?;
        self.erase_sector(victim).await?;
        Ok(true)
    }

    /// Rebuilds the RAM state from flash and marks every stale page obsolete, so that each
    /// object has a single version left on flash.
    async fn scan(&mut self) -> Result<(), NorError<F::Error>> {
        let mut found: HashMap<ObjectId, Vec<Candidate>> = HashMap::new();
        let mut stale = Vec::new();
        let mut max_version = None;
        // Newest version seen in each sector, to find the sector written last.
        let mut newest = vec![None; self.sectors.len()];

        for (sector, newest) in newest.iter_mut().enumerate() {
            let bytes = self.read_page(sector * Self::PAGES_PER_SECTOR).await?;
            let Some(header) = SectorHeader::decode(&bytes) else {
                continue;
            };
            self.sectors[sector].erase_count = header.erase_count;
            if header.retired {
                continue;
            }
            self.sectors[sector].formatted = true;

            for index in 1..Self::PAGES_PER_SECTOR {
                let page = sector * Self::PAGES_PER_SECTOR + index;
                let bytes = self.read_page(page).await?;
                if is_blank(&bytes) {
                    continue;
                }
                self.sectors[sector].next_page = index + 1;
                let Some(header) = PageHeader::decode(&bytes) else {
                    continue;
                };
                max_version = max_version.max(Some(header.version));
                *newest = (*newest).max(Some(header.version));
                if is_obsolete(&bytes) {
                    continue;
                }

                let candidates = found.entry(ObjectId(header.object_id)).or_default();
                let at = match candidates.iter().position(|c| c.version == header.version) {
                    Some(at) => at,
                    None => {
                        candidates.push(Candidate {
                            version: header.version,
                            pages: vec![None; usize::from(header.count)],
                        });
                        candidates.len() - 1
                    }
                };
                let candidate = &mut candidates[at];
                let slot = usize::from(header.index);
                // A duplicate left by a cut garbage collection, or a count that doesn't match.
                if candidate.pages.len() != usize::from(header.count)
                    || candidate.pages[slot].is_some()
                {
                    stale.push(page);
                } else {
                    candidate.pages[slot] = Some(page as u16);
                }
            }
        }

        for (id, candidates) in found {
            let current = candidates
                .iter()
                .filter(|c| c.pages.iter().all(Option::is_some))
                .map(|c| c.version)
                .max();
            for candidate in candidates {
                let pages: Vec<u16> = candidate.pages.into_iter().flatten().collect();
                if Some(candidate.version) == current {
                    for &page in &pages {
                        self.sectors[Self::sector_of(usize::from(page))].live += 1;
                    }
                    let version = candidate.version;
                    self.objects.insert(id, Object { version, pages });
                } else {
                    stale.extend(pages.into_iter().map(usize::from));
                }
            }
        }
        for page in stale {
            self.mark_obsolete(page).await?;
        }

        // Keep allocating where the last write happened; close the other partly used sectors.
        self.active = (0..self.sectors.len())
            .filter(|&s| {
                let s = &self.sectors[s];
                s.formatted && s.next_page > 1 && s.next_page < Self::PAGES_PER_SECTOR
            })
            .max_by_key(|&s| newest[s]);
        for (index, sector) in self.sectors.iter_mut().enumerate() {
            if sector.formatted && sector.next_page > 1 && self.active != Some(index) {
                sector.next_page = Self::PAGES_PER_SECTOR;
            }
        }
        self.next_version = max_version.map_or(0, |v| v + 1);
        Ok(())
    }
}

impl<F: NorFlash> Backend for NorBackend<F> {
    type Error = NorError<F::Error>;
    type Config = NorConfig<F>;

    async fn mount(config: Self::Config) -> Result<Self, Self::Error> {
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
        backend.scan().await?;
        Ok(backend)
    }

    async fn write<T: Serialize>(&mut self, id: ObjectId, value: &T) -> Result<(), Self::Error> {
        let data = postcard::to_allocvec(value).map_err(NorError::Serialization)?;
        let count = data.len().div_ceil(PAYLOAD_SIZE).max(1);
        let header_count = u16::try_from(count).map_err(|_| NorError::NoSpace)?;
        self.ensure_space(count).await?;

        let version = self.next_version;
        self.next_version += 1;
        let mut pages = Vec::with_capacity(count);
        for index in 0..count {
            let chunk = &data[(index * PAYLOAD_SIZE).min(data.len())
                ..((index + 1) * PAYLOAD_SIZE).min(data.len())];
            let header = PageHeader {
                index: index as u16,
                object_id: id.0,
                version,
                count: header_count,
                len: chunk.len() as u16,
            };
            let page = self.alloc_page().await?;
            let addr = self.page_addr(page);
            self.program(addr, &header.encode(chunk)).await?;
            pages.push(page as u16);
        }

        // Every page is on flash: the new version is complete, the old one can go.
        for &page in &pages {
            self.sectors[Self::sector_of(usize::from(page))].live += 1;
        }
        if let Some(old) = self.objects.insert(id, Object { version, pages }) {
            self.release(&old.pages).await?;
        }
        Ok(())
    }

    async fn read<T: DeserializeOwned>(&mut self, id: ObjectId) -> Result<T, Self::Error> {
        let object = self.objects.get(&id).ok_or(NorError::NotFound)?;
        let version = object.version;
        let pages = object.pages.clone();

        let mut data = Vec::with_capacity(pages.len() * PAYLOAD_SIZE);
        for (index, &page) in pages.iter().enumerate() {
            let bytes = self.read_page(usize::from(page)).await?;
            let header = PageHeader::decode(&bytes).ok_or(NorError::Corrupt)?;
            if header.object_id != id.0
                || header.version != version
                || usize::from(header.index) != index
            {
                return Err(NorError::Corrupt);
            }
            data.extend_from_slice(header.payload(&bytes));
        }
        postcard::from_bytes(&data).map_err(NorError::Serialization)
    }

    async fn remove(&mut self, id: ObjectId) -> Result<(), Self::Error> {
        let object = self.objects.remove(&id).ok_or(NorError::NotFound)?;
        self.release(&object.pages).await
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
}
