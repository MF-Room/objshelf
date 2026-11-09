//! Free space: which sectors are free, and the allocation of sectors and pages.

use embedded_storage::nor_flash::NorFlash;

use super::layout::{SectorHeader, is_blank};
use super::{NorBackend, NorError, RESERVED_SECTORS, Sector};

impl<F: NorFlash> NorBackend<F> {
    /// Allocated pages that hold no live data.
    pub(super) fn garbage(&self, sector: usize) -> usize {
        let sector = &self.sectors[sector];
        sector.next_page - 1 - sector.live
    }

    fn is_free(&self, sector: usize) -> bool {
        let s = &self.sectors[sector];
        self.active != Some(sector) && (!s.formatted || s.next_page == 1)
    }

    pub(super) fn free_sectors(&self) -> usize {
        (0..self.sectors.len()).filter(|&s| self.is_free(s)).count()
    }

    pub(super) fn active_free_pages(&self) -> usize {
        self.active
            .map_or(0, |s| Self::PAGES_PER_SECTOR - self.sectors[s].next_page)
    }

    /// Pages a write can use while keeping `RESERVED_SECTORS` free.
    pub(super) fn user_free_pages(&self) -> usize {
        match self.free_sectors().checked_sub(RESERVED_SECTORS) {
            Some(spare) => self.active_free_pages() + spare * Self::DATA_PAGES,
            None => 0,
        }
    }

    /// Erases a sector and writes its header.
    pub(super) fn erase_sector(&mut self, sector: usize) -> Result<(), NorError<F::Error>> {
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
            .map_err(NorError::Flash)?;
        self.program(addr, &SectorHeader::encode(erase_count))?;
        self.sectors[sector].formatted = true;
        Ok(())
    }

    /// Makes the least worn free sector the active one.
    fn open_sector(&mut self) -> Result<usize, NorError<F::Error>> {
        let sector = (0..self.sectors.len())
            .filter(|&s| self.is_free(s))
            .min_by_key(|&s| self.sectors[s].erase_count)
            .ok_or(NorError::NoSpace)?;
        if !self.sectors[sector].formatted {
            self.erase_sector(sector)?;
        }
        self.active = Some(sector);
        Ok(sector)
    }

    /// Returns a fully erased page. Pages are allocated in order inside a sector and never
    /// reused before the sector is erased.
    pub(super) fn alloc_page(&mut self) -> Result<usize, NorError<F::Error>> {
        loop {
            let sector = match self.active {
                Some(s) if self.sectors[s].next_page < Self::PAGES_PER_SECTOR => s,
                _ => self.open_sector()?,
            };
            let page = sector * Self::PAGES_PER_SECTOR + self.sectors[sector].next_page;
            self.sectors[sector].next_page += 1;
            // A write cut by a power loss can leave a page partly programmed: skip it.
            if is_blank(&self.read_page(page)?) {
                return Ok(page);
            }
        }
    }

    pub(super) fn ensure_space(&mut self, pages: usize) -> Result<(), NorError<F::Error>> {
        while self.user_free_pages() < pages {
            if !self.collect()? {
                return Err(NorError::NoSpace);
            }
        }
        Ok(())
    }
}
