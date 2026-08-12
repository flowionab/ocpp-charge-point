//! Transactions functional block: reporting charging-session lifecycle to the CSMS via
//! TransactionEvent. See `docs/ROADMAP.md` §5.

use crate::state::{TransactionEventKind, TransactionEventOccurred};
use crate::sync::BroadcastReceiver;
use alloc::boxed::Box;

#[cfg(feature = "ocpp_1_6")]
pub use self::ocpp_1_6::Ocpp1_6TransactionNotifier;
#[cfg(feature = "ocpp_2_0_1")]
pub use self::ocpp_2_0_1::Ocpp2_0_1TransactionNotifier;
#[cfg(feature = "ocpp_2_1")]
pub use self::ocpp_2_1::Ocpp2_1TransactionNotifier;

/// What the CSMS said in reply to a `TransactionEvent`, insofar as this crate has to act on it.
///
/// A `TransactionEventResponse` is not just an acknowledgement: OCPP lets the CSMS revoke an
/// identifier mid-session by answering with a rejected `idTokenInfo` (E05), which is a decision the
/// charge point must carry out rather than log. Returning it from
/// [`TransactionNotifier::notify_transaction_event`] is what lets the *version adapter* read the
/// wire field while the *state machine* decides what it means - the same split every other
/// protocol-facing type in this crate uses.
///
/// Non-exhaustive because a response carries more the crate may come to honour (`totalCost`,
/// `chargingPriority`, `updatedPersonalMessage`); adding one should not break an integrator's own
/// notifier.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[non_exhaustive]
pub struct TransactionEventOutcome {
    /// The response's `idTokenInfo.status`, or `None` when it carried no `idTokenInfo` at all -
    /// which is the ordinary case for an event that quoted no identifier, and is *not* the same as
    /// a rejection.
    pub id_token_status: Option<crate::state::AuthorizationStatus>,
    /// The response's `transactionLimit` - a ceiling the CSMS is placing on this transaction
    /// (**E16.FR.02**, CV15), most often a prepaid balance (C17).
    ///
    /// `None` for a response that carried none, which is the ordinary case: OCPP has the CSMS
    /// send a limit "only once for every change in cost limit", so silence means "unchanged", not
    /// "no limit". 2.1 only - neither 2.0.1's nor 1.6J's response has the field, so their
    /// notifiers always report `None`.
    pub transaction_limit: Option<crate::state::TransactionLimit>,
}

impl TransactionEventOutcome {
    /// A response saying nothing about the identifier - what a notifier that cannot read one
    /// (OCPP 1.6J's `MeterValues`, say) returns.
    pub fn acknowledged() -> Self {
        Self::default()
    }

    /// A response carrying `idTokenInfo.status`.
    pub fn with_id_token_status(status: crate::state::AuthorizationStatus) -> Self {
        Self {
            id_token_status: Some(status),
            ..Self::default()
        }
    }

    /// This outcome with the CSMS's transaction limit attached (E16.FR.02).
    pub fn with_transaction_limit(self, limit: crate::state::TransactionLimit) -> Self {
        Self {
            transaction_limit: Some(limit),
            ..self
        }
    }

    /// Whether the CSMS refused the identifier this transaction is running under (E05).
    ///
    /// Only an explicit rejection counts. A response with no `idTokenInfo` says nothing about the
    /// identifier, and treating silence as refusal would cut a driver off mid-session because the
    /// CSMS chose to answer tersely.
    pub fn id_token_rejected(&self) -> bool {
        self.id_token_status == Some(crate::state::AuthorizationStatus::Rejected)
    }
}

/// Reports a transaction lifecycle event to the CSMS via TransactionEvent. Implemented per
/// protocol version (see the `ocpp_2_1` module), mirroring
/// [`crate::availability::StatusNotifier`].
#[async_trait::async_trait]
pub trait TransactionNotifier {
    /// The error type returned if the TransactionEvent request itself fails.
    type Error: core::error::Error + Send + Sync + 'static;

    /// Reports a transaction lifecycle event of `kind` for `transaction`, on the given
    /// connector.
    ///
    /// `offline` is OCPP's `offline` flag (E12, `docs/OCPP-2.1-COMPLIANCE-ROADMAP.md` CV6.1):
    /// `true` when this event was generated while the CSMS was unreachable and has been sitting in
    /// [`crate::offline_queue::OfflineQueue`] since. The encoder cannot work that out for itself -
    /// by the time it runs, the connection is back - so the queue tells it (see
    /// [`crate::offline_queue::OfflineQueue::marking_offline`]). A notifier for a protocol version
    /// with no such field ignores it.
    ///
    /// Returns what the CSMS said back, so the caller can act on a mid-session deauthorization -
    /// see [`TransactionEventOutcome`] and [`deliver_transaction_event`].
    async fn notify_transaction_event(
        &self,
        evse_id: usize,
        connector_id: usize,
        kind: TransactionEventKind,
        transaction: crate::state::Transaction,
        offline: bool,
    ) -> Result<TransactionEventOutcome, Self::Error>;
}

/// Reports `occurred` via `notifier` and applies whatever the CSMS said back to `actor` - the one
/// place a `TransactionEvent`'s *response* turns into charge-point behaviour.
///
/// **E05 (CV2.5).** A CSMS blocklist update lands mid-session as a rejected `idTokenInfo` on the
/// response to an ordinary `Updated` event. That raises
/// [`ConnectorEvent::AuthorizationRevoked`](crate::state::ConnectorEvent::AuthorizationRevoked),
/// whose consequences are the operator's configuration and not this function's business:
/// `TxCtrlr.StopTxOnInvalidId` stops at once, `TxCtrlr.MaxEnergyOnInvalidId` grants a last
/// allowance instead. Raised at most once per session, because the state machine ignores a second
/// revocation on a connector that has already left `Charging`.
///
/// A delivery failure is returned unchanged, so an offline queue still queues and retries it.
pub async fn deliver_transaction_event<N: TransactionNotifier>(
    notifier: &N,
    actor: &crate::actor::ChargePointActor,
    occurred: TransactionEventOccurred,
) -> Result<(), N::Error> {
    let (evse_id, connector_id) = (occurred.evse_id, occurred.connector_id);
    let outcome = notifier
        .notify_transaction_event(
            evse_id,
            connector_id,
            occurred.kind,
            occurred.transaction,
            occurred.offline,
        )
        .await?;
    // E16.FR.02/.03 (CV15): a ceiling the CSMS placed on this transaction. Applied before the
    // deauthorization check below because the two are independent - a response can carry both,
    // and a revoked identifier does not make the limit uninteresting to record.
    if let Some(limit) = outcome.transaction_limit {
        tracing::debug!(evse_id, connector_id, "the CSMS set a transaction limit");
        let _ = actor
            .send(crate::state::ChargePointEvent::Evse {
                evse_id,
                event: crate::state::EvseEvent::Connector {
                    connector_id,
                    event: crate::state::ConnectorEvent::TransactionLimitSet {
                        limit,
                        from_csms: true,
                    },
                },
            })
            .await;
    }
    if outcome.id_token_rejected() {
        tracing::info!(
            evse_id,
            connector_id,
            "the CSMS refused this transaction's identifier; revoking the authorization"
        );
        let _ = actor
            .send(crate::state::ChargePointEvent::Evse {
                evse_id,
                event: crate::state::EvseEvent::Connector {
                    connector_id,
                    event: crate::state::ConnectorEvent::AuthorizationRevoked,
                },
            })
            .await;
    }
    Ok(())
}

/// Forwards to the wrapped notifier - see [`crate::availability::StatusNotifier`]'s `Arc` impl for
/// why this exists.
#[async_trait::async_trait]
impl<N: TransactionNotifier + Send + Sync> TransactionNotifier for alloc::sync::Arc<N> {
    type Error = N::Error;

    async fn notify_transaction_event(
        &self,
        evse_id: usize,
        connector_id: usize,
        kind: crate::state::TransactionEventKind,
        transaction: crate::state::Transaction,
        offline: bool,
    ) -> Result<TransactionEventOutcome, Self::Error> {
        (**self)
            .notify_transaction_event(evse_id, connector_id, kind, transaction, offline)
            .await
    }
}

/// Forwards every transaction event received on `events` to the CSMS via `notifier`, forever.
/// Errors are logged and do not stop the loop - the actor already applied the event to state;
/// only the CSMS-facing report failed and is not retried. [`setup`](crate::setup) uses
/// [`crate::offline_queue::run_with_offline_queue`] instead, which queues and retries a failed
/// report rather than just logging it - prefer that for `TransactionEvent`, where a report the
/// CSMS never receives can mean lost billing/session history; this simpler fire-and-forget
/// version remains for callers who don't need retry.
///
/// The response is discarded, so a mid-session deauthorization (E05) is *not* acted on here: doing
/// that needs a [`crate::actor::ChargePointActor`] to raise the event against, which this loop
/// deliberately does not take. Callers who need it use [`deliver_transaction_event`], which
/// `crate::builder`'s queued path does.
pub async fn run_transaction_events<N: TransactionNotifier>(
    mut events: BroadcastReceiver<TransactionEventOccurred>,
    notifier: &N,
) {
    while let Ok(occurred) = events.recv().await {
        if let Err(err) = notifier
            .notify_transaction_event(
                occurred.evse_id,
                occurred.connector_id,
                occurred.kind,
                occurred.transaction,
                occurred.offline,
            )
            .await
        {
            tracing::warn!(error = %err, "transaction event notification failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{TransactionEventOutcome, TransactionNotifier, run_transaction_events};
    use crate::state::{
        Transaction, TransactionChargingState, TransactionEventKind, TransactionEventOccurred,
        TransactionId,
    };
    use crate::sync::broadcast_channel;
    use alloc::boxed::Box;
    use alloc::vec::Vec;
    use tokio::sync::watch;

    /// One call this notifier saw: `(evse_id, connector_id, kind, transaction, offline)`.
    type SeenEvent = (usize, usize, TransactionEventKind, Transaction, bool);

    struct RecordingTransactionNotifier {
        seen: watch::Sender<Vec<SeenEvent>>,
    }

    #[async_trait::async_trait]
    impl TransactionNotifier for RecordingTransactionNotifier {
        type Error = core::convert::Infallible;

        async fn notify_transaction_event(
            &self,
            evse_id: usize,
            connector_id: usize,
            kind: TransactionEventKind,
            transaction: Transaction,
            offline: bool,
        ) -> Result<TransactionEventOutcome, Self::Error> {
            self.seen
                .send_modify(|seen| seen.push((evse_id, connector_id, kind, transaction, offline)));
            Ok(TransactionEventOutcome::acknowledged())
        }
    }

    #[tokio::test]
    async fn forwards_every_transaction_event_to_the_notifier_in_order() {
        let sender = broadcast_channel();
        let receiver = sender.subscribe();
        let (seen_tx, mut seen_rx) = watch::channel(Vec::new());
        let notifier = RecordingTransactionNotifier { seen: seen_tx };

        let forwarder = tokio::spawn(async move {
            run_transaction_events(receiver, &notifier).await;
        });

        let transaction = Transaction {
            id: TransactionId(0),
            id_token: None,
            charging_state: TransactionChargingState::EvConnected,
            stop_reason: None,
            seq_no: 0,
            last_meter_sample: None,
            priority_charging: false,
            remote_start_id: None,
            reservation_id: None,
            stop_at_energy_wh: None,
            limit: None,
            csms_limit: None,
            limit_reached: None,
            energy_start_wh: None,
        };
        sender.send(TransactionEventOccurred {
            evse_id: 0,
            connector_id: 0,
            kind: TransactionEventKind::Started,
            transaction: transaction.clone(),
            offline: false,
        });

        seen_rx
            .wait_for(|seen| seen.len() == 1)
            .await
            .expect("notifier task is still running");

        drop(sender);
        forwarder.await.unwrap();

        assert_eq!(
            *seen_rx.borrow(),
            alloc::vec![(0, 0, TransactionEventKind::Started, transaction, false)]
        );
    }

    /// E05 (CV2.5): silence is not refusal. A `TransactionEventResponse` with no `idTokenInfo` is
    /// the ordinary answer to an event that quoted no identifier, and reading it as a rejection
    /// would cut a driver off mid-session because the CSMS answered tersely.
    #[test]
    fn only_an_explicit_rejection_revokes_the_identifier() {
        use crate::state::AuthorizationStatus;

        assert!(!TransactionEventOutcome::acknowledged().id_token_rejected());
        assert!(
            !TransactionEventOutcome::with_id_token_status(AuthorizationStatus::Accepted)
                .id_token_rejected()
        );
        assert!(
            TransactionEventOutcome::with_id_token_status(AuthorizationStatus::Rejected)
                .id_token_rejected()
        );
    }

    /// A notifier that answers every event with a fixed outcome - the CSMS half of the E05 test
    /// below.
    struct FixedOutcomeNotifier(TransactionEventOutcome);

    #[async_trait::async_trait]
    impl TransactionNotifier for FixedOutcomeNotifier {
        type Error = core::convert::Infallible;

        async fn notify_transaction_event(
            &self,
            _evse_id: usize,
            _connector_id: usize,
            _kind: TransactionEventKind,
            _transaction: Transaction,
            _offline: bool,
        ) -> Result<TransactionEventOutcome, Self::Error> {
            Ok(self.0)
        }
    }

    /// Drives connector 0 of a fresh actor to `Charging` with a transaction running.
    async fn charging_actor() -> crate::actor::ChargePointActor {
        use crate::state::{ChargePointEvent, ConnectorEvent, EvseEvent, IdToken, IdTokenKind};

        let actor = crate::actor::ChargePointActor::spawn([1], &crate::executor::TokioExecutor);
        let id_token = IdToken {
            value: "04A224B2".into(),
            kind: IdTokenKind::ISO14443,
        };
        for event in [
            ConnectorEvent::CableConnected,
            ConnectorEvent::LockConfirmed,
            ConnectorEvent::IdTokenPresented(id_token.clone()),
            ConnectorEvent::ChargingAuthorized(id_token),
            ConnectorEvent::ContactorClosed,
        ] {
            let _ = actor
                .send(ChargePointEvent::Evse {
                    evse_id: 0,
                    event: EvseEvent::Connector {
                        connector_id: 0,
                        event,
                    },
                })
                .await;
        }
        actor
    }

    /// **E05 (CV2.5), the producer this crate was missing.** A CSMS blocklist update arrives as a
    /// rejected `idTokenInfo` on the response to an ordinary transaction event, and the charge
    /// point has to act on it rather than log it.
    #[tokio::test]
    async fn a_rejected_id_token_on_the_response_stops_the_session() {
        use crate::state::{AuthorizationStatus, ConnectorState};

        let actor = charging_actor().await;
        assert_eq!(
            actor.state().evses[0].connectors[0],
            ConnectorState::Charging
        );
        let notifier = FixedOutcomeNotifier(TransactionEventOutcome::with_id_token_status(
            AuthorizationStatus::Rejected,
        ));

        super::deliver_transaction_event(
            &notifier,
            &actor,
            TransactionEventOccurred {
                evse_id: 0,
                connector_id: 0,
                kind: TransactionEventKind::Updated(
                    crate::state::TransactionUpdateReason::MeterValuePeriodic,
                ),
                transaction: actor.state().evses[0].transactions[0].clone().unwrap(),
                offline: false,
            },
        )
        .await
        .expect("the notifier cannot fail");

        // `StopTxOnInvalidId` defaults to `true`, so the stop is immediate and fail-safe.
        assert_eq!(
            actor.state().evses[0].connectors[0],
            ConnectorState::Stopping
        );
    }

    /// CV15 (E16.FR.02): a ceiling on the response is not an acknowledgement to be logged - it is
    /// the CSMS setting a limit this session must run under, so it has to reach the state machine.
    #[tokio::test]
    async fn a_transaction_limit_on_the_response_reaches_the_transaction() {
        let actor = charging_actor().await;
        let notifier = FixedOutcomeNotifier(
            TransactionEventOutcome::acknowledged().with_transaction_limit(
                crate::state::TransactionLimit {
                    max_energy_wh: Some(20_000.0),
                    ..Default::default()
                },
            ),
        );

        super::deliver_transaction_event(
            &notifier,
            &actor,
            TransactionEventOccurred {
                evse_id: 0,
                connector_id: 0,
                kind: TransactionEventKind::Started,
                transaction: actor.state().evses[0].transactions[0].clone().unwrap(),
                offline: false,
            },
        )
        .await
        .expect("the notifier cannot fail");

        assert_eq!(
            actor.state().evses[0].transactions[0]
                .as_ref()
                .and_then(|transaction| transaction.limit)
                .and_then(|limit| limit.max_energy_wh),
            Some(20_000.0),
        );
        // And it is remembered as the CSMS's ceiling, which is what later clamps a locally-set
        // one (E16.FR.04).
        assert!(
            actor.state().evses[0].transactions[0]
                .as_ref()
                .is_some_and(|transaction| transaction.csms_limit.is_some())
        );
    }

    /// The far more common case, and the one a bug here would break: an accepted (or silent)
    /// response leaves the session exactly alone.
    #[tokio::test]
    async fn an_accepted_response_leaves_the_session_charging() {
        use crate::state::ConnectorState;

        for outcome in [
            TransactionEventOutcome::acknowledged(),
            TransactionEventOutcome::with_id_token_status(
                crate::state::AuthorizationStatus::Accepted,
            ),
        ] {
            let actor = charging_actor().await;
            let notifier = FixedOutcomeNotifier(outcome);

            super::deliver_transaction_event(
                &notifier,
                &actor,
                TransactionEventOccurred {
                    evse_id: 0,
                    connector_id: 0,
                    kind: TransactionEventKind::Updated(
                        crate::state::TransactionUpdateReason::MeterValuePeriodic,
                    ),
                    transaction: actor.state().evses[0].transactions[0].clone().unwrap(),
                    offline: false,
                },
            )
            .await
            .expect("the notifier cannot fail");

            assert_eq!(
                actor.state().evses[0].connectors[0],
                ConnectorState::Charging,
                "{outcome:?}"
            );
        }
    }
}

#[cfg(feature = "ocpp_2_1")]
pub(crate) mod ocpp_2_1 {
    pub use with_clock::Ocpp2_1TransactionNotifier;

    use crate::meter_values::MeasurandSet;
    use crate::state::{
        MeterSample, StopReason, Transaction, TransactionChargingState, TransactionEventKind,
        TransactionLimitKind, TransactionUpdateReason,
    };
    use crate::wire::v21::common::{
        ChargingStateEnum, MeasurandEnum, MeterValue, ReadingContextEnum, ReasonEnum, SampledValue,
        TransactionEventEnum, TriggerReasonEnum,
    };
    use alloc::vec;
    use alloc::vec::Vec;
    use chrono::{DateTime, Utc};

    pub(super) fn map_event_type(kind: TransactionEventKind) -> TransactionEventEnum {
        match kind {
            TransactionEventKind::Started => TransactionEventEnum::Started,
            TransactionEventKind::Updated(_) => TransactionEventEnum::Updated,
            TransactionEventKind::Ended => TransactionEventEnum::Ended,
        }
    }

    pub(super) fn map_charging_state(state: TransactionChargingState) -> ChargingStateEnum {
        match state {
            TransactionChargingState::EvConnected => ChargingStateEnum::EVConnected,
            TransactionChargingState::Charging => ChargingStateEnum::Charging,
            TransactionChargingState::SuspendedEV => ChargingStateEnum::SuspendedEV,
            TransactionChargingState::SuspendedEVSE => ChargingStateEnum::SuspendedEVSE,
            TransactionChargingState::Idle => ChargingStateEnum::Idle,
        }
    }

    pub(super) fn map_stop_reason(reason: StopReason) -> ReasonEnum {
        match reason {
            StopReason::Local => ReasonEnum::Local,
            StopReason::Remote => ReasonEnum::Remote,
            StopReason::EVDisconnected => ReasonEnum::EVDisconnected,
            StopReason::EmergencyStop => ReasonEnum::EmergencyStop,
            StopReason::Reset => ReasonEnum::ImmediateReset,
            StopReason::PowerLoss => ReasonEnum::PowerLoss,
            StopReason::DeAuthorized => ReasonEnum::DeAuthorized,
        }
    }

    /// The OCPP `triggerReason` a TransactionEvent carries isn't part of our internal
    /// `Transaction`/`TransactionEventKind` model (see CLAUDE.md's version-adapter principle) -
    /// it's derived here from the event kind and, for `Ended`, the transaction's stop reason.
    pub(super) fn trigger_reason_for(
        kind: TransactionEventKind,
        transaction: &Transaction,
    ) -> TriggerReasonEnum {
        match kind {
            // F01.FR.19/F02.FR.21 (CV6): a transaction the CSMS asked for reports what actually
            // triggered it. A `remoteStartId` is only ever set by
            // `handle_request_start_transaction`, so its presence *is* "this was a RemoteStart".
            TransactionEventKind::Started if transaction.remote_start_id.is_some() => {
                TriggerReasonEnum::RemoteStart
            }
            TransactionEventKind::Started => TriggerReasonEnum::Authorized,
            TransactionEventKind::Updated(TransactionUpdateReason::ChargingStateChanged) => {
                TriggerReasonEnum::ChargingStateChanged
            }
            TransactionEventKind::Updated(TransactionUpdateReason::MeterValuePeriodic) => {
                TriggerReasonEnum::MeterValuePeriodic
            }
            // K11.FR.04/K13.FR.03 (CV18): an external control system moved the rate.
            TransactionEventKind::Updated(TransactionUpdateReason::ChargingRateChanged) => {
                TriggerReasonEnum::ChargingRateChanged
            }
            // E16.FR.01/.03 (CV15): confirming the ceiling now in force.
            TransactionEventKind::Updated(TransactionUpdateReason::LimitSet) => {
                TriggerReasonEnum::LimitSet
            }
            // E16.FR.05: *which* ceiling was reached, which is the whole reason the four values
            // exist - a bare `SuspendedEVSE` cannot be told from a smart-charging suspension.
            TransactionEventKind::Updated(TransactionUpdateReason::LimitReached(kind)) => {
                match kind {
                    TransactionLimitKind::Cost => TriggerReasonEnum::CostLimitReached,
                    TransactionLimitKind::Energy => TriggerReasonEnum::EnergyLimitReached,
                    TransactionLimitKind::Soc => TriggerReasonEnum::SoCLimitReached,
                    TransactionLimitKind::Time => TriggerReasonEnum::TimeLimitReached,
                }
            }
            TransactionEventKind::Ended => match transaction.stop_reason {
                Some(StopReason::EmergencyStop) => TriggerReasonEnum::AbnormalCondition,
                Some(StopReason::Remote) => TriggerReasonEnum::RemoteStop,
                Some(StopReason::EVDisconnected) => TriggerReasonEnum::EVDeparted,
                Some(StopReason::Reset) => TriggerReasonEnum::ResetCommand,
                // No `PowerLoss` trigger reason exists in 2.x - a transaction closed out from
                // its persisted record on the next boot is exactly the "something went wrong
                // outside the normal flow" case `AbnormalCondition` covers. The precise cause
                // still reaches the CSMS in `stoppedReason` (`ReasonEnum::PowerLoss`).
                Some(StopReason::PowerLoss) => TriggerReasonEnum::AbnormalCondition,
                // E05: the CSMS refused the identifier, so that is what triggered the end.
                Some(StopReason::DeAuthorized) => TriggerReasonEnum::Deauthorized,
                Some(StopReason::Local) | None => TriggerReasonEnum::StopAuthorized,
            },
        }
    }

    /// Builds one `sampledValue` per measurand that `sample` carries **and** `measurands` selects -
    /// the energy register plus power/current/voltage/SoC where the hardware reported them (J01,
    /// J02, CV2.6).
    ///
    /// The two conditions are separate on purpose. What the hardware reported is a fact about this
    /// station; what `measurands` selects is what the CSMS asked to be told about. A measurand the
    /// CSMS did not ask for is dropped even when the meter produced it, and one it did ask for is
    /// still absent when the meter did not - inventing a reading to satisfy a configured list would
    /// be worse than omitting it.
    ///
    /// Shared with [`crate::meter_values`]' standalone `MeterValues` adapter through
    /// `build_meter_values`: a reading is a reading, and two mappings of the same measurands into
    /// the same wire type would be two places for a unit bug to hide.
    fn sampled_values(sample: MeterSample, measurands: MeasurandSet) -> Vec<SampledValue> {
        let mut values = Vec::new();
        if measurands.energy {
            values.push(sampled_value(
                sample.energy_wh as f64,
                MeasurandEnum::EnergyActiveImportRegister,
            ));
        }
        if let Some(power_w) = sample.power_w.filter(|_| measurands.power) {
            values.push(sampled_value(
                power_w as f64,
                MeasurandEnum::PowerActiveImport,
            ));
        }
        if let Some(current_ma) = sample.current_ma.filter(|_| measurands.current) {
            values.push(sampled_value(
                current_ma as f64 / 1_000.0,
                MeasurandEnum::CurrentImport,
            ));
        }
        if let Some(voltage_v) = sample.voltage_v.filter(|_| measurands.voltage) {
            values.push(sampled_value(voltage_v as f64, MeasurandEnum::Voltage));
        }
        if let Some(soc_percent) = sample.soc_percent.filter(|_| measurands.soc) {
            values.push(sampled_value(soc_percent as f64, MeasurandEnum::SoC));
        }
        values
    }

    fn sampled_value(value: f64, measurand: MeasurandEnum) -> SampledValue {
        SampledValue {
            value,
            measurand: Some(measurand),
            context: Some(ReadingContextEnum::SamplePeriodic),
            phase: None,
            location: None,
            signed_meter_value: None,
            unit_of_measure: None,
            custom_data: None,
        }
    }

    /// Builds the TransactionEvent `meterValue` list from a transaction's most recent sample -
    /// empty if it never got one (e.g. `Started`, or a transaction that ended before charging
    /// began), and empty if `measurands` selects nothing. See `docs/ROADMAP.md` §10.
    ///
    /// A cleared measurand list produces **no `MeterValue` at all** rather than one carrying an
    /// empty `sampledValue` list (CV2.6): the latter reports a timestamp and nothing else, which
    /// tells the CSMS nothing it did not already know while still costing it a message to parse.
    /// Staying a pure function of its arguments is what lets the caller read the configuration
    /// fresh per message without this function needing to know a device model exists.
    pub(crate) fn build_meter_values(
        sample: Option<MeterSample>,
        timestamp: DateTime<Utc>,
        measurands: MeasurandSet,
    ) -> Vec<MeterValue> {
        let Some(sample) = sample else {
            return Vec::new();
        };
        let sampled_value = sampled_values(sample, measurands);
        if sampled_value.is_empty() {
            return Vec::new();
        }
        vec![MeterValue {
            timestamp: timestamp.into(),
            sampled_value,
            custom_data: None,
        }]
    }

    // `TransactionEventRequest` needs a timestamp, produced from a caller-supplied `Clock` - see
    // `crate::clock::is_synchronized`'s docs for the policy on an unsynchronized reading (send it
    // as-is, warn, never fabricate or drop it).
    mod with_clock {
        use super::{
            build_meter_values, map_charging_state, map_event_type, map_stop_reason,
            trigger_reason_for,
        };
        use crate::actor::ChargePointActor;
        use crate::clock::{Clock, is_synchronized};
        use crate::meter_values::{MeasurandSet, transaction_event_measurands};
        use crate::pricing::{DimensionCost, TotalCostKind, TransactionCost};
        use crate::state::{Tariff, Transaction, TransactionEventKind, TransactionUpdateReason};
        use crate::tariff::advance_running_cost;
        use crate::transactions::{TransactionEventOutcome, TransactionNotifier};
        use crate::wire::v21::TransactionEventRequest;
        use crate::wire::v21::common::AuthorizationStatusEnum;
        use crate::wire::v21::common::{
            ChargingPeriod, CostDetails, CostDimension, CostDimensionEnum, EVSE, Price,
            TariffCostEnum, TotalCost, TotalPrice, TotalUsage, Transaction as WireTransaction,
            TransactionLimit as WireTransactionLimit,
        };
        use alloc::boxed::Box;
        use alloc::vec::Vec;
        use chrono::{DateTime, Utc};
        use ocpp_client::ClientError;
        use ocpp_client::ocpp_2_1::{OCPP2_1Client, OCPP2_1Error};

        /// Converts what [`advance_running_cost`] computed into OCPP's `CostDetailsType`, for
        /// I12's `costDetails` on a `TransactionEventRequest`. `None` only if the tariff's own
        /// `currency` does not fit ISO 4217's three letters - not reachable through this crate's
        /// own `SetDefaultTariff`/`ChangeTransactionTariff` handlers, but a malformed value should
        /// drop the report rather than crash it.
        fn build_cost_details(cost: &TransactionCost, tariff: &Tariff) -> Option<CostDetails> {
            let totals = cost.totals(tariff);
            let usage = cost.usage();
            let currency = heapless::String::try_from(totals.currency.as_str()).ok()?;

            let price = |amount: DimensionCost| Price {
                custom_data: None,
                excl_tax: Some(amount.excl_tax.to_decimal()),
                incl_tax: Some(amount.incl_tax.to_decimal()),
                tax_rates: None,
            };
            let total_cost = TotalCost {
                charging_time: totals.charging_time.map(price),
                currency,
                custom_data: None,
                energy: totals.energy.map(price),
                fixed: totals.fixed.map(price),
                idle_time: totals.idle_time.map(price),
                reservation_fixed: totals.reservation_fixed.map(price),
                reservation_time: totals.reservation_time.map(price),
                total: TotalPrice {
                    custom_data: None,
                    excl_tax: Some(totals.total.excl_tax.to_decimal()),
                    incl_tax: Some(totals.total.incl_tax.to_decimal()),
                },
                type_of_cost: match totals.kind {
                    TotalCostKind::Normal => TariffCostEnum::NormalCost,
                    TotalCostKind::Min => TariffCostEnum::MinCost,
                    TotalCostKind::Max => TariffCostEnum::MaxCost,
                },
            };
            // `Usage`'s energy is in mWh (see `crate::pricing`'s scale docs); the wire wants kWh,
            // and 10^6 mWh is 1 kWh - the same scale that makes `Money::SCALE` line up with a
            // whole currency unit.
            let total_usage = TotalUsage {
                charging_time: usage.charging_secs,
                custom_data: None,
                energy: usage.energy_mwh as f64 / 1_000_000.0,
                idle_time: usage.idle_secs,
                reservation_time: usage.reservation_secs,
            };
            // Per-period dimensions are limited to what converts unambiguously: energy, charging
            // time and idle time all share a scale already used elsewhere in this crate. Power and
            // current would need their own unit conversion this module has not yet verified
            // against a wire example, so `CostPeriod::min/max_power_mw`/`min/max_current_ma` are
            // left unreported for now rather than guessed at.
            let charging_periods = cost
                .periods()
                .iter()
                .map(|period| {
                    let dimension = |kind, volume| CostDimension {
                        custom_data: None,
                        r#type: kind,
                        volume,
                    };
                    ChargingPeriod {
                        custom_data: None,
                        dimensions: Some(alloc::vec![
                            dimension(
                                CostDimensionEnum::Energy,
                                period.energy_mwh as f64 / 1_000_000.0
                            ),
                            dimension(CostDimensionEnum::ChargingTime, period.charging_secs as f64),
                            dimension(CostDimensionEnum::IdleTIme, period.idle_secs as f64),
                        ]),
                        start_period: period.start.into(),
                        tariff_id: heapless::String::try_from(period.tariff_id.0.as_str()).ok(),
                    }
                })
                .collect::<Vec<_>>();

            Some(CostDetails {
                charging_periods: (!charging_periods.is_empty()).then_some(charging_periods),
                custom_data: None,
                failure_reason: None,
                failure_to_calculate: None,
                total_cost,
                total_usage,
            })
        }

        /// Builds the `TransactionEventRequest` for `kind`/`transaction`, timestamped `now` and
        /// carrying only the measurands `measurands` selects. Pure (no network I/O, no
        /// device-model read, no clock of its own), so a fixed instant can assert the exact
        /// timestamp that reaches the wire request without needing a live `OCPP2_1Client` - see
        /// this module's tests. Warns (see `crate::clock::is_synchronized`'s docs) but still
        /// builds and returns the request when `now` reads as unsynchronized - never substitutes,
        /// clamps, or omits it.
        ///
        /// `now` is taken rather than read from a [`Clock`] here so the same instant prices
        /// `cost` (see below) and stamps this request - `send_transaction_event`'s caller reads
        /// its `Clock` exactly once per event, for both.
        ///
        /// `cost` is whatever [`advance_running_cost`] computed for this transaction at `now` -
        /// `None` when no tariff currently prices it, in which case `costDetails` and
        /// `transactionInfo.tariffId` are both left unset rather than reporting stale figures
        /// (I07, I08, I11, I12).
        #[allow(clippy::too_many_arguments)]
        fn build_transaction_event_request(
            now: DateTime<Utc>,
            evse_id: usize,
            connector_id: usize,
            kind: TransactionEventKind,
            transaction: Transaction,
            offline: bool,
            measurands: MeasurandSet,
            cost: Option<(TransactionCost, Tariff)>,
        ) -> TransactionEventRequest {
            if !is_synchronized(&now) {
                tracing::warn!(
                    timestamp = %now,
                    "TransactionEvent timestamp sourced from an unsynchronized clock"
                );
            }
            let meter_value = build_meter_values(transaction.last_meter_sample, now, measurands);
            let cost_details = cost
                .as_ref()
                .and_then(|(cost, tariff)| build_cost_details(cost, tariff));
            let tariff_id = cost
                .as_ref()
                .and_then(|(_, tariff)| heapless::String::try_from(tariff.id.0.as_str()).ok());
            TransactionEventRequest {
                custom_data: None,
                cost_details,
                event_type: map_event_type(kind),
                evse_sleep: None,
                meter_value: if meter_value.is_empty() {
                    None
                } else {
                    Some(meter_value)
                },
                timestamp: now.into(),
                trigger_reason: trigger_reason_for(kind, &transaction),
                seq_no: transaction.seq_no as i64,
                preconditioning_status: None,
                transaction_info: WireTransaction {
                    // The transaction id is an internal `u64` formatted as decimal, always
                    // well within the wire field's 36-byte bound.
                    transaction_id: heapless::String::try_from(transaction.id.0)
                        .expect("u64 transaction id always fits in a 36-byte wire field"),
                    charging_state: Some(map_charging_state(transaction.charging_state)),
                    time_spent_charging: None,
                    stopped_reason: transaction.stop_reason.map(map_stop_reason),
                    // CV6: F01.FR.25/F02.FR.01 - the CSMS correlates the transaction it now sees
                    // with the request it made.
                    remote_start_id: transaction.remote_start_id,
                    operation_mode: None,
                    // I11/I12: which tariff is (or just priced) this transaction.
                    tariff_id,
                    // E16.FR.01/.03 (CV15): the confirmation, carried exactly once - on the event
                    // whose trigger reason *is* `LimitSet`. Keying off the event kind rather than
                    // a "still to be reported" flag means the two cannot drift: the field is the
                    // confirmation, so it is present precisely where the trigger reason says it
                    // will be. Already filtered to what this build enforces (E16.FR.13) - the
                    // state machine did that before recording it.
                    transaction_limit: matches!(
                        kind,
                        TransactionEventKind::Updated(TransactionUpdateReason::LimitSet)
                    )
                    .then(|| transaction.limit)
                    .flatten()
                    .map(|limit| WireTransactionLimit {
                        custom_data: None,
                        max_cost: limit.max_cost,
                        max_energy: limit.max_energy_wh,
                        max_so_c: limit.max_soc_percent.map(i64::from),
                        max_time: limit.max_time_secs,
                    }),
                    custom_data: None,
                },
                // E12 (CV6.1): only stated when it is true. OCPP defaults the field to `false`,
                // so an event that went out on its first attempt says nothing rather than
                // asserting a negative.
                offline: offline.then_some(true),
                number_of_phases_used: None,
                cable_max_current: None,
                // CV6: F02.FR.06/H03 - the reservation this session consumed, so the CSMS can
                // close it out against the transaction that used it.
                reservation_id: transaction.reservation_id,
                evse: Some(EVSE {
                    id: evse_id as i64,
                    connector_id: Some(connector_id as i64),
                    custom_data: None,
                }),
                id_token: None,
            }
        }

        #[allow(clippy::too_many_arguments)]
        async fn send_transaction_event(
            client: &OCPP2_1Client,
            now: DateTime<Utc>,
            evse_id: usize,
            connector_id: usize,
            kind: TransactionEventKind,
            transaction: Transaction,
            offline: bool,
            measurands: MeasurandSet,
            cost: Option<(TransactionCost, Tariff)>,
        ) -> Result<TransactionEventOutcome, ClientError<OCPP2_1Error>> {
            let request = build_transaction_event_request(
                now,
                evse_id,
                connector_id,
                kind,
                transaction,
                offline,
                measurands,
                cost,
            );
            let response = client.send_transaction_event(request).await?;
            Ok(read_outcome(&response))
        }

        /// Reads what a `TransactionEventResponse` says about the identifier (E05, CV2.5).
        ///
        /// OCPP's `AuthorizationStatusEnumType` has ten values and this crate's has two - see
        /// `docs/ROADMAP.md` §3 for why. Everything that is not `Accepted` collapses to
        /// `Rejected`, which is the conservative direction: `Blocked`, `Expired`, `NoCredit` and
        /// the rest all mean this identifier may not keep charging.
        fn read_outcome(
            response: &crate::wire::v21::TransactionEventResponse,
        ) -> TransactionEventOutcome {
            let outcome = match response.id_token_info.as_ref() {
                Some(info) => TransactionEventOutcome::with_id_token_status(
                    if info.status == AuthorizationStatusEnum::Accepted {
                        crate::state::AuthorizationStatus::Accepted
                    } else {
                        crate::state::AuthorizationStatus::Rejected
                    },
                ),
                None => TransactionEventOutcome::acknowledged(),
            };
            // E16.FR.02 (CV15): the ceiling the CSMS is placing on this transaction. Read
            // verbatim - filtering it to what this build enforces is the state machine's job
            // (E16.FR.13), so the two questions stay in one place each.
            match response.transaction_limit.as_ref() {
                Some(limit) => outcome.with_transaction_limit(crate::state::TransactionLimit {
                    max_cost: limit.max_cost,
                    max_energy_wh: limit.max_energy,
                    // OCPP types the percentage as an integer; anything outside 0-100 is not a
                    // state of charge, and clamping would invent a limit the CSMS did not set.
                    max_soc_percent: limit.max_so_c.and_then(|soc| u8::try_from(soc).ok()),
                    max_time_secs: limit.max_time,
                }),
                None => outcome,
            }
        }

        /// Wraps an `OCPP2_1Client` with a caller-supplied [`Clock`] for the TransactionEvent
        /// timestamp, and the [`ChargePointActor`] whose device model says which measurands each
        /// event may carry - the no_std-reachable counterpart to this module's `std`-only
        /// `OCPP2_1Client` impl below.
        ///
        /// **The actor is read at send time, not at construction (CV2.6).** That is what makes
        /// `SampledDataCtrlr.Tx{Started,Updated,Ended}Measurands` behave like every other live
        /// variable in this crate: a CSMS `SetVariables` narrowing the list takes effect on the
        /// very next event, with no reconnect and no reboot. Holding a handle rather than a
        /// snapshot is the whole point - a snapshot would freeze the configuration at whatever it
        /// was when the session was wired up.
        ///
        /// `Clone` (and the [`ReconnectHandler`](crate::connection::ReconnectHandler) delegation
        /// below) so this can be registered through
        /// [`ChargePointBuilder::transaction_events`](crate::builder::ChargePointBuilder::transaction_events),
        /// which is what puts it on the path [`crate::connect::connect_and_setup`] actually uses -
        /// the same shape `Ocpp1_6TransactionNotifier` has always had.
        #[derive(Clone)]
        pub struct Ocpp2_1TransactionNotifier<C> {
            client: OCPP2_1Client,
            clock: C,
            actor: ChargePointActor,
        }

        /// Delegates to the wrapped client, so wrapping a client to filter its measurands does not
        /// cost the offline queue its reconnect flush.
        #[async_trait::async_trait]
        impl<C: Clock + Send + Sync> crate::connection::ReconnectHandler for Ocpp2_1TransactionNotifier<C> {
            async fn register_reconnect_handler<F, FF>(&self, callback: F)
            where
                F: FnMut() -> FF + Send + Sync + 'static,
                FF: core::future::Future<Output = ()> + Send + 'static,
            {
                crate::connection::ReconnectHandler::register_reconnect_handler(
                    &self.client,
                    callback,
                )
                .await;
            }
        }

        impl<C: Clock> Ocpp2_1TransactionNotifier<C> {
            /// Wraps `client`, sourcing every TransactionEvent timestamp from `clock` and every
            /// event's measurand selection from `actor`'s device model.
            pub fn with_clock(client: OCPP2_1Client, clock: C, actor: ChargePointActor) -> Self {
                Self {
                    client,
                    clock,
                    actor,
                }
            }
        }

        #[cfg(feature = "std")]
        impl Ocpp2_1TransactionNotifier<crate::clock::SystemClock> {
            /// Wraps `client`, sourcing every TransactionEvent timestamp from
            /// [`crate::clock::SystemClock`] and its measurand selection from `actor`.
            pub fn new(client: OCPP2_1Client, actor: ChargePointActor) -> Self {
                Self::with_clock(client, crate::clock::SystemClock, actor)
            }
        }

        #[async_trait::async_trait]
        impl<C: Clock + Send + Sync> TransactionNotifier for Ocpp2_1TransactionNotifier<C> {
            type Error = ClientError<OCPP2_1Error>;

            async fn notify_transaction_event(
                &self,
                evse_id: usize,
                connector_id: usize,
                kind: TransactionEventKind,
                transaction: Transaction,
                offline: bool,
            ) -> Result<TransactionEventOutcome, Self::Error> {
                // Read once and reused for both the cost calculation and the request's own
                // timestamp - see `build_transaction_event_request`'s docs on why.
                let now = self.clock.now();
                let cost =
                    advance_running_cost(&self.actor, evse_id, connector_id, now, &transaction)
                        .await;
                send_transaction_event(
                    &self.client,
                    now,
                    evse_id,
                    connector_id,
                    kind,
                    transaction,
                    offline,
                    transaction_event_measurands(&self.actor, kind),
                    cost,
                )
                .await
            }
        }

        /// The `std` convenience: `OCPP2_1Client` itself implements `TransactionNotifier` directly
        /// (sourcing its timestamp from [`crate::clock::SystemClock`]) so existing callers that
        /// pass a bare client - e.g. [`crate::connect::connect_and_setup`] - need no source change.
        ///
        /// **This impl cannot honour `SampledDataCtrlr`'s measurand lists, nor I07/I08/I11/I12's
        /// `costDetails`** and does not pretend to: a bare client has no [`ChargePointActor`] to
        /// ask, so it reports every measurand the sample carries ([`MeasurandSet::ALL`]) and no
        /// cost at all. A station that must honour J01/J02 or report a running cost wires
        /// [`Ocpp2_1TransactionNotifier`] through [`crate::builder::ChargePointBuilder`] instead.
        #[cfg(feature = "std")]
        #[async_trait::async_trait]
        impl TransactionNotifier for OCPP2_1Client {
            type Error = ClientError<OCPP2_1Error>;

            async fn notify_transaction_event(
                &self,
                evse_id: usize,
                connector_id: usize,
                kind: TransactionEventKind,
                transaction: Transaction,
                offline: bool,
            ) -> Result<TransactionEventOutcome, Self::Error> {
                send_transaction_event(
                    self,
                    crate::clock::SystemClock.now(),
                    evse_id,
                    connector_id,
                    kind,
                    transaction,
                    offline,
                    MeasurandSet::ALL,
                    // No actor, so no tariff to price against - see this impl's docs.
                    None,
                )
                .await
            }
        }

        #[cfg(test)]
        mod tests {
            use super::*;
            use crate::state::{TransactionChargingState, TransactionId};
            use chrono::{DateTime, Utc};

            /// A [`Clock`] that always reads a fixed, caller-chosen instant - mirrors
            /// `crate::persistence::tests::FixedClock`.
            struct FixedClock(DateTime<Utc>);

            impl Clock for FixedClock {
                fn now(&self) -> DateTime<Utc> {
                    self.0
                }
            }

            fn transaction() -> Transaction {
                Transaction {
                    id: TransactionId(7),
                    id_token: None,
                    charging_state: TransactionChargingState::Charging,
                    stop_reason: None,
                    seq_no: 0,
                    last_meter_sample: None,
                    priority_charging: false,
                    remote_start_id: None,
                    reservation_id: None,
                    stop_at_energy_wh: None,
                    limit: None,
                    csms_limit: None,
                    limit_reached: None,
                    energy_start_wh: None,
                }
            }

            #[test]
            fn a_fixed_clocks_reading_reaches_the_wire_request_timestamp() {
                let fixed = DateTime::parse_from_rfc3339("2026-03-04T05:06:07Z")
                    .unwrap()
                    .with_timezone(&Utc);
                let clock = FixedClock(fixed);

                let request = build_transaction_event_request(
                    clock.now(),
                    0,
                    0,
                    TransactionEventKind::Started,
                    transaction(),
                    false,
                    MeasurandSet::ALL,
                    None,
                );

                assert_eq!(request.timestamp, crate::wire::OcppTimestamp::from(fixed));
            }

            #[test]
            fn an_unsynchronized_clocks_reading_is_still_sent_not_substituted_or_dropped() {
                let unset_rtc = FixedClock(DateTime::<Utc>::from_timestamp(0, 0).unwrap());
                assert!(!is_synchronized(&unset_rtc.now()));

                // Guards the policy documented on `crate::clock::is_synchronized`: an
                // unsynchronized reading must reach the built request unchanged, never
                // substituted, clamped, or omitted.
                let request = build_transaction_event_request(
                    unset_rtc.now(),
                    0,
                    0,
                    TransactionEventKind::Started,
                    transaction(),
                    false,
                    MeasurandSet::ALL,
                    None,
                );

                assert_eq!(
                    request.timestamp,
                    crate::wire::OcppTimestamp::from(unset_rtc.0)
                );
            }

            /// **E12 (CV6.1)**: the flag says the event was *generated* while the CSMS was
            /// unreachable, which only the queue that held it knows - so it reaches the encoder as
            /// an argument. Absent rather than `false` when the event went out online: OCPP
            /// defaults the field, and a charge point should not assert a negative it was never
            /// asked about.
            #[test]
            fn the_offline_flag_is_stated_only_when_the_event_was_held() {
                let clock = FixedClock(DateTime::<Utc>::from_timestamp(1_800_000_000, 0).unwrap());

                let held = build_transaction_event_request(
                    clock.now(),
                    0,
                    0,
                    TransactionEventKind::Started,
                    transaction(),
                    true,
                    MeasurandSet::ALL,
                    None,
                );
                assert_eq!(held.offline, Some(true));

                let live = build_transaction_event_request(
                    clock.now(),
                    0,
                    0,
                    TransactionEventKind::Started,
                    transaction(),
                    false,
                    MeasurandSet::ALL,
                    None,
                );
                assert_eq!(live.offline, None);
            }

            /// A transaction carrying a full meter sample, so the measurand filter has something
            /// to filter.
            fn transaction_with_sample() -> Transaction {
                Transaction {
                    last_meter_sample: Some(crate::state::MeterSample {
                        energy_wh: 4_200,
                        power_w: Some(7_400),
                        voltage_v: Some(230),
                        ..Default::default()
                    }),
                    ..transaction()
                }
            }

            /// CV2.6/J01: `SampledDataCtrlr`'s list decides what a `TransactionEvent` reports,
            /// even where the meter produced more than the CSMS asked for.
            #[test]
            fn only_the_configured_measurands_reach_a_transaction_event() {
                let clock = FixedClock(DateTime::<Utc>::from_timestamp(1_800_000_000, 0).unwrap());

                let request = build_transaction_event_request(
                    clock.now(),
                    0,
                    0,
                    TransactionEventKind::Started,
                    transaction_with_sample(),
                    false,
                    MeasurandSet {
                        energy: true,
                        voltage: true,
                        ..MeasurandSet::default()
                    },
                    None,
                );

                let values = &request.meter_value.expect("a selected measurand")[0].sampled_value;
                assert_eq!(values.len(), 2);
                assert!(
                    values.iter().all(|value| value.measurand
                        != Some(crate::wire::v21::common::MeasurandEnum::PowerActiveImport)),
                    "power was sampled but not selected"
                );
            }

            /// CV15 (E16.FR.01/.03): the confirmation rides on the event whose trigger reason is
            /// `LimitSet`, and on no other. Both halves asserted together, because "once" is the
            /// requirement and a test of only the positive case would pass on a build that sent
            /// it every time.
            #[test]
            fn the_transaction_limit_is_confirmed_on_the_limit_set_event_and_no_other() {
                let clock = FixedClock(DateTime::<Utc>::from_timestamp(1_800_000_000, 0).unwrap());
                let limited = Transaction {
                    limit: Some(crate::state::TransactionLimit {
                        max_cost: Some(12.34),
                        max_energy_wh: Some(20_000.0),
                        ..Default::default()
                    }),
                    ..transaction_with_sample()
                };
                let request = |kind| {
                    build_transaction_event_request(
                        clock.now(),
                        0,
                        0,
                        kind,
                        limited.clone(),
                        false,
                        MeasurandSet::default(),
                        None,
                    )
                };

                let confirmation = request(TransactionEventKind::Updated(
                    TransactionUpdateReason::LimitSet,
                ));
                let limit = confirmation
                    .transaction_info
                    .transaction_limit
                    .expect("the LimitSet event carries the limit");
                assert_eq!(limit.max_cost, Some(12.34));
                assert_eq!(limit.max_energy, Some(20_000.0));
                assert_eq!(
                    confirmation.trigger_reason,
                    crate::wire::v21::common::TriggerReasonEnum::LimitSet
                );

                // The same transaction, still under the same limit, reporting a meter reading:
                // the limit is in force but has already been confirmed, so it is not repeated.
                let periodic = request(TransactionEventKind::Updated(
                    TransactionUpdateReason::MeterValuePeriodic,
                ));
                assert_eq!(periodic.transaction_info.transaction_limit, None);
            }

            /// E16.FR.05, at the wire: each ceiling reports its own trigger reason, which is the
            /// only thing telling a CSMS why the connector went `SuspendedEVSE`.
            #[test]
            fn each_limit_reached_reports_its_own_trigger_reason() {
                use crate::state::TransactionLimitKind;
                use crate::wire::v21::common::TriggerReasonEnum;

                for (kind, expected) in [
                    (
                        TransactionLimitKind::Cost,
                        TriggerReasonEnum::CostLimitReached,
                    ),
                    (
                        TransactionLimitKind::Energy,
                        TriggerReasonEnum::EnergyLimitReached,
                    ),
                    (
                        TransactionLimitKind::Soc,
                        TriggerReasonEnum::SoCLimitReached,
                    ),
                    (
                        TransactionLimitKind::Time,
                        TriggerReasonEnum::TimeLimitReached,
                    ),
                ] {
                    assert_eq!(
                        trigger_reason_for(
                            TransactionEventKind::Updated(TransactionUpdateReason::LimitReached(
                                kind
                            )),
                            &transaction(),
                        ),
                        expected,
                    );
                }
            }

            /// CV2.6: a cleared list produces no `meterValue` member at all. The event itself is a
            /// lifecycle fact and still goes out - it is the *readings* the CSMS declined.
            #[test]
            fn an_empty_measurand_set_leaves_a_transaction_event_with_no_meter_value() {
                let clock = FixedClock(DateTime::<Utc>::from_timestamp(1_800_000_000, 0).unwrap());

                let request = build_transaction_event_request(
                    clock.now(),
                    0,
                    0,
                    TransactionEventKind::Ended,
                    transaction_with_sample(),
                    false,
                    MeasurandSet::default(),
                    None,
                );

                assert_eq!(request.meter_value, None);
                assert_eq!(
                    request.event_type,
                    crate::wire::v21::common::TransactionEventEnum::Ended,
                    "the event still reports the transaction ending"
                );
            }

            fn priced_tariff() -> Tariff {
                let mut tariff = Tariff::new(crate::state::TariffId("t1".into()), "EUR");
                tariff.energy = Some(crate::state::EnergyComponent {
                    prices: alloc::vec![crate::state::EnergyPrice {
                        price_per_kwh: crate::state::Money(250_000),
                        conditions: None,
                    }],
                    tax_rates: Vec::new(),
                });
                tariff
            }

            // I07/I11/I12: whatever `advance_running_cost` computed reaches both `costDetails`
            // and `transactionInfo.tariffId` on the wire.
            #[test]
            fn cost_details_are_attached_when_a_tariff_priced_the_transaction() {
                let start = DateTime::parse_from_rfc3339("2026-03-04T05:06:07Z")
                    .unwrap()
                    .with_timezone(&Utc);
                let tariff = priced_tariff();
                let mut cost = TransactionCost::start(
                    &tariff,
                    &crate::pricing::PricingContext::new(start),
                    None,
                );
                let later = start + chrono::TimeDelta::hours(1);
                cost.advance(
                    &tariff,
                    &crate::pricing::PricingContext {
                        energy_mwh: 10_000_000,
                        charging: true,
                        ..crate::pricing::PricingContext::new(later)
                    },
                );

                let request = build_transaction_event_request(
                    later,
                    0,
                    0,
                    TransactionEventKind::Started,
                    transaction(),
                    false,
                    MeasurandSet::ALL,
                    Some((cost, tariff)),
                );

                let details = request
                    .cost_details
                    .expect("a tariff priced this transaction");
                assert_eq!(details.total_cost.currency.as_str(), "EUR");
                // 10 kWh @ 0.25/kWh.
                assert_eq!(details.total_cost.energy.unwrap().excl_tax, Some(2.5));
                assert_eq!(details.total_usage.energy, 10.0);
                assert_eq!(request.transaction_info.tariff_id.as_deref(), Some("t1"));
            }

            #[test]
            fn cost_details_are_absent_without_a_tariff() {
                let request = build_transaction_event_request(
                    Utc::now(),
                    0,
                    0,
                    TransactionEventKind::Started,
                    transaction(),
                    false,
                    MeasurandSet::ALL,
                    None,
                );

                assert_eq!(request.cost_details, None);
                assert_eq!(request.transaction_info.tariff_id, None);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::state::MeterSample;

        #[test]
        fn a_sample_with_only_energy_reports_a_single_sampled_value() {
            let sample = MeterSample {
                energy_wh: 1_500,
                ..Default::default()
            };
            let timestamp = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc);

            let values = build_meter_values(Some(sample), timestamp, MeasurandSet::ALL);

            assert_eq!(values.len(), 1);
            assert_eq!(values[0].sampled_value.len(), 1);
            assert_eq!(values[0].sampled_value[0].value, 1_500.0);
            assert_eq!(
                values[0].sampled_value[0].measurand,
                Some(MeasurandEnum::EnergyActiveImportRegister)
            );
        }

        #[test]
        fn a_sample_with_every_measurand_reports_one_sampled_value_per_measurand() {
            let sample = MeterSample {
                energy_wh: 1_500,
                power_w: Some(7_400),
                current_ma: Some(32_000),
                voltage_v: Some(230),
                soc_percent: Some(42),
            };
            let timestamp = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc);

            let values = build_meter_values(Some(sample), timestamp, MeasurandSet::ALL);

            assert_eq!(values.len(), 1);
            let sampled = &values[0].sampled_value;
            assert_eq!(sampled.len(), 5);
            assert!(sampled.iter().any(|value| value.measurand
                == Some(MeasurandEnum::EnergyActiveImportRegister)
                && value.value == 1_500.0));
            assert!(sampled.iter().any(|value| value.measurand
                == Some(MeasurandEnum::PowerActiveImport)
                && value.value == 7_400.0));
            assert!(sampled.iter().any(|value| value.measurand
                == Some(MeasurandEnum::CurrentImport)
                && value.value == 32.0));
            assert!(sampled.iter().any(
                |value| value.measurand == Some(MeasurandEnum::Voltage) && value.value == 230.0
            ));
            assert!(sampled
                .iter()
                .any(|value| value.measurand == Some(MeasurandEnum::SoC) && value.value == 42.0));
        }

        #[test]
        fn no_sample_reports_no_meter_values() {
            let timestamp = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc);

            assert_eq!(
                build_meter_values(None, timestamp, MeasurandSet::ALL),
                Vec::new()
            );
        }

        #[test]
        fn every_kind_maps_to_the_matching_wire_event_type() {
            assert_eq!(
                map_event_type(TransactionEventKind::Started),
                TransactionEventEnum::Started
            );
            assert_eq!(
                map_event_type(TransactionEventKind::Updated(
                    TransactionUpdateReason::ChargingStateChanged
                )),
                TransactionEventEnum::Updated
            );
            assert_eq!(
                map_event_type(TransactionEventKind::Ended),
                TransactionEventEnum::Ended
            );
        }

        #[test]
        fn every_charging_state_maps_to_the_matching_wire_state() {
            assert_eq!(
                map_charging_state(TransactionChargingState::EvConnected),
                ChargingStateEnum::EVConnected
            );
            assert_eq!(
                map_charging_state(TransactionChargingState::Charging),
                ChargingStateEnum::Charging
            );
            assert_eq!(
                map_charging_state(TransactionChargingState::SuspendedEV),
                ChargingStateEnum::SuspendedEV
            );
            assert_eq!(
                map_charging_state(TransactionChargingState::SuspendedEVSE),
                ChargingStateEnum::SuspendedEVSE
            );
            assert_eq!(
                map_charging_state(TransactionChargingState::Idle),
                ChargingStateEnum::Idle
            );
        }

        #[test]
        fn started_and_updated_use_fixed_trigger_reasons() {
            let transaction = Transaction {
                id: crate::state::TransactionId(0),
                id_token: None,
                charging_state: TransactionChargingState::Charging,
                stop_reason: None,
                seq_no: 0,
                last_meter_sample: None,
                priority_charging: false,
                remote_start_id: None,
                reservation_id: None,
                stop_at_energy_wh: None,
                limit: None,
                csms_limit: None,
                limit_reached: None,
                energy_start_wh: None,
            };

            assert_eq!(
                trigger_reason_for(TransactionEventKind::Started, &transaction),
                TriggerReasonEnum::Authorized
            );
            assert_eq!(
                trigger_reason_for(
                    TransactionEventKind::Updated(TransactionUpdateReason::ChargingStateChanged),
                    &transaction
                ),
                TriggerReasonEnum::ChargingStateChanged
            );
            assert_eq!(
                trigger_reason_for(
                    TransactionEventKind::Updated(TransactionUpdateReason::MeterValuePeriodic),
                    &transaction
                ),
                TriggerReasonEnum::MeterValuePeriodic
            );
        }

        /// CV6: F01.FR.19/F02.FR.21 - a transaction the CSMS asked for reports `RemoteStart`, not
        /// the `Authorized` a locally presented card produces. The `remoteStartId`'s presence is
        /// the discriminator, since only `handle_request_start_transaction` ever sets one.
        #[test]
        fn a_remotely_started_transaction_reports_remote_start_as_its_trigger() {
            let transaction = Transaction {
                id: crate::state::TransactionId(0),
                id_token: None,
                charging_state: TransactionChargingState::Charging,
                stop_reason: None,
                seq_no: 0,
                last_meter_sample: None,
                priority_charging: false,
                remote_start_id: Some(7),
                reservation_id: None,
                stop_at_energy_wh: None,
                limit: None,
                csms_limit: None,
                limit_reached: None,
                energy_start_wh: None,
            };

            assert_eq!(
                trigger_reason_for(TransactionEventKind::Started, &transaction),
                TriggerReasonEnum::RemoteStart
            );
            // Only the Started event is about how it began; a later update reports what changed.
            assert_eq!(
                trigger_reason_for(
                    TransactionEventKind::Updated(TransactionUpdateReason::ChargingStateChanged),
                    &transaction
                ),
                TriggerReasonEnum::ChargingStateChanged
            );
        }

        #[test]
        fn ended_derives_its_trigger_reason_from_the_stop_reason() {
            let base = Transaction {
                id: crate::state::TransactionId(0),
                id_token: None,
                charging_state: TransactionChargingState::EvConnected,
                stop_reason: None,
                seq_no: 2,
                last_meter_sample: None,
                priority_charging: false,
                remote_start_id: None,
                reservation_id: None,
                stop_at_energy_wh: None,
                limit: None,
                csms_limit: None,
                limit_reached: None,
                energy_start_wh: None,
            };

            assert_eq!(
                trigger_reason_for(
                    TransactionEventKind::Ended,
                    &Transaction {
                        stop_reason: Some(StopReason::EmergencyStop),
                        ..base.clone()
                    }
                ),
                TriggerReasonEnum::AbnormalCondition
            );
            assert_eq!(
                trigger_reason_for(
                    TransactionEventKind::Ended,
                    &Transaction {
                        stop_reason: Some(StopReason::Remote),
                        ..base.clone()
                    }
                ),
                TriggerReasonEnum::RemoteStop
            );
            assert_eq!(
                trigger_reason_for(
                    TransactionEventKind::Ended,
                    &Transaction {
                        stop_reason: Some(StopReason::EVDisconnected),
                        ..base.clone()
                    }
                ),
                TriggerReasonEnum::EVDeparted
            );
            assert_eq!(
                trigger_reason_for(
                    TransactionEventKind::Ended,
                    &Transaction {
                        stop_reason: Some(StopReason::Local),
                        ..base.clone()
                    }
                ),
                TriggerReasonEnum::StopAuthorized
            );
            assert_eq!(
                trigger_reason_for(TransactionEventKind::Ended, &base),
                TriggerReasonEnum::StopAuthorized
            );
        }
    }
}

/// The OCPP 2.0.1 projection - a strict subset of 2.1's `TransactionEventRequest`/`Transaction`
/// wire shapes (2.1 added `costDetails`/`evseSleep`/`preconditioningStatus` and a richer
/// `Transaction` with `operationMode`/`tariffId`/`transactionLimit` - this crate always sends
/// `None` for all of them, so nothing is actually lost), and every wire enum value this module
/// actually uses (`TransactionEventEnum`, `ChargingStateEnum`, the four `ReasonEnum`/seven
/// `TriggerReasonEnum` values referenced below, `MeasurandEnum`, `ReadingContextEnum::
/// SamplePeriodic`) exists identically in both versions. Close to a copy of the 2.1 module, just
/// targeting `OCPP2_0_1Client`.
#[cfg(feature = "ocpp_2_0_1")]
pub(crate) mod ocpp_2_0_1 {
    pub use with_clock::Ocpp2_0_1TransactionNotifier;

    use crate::meter_values::MeasurandSet;

    use crate::state::{
        MeterSample, StopReason, Transaction, TransactionChargingState, TransactionEventKind,
        TransactionLimitKind, TransactionUpdateReason,
    };
    use crate::wire::v201::common::{
        ChargingStateEnum, MeasurandEnum, MeterValue, ReadingContextEnum, ReasonEnum, SampledValue,
        TransactionEventEnum, TriggerReasonEnum,
    };
    use alloc::vec;
    use alloc::vec::Vec;
    use chrono::{DateTime, Utc};

    pub(super) fn map_event_type(kind: TransactionEventKind) -> TransactionEventEnum {
        match kind {
            TransactionEventKind::Started => TransactionEventEnum::Started,
            TransactionEventKind::Updated(_) => TransactionEventEnum::Updated,
            TransactionEventKind::Ended => TransactionEventEnum::Ended,
        }
    }

    pub(super) fn map_charging_state(state: TransactionChargingState) -> ChargingStateEnum {
        match state {
            TransactionChargingState::EvConnected => ChargingStateEnum::EVConnected,
            TransactionChargingState::Charging => ChargingStateEnum::Charging,
            TransactionChargingState::SuspendedEV => ChargingStateEnum::SuspendedEV,
            TransactionChargingState::SuspendedEVSE => ChargingStateEnum::SuspendedEVSE,
            TransactionChargingState::Idle => ChargingStateEnum::Idle,
        }
    }

    pub(super) fn map_stop_reason(reason: StopReason) -> ReasonEnum {
        match reason {
            StopReason::Local => ReasonEnum::Local,
            StopReason::Remote => ReasonEnum::Remote,
            StopReason::EVDisconnected => ReasonEnum::EVDisconnected,
            StopReason::EmergencyStop => ReasonEnum::EmergencyStop,
            StopReason::Reset => ReasonEnum::ImmediateReset,
            StopReason::PowerLoss => ReasonEnum::PowerLoss,
            StopReason::DeAuthorized => ReasonEnum::DeAuthorized,
        }
    }

    /// Mirrors [`super::ocpp_2_1::trigger_reason_for`].
    pub(super) fn trigger_reason_for(
        kind: TransactionEventKind,
        transaction: &Transaction,
    ) -> TriggerReasonEnum {
        match kind {
            // F01.FR.19/F02.FR.21 (CV6): a transaction the CSMS asked for reports what actually
            // triggered it. A `remoteStartId` is only ever set by
            // `handle_request_start_transaction`, so its presence *is* "this was a RemoteStart".
            TransactionEventKind::Started if transaction.remote_start_id.is_some() => {
                TriggerReasonEnum::RemoteStart
            }
            TransactionEventKind::Started => TriggerReasonEnum::Authorized,
            TransactionEventKind::Updated(TransactionUpdateReason::ChargingStateChanged) => {
                TriggerReasonEnum::ChargingStateChanged
            }
            TransactionEventKind::Updated(TransactionUpdateReason::MeterValuePeriodic) => {
                TriggerReasonEnum::MeterValuePeriodic
            }
            // K11.FR.04/K13.FR.03 (CV18): an external control system moved the rate.
            TransactionEventKind::Updated(TransactionUpdateReason::ChargingRateChanged) => {
                TriggerReasonEnum::ChargingRateChanged
            }
            // E16 is 2.1-only, and 2.0.1's `TriggerReasonEnum` has neither `LimitSet` nor
            // `CostLimitReached`/`SoCLimitReached`. `EnergyLimitReached` and `TimeLimitReached`
            // it does have, so those two are exact; the other two report the charging-state
            // change that accompanies them, which is true (the connector really did move to
            // `SuspendedEVSE`) even though it does not say why. A `LimitSet` confirmation has no
            // honest 2.0.1 spelling at all and is not sent - see `notify_transaction_event`.
            TransactionEventKind::Updated(TransactionUpdateReason::LimitSet) => {
                TriggerReasonEnum::ChargingStateChanged
            }
            TransactionEventKind::Updated(TransactionUpdateReason::LimitReached(kind)) => {
                match kind {
                    TransactionLimitKind::Energy => TriggerReasonEnum::EnergyLimitReached,
                    TransactionLimitKind::Time => TriggerReasonEnum::TimeLimitReached,
                    TransactionLimitKind::Cost | TransactionLimitKind::Soc => {
                        TriggerReasonEnum::ChargingStateChanged
                    }
                }
            }
            TransactionEventKind::Ended => match transaction.stop_reason {
                Some(StopReason::EmergencyStop) => TriggerReasonEnum::AbnormalCondition,
                Some(StopReason::Remote) => TriggerReasonEnum::RemoteStop,
                Some(StopReason::EVDisconnected) => TriggerReasonEnum::EVDeparted,
                Some(StopReason::Reset) => TriggerReasonEnum::ResetCommand,
                // No `PowerLoss` trigger reason exists in 2.x - a transaction closed out from
                // its persisted record on the next boot is exactly the "something went wrong
                // outside the normal flow" case `AbnormalCondition` covers. The precise cause
                // still reaches the CSMS in `stoppedReason` (`ReasonEnum::PowerLoss`).
                Some(StopReason::PowerLoss) => TriggerReasonEnum::AbnormalCondition,
                // E05: the CSMS refused the identifier, so that is what triggered the end.
                Some(StopReason::DeAuthorized) => TriggerReasonEnum::Deauthorized,
                Some(StopReason::Local) | None => TriggerReasonEnum::StopAuthorized,
            },
        }
    }

    /// Mirrors [`super::ocpp_2_1::sampled_values`], including its CV2.6 measurand filter - see
    /// there for why what the meter reported and what the CSMS selected are separate conditions.
    fn sampled_values(sample: MeterSample, measurands: MeasurandSet) -> Vec<SampledValue> {
        let mut values = Vec::new();
        if measurands.energy {
            values.push(sampled_value(
                sample.energy_wh as f64,
                MeasurandEnum::EnergyActiveImportRegister,
            ));
        }
        if let Some(power_w) = sample.power_w.filter(|_| measurands.power) {
            values.push(sampled_value(
                power_w as f64,
                MeasurandEnum::PowerActiveImport,
            ));
        }
        if let Some(current_ma) = sample.current_ma.filter(|_| measurands.current) {
            values.push(sampled_value(
                current_ma as f64 / 1_000.0,
                MeasurandEnum::CurrentImport,
            ));
        }
        if let Some(voltage_v) = sample.voltage_v.filter(|_| measurands.voltage) {
            values.push(sampled_value(voltage_v as f64, MeasurandEnum::Voltage));
        }
        if let Some(soc_percent) = sample.soc_percent.filter(|_| measurands.soc) {
            values.push(sampled_value(soc_percent as f64, MeasurandEnum::SoC));
        }
        values
    }

    fn sampled_value(value: f64, measurand: MeasurandEnum) -> SampledValue {
        SampledValue {
            value,
            measurand: Some(measurand),
            context: Some(ReadingContextEnum::SamplePeriodic),
            phase: None,
            location: None,
            signed_meter_value: None,
            unit_of_measure: None,
            custom_data: None,
        }
    }

    /// Mirrors [`super::ocpp_2_1::build_meter_values`], including its CV2.6 rule that a measurand
    /// set selecting nothing produces no `MeterValue` at all.
    pub(crate) fn build_meter_values(
        sample: Option<MeterSample>,
        timestamp: DateTime<Utc>,
        measurands: MeasurandSet,
    ) -> Vec<MeterValue> {
        let Some(sample) = sample else {
            return Vec::new();
        };
        let sampled_value = sampled_values(sample, measurands);
        if sampled_value.is_empty() {
            return Vec::new();
        }
        vec![MeterValue {
            timestamp: timestamp.into(),
            sampled_value,
            custom_data: None,
        }]
    }

    // `TransactionEventRequest` needs a timestamp, produced from a caller-supplied `Clock` - see
    // `crate::clock::is_synchronized`'s docs for the policy on an unsynchronized reading (send it
    // as-is, warn, never fabricate or drop it). Mirrors `super::ocpp_2_1::with_clock`.
    mod with_clock {
        use super::{
            build_meter_values, map_charging_state, map_event_type, map_stop_reason,
            trigger_reason_for,
        };
        use crate::actor::ChargePointActor;
        use crate::clock::{Clock, is_synchronized};
        use crate::meter_values::{MeasurandSet, transaction_event_measurands};
        use crate::state::{Transaction, TransactionEventKind};
        use crate::transactions::{TransactionEventOutcome, TransactionNotifier};
        use crate::wire::v201::TransactionEventRequest;
        use crate::wire::v201::common::{
            AuthorizationStatusEnum, EVSE, Transaction as WireTransaction,
        };
        use alloc::boxed::Box;
        use ocpp_client::ClientError;
        use ocpp_client::ocpp_2_0_1::{OCPP2_0_1Client, OCPP2_0_1Error};

        /// Mirrors `super::ocpp_2_1::with_clock::build_transaction_event_request` - pure, so a
        /// fixed [`Clock`] fake can assert the exact timestamp reaching the wire request, and
        /// carrying only the measurands `measurands` selects (CV2.6).
        fn build_transaction_event_request<C: Clock>(
            clock: &C,
            evse_id: usize,
            connector_id: usize,
            kind: TransactionEventKind,
            transaction: Transaction,
            offline: bool,
            measurands: MeasurandSet,
        ) -> TransactionEventRequest {
            let now = clock.now();
            if !is_synchronized(&now) {
                tracing::warn!(
                    timestamp = %now,
                    "TransactionEvent timestamp sourced from an unsynchronized clock"
                );
            }
            let meter_value = build_meter_values(transaction.last_meter_sample, now, measurands);
            TransactionEventRequest {
                custom_data: None,
                event_type: map_event_type(kind),
                meter_value: if meter_value.is_empty() {
                    None
                } else {
                    Some(meter_value)
                },
                timestamp: now.into(),
                trigger_reason: trigger_reason_for(kind, &transaction),
                seq_no: transaction.seq_no as i64,
                transaction_info: WireTransaction {
                    // The transaction id is an internal `u64` formatted as decimal, always
                    // well within the wire field's 36-byte bound.
                    transaction_id: heapless::String::try_from(transaction.id.0)
                        .expect("u64 transaction id always fits in a 36-byte wire field"),
                    charging_state: Some(map_charging_state(transaction.charging_state)),
                    time_spent_charging: None,
                    stopped_reason: transaction.stop_reason.map(map_stop_reason),
                    // CV6: F01.FR.25/F02.FR.01 - the CSMS correlates the transaction it now sees
                    // with the request it made.
                    remote_start_id: transaction.remote_start_id,
                    custom_data: None,
                },
                // E12 (CV6.1): only stated when it is true. OCPP defaults the field to `false`,
                // so an event that went out on its first attempt says nothing rather than
                // asserting a negative.
                offline: offline.then_some(true),
                number_of_phases_used: None,
                cable_max_current: None,
                // CV6: F02.FR.06/H03 - the reservation this session consumed, so the CSMS can
                // close it out against the transaction that used it.
                reservation_id: transaction.reservation_id,
                evse: Some(EVSE {
                    id: evse_id as i64,
                    connector_id: Some(connector_id as i64),
                    custom_data: None,
                }),
                id_token: None,
            }
        }

        #[allow(clippy::too_many_arguments)]
        async fn send_transaction_event<C: Clock>(
            client: &OCPP2_0_1Client,
            clock: &C,
            evse_id: usize,
            connector_id: usize,
            kind: TransactionEventKind,
            transaction: Transaction,
            offline: bool,
            measurands: MeasurandSet,
        ) -> Result<TransactionEventOutcome, ClientError<OCPP2_0_1Error>> {
            let request = build_transaction_event_request(
                clock,
                evse_id,
                connector_id,
                kind,
                transaction,
                offline,
                measurands,
            );
            let response = client.send_transaction_event(request).await?;
            Ok(read_outcome(&response))
        }

        /// Reads what a `TransactionEventResponse` says about the identifier - the 2.0.1 twin of
        /// `super::super::ocpp_2_1::with_clock::read_outcome`, including its ten-to-two collapse.
        fn read_outcome(
            response: &crate::wire::v201::TransactionEventResponse,
        ) -> TransactionEventOutcome {
            let Some(info) = response.id_token_info.as_ref() else {
                return TransactionEventOutcome::acknowledged();
            };
            TransactionEventOutcome::with_id_token_status(
                if info.status == AuthorizationStatusEnum::Accepted {
                    crate::state::AuthorizationStatus::Accepted
                } else {
                    crate::state::AuthorizationStatus::Rejected
                },
            )
        }

        /// Wraps an `OCPP2_0_1Client` with a caller-supplied [`Clock`] for the TransactionEvent
        /// timestamp and the [`ChargePointActor`] holding the measurand configuration - mirrors
        /// `super::ocpp_2_1::with_clock::Ocpp2_1TransactionNotifier`, including its CV2.6 rule
        /// that the device model is read per event rather than at construction, and its `Clone`/
        /// `ReconnectHandler` so it is registrable through `ChargePointBuilder`.
        #[derive(Clone)]
        pub struct Ocpp2_0_1TransactionNotifier<C> {
            client: OCPP2_0_1Client,
            clock: C,
            actor: ChargePointActor,
        }

        /// Delegates to the wrapped client - mirrors 2.1's.
        #[async_trait::async_trait]
        impl<C: Clock + Send + Sync> crate::connection::ReconnectHandler
            for Ocpp2_0_1TransactionNotifier<C>
        {
            async fn register_reconnect_handler<F, FF>(&self, callback: F)
            where
                F: FnMut() -> FF + Send + Sync + 'static,
                FF: core::future::Future<Output = ()> + Send + 'static,
            {
                crate::connection::ReconnectHandler::register_reconnect_handler(
                    &self.client,
                    callback,
                )
                .await;
            }
        }

        impl<C: Clock> Ocpp2_0_1TransactionNotifier<C> {
            /// Wraps `client`, sourcing every TransactionEvent timestamp from `clock` and every
            /// event's measurand selection from `actor`'s device model.
            pub fn with_clock(client: OCPP2_0_1Client, clock: C, actor: ChargePointActor) -> Self {
                Self {
                    client,
                    clock,
                    actor,
                }
            }
        }

        #[cfg(feature = "std")]
        impl Ocpp2_0_1TransactionNotifier<crate::clock::SystemClock> {
            /// Wraps `client`, sourcing every TransactionEvent timestamp from
            /// [`crate::clock::SystemClock`] and its measurand selection from `actor`.
            pub fn new(client: OCPP2_0_1Client, actor: ChargePointActor) -> Self {
                Self::with_clock(client, crate::clock::SystemClock, actor)
            }
        }

        #[async_trait::async_trait]
        impl<C: Clock + Send + Sync> TransactionNotifier for Ocpp2_0_1TransactionNotifier<C> {
            type Error = ClientError<OCPP2_0_1Error>;

            async fn notify_transaction_event(
                &self,
                evse_id: usize,
                connector_id: usize,
                kind: TransactionEventKind,
                transaction: Transaction,
                offline: bool,
            ) -> Result<TransactionEventOutcome, Self::Error> {
                send_transaction_event(
                    &self.client,
                    &self.clock,
                    evse_id,
                    connector_id,
                    kind,
                    transaction,
                    offline,
                    transaction_event_measurands(&self.actor, kind),
                )
                .await
            }
        }

        /// The `std` convenience: `OCPP2_0_1Client` itself implements `TransactionNotifier`
        /// directly (sourcing its timestamp from [`crate::clock::SystemClock`]) so existing
        /// callers that pass a bare client need no source change. Like 2.1's, it has no
        /// [`ChargePointActor`] to read a measurand list from and so reports every measurand the
        /// sample carries - see `super::super::ocpp_2_1::with_clock`'s note.
        #[cfg(feature = "std")]
        #[async_trait::async_trait]
        impl TransactionNotifier for OCPP2_0_1Client {
            type Error = ClientError<OCPP2_0_1Error>;

            async fn notify_transaction_event(
                &self,
                evse_id: usize,
                connector_id: usize,
                kind: TransactionEventKind,
                transaction: Transaction,
                offline: bool,
            ) -> Result<TransactionEventOutcome, Self::Error> {
                send_transaction_event(
                    self,
                    &crate::clock::SystemClock,
                    evse_id,
                    connector_id,
                    kind,
                    transaction,
                    offline,
                    MeasurandSet::ALL,
                )
                .await
            }
        }

        #[cfg(test)]
        mod tests {
            use super::*;
            use crate::state::{TransactionChargingState, TransactionId};
            use chrono::{DateTime, Utc};

            /// A [`Clock`] that always reads a fixed, caller-chosen instant.
            struct FixedClock(DateTime<Utc>);

            impl Clock for FixedClock {
                fn now(&self) -> DateTime<Utc> {
                    self.0
                }
            }

            fn transaction() -> Transaction {
                Transaction {
                    id: TransactionId(7),
                    id_token: None,
                    charging_state: TransactionChargingState::Charging,
                    stop_reason: None,
                    seq_no: 0,
                    last_meter_sample: None,
                    priority_charging: false,
                    remote_start_id: None,
                    reservation_id: None,
                    stop_at_energy_wh: None,
                    limit: None,
                    csms_limit: None,
                    limit_reached: None,
                    energy_start_wh: None,
                }
            }

            #[test]
            fn a_fixed_clocks_reading_reaches_the_wire_request_timestamp() {
                let fixed = DateTime::parse_from_rfc3339("2026-03-04T05:06:07Z")
                    .unwrap()
                    .with_timezone(&Utc);
                let clock = FixedClock(fixed);

                let request = build_transaction_event_request(
                    &clock,
                    0,
                    0,
                    TransactionEventKind::Started,
                    transaction(),
                    false,
                    MeasurandSet::ALL,
                );

                assert_eq!(request.timestamp, crate::wire::OcppTimestamp::from(fixed));
            }

            #[test]
            fn an_unsynchronized_clocks_reading_is_still_sent_not_substituted_or_dropped() {
                let unset_rtc = FixedClock(DateTime::<Utc>::from_timestamp(0, 0).unwrap());
                assert!(!is_synchronized(&unset_rtc.now()));

                let request = build_transaction_event_request(
                    &unset_rtc,
                    0,
                    0,
                    TransactionEventKind::Started,
                    transaction(),
                    false,
                    MeasurandSet::ALL,
                );

                assert_eq!(
                    request.timestamp,
                    crate::wire::OcppTimestamp::from(unset_rtc.0)
                );
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::state::MeterSample;

        #[test]
        fn a_sample_with_only_energy_reports_a_single_sampled_value() {
            let sample = MeterSample {
                energy_wh: 1_500,
                ..Default::default()
            };
            let timestamp = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc);

            let values = build_meter_values(Some(sample), timestamp, MeasurandSet::ALL);

            assert_eq!(values.len(), 1);
            assert_eq!(values[0].sampled_value.len(), 1);
            assert_eq!(values[0].sampled_value[0].value, 1_500.0);
            assert_eq!(
                values[0].sampled_value[0].measurand,
                Some(MeasurandEnum::EnergyActiveImportRegister)
            );
        }

        #[test]
        fn no_sample_reports_no_meter_values() {
            let timestamp = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc);

            assert_eq!(
                build_meter_values(None, timestamp, MeasurandSet::ALL),
                Vec::new()
            );
        }

        #[test]
        fn every_kind_maps_to_the_matching_wire_event_type() {
            assert_eq!(
                map_event_type(TransactionEventKind::Started),
                TransactionEventEnum::Started
            );
            assert_eq!(
                map_event_type(TransactionEventKind::Updated(
                    TransactionUpdateReason::ChargingStateChanged
                )),
                TransactionEventEnum::Updated
            );
            assert_eq!(
                map_event_type(TransactionEventKind::Ended),
                TransactionEventEnum::Ended
            );
        }

        #[test]
        fn every_charging_state_maps_to_the_matching_wire_state() {
            assert_eq!(
                map_charging_state(TransactionChargingState::EvConnected),
                ChargingStateEnum::EVConnected
            );
            assert_eq!(
                map_charging_state(TransactionChargingState::Charging),
                ChargingStateEnum::Charging
            );
            assert_eq!(
                map_charging_state(TransactionChargingState::SuspendedEV),
                ChargingStateEnum::SuspendedEV
            );
            assert_eq!(
                map_charging_state(TransactionChargingState::SuspendedEVSE),
                ChargingStateEnum::SuspendedEVSE
            );
            assert_eq!(
                map_charging_state(TransactionChargingState::Idle),
                ChargingStateEnum::Idle
            );
        }

        #[test]
        fn started_and_updated_use_fixed_trigger_reasons() {
            let transaction = Transaction {
                id: crate::state::TransactionId(0),
                id_token: None,
                charging_state: TransactionChargingState::Charging,
                stop_reason: None,
                seq_no: 0,
                last_meter_sample: None,
                priority_charging: false,
                remote_start_id: None,
                reservation_id: None,
                stop_at_energy_wh: None,
                limit: None,
                csms_limit: None,
                limit_reached: None,
                energy_start_wh: None,
            };

            assert_eq!(
                trigger_reason_for(TransactionEventKind::Started, &transaction),
                TriggerReasonEnum::Authorized
            );
            assert_eq!(
                trigger_reason_for(
                    TransactionEventKind::Updated(TransactionUpdateReason::ChargingStateChanged),
                    &transaction
                ),
                TriggerReasonEnum::ChargingStateChanged
            );
            assert_eq!(
                trigger_reason_for(
                    TransactionEventKind::Updated(TransactionUpdateReason::MeterValuePeriodic),
                    &transaction
                ),
                TriggerReasonEnum::MeterValuePeriodic
            );
        }

        #[test]
        fn ended_derives_its_trigger_reason_from_the_stop_reason() {
            let base = Transaction {
                id: crate::state::TransactionId(0),
                id_token: None,
                charging_state: TransactionChargingState::EvConnected,
                stop_reason: None,
                seq_no: 2,
                last_meter_sample: None,
                priority_charging: false,
                remote_start_id: None,
                reservation_id: None,
                stop_at_energy_wh: None,
                limit: None,
                csms_limit: None,
                limit_reached: None,
                energy_start_wh: None,
            };

            assert_eq!(
                trigger_reason_for(
                    TransactionEventKind::Ended,
                    &Transaction {
                        stop_reason: Some(StopReason::EmergencyStop),
                        ..base.clone()
                    }
                ),
                TriggerReasonEnum::AbnormalCondition
            );
            assert_eq!(
                trigger_reason_for(
                    TransactionEventKind::Ended,
                    &Transaction {
                        stop_reason: Some(StopReason::Remote),
                        ..base.clone()
                    }
                ),
                TriggerReasonEnum::RemoteStop
            );
            assert_eq!(
                trigger_reason_for(
                    TransactionEventKind::Ended,
                    &Transaction {
                        stop_reason: Some(StopReason::EVDisconnected),
                        ..base.clone()
                    }
                ),
                TriggerReasonEnum::EVDeparted
            );
            assert_eq!(
                trigger_reason_for(
                    TransactionEventKind::Ended,
                    &Transaction {
                        stop_reason: Some(StopReason::Local),
                        ..base.clone()
                    }
                ),
                TriggerReasonEnum::StopAuthorized
            );
            assert_eq!(
                trigger_reason_for(TransactionEventKind::Ended, &base),
                TriggerReasonEnum::StopAuthorized
            );
        }
    }
}

/// The OCPP 1.6J projection of `TransactionNotifier` - a harder case than any adapter so far
/// (see `docs/ROADMAP.md` §0): 1.6J splits the unified `TransactionEvent` stream into three
/// separate calls (`StartTransaction`/`MeterValues`/`StopTransaction`), and - unlike every other
/// identifier in this crate - **the CSMS assigns the transaction id**, not the charge point:
/// `StartTransaction.conf` returns a `transactionId` that every later `MeterValues`/
/// `StopTransaction` for that session must use instead of this crate's own [`TransactionId`].
/// [`Ocpp1_6TransactionNotifier`] wraps `OCPP1_6Client` with the connector topology needed for
/// `StartTransaction`/`MeterValues`'s flat `connectorId` (see [`crate::topology`], shared with
/// [`crate::availability::Ocpp1_6StatusNotifier`]) and a small cache mapping this crate's
/// `TransactionId` to the CSMS-assigned one, populated when `StartTransaction.conf` returns and
/// consulted (then, on `Ended`, removed) for every later call on that transaction.
///
/// Two real gaps, both inherited from the internal model rather than introduced here: 1.6J's
/// `StartTransaction.req` needs `idTag` and `meterStart` - `idTag` now has a real source
/// ([`crate::state::Transaction::id_token`]), but `meterStart` doesn't; this crate's
/// [`crate::state::MeterSample`] is only ever recorded once charging is already under way (see
/// `docs/ROADMAP.md` §10), so there's no reading captured *at* `Started` to report - this falls
/// back to `0` (documented, not silently wrong). `idTag` mapping itself (including truncation
/// for identifiers exceeding 1.6J's 20-byte bound) lives in [`crate::id_tag`], shared with every
/// other 1.6J adapter that sends an identifier on the wire.
#[cfg(feature = "ocpp_1_6")]
pub(crate) mod ocpp_1_6 {
    use crate::state::{MeterSample, StopReason, TransactionId};
    use crate::wire::v16::common::{Measurand, MeterValueItem, Reason, SampledValueItem};
    use alloc::collections::BTreeMap;
    use alloc::string::ToString;
    use alloc::vec;
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
    use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
    use ocpp_client::ocpp_1_6::OCPP1_6Client;

    pub(super) fn map_stop_reason(reason: StopReason) -> Reason {
        match reason {
            StopReason::Local => Reason::Local,
            StopReason::Remote => Reason::Remote,
            StopReason::EVDisconnected => Reason::EVDisconnected,
            StopReason::EmergencyStop => Reason::EmergencyStop,
            // A CSMS-initiated `Reset` (`ResetKind::Immediate`) that interrupted this
            // transaction is, on the wire, indistinguishable from any other "the whole charge
            // point restarted" stop - 1.6J's `Reason` enum's closest match is `HardReset`
            // (mirroring the `Hard`/`Immediate` pairing `crate::reset::ocpp_1_6` uses).
            StopReason::Reset => Reason::HardReset,
            StopReason::PowerLoss => Reason::PowerLoss,
            // 1.6J spells it the same way.
            StopReason::DeAuthorized => Reason::DeAuthorized,
        }
    }

    /// Builds one `sampledValue` per measurand present in `sample`, mirroring
    /// [`super::ocpp_2_1::sampled_values`] but for 1.6J's string-valued `SampledValueItem`.
    /// Builds the 1.6J `meterValue` body for `sample`, shared with [`crate::meter_values`]'
    /// standalone `MeterValues` adapter for the same reason the 2.x builders are.
    pub(crate) fn build_meter_values(
        sample: MeterSample,
        timestamp: chrono::DateTime<chrono::Utc>,
    ) -> Vec<MeterValueItem> {
        vec![MeterValueItem {
            timestamp: timestamp.into(),
            sampled_value: sampled_values(sample),
        }]
    }

    fn sampled_values(sample: MeterSample) -> Vec<SampledValueItem> {
        let mut values = vec![sampled_value(
            sample.energy_wh.to_string(),
            Measurand::EnergyActiveImportRegister,
        )];
        if let Some(power_w) = sample.power_w {
            values.push(sampled_value(
                power_w.to_string(),
                Measurand::PowerActiveImport,
            ));
        }
        if let Some(current_ma) = sample.current_ma {
            values.push(sampled_value(
                (current_ma as f64 / 1_000.0).to_string(),
                Measurand::CurrentImport,
            ));
        }
        if let Some(voltage_v) = sample.voltage_v {
            values.push(sampled_value(voltage_v.to_string(), Measurand::Voltage));
        }
        if let Some(soc_percent) = sample.soc_percent {
            values.push(sampled_value(soc_percent.to_string(), Measurand::SoC));
        }
        values
    }

    fn sampled_value(value: alloc::string::String, measurand: Measurand) -> SampledValueItem {
        SampledValueItem {
            context: None,
            format: None,
            location: None,
            measurand: Some(measurand),
            phase: None,
            unit: None,
            value,
        }
    }

    /// Wraps an `OCPP1_6Client` with the charge point's connector topology, a cache of
    /// CSMS-assigned transaction ids, and a caller-supplied [`crate::clock::Clock`] for the
    /// `StartTransaction`/`MeterValues`/`StopTransaction` timestamp - see this module's top-level
    /// docs. `C` defaults to nothing in particular; embedded/no_std callers construct with
    /// [`Self::with_clock`] and an RTC-backed `Clock`, while `std` callers keep using
    /// [`Self::new`] (sourcing the timestamp from [`crate::clock::SystemClock`]) unchanged.
    pub struct Ocpp1_6TransactionNotifier<C> {
        client: OCPP1_6Client,
        connector_counts: Vec<usize>,
        csms_transaction_ids:
            BlockingMutex<CriticalSectionRawMutex, RefCell<BTreeMap<TransactionId, i64>>>,
        clock: C,
    }

    impl<C: crate::clock::Clock> Ocpp1_6TransactionNotifier<C> {
        /// Wraps `client`, capturing `connector_counts` (each EVSE's connector count, in
        /// `evse_id` order) for translating connector addresses, starting with no cached
        /// CSMS-assigned transaction ids, and sourcing every wire timestamp from `clock`.
        pub fn with_clock(
            client: OCPP1_6Client,
            connector_counts: impl IntoIterator<Item = usize>,
            clock: C,
        ) -> Self {
            Self {
                client,
                connector_counts: connector_counts.into_iter().collect(),
                csms_transaction_ids: BlockingMutex::new(RefCell::new(BTreeMap::new())),
                clock,
            }
        }
    }

    #[cfg(feature = "std")]
    impl Ocpp1_6TransactionNotifier<crate::clock::SystemClock> {
        /// Wraps `client`, capturing `connector_counts` (each EVSE's connector count, in
        /// `evse_id` order) for translating connector addresses, starting with no cached
        /// CSMS-assigned transaction ids, and sourcing every wire timestamp from
        /// [`crate::clock::SystemClock`].
        pub fn new(
            client: OCPP1_6Client,
            connector_counts: impl IntoIterator<Item = usize>,
        ) -> Self {
            Self::with_clock(client, connector_counts, crate::clock::SystemClock)
        }
    }

    // `StartTransactionRequest`/`StopTransactionRequest`/`MeterValuesRequest` need a timestamp,
    // produced from `self.clock` - see `crate::clock::is_synchronized`'s docs for the policy on
    // an unsynchronized reading (send it as-is, warn, never fabricate or drop it).
    mod with_clock {
        use super::{Ocpp1_6TransactionNotifier, map_stop_reason, sampled_values};
        use crate::clock::{Clock, is_synchronized};
        use crate::id_tag::map_id_tag;
        use crate::state::{Transaction, TransactionEventKind};
        use crate::topology::flatten_ocpp_1_6_connector_id;
        use crate::transactions::{TransactionEventOutcome, TransactionNotifier};
        use crate::wire::v16::common::MeterValueItem;
        use crate::wire::v16::{
            MeterValuesRequest, StartTransactionRequest, StopTransactionRequest,
        };
        use alloc::boxed::Box;
        use alloc::vec::Vec;
        use ocpp_client::ClientError;
        use ocpp_client::ocpp_1_6::OCPP1_6Error;

        /// Delegates to the wrapped client: a reconnect is a property of the *connection*, and
        /// this wrapper adds topology and an id cache on top of one rather than owning a
        /// different one. Without this a 1.6J charge point could not use the offline queues at
        /// all, since those flush on reconnect.
        #[async_trait::async_trait]
        impl<C: Clock + Send + Sync> crate::connection::ReconnectHandler for Ocpp1_6TransactionNotifier<C> {
            async fn register_reconnect_handler<F, FF>(&self, callback: F)
            where
                F: FnMut() -> FF + Send + Sync + 'static,
                FF: core::future::Future<Output = ()> + Send + 'static,
            {
                crate::connection::ReconnectHandler::register_reconnect_handler(
                    &self.client,
                    callback,
                )
                .await;
            }
        }

        #[async_trait::async_trait]
        impl<C: Clock + Send + Sync> TransactionNotifier for Ocpp1_6TransactionNotifier<C> {
            type Error = ClientError<OCPP1_6Error>;

            /// `offline` is ignored: 1.6J's `StartTransaction`/`MeterValues`/`StopTransaction`
            /// have no field for it, and inventing one would be a wire extension no CSMS knows
            /// how to read. The queueing behaviour it describes still happens - only the
            /// after-the-fact disclosure is 2.x-only.
            async fn notify_transaction_event(
                &self,
                evse_id: usize,
                connector_id: usize,
                kind: TransactionEventKind,
                transaction: Transaction,
                _offline: bool,
            ) -> Result<TransactionEventOutcome, Self::Error> {
                let Some(connector_id) =
                    flatten_ocpp_1_6_connector_id(&self.connector_counts, evse_id, connector_id)
                else {
                    // Not addressable under 1.6J's flat numbering - see
                    // `Ocpp1_6StatusNotifier::notify_status`'s identical handling.
                    return Ok(TransactionEventOutcome::acknowledged());
                };
                let now = self.clock.now();
                if !is_synchronized(&now) {
                    tracing::warn!(
                        timestamp = %now,
                        ?kind,
                        "1.6J transaction timestamp sourced from an unsynchronized clock"
                    );
                }
                let meter_reading = transaction
                    .last_meter_sample
                    .map(|s| s.energy_wh)
                    .unwrap_or(0);

                match kind {
                    TransactionEventKind::Started => {
                        let response = self
                            .client
                            .send_start_transaction(StartTransactionRequest {
                                connector_id,
                                id_tag: map_id_tag(transaction.id_token.as_ref()),
                                meter_start: meter_reading,
                                reservation_id: None,
                                timestamp: now.into(),
                            })
                            .await?;
                        self.csms_transaction_ids.lock(|cache| {
                            cache
                                .borrow_mut()
                                .insert(transaction.id, response.transaction_id);
                        });
                        // 1.6J's `idTagInfo` is where a CSMS refuses the tag a session is running
                        // under - the same E05 decision 2.x carries on `TransactionEventResponse`.
                        return Ok(TransactionEventOutcome::with_id_token_status(
                            crate::authorization::ocpp_1_6::map_status(response.id_tag_info.status),
                        ));
                    }
                    TransactionEventKind::Updated(_) => {
                        let Some(sample) = transaction.last_meter_sample else {
                            // Nothing sampled yet - a `MeterValues` with no `sampledValue` at all
                            // isn't meaningful, unlike 2.1's `TransactionEvent`, which reports
                            // regardless (see `super::ocpp_2_1`'s `Updated` handling).
                            return Ok(TransactionEventOutcome::acknowledged());
                        };
                        let csms_transaction_id = self
                            .csms_transaction_ids
                            .lock(|cache| cache.borrow().get(&transaction.id).copied());
                        self.client
                            .send_meter_values(MeterValuesRequest {
                                connector_id,
                                meter_value: Vec::from([MeterValueItem {
                                    sampled_value: sampled_values(sample),
                                    timestamp: now.into(),
                                }]),
                                transaction_id: csms_transaction_id,
                            })
                            .await?;
                    }
                    TransactionEventKind::Ended => {
                        let csms_transaction_id = self
                            .csms_transaction_ids
                            .lock(|cache| cache.borrow_mut().remove(&transaction.id));
                        let Some(csms_transaction_id) = csms_transaction_id else {
                            // No `StartTransaction` on record for this transaction - shouldn't
                            // happen (`Started` always precedes `Ended` for the same
                            // transaction), but there's nothing to stop without the CSMS's id.
                            return Ok(TransactionEventOutcome::acknowledged());
                        };
                        let response = self
                            .client
                            .send_stop_transaction(StopTransactionRequest {
                                id_tag: Some(map_id_tag(transaction.id_token.as_ref())),
                                meter_stop: meter_reading,
                                reason: transaction.stop_reason.map(map_stop_reason),
                                timestamp: now.into(),
                                transaction_data: None,
                                transaction_id: csms_transaction_id,
                            })
                            .await?;
                        // Optional on a `StopTransaction` response, and moot by then - the
                        // transaction is already over - but read rather than discarded so the
                        // three event kinds answer the same question the same way.
                        if let Some(info) = response.id_tag_info {
                            return Ok(TransactionEventOutcome::with_id_token_status(
                                crate::authorization::ocpp_1_6::map_status(info.status),
                            ));
                        }
                    }
                }
                Ok(TransactionEventOutcome::acknowledged())
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn every_stop_reason_maps_to_the_matching_wire_reason() {
            assert_eq!(map_stop_reason(StopReason::Local), Reason::Local);
            assert_eq!(map_stop_reason(StopReason::Remote), Reason::Remote);
            assert_eq!(
                map_stop_reason(StopReason::EVDisconnected),
                Reason::EVDisconnected
            );
            assert_eq!(
                map_stop_reason(StopReason::EmergencyStop),
                Reason::EmergencyStop
            );
        }

        #[test]
        fn a_sample_reports_every_present_measurand_as_a_string() {
            let sample = MeterSample {
                energy_wh: 1_500,
                power_w: Some(7_400),
                current_ma: Some(32_000),
                voltage_v: Some(230),
                soc_percent: Some(42),
            };

            let values = sampled_values(sample);

            assert_eq!(values.len(), 5);
            assert!(values.iter().any(|value| value.measurand
                == Some(Measurand::EnergyActiveImportRegister)
                && value.value == "1500"));
            assert!(
                values
                    .iter()
                    .any(|value| value.measurand == Some(Measurand::CurrentImport)
                        && value.value == "32")
            );
        }

        #[test]
        fn ocpp1_6_transaction_notifier_can_be_constructed_from_a_client_and_topology() {
            // Compile-level check that `Ocpp1_6TransactionNotifier::new` accepts the same
            // topology shape `Ocpp1_6StatusNotifier` does - actual wiring is exercised via
            // `with_clock`'s impl, only reachable with a live `OCPP1_6Client`.
            fn assert_ctor<
                F: Fn(
                    OCPP1_6Client,
                    [usize; 2],
                ) -> Ocpp1_6TransactionNotifier<crate::clock::SystemClock>,
            >(
                _: F,
            ) {
            }
            assert_ctor(Ocpp1_6TransactionNotifier::new);
        }

        /// A [`crate::clock::Clock`] that always reads a fixed, caller-chosen instant.
        struct FixedClock(chrono::DateTime<chrono::Utc>);

        impl crate::clock::Clock for FixedClock {
            fn now(&self) -> chrono::DateTime<chrono::Utc> {
                self.0
            }
        }

        #[test]
        fn ocpp1_6_transaction_notifier_can_be_constructed_with_a_caller_supplied_clock() {
            // Compile-level check that `with_clock` accepts a non-`SystemClock` fake, unlocking
            // the no_std path `crate::clock::SystemClock` (a `std`-only type) can't reach - the
            // actual timestamp behavior is exercised end-to-end via
            // `with_clock`'s tests (only reachable with a live `OCPP1_6Client`).
            fn assert_ctor<
                F: Fn(OCPP1_6Client, [usize; 2], FixedClock) -> Ocpp1_6TransactionNotifier<FixedClock>,
            >(
                _: F,
            ) {
            }
            assert_ctor(Ocpp1_6TransactionNotifier::with_clock);
        }
    }
}
