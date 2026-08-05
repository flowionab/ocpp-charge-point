/// The connector status reported to the CSMS via StatusNotification (OCPP
/// `ConnectorStatusEnumType`) - a coarser, version-agnostic view of [`crate::state::ConnectorState`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectorStatus {
    /// No cable connected, not reserved, no fault.
    Available,
    /// A cable is connected, or a session (authorizing/starting/charging/stopping) is in
    /// progress.
    Occupied,
    /// Reserved for a specific id token (OCPP `ReserveNow`).
    Reserved,
    /// Made unavailable (OCPP `ChangeAvailability`).
    Unavailable,
    /// A hardware fault is active.
    Faulted,
}
