//! OCPP 1.6J wire adapter for the Diagnostics block's log upload (B5.1).
//!
//! 1.6J's `GetDiagnostics`/`DiagnosticsStatusNotification` is the same flow as 2.x's
//! `GetLog`/`LogStatusNotification`, but it is genuinely poorer rather than merely differently
//! named, and this crate's internal model carries the superset (per `CLAUDE.md`). Three real
//! losses, all one-directional:
//!
//! - **No log type.** 1.6J has only "diagnostics", so a 1.6J CSMS cannot ask for the security log
//!   at all - see [`crate::diagnostics::render_security_log`] for what it is missing. Every 1.6J
//!   request maps to [`LogKind::Diagnostics`].
//! - **No `requestId`.** Nothing correlates a status notification with the request that caused it,
//!   so this adapter reports `None` and the wire message has no field for one either. 2.x's
//!   N01.FR.07 has no 1.6J counterpart to violate.
//! - **No `AcceptedCanceled`.** `GetDiagnosticsResponse` carries a file name and nothing else, so
//!   a supersede is invisible to a 1.6J CSMS; it still happens internally, and the superseded
//!   upload still reports nothing.

use alloc::boxed::Box;
use alloc::string::ToString;
use alloc::sync::Arc;
use chrono::{DateTime, Utc};

use ocpp_client::ocpp_1_6::OCPP1_6Client;
use ocpp_client::ocpp_types::v16::common::DiagnosticsStatusNotificationRequestStatus;
use ocpp_client::ocpp_types::v16::{
    DiagnosticsStatusNotificationRequest, GetDiagnosticsRequest, GetDiagnosticsResponse,
};

use crate::actor::ChargePointActor;
use crate::clock::Clock;
use crate::diagnostics::{
    GetLogHandler, GetLogOutcome, LogStatusNotifier, LogUploadQueue, LogUploadRequest,
    LogUploadState, LogUploadStatus, handle_get_log,
};
use crate::hardware::LogKind;

/// 1.6J timestamps are the same RFC 3339 strings 2.x uses; an unparseable one becomes no filter,
/// exactly as in the 2.x adapters.
fn parse_time(raw: &Option<alloc::string::String>) -> Option<DateTime<Utc>> {
    raw.as_ref()
        .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        .map(|parsed| parsed.with_timezone(&Utc))
}

fn map_request(request: &GetDiagnosticsRequest) -> LogUploadRequest {
    LogUploadRequest {
        // 1.6J has nothing to correlate with - see the module docs.
        request_id: None,
        log_kind: LogKind::Diagnostics,
        remote_location: request.location.to_string(),
        oldest: parse_time(&request.start_time),
        latest: parse_time(&request.stop_time),
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

/// 1.6J's response is a file name or nothing at all - there is no status field, so a refusal is
/// expressed by naming no file.
fn get_diagnostics_response(outcome: &GetLogOutcome) -> GetDiagnosticsResponse {
    GetDiagnosticsResponse {
        file_name: outcome
            .filename()
            .and_then(|name| heapless::String::try_from(name).ok()),
    }
}

fn wire_status(status: LogUploadStatus) -> DiagnosticsStatusNotificationRequestStatus {
    match status {
        LogUploadStatus::Uploading => DiagnosticsStatusNotificationRequestStatus::Uploading,
        LogUploadStatus::Uploaded => DiagnosticsStatusNotificationRequestStatus::Uploaded,
        LogUploadStatus::UploadFailure => DiagnosticsStatusNotificationRequestStatus::UploadFailed,
    }
}

/// Wraps an [`OCPP1_6Client`] with the [`Clock`] the handler needs to name a log file.
pub struct Ocpp1_6DiagnosticsHandler<C> {
    client: OCPP1_6Client,
    clock: C,
}

impl<C: Clock + Clone + Send + Sync + 'static> Ocpp1_6DiagnosticsHandler<C> {
    /// Wraps `client`, naming log files from `clock`.
    pub fn with_clock(client: OCPP1_6Client, clock: C) -> Self {
        Self { client, clock }
    }
}

#[cfg(feature = "std")]
impl Ocpp1_6DiagnosticsHandler<crate::clock::SystemClock> {
    /// Wraps `client`, naming log files from [`crate::clock::SystemClock`].
    pub fn new(client: OCPP1_6Client) -> Self {
        Self::with_clock(client, crate::clock::SystemClock)
    }
}

#[async_trait::async_trait]
impl<C: Clock + Clone + Send + Sync + 'static> GetLogHandler for Ocpp1_6DiagnosticsHandler<C> {
    async fn register_get_log_handler(
        &self,
        actor: ChargePointActor,
        uploads: LogUploadQueue,
        state: Arc<LogUploadState>,
    ) {
        let clock = self.clock.clone();
        self.client
            .on_get_diagnostics(move |request: GetDiagnosticsRequest, _client| {
                let actor = actor.clone();
                let uploads = uploads.clone();
                let state = state.clone();
                let clock = clock.clone();
                async move {
                    let outcome =
                        handle_get_log(&actor, &uploads, &state, &clock, map_request(&request))
                            .await;
                    Ok(get_diagnostics_response(&outcome))
                }
            })
            .await;
    }
}

#[async_trait::async_trait]
impl<C: Clock + Clone + Send + Sync + 'static> LogStatusNotifier for Ocpp1_6DiagnosticsHandler<C> {
    type Error = ocpp_client::ClientError<ocpp_client::ocpp_1_6::OCPP1_6Error>;

    async fn notify_log_status(
        &self,
        _request_id: Option<i64>,
        status: LogUploadStatus,
    ) -> Result<(), Self::Error> {
        self.client
            .send_diagnostics_status_notification(DiagnosticsStatusNotificationRequest {
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
    impl GetLogHandler for OCPP1_6Client {
        async fn register_get_log_handler(
            &self,
            actor: ChargePointActor,
            uploads: LogUploadQueue,
            state: Arc<LogUploadState>,
        ) {
            Ocpp1_6DiagnosticsHandler::new(self.clone())
                .register_get_log_handler(actor, uploads, state)
                .await;
        }
    }

    #[async_trait::async_trait]
    impl LogStatusNotifier for OCPP1_6Client {
        type Error = ocpp_client::ClientError<ocpp_client::ocpp_1_6::OCPP1_6Error>;

        async fn notify_log_status(
            &self,
            request_id: Option<i64>,
            status: LogUploadStatus,
        ) -> Result<(), Self::Error> {
            Ocpp1_6DiagnosticsHandler::new(self.clone())
                .notify_log_status(request_id, status)
                .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire_request() -> GetDiagnosticsRequest {
        GetDiagnosticsRequest {
            location: "ftp://logs.example/up".into(),
            retries: Some(2),
            retry_interval: Some(20),
            start_time: Some("2026-03-04T05:06:07Z".into()),
            stop_time: None,
        }
    }

    #[test]
    fn a_1_6_request_always_means_the_diagnostics_log() {
        let mapped = map_request(&wire_request());

        // 1.6J cannot ask for the security log at all - it has no log type to ask with.
        assert_eq!(mapped.log_kind, LogKind::Diagnostics);
        assert_eq!(mapped.request_id, None);
        assert_eq!(mapped.remote_location, "ftp://logs.example/up");
        assert_eq!(mapped.retries, 2);
        assert_eq!(mapped.retry_interval_secs, 20);
        assert!(mapped.oldest.is_some());
        assert_eq!(mapped.latest, None);
    }

    #[test]
    fn a_refusal_is_expressed_by_naming_no_file() {
        // `GetDiagnosticsResponse` has no status field, so this is the only way 1.6J can say no.
        assert!(
            get_diagnostics_response(&GetLogOutcome::Rejected)
                .file_name
                .is_none()
        );
        assert_eq!(
            get_diagnostics_response(&GetLogOutcome::Accepted {
                filename: "diagnostics-20260304T050607Z.log".into()
            })
            .file_name
            .as_deref(),
            Some("diagnostics-20260304T050607Z.log")
        );
    }

    #[test]
    fn a_supersede_is_invisible_to_a_1_6_csms_but_still_names_its_file() {
        // No `AcceptedCanceled` in 1.6J. The supersede still happened internally; what a 1.6J
        // CSMS sees is an accept, which is the closest thing its response can express.
        let response = get_diagnostics_response(&GetLogOutcome::AcceptedCanceled {
            filename: "diagnostics.log".into(),
        });

        assert_eq!(response.file_name.as_deref(), Some("diagnostics.log"));
    }

    #[test]
    fn every_status_maps_onto_1_6s_narrower_enum() {
        assert_eq!(
            wire_status(LogUploadStatus::Uploading),
            DiagnosticsStatusNotificationRequestStatus::Uploading
        );
        assert_eq!(
            wire_status(LogUploadStatus::Uploaded),
            DiagnosticsStatusNotificationRequestStatus::Uploaded
        );
        // 1.6J spells this one `UploadFailed`, not 2.x's `UploadFailure`.
        assert_eq!(
            wire_status(LogUploadStatus::UploadFailure),
            DiagnosticsStatusNotificationRequestStatus::UploadFailed
        );
    }
}
