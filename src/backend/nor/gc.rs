//! Garbage collection: reclaims a sector by moving its live pages, then erasing it.

use core::cmp::Reverse;

use embedded_storage::nor_flash::NorFlash;

use super::layout::{MARKER_OFFSET, PageHeader, is_obsolete};
use super::{NorBackend, NorError};
use crate::ObjectId;

impl<F: NorFlash> NorBackend<F> {
    /// Reclaims the sector with the most garbage, the least worn one on a tie. Returns `false` if
    /// no sector can be reclaimed.
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

        for index in 1..self.sectors[victim].next_page {
            let page = victim * Self::PAGES_PER_SECTOR + index;
            let bytes = self.read_page(page)?;
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
            let new = self.alloc_page()?;
            let addr = self.page_addr(new);
            self.program(addr, &bytes[..MARKER_OFFSET])?;
            if let Some(object) = self.objects.get_mut(&id) {
                object.pages[slot] = new as u16;
            }
            self.sectors[Self::sector_of(new)].live += 1;
        }

        // Retire the sector before erasing it, so a cut erase is detected at mount.
        let addr = self.sector_addr(victim) + MARKER_OFFSET as u32;
        self.program(addr, &[0])?;
        self.erase_sector(victim)?;
        Ok(true)
    }
}
