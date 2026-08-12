//! Reservation functional block: CSMS-initiated `ReserveNow`/`CancelReservation`, plus the
//! charge point's own `ReservationStatusUpdate` when a reservation ends without being asked to.
//! See `docs/ROADMAP.md` §8 and `docs/PRODUCTION-ROADMAP.md` B8.1.
//!
//! # Reservations end three ways
//!
//! The CSMS cancels one, a cable arrives and honours it, or it ends on its own - expired, or
//! removed because the connector faulted or was made unavailable. Only the last pair is reported:
//! the CSMS already knows about a cancellation it sent, and a cable arriving is the reservation
//! doing its job. [`run_reservation_expiry`] is what makes expiry happen at all; without it a
//! reservation the CSMS expects to lapse would hold its connector forever.

use crate::actor::ChargePointActor;
use crate::clock::Clock;
use crate::state::{
    ChargePointEvent, ChargePointState, ConnectorEvent, ConnectorState, EvseEvent, IdToken,
    Reservation, ReservationId, ReservationUpdate,
};
use alloc::boxed::Box;
use chrono::{DateTime, Utc};

#[cfg(feature = "ocpp_1_6")]
pub use self::ocpp_1_6::Ocpp1_6ReserveNowHandler;

/// Converts a wire `expiryDateTime` (OCPP 2.x)/`expiryDate` (1.6J), as carried by
/// `ReserveNowRequest`, into a [`DateTime<Utc>`] for [`crate::state::Reservation::expires_at`].
///
/// Infallible since `ocpp-types` 0.2.0. This used to parse an RFC 3339 string and treat a
/// malformed value as `None` (never expires) rather than rejecting an otherwise-honorable
/// `ReserveNow`. The value now arrives as an already-validated `OcppTimestamp`, so a malformed
/// one is refused by `ocpp-client`'s decoder and answered with a `CALLERROR` before this crate
/// sees it - the lenient case has no input left to be lenient about.
// Only called from the `ocpp_1_6`/`ocpp_2_0_1`/`ocpp_2_1` adapter modules below, each gated on
// its own feature - unused (and so a `dead_code` warning without this) when none of them are
// compiled in, e.g. `--no-default-features`.
#[cfg_attr(
    not(any(feature = "ocpp_1_6", feature = "ocpp_2_0_1", feature = "ocpp_2_1")),
    allow(dead_code)
)]
fn parse_expiry_date_time(raw: &crate::wire::OcppTimestamp) -> DateTime<Utc> {
    // Returns the value rather than an `Option` since `ocpp-types` 0.2.0: `expiryDateTime` is
    // required on the wire and now arrives as an already-validated `OcppTimestamp`, so there is
    // no malformed case left for the caller to fall back on. `handle_reserve_now` still takes an
    // `Option` because a reservation without an expiry is meaningful; a *malformed* one is not.
    (*raw).into()
}

/// The outcome of a CSMS-initiated `ReserveNow` request, matching OCPP's
/// `ReserveNowStatusEnum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReserveNowOutcome {
    /// The connector was reserved.
    Accepted,
    /// The only connector(s) that could have been reserved are currently faulted.
    Faulted,
    /// The only connector(s) that could have been reserved are currently occupied (a cable is
    /// connected, or a transaction is in progress).
    Occupied,
    /// `evse_id` doesn't address an EVSE on this charge point.
    Rejected,
    /// The only connector(s) that could have been reserved are currently unavailable.
    Unavailable,
}

/// Handles a CSMS-initiated `ReserveNow` request against `actor`: finds an `Available` connector
/// on `evse_id` - or, if `evse_id` is `None`, the first `Available` connector on any EVSE - and
/// reserves it. Rejects if `evse_id` is out of range; otherwise, if no connector is currently
/// `Available`, reports why (in priority order: `Faulted`, `Unavailable`, else `Occupied`) based
/// on the states of the connectors that were considered.
///
/// OCPP's `ReserveNow` addresses at most an EVSE (`evseId` is optional and has no `connectorId`
/// counterpart) - unlike `RequestStartTransaction`/`ChangeAvailability`, there is no way for the
/// CSMS to target one specific connector directly.
///
/// `group_id_token` becomes [`Reservation::group_id_token`]: the group this bay is being held
/// for, from `ReserveNowRequest.groupIdToken` (2.x) or `parentIdTag` (1.6J). It is what lets a
/// fleet reservation be honoured by whichever of its vehicles turns up - see **F01.FR.22** and
/// [`crate::remote_control::handle_request_start_transaction`].
///
/// `expires_at` becomes [`Reservation::expires_at`] directly - callers (the `ocpp_1_6`/
/// `ocpp_2_0_1`/`ocpp_2_1` adapters below) convert it from the wire request via
/// `parse_expiry_date_time` (private to this module). `None` means a reservation that never expires, per
/// [`Reservation`]'s docs; it no longer doubles as "the CSMS sent something unparseable", which
/// the wire types now rule out.
#[tracing::instrument(skip_all, fields(reservation_id = reservation_id.0, evse_id))]
pub async fn handle_reserve_now(
    actor: &ChargePointActor,
    evse_id: Option<usize>,
    reservation_id: ReservationId,
    id_token: IdToken,
    group_id_token: Option<IdToken>,
    expires_at: Option<DateTime<Utc>>,
) -> ReserveNowOutcome {
    let state = actor.state();
    // C5 (docs/PRODUCTION-ROADMAP.md §5.5): the handler is registered whenever the
    // `reservation` Cargo feature is on, but the hardware may still declare the capability
    // runtime-absent - refuse via the same `Rejected` status a wire-level rejection already
    // uses, per `crate::refusal`'s decision table.
    if !crate::refusal::capability_present(&state.capabilities, "ReserveNow") {
        return ReserveNowOutcome::Rejected;
    }
    let Some(considered) = considered_connectors(&state, evse_id) else {
        return ReserveNowOutcome::Rejected;
    };
    if considered.is_empty() {
        return ReserveNowOutcome::Rejected;
    }

    let Some((evse_id, connector_id)) = considered
        .iter()
        .find(|(_, _, connector)| *connector == ConnectorState::Available)
        .map(|(evse_id, connector_id, _)| (*evse_id, *connector_id))
    else {
        return unavailable_outcome(&considered);
    };

    let _ = actor
        .send(ChargePointEvent::Evse {
            evse_id,
            event: EvseEvent::Connector {
                connector_id,
                event: ConnectorEvent::Reserved(Reservation {
                    id: reservation_id,
                    id_token,
                    group_id_token,
                    expires_at,
                }),
            },
        })
        .await;

    ReserveNowOutcome::Accepted
}

/// Every `(evse_id, connector_id, ConnectorState)` that a `ReserveNow` targeting `evse_id` (or,
/// if `None`, the whole charge point) could reserve. `None` if `evse_id` is out of range.
fn considered_connectors(
    state: &ChargePointState,
    evse_id: Option<usize>,
) -> Option<alloc::vec::Vec<(usize, usize, ConnectorState)>> {
    match evse_id {
        Some(evse_id) => {
            let evse = state.evses.get(evse_id)?;
            Some(
                evse.connectors
                    .iter()
                    .enumerate()
                    .map(|(connector_id, connector)| (evse_id, connector_id, *connector))
                    .collect(),
            )
        }
        None => Some(
            state
                .evses
                .iter()
                .enumerate()
                .flat_map(|(evse_id, evse)| {
                    evse.connectors
                        .iter()
                        .enumerate()
                        .map(move |(connector_id, connector)| (evse_id, connector_id, *connector))
                })
                .collect(),
        ),
    }
}

/// Picks the most informative rejection reason when no considered connector is `Available`.
fn unavailable_outcome(considered: &[(usize, usize, ConnectorState)]) -> ReserveNowOutcome {
    if considered.iter().any(|(_, _, connector)| {
        matches!(
            connector,
            ConnectorState::Faulted | ConnectorState::FaultedSafe
        )
    }) {
        ReserveNowOutcome::Faulted
    } else if considered
        .iter()
        .any(|(_, _, connector)| *connector == ConnectorState::Unavailable)
    {
        ReserveNowOutcome::Unavailable
    } else {
        ReserveNowOutcome::Occupied
    }
}

/// Registers this charge point's inbound `ReserveNow` handling with the CSMS connection.
/// Implemented per protocol version (see the `ocpp_2_1` module), mirroring
/// [`crate::remote_control::UnlockConnectorHandler`].
#[async_trait::async_trait]
pub trait ReserveNowHandler {
    /// Registers a `ReserveNow` handler with the CSMS connection that dispatches incoming
    /// requests against `actor`.
    async fn register_reserve_now_handler(&self, actor: ChargePointActor);
}

/// The outcome of a CSMS-initiated `CancelReservation` request, matching OCPP's
/// `CancelReservationStatusEnum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelReservationOutcome {
    /// The reservation was cancelled.
    Accepted,
    /// `reservation_id` doesn't address an active reservation.
    Rejected,
}

/// Handles a CSMS-initiated `CancelReservation` request against `actor`: finds the connector
/// whose active reservation is `reservation_id` and, if found, cancels it. Rejects an unknown
/// `reservation_id`.
#[tracing::instrument(skip_all, fields(reservation_id = reservation_id.0))]
pub async fn handle_cancel_reservation(
    actor: &ChargePointActor,
    reservation_id: ReservationId,
) -> CancelReservationOutcome {
    let state = actor.state();
    // C5 (docs/PRODUCTION-ROADMAP.md §5.5): mirrors the capability check in
    // `handle_reserve_now`.
    if !crate::refusal::capability_present(&state.capabilities, "CancelReservation") {
        return CancelReservationOutcome::Rejected;
    }
    let Some((evse_id, connector_id)) = find_reservation(&state, reservation_id) else {
        tracing::warn!("refusing CancelReservation: no such reservation is held here");
        return CancelReservationOutcome::Rejected;
    };

    let _ = actor
        .send(ChargePointEvent::Evse {
            evse_id,
            event: EvseEvent::Connector {
                connector_id,
                event: ConnectorEvent::ReservationCancelled,
            },
        })
        .await;

    CancelReservationOutcome::Accepted
}

/// The connector (if any) whose active reservation is `reservation_id`.
fn find_reservation(
    state: &ChargePointState,
    reservation_id: ReservationId,
) -> Option<(usize, usize)> {
    state.evses.iter().enumerate().find_map(|(evse_id, evse)| {
        evse.reservations
            .iter()
            .position(|reservation| reservation.as_ref().is_some_and(|r| r.id == reservation_id))
            .map(|connector_id| (evse_id, connector_id))
    })
}

/// Registers this charge point's inbound `CancelReservation` handling with the CSMS connection.
/// Implemented per protocol version (see the `ocpp_2_1` module), mirroring
/// [`ReserveNowHandler`].
#[async_trait::async_trait]
pub trait CancelReservationHandler {
    /// Registers a `CancelReservation` handler with the CSMS connection that dispatches incoming
    /// requests to [`handle_cancel_reservation`] against `actor`.
    async fn register_cancel_reservation_handler(&self, actor: ChargePointActor);
}

/// Reports a reservation that ended without the CSMS asking, via OCPP `ReservationStatusUpdate`.
///
/// 2.x only - 1.6J has no such message, so a 1.6J CSMS learns a reservation ended only from the
/// connector's `StatusNotification` going back to `Available`. That is a real difference between
/// the versions rather than a gap in this crate.
#[async_trait::async_trait]
pub trait ReservationStatusNotifier {
    /// What went wrong reporting the update.
    type Error: core::fmt::Display;

    /// Reports that `update.id` ended for `update.reason`.
    async fn notify_reservation_status(&self, update: ReservationUpdate)
    -> Result<(), Self::Error>;
}

#[async_trait::async_trait]
impl<T: ReservationStatusNotifier + Send + Sync + ?Sized> ReservationStatusNotifier
    for alloc::sync::Arc<T>
{
    type Error = T::Error;

    async fn notify_reservation_status(
        &self,
        update: ReservationUpdate,
    ) -> Result<(), Self::Error> {
        (**self).notify_reservation_status(update).await
    }
}

/// Forwards every reservation that ended on its own to the CSMS, until the actor stops.
///
/// A failed send is logged and dropped rather than queued. That is deliberate and worth stating:
/// a `ReservationStatusUpdate` delivered after an outage tells the CSMS about a reservation that
/// expired an unknown time ago, on a connector whose real state the CSMS has since re-learned
/// from `StatusNotification` - which is queued, ordered, and authoritative. Replaying stale
/// reservation news behind it would be worse than silence.
pub async fn run_reservation_status_updates<N: ReservationStatusNotifier>(
    mut updates: crate::sync::BroadcastReceiver<ReservationUpdate>,
    csms: &N,
) {
    while let Ok(update) = updates.recv().await {
        if let Err(err) = csms.notify_reservation_status(update).await {
            tracing::warn!(
                error = %err,
                reservation_id = update.id.0,
                "failed to report a reservation status update"
            );
        }
    }
}

/// Releases reservations whose `expiryDateTime` has passed, every `interval_secs`.
///
/// Without this a reservation only ends when the CSMS cancels it or a cable arrives, so an
/// expiry the CSMS is counting on never happens and the connector stays held indefinitely - see
/// [`crate::state::Reservation`], which described exactly this gap.
///
/// **Skipped entirely while the clock is unsynchronized** (see [`crate::clock::is_synchronized`]).
/// An unsynchronized reading is typically near the epoch, which would make every future
/// `expiryDateTime` look distant and expire nothing - but relying on that would be relying on
/// which way a broken clock happens to be broken. A charge point that does not know the time
/// cannot know a reservation has expired, and holding a connector too long is recoverable where
/// releasing one that is still valid is not.
pub async fn run_reservation_expiry<C: Clock, B: crate::provisioning::Backoff>(
    actor: &ChargePointActor,
    clock: &C,
    backoff: &B,
    interval_secs: u32,
) {
    loop {
        backoff.wait(interval_secs.max(1)).await;
        let now = clock.now();
        if !crate::clock::is_synchronized(&now) {
            tracing::debug!("skipping the reservation expiry sweep: the clock is not synchronized");
            continue;
        }
        for (evse_id, connector_id) in expired_reservations(&actor.state(), now) {
            tracing::info!(evse_id, connector_id, "releasing an expired reservation");
            let _ = actor
                .send(ChargePointEvent::Evse {
                    evse_id,
                    event: EvseEvent::Connector {
                        connector_id,
                        event: ConnectorEvent::ReservationExpired,
                    },
                })
                .await;
        }
    }
}

/// Every reserved connector whose reservation has an `expires_at` at or before `now`.
///
/// A reservation with no `expires_at` never expires, per [`crate::state::Reservation`]'s docs -
/// an unparseable `expiryDateTime` must not become "expires immediately".
fn expired_reservations(
    state: &crate::state::ChargePointState,
    now: DateTime<Utc>,
) -> alloc::vec::Vec<(usize, usize)> {
    let mut expired = alloc::vec::Vec::new();
    for (evse_id, evse) in state.evses.iter().enumerate() {
        for (connector_id, reservation) in evse.reservations.iter().enumerate() {
            if let Some(reservation) = reservation
                && reservation.expires_at.is_some_and(|expiry| expiry <= now)
            {
                expired.push((evse_id, connector_id));
            }
        }
    }
    expired
}

#[cfg(test)]
mod tests {
    use super::{
        CancelReservationOutcome, ReservationStatusNotifier, ReserveNowOutcome,
        expired_reservations, handle_cancel_reservation, handle_reserve_now,
        parse_expiry_date_time, run_reservation_expiry, run_reservation_status_updates,
    };
    use crate::actor::ChargePointActor;
    use crate::executor::TokioExecutor;
    use crate::hardware::Capabilities;
    use crate::state::{
        ChargePointEvent, ConnectorEvent, ConnectorState, EvseEvent, IdToken, IdTokenKind,
        ReservationId,
    };

    fn test_id_token() -> IdToken {
        IdToken {
            value: "04A224B2".into(),
            kind: IdTokenKind::ISO14443,
        }
    }

    #[test]
    fn a_wire_expiry_converts_to_the_instant_it_names() {
        let wire: crate::wire::OcppTimestamp = "2030-01-01T00:00:00Z".try_into().unwrap();

        assert_eq!(
            parse_expiry_date_time(&wire),
            "2030-01-01T00:00:00Z"
                .parse::<chrono::DateTime<chrono::Utc>>()
                .unwrap(),
        );
    }

    /// Replaces an older test asserting a malformed `expiryDateTime` became `None` (reserve
    /// without an expiry). Since `ocpp-types` 0.2.0 that case cannot reach this module:
    /// `expiryDateTime` is an `OcppTimestamp`, so a malformed value is refused while the CALL is
    /// decoded and answered with a `CALLERROR`. The guard moved rather than disappeared, and
    /// this pins it there.
    #[test]
    fn a_malformed_expiry_is_refused_by_the_decoder_before_it_reaches_this_crate() {
        assert!(crate::wire::OcppTimestamp::parse_rfc3339("not a date").is_err());
    }

    /// An expiry that names the same instant in a different UTC offset is the same reservation
    /// deadline. `OcppTimestamp` compares as an instant, so this is now guaranteed by the type
    /// rather than by every call site remembering to normalise.
    #[test]
    fn an_expiry_written_in_another_offset_names_the_same_deadline() {
        let utc: crate::wire::OcppTimestamp = "2030-01-01T00:00:00Z".try_into().unwrap();
        let offset: crate::wire::OcppTimestamp = "2030-01-01T02:00:00+02:00".try_into().unwrap();

        assert_eq!(
            parse_expiry_date_time(&utc),
            parse_expiry_date_time(&offset)
        );
    }

    /// Spawns an actor with the `reservation` capability declared present - every test in this
    /// module except the C5 capability-off ones below wants a charge point that actually
    /// supports reservations, mirroring what `ChargePointBuilder`/`setup()` would have declared
    /// from the hardware binding's [`crate::hardware::ChargePoint::capabilities`] in a real
    /// deployment.
    async fn spawn_with_reservation<const N: usize>(evses: [usize; N]) -> ChargePointActor {
        let actor = ChargePointActor::spawn(evses, &TokioExecutor);
        actor
            .send(ChargePointEvent::CapabilitiesDeclared(Capabilities {
                reservation: true,
                ..Default::default()
            }))
            .await
            .unwrap();
        actor
    }

    /// A [`crate::clock::Clock`] fixed at a caller-chosen instant, so expiry can be tested
    /// without waiting for one.
    #[derive(Clone)]
    struct FixedClock(chrono::DateTime<chrono::Utc>);

    impl crate::clock::Clock for FixedClock {
        fn now(&self) -> chrono::DateTime<chrono::Utc> {
            self.0
        }
    }

    fn at(rfc3339: &str) -> chrono::DateTime<chrono::Utc> {
        rfc3339.parse().unwrap()
    }

    async fn reserve_until(actor: &ChargePointActor, id: i64, expires_at: Option<&str>) {
        handle_reserve_now(
            actor,
            Some(0),
            ReservationId(id),
            test_id_token(),
            None,
            expires_at.map(at),
        )
        .await;
    }

    #[tokio::test]
    async fn only_a_reservation_past_its_expiry_is_swept() {
        let actor = spawn_with_reservation([2]).await;
        reserve_until(&actor, 1, Some("2030-01-01T00:00:00Z")).await;

        let state = actor.state();
        assert!(
            expired_reservations(&state, at("2029-12-31T23:59:59Z")).is_empty(),
            "a second before expiry is not expired"
        );
        assert_eq!(
            expired_reservations(&state, at("2030-01-01T00:00:00Z")),
            alloc::vec![(0, 0)],
            "OCPP's expiryDateTime is the instant it lapses, not the last valid one"
        );
    }

    #[tokio::test]
    async fn a_reservation_with_no_expiry_is_never_swept() {
        let actor = spawn_with_reservation([1]).await;
        reserve_until(&actor, 1, None).await;

        // An unparseable expiryDateTime becomes `None`, and must not turn into "expires now" -
        // that would let one malformed request cancel a reservation the driver is relying on.
        assert!(expired_reservations(&actor.state(), at("2999-01-01T00:00:00Z")).is_empty());
    }

    #[tokio::test]
    async fn the_sweep_frees_an_expired_connector_and_reports_it() {
        let actor = spawn_with_reservation([1]).await;
        reserve_until(&actor, 7, Some("2020-01-01T00:00:00Z")).await;

        let mut updates = actor.subscribe_reservation_updates();
        let sweeping = {
            let actor = actor.clone();
            tokio::spawn(async move {
                run_reservation_expiry(
                    &actor,
                    &FixedClock(at("2030-01-01T00:00:00Z")),
                    &crate::provisioning::TokioBackoff,
                    1,
                )
                .await;
            })
        };

        let update = tokio::time::timeout(core::time::Duration::from_secs(5), updates.recv())
            .await
            .expect("the sweep never reported anything")
            .unwrap();
        assert_eq!(update.id, ReservationId(7));
        assert_eq!(update.reason, crate::state::ReservationEndReason::Expired);
        assert_eq!(
            actor.state().evses[0].connectors[0],
            ConnectorState::Available
        );
        sweeping.abort();
    }

    #[tokio::test]
    async fn an_unsynchronized_clock_expires_nothing() {
        let actor = spawn_with_reservation([1]).await;
        // Expiry *before* the unsynchronized reading below, so the sweep would release it if the
        // guard were not there. Testing this with an epoch reading and a 2020 expiry would prove
        // nothing: the comparison fails on its own, which is exactly the "relying on which way a
        // broken clock is broken" the guard exists to avoid.
        reserve_until(&actor, 7, Some("2005-01-01T00:00:00Z")).await;

        let sweeping = {
            let actor = actor.clone();
            tokio::spawn(async move {
                // Before `clock::unsynchronized_before`, so this reading is not to be trusted -
                // a charge point that does not know the time cannot know a reservation lapsed.
                run_reservation_expiry(
                    &actor,
                    &FixedClock(at("2010-01-01T00:00:00Z")),
                    &crate::provisioning::TokioBackoff,
                    1,
                )
                .await;
            })
        };
        tokio::time::sleep(core::time::Duration::from_millis(1_500)).await;

        assert_eq!(
            actor.state().evses[0].connectors[0],
            ConnectorState::Reserved
        );
        sweeping.abort();
    }

    struct RecordingNotifier {
        updates:
            alloc::sync::Arc<std::sync::Mutex<alloc::vec::Vec<crate::state::ReservationUpdate>>>,
    }

    #[async_trait::async_trait]
    impl ReservationStatusNotifier for RecordingNotifier {
        type Error = core::convert::Infallible;

        async fn notify_reservation_status(
            &self,
            update: crate::state::ReservationUpdate,
        ) -> Result<(), Self::Error> {
            self.updates.lock().unwrap().push(update);
            Ok(())
        }
    }

    #[tokio::test]
    async fn every_reported_end_reaches_the_csms_but_a_cancellation_does_not() {
        let actor = spawn_with_reservation([2]).await;
        let recorded = alloc::sync::Arc::new(std::sync::Mutex::new(alloc::vec::Vec::new()));
        let notifier = RecordingNotifier {
            updates: recorded.clone(),
        };
        let updates = actor.subscribe_reservation_updates();
        let forwarding = tokio::spawn(async move {
            run_reservation_status_updates(updates, &notifier).await;
        });

        reserve_until(&actor, 1, None).await;
        handle_cancel_reservation(&actor, ReservationId(1)).await;
        reserve_until(&actor, 2, None).await;
        let _ = actor
            .send(ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event: ConnectorEvent::FaultDetected,
                },
            })
            .await;

        for _ in 0..50 {
            if !recorded.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(core::time::Duration::from_millis(10)).await;
        }
        let recorded = recorded.lock().unwrap();
        assert_eq!(
            recorded.len(),
            1,
            "only the fault should have been reported"
        );
        assert_eq!(recorded[0].id, ReservationId(2));
        assert_eq!(
            recorded[0].reason,
            crate::state::ReservationEndReason::Removed
        );
        forwarding.abort();
    }

    #[tokio::test]
    async fn reserving_an_available_connector_on_a_given_evse_succeeds() {
        let actor = spawn_with_reservation([1]).await;

        let outcome = handle_reserve_now(
            &actor,
            Some(0),
            ReservationId(1),
            test_id_token(),
            None,
            None,
        )
        .await;

        assert_eq!(outcome, ReserveNowOutcome::Accepted);
        assert_eq!(
            actor.state().evses[0].connectors[0],
            ConnectorState::Reserved
        );
        assert_eq!(
            actor.state().evses[0].reservations[0]
                .as_ref()
                .and_then(|r| r.expires_at),
            None,
            "no expiry was supplied to the handler"
        );
    }

    #[tokio::test]
    async fn reserving_with_an_expiry_records_it_on_the_reservation() {
        use chrono::{DateTime, Utc};

        let actor = spawn_with_reservation([1]).await;
        let expires_at: DateTime<Utc> = "2030-01-01T00:00:00Z".parse().unwrap();

        let outcome = handle_reserve_now(
            &actor,
            Some(0),
            ReservationId(1),
            test_id_token(),
            None,
            Some(expires_at),
        )
        .await;

        assert_eq!(outcome, ReserveNowOutcome::Accepted);
        assert_eq!(
            actor.state().evses[0].reservations[0]
                .as_ref()
                .and_then(|r| r.expires_at),
            Some(expires_at)
        );
    }

    #[tokio::test]
    async fn no_evse_id_reserves_the_first_available_connector_on_any_evse() {
        let actor = spawn_with_reservation([1, 1]).await;
        actor
            .send(ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event: ConnectorEvent::SetUnavailable,
                },
            })
            .await
            .unwrap();

        let outcome =
            handle_reserve_now(&actor, None, ReservationId(1), test_id_token(), None, None).await;

        assert_eq!(outcome, ReserveNowOutcome::Accepted);
        assert_eq!(
            actor.state().evses[1].connectors[0],
            ConnectorState::Reserved
        );
    }

    #[tokio::test]
    async fn an_unknown_evse_is_rejected() {
        let actor = spawn_with_reservation([1]).await;

        let outcome = handle_reserve_now(
            &actor,
            Some(5),
            ReservationId(1),
            test_id_token(),
            None,
            None,
        )
        .await;

        assert_eq!(outcome, ReserveNowOutcome::Rejected);
    }

    #[tokio::test]
    async fn an_occupied_connector_reports_occupied() {
        let actor = spawn_with_reservation([1]).await;
        actor
            .send(ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event: ConnectorEvent::CableConnected,
                },
            })
            .await
            .unwrap();

        let outcome = handle_reserve_now(
            &actor,
            Some(0),
            ReservationId(1),
            test_id_token(),
            None,
            None,
        )
        .await;

        assert_eq!(outcome, ReserveNowOutcome::Occupied);
    }

    #[tokio::test]
    async fn an_unavailable_connector_reports_unavailable() {
        let actor = spawn_with_reservation([1]).await;
        actor
            .send(ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event: ConnectorEvent::SetUnavailable,
                },
            })
            .await
            .unwrap();

        let outcome = handle_reserve_now(
            &actor,
            Some(0),
            ReservationId(1),
            test_id_token(),
            None,
            None,
        )
        .await;

        assert_eq!(outcome, ReserveNowOutcome::Unavailable);
    }

    #[tokio::test]
    async fn cancelling_a_known_reservation_frees_the_connector() {
        let actor = spawn_with_reservation([1]).await;
        handle_reserve_now(
            &actor,
            Some(0),
            ReservationId(1),
            test_id_token(),
            None,
            None,
        )
        .await;

        let outcome = handle_cancel_reservation(&actor, ReservationId(1)).await;

        assert_eq!(outcome, CancelReservationOutcome::Accepted);
        assert_eq!(
            actor.state().evses[0].connectors[0],
            ConnectorState::Available
        );
    }

    #[tokio::test]
    async fn cancelling_an_unknown_reservation_is_rejected() {
        let actor = spawn_with_reservation([1]).await;

        let outcome = handle_cancel_reservation(&actor, ReservationId(1)).await;

        assert_eq!(outcome, CancelReservationOutcome::Rejected);
    }

    // C5 (docs/PRODUCTION-ROADMAP.md §5.5): with the `reservation` capability runtime-absent
    // (the default - see `Capabilities::default`), both handlers must refuse via the same
    // `Rejected` status a normal CALLRESULT already uses, per `crate::refusal`'s decision table -
    // never let a `NotImplemented`/generic error leak out of a handler that's actually registered.

    #[tokio::test]
    async fn reserve_now_is_rejected_when_the_reservation_capability_is_absent() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let outcome = handle_reserve_now(
            &actor,
            Some(0),
            ReservationId(1),
            test_id_token(),
            None,
            None,
        )
        .await;

        assert_eq!(outcome, ReserveNowOutcome::Rejected);
        assert_eq!(
            actor.state().evses[0].connectors[0],
            ConnectorState::Available,
            "a refused reservation must not mutate connector state"
        );
    }

    #[tokio::test]
    async fn cancel_reservation_is_rejected_when_the_reservation_capability_is_absent() {
        let actor = spawn_with_reservation([1]).await;
        handle_reserve_now(
            &actor,
            Some(0),
            ReservationId(1),
            test_id_token(),
            None,
            None,
        )
        .await;
        // Turn the capability back off - the reservation already made must not become
        // cancellable just because it exists.
        actor
            .send(ChargePointEvent::CapabilitiesDeclared(
                Capabilities::default(),
            ))
            .await
            .unwrap();

        let outcome = handle_cancel_reservation(&actor, ReservationId(1)).await;

        assert_eq!(outcome, CancelReservationOutcome::Rejected);
        assert_eq!(
            actor.state().evses[0].connectors[0],
            ConnectorState::Reserved,
            "a refused cancellation must not mutate connector state"
        );
    }
}

#[cfg(feature = "ocpp_2_1")]
mod ocpp_2_1 {
    use super::{
        CancelReservationHandler, CancelReservationOutcome, ReservationStatusNotifier,
        ReserveNowHandler, ReserveNowOutcome, handle_cancel_reservation, handle_reserve_now,
    };
    use crate::actor::ChargePointActor;
    use crate::state::{
        IdToken, IdTokenKind, ReservationEndReason, ReservationId, ReservationUpdate,
    };
    use crate::wire::v21::common::{
        CancelReservationStatusEnum, ReservationUpdateStatusEnum, ReserveNowStatusEnum,
    };
    use crate::wire::v21::{
        CancelReservationResponse, ReservationStatusUpdateRequest, ReserveNowRequest,
        ReserveNowResponse,
    };
    use alloc::boxed::Box;
    use alloc::string::ToString;
    use ocpp_client::ocpp_2_1::OCPP2_1Client;

    fn map_id_token_kind(kind: &str) -> IdTokenKind {
        match kind {
            "Central" => IdTokenKind::Central,
            "DirectPayment" => IdTokenKind::DirectPayment,
            "eMAID" => IdTokenKind::EMAID,
            "EVCCID" => IdTokenKind::EVCCID,
            "ISO14443" => IdTokenKind::ISO14443,
            "ISO15693" => IdTokenKind::ISO15693,
            "KeyCode" => IdTokenKind::KeyCode,
            "Local" => IdTokenKind::Local,
            "MacAddress" => IdTokenKind::MacAddress,
            "NoAuthorization" => IdTokenKind::NoAuthorization,
            _ => IdTokenKind::Vin,
        }
    }

    fn map_id_token(id_token: &crate::wire::v21::common::IdToken) -> IdToken {
        IdToken {
            value: id_token.id_token.to_string(),
            kind: map_id_token_kind(id_token.r#type.as_str()),
        }
    }

    pub(super) fn map_reserve_now_outcome(outcome: ReserveNowOutcome) -> ReserveNowStatusEnum {
        match outcome {
            ReserveNowOutcome::Accepted => ReserveNowStatusEnum::Accepted,
            ReserveNowOutcome::Faulted => ReserveNowStatusEnum::Faulted,
            ReserveNowOutcome::Occupied => ReserveNowStatusEnum::Occupied,
            ReserveNowOutcome::Rejected => ReserveNowStatusEnum::Rejected,
            ReserveNowOutcome::Unavailable => ReserveNowStatusEnum::Unavailable,
        }
    }

    pub(super) fn map_cancel_reservation_outcome(
        outcome: CancelReservationOutcome,
    ) -> CancelReservationStatusEnum {
        match outcome {
            CancelReservationOutcome::Accepted => CancelReservationStatusEnum::Accepted,
            CancelReservationOutcome::Rejected => CancelReservationStatusEnum::Rejected,
        }
    }

    /// A negative wire `evseId` can't address an EVSE - treated the same as an out-of-range one,
    /// without needing to consult the actor.
    fn parse_evse_id(request: &ReserveNowRequest) -> Result<Option<usize>, ()> {
        match request.evse_id {
            None => Ok(None),
            Some(evse_id) => usize::try_from(evse_id).map(Some).map_err(|_| ()),
        }
    }

    #[async_trait::async_trait]
    impl ReserveNowHandler for OCPP2_1Client {
        async fn register_reserve_now_handler(&self, actor: ChargePointActor) {
            self.on_reserve_now(move |request, _client| {
                let actor = actor.clone();
                async move {
                    let outcome = match parse_evse_id(&request) {
                        Ok(evse_id) => {
                            handle_reserve_now(
                                &actor,
                                evse_id,
                                ReservationId(request.id),
                                map_id_token(&request.id_token),
                                request.group_id_token.as_ref().map(map_id_token),
                                Some(super::parse_expiry_date_time(&request.expiry_date_time)),
                            )
                            .await
                        }
                        Err(()) => ReserveNowOutcome::Rejected,
                    };
                    Ok(ReserveNowResponse {
                        custom_data: None,
                        status: map_reserve_now_outcome(outcome),
                        status_info: None,
                    })
                }
            })
            .await;
        }
    }

    #[async_trait::async_trait]
    impl CancelReservationHandler for OCPP2_1Client {
        async fn register_cancel_reservation_handler(&self, actor: ChargePointActor) {
            self.on_cancel_reservation(move |request, _client| {
                let actor = actor.clone();
                async move {
                    let outcome =
                        handle_cancel_reservation(&actor, ReservationId(request.reservation_id))
                            .await;
                    Ok(CancelReservationResponse {
                        custom_data: None,
                        status: map_cancel_reservation_outcome(outcome),
                        status_info: None,
                    })
                }
            })
            .await;
        }
    }

    /// This crate's end reason onto 2.1's status enum.
    ///
    /// 2.1 adds a third value, `NoTransaction` - the reservation was honoured but no transaction
    /// followed - which this crate never produces: knowing it needs a timer running from the
    /// moment the cable arrives, and OCPP names no duration for it. Nothing here maps to it
    /// rather than something being stretched to fit. See `docs/PRODUCTION-ROADMAP.md` B8.1.
    fn map_end_reason(reason: ReservationEndReason) -> ReservationUpdateStatusEnum {
        match reason {
            ReservationEndReason::Expired => ReservationUpdateStatusEnum::Expired,
            ReservationEndReason::Removed => ReservationUpdateStatusEnum::Removed,
        }
    }

    #[async_trait::async_trait]
    impl ReservationStatusNotifier for OCPP2_1Client {
        type Error = ocpp_client::ClientError<ocpp_client::ocpp_2_1::OCPP2_1Error>;

        async fn notify_reservation_status(
            &self,
            update: ReservationUpdate,
        ) -> Result<(), Self::Error> {
            self.send_reservation_status_update(ReservationStatusUpdateRequest {
                custom_data: None,
                reservation_id: update.id.0,
                reservation_update_status: map_end_reason(update.reason),
            })
            .await
            .map(|_| ())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn every_reserve_now_outcome_maps_to_the_matching_wire_status() {
            assert_eq!(
                map_reserve_now_outcome(ReserveNowOutcome::Accepted),
                ReserveNowStatusEnum::Accepted
            );
            assert_eq!(
                map_reserve_now_outcome(ReserveNowOutcome::Faulted),
                ReserveNowStatusEnum::Faulted
            );
            assert_eq!(
                map_reserve_now_outcome(ReserveNowOutcome::Occupied),
                ReserveNowStatusEnum::Occupied
            );
            assert_eq!(
                map_reserve_now_outcome(ReserveNowOutcome::Rejected),
                ReserveNowStatusEnum::Rejected
            );
            assert_eq!(
                map_reserve_now_outcome(ReserveNowOutcome::Unavailable),
                ReserveNowStatusEnum::Unavailable
            );
        }

        #[test]
        fn every_cancel_reservation_outcome_maps_to_the_matching_wire_status() {
            assert_eq!(
                map_cancel_reservation_outcome(CancelReservationOutcome::Accepted),
                CancelReservationStatusEnum::Accepted
            );
            assert_eq!(
                map_cancel_reservation_outcome(CancelReservationOutcome::Rejected),
                CancelReservationStatusEnum::Rejected
            );
        }

        fn wire_id_token() -> crate::wire::v21::common::IdToken {
            crate::wire::v21::common::IdToken {
                additional_info: None,
                id_token: heapless::String::try_from("04A224B2").unwrap(),
                r#type: heapless::String::try_from("ISO14443").unwrap(),
                custom_data: None,
            }
        }

        #[test]
        fn a_wire_id_token_maps_to_the_internal_representation() {
            let mapped = map_id_token(&wire_id_token());
            assert_eq!(mapped.value, "04A224B2");
            assert_eq!(mapped.kind, IdTokenKind::ISO14443);
        }

        fn request(evse_id: Option<i64>) -> ReserveNowRequest {
            ReserveNowRequest {
                connector_type: None,
                custom_data: None,
                evse_id,
                expiry_date_time: "2026-01-01T00:00:00Z".try_into().unwrap(),
                group_id_token: None,
                id: 1,
                id_token: wire_id_token(),
            }
        }

        #[test]
        fn no_evse_id_parses_to_none() {
            assert_eq!(parse_evse_id(&request(None)), Ok(None));
        }

        #[test]
        fn a_valid_evse_id_parses() {
            assert_eq!(parse_evse_id(&request(Some(1))), Ok(Some(1)));
        }

        #[test]
        fn a_negative_evse_id_fails_to_parse() {
            assert_eq!(parse_evse_id(&request(Some(-1))), Err(()));
        }
    }
}

/// The OCPP 2.0.1 projection - identical `ReserveNowRequest`/`CancelReservationRequest`/
/// `ReserveNowStatusEnum`/`CancelReservationStatusEnum` wire shapes to 2.1's (2.1's
/// `connector_type` field also loosened from an enum to a free-form string, but this crate always
/// sends `None` for it either way), so this is close to a copy of the 2.1 module - **except**
/// `id_token` mapping, which for 2.0.1 goes through the same closed `IdTokenEnum`
/// [`crate::remote_control::ocpp_2_0_1::map_id_token_kind`] already had to handle for
/// `RequestStartTransaction`'s inbound `id_token` - reused directly here instead of a third copy.
#[cfg(feature = "ocpp_2_0_1")]
mod ocpp_2_0_1 {
    use super::{
        CancelReservationHandler, CancelReservationOutcome, ReservationStatusNotifier,
        ReserveNowHandler, ReserveNowOutcome, handle_cancel_reservation, handle_reserve_now,
    };
    use crate::actor::ChargePointActor;
    use crate::remote_control::ocpp_2_0_1::map_id_token_kind;
    use crate::state::{IdToken, ReservationEndReason, ReservationId, ReservationUpdate};
    use crate::wire::v201::common::{
        CancelReservationStatusEnum, ReservationUpdateStatusEnum, ReserveNowStatusEnum,
    };
    use crate::wire::v201::{
        CancelReservationResponse, ReservationStatusUpdateRequest, ReserveNowRequest,
        ReserveNowResponse,
    };
    use alloc::boxed::Box;
    use alloc::string::ToString;
    use ocpp_client::ocpp_2_0_1::OCPP2_0_1Client;

    fn map_id_token(id_token: &crate::wire::v201::common::IdToken) -> IdToken {
        IdToken {
            value: id_token.id_token.to_string(),
            kind: map_id_token_kind(id_token.r#type.clone()),
        }
    }

    pub(super) fn map_reserve_now_outcome(outcome: ReserveNowOutcome) -> ReserveNowStatusEnum {
        match outcome {
            ReserveNowOutcome::Accepted => ReserveNowStatusEnum::Accepted,
            ReserveNowOutcome::Faulted => ReserveNowStatusEnum::Faulted,
            ReserveNowOutcome::Occupied => ReserveNowStatusEnum::Occupied,
            ReserveNowOutcome::Rejected => ReserveNowStatusEnum::Rejected,
            ReserveNowOutcome::Unavailable => ReserveNowStatusEnum::Unavailable,
        }
    }

    pub(super) fn map_cancel_reservation_outcome(
        outcome: CancelReservationOutcome,
    ) -> CancelReservationStatusEnum {
        match outcome {
            CancelReservationOutcome::Accepted => CancelReservationStatusEnum::Accepted,
            CancelReservationOutcome::Rejected => CancelReservationStatusEnum::Rejected,
        }
    }

    fn parse_evse_id(request: &ReserveNowRequest) -> Result<Option<usize>, ()> {
        match request.evse_id {
            None => Ok(None),
            Some(evse_id) => usize::try_from(evse_id).map(Some).map_err(|_| ()),
        }
    }

    #[async_trait::async_trait]
    impl ReserveNowHandler for OCPP2_0_1Client {
        async fn register_reserve_now_handler(&self, actor: ChargePointActor) {
            self.on_reserve_now(move |request, _client| {
                let actor = actor.clone();
                async move {
                    let outcome = match parse_evse_id(&request) {
                        Ok(evse_id) => {
                            handle_reserve_now(
                                &actor,
                                evse_id,
                                ReservationId(request.id),
                                map_id_token(&request.id_token),
                                request.group_id_token.as_ref().map(map_id_token),
                                Some(super::parse_expiry_date_time(&request.expiry_date_time)),
                            )
                            .await
                        }
                        Err(()) => ReserveNowOutcome::Rejected,
                    };
                    Ok(ReserveNowResponse {
                        custom_data: None,
                        status: map_reserve_now_outcome(outcome),
                        status_info: None,
                    })
                }
            })
            .await;
        }
    }

    #[async_trait::async_trait]
    impl CancelReservationHandler for OCPP2_0_1Client {
        async fn register_cancel_reservation_handler(&self, actor: ChargePointActor) {
            self.on_cancel_reservation(move |request, _client| {
                let actor = actor.clone();
                async move {
                    let outcome =
                        handle_cancel_reservation(&actor, ReservationId(request.reservation_id))
                            .await;
                    Ok(CancelReservationResponse {
                        custom_data: None,
                        status: map_cancel_reservation_outcome(outcome),
                        status_info: None,
                    })
                }
            })
            .await;
        }
    }

    /// This crate's end reason onto 2.0.1's status enum. 2.0.1 has only these two - it is 2.1
    /// that added `NoTransaction` - so nothing is lost here.
    fn map_end_reason(reason: ReservationEndReason) -> ReservationUpdateStatusEnum {
        match reason {
            ReservationEndReason::Expired => ReservationUpdateStatusEnum::Expired,
            ReservationEndReason::Removed => ReservationUpdateStatusEnum::Removed,
        }
    }

    #[async_trait::async_trait]
    impl ReservationStatusNotifier for OCPP2_0_1Client {
        type Error = ocpp_client::ClientError<ocpp_client::ocpp_2_0_1::OCPP2_0_1Error>;

        async fn notify_reservation_status(
            &self,
            update: ReservationUpdate,
        ) -> Result<(), Self::Error> {
            self.send_reservation_status_update(ReservationStatusUpdateRequest {
                custom_data: None,
                reservation_id: update.id.0,
                reservation_update_status: map_end_reason(update.reason),
            })
            .await
            .map(|_| ())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn every_reserve_now_outcome_maps_to_the_matching_wire_status() {
            assert_eq!(
                map_reserve_now_outcome(ReserveNowOutcome::Accepted),
                ReserveNowStatusEnum::Accepted
            );
            assert_eq!(
                map_reserve_now_outcome(ReserveNowOutcome::Faulted),
                ReserveNowStatusEnum::Faulted
            );
            assert_eq!(
                map_reserve_now_outcome(ReserveNowOutcome::Occupied),
                ReserveNowStatusEnum::Occupied
            );
            assert_eq!(
                map_reserve_now_outcome(ReserveNowOutcome::Rejected),
                ReserveNowStatusEnum::Rejected
            );
            assert_eq!(
                map_reserve_now_outcome(ReserveNowOutcome::Unavailable),
                ReserveNowStatusEnum::Unavailable
            );
        }

        #[test]
        fn every_cancel_reservation_outcome_maps_to_the_matching_wire_status() {
            assert_eq!(
                map_cancel_reservation_outcome(CancelReservationOutcome::Accepted),
                CancelReservationStatusEnum::Accepted
            );
            assert_eq!(
                map_cancel_reservation_outcome(CancelReservationOutcome::Rejected),
                CancelReservationStatusEnum::Rejected
            );
        }

        fn wire_id_token() -> crate::wire::v201::common::IdToken {
            crate::wire::v201::common::IdToken {
                additional_info: None,
                id_token: heapless::String::try_from("04A224B2").unwrap(),
                r#type: crate::wire::v201::common::IdTokenEnum::ISO14443,
                custom_data: None,
            }
        }

        #[test]
        fn a_wire_id_token_maps_to_the_internal_representation() {
            let mapped = map_id_token(&wire_id_token());
            assert_eq!(mapped.value, "04A224B2");
            assert_eq!(mapped.kind, crate::state::IdTokenKind::ISO14443);
        }

        fn request(evse_id: Option<i64>) -> ReserveNowRequest {
            ReserveNowRequest {
                connector_type: None,
                custom_data: None,
                evse_id,
                expiry_date_time: "2026-01-01T00:00:00Z".try_into().unwrap(),
                group_id_token: None,
                id: 1,
                id_token: wire_id_token(),
            }
        }

        #[test]
        fn no_evse_id_parses_to_none() {
            assert_eq!(parse_evse_id(&request(None)), Ok(None));
        }

        #[test]
        fn a_valid_evse_id_parses() {
            assert_eq!(parse_evse_id(&request(Some(1))), Ok(Some(1)));
        }

        #[test]
        fn a_negative_evse_id_fails_to_parse() {
            assert_eq!(parse_evse_id(&request(Some(-1))), Err(()));
        }
    }
}

/// The OCPP 1.6J projection of `ReserveNowHandler`/`CancelReservationHandler`. `CancelReservation`
/// is the simple case - `CancelReservationRequest.reservationId` is a bare `i64`, same as
/// [`crate::state::ReservationId`], so `CancelReservationHandler` is implemented directly on
/// `OCPP1_6Client` with no wrapper needed (mirroring `RequestStopTransactionHandler`'s 1.6J
/// adapter in `crate::remote_control`).
///
/// `ReserveNow` needs the topology-aware translation every inbound 1.6J handler with a flat
/// `connectorId` needs, via [`crate::topology::unflatten_ocpp_1_6_connector_id`] - so
/// [`Ocpp1_6ReserveNowHandler`] wraps `OCPP1_6Client` with that topology, the same way
/// `crate::remote_control::Ocpp1_6RemoteControlHandler` does. Unlike `UnlockConnector`'s mandatory
/// specific connector, 1.6J's `ReserveNowRequest.connectorId` is `0` to mean "the Charge Point may
/// choose any connector" (matching `evseId: None`'s "search every EVSE" in later versions) or a
/// specific flat connector otherwise; [`handle_reserve_now`] only targets at EVSE granularity
/// itself (picking the first `Available` connector on it), so - the same reduction
/// `crate::remote_control`'s `RemoteStartTransaction` adapter makes - a specific `connectorId` is
/// unflattened down to its EVSE half and the specific connector within it is dropped.
/// `ReserveNowRequest.idTag` has no type/kind metadata (see `crate::id_tag`), so
/// [`crate::id_tag::map_id_token`] fills in `IdTokenKind::Central`. 1.6J's `ReserveNowResponseStatus`
/// already matches [`ReserveNowOutcome`] exactly (`Accepted`/`Faulted`/`Occupied`/`Rejected`/
/// `Unavailable`), so no narrowing is needed there.
#[cfg(feature = "ocpp_1_6")]
mod ocpp_1_6 {
    use super::{
        CancelReservationHandler, CancelReservationOutcome, ReserveNowHandler, ReserveNowOutcome,
        handle_cancel_reservation, handle_reserve_now,
    };
    use crate::actor::ChargePointActor;
    use crate::id_tag::map_id_token;
    use crate::state::ReservationId;
    use crate::topology::unflatten_ocpp_1_6_connector_id;
    use crate::wire::v16::common::{CancelReservationResponseStatus, ReserveNowResponseStatus};
    use crate::wire::v16::{CancelReservationResponse, ReserveNowRequest, ReserveNowResponse};
    use alloc::boxed::Box;
    use alloc::vec::Vec;
    use ocpp_client::ocpp_1_6::OCPP1_6Client;

    pub(super) fn map_reserve_now_outcome(outcome: ReserveNowOutcome) -> ReserveNowResponseStatus {
        match outcome {
            ReserveNowOutcome::Accepted => ReserveNowResponseStatus::Accepted,
            ReserveNowOutcome::Faulted => ReserveNowResponseStatus::Faulted,
            ReserveNowOutcome::Occupied => ReserveNowResponseStatus::Occupied,
            ReserveNowOutcome::Rejected => ReserveNowResponseStatus::Rejected,
            ReserveNowOutcome::Unavailable => ReserveNowResponseStatus::Unavailable,
        }
    }

    pub(super) fn map_cancel_reservation_outcome(
        outcome: CancelReservationOutcome,
    ) -> CancelReservationResponseStatus {
        match outcome {
            CancelReservationOutcome::Accepted => CancelReservationResponseStatus::Accepted,
            CancelReservationOutcome::Rejected => CancelReservationResponseStatus::Rejected,
        }
    }

    /// `Ok(None)` means "the Charge Point may choose any connector" (`connectorId == 0`);
    /// `Ok(Some(evse_id))` means the request's `connectorId` resolved to that EVSE; `Err(())`
    /// means it didn't address a real connector under `connector_counts` and the request must be
    /// rejected outright.
    pub(super) fn parse_evse_id(
        connector_counts: &[usize],
        request: &ReserveNowRequest,
    ) -> Result<Option<usize>, ()> {
        if request.connector_id == 0 {
            return Ok(None);
        }
        unflatten_ocpp_1_6_connector_id(connector_counts, request.connector_id)
            .map(|(evse_id, _)| Some(evse_id))
            .ok_or(())
    }

    /// Wraps an `OCPP1_6Client` with the charge point's connector topology, needed to translate
    /// `ReserveNowRequest`'s flat `connectorId` into this crate's `(evse_id, connector_id)`
    /// addressing - see this module's docs.
    pub struct Ocpp1_6ReserveNowHandler {
        client: OCPP1_6Client,
        connector_counts: Vec<usize>,
    }

    impl Ocpp1_6ReserveNowHandler {
        /// Wraps `client`, capturing `connector_counts` (each EVSE's connector count, in
        /// `evse_id` order) for translating connector addresses on every request.
        pub fn new(
            client: OCPP1_6Client,
            connector_counts: impl IntoIterator<Item = usize>,
        ) -> Self {
            Self {
                client,
                connector_counts: connector_counts.into_iter().collect(),
            }
        }
    }

    #[async_trait::async_trait]
    impl ReserveNowHandler for Ocpp1_6ReserveNowHandler {
        async fn register_reserve_now_handler(&self, actor: ChargePointActor) {
            let connector_counts = self.connector_counts.clone();
            self.client
                .on_reserve_now(move |request, _client| {
                    let actor = actor.clone();
                    let connector_counts = connector_counts.clone();
                    async move {
                        let outcome = match parse_evse_id(&connector_counts, &request) {
                            Ok(evse_id) => {
                                handle_reserve_now(
                                    &actor,
                                    evse_id,
                                    ReservationId(request.reservation_id),
                                    map_id_token(&request.id_tag),
                                    // 1.6J's `parentIdTag` is the group id under another name -
                                    // it is exactly the field a fleet reservation is booked
                                    // under, so it maps onto `groupIdToken` (F01.FR.22).
                                    request.parent_id_tag.as_ref().map(|parent| {
                                        crate::state::IdToken {
                                            value: parent.as_str().into(),
                                            kind: crate::state::IdTokenKind::Central,
                                        }
                                    }),
                                    Some(super::parse_expiry_date_time(&request.expiry_date)),
                                )
                                .await
                            }
                            Err(()) => ReserveNowOutcome::Rejected,
                        };
                        Ok(ReserveNowResponse {
                            status: map_reserve_now_outcome(outcome),
                        })
                    }
                })
                .await;
        }
    }

    /// Delegates to the wrapped client. `CancelReservation` addresses a reservation id, not a
    /// connector, so 1.6J needs no topology for it - but
    /// [`ChargePointBuilder::reservation`](crate::builder::ChargePointBuilder::reservation)
    /// registers both reservation handlers together, so the wrapper implements this one too
    /// rather than making a 1.6J caller pass two types for one functional block.
    #[async_trait::async_trait]
    impl CancelReservationHandler for Ocpp1_6ReserveNowHandler {
        async fn register_cancel_reservation_handler(&self, actor: ChargePointActor) {
            CancelReservationHandler::register_cancel_reservation_handler(&self.client, actor)
                .await;
        }
    }

    #[async_trait::async_trait]
    impl CancelReservationHandler for OCPP1_6Client {
        async fn register_cancel_reservation_handler(&self, actor: ChargePointActor) {
            self.on_cancel_reservation(move |request, _client| {
                let actor = actor.clone();
                async move {
                    let outcome =
                        handle_cancel_reservation(&actor, ReservationId(request.reservation_id))
                            .await;
                    Ok(CancelReservationResponse {
                        status: map_cancel_reservation_outcome(outcome),
                    })
                }
            })
            .await;
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn every_reserve_now_outcome_maps_to_a_wire_status() {
            assert_eq!(
                map_reserve_now_outcome(ReserveNowOutcome::Accepted),
                ReserveNowResponseStatus::Accepted
            );
            assert_eq!(
                map_reserve_now_outcome(ReserveNowOutcome::Faulted),
                ReserveNowResponseStatus::Faulted
            );
            assert_eq!(
                map_reserve_now_outcome(ReserveNowOutcome::Occupied),
                ReserveNowResponseStatus::Occupied
            );
            assert_eq!(
                map_reserve_now_outcome(ReserveNowOutcome::Rejected),
                ReserveNowResponseStatus::Rejected
            );
            assert_eq!(
                map_reserve_now_outcome(ReserveNowOutcome::Unavailable),
                ReserveNowResponseStatus::Unavailable
            );
        }

        #[test]
        fn every_cancel_reservation_outcome_maps_to_a_wire_status() {
            assert_eq!(
                map_cancel_reservation_outcome(CancelReservationOutcome::Accepted),
                CancelReservationResponseStatus::Accepted
            );
            assert_eq!(
                map_cancel_reservation_outcome(CancelReservationOutcome::Rejected),
                CancelReservationResponseStatus::Rejected
            );
        }

        fn request(connector_id: i64) -> ReserveNowRequest {
            ReserveNowRequest {
                connector_id,
                expiry_date: "2030-01-01T00:00:00Z".try_into().unwrap(),
                id_tag: crate::wire::v16::IdTag::try_from("04A224B2").unwrap(),
                parent_id_tag: None,
                reservation_id: 1,
            }
        }

        #[test]
        fn connector_id_zero_means_the_charge_point_may_choose() {
            let connector_counts = [1, 1];

            assert_eq!(parse_evse_id(&connector_counts, &request(0)), Ok(None));
        }

        #[test]
        fn a_specific_connector_id_resolves_to_its_evse() {
            let connector_counts = [2, 1];

            assert_eq!(parse_evse_id(&connector_counts, &request(3)), Ok(Some(1)));
        }

        #[test]
        fn an_out_of_range_connector_id_is_rejected() {
            let connector_counts = [1, 1];

            assert_eq!(parse_evse_id(&connector_counts, &request(5)), Err(()));
        }

        #[test]
        fn ocpp1_6_reserve_now_handler_implements_the_handler_trait() {
            fn assert_impl<T: ReserveNowHandler>() {}
            assert_impl::<Ocpp1_6ReserveNowHandler>();
            fn assert_cancel_impl<T: CancelReservationHandler>() {}
            assert_cancel_impl::<OCPP1_6Client>();
        }
    }
}
