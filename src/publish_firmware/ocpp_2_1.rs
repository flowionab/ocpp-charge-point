//! OCPP 2.1 wire adapter for the Firmware Publishing block (B3.4).

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;

use crate::wire::v21::common::{
    GenericStatusEnum, PublishFirmwareStatusEnum, UnpublishFirmwareStatusEnum,
};
use crate::wire::v21::{
    PublishFirmwareRequest, PublishFirmwareResponse, PublishFirmwareStatusNotificationRequest,
    UnpublishFirmwareRequest, UnpublishFirmwareResponse,
};
use ocpp_client::ocpp_2_1::OCPP2_1Client;

use crate::actor::ChargePointActor;
use crate::hardware::FirmwarePublisher;
use crate::publish_firmware::{
    PublishFirmwareHandler, PublishFirmwareOutcome, PublishFirmwareQueue,
    PublishFirmwareRequest as CoreRequest, PublishFirmwareState, PublishFirmwareStatus,
    PublishFirmwareStatusNotifier, UnpublishFirmwareOutcome, handle_publish_firmware,
    handle_unpublish_firmware,
};

fn map_publish_request(request: &PublishFirmwareRequest) -> CoreRequest {
    CoreRequest {
        request_id: request.request_id,
        location: request.location.clone(),
        checksum: request.checksum.to_string(),
        retries: request
            .retries
            .and_then(|retries| u32::try_from(retries).ok())
            .unwrap_or(crate::publish_firmware::DEFAULT_RETRIES),
        retry_interval_secs: request
            .retry_interval
            .and_then(|interval| u32::try_from(interval).ok())
            .filter(|interval| *interval > 0)
            .unwrap_or(crate::publish_firmware::DEFAULT_RETRY_INTERVAL_SECS),
    }
}

fn publish_response(outcome: PublishFirmwareOutcome) -> PublishFirmwareResponse {
    PublishFirmwareResponse {
        custom_data: None,
        status: match outcome {
            PublishFirmwareOutcome::Accepted => GenericStatusEnum::Accepted,
            PublishFirmwareOutcome::Rejected => GenericStatusEnum::Rejected,
        },
        status_info: None,
    }
}

fn unpublish_response(outcome: UnpublishFirmwareOutcome) -> UnpublishFirmwareResponse {
    UnpublishFirmwareResponse {
        custom_data: None,
        status: match outcome {
            UnpublishFirmwareOutcome::Unpublished => UnpublishFirmwareStatusEnum::Unpublished,
            UnpublishFirmwareOutcome::NoFirmware => UnpublishFirmwareStatusEnum::NoFirmware,
            UnpublishFirmwareOutcome::DownloadOngoing => {
                UnpublishFirmwareStatusEnum::DownloadOngoing
            }
        },
    }
}

pub(super) fn wire_status(status: PublishFirmwareStatus) -> PublishFirmwareStatusEnum {
    match status {
        PublishFirmwareStatus::Idle => PublishFirmwareStatusEnum::Idle,
        PublishFirmwareStatus::DownloadScheduled => PublishFirmwareStatusEnum::DownloadScheduled,
        PublishFirmwareStatus::Downloading => PublishFirmwareStatusEnum::Downloading,
        PublishFirmwareStatus::Downloaded => PublishFirmwareStatusEnum::Downloaded,
        PublishFirmwareStatus::Published => PublishFirmwareStatusEnum::Published,
        PublishFirmwareStatus::DownloadFailed => PublishFirmwareStatusEnum::DownloadFailed,
        PublishFirmwareStatus::DownloadPaused => PublishFirmwareStatusEnum::DownloadPaused,
        PublishFirmwareStatus::InvalidChecksum => PublishFirmwareStatusEnum::InvalidChecksum,
        PublishFirmwareStatus::ChecksumVerified => PublishFirmwareStatusEnum::ChecksumVerified,
        PublishFirmwareStatus::PublishFailed => PublishFirmwareStatusEnum::PublishFailed,
    }
}

/// Handles 2.1's `PublishFirmware`/`UnpublishFirmware` and sends its
/// `PublishFirmwareStatusNotification`s.
pub struct Ocpp2_1PublishFirmwareHandler {
    client: OCPP2_1Client,
}

impl Ocpp2_1PublishFirmwareHandler {
    /// Wraps `client`.
    pub fn new(client: OCPP2_1Client) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl PublishFirmwareHandler for Ocpp2_1PublishFirmwareHandler {
    async fn register_publish_firmware_handlers<F>(
        &self,
        actor: ChargePointActor,
        publishes: PublishFirmwareQueue,
        publisher: F,
        state: Arc<PublishFirmwareState>,
    ) where
        F: FirmwarePublisher + Send + Sync + 'static,
    {
        let publisher = Arc::new(publisher);

        let publish_actor = actor.clone();
        let publishes_for_handler = publishes.clone();
        let publish_state = state.clone();
        self.client
            .on_publish_firmware(move |request: PublishFirmwareRequest, _client| {
                let actor = publish_actor.clone();
                let publishes = publishes_for_handler.clone();
                let state = publish_state.clone();
                async move {
                    let outcome = handle_publish_firmware(
                        &actor,
                        &publishes,
                        &state,
                        map_publish_request(&request),
                    )
                    .await;
                    Ok(publish_response(outcome))
                }
            })
            .await;

        self.client
            .on_unpublish_firmware(move |request: UnpublishFirmwareRequest, _client| {
                let actor = actor.clone();
                let publisher = publisher.clone();
                let state = state.clone();
                async move {
                    let outcome =
                        handle_unpublish_firmware(&actor, &publisher, &state, &request.checksum)
                            .await;
                    Ok(unpublish_response(outcome))
                }
            })
            .await;
    }
}

#[async_trait::async_trait]
impl PublishFirmwareStatusNotifier for Ocpp2_1PublishFirmwareHandler {
    type Error = ocpp_client::ClientError<ocpp_client::ocpp_2_1::OCPP2_1Error>;

    async fn notify_publish_firmware_status(
        &self,
        request_id: i64,
        status: PublishFirmwareStatus,
        locations: Option<&[String]>,
    ) -> Result<(), Self::Error> {
        self.client
            .send_publish_firmware_status_notification(PublishFirmwareStatusNotificationRequest {
                custom_data: None,
                location: locations.map(|locations| locations.to_vec()),
                request_id: Some(request_id),
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
    impl PublishFirmwareHandler for OCPP2_1Client {
        async fn register_publish_firmware_handlers<F>(
            &self,
            actor: ChargePointActor,
            publishes: PublishFirmwareQueue,
            publisher: F,
            state: Arc<PublishFirmwareState>,
        ) where
            F: FirmwarePublisher + Send + Sync + 'static,
        {
            Ocpp2_1PublishFirmwareHandler::new(self.clone())
                .register_publish_firmware_handlers(actor, publishes, publisher, state)
                .await;
        }
    }

    #[async_trait::async_trait]
    impl PublishFirmwareStatusNotifier for OCPP2_1Client {
        type Error = ocpp_client::ClientError<ocpp_client::ocpp_2_1::OCPP2_1Error>;

        async fn notify_publish_firmware_status(
            &self,
            request_id: i64,
            status: PublishFirmwareStatus,
            locations: Option<&[String]>,
        ) -> Result<(), Self::Error> {
            Ocpp2_1PublishFirmwareHandler::new(self.clone())
                .notify_publish_firmware_status(request_id, status, locations)
                .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire_request() -> PublishFirmwareRequest {
        PublishFirmwareRequest {
            custom_data: None,
            checksum: "d41d8cd98f00b204e9800998ecf8427e".try_into().unwrap(),
            location: "https://fw.example/image.bin".into(),
            request_id: 42,
            retries: Some(2),
            retry_interval: Some(30),
        }
    }

    #[test]
    fn the_request_id_location_checksum_and_retry_policy_all_come_across() {
        let mapped = map_publish_request(&wire_request());

        assert_eq!(mapped.request_id, 42);
        assert_eq!(mapped.location, "https://fw.example/image.bin");
        assert_eq!(mapped.checksum, "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(mapped.retries, 2);
        assert_eq!(mapped.retry_interval_secs, 30);
    }

    #[test]
    fn a_missing_retry_policy_falls_back_to_the_documented_defaults() {
        let mapped = map_publish_request(&PublishFirmwareRequest {
            retries: None,
            retry_interval: None,
            ..wire_request()
        });

        assert_eq!(mapped.retries, crate::publish_firmware::DEFAULT_RETRIES);
        assert_eq!(
            mapped.retry_interval_secs,
            crate::publish_firmware::DEFAULT_RETRY_INTERVAL_SECS
        );
    }

    #[test]
    fn every_internal_status_has_its_own_2_1_value() {
        let all = [
            (PublishFirmwareStatus::Idle, PublishFirmwareStatusEnum::Idle),
            (
                PublishFirmwareStatus::DownloadScheduled,
                PublishFirmwareStatusEnum::DownloadScheduled,
            ),
            (
                PublishFirmwareStatus::Downloading,
                PublishFirmwareStatusEnum::Downloading,
            ),
            (
                PublishFirmwareStatus::Downloaded,
                PublishFirmwareStatusEnum::Downloaded,
            ),
            (
                PublishFirmwareStatus::Published,
                PublishFirmwareStatusEnum::Published,
            ),
            (
                PublishFirmwareStatus::DownloadFailed,
                PublishFirmwareStatusEnum::DownloadFailed,
            ),
            (
                PublishFirmwareStatus::DownloadPaused,
                PublishFirmwareStatusEnum::DownloadPaused,
            ),
            (
                PublishFirmwareStatus::InvalidChecksum,
                PublishFirmwareStatusEnum::InvalidChecksum,
            ),
            (
                PublishFirmwareStatus::ChecksumVerified,
                PublishFirmwareStatusEnum::ChecksumVerified,
            ),
            (
                PublishFirmwareStatus::PublishFailed,
                PublishFirmwareStatusEnum::PublishFailed,
            ),
        ];
        for (internal, wire) in all {
            assert_eq!(wire_status(internal), wire);
        }
    }

    #[test]
    fn outcomes_map_onto_their_documented_statuses() {
        assert_eq!(
            publish_response(PublishFirmwareOutcome::Accepted).status,
            GenericStatusEnum::Accepted
        );
        assert_eq!(
            publish_response(PublishFirmwareOutcome::Rejected).status,
            GenericStatusEnum::Rejected
        );
        assert_eq!(
            unpublish_response(UnpublishFirmwareOutcome::Unpublished).status,
            UnpublishFirmwareStatusEnum::Unpublished
        );
        assert_eq!(
            unpublish_response(UnpublishFirmwareOutcome::NoFirmware).status,
            UnpublishFirmwareStatusEnum::NoFirmware
        );
        assert_eq!(
            unpublish_response(UnpublishFirmwareOutcome::DownloadOngoing).status,
            UnpublishFirmwareStatusEnum::DownloadOngoing
        );
    }
}
