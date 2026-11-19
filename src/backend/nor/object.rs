//! Reading and writing the pages of one object.

use alloc::vec::Vec;

use embedded_storage::nor_flash::NorFlash;

use super::layout::{PAYLOAD_SIZE, PageHeader};
use super::{NorBackend, NorError, Object};
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
    /// the old one.
    pub(super) fn write_data(
        &mut self,
        id: ObjectId,
        data: &[u8],
    ) -> Result<(), NorError<F::Error>> {
        let count = data.len().div_ceil(PAYLOAD_SIZE).max(1);
        let header_count = u16::try_from(count).map_err(|_| NorError::NoSpace)?;
        self.ensure_space(count)?;

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
            let page = self.write_page(&header.encode(chunk))?;
            pages.push(page as u16);
        }

        // Every page is on flash: the new version is complete, the old one can go.
        for &page in &pages {
            self.sectors[Self::sector_of(usize::from(page))].live += 1;
        }
        if let Some(old) = self.objects.insert(id, Object { version, pages }) {
            self.release(&old.pages)?;
        }
        Ok(())
    }

    /// Marks the pages of a replaced or removed version obsolete.
    pub(super) fn release(&mut self, pages: &[u16]) -> Result<(), NorError<F::Error>> {
        for &page in pages {
            let page = usize::from(page);
            self.sectors[Self::sector_of(page)].live -= 1;
            self.mark_obsolete(page)?;
        }
        Ok(())
    }
}
