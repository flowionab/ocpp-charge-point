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
    Charging,
    SuspendedEV,
    SuspendedEVSE,
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
    /// Stopped because a CSMS-initiated `Reset` (`Immediate`) interrupted it. See
    /// `docs/ROADMAP.md` §2.
    Reset,
}

/// A charging session tied to one connector, distinct from [`crate::state::ConnectorState`] -
/// several connector states (e.g. `Starting`, `Charging`, `Stopping`) share one active
/// transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transaction {
    pub id: TransactionId,
    pub charging_state: TransactionChargingState,
    pub stop_reason: Option<StopReason>,
    /// Monotonically increasing per transaction, per the OCPP TransactionEvent `seqNo` field.
    pub seq_no: u32,
    /// The most recent meter reading reported while this transaction was `Charging`, if any -
    /// see the Meter values functional block (`docs/ROADMAP.md` §10).
    pub last_meter_sample: Option<crate::state::MeterSample>,
}
