//! Tariff and cost functional block: the tariff store and per-transaction tariff assignment
//! (OCPP 2.1's `SetDefaultTariff`, `ChangeTransactionTariff`, `ClearTariffs`, `GetTariffs`). See
//! `docs/ROADMAP.md` §9 and `docs/PRODUCTION-ROADMAP.md` B7.1.
//!
//! **2.1-only.** 1.6J and 2.0.1 have no tariff messages at all - a spec boundary this crate
//! reflects by never registering these handlers under those versions, not a gap to close.
//!
//! This module **stores and reports** tariffs; it does not **compute** a cost from one. OCPP's
//! wire `Tariff` carries priced structure (energy/time/fixed-fee components, each with its own
//! conditions) this crate deliberately does not model - see [`crate::state::Tariff`]'s docs for
//! why - so there is no running cost derived from an assigned tariff here. `docs/ROADMAP.md` §9
//! already tracks that as open (outbound cost reporting needs a real pricing model); this block
//! does not change that. [`crate::cost::handle_cost_updated`] - the CSMS *telling* this charge
//! point a running cost - is unrelated and keeps working exactly as before.
//!
//! A default tariff (`SetDefaultTariff`) is scoped to one EVSE or the whole charge point and
//! lives in [`crate::state::TariffStore`]. A driver tariff (`ChangeTransactionTariff`) is scoped
//! to one running transaction and lives on
//! [`crate::state::EvseState::transaction_tariffs`] instead, cleared the moment that transaction
//! starts or ends - mirroring [`crate::state::EvseState::running_costs`] exactly, for the same
//! reason `CostUpdated` needed it: a new session on the same connector must never inherit a
//! previous driver's assignment.

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::actor::ChargePointActor;
use crate::state::{
    ChargePointEvent, ChargePointState, ConnectorEvent, EvseEvent, Tariff, TariffClearCriteria,
    TariffId, TariffScope, TariffSetRejection, TransactionId,
};

/// The outcome of a CSMS-initiated `SetDefaultTariff`, matching OCPP's `TariffSetStatusEnum`
/// minus `ConditionNotSupported` - this crate never models tariff *conditions* (see this module's
/// docs), so it can never detect one being unsupported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetDefaultTariffOutcome {
    /// The tariff was installed as the default for the requested scope.
    Accepted,
    /// A tariff with this id is already installed at a different scope - see
    /// [`crate::state::TariffSetRejection::DuplicateTariffId`].
    DuplicateTariffId,
    /// The store is already at [`crate::state::StateLimits::max_tariffs`] and this tariff isn't
    /// replacing an existing one.
    TooManyElements,
    /// The request could not be honoured - tariff and cost isn't available, or it addresses an
    /// EVSE this charge point doesn't have.
    Rejected,
}

/// Handles a CSMS-initiated `SetDefaultTariff` against `actor`. Like every handler in this crate,
/// the outcome is decided against the real state (via a trial install on a clone of the store)
/// before anything is dispatched, so the status the CSMS receives is what actually happened -
/// mirrors `crate::smart_charging::handle_set_charging_profile`.
pub async fn handle_set_default_tariff(
    actor: &ChargePointActor,
    scope: TariffScope,
    tariff: Tariff,
) -> SetDefaultTariffOutcome {
    let state = actor.state();
    if !crate::refusal::capability_present(&state.capabilities, "SetDefaultTariff") {
        return SetDefaultTariffOutcome::Rejected;
    }
    if let TariffScope::Evse(evse_id) = scope
        && evse_id >= state.evses.len()
    {
        return SetDefaultTariffOutcome::Rejected;
    }
    let mut trial = state.tariffs.clone();
    match trial.set_default(scope, tariff.clone()) {
        Ok(()) => {
            let _ = actor
                .send(ChargePointEvent::DefaultTariffSet {
                    scope,
                    tariff: Box::new(tariff),
                })
                .await;
            SetDefaultTariffOutcome::Accepted
        }
        Err(TariffSetRejection::DuplicateTariffId) => SetDefaultTariffOutcome::DuplicateTariffId,
        Err(TariffSetRejection::TooManyTariffs) => SetDefaultTariffOutcome::TooManyElements,
    }
}

/// The outcome of a CSMS-initiated `ChangeTransactionTariff`, matching a subset of OCPP's
/// `TariffChangeStatusEnum` - this crate never produces `TooManyElements`,
/// `ConditionNotSupported` or `NoCurrencyChange`, for the same reason [`SetDefaultTariffOutcome`]
/// never produces `ConditionNotSupported`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeTransactionTariffOutcome {
    /// The tariff was assigned to the named transaction.
    Accepted,
    /// No transaction with that id is currently running on this charge point - refused rather
    /// than silently stored, per `docs/PRODUCTION-ROADMAP.md` B7.1.
    TxNotFound,
    /// The request could not be honoured - tariff and cost isn't available.
    Rejected,
}

/// The connector (if any) whose active transaction is `transaction_id` - mirrors
/// `crate::cost::find_transaction` exactly (this crate's other transaction-id lookup, for the
/// same reason: `ChangeTransactionTariff`, like `CostUpdated`, addresses a transaction by id
/// rather than by connector).
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

/// Handles a CSMS-initiated `ChangeTransactionTariff` against `actor`: finds the connector whose
/// active transaction is `transaction_id` and assigns `tariff` to it as a driver tariff.
pub async fn handle_change_transaction_tariff(
    actor: &ChargePointActor,
    transaction_id: TransactionId,
    tariff: Tariff,
) -> ChangeTransactionTariffOutcome {
    let state = actor.state();
    if !crate::refusal::capability_present(&state.capabilities, "ChangeTransactionTariff") {
        return ChangeTransactionTariffOutcome::Rejected;
    }
    let Some((evse_id, connector_id)) = find_transaction(&state, transaction_id) else {
        return ChangeTransactionTariffOutcome::TxNotFound;
    };

    let _ = actor
        .send(ChargePointEvent::Evse {
            evse_id,
            event: EvseEvent::Connector {
                connector_id,
                event: ConnectorEvent::TariffAssigned(tariff),
            },
        })
        .await;
    ChangeTransactionTariffOutcome::Accepted
}

/// Which OCPP `TariffClearStatusEnum` one `ClearTariffs` result carries. This crate never
/// produces `Rejected` - nothing about a stored default tariff makes clearing it refusable once
/// the capability check has passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TariffClearStatus {
    /// The tariff was found and removed.
    Accepted,
    /// No tariff matched.
    NoTariff,
}

/// One tariff's result within a `ClearTariffs` response - OCPP reports per-tariff status rather
/// than failing the whole batch, mirroring `crate::device_model::handle_get_variables`/
/// `handle_set_variables`'s per-item resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TariffClearOutcome {
    /// The tariff this result is about. `None` only when nothing at all matched an unfiltered
    /// (no `tariffIds`) request - OCPP's `NoTariff`-with-no-id case.
    pub tariff_id: Option<TariffId>,
    /// Whether it was cleared.
    pub status: TariffClearStatus,
}

/// Handles a CSMS-initiated `ClearTariffs` against `actor`. Only ever clears
/// [`crate::state::TariffStore`] (default tariffs) - a driver tariff assigned via
/// `ChangeTransactionTariff` already ends with its transaction on its own (see this module's
/// docs), so there is nothing here for it to remove; a CSMS that wants to change one mid-session
/// sends another `ChangeTransactionTariff` instead.
///
/// Unlike every `CallResultStatus`-shaped handler in this crate, this one does **not** check
/// [`crate::refusal::capability_present`] itself - `ClearTariffsResponse` has no top-level status
/// field to refuse through, so that check belongs to the wire adapter, which answers a CALLERROR
/// instead (mirrors `crate::cost::ocpp_2_1::handle`).
pub async fn handle_clear_tariffs(
    actor: &ChargePointActor,
    scope: Option<TariffScope>,
    ids: Option<Vec<TariffId>>,
) -> Vec<TariffClearOutcome> {
    let state = actor.state();
    match &ids {
        Some(ids) if !ids.is_empty() => {
            let mut results = Vec::with_capacity(ids.len());
            for id in ids {
                let criteria = TariffClearCriteria {
                    scope,
                    id: Some(id.clone()),
                };
                let found = !state.tariffs.matching(&criteria).is_empty();
                if found {
                    let _ = actor
                        .send(ChargePointEvent::TariffsCleared { criteria })
                        .await;
                }
                results.push(TariffClearOutcome {
                    tariff_id: Some(id.clone()),
                    status: if found {
                        TariffClearStatus::Accepted
                    } else {
                        TariffClearStatus::NoTariff
                    },
                });
            }
            results
        }
        _ => {
            let criteria = TariffClearCriteria { scope, id: None };
            let matched: Vec<TariffId> = state
                .tariffs
                .matching(&criteria)
                .into_iter()
                .map(|installed| installed.tariff.id.clone())
                .collect();
            if matched.is_empty() {
                return alloc::vec![TariffClearOutcome {
                    tariff_id: None,
                    status: TariffClearStatus::NoTariff,
                }];
            }
            let _ = actor
                .send(ChargePointEvent::TariffsCleared { criteria })
                .await;
            matched
                .into_iter()
                .map(|tariff_id| TariffClearOutcome {
                    tariff_id: Some(tariff_id),
                    status: TariffClearStatus::Accepted,
                })
                .collect()
        }
    }
}

/// Which kind of tariff a [`TariffReport`] describes, matching OCPP's `TariffKindEnum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TariffKind {
    /// A default tariff installed via `SetDefaultTariff`, scoped to an EVSE or the whole charge
    /// point.
    Default,
    /// A driver tariff assigned to one running transaction via `ChangeTransactionTariff`.
    Driver,
}

/// One tariff `GetTariffs` reports - protocol-independent, combining
/// [`crate::state::TariffStore`]'s default tariffs with any driver tariff currently assigned to a
/// running transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TariffReport {
    /// The tariff's identifier.
    pub tariff_id: TariffId,
    /// Whether this is a default or a driver tariff.
    pub kind: TariffKind,
    /// Where it applies - the whole charge point, or one EVSE.
    pub scope: TariffScope,
    /// When it became active, if the CSMS said.
    pub valid_from: Option<chrono::DateTime<chrono::Utc>>,
}

/// The outcome of a CSMS-initiated `GetTariffs`, matching OCPP's `TariffGetStatusEnum`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GetTariffsOutcome {
    /// At least one tariff matched - see [`TariffReport`].
    Accepted(Vec<TariffReport>),
    /// Nothing matched.
    NoTariff,
    /// The request could not be honoured - tariff and cost isn't available.
    Rejected,
}

/// Handles a CSMS-initiated `GetTariffs` against `actor`. `evse_id` is `None` for OCPP's
/// `evseId = 0` ("from all EVSEs" - no filter at all); `Some(evse_id)` reports that EVSE's own
/// tariff plus the charge-point-wide default (see
/// [`crate::state::TariffStore::selected_by_evse`]) and any driver tariff currently assigned to a
/// transaction running on it.
pub fn handle_get_tariffs(actor: &ChargePointActor, evse_id: Option<usize>) -> GetTariffsOutcome {
    let state = actor.state();
    if !crate::refusal::capability_present(&state.capabilities, "GetTariffs") {
        return GetTariffsOutcome::Rejected;
    }
    let mut reports: Vec<TariffReport> = state
        .tariffs
        .selected_by_evse(evse_id)
        .into_iter()
        .map(|installed| TariffReport {
            tariff_id: installed.tariff.id.clone(),
            kind: TariffKind::Default,
            scope: installed.scope,
            valid_from: installed.tariff.valid_from,
        })
        .collect();
    for (id, evse) in state.evses.iter().enumerate() {
        if evse_id.is_some_and(|target| target != id) {
            continue;
        }
        for tariff in evse.transaction_tariffs.iter().flatten() {
            reports.push(TariffReport {
                tariff_id: tariff.id.clone(),
                kind: TariffKind::Driver,
                scope: TariffScope::Evse(id),
                valid_from: tariff.valid_from,
            });
        }
    }
    if reports.is_empty() {
        GetTariffsOutcome::NoTariff
    } else {
        GetTariffsOutcome::Accepted(reports)
    }
}

/// Registers this charge point's inbound `SetDefaultTariff` handling with the CSMS connection.
/// Implemented per protocol version (see the `ocpp_2_1` module) - **2.1 only**, per this module's
/// docs.
#[async_trait::async_trait]
pub trait SetDefaultTariffHandler {
    /// Registers a `SetDefaultTariff` handler with the CSMS connection that dispatches incoming
    /// requests to [`handle_set_default_tariff`] against `actor`.
    async fn register_set_default_tariff_handler(&self, actor: ChargePointActor);
}

/// Registers this charge point's inbound `ChangeTransactionTariff` handling with the CSMS
/// connection. **2.1 only**.
#[async_trait::async_trait]
pub trait ChangeTransactionTariffHandler {
    /// Registers a `ChangeTransactionTariff` handler with the CSMS connection that dispatches
    /// incoming requests to [`handle_change_transaction_tariff`] against `actor`.
    async fn register_change_transaction_tariff_handler(&self, actor: ChargePointActor);
}

/// Registers this charge point's inbound `ClearTariffs` handling with the CSMS connection.
/// **2.1 only**.
#[async_trait::async_trait]
pub trait ClearTariffsHandler {
    /// Registers a `ClearTariffs` handler with the CSMS connection that dispatches incoming
    /// requests to [`handle_clear_tariffs`] against `actor`.
    async fn register_clear_tariffs_handler(&self, actor: ChargePointActor);
}

/// Registers this charge point's inbound `GetTariffs` handling with the CSMS connection.
/// **2.1 only**.
#[async_trait::async_trait]
pub trait GetTariffsHandler {
    /// Registers a `GetTariffs` handler with the CSMS connection that dispatches incoming
    /// requests to [`handle_get_tariffs`] against `actor`.
    async fn register_get_tariffs_handler(&self, actor: ChargePointActor);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::TokioExecutor;
    use crate::hardware::Capabilities;
    use crate::state::{ChargePointEvent, IdToken, IdTokenKind};

    fn test_id_token() -> IdToken {
        IdToken {
            value: "04A224B2".into(),
            kind: IdTokenKind::ISO14443,
        }
    }

    fn tariff(id: &str) -> Tariff {
        Tariff {
            id: TariffId(id.into()),
            currency: "EUR".into(),
            valid_from: None,
        }
    }

    async fn actor_with_capability() -> ChargePointActor {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        actor
            .send(ChargePointEvent::CapabilitiesDeclared(Capabilities {
                tariff_and_cost: true,
                ..Default::default()
            }))
            .await
            .unwrap();
        actor
    }

    async fn actor_with_active_transaction() -> ChargePointActor {
        let actor = actor_with_capability().await;
        for event in [
            ConnectorEvent::CableConnected,
            ConnectorEvent::LockConfirmed,
            ConnectorEvent::IdTokenPresented(test_id_token()),
            ConnectorEvent::ChargingAuthorized(test_id_token()),
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
    async fn set_default_tariff_installs_at_its_scope() {
        let actor = actor_with_capability().await;

        let outcome = handle_set_default_tariff(&actor, TariffScope::Evse(0), tariff("t1")).await;

        assert_eq!(outcome, SetDefaultTariffOutcome::Accepted);
        assert_eq!(actor.state().tariffs.len(), 1);
    }

    #[tokio::test]
    async fn set_default_tariff_is_rejected_without_the_capability() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let outcome = handle_set_default_tariff(&actor, TariffScope::Evse(0), tariff("t1")).await;

        assert_eq!(outcome, SetDefaultTariffOutcome::Rejected);
    }

    #[tokio::test]
    async fn set_default_tariff_refuses_a_duplicate_id_at_a_different_scope() {
        let actor = actor_with_capability().await;
        handle_set_default_tariff(&actor, TariffScope::ChargePoint, tariff("t1")).await;

        let outcome = handle_set_default_tariff(&actor, TariffScope::Evse(0), tariff("t1")).await;

        assert_eq!(outcome, SetDefaultTariffOutcome::DuplicateTariffId);
    }

    #[tokio::test]
    async fn set_default_tariff_for_an_unknown_evse_is_rejected() {
        let actor = actor_with_capability().await;

        let outcome = handle_set_default_tariff(&actor, TariffScope::Evse(99), tariff("t1")).await;

        assert_eq!(outcome, SetDefaultTariffOutcome::Rejected);
    }

    #[tokio::test]
    async fn change_transaction_tariff_assigns_the_running_transaction() {
        let actor = actor_with_active_transaction().await;

        let outcome =
            handle_change_transaction_tariff(&actor, TransactionId(0), tariff("t1")).await;

        assert_eq!(outcome, ChangeTransactionTariffOutcome::Accepted);
        assert_eq!(
            actor.state().evses[0].transaction_tariffs[0],
            Some(tariff("t1"))
        );
    }

    #[tokio::test]
    async fn change_transaction_tariff_for_a_transaction_that_is_not_running_is_refused() {
        let actor = actor_with_capability().await;

        let outcome =
            handle_change_transaction_tariff(&actor, TransactionId(999), tariff("t1")).await;

        assert_eq!(outcome, ChangeTransactionTariffOutcome::TxNotFound);
    }

    #[tokio::test]
    async fn change_transaction_tariff_is_rejected_without_the_capability() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let outcome =
            handle_change_transaction_tariff(&actor, TransactionId(0), tariff("t1")).await;

        assert_eq!(outcome, ChangeTransactionTariffOutcome::Rejected);
    }

    #[tokio::test]
    async fn clear_tariffs_by_id_reports_accepted_and_removes_it() {
        let actor = actor_with_capability().await;
        handle_set_default_tariff(&actor, TariffScope::Evse(0), tariff("t1")).await;

        let results =
            handle_clear_tariffs(&actor, None, Some(alloc::vec![TariffId("t1".into())])).await;

        assert_eq!(
            results,
            alloc::vec![TariffClearOutcome {
                tariff_id: Some(TariffId("t1".into())),
                status: TariffClearStatus::Accepted,
            }]
        );
        assert!(actor.state().tariffs.is_empty());
    }

    #[tokio::test]
    async fn clear_tariffs_by_an_unknown_id_reports_no_tariff_and_changes_nothing() {
        let actor = actor_with_capability().await;
        handle_set_default_tariff(&actor, TariffScope::Evse(0), tariff("t1")).await;

        let results =
            handle_clear_tariffs(&actor, None, Some(alloc::vec![TariffId("nope".into())])).await;

        assert_eq!(
            results,
            alloc::vec![TariffClearOutcome {
                tariff_id: Some(TariffId("nope".into())),
                status: TariffClearStatus::NoTariff,
            }]
        );
        assert_eq!(actor.state().tariffs.len(), 1);
    }

    #[tokio::test]
    async fn clear_tariffs_with_no_ids_clears_every_matching_tariff() {
        let actor = actor_with_capability().await;
        handle_set_default_tariff(&actor, TariffScope::Evse(0), tariff("t1")).await;
        handle_set_default_tariff(&actor, TariffScope::ChargePoint, tariff("t2")).await;

        let results = handle_clear_tariffs(&actor, None, None).await;

        assert_eq!(results.len(), 2);
        assert!(
            results
                .iter()
                .all(|result| result.status == TariffClearStatus::Accepted)
        );
        assert!(actor.state().tariffs.is_empty());
    }

    #[tokio::test]
    async fn clear_tariffs_with_nothing_installed_reports_a_single_no_tariff() {
        let actor = actor_with_capability().await;

        let results = handle_clear_tariffs(&actor, None, None).await;

        assert_eq!(
            results,
            alloc::vec![TariffClearOutcome {
                tariff_id: None,
                status: TariffClearStatus::NoTariff,
            }]
        );
    }

    #[tokio::test]
    async fn get_tariffs_for_evse_zero_returns_everything_including_driver_tariffs() {
        let actor = actor_with_active_transaction().await;
        handle_set_default_tariff(&actor, TariffScope::Evse(0), tariff("t1")).await;
        handle_change_transaction_tariff(&actor, TransactionId(0), tariff("t2")).await;

        let outcome = handle_get_tariffs(&actor, None);

        let GetTariffsOutcome::Accepted(reports) = outcome else {
            panic!("expected Accepted, got {outcome:?}");
        };
        assert_eq!(reports.len(), 2);
        assert!(
            reports
                .iter()
                .any(|report| report.kind == TariffKind::Default)
        );
        assert!(
            reports
                .iter()
                .any(|report| report.kind == TariffKind::Driver)
        );
    }

    #[tokio::test]
    async fn get_tariffs_with_nothing_installed_reports_no_tariff() {
        let actor = actor_with_capability().await;

        assert_eq!(
            handle_get_tariffs(&actor, None),
            GetTariffsOutcome::NoTariff
        );
    }

    #[tokio::test]
    async fn get_tariffs_is_rejected_without_the_capability() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        assert_eq!(
            handle_get_tariffs(&actor, Some(0)),
            GetTariffsOutcome::Rejected
        );
    }
}

#[cfg(feature = "ocpp_2_1")]
pub mod ocpp_2_1 {
    //! OCPP 2.1 wire adapter for `SetDefaultTariff`/`ChangeTransactionTariff`/`ClearTariffs`/
    //! `GetTariffs` - the only version with tariff messages at all (see this module's parent
    //! docs).

    use super::{
        ChangeTransactionTariffHandler, ChangeTransactionTariffOutcome, ClearTariffsHandler,
        GetTariffsHandler, GetTariffsOutcome, SetDefaultTariffHandler, SetDefaultTariffOutcome,
        TariffClearStatus, TariffKind, handle_change_transaction_tariff, handle_clear_tariffs,
        handle_get_tariffs, handle_set_default_tariff,
    };
    use crate::actor::ChargePointActor;
    use crate::state::{Tariff, TariffId, TariffScope, TransactionId};
    use alloc::vec::Vec;

    use crate::wire::v21::common::{
        ClearTariffsResult, TariffAssignment, TariffChangeStatusEnum, TariffClearStatusEnum,
        TariffGetStatusEnum, TariffKindEnum, TariffSetStatusEnum,
    };
    use crate::wire::v21::{
        ChangeTransactionTariffRequest, ChangeTransactionTariffResponse, ClearTariffsRequest,
        ClearTariffsResponse, GetTariffsRequest, GetTariffsResponse, SetDefaultTariffRequest,
        SetDefaultTariffResponse,
    };
    use ocpp_client::ocpp_2_1::{OCPP2_1Client, OCPP2_1Error};

    /// OCPP addresses tariff scope as an `evseId` where `0` means "the whole charge point" and
    /// `n > 0` means the EVSE this crate indexes as `n - 1` - the same 1-based/0-based split
    /// `crate::smart_charging::ocpp_2_1` already uses for charging profiles.
    fn parse_scope(evse_id: i64) -> Option<TariffScope> {
        match evse_id {
            0 => Some(TariffScope::ChargePoint),
            id if id > 0 => Some(TariffScope::Evse(usize::try_from(id).ok()? - 1)),
            _ => None,
        }
    }

    fn scope_to_wire(scope: TariffScope) -> i64 {
        match scope {
            TariffScope::ChargePoint => 0,
            // `+ 1`: the inverse of `parse_scope`'s `- 1`.
            TariffScope::Evse(id) => i64::try_from(id).unwrap_or(i64::MAX) + 1,
        }
    }

    /// `evseId = 0` means "from every EVSE" for `GetTariffs`/`ClearTariffs` - unlike
    /// `parse_scope`, this has no `ChargePoint` scope of its own to return, only "no filter".
    /// Negative values are likewise treated as "no filter" rather than propagated as an error -
    /// OCPP defines no negative `evseId`, so there is nothing more specific to refuse with.
    fn parse_evse_filter(evse_id: i64) -> Option<usize> {
        usize::try_from(evse_id - 1).ok()
    }

    fn wire_tariff(tariff: &crate::wire::v21::common::Tariff) -> Tariff {
        Tariff {
            id: TariffId(tariff.tariff_id.as_str().into()),
            currency: tariff.currency.as_str().into(),
            // Infallible since `ocpp-types` 0.2.0: `validFrom` arrives as an already-validated
            // `OcppTimestamp`, so the "unparseable becomes absent" fallback has no case left.
            valid_from: tariff.valid_from.map(Into::into),
        }
    }

    async fn handle_set_default(
        actor: &ChargePointActor,
        request: &SetDefaultTariffRequest,
    ) -> Result<SetDefaultTariffResponse, OCPP2_1Error> {
        let Some(scope) = parse_scope(request.evse_id) else {
            return Ok(SetDefaultTariffResponse {
                custom_data: None,
                status: TariffSetStatusEnum::Rejected,
                status_info: None,
            });
        };
        let outcome = handle_set_default_tariff(actor, scope, wire_tariff(&request.tariff)).await;
        let status = match outcome {
            SetDefaultTariffOutcome::Accepted => TariffSetStatusEnum::Accepted,
            SetDefaultTariffOutcome::DuplicateTariffId => TariffSetStatusEnum::DuplicateTariffId,
            SetDefaultTariffOutcome::TooManyElements => TariffSetStatusEnum::TooManyElements,
            SetDefaultTariffOutcome::Rejected => TariffSetStatusEnum::Rejected,
        };
        Ok(SetDefaultTariffResponse {
            custom_data: None,
            status,
            status_info: None,
        })
    }

    #[async_trait::async_trait]
    impl SetDefaultTariffHandler for OCPP2_1Client {
        async fn register_set_default_tariff_handler(&self, actor: ChargePointActor) {
            self.on_set_default_tariff(move |request, _client| {
                let actor = actor.clone();
                async move { handle_set_default(&actor, &request).await }
            })
            .await;
        }
    }

    async fn handle_change_transaction(
        actor: &ChargePointActor,
        request: &ChangeTransactionTariffRequest,
    ) -> Result<ChangeTransactionTariffResponse, OCPP2_1Error> {
        let Some(transaction_id) = request
            .transaction_id
            .parse::<u64>()
            .ok()
            .map(TransactionId)
        else {
            return Ok(ChangeTransactionTariffResponse {
                custom_data: None,
                status: TariffChangeStatusEnum::TxNotFound,
                status_info: None,
            });
        };
        let outcome =
            handle_change_transaction_tariff(actor, transaction_id, wire_tariff(&request.tariff))
                .await;
        let status = match outcome {
            ChangeTransactionTariffOutcome::Accepted => TariffChangeStatusEnum::Accepted,
            ChangeTransactionTariffOutcome::TxNotFound => TariffChangeStatusEnum::TxNotFound,
            ChangeTransactionTariffOutcome::Rejected => TariffChangeStatusEnum::Rejected,
        };
        Ok(ChangeTransactionTariffResponse {
            custom_data: None,
            status,
            status_info: None,
        })
    }

    #[async_trait::async_trait]
    impl ChangeTransactionTariffHandler for OCPP2_1Client {
        async fn register_change_transaction_tariff_handler(&self, actor: ChargePointActor) {
            self.on_change_transaction_tariff(move |request, _client| {
                let actor = actor.clone();
                async move { handle_change_transaction(&actor, &request).await }
            })
            .await;
        }
    }

    /// `ClearTariffsResponse` carries no top-level status field, so a runtime-absent capability
    /// must refuse with a CALLERROR rather than an optimistic empty result list - mirrors
    /// `crate::cost::ocpp_2_1::handle`. Everything else delegates straight to
    /// [`handle_clear_tariffs`].
    async fn handle_clear(
        actor: &ChargePointActor,
        request: &ClearTariffsRequest,
    ) -> Result<ClearTariffsResponse, OCPP2_1Error> {
        if !crate::refusal::capability_present(&actor.state().capabilities, "ClearTariffs") {
            return Err(crate::refusal::ocpp_2_1_not_supported("ClearTariffs"));
        }
        let scope = request.evse_id.and_then(parse_scope);
        let ids: Option<Vec<TariffId>> = request
            .tariff_ids
            .as_ref()
            .map(|ids| ids.iter().map(|id| TariffId(id.as_str().into())).collect());
        let results = handle_clear_tariffs(actor, scope, ids).await;
        let clear_tariffs_result = results
            .into_iter()
            .map(|result| ClearTariffsResult {
                custom_data: None,
                status: match result.status {
                    TariffClearStatus::Accepted => TariffClearStatusEnum::Accepted,
                    TariffClearStatus::NoTariff => TariffClearStatusEnum::NoTariff,
                },
                status_info: None,
                tariff_id: result
                    .tariff_id
                    .and_then(|id| heapless::String::try_from(id.0.as_str()).ok()),
            })
            .collect();
        Ok(ClearTariffsResponse {
            clear_tariffs_result,
            custom_data: None,
        })
    }

    #[async_trait::async_trait]
    impl ClearTariffsHandler for OCPP2_1Client {
        async fn register_clear_tariffs_handler(&self, actor: ChargePointActor) {
            self.on_clear_tariffs(move |request, _client| {
                let actor = actor.clone();
                async move { handle_clear(&actor, &request).await }
            })
            .await;
        }
    }

    fn report_to_wire(report: &super::TariffReport) -> Option<TariffAssignment> {
        Some(TariffAssignment {
            custom_data: None,
            evse_ids: match report.scope {
                TariffScope::ChargePoint => None,
                TariffScope::Evse(id) => Some(alloc::vec![scope_to_wire(TariffScope::Evse(id))]),
            },
            id_tokens: None,
            tariff_id: heapless::String::try_from(report.tariff_id.0.as_str()).ok()?,
            tariff_kind: match report.kind {
                TariffKind::Default => TariffKindEnum::DefaultTariff,
                TariffKind::Driver => TariffKindEnum::DriverTariff,
            },
            valid_from: report.valid_from.map(Into::into),
        })
    }

    fn handle_get(actor: &ChargePointActor, request: &GetTariffsRequest) -> GetTariffsResponse {
        let outcome = handle_get_tariffs(actor, parse_evse_filter(request.evse_id));
        match outcome {
            GetTariffsOutcome::Accepted(reports) => GetTariffsResponse {
                custom_data: None,
                status: TariffGetStatusEnum::Accepted,
                status_info: None,
                tariff_assignments: Some(reports.iter().filter_map(report_to_wire).collect()),
            },
            GetTariffsOutcome::NoTariff => GetTariffsResponse {
                custom_data: None,
                status: TariffGetStatusEnum::NoTariff,
                status_info: None,
                tariff_assignments: None,
            },
            GetTariffsOutcome::Rejected => GetTariffsResponse {
                custom_data: None,
                status: TariffGetStatusEnum::Rejected,
                status_info: None,
                tariff_assignments: None,
            },
        }
    }

    #[async_trait::async_trait]
    impl GetTariffsHandler for OCPP2_1Client {
        async fn register_get_tariffs_handler(&self, actor: ChargePointActor) {
            self.on_get_tariffs(move |request, _client| {
                let actor = actor.clone();
                async move { Ok::<_, OCPP2_1Error>(handle_get(&actor, &request)) }
            })
            .await;
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::executor::TokioExecutor;
        use crate::hardware::Capabilities;
        use crate::state::ChargePointEvent;
        use crate::wire::v21::RpcErrorCode;
        use crate::wire::v21::common::Tariff as WireTariff;

        fn wire_tariff_request(id: &str) -> WireTariff {
            WireTariff {
                charging_time: None,
                currency: heapless::String::try_from("EUR").unwrap(),
                custom_data: None,
                description: None,
                energy: None,
                fixed_fee: None,
                idle_time: None,
                max_cost: None,
                min_cost: None,
                reservation_fixed: None,
                reservation_time: None,
                tariff_id: heapless::String::try_from(id).unwrap(),
                valid_from: None,
            }
        }

        #[test]
        fn scope_zero_is_charge_point_wide() {
            assert_eq!(parse_scope(0), Some(TariffScope::ChargePoint));
        }

        #[test]
        fn scope_one_is_the_first_evse() {
            assert_eq!(parse_scope(1), Some(TariffScope::Evse(0)));
        }

        #[test]
        fn a_negative_scope_does_not_parse() {
            assert_eq!(parse_scope(-1), None);
        }

        #[test]
        fn scope_round_trips_through_the_wire() {
            assert_eq!(scope_to_wire(TariffScope::ChargePoint), 0);
            assert_eq!(scope_to_wire(TariffScope::Evse(0)), 1);
            assert_eq!(
                parse_scope(scope_to_wire(TariffScope::Evse(3))),
                Some(TariffScope::Evse(3))
            );
        }

        #[test]
        fn evse_filter_zero_means_no_filter() {
            assert_eq!(parse_evse_filter(0), None);
        }

        #[test]
        fn evse_filter_one_is_the_first_evse() {
            assert_eq!(parse_evse_filter(1), Some(0));
        }

        #[tokio::test]
        async fn set_default_tariff_is_a_not_supported_status_when_the_capability_is_absent() {
            let actor = ChargePointActor::spawn([1], &TokioExecutor);
            let request = SetDefaultTariffRequest {
                custom_data: None,
                evse_id: 1,
                tariff: wire_tariff_request("t1"),
            };

            let response = handle_set_default(&actor, &request).await.unwrap();

            assert_eq!(response.status, TariffSetStatusEnum::Rejected);
        }

        #[tokio::test]
        async fn set_default_tariff_accepts_a_valid_request() {
            let actor = ChargePointActor::spawn([1], &TokioExecutor);
            actor
                .send(ChargePointEvent::CapabilitiesDeclared(Capabilities {
                    tariff_and_cost: true,
                    ..Default::default()
                }))
                .await
                .unwrap();
            let request = SetDefaultTariffRequest {
                custom_data: None,
                evse_id: 1,
                tariff: wire_tariff_request("t1"),
            };

            let response = handle_set_default(&actor, &request).await.unwrap();

            assert_eq!(response.status, TariffSetStatusEnum::Accepted);
            assert_eq!(actor.state().tariffs.len(), 1);
        }

        #[tokio::test]
        async fn change_transaction_tariff_reports_tx_not_found_for_an_unparseable_id() {
            let actor = ChargePointActor::spawn([1], &TokioExecutor);
            let request = ChangeTransactionTariffRequest {
                custom_data: None,
                tariff: wire_tariff_request("t1"),
                transaction_id: heapless::String::try_from("not-a-number").unwrap(),
            };

            let response = handle_change_transaction(&actor, &request).await.unwrap();

            assert_eq!(response.status, TariffChangeStatusEnum::TxNotFound);
        }

        #[tokio::test]
        async fn clear_tariffs_is_a_call_error_when_the_capability_is_absent() {
            let actor = ChargePointActor::spawn([1], &TokioExecutor);
            let request = ClearTariffsRequest {
                custom_data: None,
                evse_id: None,
                tariff_ids: None,
            };

            let result = handle_clear(&actor, &request).await;

            assert_eq!(result.unwrap_err().code, RpcErrorCode::NotSupported);
        }

        #[tokio::test]
        async fn clear_tariffs_reports_no_tariff_with_nothing_installed() {
            let actor = ChargePointActor::spawn([1], &TokioExecutor);
            actor
                .send(ChargePointEvent::CapabilitiesDeclared(Capabilities {
                    tariff_and_cost: true,
                    ..Default::default()
                }))
                .await
                .unwrap();
            let request = ClearTariffsRequest {
                custom_data: None,
                evse_id: None,
                tariff_ids: None,
            };

            let response = handle_clear(&actor, &request).await.unwrap();

            assert_eq!(response.clear_tariffs_result.len(), 1);
            assert_eq!(
                response.clear_tariffs_result[0].status,
                TariffClearStatusEnum::NoTariff
            );
        }

        #[tokio::test]
        async fn get_tariffs_reports_no_tariff_with_nothing_installed() {
            let actor = ChargePointActor::spawn([1], &TokioExecutor);
            actor
                .send(ChargePointEvent::CapabilitiesDeclared(Capabilities {
                    tariff_and_cost: true,
                    ..Default::default()
                }))
                .await
                .unwrap();
            let request = GetTariffsRequest {
                custom_data: None,
                evse_id: 0,
            };

            let response = handle_get(&actor, &request);

            assert_eq!(response.status, TariffGetStatusEnum::NoTariff);
        }

        #[tokio::test]
        async fn get_tariffs_is_rejected_when_the_capability_is_absent() {
            let actor = ChargePointActor::spawn([1], &TokioExecutor);
            let request = GetTariffsRequest {
                custom_data: None,
                evse_id: 0,
            };

            let response = handle_get(&actor, &request);

            assert_eq!(response.status, TariffGetStatusEnum::Rejected);
        }
    }
}
