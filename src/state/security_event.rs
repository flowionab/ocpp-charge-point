use alloc::string::String;

/// The kind of security-relevant event reported via SecurityEventNotification (OCPP's "Security
/// events" list - an Appendix in the 2.0.1/2.1 spec, extensible for vendor-specific values).
/// Covers the standardized values; `Other` carries anything vendor-specific or not (yet) in this
/// list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecurityEventType {
    FirmwareUpdated,
    FailedToAuthenticateAtCsms,
    CsmsFailedToAuthenticate,
    SettingSystemTime,
    StartupOfTheDevice,
    ResetOrReboot,
    SecurityLogWasCleared,
    ReconfigurationOfSecurityParameters,
    MemoryExhaustion,
    InvalidMessages,
    AttemptedReplayAttacks,
    TamperDetectionActivated,
    InvalidFirmwareSignature,
    InvalidFirmwareSigningCertificate,
    InvalidCsmsCertificate,
    InvalidChargingStationCertificate,
    InvalidTlsVersion,
    InvalidTlsCipherSuite,
    /// A vendor-specific or not-yet-standardized event, carrying the raw OCPP `type` string.
    Other(String),
}

/// A security event occurrence, reported to the CSMS via SecurityEventNotification. Nothing in
/// this crate currently detects one of these autonomously (no certificate/firmware/TLS handling
/// exists yet - see `docs/ROADMAP.md` §1/§12); this is the reporting pipeline, raised via
/// [`crate::security::report_security_event`] by hardware (e.g. a tamper switch) or by future
/// functional blocks once they exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecurityEvent {
    pub event_type: SecurityEventType,
    /// Additional free-text technical detail (OCPP `techInfo`).
    pub tech_info: Option<String>,
}
