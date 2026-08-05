//! Security functional block: reporting security-relevant events to the CSMS via
//! SecurityEventNotification. See `docs/ROADMAP.md` §1.
//!
//! This is the reporting pipeline only - nothing in this crate autonomously detects a security
//! event yet (no certificate/firmware/TLS handling exists here - that's `ocpp-client`'s and,
//! eventually, this crate's own §1/§12 work). Hardware (e.g. a tamper switch) or a future
//! functional block raises one via [`report_security_event`].

use crate::actor::ChargePointActor;
use crate::state::{ChargePointEvent, SecurityEvent, SecurityEventType};
use crate::sync::{BroadcastReceiver, RecvError};
use alloc::boxed::Box;

/// Reports a security event to the CSMS via SecurityEventNotification. Implemented per protocol
/// version (see the `ocpp_2_1` module), mirroring [`crate::availability::StatusNotifier`].
#[async_trait::async_trait]
pub trait SecurityEventNotifier {
    type Error: core::error::Error + Send + Sync + 'static;

    async fn notify_security_event(
        &self,
        event_type: &SecurityEventType,
        tech_info: Option<&str>,
    ) -> Result<(), Self::Error>;
}

/// Records that `event` occurred, feeding it into the actor so the Security functional block
/// reports it via SecurityEventNotification. The one entry point for raising a security event -
/// hardware and future functional blocks (certificate handling, firmware updates) should call
/// this rather than constructing a `ChargePointEvent::SecurityEventOccurred` by hand.
pub async fn report_security_event(actor: &ChargePointActor, event: SecurityEvent) {
    let _ = actor
        .send(ChargePointEvent::SecurityEventOccurred(event))
        .await;
}

/// Forwards every security event received on `events` to the CSMS via `notifier`, forever.
/// Errors are logged and do not stop the loop - the actor already recorded the event; only the
/// CSMS-facing report failed.
pub async fn run_security_events<N: SecurityEventNotifier>(
    mut events: BroadcastReceiver<SecurityEvent>,
    notifier: &N,
) {
    loop {
        match events.recv().await {
            Ok(event) => {
                if let Err(err) = notifier
                    .notify_security_event(&event.event_type, event.tech_info.as_deref())
                    .await
                {
                    tracing::warn!(error = %err, "security event notification failed");
                }
            }
            Err(RecvError::Closed) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{run_security_events, SecurityEventNotifier};
    use crate::state::{SecurityEvent, SecurityEventType};
    use crate::sync::broadcast_channel;
    use alloc::boxed::Box;
    use alloc::string::String;
    use alloc::vec::Vec;
    use tokio::sync::watch;

    struct RecordingSecurityEventNotifier {
        seen: watch::Sender<Vec<(SecurityEventType, Option<String>)>>,
    }

    #[async_trait::async_trait]
    impl SecurityEventNotifier for RecordingSecurityEventNotifier {
        type Error = core::convert::Infallible;

        async fn notify_security_event(
            &self,
            event_type: &SecurityEventType,
            tech_info: Option<&str>,
        ) -> Result<(), Self::Error> {
            self.seen
                .send_modify(|seen| seen.push((event_type.clone(), tech_info.map(Into::into))));
            Ok(())
        }
    }

    #[tokio::test]
    async fn forwards_every_security_event_to_the_notifier_in_order() {
        let sender = broadcast_channel();
        let receiver = sender.subscribe();
        let (seen_tx, mut seen_rx) = watch::channel(Vec::new());
        let notifier = RecordingSecurityEventNotifier { seen: seen_tx };

        let forwarder = tokio::spawn(async move {
            run_security_events(receiver, &notifier).await;
        });

        sender.send(SecurityEvent {
            event_type: SecurityEventType::TamperDetectionActivated,
            tech_info: Some("case opened".into()),
        });

        seen_rx
            .wait_for(|seen| !seen.is_empty())
            .await
            .expect("notifier task is still running");

        // Dropping the sender closes the channel, which ends `run_security_events`'s loop.
        drop(sender);
        forwarder.await.unwrap();

        assert_eq!(
            *seen_rx.borrow(),
            alloc::vec![(
                SecurityEventType::TamperDetectionActivated,
                Some(String::from("case opened"))
            )]
        );
    }
}

#[cfg(test)]
mod report_tests {
    use super::report_security_event;
    use crate::actor::ChargePointActor;
    use crate::executor::TokioExecutor;
    use crate::state::{SecurityEvent, SecurityEventType};

    #[tokio::test]
    async fn reporting_an_event_broadcasts_it_to_subscribers() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let mut events = actor.subscribe_security_events();

        report_security_event(
            &actor,
            SecurityEvent {
                event_type: SecurityEventType::TamperDetectionActivated,
                tech_info: Some("case opened".into()),
            },
        )
        .await;

        let received = events.recv().await.unwrap();
        assert_eq!(
            received.event_type,
            SecurityEventType::TamperDetectionActivated
        );
        assert_eq!(received.tech_info.as_deref(), Some("case opened"));
    }
}

#[cfg(feature = "ocpp_2_1")]
mod ocpp_2_1 {
    use crate::state::SecurityEventType;
    use alloc::string::{String, ToString};

    /// The OCPP wire `type` string for `event_type` - the standardized values, or the raw
    /// vendor-specific string for `Other`.
    pub(super) fn wire_type(event_type: &SecurityEventType) -> String {
        match event_type {
            SecurityEventType::FirmwareUpdated => "FirmwareUpdated".to_string(),
            SecurityEventType::FailedToAuthenticateAtCsms => {
                "FailedToAuthenticateAtCsms".to_string()
            }
            SecurityEventType::CsmsFailedToAuthenticate => "CSMSFailedToAuthenticate".to_string(),
            SecurityEventType::SettingSystemTime => "SettingSystemTime".to_string(),
            SecurityEventType::StartupOfTheDevice => "StartupOfTheDevice".to_string(),
            SecurityEventType::ResetOrReboot => "ResetOrReboot".to_string(),
            SecurityEventType::SecurityLogWasCleared => "SecurityLogWasCleared".to_string(),
            SecurityEventType::ReconfigurationOfSecurityParameters => {
                "ReconfigurationOfSecurityParameters".to_string()
            }
            SecurityEventType::MemoryExhaustion => "MemoryExhaustion".to_string(),
            SecurityEventType::InvalidMessages => "InvalidMessages".to_string(),
            SecurityEventType::AttemptedReplayAttacks => "AttemptedReplayAttacks".to_string(),
            SecurityEventType::TamperDetectionActivated => "TamperDetectionActivated".to_string(),
            SecurityEventType::InvalidFirmwareSignature => "InvalidFirmwareSignature".to_string(),
            SecurityEventType::InvalidFirmwareSigningCertificate => {
                "InvalidFirmwareSigningCertificate".to_string()
            }
            SecurityEventType::InvalidCsmsCertificate => "InvalidCSMSCertificate".to_string(),
            SecurityEventType::InvalidChargingStationCertificate => {
                "InvalidChargingStationCertificate".to_string()
            }
            SecurityEventType::InvalidTlsVersion => "InvalidTLSVersion".to_string(),
            SecurityEventType::InvalidTlsCipherSuite => "InvalidTLSCipherSuite".to_string(),
            SecurityEventType::Other(value) => value.clone(),
        }
    }

    // `SecurityEventNotificationRequest` needs a timestamp; producing one without a
    // caller-supplied `Clock` requires the `std`-only `SystemClock` (see `crate::clock`), so
    // this impl - unlike `wire_type` above - needs both `ocpp_2_1` and `std`.
    #[cfg(feature = "std")]
    mod with_system_clock {
        use super::wire_type;
        use crate::clock::{Clock, SystemClock};
        use crate::security::SecurityEventNotifier;
        use crate::state::SecurityEventType;
        use alloc::boxed::Box;
        use ocpp_client::ocpp_2_1::{OCPP2_1Client, OCPP2_1Error};
        use ocpp_client::ocpp_types::v21::SecurityEventNotificationRequest;
        use ocpp_client::ClientError;

        #[async_trait::async_trait]
        impl SecurityEventNotifier for OCPP2_1Client {
            type Error = ClientError<OCPP2_1Error>;

            async fn notify_security_event(
                &self,
                event_type: &SecurityEventType,
                tech_info: Option<&str>,
            ) -> Result<(), Self::Error> {
                self.send_security_event_notification(SecurityEventNotificationRequest {
                    custom_data: None,
                    // Silently dropped if it doesn't fit OCPP's 255-byte bound - the caller
                    // supplied `techInfo` is free-form technical detail, not something we can
                    // truncate safely mid-UTF-8, and dropping it still delivers the (bounded,
                    // always-fitting) `type` below.
                    tech_info: tech_info.and_then(|info| heapless::String::try_from(info).ok()),
                    timestamp: SystemClock.now().to_rfc3339(),
                    // Falls back to a fixed literal if a vendor-supplied `Other` string exceeds
                    // OCPP's 50-byte bound - every standardized value fits by construction.
                    r#type: heapless::String::try_from(wire_type(event_type).as_str())
                        .unwrap_or_else(|_| heapless::String::try_from("Other").unwrap()),
                })
                .await?;
                Ok(())
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn every_standardized_value_maps_to_its_wire_type_string() {
            assert_eq!(
                wire_type(&SecurityEventType::FirmwareUpdated),
                "FirmwareUpdated"
            );
            assert_eq!(
                wire_type(&SecurityEventType::FailedToAuthenticateAtCsms),
                "FailedToAuthenticateAtCsms"
            );
            assert_eq!(
                wire_type(&SecurityEventType::CsmsFailedToAuthenticate),
                "CSMSFailedToAuthenticate"
            );
            assert_eq!(
                wire_type(&SecurityEventType::TamperDetectionActivated),
                "TamperDetectionActivated"
            );
            assert_eq!(
                wire_type(&SecurityEventType::InvalidCsmsCertificate),
                "InvalidCSMSCertificate"
            );
            assert_eq!(
                wire_type(&SecurityEventType::InvalidTlsVersion),
                "InvalidTLSVersion"
            );
        }

        #[test]
        fn other_carries_its_raw_string_through() {
            assert_eq!(
                wire_type(&SecurityEventType::Other("VendorSpecificThing".into())),
                "VendorSpecificThing"
            );
        }

        #[test]
        fn every_standardized_value_fits_the_fifty_byte_wire_bound() {
            let all = [
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
                SecurityEventType::InvalidTlsVersion,
                SecurityEventType::InvalidTlsCipherSuite,
            ];
            for event_type in all {
                assert!(wire_type(&event_type).len() <= 50);
            }
        }
    }
}
