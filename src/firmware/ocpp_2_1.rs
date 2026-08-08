//! OCPP 2.1 wire adapter for the Firmware Management block (B3.2).

use alloc::boxed::Box;
use alloc::string::ToString;
use alloc::sync::Arc;

use crate::wire::v21::common::{FirmwareStatusEnum, UpdateFirmwareStatusEnum};
use crate::wire::v21::{
    FirmwareStatusNotificationRequest, UpdateFirmwareRequest, UpdateFirmwareResponse,
};
use ocpp_client::ocpp_2_1::OCPP2_1Client;

use crate::actor::ChargePointActor;
use crate::firmware::{
    FirmwareStatus, FirmwareStatusNotifier, FirmwareUpdateQueue, FirmwareUpdateRequest,
    FirmwareUpdateState, UpdateFirmwareHandler, UpdateFirmwareOutcome, handle_update_firmware,
};

fn map_request(request: &UpdateFirmwareRequest) -> FirmwareUpdateRequest {
    let firmware = &request.firmware;
    FirmwareUpdateRequest {
        request_id: Some(request.request_id),
        location: firmware.location.to_string(),
        // `retrieveDateTime` is required on the wire, and since `ocpp-types` 0.2.0 it arrives
        // already validated as an `OcppTimestamp`, so this can no longer fail: an unparseable
        // one is rejected by `ocpp-client`'s decoder before it reaches this crate. The old
        // "unparseable becomes now" fallback therefore has no case left to cover.
        retrieve_at: Some(firmware.retrieve_date_time.into()),
        install_at: firmware.install_date_time.map(Into::into),
        signature: firmware.signature.as_deref().map(Into::into),
        signing_certificate: firmware.signing_certificate.as_deref().map(Into::into),
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

fn update_response(outcome: UpdateFirmwareOutcome) -> UpdateFirmwareResponse {
    UpdateFirmwareResponse {
        custom_data: None,
        status: match outcome {
            UpdateFirmwareOutcome::Accepted => UpdateFirmwareStatusEnum::Accepted,
            UpdateFirmwareOutcome::AcceptedCanceled => UpdateFirmwareStatusEnum::AcceptedCanceled,
            UpdateFirmwareOutcome::Rejected => UpdateFirmwareStatusEnum::Rejected,
        },
        // `InvalidCertificate`/`RevokedCertificate` are never returned: verifying the signing
        // certificate is B3.3, and answering `InvalidCertificate` without having checked would be
        // a lie in the more dangerous direction.
        status_info: None,
    }
}

pub(super) fn wire_status(status: FirmwareStatus) -> FirmwareStatusEnum {
    match status {
        FirmwareStatus::Idle => FirmwareStatusEnum::Idle,
        FirmwareStatus::DownloadScheduled => FirmwareStatusEnum::DownloadScheduled,
        FirmwareStatus::Downloading => FirmwareStatusEnum::Downloading,
        FirmwareStatus::Downloaded => FirmwareStatusEnum::Downloaded,
        FirmwareStatus::DownloadFailed => FirmwareStatusEnum::DownloadFailed,
        FirmwareStatus::InstallScheduled => FirmwareStatusEnum::InstallScheduled,
        FirmwareStatus::Installing => FirmwareStatusEnum::Installing,
        FirmwareStatus::Installed => FirmwareStatusEnum::Installed,
        FirmwareStatus::InstallationFailed => FirmwareStatusEnum::InstallationFailed,
        FirmwareStatus::InstallRebooting => FirmwareStatusEnum::InstallRebooting,
    }
}

/// Handles 2.1's `UpdateFirmware` and sends its `FirmwareStatusNotification`s.
pub struct Ocpp2_1FirmwareHandler {
    client: OCPP2_1Client,
}

impl Ocpp2_1FirmwareHandler {
    /// Wraps `client`.
    pub fn new(client: OCPP2_1Client) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl UpdateFirmwareHandler for Ocpp2_1FirmwareHandler {
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
                    let outcome =
                        handle_update_firmware(&actor, &updates, &state, map_request(&request))
                            .await;
                    Ok(update_response(outcome))
                }
            })
            .await;
    }
}

#[async_trait::async_trait]
impl FirmwareStatusNotifier for Ocpp2_1FirmwareHandler {
    type Error = ocpp_client::ClientError<ocpp_client::ocpp_2_1::OCPP2_1Error>;

    async fn notify_firmware_status(
        &self,
        request_id: Option<i64>,
        status: FirmwareStatus,
    ) -> Result<(), Self::Error> {
        self.client
            .send_firmware_status_notification(FirmwareStatusNotificationRequest {
                custom_data: None,
                // L01.FR.20: mandatory unless the status is `Idle`, which is the one state with
                // no update to correlate to.
                request_id: match status {
                    FirmwareStatus::Idle => None,
                    _ => request_id,
                },
                status: wire_status(status),
                status_info: None,
            })
            .await
            .map(|_| ())
    }
}

/// The `std` convenience: a bare [`OCPP2_1Client`] handles this block directly.
#[cfg(feature = "std")]
mod std_impls {
    use super::*;

    #[async_trait::async_trait]
    impl UpdateFirmwareHandler for OCPP2_1Client {
        async fn register_update_firmware_handler(
            &self,
            actor: ChargePointActor,
            updates: FirmwareUpdateQueue,
            state: Arc<FirmwareUpdateState>,
        ) {
            Ocpp2_1FirmwareHandler::new(self.clone())
                .register_update_firmware_handler(actor, updates, state)
                .await;
        }
    }

    #[async_trait::async_trait]
    impl FirmwareStatusNotifier for OCPP2_1Client {
        type Error = ocpp_client::ClientError<ocpp_client::ocpp_2_1::OCPP2_1Error>;

        async fn notify_firmware_status(
            &self,
            request_id: Option<i64>,
            status: FirmwareStatus,
        ) -> Result<(), Self::Error> {
            Ocpp2_1FirmwareHandler::new(self.clone())
                .notify_firmware_status(request_id, status)
                .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::v21::common::Firmware;

    fn wire_request() -> UpdateFirmwareRequest {
        UpdateFirmwareRequest {
            custom_data: None,
            firmware: Firmware {
                custom_data: None,
                install_date_time: Some("2026-03-04T06:00:00Z".try_into().unwrap()),
                location: "https://fw.example/image.bin".into(),
                retrieve_date_time: "2026-03-04T05:00:00Z".try_into().unwrap(),
                signature: Some("c2ln".into()),
                signing_certificate: Some("Y2VydA==".into()),
            },
            request_id: 77,
            retries: Some(2),
            retry_interval: Some(30),
        }
    }

    #[test]
    fn the_schedule_signature_and_retry_policy_all_come_across() {
        let mapped = map_request(&wire_request());

        assert_eq!(mapped.request_id, Some(77));
        assert_eq!(mapped.location, "https://fw.example/image.bin");
        assert!(mapped.retrieve_at.is_some());
        assert!(mapped.install_at.is_some());
        assert_eq!(mapped.retries, 2);
        assert_eq!(mapped.retry_interval_secs, 30);
        // Carried untouched for B3.3 rather than dropped, so verification can land there without
        // another breaking change to this shape.
        assert_eq!(mapped.signature.as_deref(), Some("c2ln"));
        assert_eq!(mapped.signing_certificate.as_deref(), Some("Y2VydA=="));
    }

    /// Replaces an older test that fed `map_request` a malformed `retrieveDateTime` and asserted
    /// it degraded to `retrieve_at: None` ("start now"). Since `ocpp-types` 0.2.0 that input
    /// cannot reach `map_request` at all: `retrieveDateTime` is an `OcppTimestamp`, so a
    /// malformed value is refused while the CALL is being decoded and answered with a
    /// `CALLERROR` - the charge point never builds a `FirmwareUpdateRequest` from it. This pins
    /// the guard at its new location rather than dropping the coverage.
    #[test]
    fn a_malformed_retrieve_time_is_refused_by_the_decoder_before_it_reaches_this_crate() {
        assert!(crate::wire::OcppTimestamp::parse_rfc3339("not a timestamp").is_err());
    }

    /// The flip side: a well-formed `retrieveDateTime` reaches `map_request` as the instant the
    /// CSMS named, so scheduling the download is exact rather than "close enough".
    #[test]
    fn a_well_formed_retrieve_time_maps_to_the_instant_the_csms_named() {
        let request = wire_request();

        assert_eq!(
            map_request(&request).retrieve_at,
            Some(
                "2026-03-04T05:00:00Z"
                    .parse::<chrono::DateTime<chrono::Utc>>()
                    .unwrap()
            ),
        );
    }

    #[test]
    fn every_internal_status_has_its_own_2_1_value() {
        // 2.1's enum is the superset this crate models, so nothing collapses here - unlike 1.6J.
        let all = [
            (FirmwareStatus::Idle, FirmwareStatusEnum::Idle),
            (
                FirmwareStatus::DownloadScheduled,
                FirmwareStatusEnum::DownloadScheduled,
            ),
            (FirmwareStatus::Downloading, FirmwareStatusEnum::Downloading),
            (FirmwareStatus::Downloaded, FirmwareStatusEnum::Downloaded),
            (
                FirmwareStatus::DownloadFailed,
                FirmwareStatusEnum::DownloadFailed,
            ),
            (
                FirmwareStatus::InstallScheduled,
                FirmwareStatusEnum::InstallScheduled,
            ),
            (FirmwareStatus::Installing, FirmwareStatusEnum::Installing),
            (FirmwareStatus::Installed, FirmwareStatusEnum::Installed),
            (
                FirmwareStatus::InstallationFailed,
                FirmwareStatusEnum::InstallationFailed,
            ),
            (
                FirmwareStatus::InstallRebooting,
                FirmwareStatusEnum::InstallRebooting,
            ),
        ];
        for (internal, wire) in all {
            assert_eq!(wire_status(internal), wire);
        }
    }

    #[test]
    fn a_supersede_is_reported_as_accepted_canceled() {
        assert_eq!(
            update_response(UpdateFirmwareOutcome::AcceptedCanceled).status,
            UpdateFirmwareStatusEnum::AcceptedCanceled
        );
        assert_eq!(
            update_response(UpdateFirmwareOutcome::Rejected).status,
            UpdateFirmwareStatusEnum::Rejected
        );
    }
}
