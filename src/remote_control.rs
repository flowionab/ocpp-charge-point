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
    /// The request was accepted, but no transaction exists yet - **F02, "Remote Start First"**
    /// (CV7). The charge point is holding the request against a connector and will start when the
    /// driver plugs in, or drop it when `TxCtrlr.EVConnectionTimeOut` expires.
    ///
    /// Maps to OCPP's `Accepted` on the wire, same as [`Self::Accepted`] - the distinction is for
    /// this crate's callers, since there is no `transactionId` to quote yet.
    AcceptedPendingCable,
    /// The request was accepted, and the identifier it carried is being authorized before energy
    /// may flow - **F01.FR.01** (CV7), the path `AuthCtrlr.AuthorizeRemoteStart` selects.
    ///
    /// The cable is already in; what is outstanding is the authorization decision, which is why
    /// this is distinct from [`Self::AcceptedPendingCable`]. Maps to OCPP's `Accepted` on the
    /// wire like both its siblings - the requirement's own note says the charge point responds
    /// first and authorizes afterwards - and carries no `transactionId`, because none exists yet.
    AcceptedPendingAuthorization,
    /// No connector could take the request - see [`find_start_target`] for the conditions
    /// F01.FR.21-.24 name.
    Rejected,
}

/// Handles a CSMS-initiated `RequestStartTransaction` request against `actor` (F01/F02, CV7).
///
/// Finds a connector that can take the request (see [`find_start_target`]) and either starts the
/// transaction immediately - if the cable is already latched, F01 - or **holds the request until
/// the driver plugs in**, which is F02 and did not work at all before CV7. Starts a transaction
/// without a separate Authorize round-trip (the CSMS's own request is itself the authorization
/// decision; see `ConnectorEvent::RemoteStartRequested`) - unless
/// `AuthCtrlr.AuthorizeRemoteStart` says to authorize it first, which is **F01.FR.01** and
/// answers [`RequestStartTransactionOutcome::AcceptedPendingAuthorization`].
///
/// `id_token` is the identifier the CSMS supplied, recorded on the started `Transaction`.
/// `group_id_token` is the request's `groupIdToken`, used only to decide whether a reservation
/// held for a group admits this driver (**F01.FR.22**, see [`find_start_target`]); it is never
/// recorded on the transaction. 1.6J's `RemoteStartTransaction` has no such field, so its adapter
/// passes `None`.
///
/// Rejects if `evse_id` is out of range, or no connector passes F01.FR.21-.24.
#[tracing::instrument(skip_all, fields(evse_id))]
pub async fn handle_request_start_transaction(
    actor: &ChargePointActor,
    evse_id: Option<usize>,
    id_token: IdToken,
    group_id_token: Option<IdToken>,
    remote_start_id: Option<i64>,
) -> RequestStartTransactionOutcome {
    // B02.FR.05: while the CSMS has answered `Pending` (or has not answered at all), a remote
    // start is refused outright - even on a charge point configured to accept local transactions
    // in that state. A CSMS that wants to drive transactions must accept the station first.
    if !actor.state().may_send_requests() {
        tracing::warn!(
            "refusing RequestStartTransaction: the CSMS has not accepted this charge point yet"
        );
        return RequestStartTransactionOutcome::Rejected;
    }
    // F01.FR.21-.24 / F02.FR.23-.26 (CV7): the rejection conditions OCPP names, checked
    // explicitly so each produces a `Rejected` for the reason the spec gives rather than as a
    // side effect of no connector happening to be latched.
    let state = actor.state();
    if let Some(evse_id) = evse_id
        && state.evses.get(evse_id).is_none()
    {
        tracing::warn!(evse_id, "refusing RequestStartTransaction: no such EVSE");
        return RequestStartTransactionOutcome::Rejected;
    }
    let Some((evse_id, connector_id, latched)) =
        find_start_target(&state, evse_id, &id_token, group_id_token.as_ref())
    else {
        tracing::warn!("refusing RequestStartTransaction: no connector can take it");
        return RequestStartTransactionOutcome::Rejected;
    };

    if !latched {
        // **F02 - "Remote Start Transaction - Remote Start First"**. The cable is not in yet, and
        // that is the whole premise of the use case: accept, hold the request against the
        // connector, and start when the driver plugs in. Before CV7 this was rejected outright,
        // so F02 did not work at all.
        //
        // No transaction exists yet, so there is no `transactionId` to return - which is correct
        // under this crate's default `TxStartPoint` of `Authorized`. A station configured for
        // `EVConnected` (CV2.2) creates one at the latch, and F01.FR.13's "return the
        // transactionId" case belongs to a start point earlier than any this crate observes.
        let _ = actor
            .send(ChargePointEvent::Evse {
                evse_id,
                event: EvseEvent::Connector {
                    connector_id,
                    event: ConnectorEvent::RemoteStartPending(crate::state::PendingRemoteStart {
                        id_token,
                        remote_start_id,
                    }),
                },
            })
            .await;
        tracing::info!(
            evse_id,
            connector_id,
            "accepted a remote start; waiting for the driver to plug in"
        );
        return RequestStartTransactionOutcome::AcceptedPendingCable;
    }

    // F01: the cable is already latched, so the transaction starts now.
    let _ = actor
        .send(ChargePointEvent::Evse {
            evse_id,
            event: EvseEvent::Connector {
                connector_id,
                event: ConnectorEvent::RemoteStartRequested {
                    id_token,
                    remote_start_id,
                },
            },
        })
        .await;

    let state = actor.state();
    match &state.evses[evse_id].transactions[connector_id] {
        Some(transaction) => RequestStartTransactionOutcome::Accepted {
            transaction_id: transaction.id,
        },
        // F01.FR.01 (CV7): the operator asked for this to be authorized first, so the connector
        // is waiting on the CSMS rather than on the contactor.
        None if state.evses[evse_id].connectors[connector_id] == ConnectorState::Authorizing => {
            tracing::info!(
                evse_id,
                connector_id,
                "accepted a remote start; authorizing the identifier before allowing energy \
                 transfer"
            );
            RequestStartTransactionOutcome::AcceptedPendingAuthorization
        }
        // The connector moved but no transaction exists yet - a `TxStartPoint` later than
        // `Authorized` (CV2.2). The request was still accepted; there is simply no id to quote.
        None => RequestStartTransactionOutcome::AcceptedPendingCable,
    }
}

/// Releases held authorizations whose driver never plugged in - **F02.FR.07/.08** and
/// **E03.FR.15**, `TxCtrlr.EVConnectionTimeOut` (`docs/OCPP-2.1-COMPLIANCE-ROADMAP.md` CV2.3).
///
/// Covers both halves of the same problem, because both land in the same slot (see
/// [`crate::state::EvseState::pending_remote_starts`]): a `RequestStartTransaction` accepted before
/// the cable arrived (F02), and a card presented at the reader and accepted before the cable
/// arrived (E03). Without this, an authorization whose driver never arrives is held until the
/// connector happens to be used, which would let a stale one fire for whoever plugs in next -
/// E03.FR.15's "SHALL deauthorize" in as many words.
///
/// # Why the timing lives here rather than on the held request
///
/// The obvious design is a timestamp on [`crate::state::PendingRemoteStart`]. That would need a
/// clock inside `handle_request_start_transaction`, which is reached from every protocol version's
/// inbound adapter - so it would push a `MonotonicClock` through three adapter constructions and
/// their traits, to serve one field. Keeping the deadlines in this loop instead costs one small map
/// and changes no signature: the loop is the only thing that needs to know when a request was
/// first seen, because it is the only thing that acts on the answer.
///
/// The cost is accuracy: a request is released between `EVConnectionTimeOut` and that plus one
/// sweep interval after it was accepted. `interval_secs` therefore wants to be a fraction of the
/// timeout, not equal to it.
///
/// Runs forever. `EVConnectionTimeOut` is re-read every sweep, so a CSMS changing it takes effect
/// without a reboot; `0` means "no timeout" and holds requests indefinitely, matching how this
/// crate reads every other `0`-valued interval.
pub async fn run_pending_remote_start_timeouts<B, M>(
    actor: &ChargePointActor,
    backoff: &B,
    monotonic: &M,
    interval_secs: u32,
) where
    B: crate::provisioning::Backoff,
    M: crate::clock::MonotonicClock,
{
    let mut first_seen: alloc::collections::BTreeMap<
        (usize, usize),
        crate::clock::MonotonicInstant,
    > = alloc::collections::BTreeMap::new();
    loop {
        backoff.wait(interval_secs.max(1)).await;
        let now = monotonic.now();
        let timeout = ev_connection_timeout_secs(actor);

        let held = held_starts_awaiting_a_cable(&actor.state());

        // Forget connectors that are no longer holding anything, so a later request on the same
        // connector is timed from when *it* arrived rather than from a previous one.
        first_seen.retain(|address, _| held.contains(address));
        if timeout == 0 {
            continue;
        }

        for address in held {
            let since = *first_seen.entry(address).or_insert(now);
            if now.duration_since(since).as_secs() < u64::from(timeout) {
                continue;
            }
            let (evse_id, connector_id) = address;
            tracing::info!(
                evse_id,
                connector_id,
                timeout,
                "deauthorizing a held start the driver never plugged in for"
            );
            let _ = actor
                .send(ChargePointEvent::Evse {
                    evse_id,
                    event: EvseEvent::Connector {
                        connector_id,
                        event: ConnectorEvent::RemoteStartPendingCleared,
                    },
                })
                .await;
            first_seen.remove(&address);
        }
    }
}

/// Every `(evse_id, connector_id)` holding an authorization that is still **waiting for a cable**,
/// and so is subject to `TxCtrlr.EVConnectionTimeOut`.
///
/// The name of the variable is the whole rule: it times how long to wait for the *EV to connect*.
/// A connector whose cable is already in is waiting for something else, and releasing its hold
/// would deauthorize a session for a reason that demonstrably did not happen.
///
/// Today that excludes exactly one state, [`ConnectorState::Authorizing`] - a remote start the
/// operator asked to have authorized first (F01.FR.01, CV7), held across the round trip to the
/// CSMS. Every other way a hold exists (F02's pre-cable request, E03's pre-cable card) sits on a
/// connector that has no cable by construction.
fn held_starts_awaiting_a_cable(
    state: &crate::state::ChargePointState,
) -> alloc::vec::Vec<(usize, usize)> {
    state
        .evses
        .iter()
        .enumerate()
        .flat_map(|(evse_id, evse)| {
            evse.pending_remote_starts
                .iter()
                .enumerate()
                .filter(|(_, pending)| pending.is_some())
                .filter(move |(connector_id, _)| {
                    evse.connectors.get(*connector_id)
                        != Some(&crate::state::ConnectorState::Authorizing)
                })
                .map(move |(connector_id, _)| (evse_id, connector_id))
        })
        .collect()
}

/// `TxCtrlr.EVConnectionTimeOut` in seconds, or `0` (meaning "no timeout") when it is absent or
/// unparseable.
pub fn ev_connection_timeout_secs(actor: &ChargePointActor) -> u32 {
    actor
        .state()
        .device_model
        .get(
            &crate::state::Component {
                name: "TxCtrlr".into(),
                instance: None,
                evse: None,
            },
            &crate::state::Variable {
                name: "EVConnectionTimeOut".into(),
                instance: None,
            },
        )
        .and_then(|definition| definition.attribute(crate::state::VariableAttributeType::Actual))
        .and_then(|attribute| attribute.value.parse::<u32>().ok())
        .unwrap_or(0)
}

/// The connector a `RequestStartTransaction` should act on, and whether its cable is already
/// latched (CV7).
///
/// `true` means start now (F01); `false` means hold the request until the driver plugs in (F02).
/// `None` means every candidate is excluded by one of F01.FR.21-.24's conditions:
///
/// - **Reserved for someone else** (FR.21/.22) - a reservation is a promise to one driver, and a
///   CSMS start for a different identifier must not break it.
/// - **Unavailable or faulted** (FR.23) - nothing can be started on it.
/// - **Occupied by an authorized transaction** (FR.24) - only a connector with no transaction, or
///   one not yet authorized, can be matched to a new request.
///
/// A latched connector is preferred over an idle one so the common case (driver already plugged
/// in, CSMS starts remotely) still starts immediately rather than waiting for a cable that is
/// already there. Among connectors with no cable, one this driver *reserved* is preferred over a
/// merely free one, so the reservation is consumed rather than stranded.
fn find_start_target(
    state: &ChargePointState,
    evse_id: Option<usize>,
    id_token: &IdToken,
    group_id_token: Option<&IdToken>,
) -> Option<(usize, usize, bool)> {
    let evses: alloc::vec::Vec<usize> = match evse_id {
        Some(evse_id) => alloc::vec![evse_id],
        None => (0..state.evses.len()).collect(),
    };
    let mut waiting = None;
    let mut reserved_waiting = None;
    for evse_id in evses {
        let Some(evse) = state.evses.get(evse_id) else {
            continue;
        };
        for connector_id in 0..evse.connectors.len() {
            if !can_start_here(state, evse_id, connector_id, id_token, group_id_token) {
                continue;
            }
            match evse.connectors[connector_id] {
                ConnectorState::Locked => return Some((evse_id, connector_id, true)),
                // Reached only when the reservation admits this driver - `can_start_here` has
                // already refused every other one.
                ConnectorState::Reserved if reserved_waiting.is_none() => {
                    reserved_waiting = Some((evse_id, connector_id, false));
                }
                ConnectorState::Available if waiting.is_none() => {
                    waiting = Some((evse_id, connector_id, false));
                }
                _ => {}
            }
        }
    }
    // A bay this driver reserved beats a merely free one: sending them elsewhere would leave the
    // reservation to expire unused beside the connector they were told to use.
    reserved_waiting.or(waiting)
}

/// Whether one connector passes F01.FR.21-.24 - see [`find_start_target`].
fn can_start_here(
    state: &ChargePointState,
    evse_id: usize,
    connector_id: usize,
    id_token: &IdToken,
    group_id_token: Option<&IdToken>,
) -> bool {
    let evse = &state.evses[evse_id];
    if evse.status != crate::state::EvseStatus::Available {
        return false;
    }
    match evse.connectors[connector_id] {
        // `Reserved` is here because FR.21/.22 are rules about *whose* reservation it is, and they
        // say nothing unless a matching identifier can get as far as the comparison below.
        // Excluding the state outright would refuse a remote start from the very driver the bay is
        // being held for - which is the one thing a reservation exists to enable.
        ConnectorState::Available | ConnectorState::Locked | ConnectorState::Reserved => {}
        // FR.23 and everything else mid-session: not a connector a new request can be matched to.
        _ => return false,
    }
    // FR.24: an EVSE whose transaction has already been authorized is taken.
    if evse.transactions[connector_id].is_some() {
        return false;
    }
    // FR.21/.22: a reservation held for someone else. The rule refuses only when *neither* the
    // identifier nor the group matches - a fleet books a bay under one token and whichever
    // vehicle turns up starts on another, so requiring the identifier alone would make every
    // group reservation unusable by the group it was made for.
    if let Some(reservation) = evse.reservations[connector_id].as_ref()
        && !reservation_admits(reservation, id_token, group_id_token)
    {
        return false;
    }
    true
}

/// Whether `reservation` may be honoured by this identifier - **F01.FR.22**/F02.FR.24.
///
/// The identifier matches, or the group does. An absent group on either side is not a wildcard:
/// two reservations that name no group are not thereby in the same group, and a request naming a
/// group does not thereby open a reservation that named none. Only a group present on *both*
/// sides and equal counts, which is why this is not a plain `==` on two `Option`s.
fn reservation_admits(
    reservation: &crate::state::Reservation,
    id_token: &IdToken,
    group_id_token: Option<&IdToken>,
) -> bool {
    if reservation.id_token.value == id_token.value {
        return true;
    }
    matches!(
        (reservation.group_id_token.as_ref(), group_id_token),
        (Some(reserved_for), Some(presented)) if reserved_for.value == presented.value
    )
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
    // B02.FR.05, the other half - see `handle_request_start_transaction`.
    if !state.may_send_requests() {
        tracing::warn!(
            "refusing RequestStopTransaction: the CSMS has not accepted this charge point yet"
        );
        return RequestStopTransactionOutcome::Rejected;
    }
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
    /// An actor the CSMS has already accepted.
    ///
    /// Every handler in this module refuses outright until the charge point is accepted
    /// (B02.FR.05, CV4), so "accepted" is the state a remote-control test is actually about -
    /// spawning a bare actor would test the boot gate over and over instead. The one test that
    /// *is* about the gate builds its own actor.
    async fn accepted_actor<const N: usize>(connector_counts: [usize; N]) -> ChargePointActor {
        let actor = ChargePointActor::spawn(connector_counts, &TokioExecutor);
        actor
            .send(ChargePointEvent::RegistrationStatusReceived(
                crate::state::RegistrationStatus::Accepted,
            ))
            .await
            .expect("the actor accepts events");
        actor
    }

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
    async fn actor_with_unlock_confirmed() -> ChargePointActor {
        let actor = accepted_actor([1]).await;
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
        let actor = actor_with_unlock_confirmed().await;
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
        let actor = accepted_actor([1]).await;

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
        let actor = accepted_actor([1]).await;

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
        let actor = accepted_actor([1]).await;
        lock_connector(&actor, 0, 0).await;

        let outcome =
            handle_request_start_transaction(&actor, Some(0), test_id_token(), None, None).await;

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
        let actor = accepted_actor([1, 1]).await;
        lock_connector(&actor, 1, 0).await;

        let outcome =
            handle_request_start_transaction(&actor, None, test_id_token(), None, None).await;

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
        let actor = accepted_actor([1]).await;
        lock_connector(&actor, 0, 0).await;

        let outcome =
            handle_request_start_transaction(&actor, Some(5), test_id_token(), None, None).await;

        assert_eq!(outcome, RequestStartTransactionOutcome::Rejected);
    }

    /// CV6: F01.FR.25/F02.FR.01 - the `remoteStartId` the CSMS supplied is recorded on the
    /// transaction the request starts, so every one of that transaction's events can quote it
    /// back. Without it the CSMS cannot tell which of its own requests produced the transaction
    /// it is now being told about; the transaction id is no help, since the charge point chose it.
    #[tokio::test]
    async fn a_remote_start_records_its_remote_start_id_on_the_transaction_it_begins() {
        let actor = accepted_actor([1]).await;
        lock_connector(&actor, 0, 0).await;

        let outcome =
            handle_request_start_transaction(&actor, Some(0), test_id_token(), None, Some(4242))
                .await;

        assert!(matches!(
            outcome,
            RequestStartTransactionOutcome::Accepted { .. }
        ));
        assert_eq!(
            actor.state().evses[0].transactions[0]
                .as_ref()
                .and_then(|transaction| transaction.remote_start_id),
            Some(4242)
        );
    }

    /// A locally started transaction carries none - there is nothing to correlate it with, and
    /// inventing an id the CSMS never issued would be worse than reporting none.
    #[tokio::test]
    async fn a_locally_started_transaction_has_no_remote_start_id() {
        let actor = accepted_actor([1]).await;
        lock_connector(&actor, 0, 0).await;
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
            actor.state().evses[0].transactions[0]
                .as_ref()
                .and_then(|transaction| transaction.remote_start_id),
            None
        );
    }

    /// B02.FR.05 (CV4): while the CSMS has answered `Pending` - or has not answered at all - both
    /// remote-control requests are refused, *even on a connector that would otherwise be ready*.
    /// A CSMS that wants to drive transactions has to accept the charge point first.
    #[tokio::test]
    async fn remote_start_and_stop_are_rejected_until_the_csms_accepts_the_charge_point() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        lock_connector(&actor, 0, 0).await;

        // Nothing answered yet.
        assert_eq!(
            handle_request_start_transaction(&actor, Some(0), test_id_token(), None, None).await,
            RequestStartTransactionOutcome::Rejected
        );
        assert_eq!(
            handle_request_stop_transaction(&actor, TransactionId(0)).await,
            RequestStopTransactionOutcome::Rejected
        );

        // Pending is not permission.
        actor
            .send(ChargePointEvent::RegistrationStatusReceived(
                crate::state::RegistrationStatus::Pending,
            ))
            .await
            .unwrap();
        assert_eq!(
            handle_request_start_transaction(&actor, Some(0), test_id_token(), None, None).await,
            RequestStartTransactionOutcome::Rejected
        );
        assert_eq!(
            actor.state().evses[0].connectors[0],
            ConnectorState::Locked,
            "a refused remote start must not have moved the connector"
        );

        // Accepted: the same request now works, so the refusals above were the gate and not
        // some other precondition.
        actor
            .send(ChargePointEvent::RegistrationStatusReceived(
                crate::state::RegistrationStatus::Accepted,
            ))
            .await
            .unwrap();
        assert_eq!(
            handle_request_start_transaction(&actor, Some(0), test_id_token(), None, None).await,
            RequestStartTransactionOutcome::Accepted {
                transaction_id: TransactionId(0)
            }
        );
    }

    #[tokio::test]
    /// **F02, the use case CV7 exists for.** No cable yet is not a refusal - it is the premise:
    /// the station accepts, holds the request, and starts when the driver plugs in. This used to
    /// assert `Rejected`, which is precisely the bug.
    async fn a_remote_start_with_no_cable_yet_is_accepted_and_started_when_the_driver_plugs_in() {
        let actor = accepted_actor([1]).await;

        assert_eq!(
            handle_request_start_transaction(&actor, Some(0), test_id_token(), None, Some(77))
                .await,
            RequestStartTransactionOutcome::AcceptedPendingCable
        );
        assert!(
            actor.state().evses[0].transactions[0].is_none(),
            "nothing starts until the cable is in"
        );

        // The driver plugs in. The held request fires, and the transaction carries the
        // `remoteStartId` from the original request (CV6).
        lock_connector(&actor, 0, 0).await;

        let transaction = actor.state().evses[0].transactions[0]
            .clone()
            .expect("the cable arriving starts the held request");
        assert_eq!(transaction.remote_start_id, Some(77));
        assert_eq!(
            actor.state().evses[0].pending_remote_starts[0],
            None,
            "the held request is consumed, not left to fire again"
        );
    }

    /// `EVConnectionTimeOut` times how long to wait for the *EV to connect*. A remote start the
    /// operator asked to have authorized (F01.FR.01, CV7) is held on a connector whose cable is
    /// already latched, waiting on the CSMS rather than on a driver - so the sweep must leave it
    /// alone. Releasing it would deauthorize a session for the one reason that demonstrably did
    /// not happen, and would drop the `remoteStartId` with it.
    #[tokio::test]
    async fn a_remote_start_awaiting_authorization_is_not_swept_as_a_driver_who_never_arrived() {
        use crate::state::{Component, DeviceModelEvent, Variable, VariableAttributeType};

        let actor = accepted_actor([2]).await;
        actor
            .send(ChargePointEvent::DeviceModel(
                DeviceModelEvent::AttributeValueSet {
                    component: Component {
                        name: "AuthCtrlr".into(),
                        instance: None,
                        evse: None,
                    },
                    variable: Variable {
                        name: "AuthorizeRemoteStart".into(),
                        instance: None,
                    },
                    attribute_type: VariableAttributeType::Actual,
                    value: "true".into(),
                },
            ))
            .await
            .unwrap();

        // Connector 0 has its cable in, so the remote start lands there and waits for the CSMS.
        lock_connector(&actor, 0, 0).await;
        handle_request_start_transaction(&actor, Some(0), test_id_token(), None, Some(1)).await;
        assert_eq!(
            actor.state().evses[0].connectors[0],
            crate::state::ConnectorState::Authorizing
        );
        assert!(actor.state().evses[0].pending_remote_starts[0].is_some());

        // Connector 1 has no cable, so its request is held for a driver who may never arrive.
        handle_request_start_transaction(&actor, Some(0), test_id_token(), None, Some(2)).await;
        assert!(actor.state().evses[0].pending_remote_starts[1].is_some());

        assert_eq!(
            super::held_starts_awaiting_a_cable(&actor.state()),
            alloc::vec![(0, 1)],
            "only the connector still waiting for a cable is the sweep's business"
        );
    }

    /// F02.FR.07/.08 (CV2.3): a held remote start whose driver never plugs in is released, so it
    /// cannot fire for whoever uses the connector next.
    #[tokio::test]
    async fn a_held_remote_start_is_released_once_ev_connection_timeout_passes() {
        use crate::state::{Component, DeviceModelEvent, Variable, VariableAttributeType};

        let actor = accepted_actor([1]).await;
        actor
            .send(ChargePointEvent::DeviceModel(
                DeviceModelEvent::AttributeValueSet {
                    component: Component {
                        name: "TxCtrlr".into(),
                        instance: None,
                        evse: None,
                    },
                    variable: Variable {
                        name: "EVConnectionTimeOut".into(),
                        instance: None,
                    },
                    attribute_type: VariableAttributeType::Actual,
                    value: "30".into(),
                },
            ))
            .await
            .unwrap();
        assert_eq!(super::ev_connection_timeout_secs(&actor), 30);

        let outcome =
            handle_request_start_transaction(&actor, Some(0), test_id_token(), None, Some(1)).await;
        assert_eq!(
            outcome,
            RequestStartTransactionOutcome::AcceptedPendingCable
        );
        assert!(
            actor.state().evses[0].pending_remote_starts[0].is_some(),
            "holding the request has to be a published state change, or nothing downstream - the \
             timeout sweep included - can see it"
        );

        // The sweep's own release path, driven directly - the loop is an infinite timer, so what
        // is worth pinning is that clearing it works and that a later plug-in starts nothing.
        actor
            .send(ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event: ConnectorEvent::RemoteStartPendingCleared,
                },
            })
            .await
            .unwrap();

        assert!(actor.state().evses[0].pending_remote_starts[0].is_none());

        lock_connector(&actor, 0, 0).await;
        assert!(
            actor.state().evses[0].transactions[0].is_none(),
            "a released request must not fire for the next driver"
        );
    }

    /// The registered default is OCPP's 120s, not "off" - so a station nobody configured still
    /// releases a request whose driver never arrives. `0` remains this crate's "no timeout",
    /// consistent with how it reads every other interval.
    #[tokio::test]
    async fn the_default_ev_connection_timeout_is_the_one_ocpp_registers() {
        let actor = accepted_actor([1]).await;

        // 120s, the value `DEFAULT_VARIABLES` registers - OCPP's own default rather than "off".
        assert_eq!(super::ev_connection_timeout_secs(&actor), 120);
    }

    /// F01.FR.23: an EVSE that is out of service takes nothing, cable or no cable.
    #[tokio::test]
    async fn a_remote_start_on_an_unavailable_evse_is_rejected() {
        let actor = accepted_actor([1]).await;
        actor
            .send(ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::SetUnavailable,
            })
            .await
            .unwrap();

        assert_eq!(
            handle_request_start_transaction(&actor, Some(0), test_id_token(), None, None).await,
            RequestStartTransactionOutcome::Rejected
        );
    }

    /// F01.FR.21: a reservation is a promise to one driver. A CSMS start for a *different*
    /// identifier must not break it - while the same identifier is exactly who the connector was
    /// being held for.
    #[tokio::test]
    async fn a_remote_start_is_rejected_on_a_connector_reserved_for_someone_else() {
        let actor = accepted_actor([1]).await;
        actor
            .send(ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event: ConnectorEvent::Reserved(crate::state::Reservation {
                        id: crate::state::ReservationId(1),
                        id_token: crate::state::IdToken {
                            value: "SOMEONE-ELSE".into(),
                            kind: crate::state::IdTokenKind::ISO14443,
                        },
                        group_id_token: None,
                        expires_at: None,
                    }),
                },
            })
            .await
            .unwrap();

        assert_eq!(
            handle_request_start_transaction(&actor, Some(0), test_id_token(), None, None).await,
            RequestStartTransactionOutcome::Rejected
        );
    }

    /// **F01.FR.21** is a rule about *whose* reservation it is, and it only says anything if a
    /// matching identifier gets through. Before CV7's group work this check was unreachable: a
    /// `Reserved` connector was excluded by state before the identity comparison ran, so a
    /// reservation blocked remote starts from the very driver it was made for - the one thing a
    /// reservation exists to enable.
    #[tokio::test]
    async fn a_remote_start_is_accepted_on_a_connector_reserved_for_that_same_identifier() {
        let actor = accepted_actor([1]).await;
        reserve(&actor, &test_id_token().value, None).await;

        assert_eq!(
            handle_request_start_transaction(&actor, Some(0), test_id_token(), None, Some(3)).await,
            RequestStartTransactionOutcome::AcceptedPendingCable
        );

        lock_connector(&actor, 0, 0).await;
        assert!(actor.state().evses[0].transactions[0].is_some());
    }

    /// A reservation for this driver is preferred over a free bay, so the reservation is actually
    /// consumed rather than left to expire beside the connector they were sent to instead.
    #[tokio::test]
    async fn a_reserved_connector_is_preferred_over_a_merely_free_one() {
        let actor = accepted_actor([2]).await;
        // Connector 0 stays free; connector 1 is held for this driver.
        actor
            .send(ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 1,
                    event: ConnectorEvent::Reserved(crate::state::Reservation {
                        id: crate::state::ReservationId(1),
                        id_token: test_id_token(),
                        group_id_token: None,
                        expires_at: None,
                    }),
                },
            })
            .await
            .unwrap();

        handle_request_start_transaction(&actor, Some(0), test_id_token(), None, Some(3)).await;

        assert!(
            actor.state().evses[0].pending_remote_starts[1].is_some(),
            "the held start belongs on the connector this driver reserved"
        );
        assert!(actor.state().evses[0].pending_remote_starts[0].is_none());
    }

    /// **F01.FR.22** (CV7): the reservation is held for a *group*, and the CSMS starts for a
    /// different driver in that group. The rule rejects only when **neither** the idToken nor the
    /// groupIdToken matches, so this must be accepted - a fleet reservation the fleet cannot use
    /// is a reservation for nobody.
    #[tokio::test]
    async fn a_remote_start_is_accepted_for_another_member_of_the_reserved_group() {
        let actor = accepted_actor([1]).await;
        reserve(&actor, "SOMEONE-ELSE", Some("FLEET-7")).await;

        // No cable yet - a reserved bay is by definition one nobody has plugged into - so this is
        // F02's held form, and the reservation is honoured when the driver arrives.
        assert_eq!(
            handle_request_start_transaction(
                &actor,
                Some(0),
                test_id_token(),
                Some(group_token("FLEET-7")),
                Some(3),
            )
            .await,
            RequestStartTransactionOutcome::AcceptedPendingCable
        );

        lock_connector(&actor, 0, 0).await;
        let transaction = actor.state().evses[0].transactions[0]
            .clone()
            .expect("the cable arriving dispatches the held start");
        assert_eq!(transaction.remote_start_id, Some(3));
        assert_eq!(
            transaction.reservation_id,
            Some(1),
            "the transaction consumes the reservation it was admitted by"
        );
    }

    /// The group has to actually match. A reservation held for one fleet is not a reservation the
    /// next fleet may walk into.
    #[tokio::test]
    async fn a_remote_start_for_a_different_group_is_still_rejected() {
        let actor = accepted_actor([1]).await;
        reserve(&actor, "SOMEONE-ELSE", Some("FLEET-7")).await;

        assert_eq!(
            handle_request_start_transaction(
                &actor,
                Some(0),
                test_id_token(),
                Some(group_token("FLEET-9")),
                None,
            )
            .await,
            RequestStartTransactionOutcome::Rejected
        );
    }

    /// A reservation with no group, and a request carrying one, must not match on "both absent"
    /// or on some accidental equality - the idToken is the only thing left to compare.
    #[tokio::test]
    async fn a_group_on_the_request_does_not_open_a_reservation_that_has_none() {
        let actor = accepted_actor([1]).await;
        reserve(&actor, "SOMEONE-ELSE", None).await;

        assert_eq!(
            handle_request_start_transaction(
                &actor,
                Some(0),
                test_id_token(),
                Some(group_token("FLEET-7")),
                None,
            )
            .await,
            RequestStartTransactionOutcome::Rejected
        );
    }

    /// And the reverse: a *grouped* reservation is not opened by a request that names no group.
    #[tokio::test]
    async fn a_request_with_no_group_does_not_open_a_grouped_reservation() {
        let actor = accepted_actor([1]).await;
        reserve(&actor, "SOMEONE-ELSE", Some("FLEET-7")).await;

        assert_eq!(
            handle_request_start_transaction(&actor, Some(0), test_id_token(), None, None).await,
            RequestStartTransactionOutcome::Rejected
        );
    }

    fn group_token(value: &str) -> crate::state::IdToken {
        crate::state::IdToken {
            value: value.into(),
            kind: crate::state::IdTokenKind::Central,
        }
    }

    /// Reserves connector 0 for `id_token`, optionally on behalf of a group.
    async fn reserve(actor: &ChargePointActor, id_token: &str, group: Option<&str>) {
        actor
            .send(ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event: ConnectorEvent::Reserved(crate::state::Reservation {
                        id: crate::state::ReservationId(1),
                        id_token: crate::state::IdToken {
                            value: id_token.into(),
                            kind: crate::state::IdTokenKind::ISO14443,
                        },
                        group_id_token: group.map(group_token),
                        expires_at: None,
                    }),
                },
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn an_unknown_evse_is_still_rejected() {
        let actor = accepted_actor([1]).await;

        assert_eq!(
            handle_request_start_transaction(&actor, Some(9), test_id_token(), None, None).await,
            RequestStartTransactionOutcome::Rejected
        );
    }

    /// Spawns an actor with connector 0 `Charging` on a fresh transaction.
    async fn charging_actor() -> ChargePointActor {
        let actor = accepted_actor([1]).await;
        lock_connector(&actor, 0, 0).await;
        handle_request_start_transaction(&actor, Some(0), test_id_token(), None, None).await;
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
        let actor = accepted_actor([1]).await;
        lock_connector(&actor, 0, 0).await;
        handle_request_start_transaction(&actor, Some(0), test_id_token(), None, None).await;
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
        let actor = accepted_actor([1]).await;
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
        let actor = accepted_actor([1, 1]).await;
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
        let actor = accepted_actor([1]).await;
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
            // F02 (no cable yet) and F01.FR.01 (authorizing first): accepted, with no
            // transaction to name yet. OCPP has one `Accepted` for all three.
            RequestStartTransactionOutcome::AcceptedPendingCable
            | RequestStartTransactionOutcome::AcceptedPendingAuthorization => {
                (RequestStartStopStatusEnum::Accepted, None)
            }
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
                                // F01.FR.22: consulted only against a reservation held for a
                                // group, never recorded on the transaction.
                                request.group_id_token.as_ref().map(map_id_token),
                                Some(request.remote_start_id),
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
            // F02 (no cable yet) and F01.FR.01 (authorizing first): accepted, with no
            // transaction to name yet. OCPP has one `Accepted` for all three.
            RequestStartTransactionOutcome::AcceptedPendingCable
            | RequestStartTransactionOutcome::AcceptedPendingAuthorization => {
                (RequestStartStopStatusEnum::Accepted, None)
            }
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
                                // F01.FR.22: consulted only against a reservation held for a
                                // group, never recorded on the transaction.
                                request.group_id_token.as_ref().map(map_id_token),
                                Some(request.remote_start_id),
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
            // 1.6J's `RemoteStartTransaction` carries no transaction id either way, so the two
            // accepted shapes project onto the same status - the distinction only exists in this
            // crate's own outcome type.
            RequestStartTransactionOutcome::Accepted { .. }
            | RequestStartTransactionOutcome::AcceptedPendingCable
            | RequestStartTransactionOutcome::AcceptedPendingAuthorization => {
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
                                    // 1.6J's `RemoteStartTransaction` carries an `idTag` alone -
                                    // no `parentIdTag`, so F01.FR.22's group half cannot be
                                    // matched on this version and an idToken mismatch refuses.
                                    None,
                                    // 1.6J's `RemoteStartTransaction` has no `remoteStartId` -
                                    // the field arrived with 2.0. Nothing to correlate, and
                                    // nothing downstream reports one on a 1.6J connection.
                                    None,
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
