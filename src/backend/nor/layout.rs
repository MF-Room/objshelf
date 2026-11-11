//! On-flash layout.
//!
//! The region is split in sectors (`NorFlash::ERASE_SIZE`), and sectors in 256-byte pages. Page 0
//! of a sector holds the sector header, the other pages hold object data.
//!
//! Every byte is programmed at most once between two erases, and only from `0xFF`: the markers set
//! after a page is written (page obsolete, sector retired) are the last byte of the page, which the
//! first write leaves erased.

use crc::{CRC_32_ISO_HDLC, Crc};

/// CRC-32 (IEEE 802.3), with its 1 KiB table computed at compile time.
static CRC32: Crc<u32> = Crc::<u32>::new(&CRC_32_ISO_HDLC);

/// Program page size: one Page Program never crosses a page boundary.
pub(super) const PAGE_SIZE: usize = 256;

/// Offset of the marker byte, `0xFF` until the page is obsolete or the sector retired.
pub(super) const MARKER_OFFSET: usize = PAGE_SIZE - 1;

const PAGE_HEADER_SIZE: usize = 20;

/// Object bytes in one data page.
pub(super) const PAYLOAD_SIZE: usize = MARKER_OFFSET - PAGE_HEADER_SIZE;

// `0x4F` is ASCII `O`, for objshelf, and `0x53` is `S`, for sector. Neither magic is all `0xFF`
// (erased) or all `0x00`, and a CRC follows each of them.
const PAGE_MAGIC: u16 = 0x4F01;
const SECTOR_MAGIC: u16 = 0x4F53;

/// Version of the on-flash format, in each sector header. Mount fails on a sector of another
/// version instead of erasing it as unformatted. 0 until the first release: change it when a
/// released format changes.
pub(super) const FORMAT_VERSION: u16 = 0;
const SECTOR_HEADER_SIZE: usize = 12;

/// Header of a data page.
///
/// ```text
/// offset  size  field
/// 0       2     magic      PAGE_MAGIC
/// 2       2     index      position of the page in the object
/// 4       4     object_id
/// 8       4     version    global write counter
/// 12      2     count      number of pages of this version
/// 14      2     len        payload bytes in this page
/// 16      4     crc        CRC32 of bytes 0..16 and the payload
/// 20      235   payload
/// 255     1     obsolete   0xFF while the page is live
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PageHeader {
    pub(super) index: u16,
    pub(super) object_id: u32,
    pub(super) version: u32,
    pub(super) count: u16,
    pub(super) len: u16,
}

impl PageHeader {
    /// Bytes to program for this page: everything but the obsolete marker.
    pub(super) fn encode(&self, payload: &[u8]) -> [u8; MARKER_OFFSET] {
        debug_assert_eq!(payload.len(), usize::from(self.len));
        let mut bytes = [0xFF; MARKER_OFFSET];
        bytes[0..2].copy_from_slice(&PAGE_MAGIC.to_le_bytes());
        bytes[2..4].copy_from_slice(&self.index.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.object_id.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.version.to_le_bytes());
        bytes[12..14].copy_from_slice(&self.count.to_le_bytes());
        bytes[14..16].copy_from_slice(&self.len.to_le_bytes());
        let crc = crc32(&[&bytes[..16], payload]);
        bytes[16..20].copy_from_slice(&crc.to_le_bytes());
        bytes[PAGE_HEADER_SIZE..PAGE_HEADER_SIZE + payload.len()].copy_from_slice(payload);
        bytes
    }

    /// Header of a data page, or `None` if the page holds no complete, valid page (erased, cut
    /// write, corruption).
    pub(super) fn decode(page: &[u8; PAGE_SIZE]) -> Option<Self> {
        if le16(page, 0) != PAGE_MAGIC {
            return None;
        }
        let header = Self {
            index: le16(page, 2),
            object_id: le32(page, 4),
            version: le32(page, 8),
            count: le16(page, 12),
            len: le16(page, 14),
        };
        if header.index >= header.count || usize::from(header.len) > PAYLOAD_SIZE {
            return None;
        }
        let crc = crc32(&[&page[..16], header.payload(page)]);
        (crc == le32(page, 16)).then_some(header)
    }

    pub(super) fn payload<'a>(&self, page: &'a [u8; PAGE_SIZE]) -> &'a [u8] {
        &page[PAGE_HEADER_SIZE..PAGE_HEADER_SIZE + usize::from(self.len)]
    }
}

/// Header of a sector, in its page 0.
///
/// ```text
/// offset  size  field
/// 0       2     magic        SECTOR_MAGIC
/// 2       2     version      FORMAT_VERSION when written by this code
/// 4       4     erase_count
/// 8       4     crc          CRC32 of bytes 0..8
/// 255     1     retired      0xFF until the sector is about to be erased
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SectorHeader {
    /// Version of the on-flash format of the sector.
    pub(super) version: u16,
    pub(super) erase_count: u32,
    /// Set before the sector is erased: if the erase is cut, the sector is erased again.
    pub(super) retired: bool,
}

impl SectorHeader {
    /// Bytes to program right after the erase.
    pub(super) fn encode(erase_count: u32) -> [u8; SECTOR_HEADER_SIZE] {
        let mut bytes = [0; SECTOR_HEADER_SIZE];
        bytes[0..2].copy_from_slice(&SECTOR_MAGIC.to_le_bytes());
        bytes[2..4].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        bytes[4..8].copy_from_slice(&erase_count.to_le_bytes());
        let crc = crc32(&[&bytes[..8]]);
        bytes[8..12].copy_from_slice(&crc.to_le_bytes());
        bytes
    }

    /// Header of a sector, of any format version, or `None` if it has none (never formatted,
    /// erase or header write cut).
    pub(super) fn decode(page: &[u8; PAGE_SIZE]) -> Option<Self> {
        if le16(page, 0) != SECTOR_MAGIC || crc32(&[&page[..8]]) != le32(page, 8) {
            return None;
        }
        Some(Self {
            version: le16(page, 2),
            erase_count: le32(page, 4),
            retired: page[MARKER_OFFSET] != 0xFF,
        })
    }
}

pub(super) fn is_obsolete(page: &[u8; PAGE_SIZE]) -> bool {
    page[MARKER_OFFSET] != 0xFF
}

pub(super) fn is_blank(bytes: &[u8]) -> bool {
    bytes.iter().all(|&b| b == 0xFF)
}

fn le16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// CRC-32 of the concatenated `parts`.
fn crc32(parts: &[&[u8]]) -> u32 {
    let mut digest = CRC32.digest();
    for part in parts {
        digest.update(part);
    }
    digest.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_check_value() {
        assert_eq!(crc32(&[b"1234", b"56789"]), 0xCBF4_3926);
    }

    #[test]
    fn page_round_trip() {
        let header = PageHeader {
            index: 1,
            object_id: 7,
            version: 42,
            count: 3,
            len: 4,
        };
        let mut page = [0xFF; PAGE_SIZE];
        page[..MARKER_OFFSET].copy_from_slice(&header.encode(b"data"));
        assert_eq!(PageHeader::decode(&page), Some(header));
        assert_eq!(header.payload(&page), b"data");
        assert!(!is_obsolete(&page));

        page[21] ^= 1;
        assert_eq!(PageHeader::decode(&page), None);
    }

    #[test]
    fn sector_round_trip() {
        let mut page = [0xFF; PAGE_SIZE];
        page[..SECTOR_HEADER_SIZE].copy_from_slice(&SectorHeader::encode(5));
        let header = SectorHeader {
            version: FORMAT_VERSION,
            erase_count: 5,
            retired: false,
        };
        assert_eq!(SectorHeader::decode(&page), Some(header));

        page[MARKER_OFFSET] = 0;
        assert!(SectorHeader::decode(&page).unwrap().retired);
        assert_eq!(SectorHeader::decode(&[0xFF; PAGE_SIZE]), None);
    }
}
