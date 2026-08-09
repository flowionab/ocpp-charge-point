//! OCPP 1.6J wire adapter for the Firmware Management block (B3.2/B3.3).
//!
//! 1.6J's *plain* firmware update is the same flow, expressed far more thinly, and every
//! difference from this crate's model (the superset, per `CLAUDE.md`) is a loss in the same
//! direction:
//!
//! - **`UpdateFirmwareResponse` is empty.** There is no status field at all, so a 1.6J CSMS is
//!   told nothing about acceptance - not `Rejected`, not `AcceptedCanceled`. A refusal is only
//!   visible in the `FirmwareStatusNotification` that does *not* follow.
//! - **No `requestId`.** Nothing correlates a status with the request that caused it.
//! - **No `installDate`.** Only `retrieveDate`, so a 1.6J CSMS can schedule the download but not
//!   the installation; installation follows as soon as the charge point is able (L01.FR.05).
//! - **Seven statuses, not twelve.** `DownloadScheduled`, `InstallScheduled` and
//!   `InstallRebooting` have no 1.6J value; see [`wire_status`] for what each becomes and why. The
//!   two B3.3 statuses, `SignatureVerified`/`InvalidSignature`, are likewise projected rather than
//!   represented - see [`wire_status`] - because plain `UpdateFirmware` never carries a signature
//!   to verify in the first place.
//! - **No signature or certificate.** 1.6J's plain `UpdateFirmware` carries neither, so an update
//!   accepted through [`Ocpp1_6FirmwareHandler::register_update_firmware_handler`] is unsigned by
//!   construction and never reaches B3.3's verification step.
//!
//! # `SignedUpdateFirmware` (the Security Whitepaper, B3.3)
//!
//! `ocpp-types`/`ocpp-client` 0.5.0 generate and wire `SignedUpdateFirmware` and
//! `SignedFirmwareStatusNotification` as ordinary actions - the roadmap's D2.2 note that they were
//! absent upstream no longer holds; they were added upstream since that note was written, and this
//! module wires them like any other action. Unlike plain `UpdateFirmware`, `Firmware.signature`
//! and `Firmware.signingCertificate` are **mandatory** here, so every update accepted through
//! [`Ocpp1_6FirmwareHandler::register_signed_update_firmware_handler`] carries both and always
//! goes through B3.3's verification gate. `SignedUpdateFirmwareResponseStatus` also carries
//! `InvalidCertificate`/`RevokedCertificate`, which - like 2.x's identical pair - this adapter
//! never returns: that response goes out before any download starts, and [`crate::firmware`]'s
//! verification gate is scoped to the *downloaded image*, not a certificate chain check with
//! nothing yet fetched to check it against.
//!
//! Both flows feed the same [`crate::firmware::FirmwareUpdateQueue`]/
//! [`crate::firmware::FirmwareUpdateState`] and are driven by the same worker; the two are told
//! apart on the way out purely by whether a request id is present, since a plain `UpdateFirmware`
//! never carries one and a `SignedUpdateFirmware` always does - see
//! [`FirmwareStatusNotifier::notify_firmware_status`]'s implementation below.

use alloc::boxed::Box;
use alloc::string::ToString;
use alloc::sync::Arc;

use crate::wire::v16::common::{
    FirmwareStatusNotificationRequestStatus, SignedFirmwareStatusNotificationRequestStatus,
    SignedUpdateFirmwareResponseStatus,
};
use crate::wire::v16::{
    FirmwareStatusNotificationRequest, SignedFirmwareStatusNotificationRequest,
    SignedUpdateFirmwareRequest, SignedUpdateFirmwareResponse, UpdateFirmwareRequest,
    UpdateFirmwareResponse,
};
use ocpp_client::ocpp_1_6::OCPP1_6Client;

use crate::actor::ChargePointActor;
use crate::firmware::{
    FirmwareStatus, FirmwareStatusNotifier, FirmwareUpdateQueue, FirmwareUpdateRequest,
    FirmwareUpdateState, SignedUpdateFirmwareHandler, UpdateFirmwareHandler, UpdateFirmwareOutcome,
    handle_update_firmware,
};

fn map_request(request: &UpdateFirmwareRequest) -> FirmwareUpdateRequest {
    FirmwareUpdateRequest {
        request_id: None,
        location: request.location.to_string(),
        // Infallible since `ocpp-types` 0.2.0 - `retrieveDate` arrives already validated,
        // so an unparseable one is refused by `ocpp-client`'s decoder rather than reaching here.
        retrieve_at: Some(request.retrieve_date.into()),
        // 1.6J has no `installDate`: installation happens as soon as the charge point is able.
        install_at: None,
        signature: None,
        signing_certificate: None,
        retries: request
            .retries
            .and_then(|retries| u32::try_from(retries).ok())
            .unwrap_or(crate::firmware::DEFAULT_RETRIES),
        retry_interval_secs: request
            .retry_interval
            .and_then(|interval| u32::try_from(interval).ok())
            .filter(|interval| *interval > 0)
            .unwrap_or(crate::firmware::DEFAULT_RETRY_INTERVAL_SECS),
    }
}

/// This crate's status onto 1.6J's narrower enum.
///
/// Five of them have no 1.6J value, and each is projected onto the nearest status that is *true*
/// rather than the nearest that is convenient:
///
/// - `DownloadScheduled` → `Idle`: nothing is happening yet, which is what `Idle` means. Reporting
///   `Downloading` would claim a transfer that has not started.
/// - `InstallScheduled` → `Downloaded`: the image is fetched and nothing further has happened,
///   which is exactly the state the charge point is in.
/// - `InstallRebooting` → `Installing`: installation is under way and the charge point is about to
///   disappear. `Installed` would be a lie - it has not booted the new image yet - and a 1.6J CSMS
///   seeing `Installing` followed by a reconnect can draw the right conclusion.
/// - `SignatureVerified` → `Downloaded`: verification (B3.3) only ever runs on an update carrying
///   a signature, and plain `UpdateFirmware` never does - this arm exists for exhaustiveness, not
///   because it is reachable through this handler. If it were ever reached, `Downloaded` is still
///   true: the image is fetched and nothing has failed.
/// - `InvalidSignature` → `DownloadFailed`: same reachability note. `DownloadFailed` is the
///   nearest true meaning 1.6J has for "this image cannot be used" - the closest existing status
///   is not `InstallationFailed`, which would claim an installation was attempted.
pub(super) fn wire_status(status: FirmwareStatus) -> FirmwareStatusNotificationRequestStatus {
    match status {
        FirmwareStatus::Idle | FirmwareStatus::DownloadScheduled => {
            FirmwareStatusNotificationRequestStatus::Idle
        }
        FirmwareStatus::Downloading => FirmwareStatusNotificationRequestStatus::Downloading,
        FirmwareStatus::Downloaded
        | FirmwareStatus::InstallScheduled
        | FirmwareStatus::SignatureVerified => FirmwareStatusNotificationRequestStatus::Downloaded,
        FirmwareStatus::DownloadFailed | FirmwareStatus::InvalidSignature => {
            FirmwareStatusNotificationRequestStatus::DownloadFailed
        }
        FirmwareStatus::Installing | FirmwareStatus::InstallRebooting => {
            FirmwareStatusNotificationRequestStatus::Installing
        }
        FirmwareStatus::Installed => FirmwareStatusNotificationRequestStatus::Installed,
        FirmwareStatus::InstallationFailed => {
            FirmwareStatusNotificationRequestStatus::InstallationFailed
        }
    }
}

/// Maps a `SignedUpdateFirmware` request onto this crate's protocol-independent model.
///
/// Unlike plain `UpdateFirmware`, `signature`/`signingCertificate` are mandatory on the wire, so
/// they are always carried across as `Some` - this is a signed update by construction, and the
/// firmware worker's B3.3 verification gate always runs for it.
fn map_signed_request(request: &SignedUpdateFirmwareRequest) -> FirmwareUpdateRequest {
    let firmware = &request.firmware;
    FirmwareUpdateRequest {
        request_id: Some(request.request_id),
        location: firmware.location.to_string(),
        retrieve_at: Some(firmware.retrieve_date_time.into()),
        install_at: firmware.install_date_time.map(Into::into),
        signature: Some(firmware.signature.to_string()),
        signing_certificate: Some(firmware.signing_certificate.to_string()),
        retries: request
            .retries
            .and_then(|retries| u32::try_from(retries).ok())
            .unwrap_or(crate::firmware::DEFAULT_RETRIES),
        retry_interval_secs: request
            .retry_interval
            .and_then(|interval| u32::try_from(interval).ok())
            .filter(|interval| *interval > 0)
            .unwrap_or(crate::firmware::DEFAULT_RETRY_INTERVAL_SECS),
    }
}

/// This crate's status onto `SignedFirmwareStatusNotificationRequestStatus`. Unlike plain 1.6J,
/// nothing is lossy here: every status this crate produces has a directly corresponding wire
/// value, which is the whole reason the Security Whitepaper's status enum exists.
pub(super) fn signed_wire_status(
    status: FirmwareStatus,
) -> SignedFirmwareStatusNotificationRequestStatus {
    match status {
        FirmwareStatus::Idle => SignedFirmwareStatusNotificationRequestStatus::Idle,
        FirmwareStatus::DownloadScheduled => {
            SignedFirmwareStatusNotificationRequestStatus::DownloadScheduled
        }
        FirmwareStatus::Downloading => SignedFirmwareStatusNotificationRequestStatus::Downloading,
        FirmwareStatus::Downloaded => SignedFirmwareStatusNotificationRequestStatus::Downloaded,
        FirmwareStatus::DownloadFailed => {
            SignedFirmwareStatusNotificationRequestStatus::DownloadFailed
        }
        FirmwareStatus::InstallScheduled => {
            SignedFirmwareStatusNotificationRequestStatus::InstallScheduled
        }
        FirmwareStatus::Installing => SignedFirmwareStatusNotificationRequestStatus::Installing,
        FirmwareStatus::Installed => SignedFirmwareStatusNotificationRequestStatus::Installed,
        FirmwareStatus::InstallationFailed => {
            SignedFirmwareStatusNotificationRequestStatus::InstallationFailed
        }
        FirmwareStatus::InstallRebooting => {
            SignedFirmwareStatusNotificationRequestStatus::InstallRebooting
        }
        FirmwareStatus::SignatureVerified => {
            SignedFirmwareStatusNotificationRequestStatus::SignatureVerified
        }
        FirmwareStatus::InvalidSignature => {
            SignedFirmwareStatusNotificationRequestStatus::InvalidSignature
        }
    }
}

/// `UpdateFirmwareOutcome` onto `SignedUpdateFirmwareResponseStatus`.
///
/// `InvalidCertificate`/`RevokedCertificate` are never returned - see the module docs for why a
/// pre-download certificate check is out of this adapter's scope.
fn signed_update_response(outcome: UpdateFirmwareOutcome) -> SignedUpdateFirmwareResponseStatus {
    match outcome {
        UpdateFirmwareOutcome::Accepted => SignedUpdateFirmwareResponseStatus::Accepted,
        UpdateFirmwareOutcome::AcceptedCanceled => {
            SignedUpdateFirmwareResponseStatus::AcceptedCanceled
        }
        UpdateFirmwareOutcome::Rejected => SignedUpdateFirmwareResponseStatus::Rejected,
    }
}

/// Handles 1.6J's `UpdateFirmware` and sends its `FirmwareStatusNotification`s.
pub struct Ocpp1_6FirmwareHandler {
    client: OCPP1_6Client,
}

impl Ocpp1_6FirmwareHandler {
    /// Wraps `client`.
    pub fn new(client: OCPP1_6Client) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl UpdateFirmwareHandler for Ocpp1_6FirmwareHandler {
    async fn register_update_firmware_handler(
        &self,
        actor: ChargePointActor,
        updates: FirmwareUpdateQueue,
        state: Arc<FirmwareUpdateState>,
    ) {
        self.client
            .on_update_firmware(move |request: UpdateFirmwareRequest, _client| {
                let actor = actor.clone();
                let updates = updates.clone();
                let state = state.clone();
                async move {
                    // The outcome is deliberately dropped: 1.6J's response is `{}`, with nowhere
                    // to put a status. A refusal shows up as the absence of the notifications
                    // that would otherwise follow.
                    let _ = handle_update_firmware(&actor, &updates, &state, map_request(&request))
                        .await;
                    Ok(UpdateFirmwareResponse {})
                }
            })
            .await;
    }
}

#[async_trait::async_trait]
impl SignedUpdateFirmwareHandler for Ocpp1_6FirmwareHandler {
    async fn register_signed_update_firmware_handler(
        &self,
        actor: ChargePointActor,
        updates: FirmwareUpdateQueue,
        state: Arc<FirmwareUpdateState>,
    ) {
        self.client
            .on_signed_update_firmware(move |request: SignedUpdateFirmwareRequest, _client| {
                let actor = actor.clone();
                let updates = updates.clone();
                let state = state.clone();
                async move {
                    let outcome = handle_update_firmware(
                        &actor,
                        &updates,
                        &state,
                        map_signed_request(&request),
                    )
                    .await;
                    Ok(SignedUpdateFirmwareResponse {
                        status: signed_update_response(outcome),
                    })
                }
            })
            .await;
    }
}

#[async_trait::async_trait]
impl FirmwareStatusNotifier for Ocpp1_6FirmwareHandler {
    type Error = ocpp_client::ClientError<ocpp_client::ocpp_1_6::OCPP1_6Error>;

    async fn notify_firmware_status(
        &self,
        request_id: Option<i64>,
        status: FirmwareStatus,
    ) -> Result<(), Self::Error> {
        // A plain `UpdateFirmware` never carries a request id (see the module docs); a
        // `SignedUpdateFirmware` always does. That is enough to tell the two flows apart on the
        // way out and report through the wire shape that matches how the update came in.
        match request_id {
            Some(id) => self
                .client
                .send_signed_firmware_status_notification(SignedFirmwareStatusNotificationRequest {
                    status: signed_wire_status(status),
                    request_id: Some(id),
                })
                .await
                .map(|_| ()),
            None => self
                .client
                .send_firmware_status_notification(FirmwareStatusNotificationRequest {
                    status: wire_status(status),
                })
                .await
                .map(|_| ()),
        }
    }
}

/// The `std` convenience: a bare [`OCPP1_6Client`] handles this block directly.
#[cfg(feature = "std")]
mod std_impls {
    use super::*;

    #[async_trait::async_trait]
    impl UpdateFirmwareHandler for OCPP1_6Client {
        async fn register_update_firmware_handler(
            &self,
            actor: ChargePointActor,
            updates: FirmwareUpdateQueue,
            state: Arc<FirmwareUpdateState>,
        ) {
            Ocpp1_6FirmwareHandler::new(self.clone())
                .register_update_firmware_handler(actor, updates, state)
                .await;
        }
    }

    #[async_trait::async_trait]
    impl SignedUpdateFirmwareHandler for OCPP1_6Client {
        async fn register_signed_update_firmware_handler(
            &self,
            actor: ChargePointActor,
            updates: FirmwareUpdateQueue,
            state: Arc<FirmwareUpdateState>,
        ) {
            Ocpp1_6FirmwareHandler::new(self.clone())
                .register_signed_update_firmware_handler(actor, updates, state)
                .await;
        }
    }

    #[async_trait::async_trait]
    impl FirmwareStatusNotifier for OCPP1_6Client {
        type Error = ocpp_client::ClientError<ocpp_client::ocpp_1_6::OCPP1_6Error>;

        async fn notify_firmware_status(
            &self,
            request_id: Option<i64>,
            status: FirmwareStatus,
        ) -> Result<(), Self::Error> {
            Ocpp1_6FirmwareHandler::new(self.clone())
                .notify_firmware_status(request_id, status)
                .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_1_6_request_carries_no_install_schedule_signature_or_request_id() {
        let mapped = map_request(&UpdateFirmwareRequest {
            location: "ftp://fw.example/image.bin".into(),
            retries: Some(1),
            retrieve_date: "2026-03-04T05:00:00Z".try_into().unwrap(),
            retry_interval: Some(60),
        });

        assert_eq!(mapped.location, "ftp://fw.example/image.bin");
        assert!(mapped.retrieve_at.is_some());
        assert_eq!(mapped.request_id, None);
        assert_eq!(mapped.install_at, None);
        // 1.6J's plain UpdateFirmware is unsigned by construction - see the module docs.
        assert_eq!(mapped.signature, None);
        assert_eq!(mapped.signing_certificate, None);
        assert_eq!(mapped.retries, 1);
        assert_eq!(mapped.retry_interval_secs, 60);
    }

    #[test]
    fn the_statuses_1_6_lacks_project_onto_true_ones_rather_than_convenient_ones() {
        // Nothing has started, which is what Idle means - not Downloading, which would claim a
        // transfer that has not begun.
        assert_eq!(
            wire_status(FirmwareStatus::DownloadScheduled),
            FirmwareStatusNotificationRequestStatus::Idle
        );
        // The image is fetched and nothing further has happened yet.
        assert_eq!(
            wire_status(FirmwareStatus::InstallScheduled),
            FirmwareStatusNotificationRequestStatus::Downloaded
        );
        // Installing, not Installed: the new image has not booted yet, and saying otherwise would
        // have a CSMS record a version this charge point is not running.
        assert_eq!(
            wire_status(FirmwareStatus::InstallRebooting),
            FirmwareStatusNotificationRequestStatus::Installing
        );
        // B3.3's two statuses are unreachable through the plain handler (it never verifies a
        // signature), but the projection is still the nearest true meaning: fetched-and-fine, or
        // this-image-cannot-be-used.
        assert_eq!(
            wire_status(FirmwareStatus::SignatureVerified),
            FirmwareStatusNotificationRequestStatus::Downloaded
        );
        assert_eq!(
            wire_status(FirmwareStatus::InvalidSignature),
            FirmwareStatusNotificationRequestStatus::DownloadFailed
        );
    }

    #[test]
    fn the_statuses_1_6_does_have_map_straight_across() {
        for (internal, wire) in [
            (
                FirmwareStatus::Downloading,
                FirmwareStatusNotificationRequestStatus::Downloading,
            ),
            (
                FirmwareStatus::Downloaded,
                FirmwareStatusNotificationRequestStatus::Downloaded,
            ),
            (
                FirmwareStatus::DownloadFailed,
                FirmwareStatusNotificationRequestStatus::DownloadFailed,
            ),
            (
                FirmwareStatus::Installing,
                FirmwareStatusNotificationRequestStatus::Installing,
            ),
            (
                FirmwareStatus::Installed,
                FirmwareStatusNotificationRequestStatus::Installed,
            ),
            (
                FirmwareStatus::InstallationFailed,
                FirmwareStatusNotificationRequestStatus::InstallationFailed,
            ),
            (
                FirmwareStatus::Idle,
                FirmwareStatusNotificationRequestStatus::Idle,
            ),
        ] {
            assert_eq!(wire_status(internal), wire);
        }
    }

    fn signed_wire_request() -> SignedUpdateFirmwareRequest {
        SignedUpdateFirmwareRequest {
            firmware: crate::wire::v16::common::Firmware {
                install_date_time: Some("2026-03-04T06:00:00Z".try_into().unwrap()),
                location: heapless::String::try_from("https://fw.example/image.bin").unwrap(),
                retrieve_date_time: "2026-03-04T05:00:00Z".try_into().unwrap(),
                signature: "c2ln".into(),
                signing_certificate: "Y2VydA==".into(),
            },
            request_id: 42,
            retries: Some(1),
            retry_interval: Some(60),
        }
    }

    #[test]
    fn a_signed_request_always_carries_a_signature_certificate_and_request_id() {
        let mapped = map_signed_request(&signed_wire_request());

        assert_eq!(mapped.request_id, Some(42));
        assert_eq!(mapped.location, "https://fw.example/image.bin");
        assert!(mapped.retrieve_at.is_some());
        assert!(mapped.install_at.is_some());
        // Mandatory on the wire, so always `Some` - unlike plain `UpdateFirmware`.
        assert_eq!(mapped.signature.as_deref(), Some("c2ln"));
        assert_eq!(mapped.signing_certificate.as_deref(), Some("Y2VydA=="));
        assert_eq!(mapped.retries, 1);
        assert_eq!(mapped.retry_interval_secs, 60);
    }

    #[test]
    fn every_internal_status_has_its_own_signed_wire_value() {
        // The Security Whitepaper's status enum is (almost) the superset this crate models, so
        // nothing is projected here - unlike plain 1.6J's `wire_status`.
        let all = [
            (
                FirmwareStatus::Idle,
                SignedFirmwareStatusNotificationRequestStatus::Idle,
            ),
            (
                FirmwareStatus::DownloadScheduled,
                SignedFirmwareStatusNotificationRequestStatus::DownloadScheduled,
            ),
            (
                FirmwareStatus::Downloading,
                SignedFirmwareStatusNotificationRequestStatus::Downloading,
            ),
            (
                FirmwareStatus::Downloaded,
                SignedFirmwareStatusNotificationRequestStatus::Downloaded,
            ),
            (
                FirmwareStatus::DownloadFailed,
                SignedFirmwareStatusNotificationRequestStatus::DownloadFailed,
            ),
            (
                FirmwareStatus::InstallScheduled,
                SignedFirmwareStatusNotificationRequestStatus::InstallScheduled,
            ),
            (
                FirmwareStatus::Installing,
                SignedFirmwareStatusNotificationRequestStatus::Installing,
            ),
            (
                FirmwareStatus::Installed,
                SignedFirmwareStatusNotificationRequestStatus::Installed,
            ),
            (
                FirmwareStatus::InstallationFailed,
                SignedFirmwareStatusNotificationRequestStatus::InstallationFailed,
            ),
            (
                FirmwareStatus::InstallRebooting,
                SignedFirmwareStatusNotificationRequestStatus::InstallRebooting,
            ),
            (
                FirmwareStatus::SignatureVerified,
                SignedFirmwareStatusNotificationRequestStatus::SignatureVerified,
            ),
            (
                FirmwareStatus::InvalidSignature,
                SignedFirmwareStatusNotificationRequestStatus::InvalidSignature,
            ),
        ];
        for (internal, wire) in all {
            assert_eq!(signed_wire_status(internal), wire);
        }
    }

    #[test]
    fn a_signed_supersede_is_reported_as_accepted_canceled_never_a_certificate_status() {
        assert_eq!(
            signed_update_response(UpdateFirmwareOutcome::Accepted),
            SignedUpdateFirmwareResponseStatus::Accepted
        );
        assert_eq!(
            signed_update_response(UpdateFirmwareOutcome::AcceptedCanceled),
            SignedUpdateFirmwareResponseStatus::AcceptedCanceled
        );
        assert_eq!(
            signed_update_response(UpdateFirmwareOutcome::Rejected),
            SignedUpdateFirmwareResponseStatus::Rejected
        );
    }
}
