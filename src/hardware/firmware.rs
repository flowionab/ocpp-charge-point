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

/// What checking a downloaded firmware image's signature concluded (`docs/PRODUCTION-ROADMAP.md`
/// B3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirmwareVerificationOutcome {
    /// The signature verifies against the signing certificate, and that certificate chains to a
    /// root this charge point trusts. Installation may proceed.
    Valid,
    /// The signature does not verify against the image, or against the certificate - the image
    /// was altered, or was never signed with the key the certificate claims.
    InvalidSignature,
    /// The signature may be fine, but the certificate it was checked against is not one this
    /// charge point trusts: unknown issuer, expired, revoked, or wrong use.
    InvalidSigningCertificate,
}

/// Checks a downloaded firmware image's signature before it is installed
/// (`docs/PRODUCTION-ROADMAP.md` B3.3) - the gate [`crate::firmware`]'s worker runs between
/// [`FileTransfer::download`](crate::hardware::FileTransfer::download) and
/// [`FirmwareInstaller::install`].
///
/// # Why this is a hardware trait, and why it takes no digest
///
/// [`FileTransfer::download`](crate::hardware::FileTransfer::download)'s own docs explain why a
/// downloaded image's bytes never cross into this crate: it targets microcontrollers with
/// kilobytes of RAM, and a multi-megabyte firmware image cannot be buffered to hand back. That
/// same fact decides where verification has to live - **not** a digest-in, signature-out
/// operation on [`crate::hardware::KeyStore`], because computing that digest would require
/// exactly the bytes this crate has deliberately never received. The implementor of this trait is
/// the same implementor that streamed the image somewhere in
/// [`FileTransfer::download`](crate::hardware::FileTransfer::download), so it is the only party
/// that can hash it.
///
/// Verifying also needs two things this crate has never carried a dependency for and does not
/// start now (see [`crate::hardware::CertificateStore`]'s and
/// [`crate::hardware::SoftwareCrypto`]'s module docs for the same stance): parsing the X.509
/// `signing_certificate` and checking it chains to a trusted root, and the asymmetric signature
/// check itself. Both are the implementor's to do, with whatever crypto library and trust store
/// (commonly the same one behind their [`CertificateStore`](crate::hardware::CertificateStore))
/// they already have for TLS. This crate's contribution is calling this trait at the right point
/// in the state machine - after download, strictly before install - and turning the answer into
/// the protocol-correct `FirmwareStatusNotification` and `SecurityEventNotification`.
///
/// # Error handling and the fail-safe default
///
/// [`NoFirmwareVerifier`] - what a charge point has until an integrator wires a real one - always
/// fails to verify. Per `CLAUDE.md`'s "fail-safe over fail-open": a firmware update this crate
/// cannot verify (no verifier configured, or an algorithm the verifier does not support) is
/// refused rather than installed unchecked. An `Err` from [`Self::verify`] is treated exactly the
/// same way by [`crate::firmware`]'s worker as [`FirmwareVerificationOutcome::InvalidSignature`] -
/// "could not verify" and "verified and failed" both mean the image does not get installed.
#[async_trait::async_trait]
pub trait FirmwareVerifier {
    /// The error type returned when verification could not even be attempted.
    type Error: core::error::Error + Send + Sync + 'static;

    /// Verifies the most recently downloaded firmware image against `signing_certificate` (as
    /// carried by `UpdateFirmware`'s/`SignedUpdateFirmware`'s `signingCertificate`, PEM-encoded)
    /// and `signature` (as carried by `signature`, base64-encoded per OCPP's convention).
    ///
    /// Both arrive as `Option` because 2.x's plain `UpdateFirmware` may omit them; `None` for
    /// either is passed straight through rather than treated as a request to skip verification -
    /// [`crate::firmware`] only calls this at all when the update carried at least one of them,
    /// and the choice of what an absent counterpart means (whether the image can be trusted with
    /// only a signature and no certificate, say) is the implementor's, not this crate's.
    async fn verify(
        &self,
        signing_certificate: Option<&str>,
        signature: Option<&str>,
    ) -> Result<FirmwareVerificationOutcome, Self::Error>;
}

#[async_trait::async_trait]
impl<T: FirmwareVerifier + Send + Sync + ?Sized> FirmwareVerifier for alloc::sync::Arc<T> {
    type Error = T::Error;

    async fn verify(
        &self,
        signing_certificate: Option<&str>,
        signature: Option<&str>,
    ) -> Result<FirmwareVerificationOutcome, Self::Error> {
        (**self).verify(signing_certificate, signature).await
    }
}

/// A [`FirmwareVerifier`] for charge points with no signature-verification capability, mirroring
/// [`NoFirmwareInstaller`].
///
/// Always fails - never [`FirmwareVerificationOutcome::Valid`] - so a charge point with no real
/// verifier refuses every signed update rather than installing it unchecked. See the trait's docs
/// for why this is the fail-safe default rather than, say, treating "no verifier" as "nothing to
/// check".
#[derive(Debug, Clone, Copy, Default)]
pub struct NoFirmwareVerifier;

/// The error [`NoFirmwareVerifier`] returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoFirmwareVerifierError;

impl core::fmt::Display for NoFirmwareVerifierError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("this charge point has no firmware signature verification capability")
    }
}

impl core::error::Error for NoFirmwareVerifierError {}

#[async_trait::async_trait]
impl FirmwareVerifier for NoFirmwareVerifier {
    type Error = NoFirmwareVerifierError;

    async fn verify(
        &self,
        _signing_certificate: Option<&str>,
        _signature: Option<&str>,
    ) -> Result<FirmwareVerificationOutcome, Self::Error> {
        Err(NoFirmwareVerifierError)
    }
}

#[cfg(test)]
mod verifier_tests {
    use super::*;

    #[tokio::test]
    async fn a_charge_point_with_no_verifier_refuses_rather_than_claiming_a_signature_is_valid() {
        let verifier = NoFirmwareVerifier;

        assert_eq!(
            verifier
                .verify(Some("-----BEGIN CERTIFICATE-----"), Some("c2ln"))
                .await,
            Err(NoFirmwareVerifierError)
        );
    }
}
