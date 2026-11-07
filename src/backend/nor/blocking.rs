//! A blocking flash driver behind the async flash traits the backend is written with. Each
//! operation runs to completion when called, so every future completes on its first poll.

use embedded_storage::nor_flash as blocking;
use embedded_storage_async::nor_flash::{ErrorType, NorFlash, ReadNorFlash};

pub(super) struct Blocking<F>(pub(super) F);

impl<F: ErrorType> ErrorType for Blocking<F> {
    type Error = F::Error;
}

impl<F: blocking::ReadNorFlash> ReadNorFlash for Blocking<F> {
    const READ_SIZE: usize = F::READ_SIZE;

    async fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
        self.0.read(offset, bytes)
    }

    fn capacity(&self) -> usize {
        self.0.capacity()
    }
}

impl<F: blocking::NorFlash> NorFlash for Blocking<F> {
    const WRITE_SIZE: usize = F::WRITE_SIZE;
    const ERASE_SIZE: usize = F::ERASE_SIZE;

    async fn erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error> {
        self.0.erase(from, to)
    }

    async fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
        self.0.write(offset, bytes)
    }
}
