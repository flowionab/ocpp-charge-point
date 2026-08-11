//! Tariff and cost functional block: the tariff store and per-transaction tariff assignment
//! (OCPP 2.1's `SetDefaultTariff`, `ChangeTransactionTariff`, `ClearTariffs`, `GetTariffs`). See
//! `docs/ROADMAP.md` §9 and `docs/PRODUCTION-ROADMAP.md` B7.1.
//!
//! **2.1-only.** 1.6J and 2.0.1 have no tariff messages at all - a spec boundary this crate
//! reflects by never registering these handlers under those versions, not a gap to close.
//!
//! This module **stores and reports** tariffs, and - since CV8 - drives [`crate::pricing`] from
//! whichever one currently prices a transaction: [`effective_tariff`] resolves it (the driver
//! tariff below if one is assigned, otherwise [`crate::state::TariffStore`]'s applicable default)
//! and [`advance_running_cost`] feeds the engine from the transaction's own meter samples,
//! storing the result on [`crate::state::EvseState::running_cost`] for `crate::transactions` to
//! report as `costDetails` (I07, I08, I11, I12). [`crate::cost::handle_cost_updated`] - the CSMS
//! *telling* this charge point a running cost, rather than the station working it out - is
//! unrelated and keeps working exactly as before.
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
    TariffId, TariffScope, TariffSetRejection, Transaction, TransactionChargingState,
    TransactionId,
};
use chrono::{DateTime, Utc};

/// The outcome of a CSMS-initiated `SetDefaultTariff`, matching OCPP's `TariffSetStatusEnum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetDefaultTariffOutcome {
    /// The tariff was installed as the default for the requested scope (I07.FR.10).
    Accepted,
    /// A tariff with this id is already installed at a different scope - see
    /// [`crate::state::TariffSetRejection::DuplicateTariffId`] (I07.FR.04).
    DuplicateTariffId,
    /// Either the tariff carries more price elements in one dimension than
    /// `TariffCostCtrlr.MaxElements[Tariff]` allows (I07.FR.02), or the store is already at
    /// [`crate::state::StateLimits::max_tariffs`] and this tariff isn't replacing an existing one.
    TooManyElements,
    /// The tariff uses conditions and this station reports
    /// `TariffCostCtrlr.ConditionsSupported[Tariff] = false` (I07.FR.03).
    ConditionNotSupported,
    /// The request addresses an EVSE this charge point doesn't have (I07.FR.06). OCPP answers
    /// `Rejected` for this and for [`Self::Rejected`] alike; they are separate variants so the
    /// adapter can attach the `reasonCode` that tells the two apart.
    UnknownEvse,
    /// The request could not be honoured - tariff and cost isn't available (I07.FR.01), or the
    /// tariff structure is one this station cannot represent (I07.FR.05).
    Rejected,
}

/// Handles a CSMS-initiated `SetDefaultTariff` against `actor`. Like every handler in this crate,
/// the outcome is decided against the real state (via a trial install on a clone of the store)
/// before anything is dispatched, so the status the CSMS receives is what actually happened -
/// mirrors `crate::smart_charging::handle_set_charging_profile`.
#[tracing::instrument(skip_all)]
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
        return SetDefaultTariffOutcome::UnknownEvse;
    }
    if tariff.max_prices_per_dimension() > max_price_elements(&state) {
        return SetDefaultTariffOutcome::TooManyElements;
    }
    if tariff.has_conditions() && !conditions_supported(&state) {
        return SetDefaultTariffOutcome::ConditionNotSupported;
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

/// How many price elements one dimension of a tariff may carry before this station answers
/// `TooManyElements` - the value it registers as `TariffCostCtrlr.MaxElements[Tariff]`
/// (I07.FR.02, I11.FR.02).
///
/// A bound rather than "as many as `alloc` will hold": a tariff arrives from the network, is kept
/// for the life of the installation, and is walked once per priced interval on a device with
/// kilobytes of RAM. Sixteen is twice the largest structure `ocpp-types`' own fixed-capacity
/// representation carries and several times the most complex worked example in the
/// specification.
pub const MAX_TARIFF_PRICE_ELEMENTS: usize = 16;

/// `TariffCostCtrlr.MaxElements[Tariff]`, or [`MAX_TARIFF_PRICE_ELEMENTS`] when it is absent or
/// unparseable - the variable is registered `ReadOnly`, so the two agree unless a build has
/// removed it.
fn max_price_elements(state: &ChargePointState) -> usize {
    tariff_variable(state, "MaxElements")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(MAX_TARIFF_PRICE_ELEMENTS)
}

/// `TariffCostCtrlr.ConditionsSupported[Tariff]`. Defaults to `true`, which is what this build
/// registers: [`crate::pricing`] evaluates every condition OCPP defines.
fn conditions_supported(state: &ChargePointState) -> bool {
    tariff_variable(state, "ConditionsSupported")
        .and_then(|value| value.parse::<bool>().ok())
        .unwrap_or(true)
}

/// The `Actual` value of `TariffCostCtrlr.<variable>[Tariff]`, if registered.
fn tariff_variable(state: &ChargePointState, variable: &str) -> Option<alloc::string::String> {
    use crate::state::{Component, Variable, VariableAttributeType};
    state
        .device_model
        .get(
            &Component {
                name: "TariffCostCtrlr".into(),
                instance: None,
                evse: None,
            },
            &Variable {
                name: variable.into(),
                instance: Some("Tariff".into()),
            },
        )?
        .attribute(VariableAttributeType::Actual)
        .map(|attribute| attribute.value.clone())
}

/// The outcome of a CSMS-initiated `ChangeTransactionTariff`, matching OCPP's
/// `TariffChangeStatusEnum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeTransactionTariffOutcome {
    /// The tariff was assigned to the named transaction (I11.FR.06).
    Accepted,
    /// No transaction with that id is currently running on this charge point (I11.FR.04) -
    /// refused rather than silently stored, per `docs/PRODUCTION-ROADMAP.md` B7.1.
    TxNotFound,
    /// The tariff carries more price elements in one dimension than
    /// `TariffCostCtrlr.MaxElements[Tariff]` allows (I11.FR.02).
    TooManyElements,
    /// The tariff uses conditions and this station reports
    /// `TariffCostCtrlr.ConditionsSupported[Tariff] = false` (I11.FR.03).
    ConditionNotSupported,
    /// The new tariff is in a different currency from the one already pricing the transaction
    /// (I11.FR.05) - switching currency mid-session is not allowed, since the running cost the
    /// driver has already been shown would silently change meaning.
    NoCurrencyChange,
    /// The request could not be honoured - tariff and cost isn't available (I11.FR.01), or the
    /// tariff structure is one this station cannot represent.
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

/// The currency the tariff currently pricing this connector's transaction is stated in, for
/// I11.FR.05.
///
/// The driver tariff wins where there is one; otherwise it is the default tariff installed for
/// this EVSE. Deliberately does *not* consult a clock to pick between a scope's current and
/// future default: a currency belongs to an operator's whole tariff set rather than to an
/// instant, so a scope whose tariffs disagree about it is a CSMS error either way, and the
/// most recently installed one is the newest instruction.
fn active_currency(
    state: &ChargePointState,
    evse_id: usize,
    connector_id: usize,
) -> Option<alloc::string::String> {
    if let Some(tariff) = state
        .evses
        .get(evse_id)
        .and_then(|evse| evse.transaction_tariffs.get(connector_id))
        .and_then(Option::as_ref)
    {
        return Some(tariff.currency.clone());
    }
    state
        .tariffs
        .selected_by_evse(Some(evse_id))
        .last()
        .map(|installed| installed.tariff.currency.clone())
}

/// Handles a CSMS-initiated `ChangeTransactionTariff` against `actor`: finds the connector whose
/// active transaction is `transaction_id` and assigns `tariff` to it as a driver tariff.
#[tracing::instrument(skip_all)]
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
    if tariff.max_prices_per_dimension() > max_price_elements(&state) {
        return ChangeTransactionTariffOutcome::TooManyElements;
    }
    if tariff.has_conditions() && !conditions_supported(&state) {
        return ChangeTransactionTariffOutcome::ConditionNotSupported;
    }
    if active_currency(&state, evse_id, connector_id)
        .is_some_and(|currency| currency != tariff.currency)
    {
        return ChangeTransactionTariffOutcome::NoCurrencyChange;
    }

    let _ = actor
        .send(ChargePointEvent::Evse {
            evse_id,
            event: EvseEvent::Connector {
                connector_id,
                event: ConnectorEvent::TariffAssigned(alloc::boxed::Box::new(tariff)),
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
#[tracing::instrument(skip_all)]
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
    /// Every EVSE this tariff is installed at, as this crate's 0-based ids (I09.FR.04/.05/.06).
    ///
    /// A charge-point-wide default is reported against *each* EVSE rather than as a `0`: I07.FR.12
    /// defines `evseId = 0` as "install at each EVSE", so that is what is installed and that is
    /// what an honest report shows. Empty only for a driver tariff whose transaction has not
    /// started yet, which has no EVSE to name.
    pub evse_ids: Vec<usize>,
    /// The identifiers this tariff was issued for - a driver tariff's `idTokens` (I09.FR.05).
    /// Empty for a default tariff, which applies to whoever plugs in.
    pub id_tokens: Vec<crate::state::IdToken>,
    /// When it became active, if the CSMS said (I09.FR.07).
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
            // I09.FR.04: every EVSE this tariff is installed at. A charge-point-wide default is
            // installed at all of them (I07.FR.12), narrowed to the requested one when the CSMS
            // asked about a single EVSE.
            evse_ids: match installed.scope {
                TariffScope::ChargePoint => match evse_id {
                    Some(evse_id) => alloc::vec![evse_id],
                    None => (0..state.evses.len()).collect(),
                },
                TariffScope::Evse(id) => alloc::vec![id],
            },
            id_tokens: Vec::new(),
            valid_from: installed.tariff.valid_from,
        })
        .collect();
    for (id, evse) in state.evses.iter().enumerate() {
        if evse_id.is_some_and(|target| target != id) {
            continue;
        }
        for (connector_id, tariff) in evse.transaction_tariffs.iter().enumerate() {
            let Some(tariff) = tariff else { continue };
            reports.push(TariffReport {
                tariff_id: tariff.id.clone(),
                kind: TariffKind::Driver,
                // I09.FR.06: a driver tariff is reported against the EVSE whose transaction it is
                // pricing.
                evse_ids: alloc::vec![id],
                // I09.FR.05: the identifier the tariff was issued for, which is the one that
                // authorized the transaction it is assigned to.
                id_tokens: evse
                    .transactions
                    .get(connector_id)
                    .and_then(Option::as_ref)
                    .and_then(|transaction| transaction.id_token.clone())
                    .into_iter()
                    .collect(),
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

/// The tariff currently pricing `connector_id`'s transaction on `evse_id`, at `now` - the driver
/// tariff in [`crate::state::EvseState::transaction_tariffs`] if `ChangeTransactionTariff` has
/// assigned one (I08, I11), otherwise whichever of [`crate::state::TariffStore`]'s tariffs is
/// [`crate::state::TariffStore::effective_at`] this EVSE right now (I07). `None` when neither
/// applies, which is not an error: a transaction with no tariff simply costs nothing to report.
///
/// Re-resolved on every call rather than pinned at the transaction's start, which is what lets a
/// default tariff installed with a future `validFrom` (I07's scenario #3) take over automatically
/// at the instant it becomes valid, mid-session, with no `ChangeTransactionTariff` needed -
/// [`crate::pricing::TransactionCost::advance`] already detects the tariff id changing and seals
/// what was charged under the old one. The cost is that a `ClearTariffs` removing the *only*
/// tariff a transaction is using this way freezes its running cost rather than continuing under
/// a tariff this station no longer holds a copy of; a driver tariff never has this problem, since
/// assigning one does not touch the store at all.
pub(crate) fn effective_tariff(
    state: &ChargePointState,
    evse_id: usize,
    connector_id: usize,
    now: DateTime<Utc>,
) -> Option<Tariff> {
    if let Some(driver) = state
        .evses
        .get(evse_id)?
        .transaction_tariffs
        .get(connector_id)?
        .clone()
    {
        return Some(driver);
    }
    state.tariffs.effective_at(evse_id, now).cloned()
}

/// What [`effective_tariff`] can see of a transaction's own progress, translated into
/// [`crate::pricing::PricingContext`] (I12).
///
/// Only what [`crate::state::Transaction`] already records: local time offset, EVSE kind and
/// payment brand/recognition are not tracked anywhere in this crate's state yet, so every
/// condition that depends on one of them is correctly evaluated as unmet rather than guessed -
/// see `crate::pricing`'s own docs on an unevaluable condition.
fn pricing_context(
    transaction: &Transaction,
    now: DateTime<Utc>,
) -> crate::pricing::PricingContext {
    let sample = transaction.last_meter_sample;
    crate::pricing::PricingContext {
        // `MeterSample` is in Wh/W/mA; `PricingContext` wants mWh/mW/mA - see
        // `crate::state::Money::milli_from_decimal`'s docs for the same thousandths convention.
        energy_mwh: sample
            .map(|s| s.energy_wh.saturating_mul(1_000))
            .unwrap_or(0),
        power_mw: sample
            .and_then(|s| s.power_w)
            .map(|w| w.saturating_mul(1_000)),
        current_ma: sample.and_then(|s| s.current_ma),
        charging: transaction.charging_state == TransactionChargingState::Charging,
        ..crate::pricing::PricingContext::new(now)
    }
}

/// Advances `evse_id`/`connector_id`'s local running-cost calculation to `now` (I07, I08, I11,
/// I12), storing the result on [`crate::state::EvseState::running_cost`] and returning it
/// alongside the tariff that produced it - `crate::transactions` reads both, because
/// `min_cost`/`max_cost` live on [`Tariff`] rather than on
/// [`crate::pricing::TransactionCost`] itself (see `TransactionCost::totals`'s signature).
///
/// `None` when no tariff currently prices this transaction - see [`effective_tariff`] - in which
/// case nothing is stored and any previously computed total is left exactly as it was, not
/// cleared: a transaction that briefly has no tariff (say, between one ending and the next being
/// installed) should not lose the total it already earned.
///
/// Takes `now` and reads the actor's state rather than being folded into
/// [`crate::state::ChargePointState::apply`] itself, because the state machine is deliberately
/// clock-free (see `crate::clock`'s docs) - this is meant to be called from the same adapter that
/// is about to timestamp a `TransactionEvent` with its own [`crate::clock::Clock`], so the two
/// timestamps agree.
pub(crate) async fn advance_running_cost(
    actor: &ChargePointActor,
    evse_id: usize,
    connector_id: usize,
    now: DateTime<Utc>,
    transaction: &Transaction,
) -> Option<(crate::pricing::TransactionCost, Tariff)> {
    let state = actor.state();
    let tariff = effective_tariff(&state, evse_id, connector_id, now)?;
    let context = pricing_context(transaction, now);
    let existing = state
        .evses
        .get(evse_id)?
        .running_cost
        .get(connector_id)?
        .clone();
    let cost = match existing {
        // I11: a tariff change - driver-assigned, or a scheduled default taking over - is
        // detected and sealed inside `advance` itself; see `TransactionCost::advance`'s docs.
        Some(mut cost) => {
            cost.advance(&tariff, &context);
            cost
        }
        // No tariff has priced this transaction before now - either it just started, or one
        // only became available partway through. Either way pricing starts *from here*, never
        // retroactively: I12.FR.30's fixed fee and I12.FR.07/.08's reservation dimensions are
        // therefore only charged when a tariff was already assigned at the transaction's true
        // start - there is no meter history to back-price them against once it wasn't.
        None => crate::pricing::TransactionCost::start(&tariff, &context, None),
    };
    let _ = actor
        .send(ChargePointEvent::Evse {
            evse_id,
            event: EvseEvent::Connector {
                connector_id,
                event: ConnectorEvent::RunningCostAdvanced(Box::new(cost.clone())),
            },
        })
        .await;
    Some((cost, tariff))
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
        Tariff::new(TariffId(id.into()), "EUR")
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

        // I07.FR.06 - `Rejected` on the wire, with the `reasonCode` that says which rejection.
        assert_eq!(outcome, SetDefaultTariffOutcome::UnknownEvse);
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

    fn energy_priced_tariff(id: &str, price_per_kwh: crate::state::Money) -> Tariff {
        let mut tariff = tariff(id);
        tariff.energy = Some(crate::state::EnergyComponent {
            prices: alloc::vec![crate::state::EnergyPrice {
                price_per_kwh,
                conditions: None,
            }],
            tax_rates: Vec::new(),
        });
        tariff
    }

    /// Drives connector 0 of EVSE 0 all the way to `Charging` and records one meter sample -
    /// what `advance_running_cost` needs `crate::state::Transaction::last_meter_sample` to hold.
    async fn actor_with_charging_transaction(energy_wh: i64) -> ChargePointActor {
        let actor = actor_with_active_transaction().await;
        for event in [
            ConnectorEvent::ContactorClosed,
            ConnectorEvent::MeterValueSampled(crate::state::MeterSample {
                energy_wh,
                ..Default::default()
            }),
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
    async fn effective_tariff_prefers_the_driver_tariff_over_the_default() {
        let actor = actor_with_active_transaction().await;
        handle_set_default_tariff(&actor, TariffScope::Evse(0), tariff("default")).await;
        handle_change_transaction_tariff(&actor, TransactionId(0), tariff("driver")).await;

        let resolved = effective_tariff(&actor.state(), 0, 0, Utc::now());

        assert_eq!(resolved, Some(tariff("driver")));
    }

    #[tokio::test]
    async fn effective_tariff_falls_back_to_the_default_tariff() {
        let actor = actor_with_active_transaction().await;
        handle_set_default_tariff(&actor, TariffScope::Evse(0), tariff("default")).await;

        let resolved = effective_tariff(&actor.state(), 0, 0, Utc::now());

        assert_eq!(resolved, Some(tariff("default")));
    }

    #[tokio::test]
    async fn effective_tariff_is_none_without_either() {
        let actor = actor_with_active_transaction().await;

        assert_eq!(effective_tariff(&actor.state(), 0, 0, Utc::now()), None);
    }

    /// Records another meter sample against connector 0 of EVSE 0, mirroring what hardware
    /// pushes in mid-session - see `actor_with_charging_transaction`.
    async fn send_meter_sample(actor: &ChargePointActor, energy_wh: i64) {
        actor
            .send(ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event: ConnectorEvent::MeterValueSampled(crate::state::MeterSample {
                        energy_wh,
                        ..Default::default()
                    }),
                },
            })
            .await
            .unwrap();
    }

    async fn current_transaction(actor: &ChargePointActor) -> Transaction {
        actor.state().evses[0].transactions[0].clone().unwrap()
    }

    #[tokio::test]
    async fn advancing_the_running_cost_without_a_tariff_computes_nothing() {
        let actor = actor_with_charging_transaction(10_000).await;

        let transaction = current_transaction(&actor).await;
        let result = advance_running_cost(&actor, 0, 0, Utc::now(), &transaction).await;

        assert_eq!(result, None);
        assert_eq!(actor.state().evses[0].running_cost[0], None);
    }

    // I07: a default tariff, with nothing driver-assigned, prices the energy a transaction
    // delivers *after* it becomes available - see `advance_running_cost`'s docs on why the
    // energy already on the meter when a tariff first applies is not charged retroactively.
    #[tokio::test]
    async fn advancing_the_running_cost_prices_from_the_default_tariff() {
        let actor = actor_with_charging_transaction(0).await;
        handle_set_default_tariff(
            &actor,
            TariffScope::Evse(0),
            energy_priced_tariff("default", crate::state::Money(250_000)),
        )
        .await;
        // Establishes the pricing baseline at the meter's current (zero) reading.
        advance_running_cost(&actor, 0, 0, Utc::now(), &current_transaction(&actor).await)
            .await
            .expect("a default tariff is installed");

        send_meter_sample(&actor, 10_000).await;
        let (cost, tariff_used) =
            advance_running_cost(&actor, 0, 0, Utc::now(), &current_transaction(&actor).await)
                .await
                .expect("a default tariff is installed");

        assert_eq!(tariff_used.id, TariffId("default".into()));
        // 10 kWh @ 0.25/kWh.
        assert_eq!(
            cost.totals(&tariff_used).energy.unwrap().excl_tax,
            crate::state::Money(2_500_000)
        );
        assert_eq!(actor.state().evses[0].running_cost[0].as_ref(), Some(&cost));
    }

    // I08: a driver tariff assigned mid-session outprices the default from that point on.
    #[tokio::test]
    async fn advancing_the_running_cost_prefers_the_driver_tariff() {
        let actor = actor_with_charging_transaction(10_000).await;
        handle_set_default_tariff(
            &actor,
            TariffScope::Evse(0),
            energy_priced_tariff("default", crate::state::Money(250_000)),
        )
        .await;
        handle_change_transaction_tariff(
            &actor,
            TransactionId(0),
            energy_priced_tariff("driver", crate::state::Money(1_000_000)),
        )
        .await;

        let transaction = current_transaction(&actor).await;
        let (_, tariff_used) = advance_running_cost(&actor, 0, 0, Utc::now(), &transaction)
            .await
            .expect("a driver tariff is assigned");

        assert_eq!(tariff_used.id, TariffId("driver".into()));
    }

    // Two later calls, each with more energy delivered, both advance the same accumulator
    // rather than restarting it - restarting would silently drop what the first already priced.
    #[tokio::test]
    async fn later_advances_accumulate_rather_than_restarting() {
        let actor = actor_with_charging_transaction(0).await;
        handle_set_default_tariff(
            &actor,
            TariffScope::Evse(0),
            energy_priced_tariff("default", crate::state::Money(250_000)),
        )
        .await;
        advance_running_cost(&actor, 0, 0, Utc::now(), &current_transaction(&actor).await)
            .await
            .unwrap();

        send_meter_sample(&actor, 5_000).await;
        advance_running_cost(&actor, 0, 0, Utc::now(), &current_transaction(&actor).await)
            .await
            .unwrap();

        send_meter_sample(&actor, 10_000).await;
        let (cost, tariff_used) =
            advance_running_cost(&actor, 0, 0, Utc::now(), &current_transaction(&actor).await)
                .await
                .unwrap();

        // 10 kWh total @ 0.25/kWh across both advances, not just the last one's 5 kWh delta.
        assert_eq!(
            cost.totals(&tariff_used).energy.unwrap().excl_tax,
            crate::state::Money(2_500_000)
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
    use crate::state::{
        EnergyComponent, EnergyPrice, EvseKind, FixedComponent, FixedPrice, Money, Price, Tariff,
        TariffConditions, TariffConditionsFixed, TariffId, TariffScope, TaxPercent, TaxRate,
        TimeComponent, TimeOfDay, TimePrice, TransactionId, milli_from_decimal,
    };
    use alloc::boxed::Box;
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

    /// Converts one wire `TariffConditionsType`.
    ///
    /// Every physical threshold is an OCPP `decimal`, which is where a float would otherwise enter
    /// the model; [`milli_from_decimal`] converts it once, here, into the thousandths
    /// [`TariffConditions`] stores. A threshold that is not a finite number the station can hold
    /// makes the whole tariff invalid rather than silently dropping the restriction - dropping a
    /// `maxPower` would apply an expensive price to a session that was meant to escape it.
    fn wire_conditions(
        conditions: &crate::wire::v21::common::TariffConditions,
    ) -> Option<TariffConditions> {
        let milli = |value: Option<f64>| match value {
            None => Some(None),
            Some(value) => milli_from_decimal(value).map(Some),
        };
        Some(TariffConditions {
            start_time_of_day: conditions.start_time_of_day.map(wire_time_of_day),
            end_time_of_day: conditions.end_time_of_day.map(wire_time_of_day),
            day_of_week: conditions
                .day_of_week
                .iter()
                .flatten()
                .map(wire_weekday)
                .collect(),
            valid_from_date: wire_date(conditions.valid_from_date)?,
            valid_to_date: wire_date(conditions.valid_to_date)?,
            evse_kind: conditions.evse_kind.as_ref().map(wire_evse_kind),
            min_energy_mwh: milli(conditions.min_energy)?,
            max_energy_mwh: milli(conditions.max_energy)?,
            min_current_ma: milli(conditions.min_current)?,
            max_current_ma: milli(conditions.max_current)?,
            min_power_mw: milli(conditions.min_power)?,
            max_power_mw: milli(conditions.max_power)?,
            min_time_secs: conditions.min_time,
            max_time_secs: conditions.max_time,
            min_charging_time_secs: conditions.min_charging_time,
            max_charging_time_secs: conditions.max_charging_time,
            min_idle_time_secs: conditions.min_idle_time,
            max_idle_time_secs: conditions.max_idle_time,
        })
    }

    fn wire_fixed_conditions(
        conditions: &crate::wire::v21::common::TariffConditionsFixed,
    ) -> Option<TariffConditionsFixed> {
        Some(TariffConditionsFixed {
            start_time_of_day: conditions.start_time_of_day.map(wire_time_of_day),
            end_time_of_day: conditions.end_time_of_day.map(wire_time_of_day),
            day_of_week: conditions
                .day_of_week
                .iter()
                .flatten()
                .map(wire_weekday)
                .collect(),
            valid_from_date: wire_date(conditions.valid_from_date)?,
            valid_to_date: wire_date(conditions.valid_to_date)?,
            evse_kind: conditions.evse_kind.as_ref().map(wire_evse_kind),
            payment_brand: conditions
                .payment_brand
                .as_ref()
                .map(|brand| brand.as_str().into()),
            payment_recognition: conditions
                .payment_recognition
                .as_ref()
                .map(|kind| kind.as_str().into()),
        })
    }

    fn wire_time_of_day(time: crate::wire::OcppTimeOfDay) -> TimeOfDay {
        // Infallible: `OcppTimeOfDay` validated the `hh:mm` on the way in, so it can only hold an
        // hour under 24 and a minute under 60 - which is exactly `TimeOfDay`'s own invariant.
        TimeOfDay {
            hour: time.hour(),
            minute: time.minute(),
        }
    }

    /// `None` for an absent date; `Some(None)` never occurs, so an unrepresentable date propagates
    /// as a tariff-level rejection rather than a silently dropped restriction.
    fn wire_date(date: Option<crate::wire::OcppDate>) -> Option<Option<chrono::NaiveDate>> {
        match date {
            None => Some(None),
            Some(date) => chrono::NaiveDate::from_ymd_opt(
                i32::from(date.year()),
                u32::from(date.month()),
                u32::from(date.day()),
            )
            .map(Some),
        }
    }

    fn wire_weekday(day: &crate::wire::v21::common::DayOfWeekEnum) -> chrono::Weekday {
        use crate::wire::v21::common::DayOfWeekEnum;
        match day {
            DayOfWeekEnum::Monday => chrono::Weekday::Mon,
            DayOfWeekEnum::Tuesday => chrono::Weekday::Tue,
            DayOfWeekEnum::Wednesday => chrono::Weekday::Wed,
            DayOfWeekEnum::Thursday => chrono::Weekday::Thu,
            DayOfWeekEnum::Friday => chrono::Weekday::Fri,
            DayOfWeekEnum::Saturday => chrono::Weekday::Sat,
            DayOfWeekEnum::Sunday => chrono::Weekday::Sun,
        }
    }

    fn wire_evse_kind(kind: &crate::wire::v21::common::EvseKindEnum) -> EvseKind {
        match kind {
            crate::wire::v21::common::EvseKindEnum::AC => EvseKind::Ac,
            crate::wire::v21::common::EvseKindEnum::DC => EvseKind::Dc,
        }
    }

    fn wire_tax_rates(
        rates: Option<&heapless::Vec<crate::wire::v21::common::TaxRate, 5>>,
    ) -> Option<Vec<TaxRate>> {
        rates
            .map(|rates| {
                rates
                    .iter()
                    .map(|rate| {
                        Some(TaxRate {
                            kind: rate.r#type.as_str().into(),
                            percent: TaxPercent::from_decimal(rate.tax)?,
                            // OCPP defaults an absent `stack` to 0. A negative or absurd stack
                            // level cannot be honoured, so it invalidates the tariff rather than
                            // being clamped into a level it was not meant for.
                            stack: rate
                                .stack
                                .map_or(Some(0), |stack| u8::try_from(stack).ok())?,
                        })
                    })
                    .collect::<Option<Vec<_>>>()
            })
            .unwrap_or_else(|| Some(Vec::new()))
    }

    fn wire_energy(component: &crate::wire::v21::common::TariffEnergy) -> Option<EnergyComponent> {
        Some(EnergyComponent {
            prices: component
                .prices
                .iter()
                .map(|price| {
                    Some(EnergyPrice {
                        price_per_kwh: Money::from_decimal(price.price_kwh)?,
                        conditions: match &price.conditions {
                            None => None,
                            Some(conditions) => Some(wire_conditions(conditions)?),
                        },
                    })
                })
                .collect::<Option<Vec<_>>>()?,
            tax_rates: wire_tax_rates(component.tax_rates.as_ref())?,
        })
    }

    fn wire_time(component: &crate::wire::v21::common::TariffTime) -> Option<TimeComponent> {
        Some(TimeComponent {
            prices: component
                .prices
                .iter()
                .map(|price| {
                    Some(TimePrice {
                        price_per_minute: Money::from_decimal(price.price_minute)?,
                        conditions: match &price.conditions {
                            None => None,
                            Some(conditions) => Some(wire_conditions(conditions)?),
                        },
                    })
                })
                .collect::<Option<Vec<_>>>()?,
            tax_rates: wire_tax_rates(component.tax_rates.as_ref())?,
        })
    }

    fn wire_fixed(component: &crate::wire::v21::common::TariffFixed) -> Option<FixedComponent> {
        Some(FixedComponent {
            prices: component
                .prices
                .iter()
                .map(|price| {
                    Some(FixedPrice {
                        price: Money::from_decimal(price.price_fixed)?,
                        conditions: match &price.conditions {
                            None => None,
                            Some(conditions) => Some(wire_fixed_conditions(conditions)?),
                        },
                    })
                })
                .collect::<Option<Vec<_>>>()?,
            tax_rates: wire_tax_rates(component.tax_rates.as_ref())?,
        })
    }

    fn wire_price(price: &crate::wire::v21::common::Price) -> Option<Price> {
        let convert = |value: Option<f64>| match value {
            None => Some(None),
            Some(value) => Money::from_decimal(value).map(Some),
        };
        Some(Price {
            excl_tax: convert(price.excl_tax)?,
            incl_tax: convert(price.incl_tax)?,
        })
    }

    /// Converts a wire `TariffType` into this crate's [`Tariff`], or `None` when the structure is
    /// one this station cannot represent - which the caller answers with `Rejected` and a
    /// `reasonCode` of `InvalidValue` (I07.FR.05).
    ///
    /// The whole tariff fails rather than the offending element being dropped. A tariff is a price
    /// list a driver is charged against: a partially-parsed one would price a session by rules
    /// neither the CSMS nor the driver ever agreed to, which is worse than refusing it and
    /// falling back to the CSMS's own cost calculation.
    fn wire_tariff(tariff: &crate::wire::v21::common::Tariff) -> Option<Tariff> {
        let component = |component: &Option<crate::wire::v21::common::TariffTime>| match component {
            None => Some(None),
            Some(component) => wire_time(component).map(Some),
        };
        let fixed = |component: &Option<crate::wire::v21::common::TariffFixed>| match component {
            None => Some(None),
            Some(component) => wire_fixed(component).map(Some),
        };
        let bound = |price: &Option<crate::wire::v21::common::Price>| match price {
            None => Some(None),
            Some(price) => wire_price(price).map(Some),
        };
        Some(Tariff {
            id: TariffId(tariff.tariff_id.as_str().into()),
            currency: tariff.currency.as_str().into(),
            // Infallible since `ocpp-types` 0.2.0: `validFrom` arrives as an already-validated
            // `OcppTimestamp`, so the "unparseable becomes absent" fallback has no case left.
            valid_from: tariff.valid_from.map(Into::into),
            energy: match &tariff.energy {
                None => None,
                Some(energy) => Some(wire_energy(energy)?),
            },
            charging_time: component(&tariff.charging_time)?,
            idle_time: component(&tariff.idle_time)?,
            fixed_fee: fixed(&tariff.fixed_fee)?,
            reservation_time: component(&tariff.reservation_time)?,
            reservation_fixed: fixed(&tariff.reservation_fixed)?,
            min_cost: bound(&tariff.min_cost)?,
            max_cost: bound(&tariff.max_cost)?,
        })
    }

    async fn handle_set_default(
        actor: &ChargePointActor,
        request: &SetDefaultTariffRequest,
    ) -> Result<SetDefaultTariffResponse, OCPP2_1Error> {
        // I07.FR.06 (`UnknownEVSE`) and I07.FR.05 (`InvalidValue`) both answer `Rejected`; the
        // `statusInfo.reasonCode` is what tells the CSMS which, and OCPP marks it optional
        // precisely so a station that knows can say.
        let Some(scope) = parse_scope(request.evse_id) else {
            return Ok(rejected_set(Some(UNKNOWN_EVSE)));
        };
        let Some(tariff) = wire_tariff(&request.tariff) else {
            tracing::warn!(
                tariff_id = request.tariff.tariff_id.as_str(),
                "refusing a tariff this station cannot represent"
            );
            return Ok(rejected_set(Some(INVALID_VALUE)));
        };
        let outcome = handle_set_default_tariff(actor, scope, tariff).await;
        let status = match outcome {
            SetDefaultTariffOutcome::Accepted => TariffSetStatusEnum::Accepted,
            SetDefaultTariffOutcome::DuplicateTariffId => TariffSetStatusEnum::DuplicateTariffId,
            SetDefaultTariffOutcome::TooManyElements => TariffSetStatusEnum::TooManyElements,
            SetDefaultTariffOutcome::ConditionNotSupported => {
                TariffSetStatusEnum::ConditionNotSupported
            }
            SetDefaultTariffOutcome::UnknownEvse => return Ok(rejected_set(Some(UNKNOWN_EVSE))),
            SetDefaultTariffOutcome::Rejected => return Ok(rejected_set(None)),
        };
        Ok(SetDefaultTariffResponse {
            custom_data: None,
            status,
            status_info: None,
        })
    }

    /// OCPP's `reasonCode` for a tariff structure this station cannot represent (I07.FR.05).
    const INVALID_VALUE: &str = "InvalidValue";
    /// OCPP's `reasonCode` for an `evseId` this charge point does not have (I07.FR.06).
    const UNKNOWN_EVSE: &str = "UnknownEVSE";

    fn rejected_set(reason: Option<&str>) -> SetDefaultTariffResponse {
        SetDefaultTariffResponse {
            custom_data: None,
            status: TariffSetStatusEnum::Rejected,
            status_info: reason.and_then(|reason| {
                Some(crate::wire::v21::common::StatusInfo {
                    additional_info: None,
                    custom_data: None,
                    reason_code: heapless::String::try_from(reason).ok()?,
                })
            }),
        }
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
        let Some(tariff) = wire_tariff(&request.tariff) else {
            tracing::warn!(
                tariff_id = request.tariff.tariff_id.as_str(),
                "refusing a tariff this station cannot represent"
            );
            return Ok(ChangeTransactionTariffResponse {
                custom_data: None,
                status: TariffChangeStatusEnum::Rejected,
                status_info: None,
            });
        };
        let outcome = handle_change_transaction_tariff(actor, transaction_id, tariff).await;
        let status = match outcome {
            ChangeTransactionTariffOutcome::Accepted => TariffChangeStatusEnum::Accepted,
            ChangeTransactionTariffOutcome::TxNotFound => TariffChangeStatusEnum::TxNotFound,
            ChangeTransactionTariffOutcome::TooManyElements => {
                TariffChangeStatusEnum::TooManyElements
            }
            ChangeTransactionTariffOutcome::ConditionNotSupported => {
                TariffChangeStatusEnum::ConditionNotSupported
            }
            ChangeTransactionTariffOutcome::NoCurrencyChange => {
                TariffChangeStatusEnum::NoCurrencyChange
            }
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
            // I09.FR.04/.05/.06. `+ 1` because OCPP numbers EVSEs from 1 - the same conversion
            // `scope_to_wire` does for a scope.
            evse_ids: (!report.evse_ids.is_empty()).then(|| {
                report
                    .evse_ids
                    .iter()
                    .map(|id| scope_to_wire(TariffScope::Evse(*id)))
                    .collect()
            }),
            // The driver's own identifier, which is exactly what I09.FR.05 asks for. It leaves
            // this station only towards the CSMS that issued the tariff for it.
            id_tokens: (!report.id_tokens.is_empty()).then(|| {
                report
                    .id_tokens
                    .iter()
                    .filter_map(|token| heapless::String::try_from(token.value.as_str()).ok())
                    .collect()
            }),
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
