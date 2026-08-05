//! Remote control functional block: CSMS-initiated actions such as `UnlockConnector`. See
//! `docs/ROADMAP.md` §6.

use crate::actor::ChargePointActor;
use crate::availability::{AvailabilityTarget, StatusNotifier};
use crate::provisioning::HeartbeatSender;
use crate::state::{
    ChargePointEvent, ChargePointState, ConnectorEvent, ConnectorState, EvseEvent, StopReason,
    TransactionId,
};
use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

/// The outcome of a CSMS-initiated `UnlockConnector` request, matching OCPP's
/// `UnlockStatusEnum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnlockOutcome {
    /// The connector was unlocked.
    Unlocked,
    /// The connector could not be unlocked (it wasn't in a state where unlocking is meaningful,
    /// or the hardware reported a fault while unlocking).
    UnlockFailed,
    /// The connector has an active, authorized transaction and must not be unlocked remotely.
    OngoingAuthorizedTransaction,
    /// `evse_id`/`connector_id` doesn't address a connector on this charge point.
    UnknownConnector,
}

/// Handles a CSMS-initiated `UnlockConnector` request against `actor`. Only a `Locked`
/// connector (cable locked, no active transaction) can be remotely unlocked; a connector with a
/// transaction in progress is refused rather than interrupted. When the request is valid, this
/// drives the connector through to hardware confirmation before answering - the OCPP `Unlocked`
/// status must reflect an unlock that actually happened, not merely one that was requested.
pub async fn handle_unlock_request(
    actor: &ChargePointActor,
    evse_id: usize,
    connector_id: usize,
) -> UnlockOutcome {
    let Some(connector) = actor
        .state()
        .evses
        .get(evse_id)
        .and_then(|evse| evse.connectors.get(connector_id).copied())
    else {
        return UnlockOutcome::UnknownConnector;
    };

    match connector {
        ConnectorState::Authorizing
        | ConnectorState::Starting
        | ConnectorState::Charging
        | ConnectorState::Stopping => UnlockOutcome::OngoingAuthorizedTransaction,
        ConnectorState::Locked => {
            let mut updates = actor.subscribe();
            let _ = actor
                .send(ChargePointEvent::Evse {
                    evse_id,
                    event: EvseEvent::Connector {
                        connector_id,
                        event: ConnectorEvent::RemoteUnlockRequested,
                    },
                })
                .await;

            loop {
                let current = updates.borrow().evses[evse_id].connectors[connector_id];
                match current {
                    ConnectorState::Available => return UnlockOutcome::Unlocked,
                    ConnectorState::Faulted | ConnectorState::FaultedSafe => {
                        return UnlockOutcome::UnlockFailed;
                    }
                    _ => updates.changed().await,
                }
            }
        }
        _ => UnlockOutcome::UnlockFailed,
    }
}

/// Registers this charge point's inbound `UnlockConnector` handling with the CSMS connection.
/// Implemented per protocol version (see the `ocpp_2_1` module), mirroring
/// [`crate::availability::StatusNotifier`] but for an inbound CSMS-initiated call rather than
/// an outbound report.
#[async_trait::async_trait]
pub trait UnlockConnectorHandler {
    async fn register_unlock_connector_handler(&self, actor: ChargePointActor);
}

/// The outcome of a CSMS-initiated `RequestStartTransaction` request, matching (a subset of)
/// OCPP's `RequestStartStopStatusEnum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestStartTransactionOutcome {
    /// A transaction was started on the returned connector.
    Accepted {
        transaction_id: TransactionId,
    },
    Rejected,
}

/// Handles a CSMS-initiated `RequestStartTransaction` request against `actor`: finds a `Locked`
/// connector (cable connected, no active transaction) on `evse_id` - or, if `evse_id` is
/// `None`, the first `Locked` connector on any EVSE - and starts a transaction on it directly,
/// without a separate Authorize round-trip (the CSMS's own request is itself the authorization
/// decision; see `ConnectorEvent::RemoteStartRequested`). Rejects if `evse_id` is out of range,
/// or no matching connector is currently `Locked`.
///
/// The presented `id_token` isn't recorded - like the physical-presentation path, `Transaction`
/// doesn't carry an id token yet (see `docs/ROADMAP.md` §3/§5).
pub async fn handle_request_start_transaction(
    actor: &ChargePointActor,
    evse_id: Option<usize>,
) -> RequestStartTransactionOutcome {
    let Some((evse_id, connector_id)) = find_locked_connector(&actor.state(), evse_id) else {
        return RequestStartTransactionOutcome::Rejected;
    };

    let _ = actor
        .send(ChargePointEvent::Evse {
            evse_id,
            event: EvseEvent::Connector {
                connector_id,
                event: ConnectorEvent::RemoteStartRequested,
            },
        })
        .await;

    match actor.state().evses[evse_id].transactions[connector_id] {
        Some(transaction) => RequestStartTransactionOutcome::Accepted {
            transaction_id: transaction.id,
        },
        None => RequestStartTransactionOutcome::Rejected,
    }
}

/// The first `Locked` connector on `evse_id`, or on any EVSE (in order) if `evse_id` is `None`.
fn find_locked_connector(
    state: &ChargePointState,
    evse_id: Option<usize>,
) -> Option<(usize, usize)> {
    match evse_id {
        Some(evse_id) => {
            let evse = state.evses.get(evse_id)?;
            let connector_id = evse
                .connectors
                .iter()
                .position(|connector| *connector == ConnectorState::Locked)?;
            Some((evse_id, connector_id))
        }
        None => state.evses.iter().enumerate().find_map(|(evse_id, evse)| {
            evse.connectors
                .iter()
                .position(|connector| *connector == ConnectorState::Locked)
                .map(|connector_id| (evse_id, connector_id))
        }),
    }
}

/// Registers this charge point's inbound `RequestStartTransaction` handling with the CSMS
/// connection. Implemented per protocol version (see the `ocpp_2_1` module), mirroring
/// [`UnlockConnectorHandler`].
#[async_trait::async_trait]
pub trait RequestStartTransactionHandler {
    async fn register_request_start_transaction_handler(&self, actor: ChargePointActor);
}

/// The outcome of a CSMS-initiated `RequestStopTransaction` request, matching (a subset of)
/// OCPP's `RequestStartStopStatusEnum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestStopTransactionOutcome {
    Accepted,
    Rejected,
}

/// Handles a CSMS-initiated `RequestStopTransaction` request against `actor`: finds the
/// connector whose active transaction is `transaction_id` and, if it's currently `Charging`,
/// stops it (`ConnectorEvent::ChargingStopped(StopReason::Remote)`) and accepts. Rejects an
/// unknown `transaction_id`, or one that isn't currently `Charging` - the connector state
/// machine's only stop path is `Charging` -> `Stopping`, so e.g. a transaction still `Starting`
/// (contactor not yet confirmed closed) can't be stopped this way yet.
pub async fn handle_request_stop_transaction(
    actor: &ChargePointActor,
    transaction_id: TransactionId,
) -> RequestStopTransactionOutcome {
    let state = actor.state();
    let Some((evse_id, connector_id)) = find_transaction(&state, transaction_id) else {
        return RequestStopTransactionOutcome::Rejected;
    };
    if state.evses[evse_id].connectors[connector_id] != ConnectorState::Charging {
        return RequestStopTransactionOutcome::Rejected;
    }

    let _ = actor
        .send(ChargePointEvent::Evse {
            evse_id,
            event: EvseEvent::Connector {
                connector_id,
                event: ConnectorEvent::ChargingStopped(StopReason::Remote),
            },
        })
        .await;

    RequestStopTransactionOutcome::Accepted
}

/// The connector (if any) whose active transaction is `transaction_id`.
fn find_transaction(
    state: &ChargePointState,
    transaction_id: TransactionId,
) -> Option<(usize, usize)> {
    state.evses.iter().enumerate().find_map(|(evse_id, evse)| {
        evse.transactions
            .iter()
            .position(|transaction| transaction.is_some_and(|t| t.id == transaction_id))
            .map(|connector_id| (evse_id, connector_id))
    })
}

/// Registers this charge point's inbound `RequestStopTransaction` handling with the CSMS
/// connection. Implemented per protocol version (see the `ocpp_2_1` module), mirroring
/// [`RequestStartTransactionHandler`].
#[async_trait::async_trait]
pub trait RequestStopTransactionHandler {
    async fn register_request_stop_transaction_handler(&self, actor: ChargePointActor);
}

/// A CSMS-initiated `TriggerMessage` request to (re-)send a specific outbound message - the
/// subset of OCPP's `MessageTriggerEnumType` this crate can currently fulfil, each backed by an
/// outbound trait it already has (`HeartbeatSender`, `StatusNotifier`). Everything else
/// (`BootNotification` - needs hardware vendor/model this module has no access to;
/// `MeterValues`/`TransactionEvent` - needs a "resend current snapshot" capability neither
/// functional block has yet; firmware/log/certificate triggers, `CustomTrigger` - no supporting
/// functional block exists at all, §1/§12) has no internal representation to construct here.
/// There is currently no way to reach this from the network: the OCPP 2.1 wire types for
/// `TriggerMessage` don't exist yet in the `rust-ocpp`/`ocpp-client` dependencies (see
/// `docs/ROADMAP.md` §6) - this only exists as a protocol-agnostic building block for once that
/// lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerableMessage {
    Heartbeat,
    /// Re-sends `StatusNotification` for the addressed connector(s) - see
    /// [`crate::availability::AvailabilityTarget`] for what each variant covers.
    StatusNotification(AvailabilityTarget),
}

/// The outcome of a CSMS-initiated `TriggerMessage` request, matching (a subset of) OCPP's
/// `TriggerMessageStatusEnumType`. `NotImplemented` isn't representable here - it applies to
/// wire `MessageTriggerEnumType` values this module has no [`TriggerableMessage`] variant for at
/// all, so it can only be decided once a wire adapter exists to receive them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerMessageOutcome {
    Accepted,
    Rejected,
}

/// Handles a CSMS-initiated `TriggerMessage` request against `actor`, (re-)sending `message` via
/// `notifier`. A transport failure while sending is logged and does not flip the outcome to
/// `Rejected` - `Accepted` reflects that the charge point attempted the trigger, the same way
/// e.g. [`crate::availability::run_status_notifications`] logs and continues on a failed report
/// rather than treating it as a rejection of the underlying event.
pub async fn handle_trigger_message<N>(
    actor: &ChargePointActor,
    notifier: &N,
    message: TriggerableMessage,
) -> TriggerMessageOutcome
where
    N: HeartbeatSender + StatusNotifier,
{
    match message {
        TriggerableMessage::Heartbeat => {
            if let Err(err) = notifier.send_heartbeat().await {
                tracing::warn!(error = %err, "triggered heartbeat failed");
            }
            TriggerMessageOutcome::Accepted
        }
        TriggerableMessage::StatusNotification(target) => {
            trigger_status_notification(actor, notifier, target).await
        }
    }
}

async fn trigger_status_notification<N: StatusNotifier>(
    actor: &ChargePointActor,
    notifier: &N,
    target: AvailabilityTarget,
) -> TriggerMessageOutcome {
    let state = actor.state();
    let addressed = match target {
        AvailabilityTarget::ChargePoint => state
            .evses
            .iter()
            .enumerate()
            .flat_map(|(evse_id, evse)| {
                evse.connectors
                    .iter()
                    .enumerate()
                    .map(move |(connector_id, connector)| {
                        (evse_id, connector_id, connector.availability_status())
                    })
            })
            .collect::<Vec<_>>(),
        AvailabilityTarget::Evse { evse_id } => {
            let Some(evse) = state.evses.get(evse_id) else {
                return TriggerMessageOutcome::Rejected;
            };
            evse.connectors
                .iter()
                .enumerate()
                .map(|(connector_id, connector)| {
                    (evse_id, connector_id, connector.availability_status())
                })
                .collect()
        }
        AvailabilityTarget::Connector {
            evse_id,
            connector_id,
        } => {
            let Some(status) = state
                .evses
                .get(evse_id)
                .and_then(|evse| evse.connectors.get(connector_id))
                .map(|connector| connector.availability_status())
            else {
                return TriggerMessageOutcome::Rejected;
            };
            vec![(evse_id, connector_id, status)]
        }
    };

    for (evse_id, connector_id, status) in addressed {
        if let Err(err) = notifier.notify_status(evse_id, connector_id, status).await {
            tracing::warn!(
                error = %err,
                evse_id,
                connector_id,
                "triggered status notification failed"
            );
        }
    }

    TriggerMessageOutcome::Accepted
}

#[cfg(test)]
mod tests {
    use super::{
        RequestStartTransactionOutcome, RequestStopTransactionOutcome, TriggerMessageOutcome,
        TriggerableMessage, UnlockOutcome, handle_request_start_transaction,
        handle_request_stop_transaction, handle_trigger_message, handle_unlock_request,
    };
    use crate::actor::ChargePointActor;
    use crate::availability::{AvailabilityTarget, StatusNotifier};
    use crate::executor::TokioExecutor;
    use crate::provisioning::HeartbeatSender;
    use crate::state::{
        ChargePointEvent, ConnectorEvent, ConnectorState, ConnectorStatus, EvseEvent,
        HardwareCommand, TransactionId,
    };
    use crate::sync::RecvError;
    use alloc::vec::Vec;
    use tokio::sync::watch;

    /// Spawns an actor whose command broadcast is drained by a background task that
    /// immediately confirms every `UnlockConnector` command, standing in for the hardware
    /// executor loop that `setup()` normally wires up.
    fn actor_with_unlock_confirmed() -> ChargePointActor {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let mut commands = actor.subscribe_commands();
        let confirming_actor = actor.clone();
        tokio::spawn(async move {
            loop {
                match commands.recv().await {
                    Ok(HardwareCommand::UnlockConnector {
                        evse_id,
                        connector_id,
                    }) => {
                        let _ = confirming_actor
                            .send(ChargePointEvent::Evse {
                                evse_id,
                                event: EvseEvent::Connector {
                                    connector_id,
                                    event: ConnectorEvent::UnlockConfirmed,
                                },
                            })
                            .await;
                    }
                    Ok(_) => {}
                    Err(RecvError::Closed) => break,
                }
            }
        });
        actor
    }

    async fn locked_actor() -> ChargePointActor {
        let actor = actor_with_unlock_confirmed();
        for event in [
            ConnectorEvent::CableConnected,
            ConnectorEvent::LockConfirmed,
        ] {
            actor
                .send(ChargePointEvent::Evse {
                    evse_id: 0,
                    event: EvseEvent::Connector {
                        connector_id: 0,
                        event,
                    },
                })
                .await
                .unwrap();
        }
        actor
    }

    #[tokio::test]
    async fn unlocking_a_locked_connector_succeeds() {
        let actor = locked_actor().await;

        let outcome = handle_unlock_request(&actor, 0, 0).await;

        assert_eq!(outcome, UnlockOutcome::Unlocked);
    }

    #[tokio::test]
    async fn an_unknown_connector_is_reported_as_unknown() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        assert_eq!(
            handle_unlock_request(&actor, 5, 0).await,
            UnlockOutcome::UnknownConnector
        );
        assert_eq!(
            handle_unlock_request(&actor, 0, 5).await,
            UnlockOutcome::UnknownConnector
        );
    }

    #[tokio::test]
    async fn a_connector_with_no_cable_reports_unlock_failed() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        assert_eq!(
            handle_unlock_request(&actor, 0, 0).await,
            UnlockOutcome::UnlockFailed
        );
    }

    #[tokio::test]
    async fn an_active_transaction_refuses_the_unlock_request() {
        let actor = locked_actor().await;
        actor
            .send(ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event: ConnectorEvent::IdTokenPresented(crate::state::IdToken {
                        value: "04A224B2".into(),
                        kind: crate::state::IdTokenKind::ISO14443,
                    }),
                },
            })
            .await
            .unwrap();
        actor
            .send(ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event: ConnectorEvent::ChargingAuthorized,
                },
            })
            .await
            .unwrap();

        assert_eq!(
            handle_unlock_request(&actor, 0, 0).await,
            UnlockOutcome::OngoingAuthorizedTransaction
        );
    }

    async fn lock_connector(actor: &ChargePointActor, evse_id: usize, connector_id: usize) {
        for event in [
            ConnectorEvent::CableConnected,
            ConnectorEvent::LockConfirmed,
        ] {
            actor
                .send(ChargePointEvent::Evse {
                    evse_id,
                    event: EvseEvent::Connector {
                        connector_id,
                        event,
                    },
                })
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn starting_a_transaction_on_a_locked_connector_succeeds() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        lock_connector(&actor, 0, 0).await;

        let outcome = handle_request_start_transaction(&actor, Some(0)).await;

        assert_eq!(
            outcome,
            RequestStartTransactionOutcome::Accepted {
                transaction_id: TransactionId(0)
            }
        );
        assert_eq!(
            actor.state().evses[0].connectors[0],
            ConnectorState::Starting
        );
    }

    #[tokio::test]
    async fn no_evse_id_picks_the_first_locked_connector_on_any_evse() {
        let actor = ChargePointActor::spawn([1, 1], &TokioExecutor);
        lock_connector(&actor, 1, 0).await;

        let outcome = handle_request_start_transaction(&actor, None).await;

        assert_eq!(
            outcome,
            RequestStartTransactionOutcome::Accepted {
                transaction_id: TransactionId(0)
            }
        );
        assert_eq!(
            actor.state().evses[1].connectors[0],
            ConnectorState::Starting
        );
        assert_eq!(
            actor.state().evses[0].connectors[0],
            ConnectorState::Available
        );
    }

    #[tokio::test]
    async fn an_unknown_evse_is_rejected() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        lock_connector(&actor, 0, 0).await;

        let outcome = handle_request_start_transaction(&actor, Some(5)).await;

        assert_eq!(outcome, RequestStartTransactionOutcome::Rejected);
    }

    #[tokio::test]
    async fn no_locked_connector_is_rejected() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        assert_eq!(
            handle_request_start_transaction(&actor, None).await,
            RequestStartTransactionOutcome::Rejected
        );
        assert_eq!(
            handle_request_start_transaction(&actor, Some(0)).await,
            RequestStartTransactionOutcome::Rejected
        );
    }

    /// Spawns an actor with connector 0 `Charging` on a fresh transaction.
    async fn charging_actor() -> ChargePointActor {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        lock_connector(&actor, 0, 0).await;
        handle_request_start_transaction(&actor, Some(0)).await;
        actor
            .send(ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event: ConnectorEvent::ContactorClosed,
                },
            })
            .await
            .unwrap();
        actor
    }

    #[tokio::test]
    async fn stopping_a_charging_transaction_succeeds() {
        let actor = charging_actor().await;

        let outcome = handle_request_stop_transaction(&actor, TransactionId(0)).await;

        assert_eq!(outcome, RequestStopTransactionOutcome::Accepted);
        assert_eq!(
            actor.state().evses[0].connectors[0],
            ConnectorState::Stopping
        );
    }

    #[tokio::test]
    async fn an_unknown_transaction_id_is_rejected() {
        let actor = charging_actor().await;

        let outcome = handle_request_stop_transaction(&actor, TransactionId(99)).await;

        assert_eq!(outcome, RequestStopTransactionOutcome::Rejected);
    }

    #[tokio::test]
    async fn a_transaction_not_yet_charging_is_rejected() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        lock_connector(&actor, 0, 0).await;
        handle_request_start_transaction(&actor, Some(0)).await;
        // Still `Starting` here - the contactor hasn't confirmed closed yet.

        let outcome = handle_request_stop_transaction(&actor, TransactionId(0)).await;

        assert_eq!(outcome, RequestStopTransactionOutcome::Rejected);
        assert_eq!(
            actor.state().evses[0].connectors[0],
            ConnectorState::Starting
        );
    }

    type Seen = (u32, Vec<(usize, usize, ConnectorStatus)>);

    struct RecordingNotifier {
        seen: watch::Sender<Seen>,
    }

    impl RecordingNotifier {
        fn new() -> (Self, watch::Receiver<Seen>) {
            let (tx, rx) = watch::channel((0, Vec::new()));
            (Self { seen: tx }, rx)
        }
    }

    #[async_trait::async_trait]
    impl HeartbeatSender for RecordingNotifier {
        type Error = core::convert::Infallible;

        async fn send_heartbeat(&self) -> Result<(), Self::Error> {
            self.seen.send_modify(|(heartbeats, _)| *heartbeats += 1);
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl StatusNotifier for RecordingNotifier {
        type Error = core::convert::Infallible;

        async fn notify_status(
            &self,
            evse_id: usize,
            connector_id: usize,
            status: ConnectorStatus,
        ) -> Result<(), Self::Error> {
            self.seen
                .send_modify(|(_, statuses)| statuses.push((evse_id, connector_id, status)));
            Ok(())
        }
    }

    #[tokio::test]
    async fn triggering_a_heartbeat_sends_one_and_accepts() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let (notifier, seen) = RecordingNotifier::new();

        let outcome =
            handle_trigger_message(&actor, &notifier, TriggerableMessage::Heartbeat).await;

        assert_eq!(outcome, TriggerMessageOutcome::Accepted);
        assert_eq!(seen.borrow().0, 1);
    }

    #[tokio::test]
    async fn triggering_a_status_notification_for_one_connector_reports_only_that_connector() {
        let actor = ChargePointActor::spawn([2], &TokioExecutor);
        let (notifier, seen) = RecordingNotifier::new();

        let outcome = handle_trigger_message(
            &actor,
            &notifier,
            TriggerableMessage::StatusNotification(AvailabilityTarget::Connector {
                evse_id: 0,
                connector_id: 1,
            }),
        )
        .await;

        assert_eq!(outcome, TriggerMessageOutcome::Accepted);
        assert_eq!(
            seen.borrow().1,
            alloc::vec![(0, 1, ConnectorStatus::Available)]
        );
    }

    #[tokio::test]
    async fn triggering_a_status_notification_for_an_evse_reports_every_connector_on_it() {
        let actor = ChargePointActor::spawn([2], &TokioExecutor);
        let (notifier, seen) = RecordingNotifier::new();

        let outcome = handle_trigger_message(
            &actor,
            &notifier,
            TriggerableMessage::StatusNotification(AvailabilityTarget::Evse { evse_id: 0 }),
        )
        .await;

        assert_eq!(outcome, TriggerMessageOutcome::Accepted);
        assert_eq!(
            seen.borrow().1,
            alloc::vec![
                (0, 0, ConnectorStatus::Available),
                (0, 1, ConnectorStatus::Available)
            ]
        );
    }

    #[tokio::test]
    async fn triggering_a_status_notification_for_the_whole_charge_point_reports_every_connector() {
        let actor = ChargePointActor::spawn([1, 1], &TokioExecutor);
        let (notifier, seen) = RecordingNotifier::new();

        let outcome = handle_trigger_message(
            &actor,
            &notifier,
            TriggerableMessage::StatusNotification(AvailabilityTarget::ChargePoint),
        )
        .await;

        assert_eq!(outcome, TriggerMessageOutcome::Accepted);
        assert_eq!(
            seen.borrow().1,
            alloc::vec![
                (0, 0, ConnectorStatus::Available),
                (1, 0, ConnectorStatus::Available)
            ]
        );
    }

    #[tokio::test]
    async fn triggering_a_status_notification_for_an_unknown_connector_is_rejected() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let (notifier, seen) = RecordingNotifier::new();

        let outcome = handle_trigger_message(
            &actor,
            &notifier,
            TriggerableMessage::StatusNotification(AvailabilityTarget::Connector {
                evse_id: 0,
                connector_id: 5,
            }),
        )
        .await;

        assert_eq!(outcome, TriggerMessageOutcome::Rejected);
        assert!(seen.borrow().1.is_empty());
    }
}

#[cfg(feature = "ocpp_2_1")]
mod ocpp_2_1 {
    use super::{
        RequestStartTransactionHandler, RequestStartTransactionOutcome,
        RequestStopTransactionHandler, RequestStopTransactionOutcome, UnlockConnectorHandler,
        UnlockOutcome, handle_request_start_transaction, handle_request_stop_transaction,
        handle_unlock_request,
    };
    use crate::actor::ChargePointActor;
    use crate::state::TransactionId;
    use alloc::boxed::Box;
    use ocpp_client::ocpp_2_1::OCPP2_1Client;
    use ocpp_client::ocpp_types::v21::common::{RequestStartStopStatusEnum, UnlockStatusEnum};
    use ocpp_client::ocpp_types::v21::{
        RequestStartTransactionRequest, RequestStartTransactionResponse,
        RequestStopTransactionRequest, RequestStopTransactionResponse, UnlockConnectorRequest,
        UnlockConnectorResponse,
    };

    pub(super) fn map_outcome(outcome: UnlockOutcome) -> UnlockStatusEnum {
        match outcome {
            UnlockOutcome::Unlocked => UnlockStatusEnum::Unlocked,
            UnlockOutcome::UnlockFailed => UnlockStatusEnum::UnlockFailed,
            UnlockOutcome::OngoingAuthorizedTransaction => {
                UnlockStatusEnum::OngoingAuthorizedTransaction
            }
            UnlockOutcome::UnknownConnector => UnlockStatusEnum::UnknownConnector,
        }
    }

    /// A negative wire `evse_id`/`connector_id` can't address a connector - treated the same as
    /// an out-of-range one, without needing to consult the actor.
    fn connector_address(request: &UnlockConnectorRequest) -> Option<(usize, usize)> {
        let evse_id = usize::try_from(request.evse_id).ok()?;
        let connector_id = usize::try_from(request.connector_id).ok()?;
        Some((evse_id, connector_id))
    }

    #[async_trait::async_trait]
    impl UnlockConnectorHandler for OCPP2_1Client {
        async fn register_unlock_connector_handler(&self, actor: ChargePointActor) {
            self.on_unlock_connector(move |request, _client| {
                let actor = actor.clone();
                async move {
                    let outcome = match connector_address(&request) {
                        Some((evse_id, connector_id)) => {
                            handle_unlock_request(&actor, evse_id, connector_id).await
                        }
                        None => UnlockOutcome::UnknownConnector,
                    };
                    Ok(UnlockConnectorResponse {
                        custom_data: None,
                        status: map_outcome(outcome),
                        status_info: None,
                    })
                }
            })
            .await;
        }
    }

    fn map_start_outcome(
        outcome: RequestStartTransactionOutcome,
    ) -> (RequestStartStopStatusEnum, Option<heapless::String<36>>) {
        match outcome {
            RequestStartTransactionOutcome::Accepted { transaction_id } => (
                RequestStartStopStatusEnum::Accepted,
                // The transaction id is an internal `u64` formatted as decimal, always well
                // within the wire field's 36-byte bound.
                Some(
                    heapless::String::try_from(transaction_id.0)
                        .expect("u64 transaction id always fits in a 36-byte wire field"),
                ),
            ),
            RequestStartTransactionOutcome::Rejected => {
                (RequestStartStopStatusEnum::Rejected, None)
            }
        }
    }

    /// A negative wire `evse_id` can't address an EVSE - treated the same as an out-of-range
    /// one, without needing to consult the actor.
    fn parse_evse_id(request: &RequestStartTransactionRequest) -> Result<Option<usize>, ()> {
        match request.evse_id {
            None => Ok(None),
            Some(evse_id) => usize::try_from(evse_id).map(Some).map_err(|_| ()),
        }
    }

    #[async_trait::async_trait]
    impl RequestStartTransactionHandler for OCPP2_1Client {
        async fn register_request_start_transaction_handler(&self, actor: ChargePointActor) {
            self.on_request_start_transaction(move |request, _client| {
                let actor = actor.clone();
                async move {
                    let outcome = match parse_evse_id(&request) {
                        Ok(evse_id) => handle_request_start_transaction(&actor, evse_id).await,
                        Err(()) => RequestStartTransactionOutcome::Rejected,
                    };
                    let (status, transaction_id) = map_start_outcome(outcome);
                    Ok(RequestStartTransactionResponse {
                        custom_data: None,
                        status,
                        status_info: None,
                        transaction_id,
                    })
                }
            })
            .await;
        }
    }

    fn map_stop_outcome(outcome: RequestStopTransactionOutcome) -> RequestStartStopStatusEnum {
        match outcome {
            RequestStopTransactionOutcome::Accepted => RequestStartStopStatusEnum::Accepted,
            RequestStopTransactionOutcome::Rejected => RequestStartStopStatusEnum::Rejected,
        }
    }

    /// A `transaction_id` that doesn't parse as a `u64` can't address a transaction - treated
    /// the same as an unknown one, without needing to consult the actor.
    fn parse_transaction_id(request: &RequestStopTransactionRequest) -> Option<TransactionId> {
        request
            .transaction_id
            .parse::<u64>()
            .ok()
            .map(TransactionId)
    }

    #[async_trait::async_trait]
    impl RequestStopTransactionHandler for OCPP2_1Client {
        async fn register_request_stop_transaction_handler(&self, actor: ChargePointActor) {
            self.on_request_stop_transaction(move |request, _client| {
                let actor = actor.clone();
                async move {
                    let outcome = match parse_transaction_id(&request) {
                        Some(transaction_id) => {
                            handle_request_stop_transaction(&actor, transaction_id).await
                        }
                        None => RequestStopTransactionOutcome::Rejected,
                    };
                    Ok(RequestStopTransactionResponse {
                        custom_data: None,
                        status: map_stop_outcome(outcome),
                        status_info: None,
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
        fn every_outcome_maps_to_the_matching_wire_status() {
            assert_eq!(
                map_outcome(UnlockOutcome::Unlocked),
                UnlockStatusEnum::Unlocked
            );
            assert_eq!(
                map_outcome(UnlockOutcome::UnlockFailed),
                UnlockStatusEnum::UnlockFailed
            );
            assert_eq!(
                map_outcome(UnlockOutcome::OngoingAuthorizedTransaction),
                UnlockStatusEnum::OngoingAuthorizedTransaction
            );
            assert_eq!(
                map_outcome(UnlockOutcome::UnknownConnector),
                UnlockStatusEnum::UnknownConnector
            );
        }

        #[test]
        fn a_negative_wire_address_has_no_connector_address() {
            let request = UnlockConnectorRequest {
                custom_data: None,
                evse_id: -1,
                connector_id: 0,
            };

            assert_eq!(connector_address(&request), None);
        }

        #[test]
        fn an_accepted_outcome_maps_to_accepted_with_the_transaction_id() {
            assert_eq!(
                map_start_outcome(RequestStartTransactionOutcome::Accepted {
                    transaction_id: crate::state::TransactionId(7)
                }),
                (
                    RequestStartStopStatusEnum::Accepted,
                    Some(heapless::String::try_from("7").unwrap())
                )
            );
        }

        #[test]
        fn a_rejected_outcome_maps_to_rejected_with_no_transaction_id() {
            assert_eq!(
                map_start_outcome(RequestStartTransactionOutcome::Rejected),
                (RequestStartStopStatusEnum::Rejected, None)
            );
        }

        fn start_request(evse_id: Option<i64>) -> RequestStartTransactionRequest {
            RequestStartTransactionRequest {
                custom_data: None,
                evse_id,
                group_id_token: None,
                id_token: ocpp_client::ocpp_types::v21::common::IdToken {
                    additional_info: None,
                    id_token: heapless::String::try_from("04A224B2").unwrap(),
                    r#type: heapless::String::try_from("ISO14443").unwrap(),
                    custom_data: None,
                },
                remote_start_id: 1,
                charging_profile: None,
            }
        }

        // `RequestStartTransactionRequest` embeds `Option<ChargingProfile>`. `ocpp-types` 0.1.1
        // had a codegen defect where `ChargingProfile`/`ChargingSchedule` kept `heapless::Vec`
        // (not `alloc::vec::Vec`) for `charging_schedule`/`charging_schedule_period` even in the
        // `#[cfg(feature = "alloc")]` struct variant this crate builds with, inlining up to
        // 3 * 1024 nested structs and making the type itself ~80MB - enough to overflow an
        // ordinary thread's stack just building or holding one. `ocpp-types` 0.1.2 (pulled in via
        // `ocpp-client` 0.2) fixed the inner `charging_schedule_period` field to use
        // `alloc::vec::Vec`; `ChargingProfile.charging_schedule` itself still inlines up to 3
        // `ChargingSchedule`s via `heapless::Vec`, but that now measures ~56KB total (verified via
        // `size_of::<RequestStartTransactionRequest>()`) - well within any normal thread's stack,
        // so the oversized-stack workaround these tests used to need is gone.
        #[test]
        fn no_evse_id_parses_to_none() {
            assert_eq!(parse_evse_id(&start_request(None)), Ok(None));
        }

        #[test]
        fn a_valid_evse_id_parses() {
            assert_eq!(parse_evse_id(&start_request(Some(1))), Ok(Some(1)));
        }

        #[test]
        fn a_negative_evse_id_fails_to_parse() {
            assert_eq!(parse_evse_id(&start_request(Some(-1))), Err(()));
        }

        #[test]
        fn every_stop_outcome_maps_to_the_matching_wire_status() {
            assert_eq!(
                map_stop_outcome(RequestStopTransactionOutcome::Accepted),
                RequestStartStopStatusEnum::Accepted
            );
            assert_eq!(
                map_stop_outcome(RequestStopTransactionOutcome::Rejected),
                RequestStartStopStatusEnum::Rejected
            );
        }

        fn stop_request(transaction_id: &str) -> RequestStopTransactionRequest {
            RequestStopTransactionRequest {
                custom_data: None,
                transaction_id: heapless::String::try_from(transaction_id).unwrap(),
            }
        }

        #[test]
        fn a_valid_transaction_id_parses() {
            assert_eq!(
                parse_transaction_id(&stop_request("7")),
                Some(TransactionId(7))
            );
        }

        #[test]
        fn a_non_numeric_transaction_id_fails_to_parse() {
            assert_eq!(parse_transaction_id(&stop_request("not-a-number")), None);
        }
    }
}
