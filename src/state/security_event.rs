use alloc::string::String;

/// The kind of security-relevant event reported via SecurityEventNotification (OCPP's "Security
/// events" list - an Appendix in the 2.0.1/2.1 spec, extensible for vendor-specific values).
/// Covers the standardized values; `Other` carries anything vendor-specific or not (yet) in this
/// list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecurityEventType {
    /// The charge point's firmware was successfully updated.
    FirmwareUpdated,
    /// The charge point failed to authenticate itself to the CSMS (e.g. an invalid client
    /// certificate or credentials).
    FailedToAuthenticateAtCsms,
    /// The charge point failed to authenticate the CSMS (e.g. an invalid server certificate).
    CsmsFailedToAuthenticate,
    /// The charge point's system clock was set or adjusted.
    SettingSystemTime,
    /// The charge point (re)started.
    StartupOfTheDevice,
    /// The charge point was reset or rebooted.
    ResetOrReboot,
    /// The security log was cleared.
    SecurityLogWasCleared,
    /// A security-relevant configuration parameter was changed.
    ReconfigurationOfSecurityParameters,
    /// The charge point is running low on memory.
    MemoryExhaustion,
    /// A malformed or otherwise invalid message was received.
    InvalidMessages,
    /// A replay attack was detected and rejected.
    AttemptedReplayAttacks,
    /// Physical tampering with the charge point was detected.
    TamperDetectionActivated,
    /// A firmware image's signature failed verification.
    InvalidFirmwareSignature,
    /// The certificate used to sign a firmware image is invalid.
    InvalidFirmwareSigningCertificate,
    /// The CSMS's certificate is invalid.
    InvalidCsmsCertificate,
    /// The charge point's own certificate is invalid.
    InvalidChargingStationCertificate,
    /// The negotiated TLS version doesn't meet the charge point's security policy.
    InvalidTlsVersion,
    /// The negotiated TLS cipher suite doesn't meet the charge point's security policy.
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
    /// Which kind of security event occurred.
    pub event_type: SecurityEventType,
    /// Additional free-text technical detail (OCPP `techInfo`).
    pub tech_info: Option<String>,
}
