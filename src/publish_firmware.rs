//! Local-controller Firmware Publishing functional block (`docs/PRODUCTION-ROADMAP.md` B3.4):
//! `PublishFirmware`, `UnpublishFirmware`, `PublishFirmwareStatusNotification`. **2.x only** -
//! 1.6J predates the local-controller concept entirely.
//!
//! # What a local controller is
//!
//! Some OCPP deployments put one charge point between the CSMS and several others - a "local
//! controller" - so that firmware only has to travel the (possibly slow, possibly metered) link to
//! the CSMS once. The CSMS sends this charge point a `PublishFirmware`; instead of installing the
//! image, this charge point downloads it and re-serves it on the local network, so its downstream
//! neighbours can fetch it from something on their own LAN instead of each hitting the CSMS.
//! `UnpublishFirmware` takes a publication back down, and `PublishFirmwareStatusNotification`
//! reports progress the same way `FirmwareStatusNotification` does for [`crate::firmware`].
//!
//! # Relationship to `crate::firmware`
//!
//! The download half is identical in spirit to [`crate::firmware`]'s: the same
//! [`crate::hardware::FileTransfer::download`] path, honouring the same
//! `retries`/`retryInterval` the CSMS asks for (see `download_with_retries`, private to this
//! module). It is not the same
//! *code path*, because `PublishFirmwareRequest` is a materially smaller message than
//! `UpdateFirmwareRequest` - it has no `retrieveDateTime`/`installDateTime` to schedule around, so
//! there is no `DownloadScheduled` wait and no availability-holding dance. What is new here is the
//! step after the download succeeds: [`crate::hardware::FirmwarePublisher::publish`], serving the
//! image rather than installing it. See that trait's module docs for why publishing is hardware
//! surface and why checksum verification lives there rather than in this crate.
//!
//! # At most one publish in flight, several published at once
//!
//! Like [`crate::firmware::FirmwareUpdateState`], only one `PublishFirmware` is ever downloading
//! at a time - a second accepted request supersedes whatever the first was doing, the same
//! L01.FR.24-style behaviour `UpdateFirmware` has (though `PublishFirmwareResponse`'s
//! `GenericStatusEnum` has no `AcceptedCanceled` to say so on the wire; a supersede is silently
//! accepted). Once *published*, though, an image stays published independently of whatever is
//! downloading next - a local controller plausibly serves firmware for more than one charging
//! station model at once. [`PublishFirmwareState`] tracks a bounded set of currently-published
//! checksums (see [`PublishFirmwareState::with_max_published`]) so a CSMS cannot grow this
//! charge point's bookkeeping without limit; a new, distinct checksum is refused outright with
//! `Rejected` once the bound is reached, rather than silently evicting a checksum this crate
//! believes is still being served (evicting the record while the file is still being served would
//! leave `UnpublishFirmware` unable to find it again).

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;
use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;

use crate::actor::ChargePointActor;
use crate::hardware::{FileTransfer, FirmwarePublisher, TransferProgress};
use crate::provisioning::Backoff;
use crate::sync::Chan;

/// Download attempts when the CSMS names no `retries` - see
/// [`crate::firmware::DEFAULT_RETRIES`], which this mirrors.
pub const DEFAULT_RETRIES: u32 = 0;

/// Seconds between download attempts when the CSMS names no `retryInterval` - see
/// [`crate::firmware::DEFAULT_RETRY_INTERVAL_SECS`], which this mirrors.
pub const DEFAULT_RETRY_INTERVAL_SECS: u32 = 30;

/// The most distinct checksums [`PublishFirmwareState`] tracks as published at once, unless
/// overridden with [`PublishFirmwareState::with_max_published`]. Four is enough for a local
/// controller fronting a handful of charge point models simultaneously while keeping the bound
/// small and predictable on constrained hardware.
pub const DEFAULT_MAX_PUBLISHED: usize = 4;

/// A CSMS request to publish a firmware image, protocol-independent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishFirmwareRequest {
    /// Correlates every `PublishFirmwareStatusNotification` with the request that started it.
    /// Unlike [`crate::firmware::FirmwareUpdateRequest::request_id`], this is mandatory on the
    /// wire in both 2.0.1 and 2.1 - `PublishFirmware` has no 1.6J form to be optional for.
    pub request_id: i64,
    /// Where to fetch the image from.
    pub location: String,
    /// The MD5 checksum (32 lowercase hex characters) the CSMS asked to have published, and the
    /// identifier `UnpublishFirmware` will later use to take it back down.
    pub checksum: String,
    /// How many times to retry a failed download, beyond the first attempt.
    pub retries: u32,
    /// How long to wait between download attempts, in seconds.
    pub retry_interval_secs: u32,
}

/// What a charge point answers a `PublishFirmware` with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishFirmwareOutcome {
    /// The request was accepted and queued.
    Accepted,
    /// Refused - either firmware publishing is not available on this charge point, the request
    /// was malformed, or this charge point already tracks
    /// [`PublishFirmwareState::max_published`] other checksums as published.
    Rejected,
}

/// Every state a firmware publication passes through, mapped onto OCPP's
/// `PublishFirmwareStatusEnum` - identical in both 2.0.1 and 2.1.
///
/// A strict superset of what this crate emits today: `DownloadScheduled` is never reported
/// (`PublishFirmwareRequest` has no `retrieveDateTime` to schedule around, unlike
/// `UpdateFirmware`), `DownloadPaused` is never reported (no pause/resume support), and
/// `InvalidChecksum`/`ChecksumVerified` are never reported (this crate has no crypto - see
/// [`crate::hardware::FirmwarePublisher`]'s module docs on why checksum verification is the
/// integrator's, not this crate's). They stay in this enum so a future extension has a value to
/// report through rather than a breaking change to add one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishFirmwareStatus {
    /// Nothing in progress.
    Idle,
    /// Never reported today - see this enum's docs.
    DownloadScheduled,
    /// Fetching the image.
    Downloading,
    /// The image is fetched.
    Downloaded,
    /// The image is now being served on the local network.
    Published,
    /// Every download attempt failed.
    DownloadFailed,
    /// Never reported today - see this enum's docs.
    DownloadPaused,
    /// Never reported today - see this enum's docs.
    InvalidChecksum,
    /// Never reported today - see this enum's docs.
    ChecksumVerified,
    /// The image downloaded, but [`FirmwarePublisher::publish`] failed - a full disk, a bound
    /// port, or (per that trait's docs) a checksum the integrator chose to verify and reject.
    PublishFailed,
}

/// What a charge point answers an `UnpublishFirmware` with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnpublishFirmwareOutcome {
    /// The image was published and is now taken down.
    Unpublished,
    /// `checksum` is not currently published by this charge point - either it never was, it
    /// already has been unpublished, or (per C5) this charge point has no publishing capability
    /// at all, in which case nothing it has ever "published" is the simple truth rather than a
    /// synthetic refusal.
    NoFirmware,
    /// A `PublishFirmware` download for `checksum` is still in flight - nothing has been served
    /// yet for `UnpublishFirmware` to take down.
    DownloadOngoing,
}

/// Reports a firmware publication's progress to the CSMS.
#[async_trait::async_trait]
pub trait PublishFirmwareStatusNotifier {
    /// What went wrong reporting the status.
    type Error: core::fmt::Display;

    /// Reports `status` for the publish started by `request_id`. `locations` is `Some` exactly
    /// when `status` is [`PublishFirmwareStatus::Published`] - OCPP requires it there and nowhere
    /// else.
    async fn notify_publish_firmware_status(
        &self,
        request_id: i64,
        status: PublishFirmwareStatus,
        locations: Option<&[String]>,
    ) -> Result<(), Self::Error>;
}

#[async_trait::async_trait]
impl<T: PublishFirmwareStatusNotifier + Send + Sync + ?Sized> PublishFirmwareStatusNotifier
    for Arc<T>
{
    type Error = T::Error;

    async fn notify_publish_firmware_status(
        &self,
        request_id: i64,
        status: PublishFirmwareStatus,
        locations: Option<&[String]>,
    ) -> Result<(), Self::Error> {
        (**self)
            .notify_publish_firmware_status(request_id, status, locations)
            .await
    }
}

/// Registers this charge point's inbound `PublishFirmware`/`UnpublishFirmware` handling.
///
/// One trait for both messages, like [`crate::certificates::CertificateHandler`]: they share
/// [`PublishFirmwareState`] and a `hardware::FirmwarePublisher`, and a CSMS client that can answer
/// one can answer the other.
#[async_trait::async_trait]
pub trait PublishFirmwareHandler {
    /// Registers `PublishFirmware` (queued onto `publishes` for
    /// [`run_publish_firmware`] to work) and `UnpublishFirmware` (answered directly against
    /// `publisher` and `state`) dispatching against `actor`.
    async fn register_publish_firmware_handlers<F>(
        &self,
        actor: ChargePointActor,
        publishes: PublishFirmwareQueue,
        publisher: F,
        state: Arc<PublishFirmwareState>,
    ) where
        F: FirmwarePublisher + Send + Sync + 'static;
}

struct PublishFirmwareInner {
    current: u64,
    /// The ticket and checksum of whatever is downloading/publishing right now, cleared once it
    /// reaches a terminal state (published or failed) or is superseded.
    in_progress: Option<(u64, String)>,
    last_status: PublishFirmwareStatus,
    last_request_id: Option<i64>,
    /// Checksums this crate believes are currently published - see the module docs for why this
    /// is bounded rather than growing without limit.
    published: Vec<String>,
    max_published: usize,
}

/// Tracks which publish is current, what was last reported, and which checksums are currently
/// published - see the module docs for why publishing (unlike downloading) is not "at most one at
/// a time".
pub struct PublishFirmwareState {
    inner: BlockingMutex<CriticalSectionRawMutex, RefCell<PublishFirmwareInner>>,
}

impl Default for PublishFirmwareState {
    fn default() -> Self {
        Self::with_max_published(DEFAULT_MAX_PUBLISHED)
    }
}

impl PublishFirmwareState {
    /// A state with nothing published and no publish in progress, bounded at
    /// [`DEFAULT_MAX_PUBLISHED`].
    pub fn new() -> Self {
        Self::default()
    }

    /// A state that tracks at most `max_published` published checksums at once (clamped to at
    /// least 1 - a bound of zero would refuse every `PublishFirmware`, indistinguishable from not
    /// supporting the block at all; an integrator who wants that should leave
    /// [`crate::hardware::Capabilities::firmware_publishing`] unset instead).
    pub fn with_max_published(max_published: usize) -> Self {
        Self {
            inner: BlockingMutex::new(RefCell::new(PublishFirmwareInner {
                current: 0,
                in_progress: None,
                last_status: PublishFirmwareStatus::Idle,
                last_request_id: None,
                published: Vec::new(),
                max_published: max_published.max(1),
            })),
        }
    }

    /// How many distinct checksums are currently tracked as published.
    pub fn published_count(&self) -> usize {
        self.inner.lock(|inner| inner.borrow().published.len())
    }

    /// The most checksums this state will ever track as published at once.
    pub fn max_published(&self) -> usize {
        self.inner.lock(|inner| inner.borrow().max_published)
    }

    /// Whether `checksum` is currently tracked as published.
    pub fn is_published(&self, checksum: &str) -> bool {
        self.inner
            .lock(|inner| inner.borrow().published.iter().any(|c| c == checksum))
    }

    /// Whether a download/publish for `checksum` is currently in flight and has not yet reached
    /// `Published` - the case `UnpublishFirmware` must answer `DownloadOngoing` for.
    pub fn is_download_ongoing(&self, checksum: &str) -> bool {
        self.inner.lock(|inner| {
            inner
                .borrow()
                .in_progress
                .as_ref()
                .is_some_and(|(_, current)| current == checksum)
        })
    }

    fn claim(&self, checksum: &str) -> u64 {
        self.inner.lock(|inner| {
            let mut inner = inner.borrow_mut();
            inner.current += 1;
            inner.in_progress = Some((inner.current, checksum.into()));
            inner.current
        })
    }

    fn is_current(&self, ticket: u64) -> bool {
        self.inner.lock(|inner| {
            inner
                .borrow()
                .in_progress
                .as_ref()
                .is_some_and(|(current, _)| *current == ticket)
        })
    }

    fn record(&self, request_id: i64, status: PublishFirmwareStatus) {
        self.inner.lock(|inner| {
            let mut inner = inner.borrow_mut();
            inner.last_status = status;
            inner.last_request_id = Some(request_id);
            if matches!(
                status,
                PublishFirmwareStatus::Published | PublishFirmwareStatus::PublishFailed
            ) {
                inner.in_progress = None;
            }
        });
    }

    fn stop_current(&self, ticket: u64) {
        self.inner.lock(|inner| {
            let mut inner = inner.borrow_mut();
            if inner
                .in_progress
                .as_ref()
                .is_some_and(|(current, _)| *current == ticket)
            {
                inner.in_progress = None;
            }
        });
    }

    fn mark_published(&self, checksum: &str) {
        self.inner.lock(|inner| {
            let mut inner = inner.borrow_mut();
            if !inner.published.iter().any(|c| c == checksum) {
                inner.published.push(checksum.into());
            }
            inner.in_progress = None;
        });
    }

    /// Stops tracking `checksum` as published. Returns whether it was tracked at all.
    fn mark_unpublished(&self, checksum: &str) -> bool {
        self.inner.lock(|inner| {
            let mut inner = inner.borrow_mut();
            let before = inner.published.len();
            inner.published.retain(|c| c != checksum);
            inner.published.len() != before
        })
    }
}

/// The queue a handler hands accepted `PublishFirmware` requests to [`run_publish_firmware`]
/// through.
#[derive(Clone)]
pub struct PublishFirmwareQueue {
    channel: Chan<(u64, PublishFirmwareRequest)>,
}

impl PublishFirmwareQueue {
    /// A new, empty queue.
    pub fn new() -> Self {
        Self {
            channel: Chan::new(),
        }
    }
}

impl Default for PublishFirmwareQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// Decides what to answer a `PublishFirmware` with, and queues the publish if accepted.
pub async fn handle_publish_firmware(
    actor: &ChargePointActor,
    publishes: &PublishFirmwareQueue,
    state: &PublishFirmwareState,
    request: PublishFirmwareRequest,
) -> PublishFirmwareOutcome {
    if !crate::refusal::capability_present(&actor.state().capabilities, "PublishFirmware") {
        return PublishFirmwareOutcome::Rejected;
    }
    if request.location.is_empty() || request.checksum.is_empty() {
        return PublishFirmwareOutcome::Rejected;
    }
    // The module docs' bound: refuse a genuinely new checksum once the tracked set is full,
    // rather than silently forgetting one that may still be served. Re-publishing a checksum
    // already tracked (or already in flight) is always allowed - it does not grow the set.
    if !state.is_published(&request.checksum)
        && !state.is_download_ongoing(&request.checksum)
        && state.published_count() >= state.max_published()
    {
        return PublishFirmwareOutcome::Rejected;
    }

    let ticket = state.claim(&request.checksum);
    publishes.channel.send((ticket, request));
    PublishFirmwareOutcome::Accepted
}

/// Decides what to answer an `UnpublishFirmware` with, and takes the publication down against
/// `publisher` if one exists.
pub async fn handle_unpublish_firmware<F: FirmwarePublisher>(
    actor: &ChargePointActor,
    publisher: &F,
    state: &PublishFirmwareState,
    checksum: &str,
) -> UnpublishFirmwareOutcome {
    if !crate::refusal::capability_present(&actor.state().capabilities, "UnpublishFirmware") {
        // Not a synthetic refusal (C5's `RefusalShape::CallResultStatus` doesn't require one
        // specific variant, only that the answer come through the status field) - a charge point
        // with no publishing capability genuinely has never published anything, so `NoFirmware`
        // is simply true.
        return UnpublishFirmwareOutcome::NoFirmware;
    }
    if state.is_download_ongoing(checksum) {
        return UnpublishFirmwareOutcome::DownloadOngoing;
    }
    if !state.is_published(checksum) {
        return UnpublishFirmwareOutcome::NoFirmware;
    }

    match publisher.unpublish(checksum).await {
        Ok(true) => {
            state.mark_unpublished(checksum);
            UnpublishFirmwareOutcome::Unpublished
        }
        Ok(false) => {
            // A divergence between this crate's bookkeeping and the server's real state (see
            // `FirmwarePublisher::unpublish`'s docs) - stop believing we're serving it either way.
            state.mark_unpublished(checksum);
            UnpublishFirmwareOutcome::NoFirmware
        }
        Err(err) => {
            // OCPP's `UnpublishFirmwareStatusEnum` has no "try again" value - `DownloadOngoing`
            // is the closest honest answer available: the image is not (yet, from the CSMS's
            // point of view) taken down, and a CSMS seeing it is expected to retry later, which
            // is exactly what should happen here. The bookkeeping is left untouched, because the
            // call failing gives no evidence the image stopped being served.
            tracing::warn!(error = %err, "failed to unpublish a firmware image");
            UnpublishFirmwareOutcome::DownloadOngoing
        }
    }
}

/// Performs every accepted `PublishFirmware`, reporting each state change to the CSMS.
pub async fn run_publish_firmware<T, F, N, B>(
    publishes: PublishFirmwareQueue,
    state: &PublishFirmwareState,
    transfer: &T,
    publisher: &F,
    notifier: &N,
    backoff: &B,
) where
    T: FileTransfer,
    F: FirmwarePublisher,
    N: PublishFirmwareStatusNotifier,
    B: Backoff,
{
    loop {
        let (ticket, request) = publishes.channel.recv().await;
        if !state.is_current(ticket) {
            continue;
        }
        run_one_publish(
            state, ticket, transfer, publisher, notifier, backoff, &request,
        )
        .await;
    }
}

async fn run_one_publish<T, F, N, B>(
    state: &PublishFirmwareState,
    ticket: u64,
    transfer: &T,
    publisher: &F,
    notifier: &N,
    backoff: &B,
    request: &PublishFirmwareRequest,
) where
    T: FileTransfer,
    F: FirmwarePublisher,
    N: PublishFirmwareStatusNotifier,
    B: Backoff,
{
    let report = async |status: PublishFirmwareStatus, locations: Option<&[String]>| {
        state.record(request.request_id, status);
        if let Err(err) = notifier
            .notify_publish_firmware_status(request.request_id, status, locations)
            .await
        {
            tracing::warn!(error = %err, ?status, "failed to report a publish-firmware status");
        }
    };

    report(PublishFirmwareStatus::Downloading, None).await;
    if !download_with_retries(state, ticket, transfer, backoff, request).await {
        if state.is_current(ticket) {
            report(PublishFirmwareStatus::DownloadFailed, None).await;
        }
        return;
    }
    if !state.is_current(ticket) {
        return;
    }
    report(PublishFirmwareStatus::Downloaded, None).await;

    match publisher.publish(&request.checksum).await {
        Ok(locations) if !locations.is_empty() => {
            state.mark_published(&request.checksum);
            report(PublishFirmwareStatus::Published, Some(&locations)).await;
        }
        Ok(_) => {
            // An empty location list on success is a contract violation by the integrator's
            // `FirmwarePublisher` (see its docs) - treated the same as a real failure rather than
            // reported `Published` with nothing for a downstream station to fetch.
            tracing::warn!(
                "firmware publisher reported success but returned no locations; treating as a \
                 failure"
            );
            state.stop_current(ticket);
            report(PublishFirmwareStatus::PublishFailed, None).await;
        }
        Err(err) => {
            tracing::warn!(error = %err, "publishing a downloaded firmware image failed");
            state.stop_current(ticket);
            report(PublishFirmwareStatus::PublishFailed, None).await;
        }
    }
}

/// Downloads the image, retrying as the CSMS asked - the same shape as
/// [`crate::firmware`]'s own `download_with_retries`, duplicated rather than shared because the
/// two operate on different request/state types.
async fn download_with_retries<T: FileTransfer, B: Backoff>(
    state: &PublishFirmwareState,
    ticket: u64,
    transfer: &T,
    backoff: &B,
    request: &PublishFirmwareRequest,
) -> bool {
    for attempt in 0..=request.retries {
        if attempt > 0 {
            backoff.wait(request.retry_interval_secs).await;
            if !state.is_current(ticket) {
                return false;
            }
        }
        match transfer
            .download(&request.location, &TransferProgress::ignored())
            .await
        {
            Ok(()) => return true,
            Err(err) => tracing::warn!(
                error = %err,
                attempt = attempt + 1,
                attempts = request.retries + 1,
                // Never the URL: a firmware location may embed credentials - see
                // `crate::firmware::download_with_retries`'s identical reasoning.
                "a firmware download attempt failed"
            ),
        }
    }
    false
}

#[cfg(feature = "ocpp_2_0_1")]
mod ocpp_2_0_1;
#[cfg(feature = "ocpp_2_1")]
mod ocpp_2_1;

#[cfg(feature = "ocpp_2_0_1")]
pub use self::ocpp_2_0_1::Ocpp2_0_1PublishFirmwareHandler;
#[cfg(feature = "ocpp_2_1")]
pub use self::ocpp_2_1::Ocpp2_1PublishFirmwareHandler;

#[cfg(test)]
mod tests;
