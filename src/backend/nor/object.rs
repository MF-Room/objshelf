//! Reading and writing the pages of one object.

use alloc::vec::Vec;

use embedded_storage::nor_flash::NorFlash;

use super::layout::{PAYLOAD_SIZE, PageHeader};
use super::space::Purpose;
use super::{NorBackend, NorError, Object, WRITE_ATTEMPTS};
use crate::ObjectId;

impl<F: NorFlash> NorBackend<F> {
    /// Appends the payload of page `index` of the object `id` to `data`, once the page is checked.
    pub(super) fn read_payload(
        &mut self,
        id: ObjectId,
        index: usize,
        data: &mut Vec<u8>,
    ) -> Result<(), NorError<F::Error>> {
        let object = &self.objects[&id];
        let (version, page) = (object.version, object.pages[index]);
        let bytes = self.read_page(usize::from(page))?;
        let header = PageHeader::decode(&bytes).ok_or(NorError::Corrupt)?;
        if header.object_id != id.0
            || header.version != version
            || usize::from(header.index) != index
        {
            return Err(NorError::Corrupt);
        }
        data.extend_from_slice(header.payload(&bytes));
        Ok(())
    }

    /// The object `id` as stored: serialized with its name.
    pub(super) fn read_data(&mut self, id: ObjectId) -> Result<Vec<u8>, NorError<F::Error>> {
        let count = self.objects.get(&id).ok_or(NorError::NotFound)?.pages.len();
        let mut data = Vec::with_capacity(count * PAYLOAD_SIZE);
        for index in 0..count {
            self.read_payload(id, index, &mut data)?;
        }
        Ok(data)
    }

    /// Writes `data`, the object `id` serialized with its name, as its new version, then releases
    /// the old one. On an error, the object keeps its old version.
    ///
    /// The write never takes the free sectors kept for garbage collection. Pages that read back
    /// wrong can make it need more pages than `ensure_space` counted, so it can run out: it then
    /// gives up the pages it wrote, collects garbage and starts again, up to `WRITE_ATTEMPTS`
    /// times.
    pub(super) fn write_data(
        &mut self,
        id: ObjectId,
        data: &[u8],
    ) -> Result<(), NorError<F::Error>> {
        let count = data.len().div_ceil(PAYLOAD_SIZE).max(1);
        let header_count = u16::try_from(count).map_err(|_| NorError::NoSpace)?;
        let mut attempt = 1;
        let (version, pages) = loop {
            self.mark_unmarked()?;
            self.ensure_space(count)?;
            match self.write_version(id, data, header_count) {
                Err(NorError::NoSpace) if attempt < WRITE_ATTEMPTS => attempt += 1,
                result => break result?,
            }
        };

        // Every page is on flash and committed: the new version is complete, the old one can go.
        for &page in &pages {
            self.sectors[Self::sector_of(usize::from(page))].live += 1;
        }
        if let Some(old) = self.objects.insert(id, Object { version, pages }) {
            self.release(&old.pages);
        }
        Ok(())
    }

    /// Writes the `count` pages of a new version of the object `id`, the last one committed.
    /// Returns the version and its pages. On an error, the pages written go to `unmarked`.
    fn write_version(
        &mut self,
        id: ObjectId,
        data: &[u8],
        count: u16,
    ) -> Result<(u32, Vec<u16>), NorError<F::Error>> {
        let version = self.next_version;
        self.next_version += 1;
        let mut pages = Vec::with_capacity(usize::from(count));
        for index in 0..count {
            let at = usize::from(index) * PAYLOAD_SIZE;
            let chunk = &data[at.min(data.len())..(at + PAYLOAD_SIZE).min(data.len())];
            let header = PageHeader {
                index,
                object_id: id.0,
                version,
                count,
                len: chunk.len() as u16,
            };
            // The commit, in the last page, once all the others read back right.
            match self.write_page(&header.encode(chunk), index + 1 == count, Purpose::Write) {
                Ok(page) => pages.push(page as u16),
                Err(error) => {
                    // An incomplete version: the page that failed may be whole anyway, with its
                    // commit, which completes it if it was the last one.
                    self.unmarked.extend(pages);
                    return Err(error);
                }
            }
        }
        Ok((version, pages))
    }

    /// Releases the pages of a replaced or removed version: they stop counting as live, and are
    /// marked obsolete. This does not fail: the version is already dead, as a newer one holds the
    /// object or one of its pages is obsolete. Pages the flash fails to mark stay in `unmarked`,
    /// marked before the next write or removal.
    pub(super) fn release(&mut self, pages: &[u16]) {
        for &page in pages {
            self.sectors[Self::sector_of(usize::from(page))].live -= 1;
        }
        self.unmarked.extend_from_slice(pages);
        // On a flash error, the pages left stay in `unmarked`.
        let _ = self.mark_unmarked();
    }
}
