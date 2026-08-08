//! Tests for the Diagnostics block's protocol-agnostic half (B5.1).

use super::*;
use crate::executor::TokioExecutor;
use crate::hardware::{Capabilities, NoFileTransferError};
use crate::security::{SecurityEventLog, SecurityLogEntry};
use crate::state::{ChargePointEvent, SecurityEvent, SecurityEventType};
use alloc::sync::Arc;
use alloc::vec::Vec;
use std::sync::Mutex as StdMutex;

fn at(secs: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap()
}

#[derive(Clone, Copy)]
struct FixedClock(DateTime<Utc>);

impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        self.0
    }
}

/// A backoff that does not wait, so a retry loop runs at test speed.
struct InstantBackoff;

#[async_trait::async_trait]
impl Backoff for InstantBackoff {
    async fn wait(&self, _seconds: u32) {
        tokio::task::yield_now().await;
    }
}

/// Records what it was asked to upload, and fails the first `failures` attempts.
#[derive(Default)]
struct RecordingTransfer {
    uploads: StdMutex<Vec<(String, Vec<u8>)>>,
    local_uploads: StdMutex<Vec<LogKind>>,
    failures_remaining: StdMutex<u32>,
}

#[async_trait::async_trait]
impl FileTransfer for RecordingTransfer {
    type Error = NoFileTransferError;

    async fn download(
        &self,
        _url: &str,
        _progress: &TransferProgress<'_>,
    ) -> Result<(), Self::Error> {
        unreachable!("the diagnostics block never downloads")
    }

    async fn upload(
        &self,
        url: &str,
        source: UploadSource<'_>,
        _progress: &TransferProgress<'_>,
    ) -> Result<(), Self::Error> {
        {
            let mut failures = self.failures_remaining.lock().unwrap();
            if *failures > 0 {
                *failures -= 1;
                return Err(NoFileTransferError {
                    attempted: "reach the far end".into(),
                });
            }
        }
        match source {
            UploadSource::Bytes(bytes) => self
                .uploads
                .lock()
                .unwrap()
                .push((url.into(), bytes.to_vec())),
            UploadSource::Local(kind) => self.local_uploads.lock().unwrap().push(kind),
        }
        Ok(())
    }
}

#[derive(Default)]
struct RecordingNotifier {
    seen: StdMutex<Vec<(Option<i64>, LogUploadStatus)>>,
}

#[async_trait::async_trait]
impl LogStatusNotifier for RecordingNotifier {
    type Error = core::convert::Infallible;

    async fn notify_log_status(
        &self,
        request_id: Option<i64>,
        status: LogUploadStatus,
    ) -> Result<(), Self::Error> {
        self.seen.lock().unwrap().push((request_id, status));
        Ok(())
    }
}

fn request(log_kind: LogKind) -> LogUploadRequest {
    LogUploadRequest {
        request_id: Some(123),
        log_kind,
        remote_location: "https://logs.example/upload".into(),
        oldest: None,
        latest: None,
        retries: 0,
        retry_interval_secs: 1,
    }
}

async fn actor_with_diagnostics() -> ChargePointActor {
    let actor = ChargePointActor::spawn([1], &TokioExecutor);
    let _ = actor
        .send(ChargePointEvent::CapabilitiesDeclared(Capabilities {
            diagnostics: true,
            ..Capabilities::default()
        }))
        .await;
    actor
}

fn entry(secs: i64, event_type: SecurityEventType, tech_info: Option<&str>) -> SecurityLogEntry {
    SecurityLogEntry {
        event: SecurityEvent {
            event_type,
            tech_info: tech_info.map(Into::into),
        },
        recorded_at: Some(at(secs)),
    }
}

async fn settle() {
    for _ in 0..400 {
        tokio::task::yield_now().await;
    }
}

#[test]
fn the_rendered_log_is_one_greppable_line_per_event() {
    let rendered = render_security_log(&[
        entry(
            0,
            SecurityEventType::StartupOfTheDevice,
            Some("Acme Model1"),
        ),
        entry(60, SecurityEventType::TamperDetectionActivated, None),
    ]);

    let text = String::from_utf8(rendered).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].starts_with(&at(0).to_rfc3339()));
    assert!(lines[0].contains("StartupOfTheDevice"));
    assert!(lines[0].ends_with("Acme Model1"));
    // A missing techInfo still leaves the trailing field, so every line has the same shape.
    assert!(lines[1].ends_with('\t'));
}

#[test]
fn an_entry_with_no_timestamp_renders_as_unknown_rather_than_the_epoch() {
    let rendered = render_security_log(&[SecurityLogEntry {
        event: SecurityEvent {
            event_type: SecurityEventType::StartupOfTheDevice,
            tech_info: None,
        },
        // Recorded before the clock was ever set - see `SecurityLogEntry::recorded_at`.
        recorded_at: None,
    }]);

    let text = String::from_utf8(rendered).unwrap();
    assert!(
        text.starts_with("-\t"),
        "an unset clock must not be rendered as 1970: {text}"
    );
}

#[test]
fn tech_info_cannot_forge_a_column_or_an_extra_line() {
    // Free-form text straight off the wire. A `techInfo` containing a newline would otherwise add
    // a line to the log, i.e. invent a security event that never happened.
    let rendered = render_security_log(&[entry(
        0,
        SecurityEventType::MaintenanceLoginFailed,
        Some("user\tadmin\n1970-01-01T00:00:00Z\tTamperDetectionActivated\tcleared"),
    )]);

    let text = String::from_utf8(rendered).unwrap();
    assert_eq!(text.lines().count(), 1);
    assert_eq!(text.matches('\t').count(), 2);
}

#[tokio::test]
async fn an_accepted_get_log_names_a_file_and_uploads_the_security_log() {
    let actor = actor_with_diagnostics().await;
    let uploads = LogUploadQueue::new();
    let state = Arc::new(LogUploadState::new());
    let transfer = Arc::new(RecordingTransfer::default());
    let notifier = Arc::new(RecordingNotifier::default());
    let log = Arc::new(SecurityEventLog::new());
    log.record(entry(0, SecurityEventType::StartupOfTheDevice, None));

    let outcome = handle_get_log(
        &actor,
        &uploads,
        &state,
        &FixedClock(at(0)),
        request(LogKind::Security),
    )
    .await;

    // N01.FR.01: the response names the file the CSMS should expect.
    assert_eq!(
        outcome,
        GetLogOutcome::Accepted {
            filename: "security-20270115T080000Z.log".into()
        }
    );

    let worker = {
        let (uploads, state, transfer, notifier, log) = (
            uploads.clone(),
            state.clone(),
            transfer.clone(),
            notifier.clone(),
            log.clone(),
        );
        tokio::spawn(async move {
            run_log_uploads(uploads, &state, &transfer, &notifier, &log, &InstantBackoff).await;
        })
    };
    settle().await;
    worker.abort();

    // N01.FR.08/.09, with N01.FR.07's request id on both.
    assert_eq!(
        *notifier.seen.lock().unwrap(),
        alloc::vec![
            (Some(123), LogUploadStatus::Uploading),
            (Some(123), LogUploadStatus::Uploaded)
        ]
    );
    let uploaded = transfer.uploads.lock().unwrap();
    assert_eq!(uploaded.len(), 1);
    assert_eq!(uploaded[0].0, "https://logs.example/upload");
    assert!(
        String::from_utf8(uploaded[0].1.clone())
            .unwrap()
            .contains("StartupOfTheDevice")
    );
}

#[tokio::test]
async fn a_diagnostics_log_is_asked_for_by_name_rather_than_handed_over() {
    let actor = actor_with_diagnostics().await;
    let uploads = LogUploadQueue::new();
    let state = Arc::new(LogUploadState::new());
    let transfer = Arc::new(RecordingTransfer::default());
    let notifier = Arc::new(RecordingNotifier::default());
    let log = Arc::new(SecurityEventLog::new());

    handle_get_log(
        &actor,
        &uploads,
        &state,
        &FixedClock(at(0)),
        request(LogKind::Diagnostics),
    )
    .await;

    let worker = {
        let (uploads, state, transfer, notifier, log) = (
            uploads.clone(),
            state.clone(),
            transfer.clone(),
            notifier.clone(),
            log.clone(),
        );
        tokio::spawn(async move {
            run_log_uploads(uploads, &state, &transfer, &notifier, &log, &InstantBackoff).await;
        })
    };
    settle().await;
    worker.abort();

    // This crate has no idea what this charge point's diagnostics are, and does not pretend to.
    assert_eq!(
        *transfer.local_uploads.lock().unwrap(),
        alloc::vec![LogKind::Diagnostics]
    );
    assert!(transfer.uploads.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_failure_is_reported_once_after_the_retries_the_csms_asked_for() {
    let actor = actor_with_diagnostics().await;
    let uploads = LogUploadQueue::new();
    let state = Arc::new(LogUploadState::new());
    let transfer = Arc::new(RecordingTransfer {
        failures_remaining: StdMutex::new(2),
        ..Default::default()
    });
    let notifier = Arc::new(RecordingNotifier::default());
    let log = Arc::new(SecurityEventLog::new());

    // Two retries beyond the first attempt: the third attempt is the one that succeeds.
    handle_get_log(
        &actor,
        &uploads,
        &state,
        &FixedClock(at(0)),
        LogUploadRequest {
            retries: 2,
            ..request(LogKind::Security)
        },
    )
    .await;

    let worker = {
        let (uploads, state, transfer, notifier, log) = (
            uploads.clone(),
            state.clone(),
            transfer.clone(),
            notifier.clone(),
            log.clone(),
        );
        tokio::spawn(async move {
            run_log_uploads(uploads, &state, &transfer, &notifier, &log, &InstantBackoff).await;
        })
    };
    settle().await;
    worker.abort();

    // N01.FR.10 recommends reporting only after every attempt has failed - so a retry that
    // eventually works produces no failure status at all.
    assert_eq!(
        *notifier.seen.lock().unwrap(),
        alloc::vec![
            (Some(123), LogUploadStatus::Uploading),
            (Some(123), LogUploadStatus::Uploaded)
        ]
    );
    assert_eq!(transfer.uploads.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn every_attempt_failing_reports_one_upload_failure() {
    let actor = actor_with_diagnostics().await;
    let uploads = LogUploadQueue::new();
    let state = Arc::new(LogUploadState::new());
    let transfer = Arc::new(RecordingTransfer {
        failures_remaining: StdMutex::new(u32::MAX),
        ..Default::default()
    });
    let notifier = Arc::new(RecordingNotifier::default());
    let log = Arc::new(SecurityEventLog::new());

    handle_get_log(
        &actor,
        &uploads,
        &state,
        &FixedClock(at(0)),
        LogUploadRequest {
            retries: 2,
            ..request(LogKind::Security)
        },
    )
    .await;

    let worker = {
        let (uploads, state, transfer, notifier, log) = (
            uploads.clone(),
            state.clone(),
            transfer.clone(),
            notifier.clone(),
            log.clone(),
        );
        tokio::spawn(async move {
            run_log_uploads(uploads, &state, &transfer, &notifier, &log, &InstantBackoff).await;
        })
    };
    settle().await;
    worker.abort();

    assert_eq!(
        *notifier.seen.lock().unwrap(),
        alloc::vec![
            (Some(123), LogUploadStatus::Uploading),
            (Some(123), LogUploadStatus::UploadFailure)
        ]
    );
}

#[tokio::test]
async fn a_second_get_log_supersedes_the_first_and_answers_accepted_canceled() {
    let actor = actor_with_diagnostics().await;
    let uploads = LogUploadQueue::new();
    let state = Arc::new(LogUploadState::new());

    let first = handle_get_log(
        &actor,
        &uploads,
        &state,
        &FixedClock(at(0)),
        request(LogKind::Security),
    )
    .await;
    let second = handle_get_log(
        &actor,
        &uploads,
        &state,
        &FixedClock(at(0)),
        request(LogKind::Security),
    )
    .await;

    assert!(matches!(first, GetLogOutcome::Accepted { .. }));
    // N01.FR.12.
    assert!(matches!(second, GetLogOutcome::AcceptedCanceled { .. }));
}

#[tokio::test]
async fn a_superseded_upload_reports_nothing_so_the_csms_hears_only_about_the_one_it_kept() {
    let actor = actor_with_diagnostics().await;
    let uploads = LogUploadQueue::new();
    let state = Arc::new(LogUploadState::new());
    let transfer = Arc::new(RecordingTransfer::default());
    let notifier = Arc::new(RecordingNotifier::default());
    let log = Arc::new(SecurityEventLog::new());

    // Both queued before the worker starts, so the first is superseded before it ever runs.
    handle_get_log(
        &actor,
        &uploads,
        &state,
        &FixedClock(at(0)),
        LogUploadRequest {
            request_id: Some(1),
            ..request(LogKind::Security)
        },
    )
    .await;
    handle_get_log(
        &actor,
        &uploads,
        &state,
        &FixedClock(at(0)),
        LogUploadRequest {
            request_id: Some(2),
            ..request(LogKind::Security)
        },
    )
    .await;

    let worker = {
        let (uploads, state, transfer, notifier, log) = (
            uploads.clone(),
            state.clone(),
            transfer.clone(),
            notifier.clone(),
            log.clone(),
        );
        tokio::spawn(async move {
            run_log_uploads(uploads, &state, &transfer, &notifier, &log, &InstantBackoff).await;
        })
    };
    settle().await;
    worker.abort();

    // A stale `Uploaded` for request 1 would tell the CSMS an upload it was told was cancelled
    // had finished after all.
    assert_eq!(
        *notifier.seen.lock().unwrap(),
        alloc::vec![
            (Some(2), LogUploadStatus::Uploading),
            (Some(2), LogUploadStatus::Uploaded)
        ]
    );
}

#[tokio::test]
async fn a_charge_point_without_the_diagnostics_capability_rejects() {
    let actor = ChargePointActor::spawn([1], &TokioExecutor);
    let _ = actor
        .send(ChargePointEvent::CapabilitiesDeclared(
            Capabilities::default(),
        ))
        .await;

    let outcome = handle_get_log(
        &actor,
        &LogUploadQueue::new(),
        &LogUploadState::new(),
        &FixedClock(at(0)),
        request(LogKind::Security),
    )
    .await;

    // N01.FR.05: the log is not available, and saying so is different from failing later.
    assert_eq!(outcome, GetLogOutcome::Rejected);
}

#[test]
fn the_requested_window_narrows_the_log_but_keeps_untimed_entries() {
    let log = SecurityEventLog::new();
    log.record(entry(0, SecurityEventType::StartupOfTheDevice, None));
    log.record(entry(
        600,
        SecurityEventType::TamperDetectionActivated,
        None,
    ));
    log.record(SecurityLogEntry {
        event: SecurityEvent {
            event_type: SecurityEventType::MemoryExhaustion,
            tech_info: None,
        },
        recorded_at: None,
    });

    let filtered = filtered_entries(
        &log,
        &LogUploadRequest {
            oldest: Some(at(300)),
            latest: None,
            ..request(LogKind::Security)
        },
    );

    let kinds: Vec<SecurityEventType> = filtered
        .into_iter()
        .map(|entry| entry.event.event_type)
        .collect();
    // The untimed entry is kept: it cannot be shown to fall outside the window, and the period
    // around an unset clock is exactly when something interesting tends to have happened.
    assert_eq!(
        kinds,
        alloc::vec![
            SecurityEventType::TamperDetectionActivated,
            SecurityEventType::MemoryExhaustion
        ]
    );
}
