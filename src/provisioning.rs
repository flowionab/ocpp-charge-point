//! Provisioning functional block: BootNotification and charge-point registration with the
//! CSMS. See `docs/ROADMAP.md` §2.

use crate::actor::ChargePointActor;
use crate::clock::MonotonicClock;
use crate::state::{
    BootReasonCause, ChargePointEvent, Component, RegistrationStatus, Variable,
    VariableAttributeType,
};
use alloc::boxed::Box;
use chrono::{DateTime, Utc};
#[cfg(feature = "tokio-runtime")]
use core::time::Duration;

/// The longest this crate waits before retrying BootNotification after a transport-level failure
/// (as opposed to a `Pending`/`Rejected` response, which carries its own retry interval).
///
/// Reached by doubling from [`FIRST_RETRY_INTERVAL_SECS`] rather than applied to the first retry -
/// see [`register_until_accepted`] for why the first failure after a reconnect deserves a much
/// shorter wait than a CSMS that is simply down.
pub const DEFAULT_RETRY_INTERVAL_SECS: u32 = 30;

/// How long to wait before the *first* retry after a transport-level BootNotification failure.
///
/// A transport failure is not always a CSMS that is down: the common case in this crate is a send
/// that raced a redial, where the connection is back within a second and waiting
/// [`DEFAULT_RETRY_INTERVAL_SECS`] would leave the charge point unregistered - and so unable to
/// start a transaction - for half a minute with a perfectly good socket open. Retrying quickly
/// first and doubling from there covers both cases without needing to tell them apart.
pub const FIRST_RETRY_INTERVAL_SECS: u32 = 1;

/// The CSMS's answer to a BootNotification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootNotificationOutcome {
    /// The CSMS's registration decision.
    pub status: RegistrationStatus,
    /// Seconds to wait: the heartbeat interval when `status` is `Accepted`, otherwise the
    /// minimum wait before the charge point may retry BootNotification.
    pub interval_secs: u32,
    /// The CSMS's `currentTime`, as carried by every version's BootNotification response.
    /// `None` only if the wire value failed to parse as RFC 3339 (see
    /// [`parse_csms_current_time`]) - logged by the adapter that produced it, not silently
    /// dropped. Consumed by [`evaluate_time_sync`] to detect a clock step worth reporting via
    /// `SettingSystemTime` - see [`register`]/[`register_until_accepted`].
    pub current_time: Option<DateTime<Utc>>,
}

/// Sends a BootNotification and reports the CSMS's decision.
///
/// Implemented per protocol version (see the `ocpp_2_1` module) so
/// [`crate::ChargePointRuntime::register`] stays protocol-agnostic, and so tests can supply a
/// fake implementation without a live connection.
#[async_trait::async_trait]
pub trait BootNotifier {
    /// The error type returned if the BootNotification request itself fails.
    type Error: core::error::Error + Send + Sync + 'static;

    /// Sends a BootNotification identifying the charge point by `vendor_name`/`model_name` and
    /// carrying `reason` (this crate's protocol-version-independent model of *why* - see
    /// [`BootReasonCause`]'s docs; `None` means an uncommanded restart), and returns the CSMS's
    /// decision. Each protocol adapter below maps `reason` onto its own wire `BootReasonEnum` (or
    /// drops it - OCPP 1.6J's `BootNotification.req` has no `reason` field at all).
    async fn notify_boot(
        &self,
        vendor_name: &str,
        model_name: &str,
        reason: Option<BootReasonCause>,
    ) -> Result<BootNotificationOutcome, Self::Error>;
}

/// Waits between BootNotification retry attempts. Abstracted so tests can skip real delays,
/// and so embedded targets without tokio can supply their own timer.
#[async_trait::async_trait]
pub trait Backoff {
    /// Waits `seconds` before the caller retries.
    async fn wait(&self, seconds: u32);
}

/// A [`Backoff`] backed by `tokio::time::sleep`. Requires the `tokio-runtime` feature.
#[cfg(feature = "tokio-runtime")]
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioBackoff;

#[cfg(feature = "tokio-runtime")]
#[async_trait::async_trait]
impl Backoff for TokioBackoff {
    async fn wait(&self, seconds: u32) {
        tokio::time::sleep(Duration::from_secs(seconds as u64)).await;
    }
}

/// Sends a Heartbeat, telling the CSMS the charge point is still alive. Implemented per
/// protocol version (see the `ocpp_2_1` module), mirroring [`BootNotifier`].
#[async_trait::async_trait]
pub trait HeartbeatSender {
    /// The error type returned if the Heartbeat request itself fails.
    type Error: core::error::Error + Send + Sync + 'static;

    /// Sends a Heartbeat to the CSMS, returning its `currentTime` (carried by every version's
    /// Heartbeat response). `Ok(None)` only if the wire value failed to parse as RFC 3339 (see
    /// [`parse_csms_current_time`]) - logged by the adapter that produced it, not silently
    /// dropped. Consumed by [`evaluate_time_sync`] via [`run_heartbeat`].
    async fn send_heartbeat(&self) -> Result<Option<DateTime<Utc>>, Self::Error>;
}

/// Runs the Provisioning functional block's BootNotification exchange against `actor`: sends a
/// BootNotification via `notifier` and applies the CSMS's decision to the charge point's state
/// (see `ChargePointEvent::RegistrationStatusReceived`). If the response carried a parseable
/// `currentTime`, also evaluates it via [`evaluate_time_sync`] against `actor`'s stored
/// [`crate::state::TimeSyncAnchor`] (advanced by `monotonic` since it was recorded - see
/// `local_time_estimate`, private to this module), reports a `SettingSystemTime` security event when warranted, and
/// advances the stored anchor either way. This crate only *detects and reports* a clock step this
/// way - actually correcting the system/RTC clock is the integrator's job (typically in response
/// to observing the `SettingSystemTime` `SecurityEventNotification`, or by reading
/// [`ChargePointActor::state`]`().time_sync` directly), not something this function does.
///
/// This performs a single BootNotification attempt. On `Pending`/`Rejected`, callers are
/// responsible for retrying after the returned outcome's interval, per the OCPP spec. See
/// [`register_until_accepted`] for a version that retries automatically.
///
/// `reason` is this crate's protocol-version-independent model of why this BootNotification is
/// being sent - see [`BootReasonCause`]'s docs. `None` means an uncommanded restart (no cause was
/// persisted before this boot); pass whatever
/// [`crate::persistence::BootReasonStore::load`] returned, unmodified - this function does not
/// interpret `reason` itself, only forwards it to `notifier`.
pub async fn register<N: BootNotifier, M: MonotonicClock>(
    actor: &ChargePointActor,
    notifier: &N,
    monotonic: &M,
    vendor_name: &str,
    model_name: &str,
    reason: Option<BootReasonCause>,
) -> Result<RegistrationStatus, N::Error> {
    let outcome = notifier
        .notify_boot(vendor_name, model_name, reason)
        .await?;
    let _ = actor
        .send(ChargePointEvent::RegistrationStatusReceived(outcome.status))
        .await;
    if let Some(csms_time) = outcome.current_time {
        sync_time(actor, monotonic, csms_time).await;
    }
    Ok(outcome.status)
}

/// Runs BootNotification against `actor` repeatedly until the CSMS accepts registration,
/// applying every intermediate decision to the charge point's state. Per OCPP, a charge point
/// does not give up: on `Pending`/`Rejected` it waits the response's `interval_secs` before
/// retrying, and on a transport-level failure it backs off exponentially from
/// [`FIRST_RETRY_INTERVAL_SECS`] to [`DEFAULT_RETRY_INTERVAL_SECS`], resetting the moment an
/// attempt reaches the CSMS at all.
///
/// The escalation matters for how fast a charge point comes back rather than for how hard it
/// tries. A transport failure here is often a send that raced a redial - the profile switch in
/// [`crate::network_switch`] can produce exactly that - and in those cases the socket is healthy
/// again almost immediately, so the first retry should be prompt. A CSMS that is genuinely down
/// still ends up at the same half-minute cadence within a few attempts.
///
/// Every response (including intermediate `Pending`/`Rejected` ones) that carries a parseable
/// `currentTime` is evaluated for a time-sync step, exactly as [`register`] does - see that
/// function's docs for what "evaluated" means and what this crate does versus what the
/// integrator's hardware must do.
///
/// `reason` is sent unchanged with every attempt, including retries - see [`register`]'s docs for
/// what it means and where it comes from. It is *not* re-read from storage between attempts:
/// clearing the persisted cause (once this function returns, having been accepted) is the
/// caller's job - see [`crate::builder::ChargePointBuilder::boot_reason_persistence`] for the
/// clear-after-acceptance timing this crate uses and why.
pub async fn register_until_accepted<N: BootNotifier, B: Backoff, M: MonotonicClock>(
    actor: &ChargePointActor,
    notifier: &N,
    backoff: &B,
    monotonic: &M,
    vendor_name: &str,
    model_name: &str,
    reason: Option<BootReasonCause>,
) -> BootNotificationOutcome {
    let mut retry_secs = FIRST_RETRY_INTERVAL_SECS;
    loop {
        match notifier.notify_boot(vendor_name, model_name, reason).await {
            Ok(outcome) => {
                // The CSMS answered, so whatever the answer is, the transport is healthy: the
                // next transport failure starts its escalation from the bottom again.
                retry_secs = FIRST_RETRY_INTERVAL_SECS;
                let _ = actor
                    .send(ChargePointEvent::RegistrationStatusReceived(outcome.status))
                    .await;
                if let Some(csms_time) = outcome.current_time {
                    sync_time(actor, monotonic, csms_time).await;
                }
                if outcome.status == RegistrationStatus::Accepted {
                    tracing::info!("the CSMS accepted this charge point's registration");
                    return outcome;
                }
                // A CSMS that answers `Pending` or `Rejected` indefinitely - because the station
                // was never provisioned in it, or was provisioned under a different identity - is
                // one of the most common commissioning dead ends, and this loop used to sit in it
                // silently: only a *transport* failure said anything.
                tracing::warn!(
                    status = ?outcome.status,
                    retry_in_secs = outcome.interval_secs,
                    "the CSMS has not accepted this charge point's registration; retrying"
                );
                backoff.wait(outcome.interval_secs).await;
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    retry_in_secs = retry_secs,
                    "boot notification failed, retrying"
                );
                backoff.wait(retry_secs).await;
                retry_secs = retry_secs
                    .saturating_mul(2)
                    .min(DEFAULT_RETRY_INTERVAL_SECS);
            }
        }
    }
}

/// Computes this charge point's current best time estimate from `actor`'s stored
/// [`crate::state::TimeSyncAnchor`], advanced by elapsed monotonic time since the anchor was
/// recorded - `None` if no anchor exists yet (this charge point has never received a parseable
/// `currentTime`). Used as [`evaluate_time_sync`]'s `local_estimate`.
///
/// Deliberately *not* a live [`crate::clock::Clock`] reading: on hardware with no RTC,
/// `Clock::now()` never advances past [`crate::clock::unsynchronized_before`] (this crate never
/// sets the system/RTC clock itself - see [`register`]'s docs), so comparing a fresh unsynchronized
/// reading against the CSMS's `currentTime` on every single Heartbeat would report a "first sync"
/// every cycle, defeating [`CLOCK_STEP_THRESHOLD_SECS`]'s entire point. Anchoring to the last
/// accepted `currentTime` and advancing it by [`MonotonicClock`] elapsed time instead tracks real
/// elapsed time even with no RTC and no `std` clock at all.
fn local_time_estimate<M: MonotonicClock>(
    actor: &ChargePointActor,
    monotonic: &M,
) -> Option<DateTime<Utc>> {
    let anchor = actor.state().time_sync?;
    let elapsed = monotonic.now().duration_since(anchor.recorded_at);
    let elapsed = chrono::Duration::from_std(elapsed).unwrap_or_else(|_| chrono::Duration::zero());
    Some(anchor.csms_time + elapsed)
}

/// Evaluates a freshly received `csms_time` against `actor`'s current time-sync anchor (see
/// [`local_time_estimate`]), reports a `SettingSystemTime` security event via
/// [`crate::security::report_security_event`] when [`evaluate_time_sync`] judges it a step, and
/// unconditionally advances the stored anchor to `csms_time` - not only on a reported step, so the
/// baseline used for the *next* comparison is always the freshest CSMS time seen, keeping routine
/// per-cycle drift small even between steps (see
/// [`crate::state::ChargePointEvent::TimeSynced`]'s docs).
async fn sync_time<M: MonotonicClock>(
    actor: &ChargePointActor,
    monotonic: &M,
    csms_time: DateTime<Utc>,
) {
    let local_estimate = local_time_estimate(actor, monotonic);
    if let Some(step) = evaluate_time_sync(local_estimate, csms_time) {
        crate::security::report_security_event(actor, step.to_security_event()).await;
    }
    let _ = actor
        .send(ChargePointEvent::TimeSynced {
            csms_time,
            recorded_at: monotonic.now(),
        })
        .await;
}

/// Minimum absolute difference, in seconds, between this charge point's own time estimate and a
/// CSMS-supplied `currentTime` (from a BootNotification or Heartbeat response) for the
/// difference to count as a deliberate clock *step* worth raising `SettingSystemTime`
/// (`SecurityEventType::SettingSystemTime`) for - see [`evaluate_time_sync`]. Below this, the
/// difference is treated as routine drift (RTC imprecision) or the request/response round trip's
/// own latency, neither of which is security-relevant and both of which would otherwise raise a
/// `SettingSystemTime` event on almost every single Heartbeat - defeating the point of a
/// *security* event. Ten seconds is comfortably above realistic WebSocket round-trip latency
/// (milliseconds to low seconds even on a poor connection) and low-cost RTC drift over a
/// heartbeat interval (typically tens of PPM, i.e. sub-second over a minute), while still well
/// below "someone/something moved the clock" territory.
pub const CLOCK_STEP_THRESHOLD_SECS: i64 = 10;

/// A CSMS-supplied `currentTime` that [`evaluate_time_sync`] judged worth reporting as a
/// `SettingSystemTime` security event, carrying enough detail to build that report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeSyncStep {
    /// The CSMS's `currentTime`, i.e. the value the charge point's clock should be corrected to.
    pub csms_time: DateTime<Utc>,
    /// `csms_time` minus the charge point's prior estimate, in seconds - positive if the CSMS's
    /// clock is ahead. `0` when there was no prior estimate to compare against (see this
    /// function's docs).
    pub delta_secs: i64,
}

/// Decides whether a CSMS's `currentTime` (from a BootNotification or Heartbeat response)
/// represents a clock *step* worth raising `SettingSystemTime` for, as opposed to routine drift -
/// see [`CLOCK_STEP_THRESHOLD_SECS`]. `local_estimate` is this charge point's own belief about
/// the current time just before the response arrived (typically a [`crate::clock::Clock`]
/// reading taken right before the request was sent).
///
/// Returns `Some` (worth reporting) when:
/// - there was no local estimate at all (`None` - e.g. no `Clock` wired up yet), or
/// - the local estimate itself was unsynchronized (see [`crate::clock::is_synchronized`]) - the
///   very first time real time arrives is exactly the case `SettingSystemTime` exists for, or
/// - `local_estimate` and `csms_time` differ by at least [`CLOCK_STEP_THRESHOLD_SECS`].
///
/// Returns `None` (routine, not reported) when the two are already within
/// [`CLOCK_STEP_THRESHOLD_SECS`] of each other - critically, this means a CSMS that returns a
/// `currentTime` on every single Heartbeat (as the spec allows) does *not* raise a security event
/// on every heartbeat, only on a genuine step.
///
/// Deliberately a pure decision function (no `actor`, no I/O) so the threshold/first-sync/
/// recovery logic above is unit-testable without a live `ChargePointActor` - see [`register`],
/// [`register_until_accepted`], and [`run_heartbeat`] for where it's actually wired in, via
/// `local_time_estimate` and `sync_time` (both private to this module). See
/// `docs/PRODUCTION-ROADMAP.md` §9.3 (G3.2).
pub fn evaluate_time_sync(
    local_estimate: Option<DateTime<Utc>>,
    csms_time: DateTime<Utc>,
) -> Option<TimeSyncStep> {
    match local_estimate {
        None => Some(TimeSyncStep {
            csms_time,
            delta_secs: 0,
        }),
        Some(local) if !crate::clock::is_synchronized(&local) => Some(TimeSyncStep {
            csms_time,
            delta_secs: (csms_time - local).num_seconds(),
        }),
        Some(local) => {
            let delta_secs = (csms_time - local).num_seconds();
            (delta_secs.abs() >= CLOCK_STEP_THRESHOLD_SECS).then_some(TimeSyncStep {
                csms_time,
                delta_secs,
            })
        }
    }
}

/// Parses a wire `currentTime` string (as carried by a BootNotification or Heartbeat response)
/// into a [`DateTime<Utc>`], for use with [`evaluate_time_sync`]. `None` if the CSMS sent
/// something that doesn't parse as RFC 3339 - a malformed `currentTime` shouldn't be treated as
/// "no sync available" silently swallowed further up; callers should log when this returns
/// `None` for a non-empty input.
pub fn parse_csms_current_time(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// A CSMS `currentTime` onto this crate's clock type.
///
/// Since `ocpp-types` 0.2.0 this cannot fail and no longer logs: `currentTime` arrives as an
/// already-validated [`OcppTimestamp`], so a malformed value is refused by `ocpp-client`'s
/// decoder and answered with a `CALLERROR` rather than reaching this crate as an unparseable
/// string. [`parse_csms_current_time`] remains for callers holding a raw string.
///
/// [`OcppTimestamp`]: ocpp_client::ocpp_types::OcppTimestamp
// Only called from the `ocpp_1_6`/`ocpp_2_0_1`/`ocpp_2_1` adapter modules below, each gated on
// its own feature - unused (and so a `dead_code` warning without this) when none of them are
// compiled in, e.g. `--no-default-features`.
#[cfg_attr(
    not(any(feature = "ocpp_1_6", feature = "ocpp_2_0_1", feature = "ocpp_2_1")),
    allow(dead_code)
)]
fn parse_csms_current_time_logged(raw: &crate::wire::OcppTimestamp) -> DateTime<Utc> {
    (*raw).into()
}

impl TimeSyncStep {
    /// Builds the [`crate::state::SecurityEvent`] this step implies, ready to pass to
    /// [`crate::security::report_security_event`]. `tech_info` records both the corrected time
    /// and the observed delta so an operator reading the CSMS's security log can tell a first-ever
    /// sync (`delta_secs: 0`, no prior estimate) apart from a later step.
    pub fn to_security_event(self) -> crate::state::SecurityEvent {
        use alloc::format;

        crate::state::SecurityEvent {
            event_type: crate::state::SecurityEventType::SettingSystemTime,
            tech_info: Some(format!(
                "synced to {} (delta {}s)",
                self.csms_time.to_rfc3339(),
                self.delta_secs
            )),
        }
    }
}

#[cfg(test)]
mod time_sync_tests {
    use super::{CLOCK_STEP_THRESHOLD_SECS, evaluate_time_sync, parse_csms_current_time};
    use chrono::{DateTime, Duration as ChronoDuration, Utc};

    fn dt(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn no_local_estimate_is_always_worth_reporting_as_the_first_sync() {
        let csms_time = dt("2026-06-01T12:00:00Z");

        let step = evaluate_time_sync(None, csms_time).unwrap();

        assert_eq!(step.csms_time, csms_time);
        assert_eq!(step.delta_secs, 0);
    }

    #[test]
    fn an_unsynchronized_local_clock_reports_the_first_real_sync() {
        let unset_rtc = dt("1970-01-01T00:00:00Z");
        let csms_time = dt("2026-06-01T12:00:00Z");

        let step = evaluate_time_sync(Some(unset_rtc), csms_time).unwrap();

        assert_eq!(step.csms_time, csms_time);
        assert!(step.delta_secs > 0);
    }

    #[test]
    fn a_small_difference_between_two_synchronized_clocks_is_routine_and_not_reported() {
        let local = dt("2026-06-01T12:00:00Z");
        let csms_time = local + ChronoDuration::seconds(CLOCK_STEP_THRESHOLD_SECS - 1);

        assert_eq!(evaluate_time_sync(Some(local), csms_time), None);
    }

    #[test]
    fn a_difference_at_or_above_the_threshold_is_a_step_worth_reporting() {
        let local = dt("2026-06-01T12:00:00Z");
        let csms_time = local + ChronoDuration::seconds(CLOCK_STEP_THRESHOLD_SECS);

        let step = evaluate_time_sync(Some(local), csms_time).unwrap();

        assert_eq!(step.delta_secs, CLOCK_STEP_THRESHOLD_SECS);
    }

    #[test]
    fn a_backwards_step_is_also_detected_via_its_absolute_value() {
        let local = dt("2026-06-01T12:00:00Z");
        let csms_time = local - ChronoDuration::seconds(CLOCK_STEP_THRESHOLD_SECS + 5);

        let step = evaluate_time_sync(Some(local), csms_time).unwrap();

        assert_eq!(step.delta_secs, -(CLOCK_STEP_THRESHOLD_SECS + 5));
    }

    #[test]
    fn a_heartbeat_that_repeats_the_same_time_every_cycle_never_re_reports() {
        let local = dt("2026-06-01T12:00:00Z");

        // Same behaviour a CSMS returning `currentTime` on every Heartbeat response would
        // trigger - it must not raise `SettingSystemTime` every cycle.
        for _ in 0..5 {
            assert_eq!(evaluate_time_sync(Some(local), local), None);
        }
    }

    #[test]
    fn a_valid_rfc3339_current_time_parses() {
        assert_eq!(
            parse_csms_current_time("2026-06-01T12:00:00Z"),
            Some(dt("2026-06-01T12:00:00Z"))
        );
    }

    #[test]
    fn a_malformed_current_time_fails_to_parse_rather_than_panicking() {
        assert_eq!(parse_csms_current_time("not-a-timestamp"), None);
    }

    #[test]
    fn a_time_sync_step_builds_a_setting_system_time_security_event() {
        use crate::state::SecurityEventType;

        let csms_time = dt("2026-06-01T12:00:00Z");
        let step = evaluate_time_sync(None, csms_time).unwrap();

        let event = step.to_security_event();

        assert_eq!(event.event_type, SecurityEventType::SettingSystemTime);
        assert!(event.tech_info.unwrap().contains("2026-06-01"));
    }
}

/// The `(Component, Variable)` this crate's device model uses for the heartbeat interval - see
/// [`crate::state::DeviceModel::register_defaults`].
fn heartbeat_interval_variable() -> (Component, Variable) {
    (
        Component {
            name: "OCPPCommCtrlr".into(),
            instance: None,
            evse: None,
        },
        Variable {
            name: "HeartbeatInterval".into(),
            instance: None,
        },
    )
}

/// Reads `actor`'s current `OCPPCommCtrlr`/`HeartbeatInterval` device model value, parsed as
/// whole seconds. `None` if the variable isn't registered, has no `Actual` attribute, its value
/// doesn't parse as a `u32`, or parses to `0` - a `0`-second interval would turn
/// [`run_heartbeat`]'s loop into a busy-spin, so it's treated the same as "not set" rather than
/// honoured. Callers fall back to a caller-supplied default in every one of those cases.
fn heartbeat_interval_secs(actor: &ChargePointActor) -> Option<u32> {
    let (component, variable) = heartbeat_interval_variable();
    let state = actor.state();
    let value = state
        .device_model
        .get(&component, &variable)?
        .attribute(VariableAttributeType::Actual)?
        .value
        .parse::<u32>()
        .ok()?;
    (value != 0).then_some(value)
}

/// Sends a Heartbeat every cycle, forever, honouring `actor`'s current
/// `OCPPCommCtrlr`/`HeartbeatInterval` device model value (read fresh from `actor` on each cycle) on
/// every iteration - so a CSMS that changes it via `SetVariables` (OCPP 2.x) or
/// `ChangeConfiguration` (1.6J, projected onto the same variable - see
/// `crate::device_model::ocpp_1_6`) takes effect on the very next heartbeat, without a reboot.
/// Falls back to `fallback_interval_secs` (the interval the accepted BootNotification returned -
/// see [`register_until_accepted`]) whenever the device model's value is missing, unparseable,
/// or `0`, so a bad `SetVariables` write can never turn this into a busy-spin. Errors sending the
/// heartbeat itself are logged and do not stop the loop - the next heartbeat is still due at the
/// regular interval, per OCPP.
///
/// Every response that carries a parseable `currentTime` is evaluated for a time-sync step
/// exactly as [`register`] does - see that function's docs for what "evaluated" means and what
/// this crate does versus what the integrator's hardware must do. `monotonic` should be the same
/// [`MonotonicClock`] instance used for [`register`]/[`register_until_accepted`] (readings from
/// different instances are not comparable - see [`crate::clock::MonotonicInstant`]), so a
/// reconnect's fresh BootNotification and this loop's heartbeats compare against one shared,
/// ever-freshening anchor rather than each starting from "first sync" independently.
pub async fn run_heartbeat<H: HeartbeatSender, B: Backoff, M: MonotonicClock>(
    sender: &H,
    backoff: &B,
    monotonic: &M,
    actor: &ChargePointActor,
    fallback_interval_secs: u32,
) {
    loop {
        let interval_secs = heartbeat_interval_secs(actor).unwrap_or(fallback_interval_secs);
        backoff.wait(interval_secs).await;
        match sender.send_heartbeat().await {
            Ok(Some(csms_time)) => sync_time(actor, monotonic, csms_time).await,
            Ok(None) => {}
            Err(err) => tracing::warn!(error = %err, "heartbeat failed"),
        }
    }
}

#[cfg(test)]
mod time_sync_wiring_tests {
    use super::{BootNotificationOutcome, BootNotifier, HeartbeatSender, register, run_heartbeat};
    use crate::actor::ChargePointActor;
    use crate::clock::{MonotonicClock, MonotonicInstant};
    use crate::executor::TokioExecutor;
    use crate::state::{BootReasonCause, RegistrationStatus, SecurityEventType};
    use alloc::boxed::Box;
    use chrono::{DateTime, Duration as ChronoDuration, Utc};
    use core::sync::atomic::{AtomicU64, Ordering};
    use core::time::Duration;

    /// A [`MonotonicClock`] whose reading advances only when the test tells it to, so a test can
    /// assert exactly what `local_time_estimate` computed from a stored anchor plus elapsed time.
    #[derive(Default)]
    struct FakeMonotonicClock(AtomicU64);

    impl FakeMonotonicClock {
        fn advance(&self, duration: Duration) {
            self.0
                .fetch_add(duration.as_nanos() as u64, Ordering::SeqCst);
        }
    }

    impl MonotonicClock for FakeMonotonicClock {
        fn now(&self) -> MonotonicInstant {
            MonotonicInstant::from_ticks(self.0.load(Ordering::SeqCst))
        }
    }

    struct FixedTimeBootNotifier(DateTime<Utc>);

    #[async_trait::async_trait]
    impl BootNotifier for FixedTimeBootNotifier {
        type Error = core::convert::Infallible;

        async fn notify_boot(
            &self,
            _vendor_name: &str,
            _model_name: &str,
            _reason: Option<BootReasonCause>,
        ) -> Result<BootNotificationOutcome, Self::Error> {
            Ok(BootNotificationOutcome {
                status: RegistrationStatus::Accepted,
                interval_secs: 60,
                current_time: Some(self.0),
            })
        }
    }

    struct FixedTimeHeartbeatSender(DateTime<Utc>);

    #[async_trait::async_trait]
    impl HeartbeatSender for FixedTimeHeartbeatSender {
        type Error = core::convert::Infallible;

        async fn send_heartbeat(&self) -> Result<Option<DateTime<Utc>>, Self::Error> {
            Ok(Some(self.0))
        }
    }

    fn dt(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[tokio::test]
    async fn a_first_boot_notification_current_time_is_stored_as_the_sync_anchor() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let notifier = FixedTimeBootNotifier(dt("2026-06-01T12:00:00Z"));
        let monotonic = FakeMonotonicClock::default();

        register(&actor, &notifier, &monotonic, "Acme", "Charger 9000", None)
            .await
            .unwrap();

        let anchor = actor.state().time_sync.expect("anchor recorded");
        assert_eq!(anchor.csms_time, dt("2026-06-01T12:00:00Z"));
    }

    #[tokio::test]
    async fn a_first_boot_notification_reports_setting_system_time() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let notifier = FixedTimeBootNotifier(dt("2026-06-01T12:00:00Z"));
        let monotonic = FakeMonotonicClock::default();
        let mut events = actor.subscribe_security_events();

        register(&actor, &notifier, &monotonic, "Acme", "Charger 9000", None)
            .await
            .unwrap();

        let event = events.recv().await.unwrap();
        assert_eq!(event.event_type, SecurityEventType::SettingSystemTime);
    }

    #[tokio::test]
    async fn a_second_sync_consistent_with_elapsed_monotonic_time_does_not_re_report() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let monotonic = FakeMonotonicClock::default();

        register(
            &actor,
            &FixedTimeBootNotifier(dt("2026-06-01T12:00:00Z")),
            &monotonic,
            "Acme",
            "Charger 9000",
            None,
        )
        .await
        .unwrap();

        // 30 real seconds pass, and the second sync's `currentTime` reflects exactly that -
        // routine drift within `CLOCK_STEP_THRESHOLD_SECS`, not a step.
        monotonic.advance(Duration::from_secs(30));
        let mut events = actor.subscribe_security_events();

        register(
            &actor,
            &FixedTimeBootNotifier(dt("2026-06-01T12:00:00Z") + ChronoDuration::seconds(30)),
            &monotonic,
            "Acme",
            "Charger 9000",
            None,
        )
        .await
        .unwrap();

        assert!(
            tokio::time::timeout(Duration::from_millis(20), events.recv())
                .await
                .is_err(),
            "a consistent second sync must not raise SettingSystemTime"
        );
    }

    #[tokio::test]
    async fn a_second_sync_far_from_the_elapsed_monotonic_estimate_reports_a_step() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let monotonic = FakeMonotonicClock::default();

        register(
            &actor,
            &FixedTimeBootNotifier(dt("2026-06-01T12:00:00Z")),
            &monotonic,
            "Acme",
            "Charger 9000",
            None,
        )
        .await
        .unwrap();

        // Only 1 real second passes, but the CSMS's next `currentTime` jumped forward an hour -
        // a genuine step, not routine drift.
        monotonic.advance(Duration::from_secs(1));
        let mut events = actor.subscribe_security_events();

        register(
            &actor,
            &FixedTimeBootNotifier(dt("2026-06-01T12:00:00Z") + ChronoDuration::hours(1)),
            &monotonic,
            "Acme",
            "Charger 9000",
            None,
        )
        .await
        .unwrap();

        let event = events.recv().await.unwrap();
        assert_eq!(event.event_type, SecurityEventType::SettingSystemTime);
    }

    #[tokio::test]
    async fn a_heartbeat_response_current_time_advances_the_sync_anchor() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let checked_actor = actor.clone();

        struct ImmediateBackoff;
        #[async_trait::async_trait]
        impl super::Backoff for ImmediateBackoff {
            async fn wait(&self, _seconds: u32) {
                tokio::task::yield_now().await;
            }
        }

        let handle = tokio::spawn(async move {
            let monotonic = FakeMonotonicClock::default();
            let sender = FixedTimeHeartbeatSender(dt("2026-06-01T12:00:00Z"));
            run_heartbeat(&sender, &ImmediateBackoff, &monotonic, &actor, 60).await;
        });
        // Give the loop a moment to run at least one cycle, then stop it - `run_heartbeat` never
        // returns on its own.
        tokio::time::sleep(Duration::from_millis(10)).await;
        handle.abort();

        let anchor = checked_actor.state().time_sync.expect("anchor recorded");
        assert_eq!(anchor.csms_time, dt("2026-06-01T12:00:00Z"));
    }
}

#[cfg(test)]
mod tests {
    use super::{Backoff, HeartbeatSender, heartbeat_interval_secs, run_heartbeat};
    use crate::actor::ChargePointActor;
    use crate::executor::TokioExecutor;
    use crate::state::{
        ChargePointEvent, Component, DeviceModelEvent, Variable, VariableAttributeType,
    };
    use alloc::boxed::Box;
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use core::time::Duration;
    use std::sync::Mutex;

    struct CountingHeartbeatSender {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl HeartbeatSender for CountingHeartbeatSender {
        type Error = core::convert::Infallible;

        async fn send_heartbeat(
            &self,
        ) -> Result<Option<chrono::DateTime<chrono::Utc>>, Self::Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(None)
        }
    }

    struct NoopBackoff;

    #[async_trait::async_trait]
    impl Backoff for NoopBackoff {
        async fn wait(&self, _seconds: u32) {
            // A real yield point, not a no-op: `run_heartbeat`'s loop never returns, so without
            // this the loop's futures are always immediately `Ready` and the task never
            // suspends - starving the executor and preventing the `timeout` below from ever
            // firing.
            tokio::task::yield_now().await;
        }
    }

    /// A [`Backoff`] that records every `seconds` value it was called with (in order), so a test
    /// can inspect exactly which interval `run_heartbeat` used on each cycle.
    struct RecordingBackoff {
        seen: Arc<Mutex<alloc::vec::Vec<u32>>>,
    }

    #[async_trait::async_trait]
    impl Backoff for RecordingBackoff {
        async fn wait(&self, seconds: u32) {
            self.seen.lock().unwrap().push(seconds);
            tokio::task::yield_now().await;
        }
    }

    fn heartbeat_component_and_variable() -> (Component, Variable) {
        (
            Component {
                name: "OCPPCommCtrlr".into(),
                instance: None,
                evse: None,
            },
            Variable {
                name: "HeartbeatInterval".into(),
                instance: None,
            },
        )
    }

    async fn set_heartbeat_interval(actor: &ChargePointActor, value: &str) {
        let (component, variable) = heartbeat_component_and_variable();
        actor
            .send(ChargePointEvent::DeviceModel(
                DeviceModelEvent::AttributeValueSet {
                    component,
                    variable,
                    attribute_type: VariableAttributeType::Actual,
                    value: value.into(),
                },
            ))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_heartbeat_sends_at_every_interval_and_never_stops() {
        let sender = CountingHeartbeatSender {
            calls: AtomicUsize::new(0),
        };
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let _ = tokio::time::timeout(
            Duration::from_millis(20),
            run_heartbeat(
                &sender,
                &NoopBackoff,
                &crate::clock::SystemMonotonicClock,
                &actor,
                5,
            ),
        )
        .await;

        assert!(sender.calls.load(Ordering::SeqCst) > 1);
    }

    #[tokio::test]
    async fn a_fresh_actor_reports_the_built_in_default_heartbeat_interval() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        assert_eq!(heartbeat_interval_secs(&actor), Some(60));
    }

    #[tokio::test]
    async fn a_set_variables_change_is_reflected_immediately() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        set_heartbeat_interval(&actor, "120").await;

        assert_eq!(heartbeat_interval_secs(&actor), Some(120));
    }

    #[tokio::test]
    async fn an_unparseable_value_falls_back_to_none() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        set_heartbeat_interval(&actor, "not-a-number").await;

        assert_eq!(heartbeat_interval_secs(&actor), None);
    }

    #[tokio::test]
    async fn a_zero_value_falls_back_to_none_rather_than_busy_spinning() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        set_heartbeat_interval(&actor, "0").await;

        assert_eq!(heartbeat_interval_secs(&actor), None);
    }

    #[tokio::test]
    async fn a_missing_actual_attribute_falls_back_to_none() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let (component, variable) = heartbeat_component_and_variable();
        // Re-registers the variable with only a `Target` attribute - no `Actual` at all, the
        // same shape as a hardware binding that never populated it.
        actor
            .send(ChargePointEvent::DeviceModel(
                DeviceModelEvent::VariableRegistered {
                    component,
                    variable,
                    characteristics: crate::state::VariableCharacteristics {
                        data_type: crate::state::VariableDataType::Integer,
                        unit: Some("s".into()),
                        min_limit: None,
                        max_limit: None,
                        values_list: None,
                        supports_monitoring: false,
                    },
                    attributes: alloc::vec![crate::state::VariableAttribute {
                        attribute_type: crate::state::VariableAttributeType::Target,
                        value: "60".into(),
                        mutability: crate::state::VariableMutability::ReadWrite,
                        persistent: true,
                        constant: false,
                        requires_reboot: false,
                    }],
                },
            ))
            .await
            .unwrap();

        assert_eq!(heartbeat_interval_secs(&actor), None);
    }

    #[tokio::test]
    async fn run_heartbeat_picks_up_a_set_variables_change_without_restarting() {
        let sender = CountingHeartbeatSender {
            calls: AtomicUsize::new(0),
        };
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let seen = Arc::new(Mutex::new(alloc::vec::Vec::new()));
        let backoff = RecordingBackoff { seen: seen.clone() };

        let heartbeat_actor = actor.clone();
        let handle = tokio::spawn(async move {
            run_heartbeat(
                &sender,
                &backoff,
                &crate::clock::SystemMonotonicClock,
                &heartbeat_actor,
                999,
            )
            .await;
        });

        tokio::time::sleep(Duration::from_millis(5)).await;
        set_heartbeat_interval(&actor, "5").await;
        tokio::time::sleep(Duration::from_millis(5)).await;
        handle.abort();

        let seen = seen.lock().unwrap();
        assert!(
            seen.contains(&60),
            "expected the built-in default (60s) to have been used before the change: {seen:?}"
        );
        assert!(
            seen.contains(&5),
            "expected the new interval (5s) to have been picked up without restarting: {seen:?}"
        );
    }
}

/// Test-only `BootNotifier`/`HeartbeatSender` fakes shared across this crate's test modules.
#[cfg(test)]
pub(crate) mod test_support {
    use super::{BootNotificationOutcome, BootNotifier, HeartbeatSender};
    use crate::state::BootReasonCause;
    use alloc::boxed::Box;

    /// A `BootNotifier` that always returns the same outcome, regardless of vendor/model.
    #[derive(Clone)]
    pub(crate) struct FixedBootNotifier(pub BootNotificationOutcome);

    #[async_trait::async_trait]
    impl BootNotifier for FixedBootNotifier {
        type Error = core::convert::Infallible;

        async fn notify_boot(
            &self,
            _vendor_name: &str,
            _model_name: &str,
            _reason: Option<BootReasonCause>,
        ) -> Result<BootNotificationOutcome, Self::Error> {
            Ok(self.0)
        }
    }

    #[async_trait::async_trait]
    impl HeartbeatSender for FixedBootNotifier {
        type Error = core::convert::Infallible;

        async fn send_heartbeat(
            &self,
        ) -> Result<Option<chrono::DateTime<chrono::Utc>>, Self::Error> {
            Ok(self.0.current_time)
        }
    }

    #[async_trait::async_trait]
    impl crate::availability::StatusNotifier for FixedBootNotifier {
        type Error = core::convert::Infallible;

        async fn notify_status(
            &self,
            _evse_id: usize,
            _connector_id: usize,
            _status: crate::state::ConnectorStatus,
            _connector_state: crate::state::ConnectorState,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl crate::transactions::TransactionNotifier for FixedBootNotifier {
        type Error = core::convert::Infallible;

        async fn notify_transaction_event(
            &self,
            _evse_id: usize,
            _connector_id: usize,
            _kind: crate::state::TransactionEventKind,
            _transaction: crate::state::Transaction,
            _offline: bool,
        ) -> Result<crate::transactions::TransactionEventOutcome, Self::Error> {
            Ok(crate::transactions::TransactionEventOutcome::acknowledged())
        }
    }

    #[async_trait::async_trait]
    impl crate::authorization::Authorizer for FixedBootNotifier {
        type Error = core::convert::Infallible;

        async fn authorize(
            &self,
            _id_token: &crate::state::IdToken,
        ) -> Result<crate::state::AuthorizationStatus, Self::Error> {
            Ok(crate::state::AuthorizationStatus::Accepted)
        }
    }

    #[async_trait::async_trait]
    impl crate::remote_control::UnlockConnectorHandler for FixedBootNotifier {
        async fn register_unlock_connector_handler(&self, _actor: crate::actor::ChargePointActor) {}
    }

    #[async_trait::async_trait]
    impl crate::availability::ChangeAvailabilityHandler for FixedBootNotifier {
        async fn register_change_availability_handler(
            &self,
            _actor: crate::actor::ChargePointActor,
        ) {
        }
    }

    #[async_trait::async_trait]
    impl crate::remote_control::RequestStartTransactionHandler for FixedBootNotifier {
        async fn register_request_start_transaction_handler(
            &self,
            _actor: crate::actor::ChargePointActor,
        ) {
        }
    }

    #[async_trait::async_trait]
    impl crate::remote_control::RequestStopTransactionHandler for FixedBootNotifier {
        async fn register_request_stop_transaction_handler(
            &self,
            _actor: crate::actor::ChargePointActor,
        ) {
        }
    }

    #[cfg(feature = "reservation")]
    #[async_trait::async_trait]
    impl crate::reservation::ReserveNowHandler for FixedBootNotifier {
        async fn register_reserve_now_handler(&self, _actor: crate::actor::ChargePointActor) {}
    }

    #[cfg(feature = "reservation")]
    #[async_trait::async_trait]
    impl crate::reservation::CancelReservationHandler for FixedBootNotifier {
        async fn register_cancel_reservation_handler(
            &self,
            _actor: crate::actor::ChargePointActor,
        ) {
        }
    }

    #[async_trait::async_trait]
    impl crate::reset::ResetHandler for FixedBootNotifier {
        async fn register_reset_handler(&self, _actor: crate::actor::ChargePointActor) {}
    }

    #[cfg(feature = "local-auth-list")]
    #[async_trait::async_trait]
    impl crate::local_authorization_list::SendLocalListHandler for FixedBootNotifier {
        async fn register_send_local_list_handler(&self, _actor: crate::actor::ChargePointActor) {}
    }

    #[cfg(feature = "local-auth-list")]
    #[async_trait::async_trait]
    impl crate::local_authorization_list::GetLocalListVersionHandler for FixedBootNotifier {
        async fn register_get_local_list_version_handler(
            &self,
            _actor: crate::actor::ChargePointActor,
        ) {
        }
    }

    #[async_trait::async_trait]
    impl crate::device_model::GetVariablesHandler for FixedBootNotifier {
        async fn register_get_variables_handler(&self, _actor: crate::actor::ChargePointActor) {}
    }

    #[async_trait::async_trait]
    impl crate::device_model::SetVariablesHandler for FixedBootNotifier {
        async fn register_set_variables_handler<
            K: crate::hardware::KeyStore + Send + Sync + 'static,
        >(
            &self,
            _actor: crate::actor::ChargePointActor,
            _key_store: K,
        ) {
        }
    }

    #[async_trait::async_trait]
    impl crate::variable_monitoring::SetVariableMonitoringHandler for FixedBootNotifier {
        async fn register_set_variable_monitoring_handler(
            &self,
            _actor: crate::actor::ChargePointActor,
        ) {
        }
    }

    #[async_trait::async_trait]
    impl crate::variable_monitoring::ClearVariableMonitoringHandler for FixedBootNotifier {
        async fn register_clear_variable_monitoring_handler(
            &self,
            _actor: crate::actor::ChargePointActor,
        ) {
        }
    }

    #[async_trait::async_trait]
    impl crate::variable_monitoring::VariableMonitorEventNotifier for FixedBootNotifier {
        type Error = core::convert::Infallible;
        async fn notify_variable_monitor_event(
            &self,
            _event: &crate::state::TriggeredMonitor,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl crate::variable_monitoring::SetMonitoringBaseHandler for FixedBootNotifier {
        async fn register_set_monitoring_base_handler(
            &self,
            _actor: crate::actor::ChargePointActor,
        ) {
        }
    }

    #[async_trait::async_trait]
    impl crate::variable_monitoring::SetMonitoringLevelHandler for FixedBootNotifier {
        async fn register_set_monitoring_level_handler(
            &self,
            _actor: crate::actor::ChargePointActor,
        ) {
        }
    }

    #[async_trait::async_trait]
    impl crate::variable_monitoring::GetMonitoringReportHandler for FixedBootNotifier {
        async fn register_get_monitoring_report_handler(
            &self,
            _actor: crate::actor::ChargePointActor,
        ) {
        }
    }

    #[async_trait::async_trait]
    impl crate::reporting::GetBaseReportHandler for FixedBootNotifier {
        async fn register_get_base_report_handler(&self, _actor: crate::actor::ChargePointActor) {}
    }

    #[async_trait::async_trait]
    impl crate::reporting::GetReportHandler for FixedBootNotifier {
        async fn register_get_report_handler(&self, _actor: crate::actor::ChargePointActor) {}
    }

    #[async_trait::async_trait]
    impl crate::security::SecurityEventNotifier for FixedBootNotifier {
        type Error = core::convert::Infallible;

        async fn notify_security_event(
            &self,
            _event_type: &crate::state::SecurityEventType,
            _tech_info: Option<&str>,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[cfg(feature = "tariff-cost")]
    #[async_trait::async_trait]
    impl crate::cost::CostUpdatedHandler for FixedBootNotifier {
        async fn register_cost_updated_handler(&self, _actor: crate::actor::ChargePointActor) {}
    }

    #[cfg(feature = "tariff-cost")]
    #[async_trait::async_trait]
    impl crate::tariff::SetDefaultTariffHandler for FixedBootNotifier {
        async fn register_set_default_tariff_handler(
            &self,
            _actor: crate::actor::ChargePointActor,
        ) {
        }
    }

    #[cfg(feature = "tariff-cost")]
    #[async_trait::async_trait]
    impl crate::tariff::ChangeTransactionTariffHandler for FixedBootNotifier {
        async fn register_change_transaction_tariff_handler(
            &self,
            _actor: crate::actor::ChargePointActor,
        ) {
        }
    }

    #[cfg(feature = "tariff-cost")]
    #[async_trait::async_trait]
    impl crate::tariff::ClearTariffsHandler for FixedBootNotifier {
        async fn register_clear_tariffs_handler(&self, _actor: crate::actor::ChargePointActor) {}
    }

    #[cfg(feature = "tariff-cost")]
    #[async_trait::async_trait]
    impl crate::tariff::GetTariffsHandler for FixedBootNotifier {
        async fn register_get_tariffs_handler(&self, _actor: crate::actor::ChargePointActor) {}
    }

    #[async_trait::async_trait]
    impl crate::network_profile::SetNetworkProfileHandler for FixedBootNotifier {
        async fn register_set_network_profile_handler(
            &self,
            _actor: crate::actor::ChargePointActor,
        ) {
        }
    }

    #[async_trait::async_trait]
    impl crate::authorization::ClearCacheHandler for FixedBootNotifier {
        async fn register_clear_cache_handler(&self, _actor: crate::actor::ChargePointActor) {}
    }

    #[async_trait::async_trait]
    impl crate::remote_control::TriggerMessageHandler for FixedBootNotifier {
        async fn register_trigger_message_handler(&self, _actor: crate::actor::ChargePointActor) {}
    }

    #[async_trait::async_trait]
    impl crate::meter_values::MeterValuesNotifier for FixedBootNotifier {
        type Error = core::convert::Infallible;

        async fn send_meter_values(
            &self,
            _evse_id: usize,
            _connector_id: usize,
            _sample: crate::state::MeterSample,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl crate::smart_charging::SetChargingProfileHandler for FixedBootNotifier {
        async fn register_set_charging_profile_handler(
            &self,
            _actor: crate::actor::ChargePointActor,
        ) {
        }
    }

    #[async_trait::async_trait]
    impl crate::smart_charging::ClearChargingProfileHandler for FixedBootNotifier {
        async fn register_clear_charging_profile_handler(
            &self,
            _actor: crate::actor::ChargePointActor,
        ) {
        }
    }

    #[async_trait::async_trait]
    impl crate::reservation::ReservationStatusNotifier for FixedBootNotifier {
        type Error = core::convert::Infallible;

        async fn notify_reservation_status(
            &self,
            _update: crate::state::ReservationUpdate,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl crate::smart_charging::GetChargingProfilesHandler for FixedBootNotifier {
        async fn register_get_charging_profiles_handler(
            &self,
            _actor: crate::actor::ChargePointActor,
        ) {
        }
    }

    #[async_trait::async_trait]
    impl crate::smart_charging::GetCompositeScheduleHandler for FixedBootNotifier {
        async fn register_get_composite_schedule_handler(
            &self,
            _actor: crate::actor::ChargePointActor,
            _projection: alloc::sync::Arc<crate::smart_charging::ChargingLimitProjection>,
        ) {
        }
    }

    #[cfg(feature = "periodic-event-stream")]
    #[async_trait::async_trait]
    impl crate::periodic_event_stream::OpenPeriodicEventStreamHandler for FixedBootNotifier {
        async fn register_open_periodic_event_stream_handler(
            &self,
            _actor: crate::actor::ChargePointActor,
        ) {
        }
    }

    #[cfg(feature = "periodic-event-stream")]
    #[async_trait::async_trait]
    impl crate::periodic_event_stream::ClosePeriodicEventStreamHandler for FixedBootNotifier {
        async fn register_close_periodic_event_stream_handler(
            &self,
            _actor: crate::actor::ChargePointActor,
        ) {
        }
    }

    #[cfg(feature = "periodic-event-stream")]
    #[async_trait::async_trait]
    impl crate::periodic_event_stream::AdjustPeriodicEventStreamHandler for FixedBootNotifier {
        async fn register_adjust_periodic_event_stream_handler(
            &self,
            _actor: crate::actor::ChargePointActor,
        ) {
        }
    }

    #[cfg(feature = "periodic-event-stream")]
    #[async_trait::async_trait]
    impl crate::periodic_event_stream::GetPeriodicEventStreamHandler for FixedBootNotifier {
        async fn register_get_periodic_event_stream_handler(
            &self,
            _actor: crate::actor::ChargePointActor,
        ) {
        }
    }

    #[cfg(feature = "periodic-event-stream")]
    #[async_trait::async_trait]
    impl crate::periodic_event_stream::PeriodicEventStreamNotifier for FixedBootNotifier {
        type Error = core::convert::Infallible;

        async fn notify_periodic_event_stream(
            &self,
            _sample: crate::periodic_event_stream::PeriodicStreamSample,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl crate::connection::ReconnectHandler for FixedBootNotifier {
        async fn register_reconnect_handler<F, FF>(&self, _callback: F)
        where
            F: FnMut() -> FF + Send + Sync + 'static,
            FF: core::future::Future<Output = ()> + Send + 'static,
        {
        }
    }
}

#[cfg(feature = "ocpp_2_1")]
mod ocpp_2_1 {
    use super::{BootNotificationOutcome, BootNotifier, HeartbeatSender};
    use crate::state::{BootReasonCause, RegistrationStatus};
    use crate::wire::v21::BootNotificationRequest;
    use crate::wire::v21::HeartbeatRequest;
    use crate::wire::v21::common::{BootReasonEnum, ChargingStation, RegistrationStatusEnum};
    use alloc::boxed::Box;
    use ocpp_client::ClientError;
    use ocpp_client::ocpp_2_1::{OCPP2_1Client, OCPP2_1Error};

    /// This crate's internal [`BootReasonCause`] onto OCPP 2.1's `BootReasonEnum`. `None` -
    /// no persisted cause found for this boot, i.e. an *uncommanded* restart (power cut, watchdog,
    /// crash) - maps to `Unknown` rather than `PowerUp`: this crate cannot currently tell those
    /// apart from a clean power-up without an explicit hardware-supplied signal (out of scope
    /// here - see `docs/ROADMAP.md`), and `Unknown` is the only variant that doesn't overclaim
    /// knowledge it doesn't have. `ApplicationReset`/`LocalReset`/`FirmwareUpdate`/`Watchdog`/
    /// `Triggered` are likewise never produced - nothing in this crate today commands any of
    /// those causes.
    pub(super) fn map_reason(reason: Option<BootReasonCause>) -> BootReasonEnum {
        match reason {
            None => BootReasonEnum::Unknown,
            Some(BootReasonCause::RemoteReset) => BootReasonEnum::RemoteReset,
            Some(BootReasonCause::ScheduledReset) => BootReasonEnum::ScheduledReset,
        }
    }

    pub(super) fn build_request(
        vendor_name: &str,
        model_name: &str,
        reason: Option<BootReasonCause>,
    ) -> BootNotificationRequest {
        BootNotificationRequest {
            charging_station: ChargingStation {
                custom_data: None,
                firmware_version: None,
                // Vendor/model names are integrator-supplied configuration, not runtime
                // hardware readings - a name that doesn't fit the OCPP spec's bounded field
                // is a configuration error, not something to recover from at this boundary.
                model: heapless::String::try_from(model_name)
                    .expect("model name must fit in OCPP's 20-character bound"),
                modem: None,
                serial_number: None,
                vendor_name: heapless::String::try_from(vendor_name)
                    .expect("vendor name must fit in OCPP's 50-character bound"),
            },
            custom_data: None,
            reason: map_reason(reason),
        }
    }

    pub(super) fn map_status(status: RegistrationStatusEnum) -> RegistrationStatus {
        match status {
            RegistrationStatusEnum::Accepted => RegistrationStatus::Accepted,
            RegistrationStatusEnum::Pending => RegistrationStatus::Pending,
            RegistrationStatusEnum::Rejected => RegistrationStatus::Rejected,
        }
    }

    #[async_trait::async_trait]
    impl BootNotifier for OCPP2_1Client {
        type Error = ClientError<OCPP2_1Error>;

        async fn notify_boot(
            &self,
            vendor_name: &str,
            model_name: &str,
            reason: Option<BootReasonCause>,
        ) -> Result<BootNotificationOutcome, Self::Error> {
            let response = self
                .send_boot_notification(build_request(vendor_name, model_name, reason))
                .await?;

            Ok(BootNotificationOutcome {
                status: map_status(response.status),
                interval_secs: response.interval.max(0) as u32,
                current_time: Some(super::parse_csms_current_time_logged(
                    &response.current_time,
                )),
            })
        }
    }

    #[async_trait::async_trait]
    impl HeartbeatSender for OCPP2_1Client {
        type Error = ClientError<OCPP2_1Error>;

        async fn send_heartbeat(
            &self,
        ) -> Result<Option<chrono::DateTime<chrono::Utc>>, Self::Error> {
            let response = self
                .send_heartbeat(HeartbeatRequest { custom_data: None })
                .await?;
            Ok(Some(super::parse_csms_current_time_logged(
                &response.current_time,
            )))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn request_carries_the_vendor_and_model() {
            let request = build_request("Acme", "Charger 9000", None);

            assert_eq!(request.charging_station.vendor_name, "Acme");
            assert_eq!(request.charging_station.model, "Charger 9000");
        }

        #[test]
        fn no_persisted_cause_reports_unknown_rather_than_power_up() {
            assert_eq!(map_reason(None), BootReasonEnum::Unknown);
        }

        #[test]
        fn every_persisted_cause_maps_to_its_matching_wire_reason() {
            assert_eq!(
                map_reason(Some(BootReasonCause::RemoteReset)),
                BootReasonEnum::RemoteReset
            );
            assert_eq!(
                map_reason(Some(BootReasonCause::ScheduledReset)),
                BootReasonEnum::ScheduledReset
            );
        }

        #[test]
        fn every_wire_registration_status_maps_to_our_internal_status() {
            assert_eq!(
                map_status(RegistrationStatusEnum::Accepted),
                RegistrationStatus::Accepted
            );
            assert_eq!(
                map_status(RegistrationStatusEnum::Pending),
                RegistrationStatus::Pending
            );
            assert_eq!(
                map_status(RegistrationStatusEnum::Rejected),
                RegistrationStatus::Rejected
            );
        }
    }
}

/// The OCPP 2.0.1 projection of this functional block: identical wire shape to 2.1's (same
/// `ChargingStation`/`BootReasonEnum`/`RegistrationStatusEnum`, since 2.1's Provisioning block
/// didn't change this part of 2.0.1), just targeting `OCPP2_0_1Client` instead.
#[cfg(feature = "ocpp_2_0_1")]
mod ocpp_2_0_1 {
    use super::{BootNotificationOutcome, BootNotifier, HeartbeatSender};
    use crate::state::RegistrationStatus;
    use crate::wire::v201::BootNotificationRequest;
    use crate::wire::v201::HeartbeatRequest;
    use crate::wire::v201::common::{BootReasonEnum, ChargingStation, RegistrationStatusEnum};
    use alloc::boxed::Box;
    use ocpp_client::ClientError;
    use ocpp_client::ocpp_2_0_1::{OCPP2_0_1Client, OCPP2_0_1Error};

    /// Same mapping as OCPP 2.1's `map_reason` - 2.0.1's `BootReasonEnum` has the identical
    /// variant set - see that function's docs for why absence maps to `Unknown` rather than
    /// `PowerUp`.
    pub(super) fn map_reason(reason: Option<crate::state::BootReasonCause>) -> BootReasonEnum {
        use crate::state::BootReasonCause;
        match reason {
            None => BootReasonEnum::Unknown,
            Some(BootReasonCause::RemoteReset) => BootReasonEnum::RemoteReset,
            Some(BootReasonCause::ScheduledReset) => BootReasonEnum::ScheduledReset,
        }
    }

    pub(super) fn build_request(
        vendor_name: &str,
        model_name: &str,
        reason: Option<crate::state::BootReasonCause>,
    ) -> BootNotificationRequest {
        BootNotificationRequest {
            charging_station: ChargingStation {
                custom_data: None,
                firmware_version: None,
                // Vendor/model names are integrator-supplied configuration, not runtime
                // hardware readings - a name that doesn't fit the OCPP spec's bounded field
                // is a configuration error, not something to recover from at this boundary.
                model: heapless::String::try_from(model_name)
                    .expect("model name must fit in OCPP's 20-character bound"),
                modem: None,
                serial_number: None,
                vendor_name: heapless::String::try_from(vendor_name)
                    .expect("vendor name must fit in OCPP's 50-character bound"),
            },
            custom_data: None,
            reason: map_reason(reason),
        }
    }

    pub(super) fn map_status(status: RegistrationStatusEnum) -> RegistrationStatus {
        match status {
            RegistrationStatusEnum::Accepted => RegistrationStatus::Accepted,
            RegistrationStatusEnum::Pending => RegistrationStatus::Pending,
            RegistrationStatusEnum::Rejected => RegistrationStatus::Rejected,
        }
    }

    #[async_trait::async_trait]
    impl BootNotifier for OCPP2_0_1Client {
        type Error = ClientError<OCPP2_0_1Error>;

        async fn notify_boot(
            &self,
            vendor_name: &str,
            model_name: &str,
            reason: Option<crate::state::BootReasonCause>,
        ) -> Result<BootNotificationOutcome, Self::Error> {
            let response = self
                .send_boot_notification(build_request(vendor_name, model_name, reason))
                .await?;

            Ok(BootNotificationOutcome {
                status: map_status(response.status),
                interval_secs: response.interval.max(0) as u32,
                current_time: Some(super::parse_csms_current_time_logged(
                    &response.current_time,
                )),
            })
        }
    }

    #[async_trait::async_trait]
    impl HeartbeatSender for OCPP2_0_1Client {
        type Error = ClientError<OCPP2_0_1Error>;

        async fn send_heartbeat(
            &self,
        ) -> Result<Option<chrono::DateTime<chrono::Utc>>, Self::Error> {
            let response = self
                .send_heartbeat(HeartbeatRequest { custom_data: None })
                .await?;
            Ok(Some(super::parse_csms_current_time_logged(
                &response.current_time,
            )))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::state::BootReasonCause;

        #[test]
        fn request_carries_the_vendor_and_model() {
            let request = build_request("Acme", "Charger 9000", None);

            assert_eq!(request.charging_station.vendor_name, "Acme");
            assert_eq!(request.charging_station.model, "Charger 9000");
        }

        #[test]
        fn no_persisted_cause_reports_unknown_rather_than_power_up() {
            assert_eq!(map_reason(None), BootReasonEnum::Unknown);
        }

        #[test]
        fn every_persisted_cause_maps_to_its_matching_wire_reason() {
            assert_eq!(
                map_reason(Some(BootReasonCause::RemoteReset)),
                BootReasonEnum::RemoteReset
            );
            assert_eq!(
                map_reason(Some(BootReasonCause::ScheduledReset)),
                BootReasonEnum::ScheduledReset
            );
        }

        #[test]
        fn every_wire_registration_status_maps_to_our_internal_status() {
            assert_eq!(
                map_status(RegistrationStatusEnum::Accepted),
                RegistrationStatus::Accepted
            );
            assert_eq!(
                map_status(RegistrationStatusEnum::Pending),
                RegistrationStatus::Pending
            );
            assert_eq!(
                map_status(RegistrationStatusEnum::Rejected),
                RegistrationStatus::Rejected
            );
        }
    }
}

/// The OCPP 1.6J projection of this functional block: the same internal `RegistrationStatus`/
/// `BootNotificationOutcome`/`HeartbeatSender` model §0's version-adapter goal calls for,
/// targeting `OCPP1_6Client` instead of `OCPP2_1Client`. 1.6J's `BootNotification.req` has no
/// `reason` field (that's a 2.x addition) and flattens `charging_station.vendor_name`/`model`
/// into top-level `charge_point_vendor`/`charge_point_model` - otherwise the same shape and the
/// same three-value registration status, so `map_status` differs only in its wire enum type.
///
/// `notify_boot`'s `reason` parameter (this crate's [`crate::state::BootReasonCause`]) is
/// accepted, for `BootNotifier` trait uniformity across versions, but has nowhere to go on the
/// wire and is silently dropped - this whole workstream (the boot-reason row of E2, and E4.2 in
/// `docs/PRODUCTION-ROADMAP.md`) is a no-op for 1.6J.
#[cfg(feature = "ocpp_1_6")]
mod ocpp_1_6 {
    use super::{BootNotificationOutcome, BootNotifier, HeartbeatSender};
    use crate::state::{BootReasonCause, RegistrationStatus};
    use crate::wire::v16::BootNotificationRequest;
    use crate::wire::v16::HeartbeatRequest;
    use crate::wire::v16::common::BootNotificationResponseStatus;
    use alloc::boxed::Box;
    use ocpp_client::ClientError;
    use ocpp_client::ocpp_1_6::{OCPP1_6Client, OCPP1_6Error};

    pub(super) fn build_request(vendor_name: &str, model_name: &str) -> BootNotificationRequest {
        BootNotificationRequest {
            charge_box_serial_number: None,
            // Vendor/model names are integrator-supplied configuration, not runtime hardware
            // readings - a name that doesn't fit the OCPP spec's bounded field is a
            // configuration error, not something to recover from at this boundary.
            charge_point_model: heapless::String::try_from(model_name)
                .expect("model name must fit in OCPP's 20-character bound"),
            charge_point_serial_number: None,
            charge_point_vendor: heapless::String::try_from(vendor_name)
                .expect("vendor name must fit in OCPP's 20-character bound"),
            firmware_version: None,
            iccid: None,
            imsi: None,
            meter_serial_number: None,
            meter_type: None,
        }
    }

    pub(super) fn map_status(status: BootNotificationResponseStatus) -> RegistrationStatus {
        match status {
            BootNotificationResponseStatus::Accepted => RegistrationStatus::Accepted,
            BootNotificationResponseStatus::Pending => RegistrationStatus::Pending,
            BootNotificationResponseStatus::Rejected => RegistrationStatus::Rejected,
        }
    }

    #[async_trait::async_trait]
    impl BootNotifier for OCPP1_6Client {
        type Error = ClientError<OCPP1_6Error>;

        async fn notify_boot(
            &self,
            vendor_name: &str,
            model_name: &str,
            _reason: Option<BootReasonCause>,
        ) -> Result<BootNotificationOutcome, Self::Error> {
            let response = self
                .send_boot_notification(build_request(vendor_name, model_name))
                .await?;

            Ok(BootNotificationOutcome {
                status: map_status(response.status),
                interval_secs: response.interval.max(0) as u32,
                current_time: Some(super::parse_csms_current_time_logged(
                    &response.current_time,
                )),
            })
        }
    }

    #[async_trait::async_trait]
    impl HeartbeatSender for OCPP1_6Client {
        type Error = ClientError<OCPP1_6Error>;

        async fn send_heartbeat(
            &self,
        ) -> Result<Option<chrono::DateTime<chrono::Utc>>, Self::Error> {
            let response = self.send_heartbeat(HeartbeatRequest {}).await?;
            Ok(Some(super::parse_csms_current_time_logged(
                &response.current_time,
            )))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn request_carries_the_vendor_and_model() {
            let request = build_request("Acme", "Charger 9000");

            assert_eq!(request.charge_point_vendor, "Acme");
            assert_eq!(request.charge_point_model, "Charger 9000");
        }

        #[test]
        fn every_wire_registration_status_maps_to_our_internal_status() {
            assert_eq!(
                map_status(BootNotificationResponseStatus::Accepted),
                RegistrationStatus::Accepted
            );
            assert_eq!(
                map_status(BootNotificationResponseStatus::Pending),
                RegistrationStatus::Pending
            );
            assert_eq!(
                map_status(BootNotificationResponseStatus::Rejected),
                RegistrationStatus::Rejected
            );
        }
    }
}
