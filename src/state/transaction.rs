/// Identifies a transaction. Assigned by the charge point when a transaction starts
/// (`ChargePointState.next_transaction_id`, incremented monotonically).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct TransactionId(pub u64);

/// The transaction's charging state, reported via TransactionEvent (OCPP
/// `ChargingStateEnumType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TransactionChargingState {
    /// The EV is connected but energy is not (yet, or no longer) flowing.
    EvConnected,
    /// Energy is actively flowing to the EV.
    Charging,
    /// Charging is suspended by the EV.
    SuspendedEV,
    /// Charging is suspended by the EVSE.
    SuspendedEVSE,
    /// The transaction has no EV connected.
    Idle,
}

/// Which of a [`TransactionLimit`]'s four ceilings a transaction has run into
/// (`docs/OCPP-2.1-COMPLIANCE-ROADMAP.md` CV15, **E16**).
///
/// Named rather than implied, because OCPP's whole point in E16.FR.05 is that the CSMS must be
/// able to tell *why* a connector is `SuspendedEVSE`: "otherwise there is no way to know whether
/// SuspendedEVSE is caused by smart charging or by a transaction limit". Each variant maps to its
/// own `triggerReason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TransactionLimitKind {
    /// The cost ceiling - `maxCost`, the prepaid case (C17).
    Cost,
    /// The energy ceiling - `maxEnergy`, in Wh delivered by this transaction.
    Energy,
    /// The state-of-charge ceiling - `maxSoC`, as a percentage.
    Soc,
    /// The elapsed-time ceiling - `maxTime`, measured from the transaction's start (E16.FR.09).
    Time,
}

impl TransactionLimitKind {
    /// A stable, low-cardinality name for logging - see `CLAUDE.md`'s "fields over prose". The
    /// match is exhaustive with no wildcard arm on purpose: a limit kind added later must be a
    /// compile error rather than a mislabelled log line.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Cost => "cost",
            Self::Energy => "energy",
            Self::Soc => "soc",
            Self::Time => "time",
        }
    }
}

/// A ceiling on a transaction, in any combination of cost, energy, state of charge and time
/// (OCPP 2.1's `TransactionLimitType`, **E16**).
///
/// Set by the CSMS in a `TransactionEventResponse` (E16.FR.02 - the prepaid balance of C17, say),
/// or by the charge point on the driver's behalf (E16.FR.01 - "charge me 20 kWh worth", entered at
/// the station's own UI). Either way the charge point confirms it back once, in the
/// `transactionInfo.transactionLimit` of its next `TransactionEvent` with
/// `triggerReason = LimitSet`, and thereafter enforces it.
///
/// **Every field is a ceiling, and any of them being reached ends energy transfer** - "if more
/// than one limit is given, for example both a time and an energy limit, then whichever limit is
/// reached first, determines the end of energy transfer". A field left `None` is not a limit of
/// zero; it is no limit of that kind.
///
/// **A limit can be raised and lowered but never removed** (E16.FR.17): OCPP has no "unset", so a
/// party wanting to drop a ceiling sets it high enough to stop mattering. This crate takes that
/// literally - [`crate::state::ConnectorEvent::TransactionLimitSet`] replaces the whole limit, and
/// a field absent from the new one is absent from the result.
///
/// **2.1 only.** Neither 1.6J nor 2.0.1 has `TransactionLimitType`, so on those connections a
/// limit can only ever come from the charge point's own side, and is neither reported to nor
/// settable by the CSMS.
#[derive(Debug, Clone, Copy, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct TransactionLimit {
    /// The most this transaction may cost, in the currency of the tariff pricing it.
    ///
    /// Which cost it is measured against is E16.FR.15/.16's split: the charge point's own running
    /// cost when a tariff prices the session locally, and the CSMS's `totalCost`/`CostUpdated`
    /// when it does not. See [`crate::state::EvseState::running_cost`] for the first and
    /// [`crate::state::EvseState::running_costs`] for the second.
    pub max_cost: Option<f64>,
    /// The most energy this transaction may deliver, in Wh - measured from the first meter
    /// reading this transaction saw, not from the meter's absolute total.
    pub max_energy_wh: Option<f64>,
    /// The state of charge, as a percentage, at which to stop. Only meaningful where the EV
    /// reports one ([`MeterSample::soc_percent`](crate::state::MeterSample::soc_percent)), which
    /// generally means an ISO 15118 session.
    pub max_soc_percent: Option<u8>,
    /// The longest this transaction may run, in seconds **from the transaction starting** rather
    /// than from energy first flowing (E16.FR.09): the point of a time limit is usually to stop a
    /// bay being held, and depending on `TxStartPoint` a transaction may begin well before any
    /// energy moves.
    pub max_time_secs: Option<i64>,
}

impl TransactionLimit {
    /// Whether this states no ceiling at all - every field absent.
    ///
    /// Worth naming because it is the result of filtering a CSMS-sent limit down to what this
    /// build supports (E16.FR.13): a limit that survives filtering as empty must not be recorded
    /// or confirmed, since confirming it would tell the CSMS a limit was accepted when nothing
    /// was.
    pub fn is_empty(&self) -> bool {
        self.max_cost.is_none()
            && self.max_energy_wh.is_none()
            && self.max_soc_percent.is_none()
            && self.max_time_secs.is_none()
    }

    /// This limit with every field this build cannot enforce dropped - **E16.FR.13**.
    ///
    /// The station must not confirm a limit it will not act on: a CSMS that saw `maxTime` echoed
    /// back would believe the bay would free itself. Dropping it here means it is neither recorded
    /// nor confirmed, which is precisely what FR.13 asks for, and `TxCtrlr.SupportedLimits` told
    /// the CSMS to expect that.
    pub fn supported(&self) -> Self {
        let supports =
            |name: &str| crate::state::device_model::SUPPORTED_TRANSACTION_LIMITS.contains(&name);
        Self {
            max_cost: self.max_cost.filter(|_| supports("maxCost")),
            max_energy_wh: self.max_energy_wh.filter(|_| supports("maxEnergy")),
            max_soc_percent: self.max_soc_percent.filter(|_| supports("maxSoC")),
            max_time_secs: self.max_time_secs.filter(|_| supports("maxTime")),
        }
    }

    /// This limit with every field clamped to `ceiling`'s, where `ceiling` states one - **E16.FR.04**.
    ///
    /// The CSMS always has the last word. A prepaid balance the CSMS set as `maxCost` is the money
    /// on the card, and a driver entering a higher figure at the station's own UI must not raise
    /// it; the same holds for every other field. A field the CSMS never set is not a ceiling, so a
    /// locally-set value passes through untouched.
    pub fn clamped_to(&self, ceiling: &Self) -> Self {
        fn lower<T: PartialOrd>(value: Option<T>, ceiling: Option<T>) -> Option<T> {
            match (value, ceiling) {
                (Some(value), Some(ceiling)) if value > ceiling => Some(ceiling),
                (Some(value), _) => Some(value),
                (None, ceiling) => ceiling,
            }
        }
        Self {
            max_cost: lower(self.max_cost, ceiling.max_cost),
            max_energy_wh: lower(self.max_energy_wh, ceiling.max_energy_wh),
            max_soc_percent: lower(self.max_soc_percent, ceiling.max_soc_percent),
            max_time_secs: lower(self.max_time_secs, ceiling.max_time_secs),
        }
    }
}

/// Why a transaction stopped, reported via TransactionEvent (OCPP `ReasonEnumType`) - a subset
/// of the full spec enum, covering what this crate can currently detect. Extend as
/// RemoteControl/Authorization land and can supply richer reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum StopReason {
    /// Stopped locally (e.g. a stop button, or the EV/driver ending the session).
    Local,
    /// Stopped by a remote command.
    Remote,
    /// The cable was disconnected while charging.
    EVDisconnected,
    /// Stopped because a hardware fault was detected.
    EmergencyStop,
    /// Stopped because a CSMS-initiated `Reset` (`Immediate`) interrupted it. See
    /// `docs/ROADMAP.md` §2.
    Reset,
    /// The charge point lost power (or otherwise restarted) while this transaction was in
    /// flight, and it was closed out from its persisted record on the next boot rather than
    /// resumed. Never produced by a live connector transition - only by
    /// [`crate::state::ChargePointEvent::PersistedTransactionsRestored`]. See
    /// `docs/PRODUCTION-ROADMAP.md` §7.4 (E4.1).
    PowerLoss,
    /// The identifier authorizing the transaction was revoked mid-session and any allowance
    /// `TxCtrlr.MaxEnergyOnInvalidId` granted has been spent - OCPP's `DeAuthorized` (E05,
    /// `docs/OCPP-2.1-COMPLIANCE-ROADMAP.md` CV2.5).
    DeAuthorized,
}

/// A charging session tied to one connector, distinct from [`crate::state::ConnectorState`] -
/// several connector states (e.g. `Starting`, `Charging`, `Stopping`) share one active
/// transaction.
/// `PartialEq` but not `Eq` since CV15: [`TransactionLimit`]'s `maxCost` and `maxEnergy` are
/// `f64` on the wire and stay `f64` here, for the reason [`crate::state::ChargePointEffect`]
/// already is not `Eq` - rounding a limit to make a derive fit would change the limit.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Transaction {
    /// This transaction's identifier.
    pub id: TransactionId,
    /// The identifier that authorized this transaction - the one physically presented, or the
    /// one a CSMS-initiated `RequestStartTransaction` supplied. `None` only if this crate is
    /// somehow asked to report a transaction that started neither way (shouldn't happen through
    /// this crate's own state machine, but not modeled as impossible at the type level).
    pub id_token: Option<crate::state::IdToken>,
    /// The transaction's current charging state.
    pub charging_state: TransactionChargingState,
    /// Why the transaction stopped, if it has.
    pub stop_reason: Option<StopReason>,
    /// Monotonically increasing per transaction, per the OCPP TransactionEvent `seqNo` field.
    pub seq_no: u32,
    /// The most recent meter reading reported while this transaction was `Charging`, if any -
    /// see the Meter values functional block (`docs/ROADMAP.md` §10).
    pub last_meter_sample: Option<crate::state::MeterSample>,
    /// Whether priority charging has been granted for this transaction - OCPP 2.1's
    /// `UsePriorityCharging` (`docs/PRODUCTION-ROADMAP.md` B2.6).
    ///
    /// While `true`, any installed
    /// [`ChargingProfilePurpose::PriorityCharging`](crate::state::ChargingProfilePurpose::PriorityCharging)
    /// profile applies to this transaction; while `false` those profiles sit inert. The grant
    /// belongs to the transaction, not the connector, so it ends when the transaction does rather
    /// than leaking into whatever plugs in next.
    ///
    /// Always `false` under 1.6J and 2.0.1: neither version can express the purpose or the
    /// request. `#[serde(default)]` so a transaction persisted before this field existed recovers
    /// as ungranted, which is the safe reading - a recovered session should not silently keep a
    /// priority the CSMS can no longer see it holding.
    #[serde(default)]
    pub priority_charging: bool,
    /// The `remoteStartId` from the `RequestStartTransaction` that began this transaction, if it
    /// began that way (`docs/OCPP-2.1-COMPLIANCE-ROADMAP.md` CV6).
    ///
    /// OCPP requires it on the transaction's `TransactionEvent`s (F01.FR.25, F02.FR.01/.21) - it
    /// is how a CSMS correlates the transaction it now sees with the request it made, which it
    /// cannot do from the transaction id alone because the charge point chose that. Also the
    /// signal for `triggerReason = RemoteStart`: a transaction with one was started remotely, by
    /// construction.
    ///
    /// `#[serde(default)]` so a transaction persisted before this field existed recovers as
    /// locally started, which is the safe reading - inventing a correlation id the CSMS never
    /// issued would be worse than reporting none.
    #[serde(default)]
    pub remote_start_id: Option<i64>,
    /// The reservation this transaction consumed, if it started on a reserved connector
    /// (F02.FR.06, H03).
    ///
    /// OCPP expects it on the transaction's events so the CSMS can close the reservation out
    /// against the session that used it.
    #[serde(default)]
    pub reservation_id: Option<i64>,
    /// The meter reading (Wh) at which this transaction must stop because its identifier was
    /// **deauthorized mid-session** - OCPP's E05, `TxCtrlr.MaxEnergyOnInvalidId`
    /// (`docs/OCPP-2.1-COMPLIANCE-ROADMAP.md` CV2.5).
    ///
    /// `None` in the ordinary case: either the identifier is still valid, or it was revoked and
    /// `TxCtrlr.StopTxOnInvalidId` said to stop immediately.
    ///
    /// Stored as an **absolute** meter reading rather than a remaining allowance so it cannot
    /// drift: a remaining-Wh counter would have to be decremented on every sample, and a sample
    /// that is dropped, duplicated or restored from persistence would change how much free energy
    /// the driver gets. A target the meter has to reach is the same answer however the samples
    /// arrive.
    #[serde(default)]
    pub stop_at_energy_wh: Option<i64>,
    /// The ceiling this transaction is running under, already filtered to the limits this build
    /// supports (**E16**, CV15). `None` until one is set - by the CSMS in a
    /// `TransactionEventResponse`, or locally on the driver's behalf.
    #[serde(default)]
    pub limit: Option<TransactionLimit>,
    /// The last limit the *CSMS* set, kept separately from [`Self::limit`] as the ceiling a
    /// locally-set one is clamped to (**E16.FR.04**).
    ///
    /// Two fields rather than one because they answer different questions: `limit` is what is
    /// being enforced right now, this is what the CSMS will allow it to be. A driver who asks for
    /// 40 kWh on a card holding 20 kWh worth gets 20, and the CSMS's figure has to survive that
    /// to clamp the next request too.
    #[serde(default)]
    pub csms_limit: Option<TransactionLimit>,
    /// Which ceiling this transaction has run into, if any (**E16.FR.05**).
    ///
    /// `Some` means energy transfer is suspended *by the station, for this reason* - which is
    /// exactly what OCPP wants distinguishable from a smart-charging suspension. Cleared when the
    /// limit is raised past the current value, which is what resumes energy transfer
    /// (**E16.FR.14**).
    #[serde(default)]
    pub limit_reached: Option<TransactionLimitKind>,
    /// The meter reading, in Wh, this transaction started from - the baseline `maxEnergy` is
    /// measured against, since OCPP limits the energy *this transaction* delivers rather than the
    /// meter's lifetime total.
    ///
    /// `None` until the first sample arrives. Recorded here rather than only in
    /// [`crate::persistence::PersistedTransaction::meter_start`] because enforcing a limit cannot
    /// depend on an integrator having wired persistence - unlike a start *time*, a meter reading
    /// needs no clock, so the state machine can hold it without giving up being clock-free.
    #[serde(default)]
    pub energy_start_wh: Option<i64>,
}
