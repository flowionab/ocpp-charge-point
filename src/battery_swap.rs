//! Battery Swap functional block (`docs/PRODUCTION-ROADMAP.md` B8.3): `RequestBatterySwap`
//! (CSMS-initiated, answered here) and `BatterySwap` (charge-point-reported, sent here).
//!
//! **2.1 only** - battery swap does not exist before 2.1, so there is no 1.6J/2.0.1 projection to
//! write.
//!
//! See [`crate::state::battery_swap`] for what state this crate keeps between the two messages
//! and why, and [`crate::hardware::BatterySwapStation`] for the one hardware hook this block
//! needs.
//!
//! # Niche hardware, deliberately isolated
//!
//! A battery-swap station is a different kind of charge point - bays that hand out a
//! pre-charged battery rather than connectors that deliver current - so this whole block sits
//! behind the `battery-swap` Cargo feature *and* the `battery_swap` runtime capability
//! ([`crate::hardware::Capabilities::battery_swap`]):
//!
//! - A normal charge point compiles the `battery-swap` feature out entirely, and a CSMS sending
//!   `RequestBatterySwap` to it gets `ocpp-client`'s generic `NotImplemented` CALLERROR, because
//!   no handler for the action was ever registered.
//! - A station with the feature compiled in but no swap hardware wired up (or the capability
//!   left `false`) refuses `RequestBatterySwap` in the protocol-correct shape - C5,
//!   [`crate::refusal`] - rather than accepting a request nothing will ever act on.

use crate::actor::ChargePointActor;
use crate::hardware::BatterySwapStation;
use crate::state::{
    BatterySwapEvent, BatterySwapRequestId, ChargePointEvent, IdToken, PendingBatterySwap,
};
use crate::sync::BroadcastReceiver;

/// The outcome of a CSMS-initiated `RequestBatterySwap`, matching OCPP's `GenericStatusEnum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestBatterySwapOutcome {
    /// The request was recorded and the station successfully prepared for it.
    Accepted,
    /// The `battery_swap` capability is runtime-absent, the pending-swap store is already at its
    /// configured maximum, or the station's own hardware preparation failed.
    Rejected,
}

/// Handles a CSMS-initiated `RequestBatterySwap` against `actor`: records it as a
/// [`PendingBatterySwap`] (so a later correlated `BatterySwap` event can resolve it - see
/// [`crate::state::battery_swap`]'s module docs) and asks `station` to physically prepare for it.
///
/// Refuses (C5, [`crate::refusal`]) before touching the store or the hardware when the
/// `battery_swap` capability is runtime-absent. A station preparation failure also refuses and
/// rolls the store entry back - per `CLAUDE.md`, every hardware call is fallible, and a swap
/// this crate cannot actually help the driver start should not be accepted just because the
/// bookkeeping succeeded, nor should the store keep an entry the CSMS was just told is rejected.
pub async fn handle_request_battery_swap<S: BatterySwapStation>(
    actor: &ChargePointActor,
    station: &S,
    request_id: BatterySwapRequestId,
    id_token: IdToken,
) -> RequestBatterySwapOutcome {
    let state = actor.state();
    if !crate::refusal::capability_present(&state.capabilities, "RequestBatterySwap") {
        return RequestBatterySwapOutcome::Rejected;
    }

    let _ = actor
        .send(ChargePointEvent::BatterySwapRequested(PendingBatterySwap {
            request_id,
            id_token: id_token.clone(),
        }))
        .await;
    if actor.state().battery_swaps.get(request_id).is_none() {
        // The store refused it - the only remaining reason is the bound (see
        // `crate::state::BatterySwapStore::insert`'s docs).
        return RequestBatterySwapOutcome::Rejected;
    }

    if let Err(err) = station.prepare_swap(request_id, &id_token).await {
        tracing::warn!(
            error = %err,
            "battery-swap station failed to prepare for a CSMS-requested swap"
        );
        let _ = actor
            .send(ChargePointEvent::BatterySwapCancelled(request_id))
            .await;
        return RequestBatterySwapOutcome::Rejected;
    }

    RequestBatterySwapOutcome::Accepted
}

/// Registers this charge point's inbound `RequestBatterySwap` handling with the CSMS connection.
/// Implemented per protocol version (see the `ocpp_2_1` module).
#[async_trait::async_trait]
pub trait RequestBatterySwapHandler {
    /// Registers a `RequestBatterySwap` handler with the CSMS connection that dispatches incoming
    /// requests against `actor` and `station`.
    async fn register_request_battery_swap_handler<S>(&self, actor: ChargePointActor, station: S)
    where
        S: BatterySwapStation + Send + Sync + 'static;
}

/// Reports a battery-swap lifecycle event to the CSMS via `BatterySwap`. The one entry point for
/// raising one - hardware should call this directly as it detects a battery going in or out,
/// rather than constructing a [`ChargePointEvent::BatterySwapReported`] by hand. Correlating with
/// a prior `RequestBatterySwap` (if any) happens automatically, by `event.request_id`; a
/// driver-initiated swap the CSMS never asked for is equally valid and simply correlates with
/// nothing.
pub async fn report_battery_swap_event(actor: &ChargePointActor, event: BatterySwapEvent) {
    let _ = actor
        .send(ChargePointEvent::BatterySwapReported(event))
        .await;
}

/// Reports a battery-swap event to the CSMS via `BatterySwap`. Implemented per protocol version
/// (see the `ocpp_2_1` module), mirroring [`crate::security::SecurityEventNotifier`].
#[async_trait::async_trait]
pub trait BatterySwapNotifier {
    /// What went wrong reporting the event.
    type Error: core::fmt::Display;

    /// Reports `event` to the CSMS.
    async fn notify_battery_swap(&self, event: &BatterySwapEvent) -> Result<(), Self::Error>;
}

/// Forwards every reported battery-swap event to the CSMS via `notifier`, until the actor stops.
///
/// A failed send is logged and dropped rather than queued, mirroring
/// [`crate::reservation::run_reservation_status_updates`]'s reasoning: a `BatterySwap` delivered
/// long after the fact tells the CSMS about a swap it can no longer act on, and the driver has
/// long since moved on.
pub async fn run_battery_swap_events<N: BatterySwapNotifier>(
    mut events: BroadcastReceiver<BatterySwapEvent>,
    notifier: &N,
) {
    while let Ok(event) = events.recv().await {
        if let Err(err) = notifier.notify_battery_swap(&event).await {
            tracing::warn!(
                error = %err,
                request_id = event.request_id.0,
                "battery-swap event notification failed"
            );
        }
    }
}

#[cfg(feature = "ocpp_2_1")]
mod ocpp_2_1;

#[cfg(test)]
mod tests {
    use super::{
        BatterySwapNotifier, RequestBatterySwapOutcome, handle_request_battery_swap,
        report_battery_swap_event, run_battery_swap_events,
    };
    use crate::actor::ChargePointActor;
    use crate::executor::TokioExecutor;
    use crate::hardware::{
        BatterySwapStation, Capabilities, NoBatterySwapStation, NoBatterySwapStationError,
    };
    use crate::state::{
        BatteryData, BatterySwapEvent, BatterySwapEventKind, BatterySwapRequestId,
        ChargePointEvent, IdToken, IdTokenKind,
    };
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicBool, Ordering};

    fn test_id_token() -> IdToken {
        IdToken {
            value: "04A224B2".into(),
            kind: IdTokenKind::ISO14443,
        }
    }

    /// A [`BatterySwapStation`] that always succeeds, and records whether it was asked to.
    #[derive(Clone, Default)]
    struct RecordingStation {
        prepared: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl BatterySwapStation for RecordingStation {
        type Error = NoBatterySwapStationError;

        async fn prepare_swap(
            &self,
            _request_id: BatterySwapRequestId,
            _id_token: &IdToken,
        ) -> Result<(), Self::Error> {
            self.prepared.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    async fn spawn_with_battery_swap<const N: usize>(evses: [usize; N]) -> ChargePointActor {
        let actor = ChargePointActor::spawn(evses, &TokioExecutor);
        actor
            .send(ChargePointEvent::CapabilitiesDeclared(Capabilities {
                battery_swap: true,
                ..Default::default()
            }))
            .await
            .unwrap();
        actor
    }

    #[tokio::test]
    async fn an_absent_capability_refuses_without_touching_hardware() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let station = RecordingStation::default();

        let outcome =
            handle_request_battery_swap(&actor, &station, BatterySwapRequestId(1), test_id_token())
                .await;

        assert_eq!(outcome, RequestBatterySwapOutcome::Rejected);
        assert!(!station.prepared.load(Ordering::SeqCst));
        assert!(actor.state().battery_swaps.is_empty());
    }

    #[tokio::test]
    async fn an_accepted_request_is_recorded_and_prepares_the_station() {
        let actor = spawn_with_battery_swap([1]).await;
        let station = RecordingStation::default();

        let outcome =
            handle_request_battery_swap(&actor, &station, BatterySwapRequestId(7), test_id_token())
                .await;

        assert_eq!(outcome, RequestBatterySwapOutcome::Accepted);
        assert!(station.prepared.load(Ordering::SeqCst));
        assert!(
            actor
                .state()
                .battery_swaps
                .get(BatterySwapRequestId(7))
                .is_some()
        );
    }

    #[tokio::test]
    async fn a_station_that_fails_to_prepare_rejects_and_rolls_back_the_store() {
        let actor = spawn_with_battery_swap([1]).await;
        let station = NoBatterySwapStation;

        let outcome =
            handle_request_battery_swap(&actor, &station, BatterySwapRequestId(3), test_id_token())
                .await;

        assert_eq!(outcome, RequestBatterySwapOutcome::Rejected);
        assert!(actor.state().battery_swaps.is_empty());
    }

    #[tokio::test]
    async fn a_pending_request_correlates_with_its_reported_event() {
        let actor = spawn_with_battery_swap([1]).await;
        let station = RecordingStation::default();
        handle_request_battery_swap(&actor, &station, BatterySwapRequestId(9), test_id_token())
            .await;
        assert!(
            actor
                .state()
                .battery_swaps
                .get(BatterySwapRequestId(9))
                .is_some()
        );

        report_battery_swap_event(
            &actor,
            BatterySwapEvent {
                request_id: BatterySwapRequestId(9),
                event_type: BatterySwapEventKind::BatteryOut,
                id_token: test_id_token(),
                battery_data: alloc::vec![BatteryData::new(0, "SN1", 87.5, 99.0, None, None)],
            },
        )
        .await;

        assert!(
            actor
                .state()
                .battery_swaps
                .get(BatterySwapRequestId(9))
                .is_none(),
            "the correlated pending request should have been resolved"
        );
    }

    #[tokio::test]
    async fn a_driver_initiated_swap_with_no_pending_request_still_reports() {
        let actor = spawn_with_battery_swap([1]).await;

        struct RecordingNotifier {
            events: Arc<std::sync::Mutex<alloc::vec::Vec<BatterySwapEvent>>>,
        }

        #[async_trait::async_trait]
        impl BatterySwapNotifier for RecordingNotifier {
            type Error = core::convert::Infallible;

            async fn notify_battery_swap(
                &self,
                event: &BatterySwapEvent,
            ) -> Result<(), Self::Error> {
                self.events.lock().unwrap().push(event.clone());
                Ok(())
            }
        }

        let recorded = Arc::new(std::sync::Mutex::new(alloc::vec::Vec::new()));
        let notifier = RecordingNotifier {
            events: recorded.clone(),
        };
        let receiver = actor.subscribe_battery_swap_events();
        let forwarding = tokio::spawn(async move {
            run_battery_swap_events(receiver, &notifier).await;
        });

        report_battery_swap_event(
            &actor,
            BatterySwapEvent {
                request_id: BatterySwapRequestId(42),
                event_type: BatterySwapEventKind::BatteryIn,
                id_token: test_id_token(),
                battery_data: alloc::vec![BatteryData::new(0, "SN2", 10.0, 80.0, None, None)],
            },
        )
        .await;

        for _ in 0..50 {
            if !recorded.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(core::time::Duration::from_millis(10)).await;
        }
        let recorded = recorded.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].request_id, BatterySwapRequestId(42));
        forwarding.abort();
    }
}
