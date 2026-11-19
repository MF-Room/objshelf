//! RAM flash for the tests, with power-cut injection.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::ops::Range;
use std::sync::Mutex;

use embedded_storage::nor_flash::{
    ErrorType, NorFlash, NorFlashError, NorFlashErrorKind, ReadNorFlash,
};

use super::layout::PAGE_SIZE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MockError {
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
pub(crate) enum Cut {
    FirstHalf,
    SecondHalf,
}

/// Flash with 1-byte writes and `ERASE_SIZE` sectors. Like the W25Q16JV, it only programs erased
/// bytes and a write never crosses a 256-byte page: both panic, as they are backend bugs.
pub(crate) struct MockFlash<const ERASE_SIZE: usize> {
    pub(crate) data: Vec<u8>,
    /// Program and erase operations done so far.
    pub(super) ops: usize,
    /// Offset of the write the power was cut in, if a write was cut.
    pub(super) cut_write: Option<u32>,
    cut: Option<(usize, Cut)>,
    dead: bool,
    /// Runs at the start of each program operation, as an interrupt during a write.
    pub(crate) on_write: Option<Box<dyn FnMut() + Send>>,
    /// Worn pages: their byte 0 no longer programs and stays `0xFF`.
    pub(super) worn: Vec<usize>,
    /// Gets a copy of the data when the power is cut (see `copy_on_cut`).
    left: Option<Arc<Mutex<Vec<u8>>>>,
}

impl<const ERASE_SIZE: usize> MockFlash<ERASE_SIZE> {
    pub(crate) fn new(sectors: usize) -> Self {
        Self::with_data(vec![0xFF; sectors * ERASE_SIZE])
    }

    /// The same flash after a reboot.
    pub(crate) fn with_data(data: Vec<u8>) -> Self {
        Self {
            data,
            ops: 0,
            cut_write: None,
            cut: None,
            dead: false,
            on_write: None,
            worn: Vec::new(),
            left: None,
        }
    }

    /// Cuts the power during program or erase operation number `op` (0-based).
    pub(crate) fn cut_at(mut self, op: usize, cut: Cut) -> Self {
        self.cut = Some((op, cut));
        self
    }

    /// Copies the data to `left` when the power is cut, as the flash is lost with a mount that
    /// fails.
    pub(crate) fn copy_on_cut(mut self, left: Arc<Mutex<Vec<u8>>>) -> Self {
        self.left = Some(left);
        self
    }

    /// Cuts the power now: every later operation fails.
    pub(crate) fn power_cut(&mut self) {
        self.dead = true;
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

    /// Ends an operation: `PowerCut` if it was cut, after copying the data to `left`.
    fn end(&self, cut: Option<Cut>) -> Result<(), MockError> {
        if cut.is_none() {
            return Ok(());
        }
        if let Some(left) = &self.left {
            left.lock().unwrap().clone_from(&self.data);
        }
        Err(MockError::PowerCut)
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

    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
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

    fn erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error> {
        if from > to
            || !(from as usize).is_multiple_of(ERASE_SIZE)
            || !(to as usize).is_multiple_of(ERASE_SIZE)
        {
            return Err(MockError::NotAligned);
        }
        let range = self.check(from, (to - from) as usize)?;
        let cut = self.next_op()?;
        self.data[part(range, cut)].fill(0xFF);
        self.end(cut)
    }

    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
        if let Some(interrupt) = &mut self.on_write {
            interrupt();
        }
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
        if cut.is_some() {
            self.cut_write = Some(offset);
        }
        for at in part(range.clone(), cut) {
            if at % PAGE_SIZE == 0 && self.worn.contains(&(at / PAGE_SIZE)) {
                continue;
            }
            self.data[at] &= bytes[at - range.start];
        }
        self.end(cut)
    }
}
