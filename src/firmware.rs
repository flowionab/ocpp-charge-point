//! Firmware Management functional block: over-the-air updates (`docs/ROADMAP.md` §12,
//! `docs/PRODUCTION-ROADMAP.md` B3.2). OCPP's L01/L02, implemented against the vendored 2.1 spec.
//!
//! # The shape of an update
//!
//! `UpdateFirmware` names a URL, optionally when to fetch it (`retrieveDateTime`) and when to
//! install it (`installDateTime`). The charge point walks a state machine and reports **every**
//! transition as a `FirmwareStatusNotification` (L01.FR.01), all carrying the request id that
//! started the update (L01.FR.10):
//!
//! ```text
//! DownloadScheduled? → Downloading → Downloaded → InstallScheduled? → Installing → Installed
//!                            ↓            (waiting for transactions)        ↓
//!                      DownloadFailed                              InstallationFailed
//!                                                                           ↓
//!                                                                    InstallRebooting
//! ```
//!
//! Like the log upload in [`crate::diagnostics`], the work happens on a worker task rather than in
//! the handler: an update takes minutes, and the response has to go out first.
//!
//! # Waiting for transactions, and why availability changes
//!
//! L01.FR.06 says installation waits for transactions to end. L01.FR.07 goes further: while
//! waiting, every EVSE not in use is made **Unavailable**, and any that becomes free is made
//! unavailable too - otherwise a charge point that is about to reboot keeps accepting drivers who
//! will be cut off mid-session. This crate honours that unless the CSMS sets
//! `AllowNewSessionsPendingFirmwareUpdate`, and restores availability if the install fails, so a
//! failed update does not leave a charge point silently out of service.
//!
//! # Signature verification (B3.3)
//!
//! An update whose request carried a `signature` and/or `signingCertificate` is verified against
//! the downloaded image, through [`crate::hardware::FirmwareVerifier`], strictly between
//! `Downloaded` and the wait for a scheduled install (`installDateTime`) - **never** after: an
//! image that fails verification, or that this charge point could not verify at all (no verifier
//! wired, or an algorithm it does not support), is refused rather than installed, per
//! `CLAUDE.md`'s fail-safe stance. A failure is reported as `InvalidSignature` and raises the
//! matching [`crate::state::SecurityEventType`] (`InvalidFirmwareSignature` or
//! `InvalidFirmwareSigningCertificate`). An update carrying neither field is not a signed update
//! at all - OCPP allows plain, unsigned `UpdateFirmware` - and skips verification entirely, the
//! same as before B3.3. See [`crate::hardware::FirmwareVerifier`]'s docs for where the line
//! between this crate and the integrator is drawn and why.
//!
//! # What this block does not do yet
//!
//! Reporting `Installed` *after* a reboot remains open - it needs a marker that survives the
//! restart, the same shape as [`crate::persistence::BootReasonStore`]. Pre-download certificate
//! chain validation (answering 2.x's `InvalidCertificate`/`RevokedCertificate`, or 1.6J's
//! `SignedUpdateFirmware` equivalents, synchronously in the accept/reject response) is also not
//! done: that response goes out before any download starts, and
//! [`crate::hardware::FirmwareVerifier`] is scoped to checking the *downloaded image*, not to
//! chain-validating a certificate the charge point has not yet fetched anything to check it
//! against.

use alloc::boxed::Box;
use alloc::string::String;
use chrono::{DateTime, Utc};
use core::cell::RefCell;
use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;

use crate::actor::ChargePointActor;
use crate::clock::Clock;
use crate::hardware::{
    FileTransfer, FirmwareInstallOutcome, FirmwareInstaller, FirmwareVerificationOutcome,
    FirmwareVerifier, TransferProgress,
};
use crate::provisioning::Backoff;
use crate::security::report_security_event;
use crate::state::{ChargePointEvent, EvseEvent, SecurityEvent, SecurityEventType};
use crate::sync::Chan;

/// How often the worker re-checks a condition it is waiting on - a scheduled start time, or
/// transactions that have to finish first.
///
/// Ten seconds: fine-grained enough that an update does not visibly lag the moment it becomes
/// possible, coarse enough to cost nothing on a device that has other work.
const WAIT_POLL_SECS: u32 = 10;

/// Download attempts when the CSMS names no `retries` - one, no retry, for the reason
/// [`crate::diagnostics::DEFAULT_RETRIES`] gives.
pub const DEFAULT_RETRIES: u32 = 0;

/// Seconds between download attempts when the CSMS names no `retryInterval`.
pub const DEFAULT_RETRY_INTERVAL_SECS: u32 = 30;

/// A CSMS request to update this charge point's firmware, protocol-independent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirmwareUpdateRequest {
    /// Correlates every status notification with the request that started the update
    /// (L01.FR.10). 1.6J's `UpdateFirmware` carries no request id, and its
    /// `FirmwareStatusNotification` has no field for one either.
    pub request_id: Option<i64>,
    /// Where to fetch the image from.
    pub location: String,
    /// When to start downloading. `None`, or an instant already past, means "now"; a future one
    /// means `DownloadScheduled` (L01.FR.13).
    pub retrieve_at: Option<DateTime<Utc>>,
    /// When to install. `None`, or an instant already past, means "as soon as able" (L01.FR.05);
    /// a future one means `InstallScheduled` (L01.FR.16).
    pub install_at: Option<DateTime<Utc>>,
    /// The image's digital signature (base64, per OCPP's convention). `Some` marks this as a
    /// signed update: it is verified (along with `signing_certificate`) through
    /// [`crate::hardware::FirmwareVerifier`] before installing - see the module docs.
    pub signature: Option<String>,
    /// The certificate the signature should be verified against (PEM). Same status as
    /// `signature`.
    pub signing_certificate: Option<String>,
    /// How many times to retry a failed download, beyond the first attempt.
    pub retries: u32,
    /// How long to wait between download attempts, in seconds.
    pub retry_interval_secs: u32,
}

/// What a charge point answers an `UpdateFirmware` with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateFirmwareOutcome {
    /// The update was accepted.
    Accepted,
    /// Accepted, and an update that was already in progress has been cancelled (L01.FR.24). 1.6J
    /// has no such status and reports a plain accept.
    AcceptedCanceled,
    /// Refused - firmware management is not available on this charge point.
    Rejected,
}

/// Every state a firmware update passes through, mapped onto OCPP's `FirmwareStatusEnum`.
///
/// This is the superset 2.x expresses; 1.6J has only seven of them and its adapter projects the
/// rest onto the nearest it has (see `ocpp_1_6`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirmwareStatus {
    /// Nothing in progress. The only status OCPP allows without a request id (L01.FR.20).
    Idle,
    /// The download has not started because `retrieveDateTime` is still in the future
    /// (L01.FR.13).
    DownloadScheduled,
    /// Fetching the image.
    Downloading,
    /// The image is fetched.
    Downloaded,
    /// Every download attempt failed.
    DownloadFailed,
    /// The image is fetched but `installDateTime` is still in the future (L01.FR.16).
    InstallScheduled,
    /// Installation is under way.
    Installing,
    /// The new firmware is installed and running.
    Installed,
    /// Installation failed.
    InstallationFailed,
    /// The image is staged and the charge point is about to restart to run it (L01.FR.15).
    InstallRebooting,
    /// The downloaded image's signature was checked against `signingCertificate` and verified
    /// (B3.3). Only ever reported for a signed update, immediately before installation resumes.
    SignatureVerified,
    /// The downloaded image's signature failed verification, or could not be verified at all (no
    /// verifier configured, or an unsupported algorithm) - B3.3's fail-safe path. The update stops
    /// here: nothing is installed.
    InvalidSignature,
}

/// Reports a firmware update's progress to the CSMS.
#[async_trait::async_trait]
pub trait FirmwareStatusNotifier {
    /// What went wrong reporting the status.
    type Error: core::fmt::Display;

    /// Reports `status` for the update started by `request_id`.
    async fn notify_firmware_status(
        &self,
        request_id: Option<i64>,
        status: FirmwareStatus,
    ) -> Result<(), Self::Error>;
}

#[async_trait::async_trait]
impl<T: FirmwareStatusNotifier + Send + Sync + ?Sized> FirmwareStatusNotifier
    for alloc::sync::Arc<T>
{
    type Error = T::Error;

    async fn notify_firmware_status(
        &self,
        request_id: Option<i64>,
        status: FirmwareStatus,
    ) -> Result<(), Self::Error> {
        (**self).notify_firmware_status(request_id, status).await
    }
}

/// Registers this charge point's inbound `UpdateFirmware` handling.
#[async_trait::async_trait]
pub trait UpdateFirmwareHandler {
    /// Registers a handler dispatching against `actor`.
    async fn register_update_firmware_handler(
        &self,
        actor: ChargePointActor,
        updates: FirmwareUpdateQueue,
        state: alloc::sync::Arc<FirmwareUpdateState>,
    );
}

/// Registers this charge point's inbound `SignedUpdateFirmware` handling - the 1.6J Security
/// Whitepaper's signed alternative to plain `UpdateFirmware` (B3.3).
///
/// A distinct trait, not a second method on [`UpdateFirmwareHandler`], because `SignedUpdateFirmware`
/// is a genuinely different action with its own request/response shape - 1.6J is the only
/// protocol with two separate actions for the same underlying operation; 2.x's `UpdateFirmware`
/// already carries `signature`/`signingCertificate` inline, so it needs nothing here.
///
/// The default implementation is a no-op: 2.0.1 and 2.1's adapters implement this trait with an
/// empty body so [`ChargePointBuilder::firmware_updates`](crate::builder::ChargePointBuilder::firmware_updates)
/// can require it uniformly across every protocol version without 2.x's handlers having anything
/// to actually register.
#[async_trait::async_trait]
pub trait SignedUpdateFirmwareHandler {
    /// Registers a handler dispatching against `actor`, feeding the same queue and state
    /// [`UpdateFirmwareHandler::register_update_firmware_handler`] does - a `SignedUpdateFirmware`
    /// and a plain `UpdateFirmware` are the same update to the worker that drives it, only the
    /// wire shape differs.
    async fn register_signed_update_firmware_handler(
        &self,
        actor: ChargePointActor,
        updates: FirmwareUpdateQueue,
        state: alloc::sync::Arc<FirmwareUpdateState>,
    ) {
        let _ = (actor, updates, state);
    }
}

/// Tracks which update is current and what was last reported, so a superseded update can be
/// dropped and a `TriggerMessage` can be answered (L01.FR.25/26).
#[derive(Debug)]
pub struct FirmwareUpdateState {
    inner: BlockingMutex<CriticalSectionRawMutex, RefCell<FirmwareUpdateInner>>,
}

#[derive(Debug)]
struct FirmwareUpdateInner {
    current: u64,
    in_progress: bool,
    last_status: FirmwareStatus,
    last_request_id: Option<i64>,
}

impl Default for FirmwareUpdateState {
    fn default() -> Self {
        Self {
            inner: BlockingMutex::new(RefCell::new(FirmwareUpdateInner {
                current: 0,
                in_progress: false,
                last_status: FirmwareStatus::Idle,
                last_request_id: None,
            })),
        }
    }
}

impl FirmwareUpdateState {
    /// A state with no update in progress.
    pub fn new() -> Self {
        Self::default()
    }

    /// What a `TriggerMessage` for `FirmwareStatusNotification` should report: `Idle` once the
    /// last update finished installing (L01.FR.25), otherwise the last status sent (L01.FR.26).
    ///
    /// The request id goes with it, except for `Idle`, which is the one status OCPP allows to
    /// carry none (L01.FR.20) - and after an `Installed` there is no update left to correlate to.
    pub fn triggered_status(&self) -> (Option<i64>, FirmwareStatus) {
        self.inner.lock(|inner| {
            let inner = inner.borrow();
            match inner.last_status {
                FirmwareStatus::Installed | FirmwareStatus::Idle => (None, FirmwareStatus::Idle),
                status => (inner.last_request_id, status),
            }
        })
    }

    fn claim(&self) -> (u64, bool) {
        self.inner.lock(|inner| {
            let mut inner = inner.borrow_mut();
            let superseded = inner.in_progress;
            inner.current += 1;
            inner.in_progress = true;
            (inner.current, superseded)
        })
    }

    fn is_current(&self, ticket: u64) -> bool {
        self.inner.lock(|inner| inner.borrow().current == ticket)
    }

    fn record(&self, request_id: Option<i64>, status: FirmwareStatus) {
        self.inner.lock(|inner| {
            let mut inner = inner.borrow_mut();
            inner.last_status = status;
            inner.last_request_id = request_id;
            if matches!(
                status,
                FirmwareStatus::Installed
                    | FirmwareStatus::InstallationFailed
                    | FirmwareStatus::DownloadFailed
                    | FirmwareStatus::InvalidSignature
            ) {
                inner.in_progress = false;
            }
        });
    }
}

/// The queue a handler hands accepted updates to [`run_firmware_updates`] through.
#[derive(Clone)]
pub struct FirmwareUpdateQueue {
    channel: Chan<(u64, FirmwareUpdateRequest)>,
}

impl FirmwareUpdateQueue {
    /// A new, empty queue.
    pub fn new() -> Self {
        Self {
            channel: Chan::new(),
        }
    }
}

impl Default for FirmwareUpdateQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// Decides what to answer an `UpdateFirmware` with, and queues the update if accepted.
#[tracing::instrument(skip_all)]
pub async fn handle_update_firmware(
    actor: &ChargePointActor,
    updates: &FirmwareUpdateQueue,
    state: &FirmwareUpdateState,
    request: FirmwareUpdateRequest,
) -> UpdateFirmwareOutcome {
    if !crate::refusal::capability_present(&actor.state().capabilities, "UpdateFirmware") {
        return UpdateFirmwareOutcome::Rejected;
    }
    if request.location.is_empty() {
        return UpdateFirmwareOutcome::Rejected;
    }

    let (ticket, superseded) = state.claim();
    updates.channel.send((ticket, request));
    if superseded {
        // L01.FR.24. Deliberately not checked against whether a *file* exists first, which the
        // spec calls out: that is how a CSMS cancels an update without starting a real one.
        UpdateFirmwareOutcome::AcceptedCanceled
    } else {
        UpdateFirmwareOutcome::Accepted
    }
}

/// Performs every accepted firmware update, reporting each state change to the CSMS.
#[allow(clippy::too_many_arguments)]
pub async fn run_firmware_updates<T, I, N, C, B, V>(
    actor: &ChargePointActor,
    updates: FirmwareUpdateQueue,
    state: &FirmwareUpdateState,
    transfer: &T,
    installer: &I,
    notifier: &N,
    clock: &C,
    backoff: &B,
    verifier: &V,
) where
    T: FileTransfer,
    I: FirmwareInstaller,
    N: FirmwareStatusNotifier,
    C: Clock,
    B: Backoff,
    V: FirmwareVerifier,
{
    loop {
        let (ticket, request) = updates.channel.recv().await;
        if !state.is_current(ticket) {
            continue;
        }
        run_one_update(
            actor, state, ticket, transfer, installer, notifier, clock, backoff, verifier, &request,
        )
        .await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_one_update<T, I, N, C, B, V>(
    actor: &ChargePointActor,
    state: &FirmwareUpdateState,
    ticket: u64,
    transfer: &T,
    installer: &I,
    notifier: &N,
    clock: &C,
    backoff: &B,
    verifier: &V,
    request: &FirmwareUpdateRequest,
) where
    T: FileTransfer,
    I: FirmwareInstaller,
    N: FirmwareStatusNotifier,
    C: Clock,
    B: Backoff,
    V: FirmwareVerifier,
{
    let report = async |status| {
        state.record(request.request_id, status);
        if let Err(err) = notifier
            .notify_firmware_status(request.request_id, status)
            .await
        {
            tracing::warn!(error = %err, ?status, "failed to report a firmware status");
        }
    };

    // L01.FR.13: a download the CSMS scheduled for later is announced as scheduled, not silently
    // delayed - a CSMS that heard nothing could not tell a scheduled update from a lost one.
    if is_future(clock, request.retrieve_at) {
        report(FirmwareStatus::DownloadScheduled).await;
        if !wait_until(state, ticket, clock, backoff, request.retrieve_at).await {
            return;
        }
    }

    report(FirmwareStatus::Downloading).await;
    if !download_with_retries(state, ticket, transfer, backoff, request).await {
        if state.is_current(ticket) {
            report(FirmwareStatus::DownloadFailed).await;
        }
        return;
    }
    if !state.is_current(ticket) {
        return;
    }
    report(FirmwareStatus::Downloaded).await;

    // B3.3: an update carrying neither field is unsigned by OCPP's own design (plain
    // `UpdateFirmware` makes both optional), and is not held to a bar this crate never asked the
    // CSMS to clear. One carrying either is verified before anything else happens to it - strictly
    // before the install wait below, never after installing.
    if request.signature.is_some() || request.signing_certificate.is_some() {
        let verified = verifier
            .verify(
                request.signing_certificate.as_deref(),
                request.signature.as_deref(),
            )
            .await;
        let event_type = match &verified {
            Ok(FirmwareVerificationOutcome::Valid) => None,
            Ok(FirmwareVerificationOutcome::InvalidSignature) => {
                Some(SecurityEventType::InvalidFirmwareSignature)
            }
            Ok(FirmwareVerificationOutcome::InvalidSigningCertificate) => {
                Some(SecurityEventType::InvalidFirmwareSigningCertificate)
            }
            Err(err) => {
                // Cannot verify at all (no verifier wired, or an algorithm it does not support) -
                // the fail-safe answer is the same as a definite failure: refuse to install.
                tracing::warn!(error = %err, "could not verify a firmware image's signature");
                Some(SecurityEventType::InvalidFirmwareSignature)
            }
        };
        if let Some(event_type) = event_type {
            report_security_event(
                actor,
                SecurityEvent {
                    event_type,
                    tech_info: None,
                },
            )
            .await;
            if state.is_current(ticket) {
                report(FirmwareStatus::InvalidSignature).await;
            }
            return;
        }
        if !state.is_current(ticket) {
            return;
        }
        report(FirmwareStatus::SignatureVerified).await;
    }

    // L01.FR.16.
    if is_future(clock, request.install_at) {
        report(FirmwareStatus::InstallScheduled).await;
        if !wait_until(state, ticket, clock, backoff, request.install_at).await {
            return;
        }
    }

    // L01.FR.06/07: hold new sessions off while the transactions that are running finish, so the
    // reboot below does not cut anyone off mid-charge.
    let reserved_availability = !allows_new_sessions(actor);
    if reserved_availability {
        set_all_evses(actor, EvseEvent::SetUnavailable).await;
    }
    if !wait_for_idle(actor, state, ticket, backoff).await {
        if reserved_availability {
            set_all_evses(actor, EvseEvent::SetAvailable).await;
        }
        return;
    }

    report(FirmwareStatus::Installing).await;
    match installer.install().await {
        Ok(FirmwareInstallOutcome::Installed) => {
            if reserved_availability {
                set_all_evses(actor, EvseEvent::SetAvailable).await;
            }
            report(FirmwareStatus::Installed).await;
        }
        Ok(FirmwareInstallOutcome::RebootRequired) => {
            // L01.FR.15: told *before* the reboot, because afterwards there is no process left to
            // tell anyone. Availability is deliberately left withheld - the charge point is about
            // to restart, and re-opening it for a session that a reboot would kill is worse than
            // coming back up unavailable.
            report(FirmwareStatus::InstallRebooting).await;
            // Through the existing `Reset` path rather than a parallel one, per `CLAUDE.md`: it
            // already drives every connector through the fail-safe stop (open the contactor, then
            // unlock) before the reboot reaches hardware. `Immediate` is safe here precisely
            // because `wait_for_idle` above has already established there is nothing running.
            let _ = actor
                .send(ChargePointEvent::ResetRequested {
                    target: crate::state::ResetTarget::ChargePoint,
                    kind: crate::state::ResetKind::Immediate,
                })
                .await;
        }
        Err(err) => {
            tracing::warn!(error = %err, "installing firmware failed");
            if reserved_availability {
                // A failed update must not leave a charge point silently out of service.
                set_all_evses(actor, EvseEvent::SetAvailable).await;
            }
            report(FirmwareStatus::InstallationFailed).await;
        }
    }
}

/// Whether `at` is still in the future by the charge point's clock.
///
/// An unsynchronized clock treats every schedule as **due now** rather than waiting: a charge
/// point that does not know the time cannot know a scheduled instant has arrived, and an update
/// that never starts is a worse failure than one that starts early - the CSMS asked for it either
/// way, and `crate::clock` already reports the clock as untrustworthy elsewhere.
fn is_future<C: Clock>(clock: &C, at: Option<DateTime<Utc>>) -> bool {
    let now = clock.now();
    crate::clock::is_synchronized(&now) && at.is_some_and(|at| at > now)
}

/// Waits until `until` passes, or the update is superseded. Returns whether it is still current.
async fn wait_until<C: Clock, B: Backoff>(
    state: &FirmwareUpdateState,
    ticket: u64,
    clock: &C,
    backoff: &B,
    until: Option<DateTime<Utc>>,
) -> bool {
    while is_future(clock, until) {
        backoff.wait(WAIT_POLL_SECS).await;
        if !state.is_current(ticket) {
            return false;
        }
    }
    state.is_current(ticket)
}

/// Waits for every connector to be free of a transaction (L01.FR.06). Returns whether the update
/// is still current.
async fn wait_for_idle<B: Backoff>(
    actor: &ChargePointActor,
    state: &FirmwareUpdateState,
    ticket: u64,
    backoff: &B,
) -> bool {
    loop {
        let busy = actor
            .state()
            .evses
            .iter()
            .any(|evse| evse.transactions.iter().any(Option::is_some));
        if !busy {
            return state.is_current(ticket);
        }
        backoff.wait(WAIT_POLL_SECS).await;
        if !state.is_current(ticket) {
            return false;
        }
    }
}

/// `OCPPCommCtrlr`/`AllowNewSessionsPendingFirmwareUpdate`, defaulting to **false** - L01.FR.07's
/// own wording is "is false or does not exist", so an absent variable withholds new sessions.
fn allows_new_sessions(actor: &ChargePointActor) -> bool {
    let state = actor.state();
    let component = crate::state::Component {
        name: "OCPPCommCtrlr".into(),
        instance: None,
        evse: None,
    };
    let variable = crate::state::Variable {
        name: "AllowNewSessionsPendingFirmwareUpdate".into(),
        instance: None,
    };
    state
        .device_model
        .get(&component, &variable)
        .and_then(|definition| definition.attribute(crate::state::VariableAttributeType::Actual))
        .map(|attribute| attribute.value.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Applies `event` to every EVSE.
async fn set_all_evses(actor: &ChargePointActor, event: EvseEvent) {
    let evse_count = actor.state().evses.len();
    for evse_id in 0..evse_count {
        let _ = actor
            .send(ChargePointEvent::Evse {
                evse_id,
                event: event.clone(),
            })
            .await;
    }
}

/// Downloads the image, retrying as the CSMS asked. Returns whether it succeeded.
async fn download_with_retries<T: FileTransfer, B: Backoff>(
    state: &FirmwareUpdateState,
    ticket: u64,
    transfer: &T,
    backoff: &B,
    request: &FirmwareUpdateRequest,
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
                // Never the URL: a firmware location may embed credentials, and a log line is
                // where they would outlive the request.
                "a firmware download attempt failed"
            ),
        }
    }
    false
}

#[cfg(feature = "ocpp_1_6")]
mod ocpp_1_6;
#[cfg(feature = "ocpp_2_0_1")]
mod ocpp_2_0_1;
#[cfg(feature = "ocpp_2_1")]
mod ocpp_2_1;

#[cfg(feature = "ocpp_1_6")]
pub use self::ocpp_1_6::Ocpp1_6FirmwareHandler;
#[cfg(feature = "ocpp_2_0_1")]
pub use self::ocpp_2_0_1::Ocpp2_0_1FirmwareHandler;
#[cfg(feature = "ocpp_2_1")]
pub use self::ocpp_2_1::Ocpp2_1FirmwareHandler;

#[cfg(test)]
mod tests;
