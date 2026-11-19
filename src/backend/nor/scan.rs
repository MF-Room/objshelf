//! Mount: rebuilds the RAM state from flash, then writes again the objects that a power cut
//! may have left with a weak page.

use alloc::collections::BTreeSet;
use alloc::vec;
use alloc::vec::Vec;

use embedded_storage::nor_flash::NorFlash;
use hashbrown::HashMap;

use super::layout::{FORMAT_VERSION, PageHeader, SectorHeader, is_blank, is_obsolete};
use super::{NorBackend, NorError, Object};
use crate::ObjectId;

/// Pages of one version found while mounting.
struct Candidate {
    version: u32,
    pages: Vec<Option<u16>>,
}

impl<F: NorFlash> NorBackend<F> {
    /// Rebuilds the RAM state from flash and marks every stale page obsolete, so that each
    /// object has a single version left on flash. Returns the objects that had stale pages.
    ///
    /// Stale pages are what a power cut leaves behind, next to the last page it hit, which can be
    /// weak. In an update, the old version is not obsolete yet; in a relocation, the moved page
    /// still has its first copy. Neither tells a weak page from a sound one, so the objects
    /// returned are written again. The first write of an object leaves no stale page: cut at its
    /// very end, its last page can be weak and is not caught.
    pub(super) fn scan(&mut self) -> Result<BTreeSet<ObjectId>, NorError<F::Error>> {
        let mut found: HashMap<ObjectId, Vec<Candidate>> = HashMap::new();
        let mut stale = Vec::new();
        let mut suspects = BTreeSet::new();
        let mut max_version = None;
        // Newest version seen in each sector, to find the sector written last.
        let mut newest = vec![None; self.sectors.len()];
        let mut headerless = Vec::new();
        let (mut erase_sum, mut headers) = (0u64, 0u64);

        for (sector, newest) in newest.iter_mut().enumerate() {
            let bytes = self.read_page(sector * Self::PAGES_PER_SECTOR)?;
            let Some(header) = SectorHeader::decode(&bytes) else {
                headerless.push(sector);
                continue;
            };
            if header.version != FORMAT_VERSION {
                return Err(NorError::UnsupportedFormat(header.version));
            }
            self.sectors[sector].erase_count = header.erase_count;
            erase_sum += u64::from(header.erase_count);
            headers += 1;
            if header.retired {
                continue;
            }
            self.sectors[sector].formatted = true;

            for index in 1..Self::PAGES_PER_SECTOR {
                let page = sector * Self::PAGES_PER_SECTOR + index;
                let bytes = self.read_page(page)?;
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
                    suspects.insert(ObjectId(header.object_id));
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
                    suspects.insert(id);
                }
            }
        }
        for page in stale {
            self.mark_obsolete(page)?;
        }

        // A sector without a header lost its erase count to a cut erase, or was never formatted.
        // At 0, it would pass for the least worn sector and be opened again and again until its
        // count caught up: it gets the average count instead.
        if let Some(average) = erase_sum.checked_div(headers) {
            for sector in headerless {
                self.sectors[sector].erase_count = average as u32;
            }
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
        Ok(suspects)
    }

    /// Writes the object `id` again, to new pages, as the power was cut while one of its pages
    /// may have been written: the page can read right, with weak bits that flip later (see
    /// `scan`). Does nothing if the object is gone, reads `Corrupt`, does not fit (`NoSpace`) or
    /// can't be written (`VerifyFailed`): it keeps the pages it has.
    pub(super) fn rewrite(&mut self, id: ObjectId) -> Result<(), NorError<F::Error>> {
        if !self.objects.contains_key(&id) {
            return Ok(());
        }
        let result = self
            .read_data(id)
            .and_then(|data| self.write_data(id, &data));
        match result {
            Err(NorError::NoSpace | NorError::Corrupt | NorError::VerifyFailed) => Ok(()),
            result => result,
        }
    }
}
