//! OCPP 1.6J wire adapter for the Firmware Management block (B3.2).
//!
//! 1.6J's firmware update is the same flow, expressed far more thinly, and every difference is a
//! loss in the same direction - this crate's model carries the superset, per `CLAUDE.md`:
//!
//! - **`UpdateFirmwareResponse` is empty.** There is no status field at all, so a 1.6J CSMS is
//!   told nothing about acceptance - not `Rejected`, not `AcceptedCanceled`. A refusal is only
//!   visible in the `FirmwareStatusNotification` that does *not* follow.
//! - **No `requestId`.** Nothing correlates a status with the request that caused it.
//! - **No `installDate`.** Only `retrieveDate`, so a 1.6J CSMS can schedule the download but not
//!   the installation; installation follows as soon as the charge point is able (L01.FR.05).
//! - **Seven statuses, not fourteen.** `DownloadScheduled`, `InstallScheduled` and
//!   `InstallRebooting` have no 1.6J value; see [`wire_status`] for what each becomes and why.
//! - **No signature or certificate.** 1.6J's plain `UpdateFirmware` carries neither, so a 1.6J
//!   update handled here is unsigned by construction. The Security Whitepaper's
//!   `SignedUpdateFirmware`/`SignedFirmwareStatusNotification` do carry them, and as of
//!   `ocpp-types` 0.2.0 / `ocpp-client` 0.4.0 both are generated and wrapped as actions - this
//!   crate simply does not wire them yet (roadmap D2.2). Until it does, the limitation stands;
//!   it is now a gap here rather than upstream.

use alloc::boxed::Box;
use alloc::string::ToString;
use alloc::sync::Arc;

use crate::wire::v16::common::FirmwareStatusNotificationRequestStatus;
use crate::wire::v16::{
    FirmwareStatusNotificationRequest, UpdateFirmwareRequest, UpdateFirmwareResponse,
};
use ocpp_client::ocpp_1_6::OCPP1_6Client;

use crate::actor::ChargePointActor;
use crate::firmware::{
    FirmwareStatus, FirmwareStatusNotifier, FirmwareUpdateQueue, FirmwareUpdateRequest,
    FirmwareUpdateState, UpdateFirmwareHandler, handle_update_firmware,
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
/// Three of them have no 1.6J value, and each is projected onto the nearest status that is *true*
/// rather than the nearest that is convenient:
///
/// - `DownloadScheduled` → `Idle`: nothing is happening yet, which is what `Idle` means. Reporting
///   `Downloading` would claim a transfer that has not started.
/// - `InstallScheduled` → `Downloaded`: the image is fetched and nothing further has happened,
///   which is exactly the state the charge point is in.
/// - `InstallRebooting` → `Installing`: installation is under way and the charge point is about to
///   disappear. `Installed` would be a lie - it has not booted the new image yet - and a 1.6J CSMS
///   seeing `Installing` followed by a reconnect can draw the right conclusion.
pub(super) fn wire_status(status: FirmwareStatus) -> FirmwareStatusNotificationRequestStatus {
    match status {
        FirmwareStatus::Idle | FirmwareStatus::DownloadScheduled => {
            FirmwareStatusNotificationRequestStatus::Idle
        }
        FirmwareStatus::Downloading => FirmwareStatusNotificationRequestStatus::Downloading,
        FirmwareStatus::Downloaded | FirmwareStatus::InstallScheduled => {
            FirmwareStatusNotificationRequestStatus::Downloaded
        }
        FirmwareStatus::DownloadFailed => FirmwareStatusNotificationRequestStatus::DownloadFailed,
        FirmwareStatus::Installing | FirmwareStatus::InstallRebooting => {
            FirmwareStatusNotificationRequestStatus::Installing
        }
        FirmwareStatus::Installed => FirmwareStatusNotificationRequestStatus::Installed,
        FirmwareStatus::InstallationFailed => {
            FirmwareStatusNotificationRequestStatus::InstallationFailed
        }
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
impl FirmwareStatusNotifier for Ocpp1_6FirmwareHandler {
    type Error = ocpp_client::ClientError<ocpp_client::ocpp_1_6::OCPP1_6Error>;

    async fn notify_firmware_status(
        &self,
        _request_id: Option<i64>,
        status: FirmwareStatus,
    ) -> Result<(), Self::Error> {
        self.client
            .send_firmware_status_notification(FirmwareStatusNotificationRequest {
                status: wire_status(status),
            })
            .await
            .map(|_| ())
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
    fn the_three_statuses_1_6_lacks_project_onto_true_ones_rather_than_convenient_ones() {
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
}
