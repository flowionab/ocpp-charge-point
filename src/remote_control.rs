//! Remote control functional block: CSMS-initiated actions such as `UnlockConnector`. See
//! `docs/ROADMAP.md` §6.

use crate::actor::ChargePointActor;
use crate::availability::{AvailabilityTarget, StatusNotifier};
use crate::provisioning::HeartbeatSender;
use crate::state::{
    ChargePointEvent, ChargePointState, ConnectorEvent, ConnectorState, EvseEvent, IdToken,
    StopReason, TransactionId,
};
use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

#[cfg(feature = "ocpp_1_6")]
pub use self::ocpp_1_6::Ocpp1_6RemoteControlHandler;

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
    /// Registers an `UnlockConnector` handler with the CSMS connection that dispatches incoming
    /// requests to [`handle_unlock_request`] against `actor`.
    async fn register_unlock_connector_handler(&self, actor: ChargePointActor);
}

/// The outcome of a CSMS-initiated `RequestStartTransaction` request, matching (a subset of)
/// OCPP's `RequestStartStopStatusEnum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestStartTransactionOutcome {
    /// A transaction was started on the returned connector.
    Accepted {
        /// The started transaction's identifier.
        transaction_id: TransactionId,
    },
    /// No matching `Locked` connector was found, or the addressed EVSE doesn't exist.
    Rejected,
}

/// Handles a CSMS-initiated `RequestStartTransaction` request against `actor`: finds a `Locked`
/// connector (cable connected, no active transaction) on `evse_id` - or, if `evse_id` is
/// `None`, the first `Locked` connector on any EVSE - and starts a transaction on it directly,
/// without a separate Authorize round-trip (the CSMS's own request is itself the authorization
/// decision; see `ConnectorEvent::RemoteStartRequested`). `id_token` is the identifier the CSMS
/// supplied, recorded on the started `Transaction`. Rejects if `evse_id` is out of range, or no
/// matching connector is currently `Locked`.
pub async fn handle_request_start_transaction(
    actor: &ChargePointActor,
    evse_id: Option<usize>,
    id_token: IdToken,
) -> RequestStartTransactionOutcome {
    let Some((evse_id, connector_id)) = find_locked_connector(&actor.state(), evse_id) else {
        return RequestStartTransactionOutcome::Rejected;
    };

    let _ = actor
        .send(ChargePointEvent::Evse {
            evse_id,
            event: EvseEvent::Connector {
                connector_id,
                event: ConnectorEvent::RemoteStartRequested(id_token),
            },
        })
        .await;

    match &actor.state().evses[evse_id].transactions[connector_id] {
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
    /// Registers a `RequestStartTransaction` handler with the CSMS connection that dispatches
    /// incoming requests to [`handle_request_start_transaction`] against `actor`.
    async fn register_request_start_transaction_handler(&self, actor: ChargePointActor);
}

/// The outcome of a CSMS-initiated `RequestStopTransaction` request, matching (a subset of)
/// OCPP's `RequestStartStopStatusEnum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestStopTransactionOutcome {
    /// The transaction was stopped.
    Accepted,
    /// `transaction_id` was unknown, or its connector isn't currently `Charging`.
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
            .position(|transaction| transaction.as_ref().is_some_and(|t| t.id == transaction_id))
            .map(|connector_id| (evse_id, connector_id))
    })
}

/// Registers this charge point's inbound `RequestStopTransaction` handling with the CSMS
/// connection. Implemented per protocol version (see the `ocpp_2_1` module), mirroring
/// [`RequestStartTransactionHandler`].
#[async_trait::async_trait]
pub trait RequestStopTransactionHandler {
    /// Registers a `RequestStopTransaction` handler with the CSMS connection that dispatches
    /// incoming requests to [`handle_request_stop_transaction`] against `actor`.
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
    /// Re-sends `Heartbeat`.
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
    /// The charge point attempted to (re-)send the message.
    Accepted,
    /// The requested message addressed an EVSE/connector that doesn't exist.
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
                        (evse_id, connector_id, connector.availability_status(), *connector)
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
                    (evse_id, connector_id, connector.availability_status(), *connector)
                })
                .collect()
        }
        AvailabilityTarget::Connector {
            evse_id,
            connector_id,
        } => {
            let Some(connector) = state
                .evses
                .get(evse_id)
                .and_then(|evse| evse.connectors.get(connector_id))
            else {
                return TriggerMessageOutcome::Rejected;
            };
            vec![(evse_id, connector_id, connector.availability_status(), *connector)]
        }
    };

    for (evse_id, connector_id, status, connector_state) in addressed {
        if let Err(err) = notifier
            .notify_status(evse_id, connector_id, status, connector_state)
            .await
        {
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

    fn test_id_token() -> crate::state::IdToken {
        crate::state::IdToken {
            value: "04A224B2".into(),
            kind: crate::state::IdTokenKind::ISO14443,
        }
    }

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
                    event: ConnectorEvent::ChargingAuthorized(test_id_token()),
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

        let outcome = handle_request_start_transaction(&actor, Some(0), test_id_token()).await;

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

        let outcome = handle_request_start_transaction(&actor, None, test_id_token()).await;

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

        let outcome = handle_request_start_transaction(&actor, Some(5), test_id_token()).await;

        assert_eq!(outcome, RequestStartTransactionOutcome::Rejected);
    }

    #[tokio::test]
    async fn no_locked_connector_is_rejected() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        assert_eq!(
            handle_request_start_transaction(&actor, None, test_id_token()).await,
            RequestStartTransactionOutcome::Rejected
        );
        assert_eq!(
            handle_request_start_transaction(&actor, Some(0), test_id_token()).await,
            RequestStartTransactionOutcome::Rejected
        );
    }

    /// Spawns an actor with connector 0 `Charging` on a fresh transaction.
    async fn charging_actor() -> ChargePointActor {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        lock_connector(&actor, 0, 0).await;
        handle_request_start_transaction(&actor, Some(0), test_id_token()).await;
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
        handle_request_start_transaction(&actor, Some(0), test_id_token()).await;
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
            _connector_state: crate::state::ConnectorState,
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
    use crate::state::{IdToken, IdTokenKind, TransactionId};
    use alloc::boxed::Box;
    use alloc::string::ToString;
    use ocpp_client::ocpp_2_1::OCPP2_1Client;
    use ocpp_client::ocpp_types::v21::common::{RequestStartStopStatusEnum, UnlockStatusEnum};
    use ocpp_client::ocpp_types::v21::{
        RequestStartTransactionRequest, RequestStartTransactionResponse,
        RequestStopTransactionRequest, RequestStopTransactionResponse, UnlockConnectorRequest,
        UnlockConnectorResponse,
    };

    /// Mirrors [`crate::local_authorization_list::ocpp_2_1::map_id_token_kind`] - each `ocpp_2_1`
    /// submodule keeps its own copy of this small mapping rather than sharing one, matching this
    /// crate's existing per-block adapter convention.
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

    fn map_id_token(id_token: &ocpp_client::ocpp_types::v21::common::IdToken) -> IdToken {
        IdToken {
            value: id_token.id_token.to_string(),
            kind: map_id_token_kind(id_token.r#type.as_str()),
        }
    }

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
                        Ok(evse_id) => {
                            handle_request_start_transaction(
                                &actor,
                                evse_id,
                                map_id_token(&request.id_token),
                            )
                            .await
                        }
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

/// The OCPP 2.0.1 projection - identical `UnlockConnectorRequest`/`UnlockStatusEnum`/
/// `RequestStartTransactionRequest`/`RequestStopTransactionRequest`/`RequestStartStopStatusEnum`
/// wire shapes to 2.1's, so this is close to a copy of the 2.1 module - **except** `id_token`
/// mapping, which for 2.0.1 goes through the same closed 8-value `IdTokenEnum` (not a free-form
/// string) that [`crate::authorization::ocpp_2_0_1::map_id_token_kind`] maps *to*; this module
/// maps the *other* direction (a CSMS-supplied wire `IdTokenEnum` back to our internal
/// `IdTokenKind`, for `RequestStartTransaction`'s inbound `id_token`) - a different function,
/// since it's a different direction, but the same closed-enum shape to work around.
#[cfg(feature = "ocpp_2_0_1")]
pub(crate) mod ocpp_2_0_1 {
    use super::{
        RequestStartTransactionHandler, RequestStartTransactionOutcome,
        RequestStopTransactionHandler, RequestStopTransactionOutcome, UnlockConnectorHandler,
        UnlockOutcome, handle_request_start_transaction, handle_request_stop_transaction,
        handle_unlock_request,
    };
    use crate::actor::ChargePointActor;
    use crate::state::{IdToken, IdTokenKind, TransactionId};
    use alloc::boxed::Box;
    use alloc::string::ToString;
    use ocpp_client::ocpp_2_0_1::OCPP2_0_1Client;
    use ocpp_client::ocpp_types::v201::common::{
        IdTokenEnum, RequestStartStopStatusEnum, UnlockStatusEnum,
    };
    use ocpp_client::ocpp_types::v201::{
        RequestStartTransactionRequest, RequestStartTransactionResponse,
        RequestStopTransactionRequest, RequestStopTransactionResponse, UnlockConnectorRequest,
        UnlockConnectorResponse,
    };

    /// The reverse of [`crate::authorization::ocpp_2_0_1::map_id_token_kind`] - 2.0.1's
    /// `IdTokenEnumType` has no `DirectPayment`/`EVCCID`/`Vin` variant to map *from* either, so
    /// this is a total, lossless mapping (every wire variant has a matching internal kind), just
    /// not the reverse of a total mapping in the other direction.
    pub(crate) fn map_id_token_kind(kind: IdTokenEnum) -> IdTokenKind {
        match kind {
            IdTokenEnum::Central => IdTokenKind::Central,
            IdTokenEnum::EMAID => IdTokenKind::EMAID,
            IdTokenEnum::ISO14443 => IdTokenKind::ISO14443,
            IdTokenEnum::ISO15693 => IdTokenKind::ISO15693,
            IdTokenEnum::KeyCode => IdTokenKind::KeyCode,
            IdTokenEnum::Local => IdTokenKind::Local,
            IdTokenEnum::MacAddress => IdTokenKind::MacAddress,
            IdTokenEnum::NoAuthorization => IdTokenKind::NoAuthorization,
        }
    }

    fn map_id_token(id_token: &ocpp_client::ocpp_types::v201::common::IdToken) -> IdToken {
        IdToken {
            value: id_token.id_token.to_string(),
            kind: map_id_token_kind(id_token.r#type.clone()),
        }
    }

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

    fn connector_address(request: &UnlockConnectorRequest) -> Option<(usize, usize)> {
        let evse_id = usize::try_from(request.evse_id).ok()?;
        let connector_id = usize::try_from(request.connector_id).ok()?;
        Some((evse_id, connector_id))
    }

    #[async_trait::async_trait]
    impl UnlockConnectorHandler for OCPP2_0_1Client {
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

    fn parse_evse_id(request: &RequestStartTransactionRequest) -> Result<Option<usize>, ()> {
        match request.evse_id {
            None => Ok(None),
            Some(evse_id) => usize::try_from(evse_id).map(Some).map_err(|_| ()),
        }
    }

    #[async_trait::async_trait]
    impl RequestStartTransactionHandler for OCPP2_0_1Client {
        async fn register_request_start_transaction_handler(&self, actor: ChargePointActor) {
            self.on_request_start_transaction(move |request, _client| {
                let actor = actor.clone();
                async move {
                    let outcome = match parse_evse_id(&request) {
                        Ok(evse_id) => {
                            handle_request_start_transaction(
                                &actor,
                                evse_id,
                                map_id_token(&request.id_token),
                            )
                            .await
                        }
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

    fn parse_transaction_id(request: &RequestStopTransactionRequest) -> Option<TransactionId> {
        request
            .transaction_id
            .parse::<u64>()
            .ok()
            .map(TransactionId)
    }

    #[async_trait::async_trait]
    impl RequestStopTransactionHandler for OCPP2_0_1Client {
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
                id_token: ocpp_client::ocpp_types::v201::common::IdToken {
                    additional_info: None,
                    id_token: heapless::String::try_from("04A224B2").unwrap(),
                    r#type: IdTokenEnum::ISO14443,
                    custom_data: None,
                },
                remote_start_id: 1,
                charging_profile: None,
            }
        }

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

        #[test]
        fn every_wire_variant_maps_to_its_matching_internal_kind() {
            assert_eq!(map_id_token_kind(IdTokenEnum::Central), IdTokenKind::Central);
            assert_eq!(map_id_token_kind(IdTokenEnum::EMAID), IdTokenKind::EMAID);
            assert_eq!(
                map_id_token_kind(IdTokenEnum::ISO14443),
                IdTokenKind::ISO14443
            );
            assert_eq!(
                map_id_token_kind(IdTokenEnum::NoAuthorization),
                IdTokenKind::NoAuthorization
            );
        }
    }
}

/// The OCPP 1.6J projection of `UnlockConnectorHandler`/`RequestStartTransactionHandler`/
/// `RequestStopTransactionHandler`. `UnlockConnectorRequest`'s and `RemoteStartTransactionRequest`'s
/// flat `connectorId` need the same topology-aware translation `Ocpp1_6StatusNotifier`/
/// `Ocpp1_6TransactionNotifier` need for their own connector addressing, just in the opposite
/// direction (wire -> internal, via [`crate::topology::unflatten_ocpp_1_6_connector_id`], not
/// internal -> wire), so [`Ocpp1_6RemoteControlHandler`] wraps `OCPP1_6Client` with that topology
/// the same way those two wrap it with their own copy. 1.6J's `UnlockConnectorResponseStatus` is
/// also narrower than later versions' (`Unlocked`/`UnlockFailed`/`NotSupported` - no
/// `OngoingAuthorizedTransaction`/`UnknownConnector`), so `UnlockOutcome::
/// OngoingAuthorizedTransaction` collapses to `UnlockFailed` (can't unlock, same end result) and
/// `UnlockOutcome::UnknownConnector` maps to `NotSupported` (the operation doesn't apply to an
/// address that isn't a real connector) - a real, documented narrowing, not an oversight.
///
/// `RemoteStartTransactionRequest.connectorId` is *optional* (unlike `UnlockConnector`'s
/// mandatory one) and, when present, addresses a single flat connector - narrower than 2.x's
/// `evseId`, which only ever targets a whole EVSE. [`handle_request_start_transaction`] only
/// targets at EVSE granularity itself (picking the first `Locked` connector on it, same as every
/// other version's adapter), so a present `connectorId` is unflattened down to its EVSE half and
/// the specific connector within it is dropped; an absent one searches every EVSE, same as 2.x's
/// absent `evseId`. `RemoteStartTransactionRequest.idTag` has no type/kind metadata (see
/// `crate::id_tag`), so [`crate::id_tag::map_id_token`] fills in `IdTokenKind::Central`.
/// `RemoteStartTransactionResponse` has no `transactionId` field at all (unlike 2.x's optional
/// one) - 1.6J expects the CSMS to correlate the transaction from the `StartTransaction.conf`
/// that follows instead.
///
/// `RemoteStopTransactionRequest.transactionId` is a bare `i64` (not 2.x's stringified `u64`)
/// and needs no topology at all, so `RequestStopTransactionHandler` is implemented directly on
/// `OCPP1_6Client` rather than through the wrapper.
#[cfg(feature = "ocpp_1_6")]
mod ocpp_1_6 {
    use super::{
        RequestStartTransactionHandler, RequestStartTransactionOutcome,
        RequestStopTransactionHandler, RequestStopTransactionOutcome, UnlockConnectorHandler,
        UnlockOutcome, handle_request_start_transaction, handle_request_stop_transaction,
        handle_unlock_request,
    };
    use crate::actor::ChargePointActor;
    use crate::id_tag::map_id_token;
    use crate::state::TransactionId;
    use crate::topology::unflatten_ocpp_1_6_connector_id;
    use alloc::boxed::Box;
    use alloc::vec::Vec;
    use ocpp_client::ocpp_1_6::OCPP1_6Client;
    use ocpp_client::ocpp_types::v16::common::{
        RemoteStartTransactionResponseStatus, RemoteStopTransactionResponseStatus,
        UnlockConnectorResponseStatus,
    };
    use ocpp_client::ocpp_types::v16::{
        RemoteStartTransactionRequest, RemoteStartTransactionResponse,
        RemoteStopTransactionResponse, UnlockConnectorResponse,
    };

    pub(super) fn map_outcome(outcome: UnlockOutcome) -> UnlockConnectorResponseStatus {
        match outcome {
            UnlockOutcome::Unlocked => UnlockConnectorResponseStatus::Unlocked,
            UnlockOutcome::UnlockFailed | UnlockOutcome::OngoingAuthorizedTransaction => {
                UnlockConnectorResponseStatus::UnlockFailed
            }
            UnlockOutcome::UnknownConnector => UnlockConnectorResponseStatus::NotSupported,
        }
    }

    pub(super) fn map_start_outcome(
        outcome: RequestStartTransactionOutcome,
    ) -> RemoteStartTransactionResponseStatus {
        match outcome {
            RequestStartTransactionOutcome::Accepted { .. } => {
                RemoteStartTransactionResponseStatus::Accepted
            }
            RequestStartTransactionOutcome::Rejected => {
                RemoteStartTransactionResponseStatus::Rejected
            }
        }
    }

    pub(super) fn map_stop_outcome(
        outcome: RequestStopTransactionOutcome,
    ) -> RemoteStopTransactionResponseStatus {
        match outcome {
            RequestStopTransactionOutcome::Accepted => RemoteStopTransactionResponseStatus::Accepted,
            RequestStopTransactionOutcome::Rejected => RemoteStopTransactionResponseStatus::Rejected,
        }
    }

    /// `Ok(None)` means "search every EVSE" (no `connectorId` on the wire); `Ok(Some(evse_id))`
    /// means the request's `connectorId` resolved to that EVSE; `Err(())` means it didn't
    /// address a real connector under `connector_counts` and the request must be rejected
    /// outright, not treated as "search every EVSE."
    pub(super) fn parse_evse_id(
        connector_counts: &[usize],
        request: &RemoteStartTransactionRequest,
    ) -> Result<Option<usize>, ()> {
        match request.connector_id {
            None => Ok(None),
            Some(connector_id) => unflatten_ocpp_1_6_connector_id(connector_counts, connector_id)
                .map(|(evse_id, _)| Some(evse_id))
                .ok_or(()),
        }
    }

    /// Wraps an `OCPP1_6Client` with the charge point's connector topology, needed to translate
    /// `UnlockConnectorRequest`'s/`RemoteStartTransactionRequest`'s flat `connectorId` into this
    /// crate's `(evse_id, connector_id)` addressing - see this module's docs.
    pub struct Ocpp1_6RemoteControlHandler {
        client: OCPP1_6Client,
        connector_counts: Vec<usize>,
    }

    impl Ocpp1_6RemoteControlHandler {
        /// Wraps `client`, capturing `connector_counts` (each EVSE's connector count, in
        /// `evse_id` order) for translating connector addresses on every request.
        pub fn new(client: OCPP1_6Client, connector_counts: impl IntoIterator<Item = usize>) -> Self {
            Self {
                client,
                connector_counts: connector_counts.into_iter().collect(),
            }
        }
    }

    #[async_trait::async_trait]
    impl UnlockConnectorHandler for Ocpp1_6RemoteControlHandler {
        async fn register_unlock_connector_handler(&self, actor: ChargePointActor) {
            let connector_counts = self.connector_counts.clone();
            self.client
                .on_unlock_connector(move |request, _client| {
                    let actor = actor.clone();
                    let connector_counts = connector_counts.clone();
                    async move {
                        let outcome = match unflatten_ocpp_1_6_connector_id(
                            &connector_counts,
                            request.connector_id,
                        ) {
                            Some((evse_id, connector_id)) => {
                                handle_unlock_request(&actor, evse_id, connector_id).await
                            }
                            None => UnlockOutcome::UnknownConnector,
                        };
                        Ok(UnlockConnectorResponse {
                            status: map_outcome(outcome),
                        })
                    }
                })
                .await;
        }
    }

    #[async_trait::async_trait]
    impl RequestStartTransactionHandler for Ocpp1_6RemoteControlHandler {
        async fn register_request_start_transaction_handler(&self, actor: ChargePointActor) {
            let connector_counts = self.connector_counts.clone();
            self.client
                .on_remote_start_transaction(move |request, _client| {
                    let actor = actor.clone();
                    let connector_counts = connector_counts.clone();
                    async move {
                        let outcome = match parse_evse_id(&connector_counts, &request) {
                            Ok(evse_id) => {
                                handle_request_start_transaction(
                                    &actor,
                                    evse_id,
                                    map_id_token(&request.id_tag),
                                )
                                .await
                            }
                            Err(()) => RequestStartTransactionOutcome::Rejected,
                        };
                        Ok(RemoteStartTransactionResponse {
                            status: map_start_outcome(outcome),
                        })
                    }
                })
                .await;
        }
    }

    #[async_trait::async_trait]
    impl RequestStopTransactionHandler for OCPP1_6Client {
        async fn register_request_stop_transaction_handler(&self, actor: ChargePointActor) {
            self.on_remote_stop_transaction(move |request, _client| {
                let actor = actor.clone();
                async move {
                    let outcome = match u64::try_from(request.transaction_id) {
                        Ok(transaction_id) => {
                            handle_request_stop_transaction(&actor, TransactionId(transaction_id))
                                .await
                        }
                        Err(_) => RequestStopTransactionOutcome::Rejected,
                    };
                    Ok(RemoteStopTransactionResponse {
                        status: map_stop_outcome(outcome),
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
        fn every_unlock_outcome_maps_to_a_wire_status() {
            assert_eq!(
                map_outcome(UnlockOutcome::Unlocked),
                UnlockConnectorResponseStatus::Unlocked
            );
            assert_eq!(
                map_outcome(UnlockOutcome::UnlockFailed),
                UnlockConnectorResponseStatus::UnlockFailed
            );
            assert_eq!(
                map_outcome(UnlockOutcome::OngoingAuthorizedTransaction),
                UnlockConnectorResponseStatus::UnlockFailed
            );
            assert_eq!(
                map_outcome(UnlockOutcome::UnknownConnector),
                UnlockConnectorResponseStatus::NotSupported
            );
        }

        #[test]
        fn every_start_outcome_maps_to_a_wire_status() {
            assert_eq!(
                map_start_outcome(RequestStartTransactionOutcome::Accepted {
                    transaction_id: TransactionId(7)
                }),
                RemoteStartTransactionResponseStatus::Accepted
            );
            assert_eq!(
                map_start_outcome(RequestStartTransactionOutcome::Rejected),
                RemoteStartTransactionResponseStatus::Rejected
            );
        }

        #[test]
        fn every_stop_outcome_maps_to_a_wire_status() {
            assert_eq!(
                map_stop_outcome(RequestStopTransactionOutcome::Accepted),
                RemoteStopTransactionResponseStatus::Accepted
            );
            assert_eq!(
                map_stop_outcome(RequestStopTransactionOutcome::Rejected),
                RemoteStopTransactionResponseStatus::Rejected
            );
        }

        fn request(connector_id: Option<i64>) -> RemoteStartTransactionRequest {
            RemoteStartTransactionRequest {
                charging_profile: None,
                connector_id,
                id_tag: ocpp_client::ocpp_types::v16::IdTag::try_from("04A224B2").unwrap(),
            }
        }

        #[test]
        fn a_missing_connector_id_searches_every_evse() {
            let connector_counts = [1, 1];

            assert_eq!(parse_evse_id(&connector_counts, &request(None)), Ok(None));
        }

        #[test]
        fn a_present_connector_id_resolves_to_its_evse() {
            let connector_counts = [2, 1];

            assert_eq!(
                parse_evse_id(&connector_counts, &request(Some(3))),
                Ok(Some(1))
            );
        }

        #[test]
        fn an_out_of_range_connector_id_is_rejected() {
            let connector_counts = [1, 1];

            assert_eq!(parse_evse_id(&connector_counts, &request(Some(5))), Err(()));
        }

        #[test]
        fn ocpp1_6_remote_control_handler_implements_the_handler_traits() {
            fn assert_impl<T: UnlockConnectorHandler + RequestStartTransactionHandler>() {}
            assert_impl::<Ocpp1_6RemoteControlHandler>();
            fn assert_stop_impl<T: RequestStopTransactionHandler>() {}
            assert_stop_impl::<OCPP1_6Client>();
        }
    }
}
