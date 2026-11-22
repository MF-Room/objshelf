//! A blocking driver for the Winbond W25Q16JV, a 2 MiB SPI NOR flash, as the `NorFlash` the
//! objshelf NOR backend needs: reads and writes of any length at any address, erases by 4 KiB
//! sector.
//!
//! Commands from the datasheet, revision F. Block protection is left as it is: on a protected
//! chip, erases and programs do nothing, and the backend's read-back fails with `VerifyFailed`.

use embedded_hal::delay::DelayNs;
use embedded_hal::spi::{Operation, SpiDevice};
use embedded_storage::nor_flash::{
    ErrorType, NorFlash, NorFlashError, NorFlashErrorKind, ReadNorFlash, check_erase, check_read,
    check_write,
};

const WRITE_ENABLE: u8 = 0x06;
const READ_STATUS_1: u8 = 0x05;
const READ_DATA: u8 = 0x03;
const PAGE_PROGRAM: u8 = 0x02;
const SECTOR_ERASE: u8 = 0x20;
const JEDEC_ID: u8 = 0x9F;
const RELEASE_POWER_DOWN: u8 = 0xAB;

/// Set while an erase or a program runs.
const STATUS_1_BUSY: u8 = 0x01;

/// A page program writes inside one page: past its end, it wraps to the page's start.
const PAGE_SIZE: usize = 256;
const SECTOR_SIZE: usize = 4096;
const CAPACITY: usize = 2 * 1024 * 1024;

#[derive(Debug)]
pub enum Error<E> {
    Spi(E),
    NotAligned,
    OutOfBounds,
    /// The JEDEC ID read is not a W25Q16JV's.
    UnexpectedId([u8; 3]),
}

impl<E: core::fmt::Debug> NorFlashError for Error<E> {
    fn kind(&self) -> NorFlashErrorKind {
        match self {
            Self::NotAligned => NorFlashErrorKind::NotAligned,
            Self::OutOfBounds => NorFlashErrorKind::OutOfBounds,
            _ => NorFlashErrorKind::Other,
        }
    }
}

/// From the `check_*` helpers, which only fail with these two.
impl<E> From<NorFlashErrorKind> for Error<E> {
    fn from(kind: NorFlashErrorKind) -> Self {
        match kind {
            NorFlashErrorKind::NotAligned => Self::NotAligned,
            _ => Self::OutOfBounds,
        }
    }
}

pub struct W25q16jv<S> {
    spi: S,
}

impl<S: SpiDevice> W25q16jv<S> {
    /// Wakes the chip up, in case it was powered down, and checks its JEDEC ID.
    pub fn new(mut spi: S, delay: &mut impl DelayNs) -> Result<Self, Error<S::Error>> {
        spi.write(&[RELEASE_POWER_DOWN]).map_err(Error::Spi)?;
        // tRES1.
        delay.delay_us(3);
        let mut id = [0; 3];
        spi.transaction(&mut [Operation::Write(&[JEDEC_ID]), Operation::Read(&mut id)])
            .map_err(Error::Spi)?;
        // Winbond, IQ (0x40) or IM (0x70) series, 16 Mbit.
        if id[0] != 0xEF || !matches!(id[1], 0x40 | 0x70) || id[2] != 0x15 {
            return Err(Error::UnexpectedId(id));
        }
        Ok(Self { spi })
    }

    /// Sends `command` with the 24-bit address `addr`, then `data`, after a write enable, and
    /// waits until the chip is done: up to 400 ms for a sector erase, 3 ms for a page program.
    fn modify(&mut self, command: u8, addr: u32, data: &[u8]) -> Result<(), Error<S::Error>> {
        self.spi.write(&[WRITE_ENABLE]).map_err(Error::Spi)?;
        self.spi
            .transaction(&mut [
                Operation::Write(&with_addr(command, addr)),
                Operation::Write(data),
            ])
            .map_err(Error::Spi)?;
        loop {
            let mut status = [0];
            self.spi
                .transaction(&mut [
                    Operation::Write(&[READ_STATUS_1]),
                    Operation::Read(&mut status),
                ])
                .map_err(Error::Spi)?;
            if status[0] & STATUS_1_BUSY == 0 {
                return Ok(());
            }
        }
    }
}

fn with_addr(command: u8, addr: u32) -> [u8; 4] {
    let [_, a2, a1, a0] = addr.to_be_bytes();
    [command, a2, a1, a0]
}

impl<S: SpiDevice> ErrorType for W25q16jv<S> {
    type Error = Error<S::Error>;
}

impl<S: SpiDevice> ReadNorFlash for W25q16jv<S> {
    const READ_SIZE: usize = 1;

    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
        check_read(self, offset, bytes.len())?;
        self.spi
            .transaction(&mut [
                Operation::Write(&with_addr(READ_DATA, offset)),
                Operation::Read(bytes),
            ])
            .map_err(Error::Spi)
    }

    fn capacity(&self) -> usize {
        CAPACITY
    }
}

impl<S: SpiDevice> NorFlash for W25q16jv<S> {
    const WRITE_SIZE: usize = 1;
    const ERASE_SIZE: usize = SECTOR_SIZE;

    fn erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error> {
        check_erase(self, from, to)?;
        for addr in (from..to).step_by(SECTOR_SIZE) {
            self.modify(SECTOR_ERASE, addr, &[])?;
        }
        Ok(())
    }

    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
        check_write(self, offset, bytes.len())?;
        let mut addr = offset;
        let mut rest = bytes;
        while !rest.is_empty() {
            // Up to the end of the page.
            let len = rest.len().min(PAGE_SIZE - addr as usize % PAGE_SIZE);
            let (chunk, after) = rest.split_at(len);
            self.modify(PAGE_PROGRAM, addr, chunk)?;
            addr += len as u32;
            rest = after;
        }
        Ok(())
    }
}
