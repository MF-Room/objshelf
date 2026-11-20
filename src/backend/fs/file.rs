//! On-disk format of an object file.
//!
//! A 16-byte header, then the payload, then zeros up to a multiple of [`BLOCK`] bytes, as
//! `O_DIRECT` needs:
//!
//! ```text
//! offset  size  field
//! 0       4     magic      "OSHF"
//! 4       2     version    FORMAT_VERSION
//! 6       2     zero
//! 8       4     len        payload bytes
//! 12      4     crc        CRC32 of bytes 0..12 and the payload
//! 16      len   payload    postcard of (name, type tag, value)
//! ```
//!
//! A name is at most `MAX_NAME_LEN` (64) bytes, so it always ends in the first block: mount reads
//! one block per object to find the names.

use std::ops::{Deref, DerefMut};

use super::FsError;
use crate::ObjectId;
use crate::backend::crc32;

/// Alignment of the buffers, and unit of the file sizes and of the reads and writes. `O_DIRECT`
/// needs the logical block size of the device, 512 or 4096 bytes: 4096 suits both.
pub(super) const BLOCK: usize = 4096;

/// Bytes before the payload.
pub(super) const HEADER_SIZE: usize = 16;

const MAGIC: [u8; 4] = *b"OSHF";

/// Version of the on-disk format, in each file header. Mount fails on a file of another version
/// instead of skipping it. 0 until the first release: change it when a released format changes.
pub(super) const FORMAT_VERSION: u16 = 0;

/// Size of the file of an object whose payload is `payload_len` bytes: whole blocks.
pub(super) fn file_len(payload_len: usize) -> usize {
    (HEADER_SIZE + payload_len).next_multiple_of(BLOCK)
}

/// Writes the header in front of the payload, which is in `file[HEADER_SIZE..]`.
pub(super) fn seal(file: &mut [u8], payload_len: u32) {
    file[0..4].copy_from_slice(&MAGIC);
    file[4..6].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    file[6..8].fill(0);
    file[8..12].copy_from_slice(&payload_len.to_le_bytes());
    let payload = &file[HEADER_SIZE..][..payload_len as usize];
    let crc = crc32(&[&file[..12], payload]);
    file[12..16].copy_from_slice(&crc.to_le_bytes());
}

/// Checks the header at the start of `start`, the first bytes of a file of `file_len` bytes, and
/// returns the payload length. The payload's CRC is not checked: see [`payload`].
fn check_header(start: &[u8], file_len: usize) -> Result<usize, FsError> {
    if file_len < BLOCK || !file_len.is_multiple_of(BLOCK) || start[0..4] != MAGIC {
        return Err(FsError::Corrupt);
    }
    let version = u16::from_le_bytes([start[4], start[5]]);
    if version != FORMAT_VERSION {
        return Err(FsError::UnsupportedFormat(version));
    }
    let len = u32::from_le_bytes([start[8], start[9], start[10], start[11]]) as usize;
    if start[6..8] != [0, 0] || HEADER_SIZE + len > file_len {
        return Err(FsError::Corrupt);
    }
    Ok(len)
}

/// The payload of the whole `file`, once its header and CRC are checked.
pub(super) fn payload(file: &[u8]) -> Result<&[u8], FsError> {
    let len = check_header(file, file.len())?;
    let payload = &file[HEADER_SIZE..][..len];
    let crc = u32::from_le_bytes([file[12], file[13], file[14], file[15]]);
    if crc32(&[&file[..12], payload]) != crc {
        return Err(FsError::Corrupt);
    }
    Ok(payload)
}

/// The name of the object, from the first `block` of its file of `file_len` bytes. The CRC, over
/// the whole payload, is not checked: [`FsError::Corrupt`] if the name doesn't decode.
pub(super) fn name(block: &[u8], file_len: usize) -> Result<&str, FsError> {
    let len = check_header(block, file_len)?;
    let end = (HEADER_SIZE + len).min(block.len());
    postcard::take_from_bytes::<&str>(&block[HEADER_SIZE..end])
        .map(|(name, _)| name)
        .map_err(|_| FsError::Corrupt)
}

/// Kind of file in the directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    /// `{id:08x}.obj`: the current value of an object.
    Object,
    /// `{id:08x}.tmp`: a write in progress, renamed to the object's file once on disk.
    Temp,
}

/// Name of the file of `id`.
pub(super) fn file_name(id: ObjectId, kind: Kind) -> String {
    let extension = match kind {
        Kind::Object => "obj",
        Kind::Temp => "tmp",
    };
    format!("{:08x}.{extension}", id.0)
}

/// The object and kind of a file name, `None` for a file of something else: 8 lowercase hex
/// digits only, so `._*` AppleDouble files and the lock file are not objects.
pub(super) fn parse_name(name: &str) -> Option<(ObjectId, Kind)> {
    let (hex, extension) = name.split_once('.')?;
    let kind = match extension {
        "obj" => Kind::Object,
        "tmp" => Kind::Temp,
        _ => return None,
    };
    let lowercase_hex = hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if hex.len() != 8 || !lowercase_hex {
        return None;
    }
    let id = u32::from_str_radix(hex, 16).ok()?;
    Some((ObjectId(id), kind))
}

/// Zeroed buffer of whole blocks, starting at an address aligned to [`BLOCK`], as `O_DIRECT`
/// needs. It allocates one block more than its length to find an aligned start.
pub(super) struct AlignedBuf {
    bytes: Vec<u8>,
    start: usize,
    len: usize,
}

impl AlignedBuf {
    /// A buffer of `len` bytes, a multiple of [`BLOCK`].
    pub(super) fn new(len: usize) -> Self {
        debug_assert!(len.is_multiple_of(BLOCK));
        let bytes = vec![0; len + BLOCK];
        let start = bytes.as_ptr().align_offset(BLOCK);
        assert!(start < BLOCK, "a byte pointer can always be aligned");
        Self { bytes, start, len }
    }
}

impl Deref for AlignedBuf {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.bytes[self.start..][..self.len]
    }
}

impl DerefMut for AlignedBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.bytes[self.start..][..self.len]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligned_buffer() {
        for len in [BLOCK, 3 * BLOCK] {
            let buf = AlignedBuf::new(len);
            assert_eq!(buf.len(), len);
            assert!(buf.as_ptr().addr().is_multiple_of(BLOCK));
        }
    }

    #[test]
    fn file_names() {
        let id = ObjectId(0x2a);
        assert_eq!(file_name(id, Kind::Object), "0000002a.obj");
        assert_eq!(parse_name("0000002a.obj"), Some((id, Kind::Object)));
        assert_eq!(parse_name("0000002a.tmp"), Some((id, Kind::Temp)));
        for other in [
            "._0000002a.obj",
            "0000002A.obj",
            "2a.obj",
            "0000002a.bak",
            "lock",
        ] {
            assert_eq!(parse_name(other), None, "{other}");
        }
    }

    #[test]
    fn header_round_trip() {
        let mut file = AlignedBuf::new(BLOCK);
        file[HEADER_SIZE..][..5].copy_from_slice(b"hello");
        seal(&mut file, 5);
        assert_eq!(payload(&file).unwrap(), b"hello");
        file[HEADER_SIZE] ^= 1;
        assert!(matches!(payload(&file), Err(FsError::Corrupt)));
    }

    #[test]
    fn file_sizes_are_whole_blocks() {
        assert_eq!(file_len(0), BLOCK);
        assert_eq!(file_len(BLOCK - HEADER_SIZE), BLOCK);
        assert_eq!(file_len(BLOCK - HEADER_SIZE + 1), 2 * BLOCK);
    }
}
