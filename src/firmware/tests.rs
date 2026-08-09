//! Tests for the Firmware Management block's protocol-agnostic state machine (B3.2).

use super::*;
use crate::executor::TokioExecutor;
use crate::hardware::{
    Capabilities, FirmwareVerificationOutcome, FirmwareVerifier, NoFileTransferError,
    NoFirmwareInstallerError, NoFirmwareVerifier, NoFirmwareVerifierError, UploadSource,
};
use crate::state::{ConnectorEvent, IdToken, IdTokenKind};
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

struct InstantBackoff;

#[async_trait::async_trait]
impl Backoff for InstantBackoff {
    async fn wait(&self, _seconds: u32) {
        tokio::task::yield_now().await;
    }
}

#[derive(Default)]
struct FakeTransfer {
    downloads: StdMutex<Vec<String>>,
    failures_remaining: StdMutex<u32>,
}

#[async_trait::async_trait]
impl FileTransfer for FakeTransfer {
    type Error = NoFileTransferError;

    async fn download(
        &self,
        url: &str,
        _progress: &TransferProgress<'_>,
    ) -> Result<(), Self::Error> {
        let mut failures = self.failures_remaining.lock().unwrap();
        if *failures > 0 {
            *failures -= 1;
            return Err(NoFileTransferError {
                attempted: "fetch the image".into(),
            });
        }
        self.downloads.lock().unwrap().push(url.into());
        Ok(())
    }

    async fn upload(
        &self,
        _url: &str,
        _source: UploadSource<'_>,
        _progress: &TransferProgress<'_>,
    ) -> Result<(), Self::Error> {
        unreachable!("the firmware block never uploads")
    }
}

struct FakeInstaller {
    outcome: StdMutex<Result<FirmwareInstallOutcome, ()>>,
    installs: StdMutex<u32>,
}

impl FakeInstaller {
    fn new(outcome: Result<FirmwareInstallOutcome, ()>) -> Self {
        Self {
            outcome: StdMutex::new(outcome),
            installs: StdMutex::new(0),
        }
    }
}

#[async_trait::async_trait]
impl FirmwareInstaller for FakeInstaller {
    type Error = NoFirmwareInstallerError;

    async fn install(&self) -> Result<FirmwareInstallOutcome, Self::Error> {
        *self.installs.lock().unwrap() += 1;
        (*self.outcome.lock().unwrap()).map_err(|()| NoFirmwareInstallerError)
    }
}

struct FakeVerifier {
    outcome: StdMutex<Result<FirmwareVerificationOutcome, ()>>,
    calls: StdMutex<u32>,
}

impl FakeVerifier {
    fn new(outcome: Result<FirmwareVerificationOutcome, ()>) -> Self {
        Self {
            outcome: StdMutex::new(outcome),
            calls: StdMutex::new(0),
        }
    }
}

#[async_trait::async_trait]
impl FirmwareVerifier for FakeVerifier {
    type Error = NoFirmwareVerifierError;

    async fn verify(
        &self,
        _signing_certificate: Option<&str>,
        _signature: Option<&str>,
    ) -> Result<FirmwareVerificationOutcome, Self::Error> {
        *self.calls.lock().unwrap() += 1;
        (*self.outcome.lock().unwrap()).map_err(|()| NoFirmwareVerifierError)
    }
}

#[derive(Default)]
struct RecordingNotifier {
    seen: StdMutex<Vec<(Option<i64>, FirmwareStatus)>>,
}

impl RecordingNotifier {
    fn statuses(&self) -> Vec<FirmwareStatus> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|(_, status)| *status)
            .collect()
    }
}

#[async_trait::async_trait]
impl FirmwareStatusNotifier for RecordingNotifier {
    type Error = core::convert::Infallible;

    async fn notify_firmware_status(
        &self,
        request_id: Option<i64>,
        status: FirmwareStatus,
    ) -> Result<(), Self::Error> {
        self.seen.lock().unwrap().push((request_id, status));
        Ok(())
    }
}

fn request() -> FirmwareUpdateRequest {
    FirmwareUpdateRequest {
        request_id: Some(9),
        location: "https://fw.example/image.bin".into(),
        retrieve_at: None,
        install_at: None,
        signature: None,
        signing_certificate: None,
        retries: 0,
        retry_interval_secs: 1,
    }
}

/// A signed update - carries both `signature` and `signing_certificate`, which is what turns on
/// B3.3's verification gate.
fn signed_request() -> FirmwareUpdateRequest {
    FirmwareUpdateRequest {
        signature: Some("c2ln".into()),
        signing_certificate: Some("Y2VydA==".into()),
        ..request()
    }
}

async fn actor_with_firmware() -> ChargePointActor {
    let actor = ChargePointActor::spawn([1], &TokioExecutor);
    let _ = actor
        .send(ChargePointEvent::CapabilitiesDeclared(Capabilities {
            firmware_management: true,
            ..Capabilities::default()
        }))
        .await;
    actor
}

async fn settle() {
    for _ in 0..600 {
        tokio::task::yield_now().await;
    }
}

/// Drives one accepted update to completion and hands back what was reported.
async fn run(
    actor: &ChargePointActor,
    updates: FirmwareUpdateQueue,
    state: Arc<FirmwareUpdateState>,
    transfer: Arc<FakeTransfer>,
    installer: Arc<FakeInstaller>,
    notifier: Arc<RecordingNotifier>,
    clock: FixedClock,
) {
    run_with_verifier(
        actor,
        updates,
        state,
        transfer,
        installer,
        notifier,
        clock,
        Arc::new(NoFirmwareVerifier),
    )
    .await;
}

/// Like [`run`], but with a caller-chosen [`FirmwareVerifier`] rather than
/// [`NoFirmwareVerifier`] - for exercising B3.3's verification gate.
#[allow(clippy::too_many_arguments)]
async fn run_with_verifier<V: FirmwareVerifier + Send + Sync + 'static>(
    actor: &ChargePointActor,
    updates: FirmwareUpdateQueue,
    state: Arc<FirmwareUpdateState>,
    transfer: Arc<FakeTransfer>,
    installer: Arc<FakeInstaller>,
    notifier: Arc<RecordingNotifier>,
    clock: FixedClock,
    verifier: Arc<V>,
) {
    let worker_actor = actor.clone();
    let handle = tokio::spawn(async move {
        run_firmware_updates(
            &worker_actor,
            updates,
            &state,
            &transfer,
            &installer,
            &notifier,
            &clock,
            &InstantBackoff,
            &verifier,
        )
        .await;
    });
    settle().await;
    handle.abort();
}

#[tokio::test]
async fn a_straightforward_update_walks_downloading_downloaded_installing_installed() {
    let actor = actor_with_firmware().await;
    let updates = FirmwareUpdateQueue::new();
    let state = Arc::new(FirmwareUpdateState::new());
    let transfer = Arc::new(FakeTransfer::default());
    let installer = Arc::new(FakeInstaller::new(Ok(FirmwareInstallOutcome::Installed)));
    let notifier = Arc::new(RecordingNotifier::default());

    let outcome = handle_update_firmware(&actor, &updates, &state, request()).await;
    assert_eq!(outcome, UpdateFirmwareOutcome::Accepted);

    run(
        &actor,
        updates,
        state,
        transfer.clone(),
        installer.clone(),
        notifier.clone(),
        FixedClock(at(0)),
    )
    .await;

    // L01.FR.01: every state change is reported, in order.
    assert_eq!(
        notifier.statuses(),
        alloc::vec![
            FirmwareStatus::Downloading,
            FirmwareStatus::Downloaded,
            FirmwareStatus::Installing,
            FirmwareStatus::Installed
        ]
    );
    // L01.FR.10: all carrying the request id that started the update.
    assert!(
        notifier
            .seen
            .lock()
            .unwrap()
            .iter()
            .all(|(id, _)| *id == Some(9))
    );
    assert_eq!(
        *transfer.downloads.lock().unwrap(),
        alloc::vec!["https://fw.example/image.bin".to_string()]
    );
    assert_eq!(*installer.installs.lock().unwrap(), 1);
}

#[tokio::test]
async fn a_future_retrieve_time_is_announced_as_scheduled_rather_than_silently_delayed() {
    let actor = actor_with_firmware().await;
    let updates = FirmwareUpdateQueue::new();
    let state = Arc::new(FirmwareUpdateState::new());
    let notifier = Arc::new(RecordingNotifier::default());

    handle_update_firmware(
        &actor,
        &updates,
        &state,
        FirmwareUpdateRequest {
            retrieve_at: Some(at(3_600)),
            ..request()
        },
    )
    .await;

    run(
        &actor,
        updates,
        state,
        Arc::new(FakeTransfer::default()),
        Arc::new(FakeInstaller::new(Ok(FirmwareInstallOutcome::Installed))),
        notifier.clone(),
        FixedClock(at(0)),
    )
    .await;

    // L01.FR.13. A CSMS that heard nothing could not tell a scheduled update from a lost one, so
    // this must be reported and must then stay waiting.
    assert_eq!(
        notifier.statuses(),
        alloc::vec![FirmwareStatus::DownloadScheduled]
    );
}

#[tokio::test]
async fn a_future_install_time_is_announced_after_the_download_completes() {
    let actor = actor_with_firmware().await;
    let updates = FirmwareUpdateQueue::new();
    let state = Arc::new(FirmwareUpdateState::new());
    let notifier = Arc::new(RecordingNotifier::default());

    handle_update_firmware(
        &actor,
        &updates,
        &state,
        FirmwareUpdateRequest {
            install_at: Some(at(3_600)),
            ..request()
        },
    )
    .await;

    run(
        &actor,
        updates,
        state,
        Arc::new(FakeTransfer::default()),
        Arc::new(FakeInstaller::new(Ok(FirmwareInstallOutcome::Installed))),
        notifier.clone(),
        FixedClock(at(0)),
    )
    .await;

    // L01.FR.16: the image is fetched, then the install waits - the download is not held back
    // just because the installation is scheduled for later.
    assert_eq!(
        notifier.statuses(),
        alloc::vec![
            FirmwareStatus::Downloading,
            FirmwareStatus::Downloaded,
            FirmwareStatus::InstallScheduled
        ]
    );
}

#[tokio::test]
async fn an_unsynchronized_clock_starts_the_update_rather_than_waiting_forever() {
    let actor = actor_with_firmware().await;
    let updates = FirmwareUpdateQueue::new();
    let state = Arc::new(FirmwareUpdateState::new());
    let notifier = Arc::new(RecordingNotifier::default());

    handle_update_firmware(
        &actor,
        &updates,
        &state,
        FirmwareUpdateRequest {
            retrieve_at: Some(at(3_600)),
            ..request()
        },
    )
    .await;

    run(
        &actor,
        updates,
        state,
        Arc::new(FakeTransfer::default()),
        Arc::new(FakeInstaller::new(Ok(FirmwareInstallOutcome::Installed))),
        notifier.clone(),
        // An unset RTC. A charge point that cannot know the scheduled instant has arrived would
        // otherwise never start - a worse failure than starting early, since the CSMS asked for
        // the update either way.
        FixedClock(DateTime::from_timestamp(0, 0).unwrap()),
    )
    .await;

    assert_eq!(
        notifier.statuses(),
        alloc::vec![
            FirmwareStatus::Downloading,
            FirmwareStatus::Downloaded,
            FirmwareStatus::Installing,
            FirmwareStatus::Installed
        ]
    );
}

#[tokio::test]
async fn every_download_attempt_failing_reports_download_failed_and_installs_nothing() {
    let actor = actor_with_firmware().await;
    let updates = FirmwareUpdateQueue::new();
    let state = Arc::new(FirmwareUpdateState::new());
    let transfer = Arc::new(FakeTransfer {
        failures_remaining: StdMutex::new(u32::MAX),
        ..Default::default()
    });
    let installer = Arc::new(FakeInstaller::new(Ok(FirmwareInstallOutcome::Installed)));
    let notifier = Arc::new(RecordingNotifier::default());

    handle_update_firmware(
        &actor,
        &updates,
        &state,
        FirmwareUpdateRequest {
            retries: 2,
            ..request()
        },
    )
    .await;

    run(
        &actor,
        updates,
        state,
        transfer,
        installer.clone(),
        notifier.clone(),
        FixedClock(at(0)),
    )
    .await;

    assert_eq!(
        notifier.statuses(),
        alloc::vec![FirmwareStatus::Downloading, FirmwareStatus::DownloadFailed]
    );
    // Nothing was installed, which is the point: a failed download must not reach the installer.
    assert_eq!(*installer.installs.lock().unwrap(), 0);
}

#[tokio::test]
async fn a_reboot_is_announced_before_it_happens_because_afterwards_nothing_can_announce_it() {
    let actor = actor_with_firmware().await;
    let updates = FirmwareUpdateQueue::new();
    let state = Arc::new(FirmwareUpdateState::new());
    let notifier = Arc::new(RecordingNotifier::default());

    handle_update_firmware(&actor, &updates, &state, request()).await;

    run(
        &actor,
        updates,
        state,
        Arc::new(FakeTransfer::default()),
        Arc::new(FakeInstaller::new(Ok(
            FirmwareInstallOutcome::RebootRequired,
        ))),
        notifier.clone(),
        FixedClock(at(0)),
    )
    .await;

    // L01.FR.15. `Installed` is deliberately absent: the new image has not booted yet, and
    // claiming otherwise would have the CSMS record a version this charge point is not running.
    assert_eq!(
        notifier.statuses(),
        alloc::vec![
            FirmwareStatus::Downloading,
            FirmwareStatus::Downloaded,
            FirmwareStatus::Installing,
            FirmwareStatus::InstallRebooting
        ]
    );
}

#[tokio::test]
async fn a_failed_install_reports_it_and_puts_the_charge_point_back_in_service() {
    let actor = actor_with_firmware().await;
    let updates = FirmwareUpdateQueue::new();
    let state = Arc::new(FirmwareUpdateState::new());
    let notifier = Arc::new(RecordingNotifier::default());

    handle_update_firmware(&actor, &updates, &state, request()).await;

    run(
        &actor,
        updates,
        state,
        Arc::new(FakeTransfer::default()),
        Arc::new(FakeInstaller::new(Err(()))),
        notifier.clone(),
        FixedClock(at(0)),
    )
    .await;

    assert_eq!(
        notifier.statuses(),
        alloc::vec![
            FirmwareStatus::Downloading,
            FirmwareStatus::Downloaded,
            FirmwareStatus::Installing,
            FirmwareStatus::InstallationFailed
        ]
    );
    // L01.FR.07 held new sessions off while waiting; a failed update must not leave the charge
    // point silently out of service afterwards.
    assert_eq!(
        actor.state().evses[0].status,
        crate::state::EvseStatus::Available
    );
}

#[tokio::test]
async fn installation_waits_for_a_running_transaction_and_holds_new_sessions_off() {
    let actor = actor_with_firmware().await;
    let updates = FirmwareUpdateQueue::new();
    let state = Arc::new(FirmwareUpdateState::new());
    let installer = Arc::new(FakeInstaller::new(Ok(FirmwareInstallOutcome::Installed)));
    let notifier = Arc::new(RecordingNotifier::default());

    // Start charging, so the installer must not be reached.
    let id_token = IdToken {
        value: "04A224B2".into(),
        kind: IdTokenKind::ISO14443,
    };
    for event in [
        ConnectorEvent::CableConnected,
        ConnectorEvent::LockConfirmed,
        ConnectorEvent::IdTokenPresented(id_token.clone()),
        ConnectorEvent::ChargingAuthorized(id_token),
        ConnectorEvent::ContactorClosed,
    ] {
        let _ = actor
            .send(ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event,
                },
            })
            .await;
    }

    handle_update_firmware(&actor, &updates, &state, request()).await;
    run(
        &actor,
        updates,
        state,
        Arc::new(FakeTransfer::default()),
        installer.clone(),
        notifier.clone(),
        FixedClock(at(0)),
    )
    .await;

    // L01.FR.06: downloaded, but not installed - a reboot mid-charge would cut the driver off.
    assert_eq!(
        notifier.statuses(),
        alloc::vec![FirmwareStatus::Downloading, FirmwareStatus::Downloaded]
    );
    assert_eq!(*installer.installs.lock().unwrap(), 0);
    // L01.FR.07: and the EVSE is held unavailable meanwhile, so nobody else starts a session that
    // the pending reboot would kill.
    assert_eq!(
        actor.state().evses[0].status,
        crate::state::EvseStatus::Unavailable
    );
}

#[tokio::test]
async fn a_second_update_supersedes_the_first_and_reports_accepted_canceled() {
    let actor = actor_with_firmware().await;
    let updates = FirmwareUpdateQueue::new();
    let state = Arc::new(FirmwareUpdateState::new());
    let notifier = Arc::new(RecordingNotifier::default());

    let first = handle_update_firmware(&actor, &updates, &state, request()).await;
    let second = handle_update_firmware(
        &actor,
        &updates,
        &state,
        FirmwareUpdateRequest {
            request_id: Some(10),
            ..request()
        },
    )
    .await;

    assert_eq!(first, UpdateFirmwareOutcome::Accepted);
    // L01.FR.24.
    assert_eq!(second, UpdateFirmwareOutcome::AcceptedCanceled);

    run(
        &actor,
        updates,
        state,
        Arc::new(FakeTransfer::default()),
        Arc::new(FakeInstaller::new(Ok(FirmwareInstallOutcome::Installed))),
        notifier.clone(),
        FixedClock(at(0)),
    )
    .await;

    // Only the surviving update reports anything - a status for the cancelled one would tell the
    // CSMS about an update it was told was replaced.
    assert!(
        notifier
            .seen
            .lock()
            .unwrap()
            .iter()
            .all(|(id, _)| *id == Some(10))
    );
}

#[tokio::test]
async fn a_charge_point_without_firmware_management_rejects() {
    let actor = ChargePointActor::spawn([1], &TokioExecutor);
    let _ = actor
        .send(ChargePointEvent::CapabilitiesDeclared(
            Capabilities::default(),
        ))
        .await;

    let outcome = handle_update_firmware(
        &actor,
        &FirmwareUpdateQueue::new(),
        &FirmwareUpdateState::new(),
        request(),
    )
    .await;

    assert_eq!(outcome, UpdateFirmwareOutcome::Rejected);
}

#[test]
fn a_triggered_status_is_idle_only_once_the_last_update_finished_installing() {
    let state = FirmwareUpdateState::new();

    // L01.FR.26: mid-update, the last status is repeated, with its request id.
    state.record(Some(4), FirmwareStatus::Downloading);
    assert_eq!(
        state.triggered_status(),
        (Some(4), FirmwareStatus::Downloading)
    );

    // L01.FR.25: after `Installed`, `Idle` - and L01.FR.20's exception, no request id, because
    // there is no longer an update to correlate to.
    state.record(Some(4), FirmwareStatus::Installed);
    assert_eq!(state.triggered_status(), (None, FirmwareStatus::Idle));
}

// --- B3.3: signature verification ------------------------------------------------------------

#[tokio::test]
async fn an_unsigned_update_never_calls_the_verifier() {
    let actor = actor_with_firmware().await;
    let updates = FirmwareUpdateQueue::new();
    let state = Arc::new(FirmwareUpdateState::new());
    let notifier = Arc::new(RecordingNotifier::default());
    // Would report `Valid` if ever called - proves the skip, not just a lucky pass.
    let verifier = Arc::new(FakeVerifier::new(Ok(FirmwareVerificationOutcome::Valid)));

    handle_update_firmware(&actor, &updates, &state, request()).await;
    run_with_verifier(
        &actor,
        updates,
        state,
        Arc::new(FakeTransfer::default()),
        Arc::new(FakeInstaller::new(Ok(FirmwareInstallOutcome::Installed))),
        notifier.clone(),
        FixedClock(at(0)),
        verifier.clone(),
    )
    .await;

    assert_eq!(*verifier.calls.lock().unwrap(), 0);
    // An unsigned update behaves exactly as it did before B3.3 - no signature statuses appear.
    assert_eq!(
        notifier.statuses(),
        alloc::vec![
            FirmwareStatus::Downloading,
            FirmwareStatus::Downloaded,
            FirmwareStatus::Installing,
            FirmwareStatus::Installed
        ]
    );
}

#[tokio::test]
async fn a_signed_update_with_a_valid_signature_is_verified_then_installed() {
    let actor = actor_with_firmware().await;
    let updates = FirmwareUpdateQueue::new();
    let state = Arc::new(FirmwareUpdateState::new());
    let notifier = Arc::new(RecordingNotifier::default());
    let installer = Arc::new(FakeInstaller::new(Ok(FirmwareInstallOutcome::Installed)));
    let verifier = Arc::new(FakeVerifier::new(Ok(FirmwareVerificationOutcome::Valid)));

    handle_update_firmware(&actor, &updates, &state, signed_request()).await;
    run_with_verifier(
        &actor,
        updates,
        state,
        Arc::new(FakeTransfer::default()),
        installer.clone(),
        notifier.clone(),
        FixedClock(at(0)),
        verifier.clone(),
    )
    .await;

    assert_eq!(*verifier.calls.lock().unwrap(), 1);
    // Verified between `Downloaded` and `Installing` - never after installing.
    assert_eq!(
        notifier.statuses(),
        alloc::vec![
            FirmwareStatus::Downloading,
            FirmwareStatus::Downloaded,
            FirmwareStatus::SignatureVerified,
            FirmwareStatus::Installing,
            FirmwareStatus::Installed
        ]
    );
    assert_eq!(*installer.installs.lock().unwrap(), 1);
}

#[tokio::test]
async fn an_invalid_signature_refuses_to_install_and_raises_the_matching_security_event() {
    let actor = actor_with_firmware().await;
    let mut security_events = actor.subscribe_security_events();
    let updates = FirmwareUpdateQueue::new();
    let state = Arc::new(FirmwareUpdateState::new());
    let notifier = Arc::new(RecordingNotifier::default());
    let installer = Arc::new(FakeInstaller::new(Ok(FirmwareInstallOutcome::Installed)));
    let verifier = Arc::new(FakeVerifier::new(Ok(
        FirmwareVerificationOutcome::InvalidSignature,
    )));

    handle_update_firmware(&actor, &updates, &state, signed_request()).await;
    run_with_verifier(
        &actor,
        updates,
        state,
        Arc::new(FakeTransfer::default()),
        installer.clone(),
        notifier.clone(),
        FixedClock(at(0)),
        verifier,
    )
    .await;

    // Refused before install, never after - nothing reaches the installer.
    assert_eq!(*installer.installs.lock().unwrap(), 0);
    assert_eq!(
        notifier.statuses(),
        alloc::vec![
            FirmwareStatus::Downloading,
            FirmwareStatus::Downloaded,
            FirmwareStatus::InvalidSignature,
        ]
    );
    let event = security_events.recv().await.unwrap();
    assert_eq!(
        event.event_type,
        crate::state::SecurityEventType::InvalidFirmwareSignature
    );
}

#[tokio::test]
async fn an_untrusted_signing_certificate_raises_the_certificate_specific_security_event() {
    let actor = actor_with_firmware().await;
    let mut security_events = actor.subscribe_security_events();
    let updates = FirmwareUpdateQueue::new();
    let state = Arc::new(FirmwareUpdateState::new());
    let notifier = Arc::new(RecordingNotifier::default());
    let installer = Arc::new(FakeInstaller::new(Ok(FirmwareInstallOutcome::Installed)));
    let verifier = Arc::new(FakeVerifier::new(Ok(
        FirmwareVerificationOutcome::InvalidSigningCertificate,
    )));

    handle_update_firmware(&actor, &updates, &state, signed_request()).await;
    run_with_verifier(
        &actor,
        updates,
        state,
        Arc::new(FakeTransfer::default()),
        installer.clone(),
        notifier.clone(),
        FixedClock(at(0)),
        verifier,
    )
    .await;

    assert_eq!(*installer.installs.lock().unwrap(), 0);
    // The wire status is the same `InvalidSignature` OCPP offers either way - only the security
    // event distinguishes "bad signature" from "bad certificate".
    assert_eq!(
        notifier.statuses(),
        alloc::vec![
            FirmwareStatus::Downloading,
            FirmwareStatus::Downloaded,
            FirmwareStatus::InvalidSignature,
        ]
    );
    let event = security_events.recv().await.unwrap();
    assert_eq!(
        event.event_type,
        crate::state::SecurityEventType::InvalidFirmwareSigningCertificate
    );
}

#[tokio::test]
async fn a_signed_update_with_no_verifier_configured_fails_safe_and_refuses_to_install() {
    // The fail-safe default: a charge point that cannot verify at all must not install unchecked.
    let actor = actor_with_firmware().await;
    let mut security_events = actor.subscribe_security_events();
    let updates = FirmwareUpdateQueue::new();
    let state = Arc::new(FirmwareUpdateState::new());
    let notifier = Arc::new(RecordingNotifier::default());
    let installer = Arc::new(FakeInstaller::new(Ok(FirmwareInstallOutcome::Installed)));

    handle_update_firmware(&actor, &updates, &state, signed_request()).await;
    run_with_verifier(
        &actor,
        updates,
        state,
        Arc::new(FakeTransfer::default()),
        installer.clone(),
        notifier.clone(),
        FixedClock(at(0)),
        Arc::new(NoFirmwareVerifier),
    )
    .await;

    assert_eq!(*installer.installs.lock().unwrap(), 0);
    assert_eq!(
        notifier.statuses(),
        alloc::vec![
            FirmwareStatus::Downloading,
            FirmwareStatus::Downloaded,
            FirmwareStatus::InvalidSignature,
        ]
    );
    let event = security_events.recv().await.unwrap();
    assert_eq!(
        event.event_type,
        crate::state::SecurityEventType::InvalidFirmwareSignature
    );
}
