//! Garbage collection: reclaims a sector by moving its live pages, then erasing it. It also
//! levels the wear of sectors whose data never changes (static wear leveling).

use alloc::vec::Vec;
use core::cmp::Reverse;

use embedded_storage::nor_flash::NorFlash;

use super::layout::{MARKER_OFFSET, PageHeader};
use super::{NorBackend, NorError, WEAR_GAP};
use crate::ObjectId;

impl<F: NorFlash> NorBackend<F> {
    /// Reclaims the sector with the most garbage, the least worn one on a tie, then levels the
    /// wear (see `level_wear`). Returns `false` if no sector can be reclaimed.
    pub(super) fn collect(&mut self) -> Result<bool, NorError<F::Error>> {
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
        self.reclaim(victim)?;
        self.level_wear()?;
        Ok(true)
    }

    /// Static wear leveling. A sector whose data never changes has no garbage, so the garbage
    /// collection never picks it: it stops wearing, and the other sectors take every erase. When
    /// the least worn sector holding data is `WEAR_GAP` erases behind the most worn sector, its
    /// data moves to the most worn free sector, where it can stay, and it is erased: as the least
    /// worn free sector, it is the next one opened. Moves at most one sector.
    ///
    /// Free space doesn't change: the data takes one free sector and frees another. Only sectors
    /// with a header count: one without may have worn out, its header failing to write.
    fn level_wear(&mut self) -> Result<(), NorError<F::Error>> {
        let most_worn = (self.sectors.iter())
            .filter(|s| s.formatted)
            .map(|s| s.erase_count)
            .max();
        let Some(cold) = (0..self.sectors.len())
            .filter(|&s| self.sectors[s].live > 0 && self.active != Some(s))
            .min_by_key(|&s| self.sectors[s].erase_count)
        else {
            return Ok(());
        };
        let Some(target) = (0..self.sectors.len())
            .filter(|&s| self.is_free(s) && self.sectors[s].formatted)
            .max_by_key(|&s| self.sectors[s].erase_count)
        else {
            return Ok(());
        };
        let cold_count = self.sectors[cold].erase_count;
        if most_worn.is_none_or(|most| most - cold_count < WEAR_GAP)
            || self.sectors[target].erase_count <= cold_count
        {
            return Ok(());
        }

        // The pages are allocated from the active sector: the target is, during the move.
        let previous = self.active;
        if !self.activate(target)? {
            return Ok(());
        }
        let moved = self.reclaim(cold);
        // Back to the previous active sector, if it has room. The sector the data went to is then
        // closed, as pages are only allocated from the active sector.
        if let (Some(previous), Some(current)) = (previous, self.active)
            && previous != current
            && self.sectors[previous].next_page < Self::PAGES_PER_SECTOR
        {
            if self.sectors[current].next_page > 1 {
                self.sectors[current].next_page = Self::PAGES_PER_SECTOR;
            }
            self.active = Some(previous);
        }
        moved
    }

    /// Moves the live pages of `sector` to the active sector, then erases it.
    fn reclaim(&mut self, sector: usize) -> Result<(), NorError<F::Error>> {
        let mut moved = Vec::new();
        let result = self.relocate(sector, &mut moved).and_then(|()| {
            // Retire the sector before erasing it, so a cut erase is detected at mount.
            self.program_marker(self.sector_addr(sector) + MARKER_OFFSET as u32)
        });
        if let Err(error) = result {
            // Not retired, the sector still counts at mount: the first copies of the moved pages
            // would bring their version back once the new copies are marked, by a removal.
            self.unmarked.extend(moved);
            return Err(error);
        }
        // Reclaimed even if its header reads back wrong: it is then erased again before use.
        self.erase_sector(sector)?;
        Ok(())
    }

    /// Moves the live pages of `sector` to the active sector, adding the pages moved to `moved`.
    fn relocate(&mut self, sector: usize, moved: &mut Vec<u16>) -> Result<(), NorError<F::Error>> {
        for index in 1..self.sectors[sector].next_page {
            let page = sector * Self::PAGES_PER_SECTOR + index;
            let bytes = self.read_page(page)?;
            let Some(header) = PageHeader::decode(&bytes) else {
                continue;
            };
            let id = ObjectId(header.object_id);
            let slot = usize::from(header.index);
            // RAM decides, not the marker: a removal whose mark was written although the flash
            // reported an error still has its object, until it is retried.
            let live = self.objects.get(&id).is_some_and(|object| {
                object.version == header.version && object.pages.get(slot) == Some(&(page as u16))
            });
            if !live {
                continue;
            }
            // Same bytes, header included: until the sector is erased, mount sees two identical
            // copies and keeps one. The old copy no longer counts as live, so the RAM state
            // stays right if a later relocation fails.
            let new = self.write_page(&bytes[..MARKER_OFFSET])?;
            if let Some(object) = self.objects.get_mut(&id) {
                object.pages[slot] = new as u16;
            }
            self.sectors[Self::sector_of(new)].live += 1;
            self.sectors[sector].live -= 1;
            moved.push(page as u16);
        }
        Ok(())
    }
}
