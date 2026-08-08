//! Tests for the Firmware Publishing block's protocol-agnostic state machine (B3.4).

use super::*;
use crate::executor::TokioExecutor;
use crate::hardware::{Capabilities, NoFileTransferError, NoFirmwarePublisherError};
use alloc::sync::Arc;
use alloc::vec::Vec;
use std::sync::Mutex as StdMutex;

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
        _source: crate::hardware::UploadSource<'_>,
        _progress: &TransferProgress<'_>,
    ) -> Result<(), Self::Error> {
        unreachable!("the publish-firmware block never uploads")
    }
}

#[derive(Default)]
struct FakePublisher {
    outcome: StdMutex<Option<Result<Vec<String>, ()>>>,
    published: StdMutex<Vec<String>>,
    unpublished: StdMutex<Vec<String>>,
    unpublish_outcome: StdMutex<Option<Result<bool, ()>>>,
}

impl FakePublisher {
    fn succeeding(locations: Vec<String>) -> Self {
        Self {
            outcome: StdMutex::new(Some(Ok(locations))),
            ..Default::default()
        }
    }

    fn failing() -> Self {
        Self {
            outcome: StdMutex::new(Some(Err(()))),
            ..Default::default()
        }
    }
}

#[async_trait::async_trait]
impl FirmwarePublisher for FakePublisher {
    type Error = NoFirmwarePublisherError;

    async fn publish(&self, checksum: &str) -> Result<Vec<String>, Self::Error> {
        self.published.lock().unwrap().push(checksum.into());
        match self.outcome.lock().unwrap().clone() {
            Some(Ok(locations)) => Ok(locations),
            _ => Err(NoFirmwarePublisherError),
        }
    }

    async fn unpublish(&self, checksum: &str) -> Result<bool, Self::Error> {
        self.unpublished.lock().unwrap().push(checksum.into());
        match *self.unpublish_outcome.lock().unwrap() {
            Some(outcome) => outcome.map_err(|()| NoFirmwarePublisherError),
            None => Ok(true),
        }
    }
}

type NotifierLog = Vec<(i64, PublishFirmwareStatus, Option<Vec<String>>)>;

#[derive(Default)]
struct RecordingNotifier {
    seen: StdMutex<NotifierLog>,
}

impl RecordingNotifier {
    fn statuses(&self) -> Vec<PublishFirmwareStatus> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|(_, status, _)| *status)
            .collect()
    }
}

#[async_trait::async_trait]
impl PublishFirmwareStatusNotifier for RecordingNotifier {
    type Error = core::convert::Infallible;

    async fn notify_publish_firmware_status(
        &self,
        request_id: i64,
        status: PublishFirmwareStatus,
        locations: Option<&[String]>,
    ) -> Result<(), Self::Error> {
        self.seen
            .lock()
            .unwrap()
            .push((request_id, status, locations.map(|l| l.to_vec())));
        Ok(())
    }
}

fn request() -> PublishFirmwareRequest {
    PublishFirmwareRequest {
        request_id: 9,
        location: "https://fw.example/image.bin".into(),
        checksum: "d41d8cd98f00b204e9800998ecf8427e".into(),
        retries: 0,
        retry_interval_secs: 1,
    }
}

async fn actor_with_publishing() -> ChargePointActor {
    let actor = ChargePointActor::spawn([1], &TokioExecutor);
    let _ = actor
        .send(crate::state::ChargePointEvent::CapabilitiesDeclared(
            Capabilities {
                firmware_publishing: true,
                ..Capabilities::default()
            },
        ))
        .await;
    actor
}

async fn settle() {
    for _ in 0..600 {
        tokio::task::yield_now().await;
    }
}

async fn run(
    state: Arc<PublishFirmwareState>,
    publishes: PublishFirmwareQueue,
    transfer: Arc<FakeTransfer>,
    publisher: Arc<FakePublisher>,
    notifier: Arc<RecordingNotifier>,
) {
    let handle = tokio::spawn(async move {
        run_publish_firmware(
            publishes,
            &state,
            &transfer,
            &publisher,
            &notifier,
            &InstantBackoff,
        )
        .await;
    });
    settle().await;
    handle.abort();
}

#[tokio::test]
async fn a_straightforward_publish_walks_downloading_downloaded_published() {
    let actor = actor_with_publishing().await;
    let publishes = PublishFirmwareQueue::new();
    let state = Arc::new(PublishFirmwareState::new());
    let transfer = Arc::new(FakeTransfer::default());
    let publisher = Arc::new(FakePublisher::succeeding(alloc::vec![
        "http://192.0.2.1/fw/d41d8cd98f00b204e9800998ecf8427e".to_string()
    ]));
    let notifier = Arc::new(RecordingNotifier::default());

    let outcome = handle_publish_firmware(&actor, &publishes, &state, request()).await;
    assert_eq!(outcome, PublishFirmwareOutcome::Accepted);

    run(
        state.clone(),
        publishes,
        transfer.clone(),
        publisher.clone(),
        notifier.clone(),
    )
    .await;

    assert_eq!(
        notifier.statuses(),
        alloc::vec![
            PublishFirmwareStatus::Downloading,
            PublishFirmwareStatus::Downloaded,
            PublishFirmwareStatus::Published,
        ]
    );
    assert!(
        notifier
            .seen
            .lock()
            .unwrap()
            .iter()
            .all(|(id, _, _)| *id == 9)
    );
    let (_, _, locations) = notifier.seen.lock().unwrap().last().unwrap().clone();
    assert_eq!(
        locations,
        Some(alloc::vec![
            "http://192.0.2.1/fw/d41d8cd98f00b204e9800998ecf8427e".to_string()
        ])
    );
    assert!(state.is_published("d41d8cd98f00b204e9800998ecf8427e"));
    assert_eq!(
        *transfer.downloads.lock().unwrap(),
        alloc::vec!["https://fw.example/image.bin".to_string()]
    );
}

#[tokio::test]
async fn every_download_attempt_failing_reports_download_failed_and_never_publishes() {
    let actor = actor_with_publishing().await;
    let publishes = PublishFirmwareQueue::new();
    let state = Arc::new(PublishFirmwareState::new());
    let transfer = Arc::new(FakeTransfer {
        failures_remaining: StdMutex::new(u32::MAX),
        ..Default::default()
    });
    let publisher = Arc::new(FakePublisher::default());
    let notifier = Arc::new(RecordingNotifier::default());

    handle_publish_firmware(
        &actor,
        &publishes,
        &state,
        PublishFirmwareRequest {
            retries: 2,
            ..request()
        },
    )
    .await;

    run(
        state.clone(),
        publishes,
        transfer,
        publisher.clone(),
        notifier.clone(),
    )
    .await;

    assert_eq!(
        notifier.statuses(),
        alloc::vec![
            PublishFirmwareStatus::Downloading,
            PublishFirmwareStatus::DownloadFailed
        ]
    );
    assert!(publisher.published.lock().unwrap().is_empty());
    assert!(!state.is_published("d41d8cd98f00b204e9800998ecf8427e"));
}

#[tokio::test]
async fn a_publisher_failure_reports_publish_failed() {
    let actor = actor_with_publishing().await;
    let publishes = PublishFirmwareQueue::new();
    let state = Arc::new(PublishFirmwareState::new());
    let transfer = Arc::new(FakeTransfer::default());
    let publisher = Arc::new(FakePublisher::failing());
    let notifier = Arc::new(RecordingNotifier::default());

    handle_publish_firmware(&actor, &publishes, &state, request()).await;
    run(
        state.clone(),
        publishes,
        transfer,
        publisher,
        notifier.clone(),
    )
    .await;

    assert_eq!(
        notifier.statuses(),
        alloc::vec![
            PublishFirmwareStatus::Downloading,
            PublishFirmwareStatus::Downloaded,
            PublishFirmwareStatus::PublishFailed,
        ]
    );
    assert!(!state.is_published("d41d8cd98f00b204e9800998ecf8427e"));
}

#[tokio::test]
async fn an_empty_location_list_on_success_is_treated_as_a_failure() {
    let actor = actor_with_publishing().await;
    let publishes = PublishFirmwareQueue::new();
    let state = Arc::new(PublishFirmwareState::new());
    let transfer = Arc::new(FakeTransfer::default());
    let publisher = Arc::new(FakePublisher::succeeding(Vec::new()));
    let notifier = Arc::new(RecordingNotifier::default());

    handle_publish_firmware(&actor, &publishes, &state, request()).await;
    run(
        state.clone(),
        publishes,
        transfer,
        publisher,
        notifier.clone(),
    )
    .await;

    assert_eq!(
        notifier.statuses(),
        alloc::vec![
            PublishFirmwareStatus::Downloading,
            PublishFirmwareStatus::Downloaded,
            PublishFirmwareStatus::PublishFailed,
        ]
    );
    assert!(!state.is_published("d41d8cd98f00b204e9800998ecf8427e"));
}

#[tokio::test]
async fn a_second_publish_for_a_different_checksum_supersedes_the_first_download() {
    let actor = actor_with_publishing().await;
    let publishes = PublishFirmwareQueue::new();
    let state = Arc::new(PublishFirmwareState::new());
    let transfer = Arc::new(FakeTransfer::default());
    let publisher = Arc::new(FakePublisher::succeeding(alloc::vec![
        "http://x".to_string()
    ]));
    let notifier = Arc::new(RecordingNotifier::default());

    let first = handle_publish_firmware(&actor, &publishes, &state, request()).await;
    let second = handle_publish_firmware(
        &actor,
        &publishes,
        &state,
        PublishFirmwareRequest {
            request_id: 10,
            checksum: "0cc175b9c0f1b6a831c399e269772661".into(),
            ..request()
        },
    )
    .await;

    assert_eq!(first, PublishFirmwareOutcome::Accepted);
    assert_eq!(second, PublishFirmwareOutcome::Accepted);

    run(
        state.clone(),
        publishes,
        transfer,
        publisher,
        notifier.clone(),
    )
    .await;

    // Only the surviving publish reports anything.
    assert!(
        notifier
            .seen
            .lock()
            .unwrap()
            .iter()
            .all(|(id, _, _)| *id == 10)
    );
}

#[tokio::test]
async fn a_charge_point_without_firmware_publishing_rejects() {
    let actor = ChargePointActor::spawn([1], &TokioExecutor);
    let _ = actor
        .send(crate::state::ChargePointEvent::CapabilitiesDeclared(
            Capabilities::default(),
        ))
        .await;

    let outcome = handle_publish_firmware(
        &actor,
        &PublishFirmwareQueue::new(),
        &PublishFirmwareState::new(),
        request(),
    )
    .await;

    assert_eq!(outcome, PublishFirmwareOutcome::Rejected);
}

#[tokio::test]
async fn a_charge_point_without_firmware_publishing_answers_unpublish_with_no_firmware() {
    let actor = ChargePointActor::spawn([1], &TokioExecutor);
    let _ = actor
        .send(crate::state::ChargePointEvent::CapabilitiesDeclared(
            Capabilities::default(),
        ))
        .await;
    let publisher = FakePublisher::default();

    let outcome = handle_unpublish_firmware(
        &actor,
        &publisher,
        &PublishFirmwareState::new(),
        "d41d8cd98f00b204e9800998ecf8427e",
    )
    .await;

    assert_eq!(outcome, UnpublishFirmwareOutcome::NoFirmware);
    // Never even asked - there is nothing this charge point could ever have published.
    assert!(publisher.unpublished.lock().unwrap().is_empty());
}

#[tokio::test]
async fn unpublish_reports_no_firmware_for_an_unknown_checksum() {
    let actor = actor_with_publishing().await;
    let publisher = FakePublisher::default();
    let state = PublishFirmwareState::new();

    let outcome = handle_unpublish_firmware(
        &actor,
        &publisher,
        &state,
        "0000000000000000000000000000000",
    )
    .await;

    assert_eq!(outcome, UnpublishFirmwareOutcome::NoFirmware);
}

#[tokio::test]
async fn unpublish_reports_download_ongoing_while_a_publish_is_in_flight() {
    let actor = actor_with_publishing().await;
    let publishes = PublishFirmwareQueue::new();
    let state = Arc::new(PublishFirmwareState::new());
    let publisher = FakePublisher::default();

    // Accepted but never run to completion - the download is still "in flight" as far as the
    // state is concerned.
    handle_publish_firmware(&actor, &publishes, &state, request()).await;

    let outcome = handle_unpublish_firmware(
        &actor,
        &publisher,
        &state,
        "d41d8cd98f00b204e9800998ecf8427e",
    )
    .await;

    assert_eq!(outcome, UnpublishFirmwareOutcome::DownloadOngoing);
}

#[tokio::test]
async fn unpublish_takes_a_published_image_down() {
    let actor = actor_with_publishing().await;
    let publishes = PublishFirmwareQueue::new();
    let state = Arc::new(PublishFirmwareState::new());
    let transfer = Arc::new(FakeTransfer::default());
    let publisher = Arc::new(FakePublisher::succeeding(alloc::vec![
        "http://x".to_string()
    ]));
    let notifier = Arc::new(RecordingNotifier::default());

    handle_publish_firmware(&actor, &publishes, &state, request()).await;
    run(
        state.clone(),
        publishes,
        transfer,
        publisher.clone(),
        notifier,
    )
    .await;
    assert!(state.is_published("d41d8cd98f00b204e9800998ecf8427e"));

    let outcome = handle_unpublish_firmware(
        &actor,
        &*publisher,
        &state,
        "d41d8cd98f00b204e9800998ecf8427e",
    )
    .await;

    assert_eq!(outcome, UnpublishFirmwareOutcome::Unpublished);
    assert!(!state.is_published("d41d8cd98f00b204e9800998ecf8427e"));
    assert_eq!(
        *publisher.unpublished.lock().unwrap(),
        alloc::vec!["d41d8cd98f00b204e9800998ecf8427e".to_string()]
    );
}

#[tokio::test]
async fn a_new_checksum_is_rejected_once_the_published_set_is_full() {
    let actor = actor_with_publishing().await;
    let publishes = PublishFirmwareQueue::new();
    let state = Arc::new(PublishFirmwareState::with_max_published(1));
    let transfer = Arc::new(FakeTransfer::default());
    let publisher = Arc::new(FakePublisher::succeeding(alloc::vec![
        "http://x".to_string()
    ]));
    let notifier = Arc::new(RecordingNotifier::default());

    handle_publish_firmware(&actor, &publishes, &state, request()).await;
    run(
        state.clone(),
        publishes.clone(),
        transfer,
        publisher,
        notifier,
    )
    .await;
    assert!(state.is_published("d41d8cd98f00b204e9800998ecf8427e"));

    let second = handle_publish_firmware(
        &actor,
        &publishes,
        &state,
        PublishFirmwareRequest {
            request_id: 11,
            checksum: "0cc175b9c0f1b6a831c399e269772661".into(),
            ..request()
        },
    )
    .await;

    assert_eq!(second, PublishFirmwareOutcome::Rejected);

    // Re-publishing the checksum already tracked is still allowed - it does not grow the set.
    let republish = handle_publish_firmware(&actor, &publishes, &state, request()).await;
    assert_eq!(republish, PublishFirmwareOutcome::Accepted);
}
