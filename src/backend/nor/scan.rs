//! Mount: rebuilds the RAM state from flash, then writes again the objects that a power cut
//! may have left with a weak page or commit byte. A sector whose header a power cut may have
//! left weak is erased again before use.

use alloc::collections::BTreeSet;
use alloc::vec;
use alloc::vec::Vec;

use embedded_storage::nor_flash::NorFlash;
use hashbrown::HashMap;

use super::layout::{
    Commit, FORMAT_VERSION, PageHeader, SectorHeader, commit_of, is_blank, is_obsolete,
};
use super::{NorBackend, NorError, Object};
use crate::ObjectId;

/// Pages of one version found while mounting.
struct Candidate {
    version: u32,
    pages: Vec<Option<u16>>,
    /// Commit byte of the last page found.
    commit: Commit,
}

impl<F: NorFlash> NorBackend<F> {
    /// Rebuilds the RAM state from flash and marks every stale page obsolete, so that each
    /// object has a single version left on flash. Returns the objects to write again.
    ///
    /// A version counts once all its pages are there and its last page is committed. A program
    /// cut by a power loss can leave weak bits, which read right and flip later. The commit is
    /// programmed once all the pages read back right, so the pages of a committed version are
    /// whole. What a cut can still leave weak, and the object is written again for:
    /// - a commit byte, when the cut hit the commit: if it is left partly programmed, it shows;
    /// - a page copied by a garbage collection, in one program with its commit byte: it then has
    ///   two copies, as its sector is not erased yet.
    ///
    /// An object with any other stale page is written again too: an uncommitted version can be
    /// the rewrite of an earlier mount, cut before its commit.
    ///
    /// A sector header has a commit byte too, programmed once the header reads back right, and
    /// data only goes in after it. A valid header without its commit, in a sector without data,
    /// may be weak: the sector is erased again before use. A sector with data was committed when
    /// the data went in, so its header is whole, even if the commit byte has decayed since.
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

                let id = ObjectId(header.object_id);
                let candidates = found.entry(id).or_default();
                let at = match candidates.iter().position(|c| c.version == header.version) {
                    Some(at) => at,
                    None => {
                        candidates.push(Candidate {
                            version: header.version,
                            pages: vec![None; usize::from(header.count)],
                            commit: Commit::Erased,
                        });
                        candidates.len() - 1
                    }
                };
                let candidate = &mut candidates[at];
                let slot = usize::from(header.index);
                if candidate.pages.len() != usize::from(header.count) {
                    stale.push(page);
                    suspects.insert(id);
                    continue;
                }
                let last = slot + 1 == candidate.pages.len();
                let commit = if last {
                    commit_of(&bytes)
                } else {
                    Commit::Erased
                };
                match candidate.pages[slot] {
                    None => {
                        candidate.pages[slot] = Some(page as u16);
                        candidate.commit = candidate.commit.max(commit);
                    }
                    // A duplicate left by a cut garbage collection. Of a last page, the copy can
                    // be the first one found, whole but with its commit byte not programmed yet.
                    Some(kept) => {
                        suspects.insert(id);
                        if commit > candidate.commit {
                            stale.push(usize::from(kept));
                            candidate.pages[slot] = Some(page as u16);
                            candidate.commit = commit;
                        } else {
                            stale.push(page);
                        }
                    }
                }
            }
            let has_data = self.sectors[sector].next_page > 1;
            self.sectors[sector].formatted = header.committed || has_data;
        }

        for (id, candidates) in found {
            let current = candidates
                .iter()
                .filter(|c| c.commit > Commit::Erased && c.pages.iter().all(Option::is_some))
                .map(|c| c.version)
                .max();
            for candidate in candidates {
                let pages: Vec<u16> = candidate.pages.into_iter().flatten().collect();
                if Some(candidate.version) == current {
                    if candidate.commit == Commit::Partial {
                        suspects.insert(id);
                    }
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

    /// Writes the object `id` again, to new pages, as a power cut may have left one of its pages
    /// or its commit byte weak: it reads right, with bits that flip later (see `scan`). Does
    /// nothing if the object is gone, reads `Corrupt`, does not fit (`NoSpace`) or can't be
    /// written (`VerifyFailed`): it keeps the pages it has.
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
