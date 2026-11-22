//! Free space: which sectors are free, and the allocation of sectors and pages.

use embedded_storage::nor_flash::NorFlash;

use super::layout::{COMMIT_OFFSET, COMMITTED, MARKER_OFFSET, SectorHeader, is_blank};
use super::{NorBackend, NorError, RESERVED_SECTORS, Sector, WRITE_ATTEMPTS};

/// What a page is allocated for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Purpose {
    /// A page of a new version of an object. It never takes the free sectors kept for garbage
    /// collection (`RESERVED_SECTORS`).
    Write,
    /// A page moved by garbage collection, which can take any free sector.
    Collect,
}

impl<F: NorFlash> NorBackend<F> {
    /// Allocated pages that hold no live data.
    pub(super) fn garbage(&self, sector: usize) -> usize {
        let sector = &self.sectors[sector];
        sector.next_page - 1 - sector.live
    }

    pub(super) fn is_free(&self, sector: usize) -> bool {
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

    /// Free pages, those of the sectors kept for garbage collection included.
    fn free_pages(&self) -> usize {
        self.active_free_pages() + self.free_sectors() * Self::DATA_PAGES
    }

    /// Pages a write can use while keeping `RESERVED_SECTORS` free.
    pub(super) fn user_free_pages(&self) -> usize {
        match self.free_sectors().checked_sub(RESERVED_SECTORS) {
            Some(spare) => self.active_free_pages() + spare * Self::DATA_PAGES,
            None => 0,
        }
    }

    /// Erases a sector and writes its header, then reads the header back. The commit byte of the
    /// header follows, in a program of its own, and is read back too: a header write cut by a
    /// power loss can leave weak bits, which read right and flip later, after data went in.
    /// Returns `false` if the header or its commit reads back wrong: the sector stays free and
    /// unformatted, to be erased again before use.
    pub(super) fn erase_sector(&mut self, sector: usize) -> Result<bool, NorError<F::Error>> {
        // Once erased, the pages are reused: a page marked then would be another one.
        debug_assert!(
            self.unmarked.is_empty(),
            "a write marks them before collecting garbage"
        );
        let addr = self.sector_addr(sector);
        let erase_count = self.sectors[sector].erase_count.saturating_add(1);
        // The count now includes an erase done, and goes in the header.
        self.sectors[sector] = Sector {
            formatted: false,
            erase_count,
            estimated: false,
            next_page: 1,
            live: 0,
        };
        self.flash
            .erase(addr, addr + F::ERASE_SIZE as u32)
            .map_err(NorError::Flash)?;
        let formatted = self.program_checked(addr, &SectorHeader::encode(erase_count))?
            && self.program_checked(addr + COMMIT_OFFSET as u32, &[COMMITTED])?;
        self.sectors[sector].formatted = formatted;
        Ok(formatted)
    }

    /// Makes the least worn free sector the active one. If its header or its commit reads back
    /// wrong, the least worn free sector is picked again: the failed one now counts one more erase.
    ///
    /// For a write, fails with `NoSpace` if only the sectors kept for garbage collection are free:
    /// pages that read back wrong made the write need more pages than `ensure_space` counted.
    fn open_sector(&mut self, purpose: Purpose) -> Result<usize, NorError<F::Error>> {
        if purpose == Purpose::Write && self.free_sectors() <= RESERVED_SECTORS {
            return Err(NorError::NoSpace);
        }
        for _ in 0..WRITE_ATTEMPTS {
            let sector = (0..self.sectors.len())
                .filter(|&s| self.is_free(s))
                .min_by_key(|&s| self.sectors[s].erase_count)
                .ok_or(NorError::NoSpace)?;
            if self.activate(sector)? {
                return Ok(sector);
            }
        }
        Err(NorError::VerifyFailed)
    }

    /// Makes `sector`, a free one, the active sector, erasing it first if it is not formatted.
    /// Returns `false` if its header or its commit reads back wrong.
    pub(super) fn activate(&mut self, sector: usize) -> Result<bool, NorError<F::Error>> {
        if !self.sectors[sector].formatted && !self.erase_sector(sector)? {
            return Ok(false);
        }
        self.active = Some(sector);
        Ok(true)
    }

    /// Writes `bytes`, a data page without its marker, to a free page, and reads it back. With
    /// `commit`, the last page of a version, it then programs the commit byte, in a program of its
    /// own, and reads it back too. A page that reads back wrong is marked obsolete, and the write
    /// goes to the next free page. Returns the page written.
    pub(super) fn write_page(
        &mut self,
        bytes: &[u8],
        commit: bool,
        purpose: Purpose,
    ) -> Result<usize, NorError<F::Error>> {
        debug_assert!(bytes.len() <= MARKER_OFFSET);
        debug_assert!(!commit || bytes.len() <= COMMIT_OFFSET);
        for _ in 0..WRITE_ATTEMPTS {
            let page = self.alloc_page(purpose)?;
            let addr = self.page_addr(page);
            // Until it reads back right: a flash error can leave it whole, it is marked later.
            self.unmarked.push(page as u16);
            if self.program_checked(addr, bytes)?
                && (!commit || self.program_checked(addr + COMMIT_OFFSET as u32, &[COMMITTED])?)
            {
                self.unmarked.pop();
                return Ok(page);
            }
            self.mark_unmarked()?;
        }
        Err(NorError::VerifyFailed)
    }

    /// Returns a fully erased page. Pages are allocated in order inside a sector and never
    /// reused before the sector is erased.
    fn alloc_page(&mut self, purpose: Purpose) -> Result<usize, NorError<F::Error>> {
        loop {
            let sector = match self.active {
                Some(s) if self.sectors[s].next_page < Self::PAGES_PER_SECTOR => s,
                _ => self.open_sector(purpose)?,
            };
            let page = sector * Self::PAGES_PER_SECTOR + self.sectors[sector].next_page;
            self.sectors[sector].next_page += 1;
            // A write cut by a power loss can leave a page partly programmed: skip it.
            if is_blank(&self.read_page(page)?) {
                return Ok(page);
            }
        }
    }

    /// Collects garbage until a write can use `pages` pages. The first collection also levels the
    /// wear (see `level_wear`), the next ones don't: a move that stopped leaves garbage in the
    /// sector it went to, which the next collection reclaims, and leveling again would go there
    /// again, as it is then the most worn free sector.
    ///
    /// Fails with `NoSpace` when no sector can be collected, or after `WRITE_ATTEMPTS`
    /// collections in a row that free no page: the pages that read back wrong while they moved
    /// the live ones took all they freed, so the live pages would go round the sectors forever.
    pub(super) fn ensure_space(&mut self, pages: usize) -> Result<(), NorError<F::Error>> {
        let mut level = true;
        let (mut most_free, mut stalled) = (self.free_pages(), 0);
        while self.user_free_pages() < pages {
            if !self.collect()? {
                return Err(NorError::NoSpace);
            }
            if self.free_pages() > most_free {
                (most_free, stalled) = (self.free_pages(), 0);
            } else {
                stalled += 1;
                if stalled == WRITE_ATTEMPTS {
                    return Err(NorError::NoSpace);
                }
            }
            if level {
                level = false;
                self.level_wear()?;
            }
        }
        Ok(())
    }
}
