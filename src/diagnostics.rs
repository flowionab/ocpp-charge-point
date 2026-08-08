//! Diagnostics functional block: uploading a log file where the CSMS asks for it
//! (`docs/ROADMAP.md` §14, `docs/PRODUCTION-ROADMAP.md` B5.1). OCPP's N01, plus 1.6J's
//! `GetDiagnostics`, which is the same flow with a narrower log-type set.
//!
//! # Why the upload does not happen in the handler
//!
//! N01's sequence is: respond `Accepted` **first**, then report `Uploading`, then upload, then
//! report `Uploaded`. A handler that did the transfer before returning would hold the response
//! until a multi-megabyte upload over a slow link finished - by which time the CSMS has long since
//! timed out, and the status notifications the sequence asks for would arrive after the very
//! response they are supposed to follow.
//!
//! So the handler does only what it can do immediately - decide the outcome, name the file - and
//! hands the work to [`run_log_uploads`], a worker task the builder spawns. That is the same actor
//! discipline the rest of this crate follows: the decision is synchronous and the work is a
//! message.
//!
//! # What "cancel" can and cannot mean here
//!
//! N01.FR.12 says a `GetLog` arriving while one is in flight SHOULD cancel the ongoing upload and
//! answer `AcceptedCanceled`. This crate answers `AcceptedCanceled` and *supersedes* the running
//! upload - its result is discarded rather than reported, so the CSMS never sees a stale
//! `Uploaded` for a request it replaced - but it does not abort the transfer mid-flight. Aborting
//! would mean racing the transfer future against a cancellation signal, and this crate has no
//! `select!` that works on both `no_std` and std (the same constraint
//! [`crate::smart_charging::run_charging_limit_projection`] documents). The supersede is checked
//! between retry attempts, so a cancelled upload stops at the next attempt boundary rather than
//! immediately.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use chrono::{DateTime, Utc};
use core::cell::RefCell;
use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;

use crate::actor::ChargePointActor;
use crate::clock::Clock;
use crate::hardware::{FileTransfer, LogKind, TransferProgress, UploadSource};
use crate::provisioning::Backoff;
use crate::security::SecurityEventLog;
use crate::sync::Chan;

/// How many upload attempts to make when the CSMS names no `retries`.
///
/// One attempt, no retry. OCPP leaves the default open, and a charge point that invented extra
/// attempts would be spending an operator's bandwidth on a policy they did not choose - where a
/// CSMS that wants persistence has an explicit field to ask for it with.
pub const DEFAULT_RETRIES: u32 = 0;

/// How long to wait between attempts when the CSMS names no `retryInterval`, in seconds.
pub const DEFAULT_RETRY_INTERVAL_SECS: u32 = 30;

/// A CSMS request to upload one log file, protocol-independent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogUploadRequest {
    /// Correlates every status notification for this upload with the request that started it
    /// (N01.FR.07). 1.6J's `GetDiagnostics` carries no request id, so its adapter passes `None`
    /// and its `DiagnosticsStatusNotification` has no field to put one in either.
    pub request_id: Option<i64>,
    /// Which log to upload.
    pub log_kind: LogKind,
    /// Where to send it. Carries its own scheme, and may embed credentials
    /// (`ftp://user:password@host/path`) - which is why it is never logged in full.
    pub remote_location: String,
    /// Only include entries at or after this instant, if the CSMS narrowed the range.
    pub oldest: Option<DateTime<Utc>>,
    /// Only include entries at or before this instant, if the CSMS narrowed the range.
    pub latest: Option<DateTime<Utc>>,
    /// How many times to retry a failed upload, beyond the first attempt.
    pub retries: u32,
    /// How long to wait between attempts, in seconds.
    pub retry_interval_secs: u32,
}

/// What a charge point answers a `GetLog`/`GetDiagnostics` with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GetLogOutcome {
    /// The upload was accepted and started.
    Accepted {
        /// The file name the CSMS should expect, which OCPP asks for in the response
        /// (N01.FR.01).
        filename: String,
    },
    /// Accepted, and an upload that was already running has been superseded (N01.FR.12). 1.6J has
    /// no such status and its adapter reports this as a plain accept.
    AcceptedCanceled {
        /// The file name the CSMS should expect for the *new* upload.
        filename: String,
    },
    /// The requested log is not available on this charge point (N01.FR.05) - because it has no
    /// file-transfer capability, or because nothing this crate can produce matches the log type
    /// asked for.
    Rejected,
}

impl GetLogOutcome {
    /// The file name this outcome names, if it names one.
    pub fn filename(&self) -> Option<&str> {
        match self {
            Self::Accepted { filename } | Self::AcceptedCanceled { filename } => Some(filename),
            Self::Rejected => None,
        }
    }
}

/// How a log upload ended, mirroring OCPP's `UploadLogStatusEnum` - the subset a charge point can
/// actually distinguish from its own side of a transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogUploadStatus {
    /// The upload has started (N01.FR.08).
    Uploading,
    /// The upload completed (N01.FR.09).
    Uploaded,
    /// Every attempt failed (N01.FR.10). Reported after the retries are exhausted rather than
    /// after each one, which is what OCPP recommends: a CSMS wants to know the upload failed, not
    /// that an attempt did.
    UploadFailure,
}

/// Renders `entries` as the log file body for a security-log upload.
///
/// **OCPP does not prescribe a format** (N01, "The format of this log file is not prescribed"), so
/// this is a decision rather than a standard: one line per event, tab-separated, oldest first, as
/// `timestamp<TAB>type<TAB>techInfo`. Chosen because it is greppable by whoever is holding the
/// incident, needs no parser on the receiving end, and costs no dependency on a device that may
/// have kilobytes of RAM.
///
/// An entry recorded while the clock was unsynchronized has no timestamp to print, and renders as
/// `-` rather than a fabricated one or the epoch - see
/// [`SecurityLogEntry::recorded_at`](crate::security::SecurityLogEntry::recorded_at). A missing
/// `techInfo` renders as an empty final field, so every line has the same shape.
pub fn render_security_log(entries: &[crate::security::SecurityLogEntry]) -> alloc::vec::Vec<u8> {
    let mut rendered = String::new();
    for entry in entries {
        let timestamp = entry
            .recorded_at
            .map_or_else(|| "-".to_string(), |at| at.to_rfc3339());
        // Tabs and newlines inside free-form `techInfo` would forge a column or a whole line, so
        // they are replaced rather than escaped - this is a log to read, not a format to parse
        // back, and a reader should never be misled about how many events occurred.
        let tech_info = entry
            .event
            .tech_info
            .as_deref()
            .unwrap_or_default()
            .replace(['\t', '\n', '\r'], " ");
        rendered.push_str(&format!(
            "{timestamp}\t{:?}\t{tech_info}\n",
            entry.event.event_type
        ));
    }
    rendered.into_bytes()
}

/// Names the file an upload will produce, which OCPP returns in the response so an operator can
/// find it at the far end.
///
/// `<kind>-<timestamp>.log`, with the timestamp in a filename-safe basic ISO form. Named from the
/// charge point's clock rather than a counter so two uploads never collide at a destination that
/// is holding files from a whole fleet.
fn log_filename<C: Clock>(clock: &C, log_kind: LogKind) -> String {
    let kind = match log_kind {
        LogKind::Security => "security",
        LogKind::Diagnostics => "diagnostics",
        LogKind::DataCollector => "datacollector",
    };
    format!("{kind}-{}.log", clock.now().format("%Y%m%dT%H%M%SZ"))
}

/// Tracks which upload is current, so a superseded one can be recognised and dropped.
///
/// A monotonically increasing ticket rather than a boolean: two `GetLog`s in quick succession must
/// leave exactly the last one reporting, and a flag cannot tell "someone replaced me" from
/// "someone replaced the one before me".
#[derive(Debug)]
pub struct LogUploadState {
    inner: BlockingMutex<CriticalSectionRawMutex, RefCell<LogUploadInner>>,
}

impl Default for LogUploadState {
    fn default() -> Self {
        Self {
            inner: BlockingMutex::new(RefCell::new(LogUploadInner::default())),
        }
    }
}

#[derive(Debug, Default)]
struct LogUploadInner {
    /// The ticket of the most recently accepted upload.
    current: u64,
    /// Whether an upload is running now - what N01.FR.12's `AcceptedCanceled` is about.
    in_flight: bool,
}

impl LogUploadState {
    /// A state with no upload running.
    pub fn new() -> Self {
        Self::default()
    }

    /// Claims the next ticket for a newly accepted upload, returning it and whether it superseded
    /// one that was already running.
    fn claim(&self) -> (u64, bool) {
        self.inner.lock(|inner| {
            let mut inner = inner.borrow_mut();
            let superseded = inner.in_flight;
            inner.current += 1;
            inner.in_flight = true;
            (inner.current, superseded)
        })
    }

    /// Whether `ticket` is still the current upload.
    fn is_current(&self, ticket: u64) -> bool {
        self.inner.lock(|inner| inner.borrow().current == ticket)
    }

    /// Marks `ticket` finished, if it is still the current one - a superseded upload must not
    /// clear the flag its replacement is relying on.
    fn finish(&self, ticket: u64) {
        self.inner.lock(|inner| {
            let mut inner = inner.borrow_mut();
            if inner.current == ticket {
                inner.in_flight = false;
            }
        });
    }
}

/// The queue a handler hands accepted uploads to [`run_log_uploads`] through.
#[derive(Clone)]
pub struct LogUploadQueue {
    channel: Chan<(u64, LogUploadRequest)>,
}

impl LogUploadQueue {
    /// A new, empty queue.
    pub fn new() -> Self {
        Self {
            channel: Chan::new(),
        }
    }
}

impl Default for LogUploadQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// Reports a log upload's progress to the CSMS - OCPP 2.x's `LogStatusNotification`, 1.6J's
/// `DiagnosticsStatusNotification`.
#[async_trait::async_trait]
pub trait LogStatusNotifier {
    /// What went wrong reporting the status.
    type Error: core::fmt::Display;

    /// Reports `status` for the upload started by `request_id` (N01.FR.07).
    async fn notify_log_status(
        &self,
        request_id: Option<i64>,
        status: LogUploadStatus,
    ) -> Result<(), Self::Error>;
}

#[async_trait::async_trait]
impl<T: LogStatusNotifier + Send + Sync + ?Sized> LogStatusNotifier for alloc::sync::Arc<T> {
    type Error = T::Error;

    async fn notify_log_status(
        &self,
        request_id: Option<i64>,
        status: LogUploadStatus,
    ) -> Result<(), Self::Error> {
        (**self).notify_log_status(request_id, status).await
    }
}

/// Registers this charge point's inbound `GetLog` (2.x) / `GetDiagnostics` (1.6J) handling.
#[async_trait::async_trait]
pub trait GetLogHandler {
    /// Registers a handler dispatching against `actor`.
    async fn register_get_log_handler(
        &self,
        actor: ChargePointActor,
        uploads: LogUploadQueue,
        state: alloc::sync::Arc<LogUploadState>,
    );
}

/// Decides what to answer a `GetLog`/`GetDiagnostics` with, and queues the upload if it is
/// accepted.
///
/// Synchronous and immediate by design - see the module docs. The only thing that can make this
/// `Rejected` is the log genuinely not being available (N01.FR.05): no file-transfer capability
/// declared, or a log kind this charge point does not keep.
pub async fn handle_get_log<C: Clock>(
    actor: &ChargePointActor,
    uploads: &LogUploadQueue,
    state: &LogUploadState,
    clock: &C,
    request: LogUploadRequest,
) -> GetLogOutcome {
    if !crate::refusal::capability_present(&actor.state().capabilities, "GetLog") {
        return GetLogOutcome::Rejected;
    }
    if request.remote_location.is_empty() {
        return GetLogOutcome::Rejected;
    }

    let filename = log_filename(clock, request.log_kind);
    let (ticket, superseded) = state.claim();
    uploads.channel.send((ticket, request));

    if superseded {
        // N01.FR.12. The upload it replaced is still running; what changes is that its result
        // will be discarded rather than reported - see this module's docs on what cancel means.
        GetLogOutcome::AcceptedCanceled { filename }
    } else {
        GetLogOutcome::Accepted { filename }
    }
}

/// Performs every accepted log upload, reporting each one's progress to the CSMS, until the
/// process ends.
///
/// One upload at a time, deliberately: a charge point that ran several concurrently would be
/// dividing a link that is usually the scarce resource, and OCPP's own sequence has no way to
/// describe two in flight anyway.
pub async fn run_log_uploads<T, N, B>(
    uploads: LogUploadQueue,
    state: &LogUploadState,
    transfer: &T,
    notifier: &N,
    log: &SecurityEventLog,
    backoff: &B,
) where
    T: FileTransfer,
    N: LogStatusNotifier,
    B: Backoff,
{
    loop {
        let (ticket, request) = uploads.channel.recv().await;
        // Already superseded before it even started - two `GetLog`s in the same instant. Nothing
        // to report: the CSMS was told `AcceptedCanceled` by the request that replaced it.
        if !state.is_current(ticket) {
            continue;
        }

        report(notifier, request.request_id, LogUploadStatus::Uploading).await;
        let outcome = upload_with_retries(state, ticket, transfer, log, backoff, &request).await;

        // Checked again after the transfer: a `GetLog` that arrived while this was running has
        // already answered `AcceptedCanceled`, and reporting this one's result now would tell the
        // CSMS about an upload it believes was cancelled.
        if !state.is_current(ticket) {
            tracing::info!(
                request_id = ?request.request_id,
                "discarding a superseded log upload's result"
            );
            continue;
        }
        state.finish(ticket);
        report(
            notifier,
            request.request_id,
            match outcome {
                Ok(()) => LogUploadStatus::Uploaded,
                Err(()) => LogUploadStatus::UploadFailure,
            },
        )
        .await;
    }
}

/// Runs the transfer, retrying up to `request.retries` times. `Err(())` means every attempt
/// failed - the individual errors are logged as they happen, since only the final outcome is
/// reportable (N01.FR.10).
async fn upload_with_retries<T, B>(
    state: &LogUploadState,
    ticket: u64,
    transfer: &T,
    log: &SecurityEventLog,
    backoff: &B,
    request: &LogUploadRequest,
) -> Result<(), ()>
where
    T: FileTransfer,
    B: Backoff,
{
    // Rendered once, outside the retry loop: the log is a snapshot of what was there when the
    // CSMS asked, and re-rendering between attempts would silently change the file's contents
    // depending on how many times the network failed.
    let rendered = match request.log_kind {
        LogKind::Security => Some(render_security_log(&filtered_entries(log, request))),
        LogKind::Diagnostics | LogKind::DataCollector => None,
    };

    for attempt in 0..=request.retries {
        if attempt > 0 {
            backoff.wait(request.retry_interval_secs).await;
            // A supersede takes effect here rather than mid-transfer - the boundary this crate
            // can actually observe without a `select!`.
            if !state.is_current(ticket) {
                return Err(());
            }
        }
        let source = match &rendered {
            Some(bytes) => UploadSource::Bytes(bytes),
            None => UploadSource::Local(request.log_kind),
        };
        match transfer
            .upload(
                &request.remote_location,
                source,
                &TransferProgress::ignored(),
            )
            .await
        {
            Ok(()) => return Ok(()),
            Err(err) => tracing::warn!(
                error = %err,
                attempt = attempt + 1,
                attempts = request.retries + 1,
                // Never the URL: it may embed credentials (N01.FR.17's
                // `http://username:password@...`), and a log line is the one place they would
                // outlive the request.
                "a log upload attempt failed"
            ),
        }
    }
    Err(())
}

/// The security-log entries inside the window the CSMS asked for.
///
/// An entry with no timestamp (recorded while the clock was unsynchronized) is **kept** whenever
/// the CSMS narrowed the range: it cannot be shown to fall outside a window, and dropping events
/// a charge point could not time is exactly the wrong bias for a security log - the period around
/// a clock that was not yet set is when something interesting happened.
fn filtered_entries(
    log: &SecurityEventLog,
    request: &LogUploadRequest,
) -> alloc::vec::Vec<crate::security::SecurityLogEntry> {
    log.entries()
        .into_iter()
        .filter(|entry| match entry.recorded_at {
            Some(at) => {
                request.oldest.is_none_or(|oldest| at >= oldest)
                    && request.latest.is_none_or(|latest| at <= latest)
            }
            None => true,
        })
        .collect()
}

/// Reports one status, logging (and swallowing) a failure - the upload's own progress must not
/// depend on the CSMS being reachable to hear about it.
async fn report<N: LogStatusNotifier>(
    notifier: &N,
    request_id: Option<i64>,
    status: LogUploadStatus,
) {
    if let Err(err) = notifier.notify_log_status(request_id, status).await {
        tracing::warn!(error = %err, ?status, "failed to report a log upload status");
    }
}

#[cfg(feature = "ocpp_1_6")]
mod ocpp_1_6;
#[cfg(feature = "ocpp_2_0_1")]
mod ocpp_2_0_1;
#[cfg(feature = "ocpp_2_1")]
mod ocpp_2_1;

#[cfg(feature = "ocpp_1_6")]
pub use self::ocpp_1_6::Ocpp1_6DiagnosticsHandler;
#[cfg(feature = "ocpp_2_0_1")]
pub use self::ocpp_2_0_1::Ocpp2_0_1DiagnosticsHandler;
#[cfg(feature = "ocpp_2_1")]
pub use self::ocpp_2_1::Ocpp2_1DiagnosticsHandler;

#[cfg(test)]
mod tests;
