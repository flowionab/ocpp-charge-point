/// Identifies a transaction. Assigned by the charge point when a transaction starts
/// (`ChargePointState.next_transaction_id`, incremented monotonically).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TransactionId(pub u64);

/// The transaction's charging state, reported via TransactionEvent (OCPP
/// `ChargingStateEnumType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// Stopped locally (e.g. a stop button, or the EV/driver ending the session).
    Local,
    /// Stopped by a remote command.
    Remote,
    /// The cable was disconnected while charging.
    EVDisconnected,
    /// Stopped because a hardware fault was detected.
    EmergencyStop,
}

/// A charging session tied to one connector, distinct from [`crate::state::ConnectorState`] -
/// several connector states (e.g. `Starting`, `Charging`, `Stopping`) share one active
/// transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
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
}
