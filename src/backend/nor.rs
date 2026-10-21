use embedded_storage_async::nor_flash::NorFlash;

/// Backend storing objects on any NOR flash implementing `NorFlash`.
pub(crate) struct NorBackend<F: NorFlash> {
    flash: F,
}
