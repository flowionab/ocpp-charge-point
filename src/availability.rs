//! Availability functional block: reporting connector status to the CSMS via
//! StatusNotification, and handling CSMS-initiated `ChangeAvailability` requests. See
//! `docs/ROADMAP.md` §7.

use crate::actor::ChargePointActor;
use crate::state::{
    ChargePointEvent, ConnectorEvent, ConnectorState, ConnectorStatus, ConnectorStatusChanged,
    EvseEvent,
};
use crate::sync::BroadcastReceiver;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use core::cell::RefCell;
use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;

#[cfg(feature = "ocpp_1_6")]
pub use self::ocpp_1_6::{Ocpp1_6ChangeAvailabilityHandler, Ocpp1_6StatusNotifier};

/// Reports a connector's status to the CSMS via StatusNotification. Implemented per protocol
/// version (see the `ocpp_2_1`/`ocpp_1_6` modules), mirroring
/// [`crate::provisioning::BootNotifier`].
#[async_trait::async_trait]
pub trait StatusNotifier {
    /// The error type returned if the StatusNotification request itself fails.
    type Error: core::error::Error + Send + Sync + 'static;

    /// Reports `connector_id` on `evse_id`'s current status to the CSMS. `status` is the coarse,
    /// OCPP 2.x-shaped 5-value status - what most version adapters need. `connector_state`
    /// carries the same change at its full, protocol-version-independent granularity, for
    /// versions whose wire status is richer than `status` (e.g. 1.6J's `Preparing`/`Charging`/
    /// `Finishing`/`SuspendedEV`/`SuspendedEVSE`) to derive their own mapping from instead - see
    /// `docs/ROADMAP.md` §0.
    async fn notify_status(
        &self,
        evse_id: usize,
        connector_id: usize,
        status: ConnectorStatus,
        connector_state: ConnectorState,
    ) -> Result<(), Self::Error>;
}

/// Wraps any [`StatusNotifier`] to only forward a call when `status` actually differs from the
/// last one reported for that connector (or is the first one ever reported for it) - restoring
/// the "only report on a wire-visible status change" cadence for versions whose own status is no
/// richer than [`ConnectorStatus`], now that [`crate::state::ChargePointState`] itself reports
/// every internal `ConnectorState` transition, not just ones that cross a `ConnectorStatus`
/// boundary (see `docs/ROADMAP.md` §0). `connector_state` is always forwarded unchanged
/// regardless of the dedup decision - only whether `inner` is called at all is affected.
///
/// [`crate::setup`] wraps its `csms` in this for status reporting specifically, so - unless an
/// integrator bypasses `setup()` - existing deployments keep the wire cadence they had before
/// `ChargePointState` started reporting every transition. A version with its own richer status
/// (e.g. [`crate::availability::Ocpp1_6StatusNotifier`]) should **not** be wrapped in this - it
/// needs every call, including ones where `status` repeats but `connector_state` doesn't.
pub struct DedupedStatusNotifier<N> {
    inner: N,
    last_sent: LastSentStatuses,
}

/// Per-connector last-reported [`ConnectorStatus`], keyed by `(evse_id, connector_id)`.
type LastSentStatuses =
    BlockingMutex<CriticalSectionRawMutex, RefCell<BTreeMap<(usize, usize), ConnectorStatus>>>;

impl<N> DedupedStatusNotifier<N> {
    /// Wraps `inner`, starting with no cached status for any connector - so the first call for
    /// each connector is always forwarded, regardless of what `status` it reports.
    pub fn new(inner: N) -> Self {
        Self {
            inner,
            last_sent: BlockingMutex::new(RefCell::new(BTreeMap::new())),
        }
    }
}

#[async_trait::async_trait]
impl<N: StatusNotifier + Sync> StatusNotifier for DedupedStatusNotifier<N> {
    type Error = N::Error;

    async fn notify_status(
        &self,
        evse_id: usize,
        connector_id: usize,
        status: ConnectorStatus,
        connector_state: ConnectorState,
    ) -> Result<(), Self::Error> {
        let changed = self.last_sent.lock(|cache| {
            let mut cache = cache.borrow_mut();
            if cache.get(&(evse_id, connector_id)) == Some(&status) {
                false
            } else {
                cache.insert((evse_id, connector_id), status);
                true
            }
        });
        if changed {
            self.inner
                .notify_status(evse_id, connector_id, status, connector_state)
                .await
        } else {
            Ok(())
        }
    }
}

/// The scope of a CSMS-initiated `ChangeAvailability` request - OCPP's optional `evse`/
/// `connectorId` addressing collapsed to one of the three levels the internal state model
/// tracks availability at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AvailabilityTarget {
    /// The request addresses the whole charge point.
    ChargePoint,
    /// The request addresses one EVSE and every connector on it.
    Evse {
        /// The targeted EVSE's index.
        evse_id: usize,
    },
    /// The request addresses a single connector.
    Connector {
        /// The targeted connector's EVSE index.
        evse_id: usize,
        /// The targeted connector's index within its EVSE.
        connector_id: usize,
    },
}

/// The outcome of a CSMS-initiated `ChangeAvailability` request, matching (a subset of) OCPP's
/// `ChangeAvailabilityStatusEnum`. `Scheduled` - deferring the change until an in-progress
/// transaction ends - isn't modeled: `SetUnavailable` takes effect immediately regardless of an
/// active transaction (see `docs/ROADMAP.md` §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeAvailabilityOutcome {
    /// The request addressed a real EVSE/connector and was applied.
    Accepted,
    /// The request addressed an EVSE/connector index that doesn't exist.
    Rejected,
}

/// Handles a CSMS-initiated `ChangeAvailability` request against `actor`: rejects a `target`
/// that doesn't address a real EVSE/connector, otherwise applies `available` as
/// `SetAvailable`/`SetUnavailable` at the matching level of the internal state model
/// (charge-point-wide, EVSE, or connector) and accepts. Availability changes apply
/// synchronously within the actor (unlike e.g. `UnlockConnector`, no hardware round-trip is
/// needed), so this doesn't need to wait for a confirming state change.
pub async fn handle_change_availability_request(
    actor: &ChargePointActor,
    target: AvailabilityTarget,
    available: bool,
) -> ChangeAvailabilityOutcome {
    let event = match target {
        AvailabilityTarget::ChargePoint => availability_event(available),
        AvailabilityTarget::Evse { evse_id } => {
            if actor.state().evses.get(evse_id).is_none() {
                return ChangeAvailabilityOutcome::Rejected;
            }
            ChargePointEvent::Evse {
                evse_id,
                event: evse_availability_event(available),
            }
        }
        AvailabilityTarget::Connector {
            evse_id,
            connector_id,
        } => {
            let connector_exists = actor
                .state()
                .evses
                .get(evse_id)
                .is_some_and(|evse| evse.connectors.get(connector_id).is_some());
            if !connector_exists {
                return ChangeAvailabilityOutcome::Rejected;
            }
            ChargePointEvent::Evse {
                evse_id,
                event: EvseEvent::Connector {
                    connector_id,
                    event: connector_availability_event(available),
                },
            }
        }
    };

    let _ = actor.send(event).await;
    ChangeAvailabilityOutcome::Accepted
}

fn availability_event(available: bool) -> ChargePointEvent {
    if available {
        ChargePointEvent::SetAvailable
    } else {
        ChargePointEvent::SetUnavailable
    }
}

fn evse_availability_event(available: bool) -> EvseEvent {
    if available {
        EvseEvent::SetAvailable
    } else {
        EvseEvent::SetUnavailable
    }
}

fn connector_availability_event(available: bool) -> ConnectorEvent {
    if available {
        ConnectorEvent::SetAvailable
    } else {
        ConnectorEvent::SetUnavailable
    }
}

/// Registers this charge point's inbound `ChangeAvailability` handling with the CSMS
/// connection. Implemented per protocol version (see the `ocpp_2_1` module), mirroring
/// [`crate::remote_control::UnlockConnectorHandler`].
#[async_trait::async_trait]
pub trait ChangeAvailabilityHandler {
    /// Registers a `ChangeAvailability` handler with the CSMS connection that dispatches
    /// incoming requests to [`handle_change_availability_request`] against `actor`.
    async fn register_change_availability_handler(&self, actor: ChargePointActor);
}

/// Forwards every connector status change received on `changes` to the CSMS via `notifier`,
/// forever. Errors are logged and do not stop the loop or drop the change - the actor already
/// applied it to state; only the CSMS-facing report failed and is not retried. [`setup`](crate::setup)
/// uses [`crate::offline_queue::run_with_offline_queue`] instead, which queues and retries a
/// failed report rather than just logging it - prefer that for anything CSMS-connection-integrity
/// sensitive; this simpler fire-and-forget version remains for callers who don't need retry.
pub async fn run_status_notifications<N: StatusNotifier>(
    mut changes: BroadcastReceiver<ConnectorStatusChanged>,
    notifier: &N,
) {
    while let Ok(changed) = changes.recv().await {
        if let Err(err) = notifier
            .notify_status(
                changed.evse_id,
                changed.connector_id,
                changed.status,
                changed.connector_state,
            )
            .await
        {
            tracing::warn!(error = %err, "status notification failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{StatusNotifier, run_status_notifications};
    use crate::state::{ConnectorState, ConnectorStatus, ConnectorStatusChanged};
    use crate::sync::broadcast_channel;
    use alloc::boxed::Box;
    use alloc::vec::Vec;
    use tokio::sync::watch;

    pub(super) struct RecordingStatusNotifier {
        pub(super) seen: watch::Sender<Vec<(usize, usize, ConnectorStatus, ConnectorState)>>,
    }

    #[async_trait::async_trait]
    impl StatusNotifier for RecordingStatusNotifier {
        type Error = core::convert::Infallible;

        async fn notify_status(
            &self,
            evse_id: usize,
            connector_id: usize,
            status: ConnectorStatus,
            connector_state: ConnectorState,
        ) -> Result<(), Self::Error> {
            self.seen
                .send_modify(|seen| seen.push((evse_id, connector_id, status, connector_state)));
            Ok(())
        }
    }

    #[tokio::test]
    async fn forwards_every_status_change_to_the_notifier_in_order() {
        let sender = broadcast_channel();
        let receiver = sender.subscribe();
        let (seen_tx, mut seen_rx) = watch::channel(Vec::new());
        let notifier = RecordingStatusNotifier { seen: seen_tx };

        let forwarder = tokio::spawn(async move {
            run_status_notifications(receiver, &notifier).await;
        });

        sender.send(ConnectorStatusChanged {
            evse_id: 0,
            connector_id: 0,
            status: ConnectorStatus::Occupied,
            connector_state: ConnectorState::Connected,
        });
        sender.send(ConnectorStatusChanged {
            evse_id: 0,
            connector_id: 1,
            status: ConnectorStatus::Faulted,
            connector_state: ConnectorState::Faulted,
        });

        seen_rx
            .wait_for(|seen| seen.len() == 2)
            .await
            .expect("notifier task is still running");

        // Dropping the sender closes the channel, which ends `run_status_notifications`'s loop.
        drop(sender);
        forwarder.await.unwrap();

        assert_eq!(
            *seen_rx.borrow(),
            alloc::vec![
                (0, 0, ConnectorStatus::Occupied, ConnectorState::Connected),
                (0, 1, ConnectorStatus::Faulted, ConnectorState::Faulted),
            ]
        );
    }
}

#[cfg(test)]
mod deduped_status_notifier_tests {
    use super::tests::RecordingStatusNotifier;
    use super::{DedupedStatusNotifier, StatusNotifier};
    use crate::state::{ConnectorState, ConnectorStatus};
    use alloc::vec::Vec;
    use tokio::sync::watch;

    type SeenReceiver = watch::Receiver<Vec<(usize, usize, ConnectorStatus, ConnectorState)>>;

    fn notifier() -> (DedupedStatusNotifier<RecordingStatusNotifier>, SeenReceiver) {
        let (seen_tx, seen_rx) = watch::channel(Vec::new());
        (
            DedupedStatusNotifier::new(RecordingStatusNotifier { seen: seen_tx }),
            seen_rx,
        )
    }

    #[tokio::test]
    async fn the_first_call_for_a_connector_always_forwards() {
        let (notifier, seen) = notifier();

        notifier
            .notify_status(0, 0, ConnectorStatus::Occupied, ConnectorState::Connected)
            .await
            .unwrap();

        assert_eq!(seen.borrow().len(), 1);
    }

    #[tokio::test]
    async fn a_repeated_status_for_the_same_connector_is_suppressed() {
        let (notifier, seen) = notifier();

        // `Connected` -> `Locked` is a real `ConnectorState` transition but keeps the same
        // coarse `Occupied` status - exactly the case `ChargePointState` now reports on its own
        // (see `docs/ROADMAP.md` §0) that this wrapper exists to filter back out for a version
        // whose own wire status doesn't distinguish it.
        notifier
            .notify_status(0, 0, ConnectorStatus::Occupied, ConnectorState::Connected)
            .await
            .unwrap();
        notifier
            .notify_status(0, 0, ConnectorStatus::Occupied, ConnectorState::Locked)
            .await
            .unwrap();

        assert_eq!(seen.borrow().len(), 1);
    }

    #[tokio::test]
    async fn a_different_status_for_the_same_connector_forwards_again() {
        let (notifier, seen) = notifier();

        notifier
            .notify_status(0, 0, ConnectorStatus::Occupied, ConnectorState::Connected)
            .await
            .unwrap();
        notifier
            .notify_status(0, 0, ConnectorStatus::Available, ConnectorState::Available)
            .await
            .unwrap();

        assert_eq!(seen.borrow().len(), 2);
    }

    #[tokio::test]
    async fn each_connector_is_deduped_independently() {
        let (notifier, seen) = notifier();

        notifier
            .notify_status(0, 0, ConnectorStatus::Occupied, ConnectorState::Connected)
            .await
            .unwrap();
        notifier
            .notify_status(0, 1, ConnectorStatus::Occupied, ConnectorState::Connected)
            .await
            .unwrap();

        assert_eq!(seen.borrow().len(), 2);
    }
}

#[cfg(test)]
mod change_availability_tests {
    use super::{
        AvailabilityTarget, ChangeAvailabilityOutcome, handle_change_availability_request,
    };
    use crate::actor::ChargePointActor;
    use crate::executor::TokioExecutor;
    use crate::state::{ConnectorState, EvseStatus, LifecycleState};

    #[tokio::test]
    async fn making_the_charge_point_unavailable_sets_the_lifecycle_state() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let outcome =
            handle_change_availability_request(&actor, AvailabilityTarget::ChargePoint, false)
                .await;

        assert_eq!(outcome, ChangeAvailabilityOutcome::Accepted);
        assert_eq!(actor.state().lifecycle, LifecycleState::Unavailable);
    }

    #[tokio::test]
    async fn making_an_evse_unavailable_sets_only_that_evses_status() {
        let actor = ChargePointActor::spawn([1, 1], &TokioExecutor);

        let outcome = handle_change_availability_request(
            &actor,
            AvailabilityTarget::Evse { evse_id: 1 },
            false,
        )
        .await;

        assert_eq!(outcome, ChangeAvailabilityOutcome::Accepted);
        assert_eq!(actor.state().evses[1].status, EvseStatus::Unavailable);
        assert_eq!(actor.state().evses[0].status, EvseStatus::Available);
    }

    #[tokio::test]
    async fn an_unknown_evse_is_rejected() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let outcome = handle_change_availability_request(
            &actor,
            AvailabilityTarget::Evse { evse_id: 5 },
            false,
        )
        .await;

        assert_eq!(outcome, ChangeAvailabilityOutcome::Rejected);
    }

    #[tokio::test]
    async fn making_a_connector_unavailable_then_available_again_round_trips() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let outcome = handle_change_availability_request(
            &actor,
            AvailabilityTarget::Connector {
                evse_id: 0,
                connector_id: 0,
            },
            false,
        )
        .await;
        assert_eq!(outcome, ChangeAvailabilityOutcome::Accepted);
        assert_eq!(
            actor.state().evses[0].connectors[0],
            ConnectorState::Unavailable
        );

        let outcome = handle_change_availability_request(
            &actor,
            AvailabilityTarget::Connector {
                evse_id: 0,
                connector_id: 0,
            },
            true,
        )
        .await;
        assert_eq!(outcome, ChangeAvailabilityOutcome::Accepted);
        assert_eq!(
            actor.state().evses[0].connectors[0],
            ConnectorState::Available
        );
    }

    #[tokio::test]
    async fn an_unknown_connector_is_rejected() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let outcome = handle_change_availability_request(
            &actor,
            AvailabilityTarget::Connector {
                evse_id: 0,
                connector_id: 5,
            },
            false,
        )
        .await;

        assert_eq!(outcome, ChangeAvailabilityOutcome::Rejected);
    }
}

#[cfg(feature = "ocpp_2_1")]
mod ocpp_2_1 {
    use crate::actor::ChargePointActor;
    use crate::availability::{
        AvailabilityTarget, ChangeAvailabilityHandler, ChangeAvailabilityOutcome,
        handle_change_availability_request,
    };
    use crate::state::ConnectorStatus;
    use alloc::boxed::Box;
    use ocpp_client::ocpp_2_1::OCPP2_1Client;
    use ocpp_client::ocpp_types::v21::common::{
        ChangeAvailabilityStatusEnum, ConnectorStatusEnum, OperationalStatusEnum,
    };
    use ocpp_client::ocpp_types::v21::{ChangeAvailabilityRequest, ChangeAvailabilityResponse};

    // Only consumed by `with_system_clock` below (`std`-gated) and by this module's own tests;
    // without either, it's legitimately unused.
    #[cfg_attr(not(feature = "std"), allow(dead_code))]
    pub(super) fn map_status(status: ConnectorStatus) -> ConnectorStatusEnum {
        match status {
            ConnectorStatus::Available => ConnectorStatusEnum::Available,
            ConnectorStatus::Occupied => ConnectorStatusEnum::Occupied,
            ConnectorStatus::Reserved => ConnectorStatusEnum::Reserved,
            ConnectorStatus::Unavailable => ConnectorStatusEnum::Unavailable,
            ConnectorStatus::Faulted => ConnectorStatusEnum::Faulted,
        }
    }

    // `StatusNotificationRequest` needs a timestamp; producing one without a caller-supplied
    // `Clock` requires the `std`-only `SystemClock` (see `crate::clock`), so this impl - unlike
    // the rest of this file - needs both `ocpp_2_1` and `std`.
    #[cfg(feature = "std")]
    mod with_system_clock {
        use super::map_status;
        use crate::availability::StatusNotifier;
        use crate::clock::{Clock, SystemClock};
        use crate::state::{ConnectorState, ConnectorStatus};
        use alloc::boxed::Box;
        use ocpp_client::ClientError;
        use ocpp_client::ocpp_2_1::{OCPP2_1Client, OCPP2_1Error};
        use ocpp_client::ocpp_types::v21::StatusNotificationRequest;

        #[async_trait::async_trait]
        impl StatusNotifier for OCPP2_1Client {
            type Error = ClientError<OCPP2_1Error>;

            async fn notify_status(
                &self,
                evse_id: usize,
                connector_id: usize,
                status: ConnectorStatus,
                // 2.1's own `ConnectorStatusEnumType` is already only these 5 coarse values -
                // nothing richer to derive here, unlike 1.6J's adapter (see the `ocpp_1_6`
                // module).
                _connector_state: ConnectorState,
            ) -> Result<(), Self::Error> {
                self.send_status_notification(StatusNotificationRequest {
                    custom_data: None,
                    timestamp: SystemClock.now().to_rfc3339(),
                    connector_status: map_status(status),
                    evse_id: evse_id as i64,
                    connector_id: connector_id as i64,
                })
                .await?;
                Ok(())
            }
        }
    }

    /// `None` if the request's `evse`/`connectorId` addressing doesn't parse into a valid
    /// (non-negative) index - handled the same as an unknown EVSE/connector, i.e. `Rejected`.
    fn parse_target(request: &ChangeAvailabilityRequest) -> Option<AvailabilityTarget> {
        let Some(evse) = &request.evse else {
            return Some(AvailabilityTarget::ChargePoint);
        };
        let evse_id = usize::try_from(evse.id).ok()?;
        match evse.connector_id {
            None => Some(AvailabilityTarget::Evse { evse_id }),
            Some(connector_id) => Some(AvailabilityTarget::Connector {
                evse_id,
                connector_id: usize::try_from(connector_id).ok()?,
            }),
        }
    }

    fn map_outcome(outcome: ChangeAvailabilityOutcome) -> ChangeAvailabilityStatusEnum {
        match outcome {
            ChangeAvailabilityOutcome::Accepted => ChangeAvailabilityStatusEnum::Accepted,
            ChangeAvailabilityOutcome::Rejected => ChangeAvailabilityStatusEnum::Rejected,
        }
    }

    #[async_trait::async_trait]
    impl ChangeAvailabilityHandler for OCPP2_1Client {
        async fn register_change_availability_handler(&self, actor: ChargePointActor) {
            self.on_change_availability(move |request, _client| {
                let actor = actor.clone();
                async move {
                    let available =
                        matches!(request.operational_status, OperationalStatusEnum::Operative);
                    let outcome = match parse_target(&request) {
                        Some(target) => {
                            handle_change_availability_request(&actor, target, available).await
                        }
                        None => ChangeAvailabilityOutcome::Rejected,
                    };
                    Ok(ChangeAvailabilityResponse {
                        status: map_outcome(outcome),
                        status_info: None,
                        custom_data: None,
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
        fn every_internal_status_maps_to_the_matching_wire_status() {
            assert_eq!(
                map_status(ConnectorStatus::Available),
                ConnectorStatusEnum::Available
            );
            assert_eq!(
                map_status(ConnectorStatus::Occupied),
                ConnectorStatusEnum::Occupied
            );
            assert_eq!(
                map_status(ConnectorStatus::Reserved),
                ConnectorStatusEnum::Reserved
            );
            assert_eq!(
                map_status(ConnectorStatus::Unavailable),
                ConnectorStatusEnum::Unavailable
            );
            assert_eq!(
                map_status(ConnectorStatus::Faulted),
                ConnectorStatusEnum::Faulted
            );
        }

        fn request(evse: Option<(i64, Option<i64>)>) -> ChangeAvailabilityRequest {
            ChangeAvailabilityRequest {
                evse: evse.map(
                    |(id, connector_id)| ocpp_client::ocpp_types::v21::common::EVSE {
                        id,
                        connector_id,
                        custom_data: None,
                    },
                ),
                operational_status: OperationalStatusEnum::Inoperative,
                custom_data: None,
            }
        }

        #[test]
        fn no_evse_targets_the_whole_charge_point() {
            assert_eq!(
                parse_target(&request(None)),
                Some(AvailabilityTarget::ChargePoint)
            );
        }

        #[test]
        fn an_evse_with_no_connector_targets_the_evse() {
            assert_eq!(
                parse_target(&request(Some((1, None)))),
                Some(AvailabilityTarget::Evse { evse_id: 1 })
            );
        }

        #[test]
        fn an_evse_with_a_connector_targets_the_connector() {
            assert_eq!(
                parse_target(&request(Some((1, Some(2))))),
                Some(AvailabilityTarget::Connector {
                    evse_id: 1,
                    connector_id: 2,
                })
            );
        }

        #[test]
        fn a_negative_evse_id_has_no_target() {
            assert_eq!(parse_target(&request(Some((-1, None)))), None);
        }

        #[test]
        fn every_outcome_maps_to_the_matching_wire_status() {
            assert_eq!(
                map_outcome(ChangeAvailabilityOutcome::Accepted),
                ChangeAvailabilityStatusEnum::Accepted
            );
            assert_eq!(
                map_outcome(ChangeAvailabilityOutcome::Rejected),
                ChangeAvailabilityStatusEnum::Rejected
            );
        }
    }
}

/// The OCPP 2.0.1 projection - identical wire shape to 2.1's `StatusNotificationRequest`/
/// `ChangeAvailabilityRequest`/`ConnectorStatusEnum`/`ChangeAvailabilityStatusEnum`, so this is
/// close to a copy of the 2.1 one, just targeting `OCPP2_0_1Client`.
#[cfg(feature = "ocpp_2_0_1")]
mod ocpp_2_0_1 {
    use crate::actor::ChargePointActor;
    use crate::availability::{
        AvailabilityTarget, ChangeAvailabilityHandler, ChangeAvailabilityOutcome,
        handle_change_availability_request,
    };
    use crate::state::ConnectorStatus;
    use alloc::boxed::Box;
    use ocpp_client::ocpp_2_0_1::OCPP2_0_1Client;
    use ocpp_client::ocpp_types::v201::common::{
        ChangeAvailabilityStatusEnum, ConnectorStatusEnum, OperationalStatusEnum,
    };
    use ocpp_client::ocpp_types::v201::{ChangeAvailabilityRequest, ChangeAvailabilityResponse};

    #[cfg_attr(not(feature = "std"), allow(dead_code))]
    pub(super) fn map_status(status: ConnectorStatus) -> ConnectorStatusEnum {
        match status {
            ConnectorStatus::Available => ConnectorStatusEnum::Available,
            ConnectorStatus::Occupied => ConnectorStatusEnum::Occupied,
            ConnectorStatus::Reserved => ConnectorStatusEnum::Reserved,
            ConnectorStatus::Unavailable => ConnectorStatusEnum::Unavailable,
            ConnectorStatus::Faulted => ConnectorStatusEnum::Faulted,
        }
    }

    #[cfg(feature = "std")]
    mod with_system_clock {
        use super::map_status;
        use crate::availability::StatusNotifier;
        use crate::clock::{Clock, SystemClock};
        use crate::state::{ConnectorState, ConnectorStatus};
        use alloc::boxed::Box;
        use ocpp_client::ClientError;
        use ocpp_client::ocpp_2_0_1::{OCPP2_0_1Client, OCPP2_0_1Error};
        use ocpp_client::ocpp_types::v201::StatusNotificationRequest;

        #[async_trait::async_trait]
        impl StatusNotifier for OCPP2_0_1Client {
            type Error = ClientError<OCPP2_0_1Error>;

            async fn notify_status(
                &self,
                evse_id: usize,
                connector_id: usize,
                status: ConnectorStatus,
                _connector_state: ConnectorState,
            ) -> Result<(), Self::Error> {
                self.send_status_notification(StatusNotificationRequest {
                    custom_data: None,
                    timestamp: SystemClock.now().to_rfc3339(),
                    connector_status: map_status(status),
                    evse_id: evse_id as i64,
                    connector_id: connector_id as i64,
                })
                .await?;
                Ok(())
            }
        }
    }

    fn parse_target(request: &ChangeAvailabilityRequest) -> Option<AvailabilityTarget> {
        let Some(evse) = &request.evse else {
            return Some(AvailabilityTarget::ChargePoint);
        };
        let evse_id = usize::try_from(evse.id).ok()?;
        match evse.connector_id {
            None => Some(AvailabilityTarget::Evse { evse_id }),
            Some(connector_id) => Some(AvailabilityTarget::Connector {
                evse_id,
                connector_id: usize::try_from(connector_id).ok()?,
            }),
        }
    }

    fn map_outcome(outcome: ChangeAvailabilityOutcome) -> ChangeAvailabilityStatusEnum {
        match outcome {
            ChangeAvailabilityOutcome::Accepted => ChangeAvailabilityStatusEnum::Accepted,
            ChangeAvailabilityOutcome::Rejected => ChangeAvailabilityStatusEnum::Rejected,
        }
    }

    #[async_trait::async_trait]
    impl ChangeAvailabilityHandler for OCPP2_0_1Client {
        async fn register_change_availability_handler(&self, actor: ChargePointActor) {
            self.on_change_availability(move |request, _client| {
                let actor = actor.clone();
                async move {
                    let available =
                        matches!(request.operational_status, OperationalStatusEnum::Operative);
                    let outcome = match parse_target(&request) {
                        Some(target) => {
                            handle_change_availability_request(&actor, target, available).await
                        }
                        None => ChangeAvailabilityOutcome::Rejected,
                    };
                    Ok(ChangeAvailabilityResponse {
                        status: map_outcome(outcome),
                        status_info: None,
                        custom_data: None,
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
        fn every_internal_status_maps_to_the_matching_wire_status() {
            assert_eq!(
                map_status(ConnectorStatus::Available),
                ConnectorStatusEnum::Available
            );
            assert_eq!(
                map_status(ConnectorStatus::Occupied),
                ConnectorStatusEnum::Occupied
            );
            assert_eq!(
                map_status(ConnectorStatus::Reserved),
                ConnectorStatusEnum::Reserved
            );
            assert_eq!(
                map_status(ConnectorStatus::Unavailable),
                ConnectorStatusEnum::Unavailable
            );
            assert_eq!(
                map_status(ConnectorStatus::Faulted),
                ConnectorStatusEnum::Faulted
            );
        }

        fn request(evse: Option<(i64, Option<i64>)>) -> ChangeAvailabilityRequest {
            ChangeAvailabilityRequest {
                evse: evse.map(
                    |(id, connector_id)| ocpp_client::ocpp_types::v201::common::EVSE {
                        id,
                        connector_id,
                        custom_data: None,
                    },
                ),
                operational_status: OperationalStatusEnum::Inoperative,
                custom_data: None,
            }
        }

        #[test]
        fn no_evse_targets_the_whole_charge_point() {
            assert_eq!(
                parse_target(&request(None)),
                Some(AvailabilityTarget::ChargePoint)
            );
        }

        #[test]
        fn an_evse_with_no_connector_targets_the_evse() {
            assert_eq!(
                parse_target(&request(Some((1, None)))),
                Some(AvailabilityTarget::Evse { evse_id: 1 })
            );
        }

        #[test]
        fn an_evse_with_a_connector_targets_the_connector() {
            assert_eq!(
                parse_target(&request(Some((1, Some(2))))),
                Some(AvailabilityTarget::Connector {
                    evse_id: 1,
                    connector_id: 2,
                })
            );
        }

        #[test]
        fn a_negative_evse_id_has_no_target() {
            assert_eq!(parse_target(&request(Some((-1, None)))), None);
        }

        #[test]
        fn every_outcome_maps_to_the_matching_wire_status() {
            assert_eq!(
                map_outcome(ChangeAvailabilityOutcome::Accepted),
                ChangeAvailabilityStatusEnum::Accepted
            );
            assert_eq!(
                map_outcome(ChangeAvailabilityOutcome::Rejected),
                ChangeAvailabilityStatusEnum::Rejected
            );
        }
    }
}

/// The OCPP 1.6J projection of `StatusNotifier` - the harder version-adapter case `docs/
/// ROADMAP.md` §0 calls out, unlike Provisioning's `BootNotifier`/`HeartbeatSender`: 1.6J
/// addresses connectors with a single flat `connectorId: i64` and no EVSE concept at all, while
/// this crate's internal model addresses every connector as an `(evse_id, connector_id)` pair -
/// so [`Ocpp1_6StatusNotifier`] wraps `OCPP1_6Client` together with the charge point's connector
/// topology (each EVSE's connector count) to translate between the two, via
/// [`crate::topology::flatten_ocpp_1_6_connector_id`]. 1.6J's status enum is also richer than
/// the `ConnectorStatus` this
/// crate's `StatusNotifier` trait mostly targets (`Preparing`/`Charging`/`SuspendedEVSE`/
/// `SuspendedEV`/`Finishing`, not just `Occupied`), so [`map_status`] derives it from the full
/// `ConnectorState` instead. That richer status is only reported at the same cadence every other
/// version already gets, though (whenever the coarse `ConnectorStatus` changes) - see
/// `ConnectorStatusChanged::connector_state`'s docs for why finer-grained internal transitions
/// (e.g. `Starting` -> `Charging`, both `Occupied`) don't yet re-fire this effect.
#[cfg(feature = "ocpp_1_6")]
mod ocpp_1_6 {
    use crate::availability::StatusNotifier;
    use crate::state::ConnectorState;
    use crate::topology::flatten_ocpp_1_6_connector_id;
    use alloc::boxed::Box;
    use alloc::vec::Vec;
    use ocpp_client::ClientError;
    use ocpp_client::ocpp_1_6::{OCPP1_6Client, OCPP1_6Error};
    use ocpp_client::ocpp_types::v16::StatusNotificationRequest;
    use ocpp_client::ocpp_types::v16::common::{ErrorCode, StatusNotificationRequestStatus};

    /// Maps this crate's full `ConnectorState` down to 1.6J's `StatusNotificationRequestStatus` -
    /// richer than [`crate::state::ConnectorStatus`]'s 5 values, so this reads `ConnectorState`
    /// directly rather than going through that already-collapsed status. `Authorizing` has no
    /// dedicated 1.6J status (1.6J has no Authorize-before-charging step baked into connector
    /// status the way this crate's state machine does) so it maps to `Preparing`, the same as
    /// `Connected`/`Locked` - all three mean "occupied, not charging yet".
    pub(super) fn map_status(state: ConnectorState) -> StatusNotificationRequestStatus {
        match state {
            ConnectorState::Available => StatusNotificationRequestStatus::Available,
            ConnectorState::Connected
            | ConnectorState::Locked
            | ConnectorState::Authorizing
            | ConnectorState::Starting => StatusNotificationRequestStatus::Preparing,
            ConnectorState::Charging => StatusNotificationRequestStatus::Charging,
            ConnectorState::Stopping | ConnectorState::Finishing | ConnectorState::Unlocking => {
                StatusNotificationRequestStatus::Finishing
            }
            ConnectorState::Unavailable => StatusNotificationRequestStatus::Unavailable,
            ConnectorState::Faulted | ConnectorState::FaultedSafe => {
                StatusNotificationRequestStatus::Faulted
            }
            ConnectorState::Reserved => StatusNotificationRequestStatus::Reserved,
        }
    }

    /// Wraps an `OCPP1_6Client` with the charge point's connector topology, needed to translate
    /// this crate's `(evse_id, connector_id)` addressing into 1.6J's flat `connectorId` - see
    /// this module's docs. `connector_counts` is each EVSE's connector count, in `evse_id`
    /// order, same as passed to [`crate::actor::ChargePointActor::spawn`]/
    /// [`crate::ChargePointRuntime::new`] - the topology is fixed for the charge point's
    /// lifetime, so this only needs to capture it once.
    #[derive(Clone)]
    pub struct Ocpp1_6StatusNotifier {
        client: OCPP1_6Client,
        connector_counts: Vec<usize>,
    }

    impl Ocpp1_6StatusNotifier {
        /// Wraps `client`, capturing `connector_counts` (each EVSE's connector count, in
        /// `evse_id` order) for translating connector addresses on every call.
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
    impl StatusNotifier for Ocpp1_6StatusNotifier {
        type Error = ClientError<OCPP1_6Error>;

        async fn notify_status(
            &self,
            evse_id: usize,
            connector_id: usize,
            _status: crate::state::ConnectorStatus,
            connector_state: ConnectorState,
        ) -> Result<(), Self::Error> {
            let Some(connector_id) =
                flatten_ocpp_1_6_connector_id(&self.connector_counts, evse_id, connector_id)
            else {
                // Not addressable under 1.6J's flat numbering (shouldn't happen in practice -
                // this crate's own state machine only ever reports real connectors - but there's
                // no wire message to send for a connector 1.6J itself has no way to name).
                return Ok(());
            };
            self.client
                .send_status_notification(StatusNotificationRequest {
                    connector_id,
                    error_code: ErrorCode::NoError,
                    info: None,
                    status: map_status(connector_state),
                    timestamp: None,
                    vendor_error_code: None,
                    vendor_id: None,
                })
                .await?;
            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn every_connector_state_maps_to_a_wire_status() {
            assert_eq!(
                map_status(ConnectorState::Available),
                StatusNotificationRequestStatus::Available
            );
            assert_eq!(
                map_status(ConnectorState::Connected),
                StatusNotificationRequestStatus::Preparing
            );
            assert_eq!(
                map_status(ConnectorState::Locked),
                StatusNotificationRequestStatus::Preparing
            );
            assert_eq!(
                map_status(ConnectorState::Authorizing),
                StatusNotificationRequestStatus::Preparing
            );
            assert_eq!(
                map_status(ConnectorState::Starting),
                StatusNotificationRequestStatus::Preparing
            );
            assert_eq!(
                map_status(ConnectorState::Charging),
                StatusNotificationRequestStatus::Charging
            );
            assert_eq!(
                map_status(ConnectorState::Stopping),
                StatusNotificationRequestStatus::Finishing
            );
            assert_eq!(
                map_status(ConnectorState::Finishing),
                StatusNotificationRequestStatus::Finishing
            );
            assert_eq!(
                map_status(ConnectorState::Unlocking),
                StatusNotificationRequestStatus::Finishing
            );
            assert_eq!(
                map_status(ConnectorState::Unavailable),
                StatusNotificationRequestStatus::Unavailable
            );
            assert_eq!(
                map_status(ConnectorState::Faulted),
                StatusNotificationRequestStatus::Faulted
            );
            assert_eq!(
                map_status(ConnectorState::FaultedSafe),
                StatusNotificationRequestStatus::Faulted
            );
            assert_eq!(
                map_status(ConnectorState::Reserved),
                StatusNotificationRequestStatus::Reserved
            );
        }

        #[test]
        fn ocpp1_6_status_notifier_implements_status_notifier() {
            fn assert_impl<T: StatusNotifier>() {}
            assert_impl::<Ocpp1_6StatusNotifier>();
        }
    }

    // The OCPP 1.6J projection of `ChangeAvailabilityHandler` - `ChangeAvailabilityRequest`'s
    // flat `connectorId` needs the same topology-aware translation `Ocpp1_6UnlockConnectorHandler`
    // needs (see `crate::remote_control`'s 1.6J module), via
    // `crate::topology::unflatten_ocpp_1_6_connector_id`. Unlike `UnlockConnector`, `0` is a
    // meaningful address here (not just "invalid") - it means "the whole charge point," matching
    // `AvailabilityTarget::ChargePoint`; 1.6J has no `AvailabilityTarget::Evse` equivalent at
    // all (no EVSE concept, so "target an EVSE and every connector on it" isn't expressible on
    // the wire) - only whole-station or one-specific-connector.
    use crate::actor::ChargePointActor;
    use crate::availability::{
        AvailabilityTarget, ChangeAvailabilityHandler, ChangeAvailabilityOutcome,
        handle_change_availability_request,
    };
    use crate::topology::unflatten_ocpp_1_6_connector_id;
    use ocpp_client::ocpp_types::v16::common::{
        ChangeAvailabilityRequestType, ChangeAvailabilityResponseStatus,
    };
    use ocpp_client::ocpp_types::v16::{ChangeAvailabilityRequest, ChangeAvailabilityResponse};

    /// `None` only if `connector_id` is negative or past the last real connector under
    /// `connector_counts` - `0` always resolves to [`AvailabilityTarget::ChargePoint`].
    fn parse_target(
        connector_counts: &[usize],
        request: &ChangeAvailabilityRequest,
    ) -> Option<AvailabilityTarget> {
        if request.connector_id == 0 {
            return Some(AvailabilityTarget::ChargePoint);
        }
        unflatten_ocpp_1_6_connector_id(connector_counts, request.connector_id).map(
            |(evse_id, connector_id)| AvailabilityTarget::Connector {
                evse_id,
                connector_id,
            },
        )
    }

    fn map_outcome(outcome: ChangeAvailabilityOutcome) -> ChangeAvailabilityResponseStatus {
        match outcome {
            ChangeAvailabilityOutcome::Accepted => ChangeAvailabilityResponseStatus::Accepted,
            ChangeAvailabilityOutcome::Rejected => ChangeAvailabilityResponseStatus::Rejected,
        }
    }

    /// Wraps an `OCPP1_6Client` with the charge point's connector topology, needed to translate
    /// `ChangeAvailabilityRequest`'s flat `connectorId` into this crate's `(evse_id,
    /// connector_id)` addressing - see this module's docs.
    pub struct Ocpp1_6ChangeAvailabilityHandler {
        client: OCPP1_6Client,
        connector_counts: Vec<usize>,
    }

    impl Ocpp1_6ChangeAvailabilityHandler {
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
    impl ChangeAvailabilityHandler for Ocpp1_6ChangeAvailabilityHandler {
        async fn register_change_availability_handler(&self, actor: ChargePointActor) {
            let connector_counts = self.connector_counts.clone();
            self.client
                .on_change_availability(move |request, _client| {
                    let actor = actor.clone();
                    let connector_counts = connector_counts.clone();
                    async move {
                        let available =
                            matches!(request.r#type, ChangeAvailabilityRequestType::Operative);
                        let outcome = match parse_target(&connector_counts, &request) {
                            Some(target) => {
                                handle_change_availability_request(&actor, target, available).await
                            }
                            None => ChangeAvailabilityOutcome::Rejected,
                        };
                        Ok(ChangeAvailabilityResponse {
                            status: map_outcome(outcome),
                        })
                    }
                })
                .await;
        }
    }

    #[cfg(test)]
    mod change_availability_tests {
        use super::*;

        fn request(connector_id: i64) -> ChangeAvailabilityRequest {
            ChangeAvailabilityRequest {
                connector_id,
                r#type: ChangeAvailabilityRequestType::Inoperative,
            }
        }

        #[test]
        fn connector_id_zero_targets_the_whole_charge_point() {
            let connector_counts = [1, 1];

            assert_eq!(
                parse_target(&connector_counts, &request(0)),
                Some(AvailabilityTarget::ChargePoint)
            );
        }

        #[test]
        fn a_positive_connector_id_targets_the_matching_connector() {
            let connector_counts = [2, 1];

            assert_eq!(
                parse_target(&connector_counts, &request(3)),
                Some(AvailabilityTarget::Connector {
                    evse_id: 1,
                    connector_id: 0,
                })
            );
        }

        #[test]
        fn an_out_of_range_connector_id_has_no_target() {
            let connector_counts = [1, 1];

            assert_eq!(parse_target(&connector_counts, &request(5)), None);
        }

        #[test]
        fn every_outcome_maps_to_a_wire_status() {
            assert_eq!(
                map_outcome(ChangeAvailabilityOutcome::Accepted),
                ChangeAvailabilityResponseStatus::Accepted
            );
            assert_eq!(
                map_outcome(ChangeAvailabilityOutcome::Rejected),
                ChangeAvailabilityResponseStatus::Rejected
            );
        }

        #[test]
        fn ocpp1_6_change_availability_handler_implements_the_handler_trait() {
            fn assert_impl<T: ChangeAvailabilityHandler>() {}
            assert_impl::<Ocpp1_6ChangeAvailabilityHandler>();
        }
    }
}
