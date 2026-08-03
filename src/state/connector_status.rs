/// The connector status reported to the CSMS via StatusNotification (OCPP
/// `ConnectorStatusEnumType`) - a coarser, version-agnostic view of [`crate::state::ConnectorState`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectorStatus {
    Available,
    Occupied,
    Reserved,
    Unavailable,
    Faulted,
}
