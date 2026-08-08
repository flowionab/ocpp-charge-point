//! Installing a firmware image that [`FileTransfer`](crate::hardware::FileTransfer) has already
//! fetched - the second half of the hardware surface OCPP's Firmware Management block needs
//! (`docs/PRODUCTION-ROADMAP.md` B3.2).
//!
//! Separate from `FileTransfer` because installing is not transferring: a charge point may be able
//! to fetch a file and unable to flash one (a log uploader with no update partition), and the two
//! are implemented by completely different parts of an integrator's stack. Splitting them also
//! means the Diagnostics block (B5.1) never has to see a trait it would never call.

use alloc::boxed::Box;

/// What happened when the integrator installed a firmware image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirmwareInstallOutcome {
    /// The new firmware is installed and running. The charge point reports OCPP's `Installed` and
    /// carries on.
    Installed,
    /// The image is staged and will run after a restart. The charge point reports
    /// `InstallRebooting` (L01.FR.15) **before** rebooting, because after the reboot there is no
    /// process left to report anything.
    ///
    /// The implementor must **not** reboot inside `install` when returning this: the reboot is
    /// this crate's to issue, once it has told the CSMS what is about to happen.
    RebootRequired,
}

/// Installs a downloaded firmware image.
///
/// The image itself is never passed in - it is wherever
/// [`FileTransfer::download`](crate::hardware::FileTransfer::download) put it, which is somewhere
/// only the integrator knows. This crate's part is deciding *when* installation may begin (OCPP's
/// `installDateTime`, and waiting for transactions to end - L01.FR.05/06), not how it happens.
///
/// # Error handling
///
/// Fallible, and expected to be: an image may be corrupt, a partition may be unwritable, a
/// bootloader may refuse it. A failure is reported to the CSMS as `InstallationFailed` rather than
/// panicking - a charge point that dies mid-update is a truck roll.
#[async_trait::async_trait]
pub trait FirmwareInstaller {
    /// The error type returned by a failed installation.
    type Error: core::error::Error + Send + Sync + 'static;

    /// Installs the most recently downloaded firmware image.
    async fn install(&self) -> Result<FirmwareInstallOutcome, Self::Error>;
}

#[async_trait::async_trait]
impl<T: FirmwareInstaller + Send + Sync + ?Sized> FirmwareInstaller for alloc::sync::Arc<T> {
    type Error = T::Error;

    async fn install(&self) -> Result<FirmwareInstallOutcome, Self::Error> {
        (**self).install().await
    }
}

/// A [`FirmwareInstaller`] for charge points that cannot be updated over the air, mirroring
/// [`NoFileTransfer`](crate::hardware::NoFileTransfer).
///
/// Always fails, so the CSMS is told `InstallationFailed` rather than being left believing an
/// update landed. An integrator with no update path should also leave
/// [`Capabilities::firmware_management`](crate::hardware::Capabilities::firmware_management)
/// `false`, so the handler is never registered at all.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoFirmwareInstaller;

/// The error [`NoFirmwareInstaller`] returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoFirmwareInstallerError;

impl core::fmt::Display for NoFirmwareInstallerError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("this charge point has no firmware installation capability")
    }
}

impl core::error::Error for NoFirmwareInstallerError {}

#[async_trait::async_trait]
impl FirmwareInstaller for NoFirmwareInstaller {
    type Error = NoFirmwareInstallerError;

    async fn install(&self) -> Result<FirmwareInstallOutcome, Self::Error> {
        Err(NoFirmwareInstallerError)
    }
}
