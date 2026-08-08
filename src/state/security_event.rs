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
    /// The charge point discarded a renewed client certificate because it could not establish a
    /// connection using it - so it is still running on the previous one, and the CSMS's renewal
    /// did not take.
    DiscardedRenewedClientCertificate,
    /// The negotiated TLS version doesn't meet the charge point's security policy.
    InvalidTlsVersion,
    /// The negotiated TLS cipher suite doesn't meet the charge point's security policy.
    InvalidTlsCipherSuite,
    /// Someone logged in to the local maintenance interface successfully.
    ///
    /// OCPP strongly recommends `techInfo` carry the user and the origin of the attempt as
    /// `{'user': '...', 'origin': '...'}`. This crate does not construct that string, because it
    /// has no maintenance interface - the integrator raising the event is the only party that
    /// knows who logged in from where.
    MaintenanceLoginAccepted,
    /// Someone failed to log in to the local maintenance interface. Same `techInfo` recommendation
    /// as [`Self::MaintenanceLoginAccepted`].
    MaintenanceLoginFailed,
    /// A vendor-specific or not-yet-standardized event, carrying the raw OCPP `type` string.
    Other(String),
}

impl SecurityEventType {
    /// Whether OCPP classes this event as **critical**, per the `Critical` column of
    /// `docs/OCPP-2.1/Appendices_CSV_v2.1/security_events.csv`.
    ///
    /// This is not a severity hint - it decides where the event goes. OCPP's A04 splits the two
    /// destinations a security event has:
    ///
    /// - **A04.FR.01/.02**: a *critical* event is sent to the CSMS as a
    ///   `SecurityEventNotification`, queued with guaranteed delivery while offline.
    /// - **A04.FR.04**: *every* event, critical or not, is stored in the security log.
    ///
    /// Keeping non-critical events out of the notification queue is a security property rather
    /// than a tidiness one. That queue is bounded (G2.2) and drops the oldest entry when it
    /// overflows, and two of the non-critical types - [`Self::InvalidMessages`] and
    /// [`Self::AttemptedReplayAttacks`] - are exactly what an attacker can generate at will by
    /// throwing malformed frames at the charge point. If they shared the queue with critical
    /// events, that flood would evict a queued [`Self::TamperDetectionActivated`] or
    /// [`Self::InvalidFirmwareSignature`] before it ever reached the CSMS: an attacker could
    /// silence the report of their own physical intrusion. They still reach the log, which is
    /// where A04.FR.04 wants them and which is separately bounded.
    ///
    /// [`Self::Other`] is treated as **critical**. A vendor-specific event this crate has never
    /// heard of is not something to quietly downgrade - the integrator who defined it knows why
    /// they raised it, and over-reporting is the recoverable direction.
    pub fn is_critical(&self) -> bool {
        match self {
            Self::FirmwareUpdated
            | Self::SettingSystemTime
            | Self::StartupOfTheDevice
            | Self::ResetOrReboot
            | Self::SecurityLogWasCleared
            | Self::MemoryExhaustion
            | Self::TamperDetectionActivated
            | Self::InvalidFirmwareSignature
            | Self::InvalidFirmwareSigningCertificate
            | Self::InvalidCsmsCertificate
            | Self::InvalidChargingStationCertificate
            | Self::DiscardedRenewedClientCertificate
            | Self::InvalidTlsVersion
            | Self::InvalidTlsCipherSuite
            | Self::MaintenanceLoginAccepted
            | Self::MaintenanceLoginFailed
            | Self::Other(_) => true,
            Self::FailedToAuthenticateAtCsms
            | Self::CsmsFailedToAuthenticate
            | Self::ReconfigurationOfSecurityParameters
            | Self::InvalidMessages
            | Self::AttemptedReplayAttacks => false,
        }
    }
}

/// A security event occurrence.
///
/// Where it goes depends on [`SecurityEventType::is_critical`]: every event is stored in the
/// security log (A04.FR.04), and a critical one is additionally reported to the CSMS as a
/// `SecurityEventNotification` (A04.FR.01).
///
/// Raised through [`crate::security::report_security_event`] - by this crate where it can detect
/// the event itself (a clock step, a reboot, a queue overflow, a security-parameter change), and
/// by the integrator for everything only the hardware knows about (a tamper switch, a maintenance
/// login). The certificate, firmware and TLS types are declared but not yet raised anywhere,
/// because the blocks that would detect them do not exist - see `docs/PRODUCTION-ROADMAP.md`
/// B3/B4/F2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecurityEvent {
    /// Which kind of security event occurred.
    pub event_type: SecurityEventType,
    /// Additional free-text technical detail (OCPP `techInfo`).
    pub tech_info: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    /// The `Critical` column of `docs/OCPP-2.1/Appendices_CSV_v2.1/security_events.csv`,
    /// transcribed. Kept as data rather than folded into the assertions so a spec revision is a
    /// diff against the appendix rather than a hunt through match arms.
    const APPENDIX: &[(&str, bool)] = &[
        ("FirmwareUpdated", true),
        ("FailedToAuthenticateAtCsms", false),
        ("CsmsFailedToAuthenticate", false),
        ("SettingSystemTime", true),
        ("StartupOfTheDevice", true),
        ("ResetOrReboot", true),
        ("SecurityLogWasCleared", true),
        ("ReconfigurationOfSecurityParameters", false),
        ("MemoryExhaustion", true),
        ("InvalidMessages", false),
        ("AttemptedReplayAttacks", false),
        ("TamperDetectionActivated", true),
        ("InvalidFirmwareSignature", true),
        ("InvalidFirmwareSigningCertificate", true),
        ("InvalidCsmsCertificate", true),
        ("InvalidChargingStationCertificate", true),
        ("DiscardedRenewedClientCertificate", true),
        ("InvalidTLSVersion", true),
        ("InvalidTLSCipherSuite", true),
        ("MaintenanceLoginAccepted", true),
        ("MaintenanceLoginFailed", true),
    ];

    fn all_types() -> alloc::vec::Vec<SecurityEventType> {
        alloc::vec![
            SecurityEventType::FirmwareUpdated,
            SecurityEventType::FailedToAuthenticateAtCsms,
            SecurityEventType::CsmsFailedToAuthenticate,
            SecurityEventType::SettingSystemTime,
            SecurityEventType::StartupOfTheDevice,
            SecurityEventType::ResetOrReboot,
            SecurityEventType::SecurityLogWasCleared,
            SecurityEventType::ReconfigurationOfSecurityParameters,
            SecurityEventType::MemoryExhaustion,
            SecurityEventType::InvalidMessages,
            SecurityEventType::AttemptedReplayAttacks,
            SecurityEventType::TamperDetectionActivated,
            SecurityEventType::InvalidFirmwareSignature,
            SecurityEventType::InvalidFirmwareSigningCertificate,
            SecurityEventType::InvalidCsmsCertificate,
            SecurityEventType::InvalidChargingStationCertificate,
            SecurityEventType::DiscardedRenewedClientCertificate,
            SecurityEventType::InvalidTlsVersion,
            SecurityEventType::InvalidTlsCipherSuite,
            SecurityEventType::MaintenanceLoginAccepted,
            SecurityEventType::MaintenanceLoginFailed,
        ]
    }

    #[test]
    fn every_event_type_in_the_appendix_is_modelled() {
        assert_eq!(
            all_types().len(),
            APPENDIX.len(),
            "the appendix lists {} security events; a charge point that cannot name one cannot \
             report it",
            APPENDIX.len()
        );
    }

    #[test]
    fn criticality_matches_the_appendix_for_every_event() {
        for (modelled, (name, critical)) in all_types().iter().zip(APPENDIX) {
            assert_eq!(
                modelled.is_critical(),
                *critical,
                "{name} is marked Critical={critical} in the appendix"
            );
        }
    }

    #[test]
    fn the_two_remotely_triggerable_events_are_not_critical() {
        // Load-bearing, not incidental: these are the two an attacker can generate at will by
        // throwing malformed frames at the charge point. Keeping them out of the bounded
        // notification queue is what stops that flood evicting a queued tamper report.
        assert!(!SecurityEventType::InvalidMessages.is_critical());
        assert!(!SecurityEventType::AttemptedReplayAttacks.is_critical());
        assert!(SecurityEventType::TamperDetectionActivated.is_critical());
    }

    #[test]
    fn a_vendor_specific_event_is_treated_as_critical() {
        // This crate cannot know what an integrator's own event means, and silently downgrading
        // it to log-only would drop a report they deliberately raised.
        assert!(SecurityEventType::Other("VendorTamperBolt".to_string()).is_critical());
    }
}
