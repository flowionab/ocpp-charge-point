//! Serving a firmware image [`FileTransfer`](crate::hardware::FileTransfer) has already
//! downloaded to other charge points on the local network - the publishing half of OCPP's
//! local-controller Firmware Publishing block (`docs/PRODUCTION-ROADMAP.md` B3.4:
//! `PublishFirmware`/`UnpublishFirmware`/`PublishFirmwareStatusNotification`, 2.x only).
//!
//! # Why this is hardware surface
//!
//! Nothing about "serve a file to other devices on the local network" is an OCPP concern any
//! more than fetching one is (see [`crate::hardware::file_transfer`]'s module docs, which make the
//! same argument for the download direction): the protocol hands over an MD5 checksum and expects
//! one or more URLs back, and says nothing about how those URLs come to answer requests. Whether
//! that is a tiny embedded HTTP server, a directory an existing web server already serves, an FTP
//! daemon, or a vendor's own OTA relay is entirely the integrator's choice. A charge point that is
//! not acting as a local controller - the overwhelming majority of deployments - has no server to
//! publish anything with at all, which is why this is a capability
//! ([`Capabilities::firmware_publishing`](crate::hardware::Capabilities::firmware_publishing)) and
//! not something every charge point gets for free.
//!
//! # Relationship to `FileTransfer` and `FirmwareInstaller`
//!
//! `PublishFirmware` downloads the image from the location the CSMS names using exactly the same
//! [`FileTransfer::download`](crate::hardware::FileTransfer::download) path - including its
//! `retries`/`retryInterval` handling - that `UpdateFirmware` uses (see [`crate::firmware`] and
//! [`crate::publish_firmware`]). This trait covers only what happens *after* that download
//! succeeds: making the same image reachable at however many local URLs the integrator's server
//! publishes it at, and taking it back down again. It has nothing to do with
//! [`FirmwareInstaller`](crate::hardware::FirmwareInstaller) - a local controller republishing
//! firmware for the stations behind it may have no update partition of its own to install onto,
//! and a station with one may have no server to publish from.
//!
//! Like `FileTransfer::download`, the bytes never cross into this crate here either, for the same
//! RAM reasons: this is written for microcontrollers, and the only thing this crate would do with
//! a multi-megabyte image is hand it straight back to be served.
//!
//! # Checksum verification
//!
//! `checksum` is the MD5 the CSMS asked to have published, over the same file `download` just
//! fetched. This crate has no hashing/crypto of its own - the same gap that leaves `UpdateFirmware`
//! unable to verify a signature (`docs/PRODUCTION-ROADMAP.md` B3.3) - so it cannot check the
//! checksum itself. The integrator's [`publish`](FirmwarePublisher::publish) implementation *does*
//! hold the file, though, and is the right place to verify it before serving anything under a
//! checksum that does not match: returning an error from `publish` is reported to the CSMS as
//! `PublishFailed`, which is the honest answer for a checksum mismatch just as much as for a full
//! disk.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

/// Serves (and stops serving) a downloaded firmware image on the local network, so charge points
/// behind this local controller can fetch it without each hitting the CSMS. See the module docs
/// for why this is hardware surface, why it is separate from
/// [`FirmwareInstaller`](crate::hardware::FirmwareInstaller), and why checksum verification
/// belongs here rather than in this crate.
///
/// # Error handling
///
/// Fallible, and expected to be, per `CLAUDE.md`: the local server may be out of disk space, a
/// port may already be bound, a checksum the integrator chooses to verify may not match. A failure
/// is reported to the CSMS as `PublishFailed` rather than panicking - claiming a successful publish
/// that never happened would tell downstream charge points to fetch from a URL that answers
/// nothing.
///
/// # Identity
///
/// A published image is identified - to the CSMS, and back to this trait via
/// [`unpublish`](Self::unpublish) - by its MD5 checksum, never by an id this crate assigns. That
/// mirrors the wire: `UnpublishFirmwareRequest` carries only a checksum.
#[async_trait::async_trait]
pub trait FirmwarePublisher {
    /// The error type returned by a failed publish or unpublish.
    type Error: core::error::Error + Send + Sync + 'static;

    /// Publishes the most recently downloaded firmware image - the one
    /// [`FileTransfer::download`](crate::hardware::FileTransfer::download) most recently fetched -
    /// under `checksum`, and returns every URL it is now reachable at.
    ///
    /// Called only once a download has already succeeded - never with nothing to publish. Success
    /// must return at least one URL: `PublishFirmwareStatusNotification`'s `location` is mandatory
    /// on `Published` (OCPP's PublishFirmware requirements), and an empty list would be a
    /// `Published` status the CSMS could not act on.
    async fn publish(&self, checksum: &str) -> Result<Vec<String>, Self::Error>;

    /// Stops publishing the image identified by `checksum`.
    ///
    /// Returns `true` if something was actually taken down, `false` if `checksum` was not
    /// currently published by this implementor - the two cases `UnpublishFirmware`'s
    /// `Unpublished`/`NoFirmware` statuses need to tell apart. [`crate::publish_firmware`] already
    /// tracks which checksums it believes are published and only calls this for one it does, so a
    /// `false` here is a divergence between this crate's bookkeeping and the server's real state
    /// (e.g. an operator manually cleared it) rather than the normal case.
    async fn unpublish(&self, checksum: &str) -> Result<bool, Self::Error>;
}

#[async_trait::async_trait]
impl<T: FirmwarePublisher + Send + Sync + ?Sized> FirmwarePublisher for alloc::sync::Arc<T> {
    type Error = T::Error;

    async fn publish(&self, checksum: &str) -> Result<Vec<String>, Self::Error> {
        (**self).publish(checksum).await
    }

    async fn unpublish(&self, checksum: &str) -> Result<bool, Self::Error> {
        (**self).unpublish(checksum).await
    }
}

/// A [`FirmwarePublisher`] for charge points that are not acting as a local controller, mirroring
/// [`NoFirmwareInstaller`](crate::hardware::NoFirmwareInstaller).
///
/// Every operation fails with [`NoFirmwarePublisherError`], rather than succeeding silently: a
/// charge point that reported `Published` without publishing anything would send downstream
/// stations to a URL that was never actually going to answer. An integrator with no local-server
/// story should also leave
/// [`Capabilities::firmware_publishing`](crate::hardware::Capabilities::firmware_publishing)
/// `false`, so `PublishFirmware`/`UnpublishFirmware` refuse before ever reaching this trait (C5 -
/// see `crate::refusal`) rather than being accepted and then failing here.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoFirmwarePublisher;

/// The error every [`NoFirmwarePublisher`] operation returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoFirmwarePublisherError;

impl core::fmt::Display for NoFirmwarePublisherError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(
            "this charge point is not a local controller and has no firmware-publishing capability",
        )
    }
}

impl core::error::Error for NoFirmwarePublisherError {}

#[async_trait::async_trait]
impl FirmwarePublisher for NoFirmwarePublisher {
    type Error = NoFirmwarePublisherError;

    async fn publish(&self, _checksum: &str) -> Result<Vec<String>, Self::Error> {
        Err(NoFirmwarePublisherError)
    }

    async fn unpublish(&self, _checksum: &str) -> Result<bool, Self::Error> {
        Err(NoFirmwarePublisherError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_charge_point_without_a_publisher_fails_rather_than_pretending() {
        let publisher = NoFirmwarePublisher;

        assert!(
            publisher
                .publish("d41d8cd98f00b204e9800998ecf8427e")
                .await
                .is_err()
        );
        assert!(
            publisher
                .unpublish("d41d8cd98f00b204e9800998ecf8427e")
                .await
                .is_err()
        );
    }

    struct FakePublisher {
        published: alloc::sync::Arc<
            embassy_sync::blocking_mutex::Mutex<
                embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
                core::cell::RefCell<Vec<String>>,
            >,
        >,
    }

    #[async_trait::async_trait]
    impl FirmwarePublisher for FakePublisher {
        type Error = NoFirmwarePublisherError;

        async fn publish(&self, checksum: &str) -> Result<Vec<String>, Self::Error> {
            self.published
                .lock(|published| published.borrow_mut().push(checksum.into()));
            Ok(alloc::vec![alloc::format!(
                "http://192.0.2.1/fw/{checksum}"
            )])
        }

        async fn unpublish(&self, checksum: &str) -> Result<bool, Self::Error> {
            Ok(self.published.lock(|published| {
                let mut published = published.borrow_mut();
                let before = published.len();
                published.retain(|c| c != checksum);
                published.len() != before
            }))
        }
    }

    #[tokio::test]
    async fn a_real_publisher_can_publish_and_then_unpublish_by_checksum() {
        let publisher = FakePublisher {
            published: alloc::sync::Arc::new(embassy_sync::blocking_mutex::Mutex::new(
                core::cell::RefCell::new(Vec::new()),
            )),
        };

        let locations = publisher.publish("checksum-a").await.unwrap();
        assert_eq!(locations, alloc::vec!["http://192.0.2.1/fw/checksum-a"]);

        assert!(publisher.unpublish("checksum-a").await.unwrap());
        // Already gone - the second unpublish is the "not currently published" case.
        assert!(!publisher.unpublish("checksum-a").await.unwrap());
    }
}
