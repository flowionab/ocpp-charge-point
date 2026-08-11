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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
}
