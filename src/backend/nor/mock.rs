//! RAM flash for the tests, with power-cut injection.

use alloc::vec;
use alloc::vec::Vec;
use core::ops::Range;

use embedded_storage_async::nor_flash::{
    ErrorType, NorFlash, NorFlashError, NorFlashErrorKind, ReadNorFlash,
};

use super::layout::PAGE_SIZE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MockError {
    OutOfBounds,
    NotAligned,
    /// The power was cut: the operation is partly done and the flash accepts nothing more.
    PowerCut,
}

impl NorFlashError for MockError {
    fn kind(&self) -> NorFlashErrorKind {
        match self {
            MockError::OutOfBounds => NorFlashErrorKind::OutOfBounds,
            MockError::NotAligned => NorFlashErrorKind::NotAligned,
            MockError::PowerCut => NorFlashErrorKind::Other,
        }
    }
}

/// How the cut operation is left: only its first half done, or only its second half.
#[derive(Debug, Clone, Copy)]
pub(super) enum Cut {
    FirstHalf,
    SecondHalf,
}

/// Flash with 1-byte writes and `ERASE_SIZE` sectors. Like the W25Q16JV, it only programs erased
/// bytes and a write never crosses a 256-byte page: both panic, as they are backend bugs.
pub(super) struct MockFlash<const ERASE_SIZE: usize> {
    pub(super) data: Vec<u8>,
    /// Program and erase operations done so far.
    pub(super) ops: usize,
    cut: Option<(usize, Cut)>,
    dead: bool,
}

impl<const ERASE_SIZE: usize> MockFlash<ERASE_SIZE> {
    pub(super) fn new(sectors: usize) -> Self {
        Self::with_data(vec![0xFF; sectors * ERASE_SIZE])
    }

    /// The same flash after a reboot.
    pub(super) fn with_data(data: Vec<u8>) -> Self {
        Self {
            data,
            ops: 0,
            cut: None,
            dead: false,
        }
    }

    /// Cuts the power during program or erase operation number `op` (0-based).
    pub(super) fn cut_at(mut self, op: usize, cut: Cut) -> Self {
        self.cut = Some((op, cut));
        self
    }

    /// Counts a program or erase operation and returns how it is cut, if it is.
    fn next_op(&mut self) -> Result<Option<Cut>, MockError> {
        if self.dead {
            return Err(MockError::PowerCut);
        }
        let op = self.ops;
        self.ops += 1;
        match self.cut {
            Some((at, cut)) if at == op => {
                self.dead = true;
                Ok(Some(cut))
            }
            _ => Ok(None),
        }
    }

    fn check(&self, offset: u32, len: usize) -> Result<Range<usize>, MockError> {
        let start = offset as usize;
        let end = start.checked_add(len).ok_or(MockError::OutOfBounds)?;
        if end > self.data.len() {
            return Err(MockError::OutOfBounds);
        }
        Ok(start..end)
    }
}

fn part(range: Range<usize>, cut: Option<Cut>) -> Range<usize> {
    let mid = range.start + range.len() / 2;
    match cut {
        None => range,
        Some(Cut::FirstHalf) => range.start..mid,
        Some(Cut::SecondHalf) => mid..range.end,
    }
}

impl<const ERASE_SIZE: usize> ErrorType for MockFlash<ERASE_SIZE> {
    type Error = MockError;
}

impl<const ERASE_SIZE: usize> ReadNorFlash for MockFlash<ERASE_SIZE> {
    const READ_SIZE: usize = 1;

    async fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
        if self.dead {
            return Err(MockError::PowerCut);
        }
        let range = self.check(offset, bytes.len())?;
        bytes.copy_from_slice(&self.data[range]);
        Ok(())
    }

    fn capacity(&self) -> usize {
        self.data.len()
    }
}

impl<const ERASE_SIZE: usize> NorFlash for MockFlash<ERASE_SIZE> {
    const WRITE_SIZE: usize = 1;
    const ERASE_SIZE: usize = ERASE_SIZE;

    async fn erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error> {
        if from > to
            || !(from as usize).is_multiple_of(ERASE_SIZE)
            || !(to as usize).is_multiple_of(ERASE_SIZE)
        {
            return Err(MockError::NotAligned);
        }
        let range = self.check(from, (to - from) as usize)?;
        let cut = self.next_op()?;
        self.data[part(range, cut)].fill(0xFF);
        cut.map_or(Ok(()), |_| Err(MockError::PowerCut))
    }

    async fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
        let range = self.check(offset, bytes.len())?;
        if let (Some(first), Some(last)) = (range.clone().next(), range.clone().last()) {
            assert_eq!(
                first / PAGE_SIZE,
                last / PAGE_SIZE,
                "write crosses a page at {first:#x}"
            );
        }
        if let Some(at) = range.clone().find(|&at| self.data[at] != 0xFF) {
            panic!("programming a byte that is not erased at {at:#x}");
        }
        let cut = self.next_op()?;
        for at in part(range.clone(), cut) {
            self.data[at] &= bytes[at - range.start];
        }
        cut.map_or(Ok(()), |_| Err(MockError::PowerCut))
    }
}
