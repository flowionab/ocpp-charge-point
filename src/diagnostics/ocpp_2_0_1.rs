//! OCPP 2.0.1 wire adapter for the Diagnostics block's log upload (B5.1).

use alloc::boxed::Box;
use alloc::string::ToString;
use alloc::sync::Arc;
use chrono::{DateTime, Utc};

use ocpp_client::ocpp_2_0_1::OCPP2_0_1Client;
use ocpp_client::ocpp_types::v201::common::{LogEnum, LogStatusEnum, UploadLogStatusEnum};
use ocpp_client::ocpp_types::v201::{GetLogRequest, GetLogResponse, LogStatusNotificationRequest};

use crate::actor::ChargePointActor;
use crate::clock::Clock;
use crate::diagnostics::{
    GetLogHandler, GetLogOutcome, LogStatusNotifier, LogUploadQueue, LogUploadRequest,
    LogUploadState, LogUploadStatus, handle_get_log,
};
use crate::hardware::LogKind;

/// 2.1's log-type enum onto this crate's. Every value has a counterpart, so nothing is lost.
fn map_log_kind(log_type: &LogEnum) -> LogKind {
    match log_type {
        LogEnum::SecurityLog => LogKind::Security,
        // 2.0.1 has no `DataCollectorLog` - it is 2.1's addition - so its two values are the
        // only ones reachable here.
        _ => LogKind::Diagnostics,
    }
}

/// Parses a wire timestamp, treating an unparseable one as absent - the same stance the rest of
/// this crate's adapters take. A filter this charge point cannot understand becomes no filter,
/// which sends the CSMS more of its own log rather than less.
fn parse_time(raw: &Option<alloc::string::String>) -> Option<DateTime<Utc>> {
    raw.as_ref()
        .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        .map(|parsed| parsed.with_timezone(&Utc))
}

fn map_request(request: &GetLogRequest) -> LogUploadRequest {
    LogUploadRequest {
        request_id: Some(request.request_id),
        log_kind: map_log_kind(&request.log_type),
        remote_location: request.log.remote_location.to_string(),
        oldest: parse_time(&request.log.oldest_timestamp),
        latest: parse_time(&request.log.latest_timestamp),
        retries: request
            .retries
            .and_then(|retries| u32::try_from(retries).ok())
            .unwrap_or(super::DEFAULT_RETRIES),
        retry_interval_secs: request
            .retry_interval
            .and_then(|interval| u32::try_from(interval).ok())
            .filter(|interval| *interval > 0)
            .unwrap_or(super::DEFAULT_RETRY_INTERVAL_SECS),
    }
}

fn get_log_response(outcome: &GetLogOutcome) -> GetLogResponse {
    let status = match outcome {
        GetLogOutcome::Accepted { .. } => LogStatusEnum::Accepted,
        GetLogOutcome::AcceptedCanceled { .. } => LogStatusEnum::AcceptedCanceled,
        GetLogOutcome::Rejected => LogStatusEnum::Rejected,
    };
    GetLogResponse {
        custom_data: None,
        // Dropped rather than truncated if it somehow exceeds the 255-byte bound - a truncated
        // filename names a different file, which is worse than naming none.
        filename: outcome
            .filename()
            .and_then(|name| heapless::String::try_from(name).ok()),
        status,
        status_info: None,
    }
}

pub(super) fn wire_upload_status(status: LogUploadStatus) -> UploadLogStatusEnum {
    match status {
        LogUploadStatus::Uploading => UploadLogStatusEnum::Uploading,
        LogUploadStatus::Uploaded => UploadLogStatusEnum::Uploaded,
        // 2.x has four failure values (`UploadFailure`, `BadMessage`, `PermissionDenied`,
        // `NotSupportedOperation`) and this crate reports the general one. Distinguishing them
        // would mean the `FileTransfer` implementor classifying its own error, which is a
        // protocol detail this crate deliberately keeps off the hardware surface - see
        // `crate::hardware::FileTransfer`.
        LogUploadStatus::UploadFailure => UploadLogStatusEnum::UploadFailure,
    }
}

/// Wraps an [`OCPP2_0_1Client`] with the [`Clock`] the handler needs to name a log file.
pub struct Ocpp2_0_1DiagnosticsHandler<C> {
    client: OCPP2_0_1Client,
    clock: C,
}

impl<C: Clock + Clone + Send + Sync + 'static> Ocpp2_0_1DiagnosticsHandler<C> {
    /// Wraps `client`, naming log files from `clock`.
    pub fn with_clock(client: OCPP2_0_1Client, clock: C) -> Self {
        Self { client, clock }
    }
}

#[cfg(feature = "std")]
impl Ocpp2_0_1DiagnosticsHandler<crate::clock::SystemClock> {
    /// Wraps `client`, naming log files from [`crate::clock::SystemClock`].
    pub fn new(client: OCPP2_0_1Client) -> Self {
        Self::with_clock(client, crate::clock::SystemClock)
    }
}

#[async_trait::async_trait]
impl<C: Clock + Clone + Send + Sync + 'static> GetLogHandler for Ocpp2_0_1DiagnosticsHandler<C> {
    async fn register_get_log_handler(
        &self,
        actor: ChargePointActor,
        uploads: LogUploadQueue,
        state: Arc<LogUploadState>,
    ) {
        let clock = self.clock.clone();
        self.client
            .on_get_log(move |request: GetLogRequest, _client| {
                let actor = actor.clone();
                let uploads = uploads.clone();
                let state = state.clone();
                let clock = clock.clone();
                async move {
                    let outcome =
                        handle_get_log(&actor, &uploads, &state, &clock, map_request(&request))
                            .await;
                    Ok(get_log_response(&outcome))
                }
            })
            .await;
    }
}

#[async_trait::async_trait]
impl<C: Clock + Clone + Send + Sync + 'static> LogStatusNotifier
    for Ocpp2_0_1DiagnosticsHandler<C>
{
    type Error = ocpp_client::ClientError<ocpp_client::ocpp_2_0_1::OCPP2_0_1Error>;

    async fn notify_log_status(
        &self,
        request_id: Option<i64>,
        status: LogUploadStatus,
    ) -> Result<(), Self::Error> {
        self.client
            .send_log_status_notification(LogStatusNotificationRequest {
                custom_data: None,
                request_id,
                status: wire_upload_status(status),
                // 2.0.1's `LogStatusNotificationRequest` has no `statusInfo` field; 2.1 added it.
            })
            .await
            .map(|_| ())
    }
}

/// The `std` convenience: a bare [`OCPP2_0_1Client`] handles this block directly.
#[cfg(feature = "std")]
mod std_impls {
    use super::*;

    #[async_trait::async_trait]
    impl GetLogHandler for OCPP2_0_1Client {
        async fn register_get_log_handler(
            &self,
            actor: ChargePointActor,
            uploads: LogUploadQueue,
            state: Arc<LogUploadState>,
        ) {
            Ocpp2_0_1DiagnosticsHandler::new(self.clone())
                .register_get_log_handler(actor, uploads, state)
                .await;
        }
    }

    #[async_trait::async_trait]
    impl LogStatusNotifier for OCPP2_0_1Client {
        type Error = ocpp_client::ClientError<ocpp_client::ocpp_2_0_1::OCPP2_0_1Error>;

        async fn notify_log_status(
            &self,
            request_id: Option<i64>,
            status: LogUploadStatus,
        ) -> Result<(), Self::Error> {
            Ocpp2_0_1DiagnosticsHandler::new(self.clone())
                .notify_log_status(request_id, status)
                .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocpp_client::ocpp_types::v201::common::LogParameters;

    fn wire_request(log_type: LogEnum) -> GetLogRequest {
        GetLogRequest {
            custom_data: None,
            log: LogParameters {
                custom_data: None,
                latest_timestamp: Some("2026-03-04T05:06:07Z".into()),
                oldest_timestamp: None,
                remote_location: heapless::String::try_from("https://logs.example/up").unwrap(),
            },
            log_type,
            request_id: 42,
            retries: Some(3),
            retry_interval: Some(15),
        }
    }

    #[test]
    fn every_log_type_maps_onto_this_crates_kinds() {
        assert_eq!(map_log_kind(&LogEnum::SecurityLog), LogKind::Security);
        assert_eq!(map_log_kind(&LogEnum::DiagnosticsLog), LogKind::Diagnostics);
        // No `DataCollectorLog` here: that is 2.1's, and a 2.0.1 CSMS has no way to ask for it.
    }

    #[test]
    fn the_requests_retry_policy_and_window_come_across() {
        let mapped = map_request(&wire_request(LogEnum::SecurityLog));

        assert_eq!(mapped.request_id, Some(42));
        assert_eq!(mapped.retries, 3);
        assert_eq!(mapped.retry_interval_secs, 15);
        assert_eq!(mapped.oldest, None);
        assert!(mapped.latest.is_some());
        assert_eq!(mapped.remote_location, "https://logs.example/up");
    }

    #[test]
    fn an_absent_retry_policy_falls_back_rather_than_retrying_forever() {
        let mapped = map_request(&GetLogRequest {
            retries: None,
            retry_interval: None,
            ..wire_request(LogEnum::DiagnosticsLog)
        });

        assert_eq!(mapped.retries, super::super::DEFAULT_RETRIES);
        assert_eq!(
            mapped.retry_interval_secs,
            super::super::DEFAULT_RETRY_INTERVAL_SECS
        );
    }

    #[test]
    fn a_zero_retry_interval_is_replaced_rather_than_spun_on() {
        // A CSMS sending 0 would otherwise make the retry loop hammer the far end with no pause.
        let mapped = map_request(&GetLogRequest {
            retry_interval: Some(0),
            ..wire_request(LogEnum::SecurityLog)
        });

        assert_eq!(
            mapped.retry_interval_secs,
            super::super::DEFAULT_RETRY_INTERVAL_SECS
        );
    }

    #[test]
    fn the_response_carries_the_filename_and_the_right_status() {
        let accepted = get_log_response(&GetLogOutcome::Accepted {
            filename: "security-20260304T050607Z.log".into(),
        });
        assert_eq!(accepted.status, LogStatusEnum::Accepted);
        assert_eq!(
            accepted.filename.as_deref(),
            Some("security-20260304T050607Z.log")
        );

        let canceled = get_log_response(&GetLogOutcome::AcceptedCanceled {
            filename: "security.log".into(),
        });
        assert_eq!(canceled.status, LogStatusEnum::AcceptedCanceled);

        // Rejected names no file, because there is no upload to name one for.
        let rejected = get_log_response(&GetLogOutcome::Rejected);
        assert_eq!(rejected.status, LogStatusEnum::Rejected);
        assert!(rejected.filename.is_none());
    }
}
