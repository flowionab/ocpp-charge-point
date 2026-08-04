//! Availability functional block: reporting connector status to the CSMS via
//! StatusNotification, and handling CSMS-initiated `ChangeAvailability` requests. See
//! `docs/ROADMAP.md` §7.

use crate::actor::ChargePointActor;
use crate::state::{
    ChargePointEvent, ConnectorEvent, ConnectorStatus, ConnectorStatusChanged, EvseEvent,
};
use crate::sync::{BroadcastReceiver, RecvError};
use alloc::boxed::Box;

/// Reports a connector's status to the CSMS via StatusNotification. Implemented per protocol
/// version (see the `ocpp_2_1` module), mirroring [`crate::provisioning::BootNotifier`].
#[async_trait::async_trait]
pub trait StatusNotifier {
    type Error: core::error::Error + Send + Sync + 'static;

    async fn notify_status(
        &self,
        evse_id: usize,
        connector_id: usize,
        status: ConnectorStatus,
    ) -> Result<(), Self::Error>;
}

/// The scope of a CSMS-initiated `ChangeAvailability` request - OCPP's optional `evse`/
/// `connectorId` addressing collapsed to one of the three levels the internal state model
/// tracks availability at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AvailabilityTarget {
    ChargePoint,
    Evse { evse_id: usize },
    Connector { evse_id: usize, connector_id: usize },
}

/// The outcome of a CSMS-initiated `ChangeAvailability` request, matching (a subset of) OCPP's
/// `ChangeAvailabilityStatusEnum`. `Scheduled` - deferring the change until an in-progress
/// transaction ends - isn't modeled: `SetUnavailable` takes effect immediately regardless of an
/// active transaction (see `docs/ROADMAP.md` §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeAvailabilityOutcome {
    Accepted,
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
    async fn register_change_availability_handler(&self, actor: ChargePointActor);
}

/// Forwards every connector status change received on `changes` to the CSMS via `notifier`,
/// forever. Errors are logged and do not stop the loop or drop the change - the actor already
/// applied it to state; only the CSMS-facing report failed.
pub async fn run_status_notifications<N: StatusNotifier>(
    mut changes: BroadcastReceiver<ConnectorStatusChanged>,
    notifier: &N,
) {
    loop {
        match changes.recv().await {
            Ok(changed) => {
                if let Err(err) = notifier
                    .notify_status(changed.evse_id, changed.connector_id, changed.status)
                    .await
                {
                    tracing::warn!(error = %err, "status notification failed");
                }
            }
            Err(RecvError::Closed) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{StatusNotifier, run_status_notifications};
    use crate::state::{ConnectorStatus, ConnectorStatusChanged};
    use crate::sync::broadcast_channel;
    use alloc::boxed::Box;
    use alloc::vec::Vec;
    use tokio::sync::watch;

    struct RecordingStatusNotifier {
        seen: watch::Sender<Vec<(usize, usize, ConnectorStatus)>>,
    }

    #[async_trait::async_trait]
    impl StatusNotifier for RecordingStatusNotifier {
        type Error = core::convert::Infallible;

        async fn notify_status(
            &self,
            evse_id: usize,
            connector_id: usize,
            status: ConnectorStatus,
        ) -> Result<(), Self::Error> {
            self.seen
                .send_modify(|seen| seen.push((evse_id, connector_id, status)));
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
        });
        sender.send(ConnectorStatusChanged {
            evse_id: 0,
            connector_id: 1,
            status: ConnectorStatus::Faulted,
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
                (0, 0, ConnectorStatus::Occupied),
                (0, 1, ConnectorStatus::Faulted),
            ]
        );
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
        use crate::state::ConnectorStatus;
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
                    let available = matches!(
                        request.operational_status,
                        OperationalStatusEnum::Operative
                    );
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
                evse: evse.map(|(id, connector_id)| {
                    ocpp_client::ocpp_types::v21::common::EVSE {
                        id,
                        connector_id,
                        custom_data: None,
                    }
                }),
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
