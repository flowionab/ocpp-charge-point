//! Remote control functional block: CSMS-initiated actions such as `UnlockConnector`. See
//! `docs/ROADMAP.md` §6.

use crate::actor::ChargePointActor;
use crate::availability::{AvailabilityTarget, StatusNotifier};
use crate::provisioning::HeartbeatSender;
use crate::replay_protection::ReplayGuard;
use crate::security::report_security_event;
use crate::state::{
    ChargePointEvent, ChargePointState, ConnectorEvent, ConnectorState, EvseEvent, IdToken,
    SecurityEvent, SecurityEventType, StopReason, TransactionId,
};
use alloc::boxed::Box;
use alloc::format;
use alloc::vec;
use alloc::vec::Vec;

#[cfg(feature = "ocpp_1_6")]
pub use self::ocpp_1_6::{Ocpp1_6RemoteControlHandler, Ocpp1_6TriggerMessageHandler};

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
#[tracing::instrument(skip_all, fields(evse_id, connector_id))]
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
        tracing::warn!("UnlockConnector named a connector this charge point does not have");
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

/// Registers this charge point's inbound `TriggerMessage` handling with the CSMS connection.
/// Implemented per protocol version, mirroring [`UnlockConnectorHandler`].
///
/// The implementing type is also the notifier the re-send goes out through - a `TriggerMessage`
/// asking for a Heartbeat is answered by *sending a Heartbeat*, so the handler needs the same
/// connection it was asked on, not a second one.
#[async_trait::async_trait]
pub trait TriggerMessageHandler {
    /// Registers a `TriggerMessage` handler dispatching against `actor`.
    async fn register_trigger_message_handler(&self, actor: ChargePointActor);
}

/// Registers this charge point's inbound `ExtendedTriggerMessage` handling (D2.2) - 1.6J's
/// Security Whitepaper counterpart to [`TriggerMessageHandler`]'s `TriggerMessage`.
///
/// A separate trait, not a variant of [`TriggerMessageHandler`]: the two are distinct wire
/// actions, on distinct `MessageTriggerEnumType`-shaped enums
/// (`ExtendedTriggerMessageRequestRequestedMessage` adds `LogStatusNotification` and
/// `SignChargePointCertificate` where `TriggerMessageRequestRequestedMessage` has
/// `DiagnosticsStatusNotification`, and is missing from that enum in 2.x entirely - see
/// `docs/PRODUCTION-ROADMAP.md` D2.2), registered against a different `on_*` callback on
/// `OCPP1_6Client`. They share [`TriggerableMessage`]/[`handle_trigger_message`] rather than
/// forking them: both wire enums still only name two things this crate can actually resend
/// (`Heartbeat`, `StatusNotification`), and each wire enum's own type keeps a value valid for one
/// action from ever being accepted by the other - there is no shared "trigger code" a mismatched
/// value could slip through.
#[async_trait::async_trait]
pub trait ExtendedTriggerMessageHandler {
    /// Registers an `ExtendedTriggerMessage` handler dispatching against `actor`.
    async fn register_extended_trigger_message_handler(&self, actor: ChargePointActor);
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
#[tracing::instrument(skip_all, fields(evse_id))]
pub async fn handle_request_start_transaction(
    actor: &ChargePointActor,
    evse_id: Option<usize>,
    id_token: IdToken,
) -> RequestStartTransactionOutcome {
    let Some((evse_id, connector_id)) = find_locked_connector(&actor.state(), evse_id) else {
        // The usual cause of a refused RemoteStart, and invisible until now: the CSMS asked to
        // start a transaction on a connector with no cable latched.
        tracing::warn!("refusing RequestStartTransaction: no locked connector is available");
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
#[tracing::instrument(skip_all, fields(transaction_id = transaction_id.0))]
pub async fn handle_request_stop_transaction(
    actor: &ChargePointActor,
    transaction_id: TransactionId,
) -> RequestStopTransactionOutcome {
    let state = actor.state();
    let Some((evse_id, connector_id)) = find_transaction(&state, transaction_id) else {
        tracing::warn!("refusing RequestStopTransaction: no such transaction is running here");
        return RequestStopTransactionOutcome::Rejected;
    };
    if state.evses[evse_id].connectors[connector_id] != ConnectorState::Charging {
        tracing::warn!(
            connector_state = ?state.evses[evse_id].connectors[connector_id],
            "refusing RequestStopTransaction: the connector is not charging"
        );
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

/// Same as [`handle_request_stop_transaction`], but additionally reports
/// [`SecurityEventType::AttemptedReplayAttacks`] when `transaction_id` is being asked to stop
/// again after `guard` already recorded a successful stop for it - see
/// [`crate::replay_protection`] for why `TransactionId` is the one CSMS-initiated remote-control
/// key this crate currently treats as strong enough evidence of a replay, and why the report
/// never changes the returned outcome.
#[tracing::instrument(skip_all)]
pub async fn handle_request_stop_transaction_with_replay_guard(
    actor: &ChargePointActor,
    guard: &ReplayGuard<TransactionId>,
    transaction_id: TransactionId,
) -> RequestStopTransactionOutcome {
    let outcome = handle_request_stop_transaction(actor, transaction_id).await;
    match outcome {
        RequestStopTransactionOutcome::Accepted => guard.record(transaction_id),
        RequestStopTransactionOutcome::Rejected if guard.contains(&transaction_id) => {
            report_security_event(
                actor,
                SecurityEvent {
                    event_type: SecurityEventType::AttemptedReplayAttacks,
                    tech_info: Some(format!(
                        "RequestStopTransaction repeated for already-stopped transaction {}",
                        transaction_id.0
                    )),
                },
            )
            .await;
        }
        RequestStopTransactionOutcome::Rejected => {}
    }
    outcome
}

/// Registers this charge point's inbound `RequestStopTransaction` handling with the CSMS
/// connection. Implemented per protocol version (see the `ocpp_2_1` module), mirroring
/// [`RequestStartTransactionHandler`].
#[async_trait::async_trait]
pub trait RequestStopTransactionHandler {
    /// Registers a `RequestStopTransaction` handler with the CSMS connection that dispatches
    /// incoming requests to [`handle_request_stop_transaction_with_replay_guard`] against
    /// `actor`, keyed on a guard private to this registration.
    async fn register_request_stop_transaction_handler(&self, actor: ChargePointActor);
}

/// A CSMS-initiated `TriggerMessage` request to (re-)send a specific outbound message - the
/// subset of OCPP's `MessageTriggerEnumType` this crate can currently fulfil, each backed by an
/// outbound trait it already has (`HeartbeatSender`, `StatusNotifier`). Everything else
/// (`BootNotification` - needs hardware vendor/model this module has no access to;
/// `MeterValues`/`TransactionEvent` - needs a "resend current snapshot" capability neither
/// functional block has yet; firmware/log/certificate triggers, `CustomTrigger` - no supporting
/// functional block exists at all, §1/§12) has no internal representation to construct here.
/// All three versions can now reach this from the network - see
/// [`TriggerMessageHandler`] and the `ocpp_1_6`/`ocpp_2_0_1`/`ocpp_2_1` adapters below. (An
/// earlier revision of these docs said the 2.1 wire types did not exist; they do, in the
/// `ocpp-types`/`ocpp-client` versions this crate pins - see `docs/PRODUCTION-ROADMAP.md` D1.3
/// for that correction.)
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
#[tracing::instrument(skip_all)]
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
            // The `currentTime` this triggered heartbeat's response carries isn't evaluated for
            // a time-sync step here - unlike `crate::provisioning::run_heartbeat`'s regular
            // cadence, a `TriggerMessage`-driven resend has no natural place to keep a
            // `MonotonicClock` reading anchored, and a CSMS explicitly requesting a resend is not
            // the routine per-interval case `evaluate_time_sync`'s threshold is tuned for.
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
                        (
                            evse_id,
                            connector_id,
                            connector.availability_status(),
                            *connector,
                        )
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
                    (
                        evse_id,
                        connector_id,
                        connector.availability_status(),
                        *connector,
                    )
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
            vec![(
                evse_id,
                connector_id,
                connector.availability_status(),
                *connector,
            )]
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
        handle_request_stop_transaction, handle_request_stop_transaction_with_replay_guard,
        handle_trigger_message, handle_unlock_request,
    };
    use crate::actor::ChargePointActor;
    use crate::availability::{AvailabilityTarget, StatusNotifier};
    use crate::executor::TokioExecutor;
    use crate::provisioning::HeartbeatSender;
    use crate::replay_protection::ReplayGuard;
    use crate::state::{
        ChargePointEvent, ConnectorEvent, ConnectorState, ConnectorStatus, EvseEvent,
        HardwareCommand, SecurityEventType, TransactionId,
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

    #[tokio::test]
    async fn repeating_a_completed_stop_reports_a_replay_attempt() {
        let actor = charging_actor().await;
        let mut security_events = actor.subscribe_security_events();
        let guard = ReplayGuard::new();

        let first =
            handle_request_stop_transaction_with_replay_guard(&actor, &guard, TransactionId(0))
                .await;
        assert_eq!(first, RequestStopTransactionOutcome::Accepted);

        let second =
            handle_request_stop_transaction_with_replay_guard(&actor, &guard, TransactionId(0))
                .await;

        assert_eq!(second, RequestStopTransactionOutcome::Rejected);
        let event = security_events.recv().await.unwrap();
        assert_eq!(event.event_type, SecurityEventType::AttemptedReplayAttacks);
    }

    #[tokio::test]
    async fn an_unrelated_rejection_reports_nothing() {
        let actor = charging_actor().await;
        let mut security_events = actor.subscribe_security_events();
        let guard = ReplayGuard::new();

        // Never accepted before, so this is an ordinary unknown-transaction rejection, not a
        // replay of anything this guard has seen - it must not be reported.
        let outcome =
            handle_request_stop_transaction_with_replay_guard(&actor, &guard, TransactionId(99))
                .await;

        assert_eq!(outcome, RequestStopTransactionOutcome::Rejected);
        assert!(
            tokio::time::timeout(
                core::time::Duration::from_millis(50),
                security_events.recv()
            )
            .await
            .is_err(),
            "an ordinary rejection must not be reported as a replay"
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

        async fn send_heartbeat(
            &self,
        ) -> Result<Option<chrono::DateTime<chrono::Utc>>, Self::Error> {
            self.seen.send_modify(|(heartbeats, _)| *heartbeats += 1);
            Ok(None)
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

    // --- TriggerMessage (B1.4) ---

    /// Maps a wire `evse` field onto this crate's [`AvailabilityTarget`]. Absent means the whole
    /// charge point; `id`/`connectorId` are 1-based on the wire and 0-based here.
    ///
    /// `Err(())` is an address that cannot exist - a non-positive id - which the caller answers
    /// with `Rejected` rather than silently widening it to "the whole charge point".
    // Only called from the `std`-gated `TriggerMessageHandler` impl below (see its cfg for why).
    #[cfg(feature = "std")]
    fn trigger_target(evse: Option<&EVSE>) -> Result<AvailabilityTarget, ()> {
        let Some(evse) = evse else {
            return Ok(AvailabilityTarget::ChargePoint);
        };
        let evse_id = usize::try_from(evse.id)
            .map_err(|_| ())?
            .checked_sub(1)
            .ok_or(())?;
        match evse.connector_id {
            None => Ok(AvailabilityTarget::Evse { evse_id }),
            Some(connector_id) => {
                let connector_id = usize::try_from(connector_id)
                    .map_err(|_| ())?
                    .checked_sub(1)
                    .ok_or(())?;
                Ok(AvailabilityTarget::Connector {
                    evse_id,
                    connector_id,
                })
            }
        }
    }

    /// Maps a wire `requestedMessage` onto this crate's [`TriggerableMessage`], or `None` for a
    /// value no functional block here can fulfil - which is exactly OCPP's `NotImplemented`, and
    /// why [`TriggerMessageOutcome`] has no variant for it (see that type's docs): the
    /// distinction only exists at the wire, where the unsupported values live.
    #[cfg(feature = "std")]
    fn triggerable_message(
        requested: &MessageTriggerEnum,
        target: AvailabilityTarget,
    ) -> Option<TriggerableMessage> {
        match requested {
            MessageTriggerEnum::Heartbeat => Some(TriggerableMessage::Heartbeat),
            MessageTriggerEnum::StatusNotification => {
                Some(TriggerableMessage::StatusNotification(target))
            }
            _ => None,
        }
    }

    #[cfg(feature = "std")]
    fn trigger_response(status: TriggerMessageStatusEnum) -> TriggerMessageResponse {
        TriggerMessageResponse {
            custom_data: None,
            status,
            status_info: None,
        }
    }

    // `std`-gated: the notifier is `self` (the bare client), and `OCPP2_1Client` only implements
    // `StatusNotifier` under `std` (it sources the StatusNotification timestamp from
    // `crate::clock::SystemClock` - see `crate::availability`'s "std convenience" impl). Without
    // `std`, wrap the client in `Ocpp2_1StatusNotifier::with_clock` and implement
    // `TriggerMessageHandler` against that instead.
    #[cfg(feature = "std")]
    #[async_trait::async_trait]
    impl TriggerMessageHandler for OCPP2_1Client {
        async fn register_trigger_message_handler(&self, actor: ChargePointActor) {
            // The client is both the handler and the notifier the re-send goes out through: a
            // `TriggerMessage` asking for a Heartbeat is answered by sending one, on this same
            // connection.
            let notifier = self.clone();
            self.on_trigger_message(move |request: TriggerMessageRequest, _client| {
                let actor = actor.clone();
                let notifier = notifier.clone();
                async move {
                    let Ok(target) = trigger_target(request.evse.as_ref()) else {
                        return Ok(trigger_response(TriggerMessageStatusEnum::Rejected));
                    };
                    let Some(message) = triggerable_message(&request.requested_message, target)
                    else {
                        return Ok(trigger_response(TriggerMessageStatusEnum::NotImplemented));
                    };
                    let outcome = handle_trigger_message(&actor, &notifier, message).await;
                    Ok(trigger_response(match outcome {
                        TriggerMessageOutcome::Accepted => TriggerMessageStatusEnum::Accepted,
                        TriggerMessageOutcome::Rejected => TriggerMessageStatusEnum::Rejected,
                    }))
                }
            })
            .await;
        }
    }

    #[cfg(test)]
    mod trigger_message_tests {
        use super::*;

        fn evse(id: i64, connector_id: Option<i64>) -> EVSE {
            EVSE {
                connector_id,
                custom_data: None,
                id,
            }
        }

        #[test]
        fn an_absent_evse_addresses_the_whole_charge_point() {
            assert_eq!(trigger_target(None), Ok(AvailabilityTarget::ChargePoint));
        }

        #[test]
        fn wire_ids_are_one_based_and_this_crates_are_zero_based() {
            assert_eq!(
                trigger_target(Some(&evse(1, None))),
                Ok(AvailabilityTarget::Evse { evse_id: 0 })
            );
            assert_eq!(
                trigger_target(Some(&evse(2, Some(1)))),
                Ok(AvailabilityTarget::Connector {
                    evse_id: 1,
                    connector_id: 0
                })
            );
        }

        #[test]
        fn an_address_that_cannot_exist_is_rejected_rather_than_widened() {
            // `0` and negatives are not valid wire ids; treating either as "the whole charge
            // point" would answer a request the CSMS did not make.
            assert_eq!(trigger_target(Some(&evse(0, None))), Err(()));
            assert_eq!(trigger_target(Some(&evse(-1, None))), Err(()));
            assert_eq!(trigger_target(Some(&evse(1, Some(0)))), Err(()));
        }

        #[test]
        fn the_two_messages_this_crate_can_resend_map_and_the_rest_are_not_implemented() {
            assert_eq!(
                triggerable_message(
                    &MessageTriggerEnum::Heartbeat,
                    AvailabilityTarget::ChargePoint
                ),
                Some(TriggerableMessage::Heartbeat)
            );
            assert_eq!(
                triggerable_message(
                    &MessageTriggerEnum::StatusNotification,
                    AvailabilityTarget::Evse { evse_id: 0 }
                ),
                Some(TriggerableMessage::StatusNotification(
                    AvailabilityTarget::Evse { evse_id: 0 }
                ))
            );
            // Everything else needs a functional block this crate doesn't have. Reported as
            // NotImplemented rather than Rejected, which would claim the request was understood
            // and refused.
            for requested in [
                MessageTriggerEnum::BootNotification,
                MessageTriggerEnum::MeterValues,
                MessageTriggerEnum::TransactionEvent,
                MessageTriggerEnum::FirmwareStatusNotification,
                MessageTriggerEnum::LogStatusNotification,
            ] {
                assert_eq!(
                    triggerable_message(&requested, AvailabilityTarget::ChargePoint),
                    None
                );
            }
        }
    }

    use super::{
        RequestStartTransactionHandler, RequestStartTransactionOutcome,
        RequestStopTransactionHandler, RequestStopTransactionOutcome, UnlockConnectorHandler,
        UnlockOutcome, handle_request_start_transaction,
        handle_request_stop_transaction_with_replay_guard, handle_unlock_request,
    };
    // Only used by the `std`-gated `impl TriggerMessageHandler for OCPP2_1Client` above (see its
    // cfg for why) and by `trigger_message_tests`.
    #[cfg(feature = "std")]
    use super::{
        TriggerMessageHandler, TriggerMessageOutcome, TriggerableMessage, handle_trigger_message,
    };
    use crate::actor::ChargePointActor;
    #[cfg(feature = "std")]
    use crate::availability::AvailabilityTarget;
    use crate::replay_protection::ReplayGuard;
    use crate::state::{IdToken, IdTokenKind, TransactionId};
    #[cfg(feature = "std")]
    use crate::wire::v21::common::{EVSE, MessageTriggerEnum, TriggerMessageStatusEnum};
    use crate::wire::v21::common::{RequestStartStopStatusEnum, UnlockStatusEnum};
    use crate::wire::v21::{
        RequestStartTransactionRequest, RequestStartTransactionResponse,
        RequestStopTransactionRequest, RequestStopTransactionResponse, UnlockConnectorRequest,
        UnlockConnectorResponse,
    };
    #[cfg(feature = "std")]
    use crate::wire::v21::{TriggerMessageRequest, TriggerMessageResponse};
    use alloc::boxed::Box;
    use alloc::string::ToString;
    use alloc::sync::Arc;
    use ocpp_client::ocpp_2_1::OCPP2_1Client;

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

    fn map_id_token(id_token: &crate::wire::v21::common::IdToken) -> IdToken {
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
            let guard = Arc::new(ReplayGuard::new());
            self.on_request_stop_transaction(move |request, _client| {
                let actor = actor.clone();
                let guard = guard.clone();
                async move {
                    let outcome = match parse_transaction_id(&request) {
                        Some(transaction_id) => {
                            handle_request_stop_transaction_with_replay_guard(
                                &actor,
                                &guard,
                                transaction_id,
                            )
                            .await
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
                id_token: crate::wire::v21::common::IdToken {
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

    // --- TriggerMessage (B1.3) ---

    /// Maps a wire `evse` field onto this crate's [`AvailabilityTarget`]. Absent means the whole
    /// charge point; `id`/`connectorId` are 1-based on the wire and 0-based here.
    ///
    /// `Err(())` is an address that cannot exist - a non-positive id - which the caller answers
    /// with `Rejected` rather than silently widening it to "the whole charge point".
    // Only called from the `std`-gated `TriggerMessageHandler` impl below (see its cfg for why).
    #[cfg(feature = "std")]
    fn trigger_target(evse: Option<&EVSE>) -> Result<AvailabilityTarget, ()> {
        let Some(evse) = evse else {
            return Ok(AvailabilityTarget::ChargePoint);
        };
        let evse_id = usize::try_from(evse.id)
            .map_err(|_| ())?
            .checked_sub(1)
            .ok_or(())?;
        match evse.connector_id {
            None => Ok(AvailabilityTarget::Evse { evse_id }),
            Some(connector_id) => {
                let connector_id = usize::try_from(connector_id)
                    .map_err(|_| ())?
                    .checked_sub(1)
                    .ok_or(())?;
                Ok(AvailabilityTarget::Connector {
                    evse_id,
                    connector_id,
                })
            }
        }
    }

    /// Maps a wire `requestedMessage` onto this crate's [`TriggerableMessage`], or `None` for a
    /// value no functional block here can fulfil - which is exactly OCPP's `NotImplemented`, and
    /// why [`TriggerMessageOutcome`] has no variant for it (see that type's docs): the
    /// distinction only exists at the wire, where the unsupported values live.
    #[cfg(feature = "std")]
    fn triggerable_message(
        requested: &MessageTriggerEnum,
        target: AvailabilityTarget,
    ) -> Option<TriggerableMessage> {
        match requested {
            MessageTriggerEnum::Heartbeat => Some(TriggerableMessage::Heartbeat),
            MessageTriggerEnum::StatusNotification => {
                Some(TriggerableMessage::StatusNotification(target))
            }
            _ => None,
        }
    }

    #[cfg(feature = "std")]
    fn trigger_response(status: TriggerMessageStatusEnum) -> TriggerMessageResponse {
        TriggerMessageResponse {
            custom_data: None,
            status,
            status_info: None,
        }
    }

    // `std`-gated: see the matching note on the `OCPP2_1Client` impl above - `OCPP2_0_1Client`
    // only implements `StatusNotifier` under `std`.
    #[cfg(feature = "std")]
    #[async_trait::async_trait]
    impl TriggerMessageHandler for OCPP2_0_1Client {
        async fn register_trigger_message_handler(&self, actor: ChargePointActor) {
            // The client is both the handler and the notifier the re-send goes out through: a
            // `TriggerMessage` asking for a Heartbeat is answered by sending one, on this same
            // connection.
            let notifier = self.clone();
            self.on_trigger_message(move |request: TriggerMessageRequest, _client| {
                let actor = actor.clone();
                let notifier = notifier.clone();
                async move {
                    let Ok(target) = trigger_target(request.evse.as_ref()) else {
                        return Ok(trigger_response(TriggerMessageStatusEnum::Rejected));
                    };
                    let Some(message) = triggerable_message(&request.requested_message, target)
                    else {
                        return Ok(trigger_response(TriggerMessageStatusEnum::NotImplemented));
                    };
                    let outcome = handle_trigger_message(&actor, &notifier, message).await;
                    Ok(trigger_response(match outcome {
                        TriggerMessageOutcome::Accepted => TriggerMessageStatusEnum::Accepted,
                        TriggerMessageOutcome::Rejected => TriggerMessageStatusEnum::Rejected,
                    }))
                }
            })
            .await;
        }
    }

    #[cfg(test)]
    mod trigger_message_tests {
        use super::*;

        fn evse(id: i64, connector_id: Option<i64>) -> EVSE {
            EVSE {
                connector_id,
                custom_data: None,
                id,
            }
        }

        #[test]
        fn an_absent_evse_addresses_the_whole_charge_point() {
            assert_eq!(trigger_target(None), Ok(AvailabilityTarget::ChargePoint));
        }

        #[test]
        fn wire_ids_are_one_based_and_this_crates_are_zero_based() {
            assert_eq!(
                trigger_target(Some(&evse(1, None))),
                Ok(AvailabilityTarget::Evse { evse_id: 0 })
            );
            assert_eq!(
                trigger_target(Some(&evse(2, Some(1)))),
                Ok(AvailabilityTarget::Connector {
                    evse_id: 1,
                    connector_id: 0
                })
            );
        }

        #[test]
        fn an_address_that_cannot_exist_is_rejected_rather_than_widened() {
            // `0` and negatives are not valid wire ids; treating either as "the whole charge
            // point" would answer a request the CSMS did not make.
            assert_eq!(trigger_target(Some(&evse(0, None))), Err(()));
            assert_eq!(trigger_target(Some(&evse(-1, None))), Err(()));
            assert_eq!(trigger_target(Some(&evse(1, Some(0)))), Err(()));
        }

        #[test]
        fn the_two_messages_this_crate_can_resend_map_and_the_rest_are_not_implemented() {
            assert_eq!(
                triggerable_message(
                    &MessageTriggerEnum::Heartbeat,
                    AvailabilityTarget::ChargePoint
                ),
                Some(TriggerableMessage::Heartbeat)
            );
            assert_eq!(
                triggerable_message(
                    &MessageTriggerEnum::StatusNotification,
                    AvailabilityTarget::Evse { evse_id: 0 }
                ),
                Some(TriggerableMessage::StatusNotification(
                    AvailabilityTarget::Evse { evse_id: 0 }
                ))
            );
            // Everything else needs a functional block this crate doesn't have. Reported as
            // NotImplemented rather than Rejected, which would claim the request was understood
            // and refused.
            for requested in [
                MessageTriggerEnum::BootNotification,
                MessageTriggerEnum::MeterValues,
                MessageTriggerEnum::TransactionEvent,
                MessageTriggerEnum::FirmwareStatusNotification,
                MessageTriggerEnum::LogStatusNotification,
            ] {
                assert_eq!(
                    triggerable_message(&requested, AvailabilityTarget::ChargePoint),
                    None
                );
            }
        }
    }

    use super::{
        RequestStartTransactionHandler, RequestStartTransactionOutcome,
        RequestStopTransactionHandler, RequestStopTransactionOutcome, UnlockConnectorHandler,
        UnlockOutcome, handle_request_start_transaction,
        handle_request_stop_transaction_with_replay_guard, handle_unlock_request,
    };
    // Only used by the `std`-gated `impl TriggerMessageHandler for OCPP2_0_1Client` above (see
    // its cfg for why) and by `trigger_message_tests`.
    #[cfg(feature = "std")]
    use super::{
        TriggerMessageHandler, TriggerMessageOutcome, TriggerableMessage, handle_trigger_message,
    };
    use crate::actor::ChargePointActor;
    #[cfg(feature = "std")]
    use crate::availability::AvailabilityTarget;
    use crate::replay_protection::ReplayGuard;
    use crate::state::{IdToken, IdTokenKind, TransactionId};
    #[cfg(feature = "std")]
    use crate::wire::v201::common::{EVSE, MessageTriggerEnum, TriggerMessageStatusEnum};
    use crate::wire::v201::common::{IdTokenEnum, RequestStartStopStatusEnum, UnlockStatusEnum};
    use crate::wire::v201::{
        RequestStartTransactionRequest, RequestStartTransactionResponse,
        RequestStopTransactionRequest, RequestStopTransactionResponse, UnlockConnectorRequest,
        UnlockConnectorResponse,
    };
    #[cfg(feature = "std")]
    use crate::wire::v201::{TriggerMessageRequest, TriggerMessageResponse};
    use alloc::boxed::Box;
    use alloc::string::ToString;
    use alloc::sync::Arc;
    use ocpp_client::ocpp_2_0_1::OCPP2_0_1Client;

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

    fn map_id_token(id_token: &crate::wire::v201::common::IdToken) -> IdToken {
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
            let guard = Arc::new(ReplayGuard::new());
            self.on_request_stop_transaction(move |request, _client| {
                let actor = actor.clone();
                let guard = guard.clone();
                async move {
                    let outcome = match parse_transaction_id(&request) {
                        Some(transaction_id) => {
                            handle_request_stop_transaction_with_replay_guard(
                                &actor,
                                &guard,
                                transaction_id,
                            )
                            .await
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
                id_token: crate::wire::v201::common::IdToken {
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
            assert_eq!(
                map_id_token_kind(IdTokenEnum::Central),
                IdTokenKind::Central
            );
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
        ExtendedTriggerMessageHandler, RequestStartTransactionHandler,
        RequestStartTransactionOutcome, RequestStopTransactionHandler,
        RequestStopTransactionOutcome, TriggerMessageHandler, TriggerMessageOutcome,
        TriggerableMessage, UnlockConnectorHandler, UnlockOutcome,
        handle_request_start_transaction, handle_request_stop_transaction_with_replay_guard,
        handle_trigger_message, handle_unlock_request,
    };
    use crate::actor::ChargePointActor;
    use crate::availability::{AvailabilityTarget, Ocpp1_6StatusNotifier};
    use crate::id_tag::map_id_token;
    use crate::replay_protection::ReplayGuard;
    use crate::state::TransactionId;
    use crate::topology::unflatten_ocpp_1_6_connector_id;
    use crate::wire::v16::common::{
        ExtendedTriggerMessageRequestRequestedMessage as ExtendedRequestedMessage,
        ExtendedTriggerMessageResponseStatus,
        RemoteStartTransactionResponseStatus,
        RemoteStopTransactionResponseStatus,
        // Renamed upstream in `ocpp-types` 0.2.0 to make room for
        // `ExtendedTriggerMessageRequestRequestedMessage`; a pure rename, no variants changed.
        TriggerMessageRequestRequestedMessage as RequestedMessage,
        TriggerMessageResponseStatus,
        UnlockConnectorResponseStatus,
    };
    use crate::wire::v16::{
        ExtendedTriggerMessageRequest, ExtendedTriggerMessageResponse,
        RemoteStartTransactionRequest, RemoteStartTransactionResponse,
        RemoteStopTransactionResponse, TriggerMessageRequest, TriggerMessageResponse,
        UnlockConnectorResponse,
    };
    use alloc::boxed::Box;
    use alloc::sync::Arc;
    use alloc::vec::Vec;
    use ocpp_client::ocpp_1_6::OCPP1_6Client;

    // --- TriggerMessage (B1.3) ---

    /// 1.6J's flat `connectorId` onto this crate's [`AvailabilityTarget`]: absent or `0` is the
    /// whole charge point, anything else resolves through [`unflatten_ocpp_1_6_connector_id`] to
    /// one connector. Unlike the 2.x adapters, this keeps the connector half of the address -
    /// 1.6J has no EVSE to lose it to.
    ///
    /// `Err(())` is an address this topology has no connector for, answered with `Rejected`.
    fn trigger_target(
        connector_counts: &[usize],
        connector_id: Option<i64>,
    ) -> Result<AvailabilityTarget, ()> {
        match connector_id {
            None | Some(0) => Ok(AvailabilityTarget::ChargePoint),
            Some(connector_id) => {
                let (evse_id, connector_id) =
                    unflatten_ocpp_1_6_connector_id(connector_counts, connector_id).ok_or(())?;
                Ok(AvailabilityTarget::Connector {
                    evse_id,
                    connector_id,
                })
            }
        }
    }

    /// 1.6J's `requestedMessage` onto this crate's [`TriggerableMessage`], or `None` for one no
    /// functional block here can fulfil - reported as `NotImplemented`, exactly as the 2.x
    /// adapters do. 1.6J's enum is smaller (six values, with `DiagnosticsStatusNotification` where
    /// 2.x has the log/certificate triggers), but the two this crate can answer are the same two.
    fn triggerable_message(
        requested: &RequestedMessage,
        target: AvailabilityTarget,
    ) -> Option<TriggerableMessage> {
        match requested {
            RequestedMessage::Heartbeat => Some(TriggerableMessage::Heartbeat),
            RequestedMessage::StatusNotification => {
                Some(TriggerableMessage::StatusNotification(target))
            }
            _ => None,
        }
    }

    /// 1.6J's `ExtendedTriggerMessage.requestedMessage` onto this crate's [`TriggerableMessage`],
    /// or `None` for one no functional block here can fulfil (D2.2) - reported as
    /// `NotImplemented`, exactly like [`triggerable_message`]'s handling of plain
    /// `TriggerMessage`. `ExtendedTriggerMessageRequestRequestedMessage` is a different wire enum
    /// from `TriggerMessageRequestRequestedMessage` (it adds `LogStatusNotification` and
    /// `SignChargePointCertificate`, and drops `DiagnosticsStatusNotification`), so this is its
    /// own match rather than a shared one - but the two values this crate can actually fulfil are
    /// the same two, and the Rust type system already keeps a value valid for one action from
    /// ever reaching the other's handler.
    fn extended_triggerable_message(
        requested: &ExtendedRequestedMessage,
        target: AvailabilityTarget,
    ) -> Option<TriggerableMessage> {
        match requested {
            ExtendedRequestedMessage::Heartbeat => Some(TriggerableMessage::Heartbeat),
            ExtendedRequestedMessage::StatusNotification => {
                Some(TriggerableMessage::StatusNotification(target))
            }
            _ => None,
        }
    }

    /// Wraps an [`OCPP1_6Client`] with the connector topology 1.6J's flat addressing needs.
    ///
    /// This wrapper exists for a reason the 2.x adapters don't have: `handle_trigger_message`
    /// needs *one* notifier that is both a [`crate::provisioning::HeartbeatSender`] and a
    /// [`crate::availability::StatusNotifier`], and under 1.6J those live on two different types -
    /// the bare client sends heartbeats, while status notifications need
    /// [`Ocpp1_6StatusNotifier`]'s topology to flatten the connector address. This type implements
    /// both by delegating to each, so the shared protocol-agnostic handler works unchanged.
    pub struct Ocpp1_6TriggerMessageHandler {
        client: OCPP1_6Client,
        connector_counts: Vec<usize>,
        status: Arc<Ocpp1_6StatusNotifier>,
    }

    impl Ocpp1_6TriggerMessageHandler {
        /// Wraps `client`, resolving connector addresses against `connector_counts` (each EVSE's
        /// connector count, in `evse_id` order).
        pub fn new(
            client: OCPP1_6Client,
            connector_counts: impl IntoIterator<Item = usize>,
        ) -> Self {
            let connector_counts: Vec<usize> = connector_counts.into_iter().collect();
            Self {
                status: Arc::new(Ocpp1_6StatusNotifier::new(
                    client.clone(),
                    connector_counts.clone(),
                )),
                client,
                connector_counts,
            }
        }
    }

    #[async_trait::async_trait]
    impl crate::provisioning::HeartbeatSender for Ocpp1_6TriggerMessageHandler {
        type Error = <OCPP1_6Client as crate::provisioning::HeartbeatSender>::Error;

        async fn send_heartbeat(
            &self,
        ) -> Result<Option<chrono::DateTime<chrono::Utc>>, Self::Error> {
            // Delegates to the trait impl on the bare client (which builds the request and parses
            // the response's `currentTime`), not to the client's inherent `send_heartbeat`.
            crate::provisioning::HeartbeatSender::send_heartbeat(&self.client).await
        }
    }

    #[async_trait::async_trait]
    impl crate::availability::StatusNotifier for Ocpp1_6TriggerMessageHandler {
        type Error = <Ocpp1_6StatusNotifier as crate::availability::StatusNotifier>::Error;

        async fn notify_status(
            &self,
            evse_id: usize,
            connector_id: usize,
            status: crate::state::ConnectorStatus,
            connector_state: crate::state::ConnectorState,
        ) -> Result<(), Self::Error> {
            self.status
                .notify_status(evse_id, connector_id, status, connector_state)
                .await
        }
    }

    #[async_trait::async_trait]
    impl TriggerMessageHandler for Ocpp1_6TriggerMessageHandler {
        async fn register_trigger_message_handler(&self, actor: ChargePointActor) {
            let client = self.client.clone();
            let connector_counts = self.connector_counts.clone();
            let notifier = Arc::new(Ocpp1_6TriggerMessageHandler::new(
                self.client.clone(),
                self.connector_counts.clone(),
            ));
            client
                .on_trigger_message(move |request: TriggerMessageRequest, _client| {
                    let actor = actor.clone();
                    let notifier = notifier.clone();
                    let connector_counts = connector_counts.clone();
                    async move {
                        let Ok(target) = trigger_target(&connector_counts, request.connector_id)
                        else {
                            return Ok(TriggerMessageResponse {
                                status: TriggerMessageResponseStatus::Rejected,
                            });
                        };
                        let Some(message) = triggerable_message(&request.requested_message, target)
                        else {
                            return Ok(TriggerMessageResponse {
                                status: TriggerMessageResponseStatus::NotImplemented,
                            });
                        };
                        let outcome =
                            handle_trigger_message(&actor, notifier.as_ref(), message).await;
                        Ok(TriggerMessageResponse {
                            status: match outcome {
                                TriggerMessageOutcome::Accepted => {
                                    TriggerMessageResponseStatus::Accepted
                                }
                                TriggerMessageOutcome::Rejected => {
                                    TriggerMessageResponseStatus::Rejected
                                }
                            },
                        })
                    }
                })
                .await;
        }
    }

    // --- ExtendedTriggerMessage (D2.2) ---

    #[async_trait::async_trait]
    impl ExtendedTriggerMessageHandler for Ocpp1_6TriggerMessageHandler {
        async fn register_extended_trigger_message_handler(&self, actor: ChargePointActor) {
            let client = self.client.clone();
            let connector_counts = self.connector_counts.clone();
            let notifier = Arc::new(Ocpp1_6TriggerMessageHandler::new(
                self.client.clone(),
                self.connector_counts.clone(),
            ));
            client
                .on_extended_trigger_message(
                    move |request: ExtendedTriggerMessageRequest, _client| {
                        let actor = actor.clone();
                        let notifier = notifier.clone();
                        let connector_counts = connector_counts.clone();
                        async move {
                            let Ok(target) =
                                trigger_target(&connector_counts, request.connector_id)
                            else {
                                return Ok(ExtendedTriggerMessageResponse {
                                    status: ExtendedTriggerMessageResponseStatus::Rejected,
                                });
                            };
                            let Some(message) =
                                extended_triggerable_message(&request.requested_message, target)
                            else {
                                return Ok(ExtendedTriggerMessageResponse {
                                    status: ExtendedTriggerMessageResponseStatus::NotImplemented,
                                });
                            };
                            let outcome =
                                handle_trigger_message(&actor, notifier.as_ref(), message).await;
                            Ok(ExtendedTriggerMessageResponse {
                                status: match outcome {
                                    TriggerMessageOutcome::Accepted => {
                                        ExtendedTriggerMessageResponseStatus::Accepted
                                    }
                                    TriggerMessageOutcome::Rejected => {
                                        ExtendedTriggerMessageResponseStatus::Rejected
                                    }
                                },
                            })
                        }
                    },
                )
                .await;
        }
    }

    #[cfg(test)]
    mod trigger_message_tests {
        use super::*;

        #[test]
        fn connector_zero_or_absent_addresses_the_whole_charge_point() {
            let counts = [2, 2];
            assert_eq!(
                trigger_target(&counts, None),
                Ok(AvailabilityTarget::ChargePoint)
            );
            assert_eq!(
                trigger_target(&counts, Some(0)),
                Ok(AvailabilityTarget::ChargePoint)
            );
        }

        #[test]
        fn a_flat_connector_id_resolves_to_its_evse_and_connector() {
            // Two EVSEs, two connectors each: 1.6J connector 3 is EVSE 1's connector 0.
            let counts = [2, 2];
            assert_eq!(
                trigger_target(&counts, Some(1)),
                Ok(AvailabilityTarget::Connector {
                    evse_id: 0,
                    connector_id: 0
                })
            );
            assert_eq!(
                trigger_target(&counts, Some(3)),
                Ok(AvailabilityTarget::Connector {
                    evse_id: 1,
                    connector_id: 0
                })
            );
        }

        #[test]
        fn an_address_this_topology_does_not_have_is_rejected() {
            assert_eq!(trigger_target(&[2], Some(5)), Err(()));
            assert_eq!(trigger_target(&[2], Some(-1)), Err(()));
        }

        #[test]
        fn the_two_messages_this_crate_can_resend_map_and_the_rest_are_not_implemented() {
            assert_eq!(
                triggerable_message(
                    &RequestedMessage::Heartbeat,
                    AvailabilityTarget::ChargePoint
                ),
                Some(TriggerableMessage::Heartbeat)
            );
            assert_eq!(
                triggerable_message(
                    &RequestedMessage::StatusNotification,
                    AvailabilityTarget::ChargePoint
                ),
                Some(TriggerableMessage::StatusNotification(
                    AvailabilityTarget::ChargePoint
                ))
            );
            for requested in [
                RequestedMessage::BootNotification,
                RequestedMessage::DiagnosticsStatusNotification,
                RequestedMessage::FirmwareStatusNotification,
                RequestedMessage::MeterValues,
            ] {
                assert_eq!(
                    triggerable_message(&requested, AvailabilityTarget::ChargePoint),
                    None
                );
            }
        }

        #[test]
        fn extended_trigger_message_maps_the_same_two_messages_and_rejects_the_rest() {
            assert_eq!(
                extended_triggerable_message(
                    &ExtendedRequestedMessage::Heartbeat,
                    AvailabilityTarget::ChargePoint
                ),
                Some(TriggerableMessage::Heartbeat)
            );
            assert_eq!(
                extended_triggerable_message(
                    &ExtendedRequestedMessage::StatusNotification,
                    AvailabilityTarget::ChargePoint
                ),
                Some(TriggerableMessage::StatusNotification(
                    AvailabilityTarget::ChargePoint
                ))
            );
            // Distinct from `TriggerMessage`'s unsupported set: `ExtendedTriggerMessage` adds
            // `LogStatusNotification` and `SignChargePointCertificate`, and has no
            // `DiagnosticsStatusNotification` value at all - the two wire enums are different
            // types.
            for requested in [
                ExtendedRequestedMessage::BootNotification,
                ExtendedRequestedMessage::LogStatusNotification,
                ExtendedRequestedMessage::FirmwareStatusNotification,
                ExtendedRequestedMessage::MeterValues,
                ExtendedRequestedMessage::SignChargePointCertificate,
            ] {
                assert_eq!(
                    extended_triggerable_message(&requested, AvailabilityTarget::ChargePoint),
                    None
                );
            }
        }
    }

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
            RequestStopTransactionOutcome::Accepted => {
                RemoteStopTransactionResponseStatus::Accepted
            }
            RequestStopTransactionOutcome::Rejected => {
                RemoteStopTransactionResponseStatus::Rejected
            }
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

    /// Delegates to the wrapped client. `RemoteStopTransaction` addresses a transaction id, not a
    /// connector, so 1.6J needs no topology for it - but
    /// [`ChargePointBuilder::remote_control`](crate::builder::ChargePointBuilder::remote_control)
    /// registers the three remote-control handlers together, so the wrapper implements this one
    /// too rather than making a 1.6J caller pass two different types for one functional block.
    #[async_trait::async_trait]
    impl RequestStopTransactionHandler for Ocpp1_6RemoteControlHandler {
        async fn register_request_stop_transaction_handler(&self, actor: ChargePointActor) {
            RequestStopTransactionHandler::register_request_stop_transaction_handler(
                &self.client,
                actor,
            )
            .await;
        }
    }

    #[async_trait::async_trait]
    impl RequestStopTransactionHandler for OCPP1_6Client {
        async fn register_request_stop_transaction_handler(&self, actor: ChargePointActor) {
            let guard = Arc::new(ReplayGuard::new());
            self.on_remote_stop_transaction(move |request, _client| {
                let actor = actor.clone();
                let guard = guard.clone();
                async move {
                    let outcome = match u64::try_from(request.transaction_id) {
                        Ok(transaction_id) => {
                            handle_request_stop_transaction_with_replay_guard(
                                &actor,
                                &guard,
                                TransactionId(transaction_id),
                            )
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
                id_tag: crate::wire::v16::IdTag::try_from("04A224B2").unwrap(),
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
