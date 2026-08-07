//! Security functional block: reporting security-relevant events to the CSMS via
//! SecurityEventNotification. See `docs/ROADMAP.md` §1.
//!
//! This is the reporting pipeline only - nothing in this crate autonomously detects a security
//! event yet (no certificate/firmware/TLS handling exists here - that's `ocpp-client`'s and,
//! eventually, this crate's own §1/§12 work). Hardware (e.g. a tamper switch) or a future
//! functional block raises one via [`report_security_event`].

use crate::actor::ChargePointActor;
use crate::state::{ChargePointEvent, SecurityEvent, SecurityEventType};
use crate::sync::BroadcastReceiver;
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use core::cell::RefCell;
use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;

#[cfg(feature = "ocpp_2_1")]
pub use self::ocpp_2_1::Ocpp2_1SecurityEventNotifier;

/// The OCPP wire `type` string for `event_type` - the standardized values, or the raw
/// vendor-specific string for `Other`. Only 2.1 has an adapter to use this today - `ocpp-client`
/// 0.2.0 doesn't implement `SecurityEventNotification` for 2.0.1 at all (see the `ocpp_2_0_1`
/// note below) - but it's defined at this module's top level, not inside `ocpp_2_1`, since the
/// wire shape itself (a free-form string, unlike e.g. `IdToken.type`, which 2.0.1 keeps as a
/// closed enum) is not actually 2.1-specific, and a 2.0.1 adapter should reuse this the moment
/// upstream support exists rather than needing its own copy written from scratch.
#[cfg(feature = "ocpp_2_1")]
fn wire_type(event_type: &SecurityEventType) -> alloc::string::String {
    use alloc::string::ToString;
    match event_type {
        SecurityEventType::FirmwareUpdated => "FirmwareUpdated".to_string(),
        SecurityEventType::FailedToAuthenticateAtCsms => "FailedToAuthenticateAtCsms".to_string(),
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

#[cfg(all(test, feature = "ocpp_2_1"))]
mod wire_type_tests {
    use super::wire_type;
    use crate::state::SecurityEventType;

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

/// Default [`SecurityEventLog`] capacity used by [`SecurityEventLog::new`].
///
/// 50 entries keeps the log's worst-case retained memory in the low tens of KB (an entry is a
/// [`SecurityEventType`] plus an optional free-text `techInfo`, bounded by OCPP's own 255-byte
/// wire bound on that field) while still covering the recent history an operator or a CSMS
/// `GetLog` upload actually looks at after an incident. Integrators with more flash and a
/// compliance reason to keep a longer trail should pick their own via
/// [`SecurityEventLog::with_capacity`] - see `docs/PRODUCTION-ROADMAP.md` §7.2 (E2.10) and §9.2
/// (G2.2/G2.3) for the bounded-memory stance this follows.
pub const DEFAULT_SECURITY_LOG_CAPACITY: usize = 50;

/// One recorded security event, with the time it was recorded at.
///
/// `recorded_at` is stamped by the [`Clock`](crate::clock::Clock) supplied to
/// [`crate::persistence::run_security_log_persistence`] rather than carried on
/// [`SecurityEvent`] itself: the events flow through the actor, whose state machine is
/// deliberately clock-free (see [`crate::clock`]), so the timestamp is added at the log's edge -
/// the same split [`crate::persistence::PersistedTransaction::started_at`] makes.
///
/// It is `None` when the charge point had no usable time source at the moment the event was
/// recorded (see [`crate::clock::is_synchronized`]) - hardware with no RTC that hasn't yet
/// received a CSMS `currentTime`. The event is still recorded in full; only its time is honestly
/// left blank rather than fabricated as a plausible-looking 1970 stamp
/// (`docs/PRODUCTION-ROADMAP.md` §9.3, G3.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecurityLogEntry {
    /// The security event that occurred.
    pub event: SecurityEvent,
    /// When it was recorded, or `None` if the clock wasn't synchronized at the time - see the
    /// type's docs.
    pub recorded_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// The charge point's security log: a bounded, oldest-first record of the security events raised
/// on this charge point, independent of whether each one reached the CSMS.
///
/// # Why this exists alongside the offline security-event queue
///
/// [`crate::offline_queue::OfflineQueue`] holds security events *pending delivery* and drops each
/// one the moment the CSMS accepts it - it is a delivery buffer, not a history. OCPP's
/// `SecurityLogWasCleared` event, and `GetLog`'s `SecurityLog` upload, both presuppose a log that
/// outlives delivery and outlives a reboot: clearing something that was never kept is not a
/// meaningful event to report. This is that log (`docs/PRODUCTION-ROADMAP.md` §7.2, E2.10 - the
/// durability half of §8.4's F4.3; the `GetLog` reader itself is still to come, see B5.1).
///
/// # Bounds and overflow
///
/// Bounded at construction (see [`DEFAULT_SECURITY_LOG_CAPACITY`] / [`Self::with_capacity`]), so
/// an event storm - a stuck tamper switch, a CSMS that keeps failing authentication - grows the
/// log only up to that bound rather than until allocation fails (G2.2). Overflow always evicts the
/// **oldest** entry; unlike [`crate::offline_queue::OfflineQueue`] there is no policy choice here,
/// because a log's newest entries are the ones an incident investigation starts from, and the
/// alternative (refusing new entries once full) would freeze the log at the first storm and record
/// nothing about whatever came after it. The evicted entry is handed back from [`Self::record`] so
/// the caller can log that an audit-trail entry was lost.
///
/// Durability is a separate concern layered on top - see
/// [`crate::persistence::SecurityLogStore`]. This type is pure in-memory state and has no
/// [`crate::hardware::Storage`] dependency of its own, so a charge point without durable storage
/// still gets an in-RAM log for the life of the process.
///
/// Cheap to share: put it behind an [`alloc::sync::Arc`] and hand clones to the persistence task
/// and to whatever reads the log; it is internally synchronized by the same
/// `embassy-sync`-backed blocking mutex the rest of this crate's no_std-safe shared state uses.
pub struct SecurityEventLog {
    entries: BlockingMutex<CriticalSectionRawMutex, RefCell<VecDeque<SecurityLogEntry>>>,
    capacity: usize,
}

impl SecurityEventLog {
    /// An empty log holding at most [`DEFAULT_SECURITY_LOG_CAPACITY`] entries.
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_SECURITY_LOG_CAPACITY)
    }

    /// An empty log holding at most `capacity` entries (clamped to at least 1 - a log that can
    /// hold nothing would discard every event immediately, which is indistinguishable from having
    /// no log at all while still costing every write).
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: BlockingMutex::new(RefCell::new(VecDeque::new())),
            capacity: capacity.max(1),
        }
    }

    /// The maximum number of entries this log retains - see [`Self::with_capacity`].
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Appends `entry`, evicting and returning the oldest entry if the log is already at capacity
    /// (see the type's docs for why eviction is unconditional). Returns `None` when nothing was
    /// evicted.
    pub fn record(&self, entry: SecurityLogEntry) -> Option<SecurityLogEntry> {
        self.entries.lock(|entries| {
            let mut entries = entries.borrow_mut();
            let evicted = if entries.len() >= self.capacity {
                entries.pop_front()
            } else {
                None
            };
            entries.push_back(entry);
            evicted
        })
    }

    /// Every retained entry, oldest first - the order an operator reads a log in, and the order
    /// [`crate::persistence::SecurityLogStore`] snapshots and restores it in.
    pub fn entries(&self) -> alloc::vec::Vec<SecurityLogEntry> {
        self.entries
            .lock(|entries| entries.borrow().iter().cloned().collect())
    }

    /// How many entries are currently retained.
    pub fn len(&self) -> usize {
        self.entries.lock(|entries| entries.borrow().len())
    }

    /// Whether the log currently holds nothing.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Discards every entry, returning how many were discarded.
    ///
    /// This is the in-memory half only: clearing the log durably - and reporting the OCPP
    /// `SecurityLogWasCleared` event that makes the clearing itself auditable - is
    /// [`crate::persistence::clear_security_log`], which is what callers should use.
    pub fn clear(&self) -> usize {
        self.entries.lock(|entries| {
            let mut entries = entries.borrow_mut();
            let discarded = entries.len();
            entries.clear();
            discarded
        })
    }

    /// Appends every entry from `entries`, oldest first, through [`Self::record`] - so a log
    /// restored from durable storage after a reboot respects this log's capacity exactly as if the
    /// entries had been recorded one at a time while running. Returns how many entries the
    /// capacity bound dropped (non-zero only if the persisted log is larger than this log's
    /// capacity, e.g. because the capacity was lowered since the snapshot was written).
    pub fn restore(&self, entries: alloc::vec::Vec<SecurityLogEntry>) -> usize {
        entries.into_iter().fold(0, |dropped, entry| {
            dropped + usize::from(self.record(entry).is_some())
        })
    }
}

impl Default for SecurityEventLog {
    fn default() -> Self {
        Self::new()
    }
}

/// Reports a security event to the CSMS via SecurityEventNotification. Implemented per protocol
/// version (see the `ocpp_2_1` module), mirroring [`crate::availability::StatusNotifier`].
#[async_trait::async_trait]
pub trait SecurityEventNotifier {
    /// The error type returned if the SecurityEventNotification request itself fails.
    type Error: core::error::Error + Send + Sync + 'static;

    /// Reports `event_type` (with optional `tech_info`) to the CSMS.
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
/// CSMS-facing report failed and is not retried. [`setup`](crate::setup) uses
/// [`crate::offline_queue::run_with_offline_queue`] instead, which queues and retries a failed
/// report rather than just logging it; this simpler fire-and-forget version remains for callers
/// who don't need retry.
pub async fn run_security_events<N: SecurityEventNotifier>(
    mut events: BroadcastReceiver<SecurityEvent>,
    notifier: &N,
) {
    while let Ok(event) = events.recv().await {
        if let Err(err) = notifier
            .notify_security_event(&event.event_type, event.tech_info.as_deref())
            .await
        {
            tracing::warn!(error = %err, "security event notification failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{SecurityEventNotifier, run_security_events};
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
mod log_tests {
    use super::{DEFAULT_SECURITY_LOG_CAPACITY, SecurityEventLog, SecurityLogEntry};
    use crate::state::{SecurityEvent, SecurityEventType};
    use chrono::{DateTime, Utc};

    fn entry(tech_info: &str) -> SecurityLogEntry {
        SecurityLogEntry {
            event: SecurityEvent {
                event_type: SecurityEventType::TamperDetectionActivated,
                tech_info: Some(tech_info.into()),
            },
            recorded_at: DateTime::<Utc>::from_timestamp(1_800_000_000, 0),
        }
    }

    fn tech_infos(log: &SecurityEventLog) -> alloc::vec::Vec<alloc::string::String> {
        log.entries()
            .into_iter()
            .filter_map(|entry| entry.event.tech_info)
            .collect()
    }

    #[test]
    fn a_new_log_is_empty_and_uses_the_default_capacity() {
        let log = SecurityEventLog::new();
        assert!(log.is_empty());
        assert_eq!(log.len(), 0);
        assert_eq!(log.capacity(), DEFAULT_SECURITY_LOG_CAPACITY);
        assert_eq!(log.entries(), alloc::vec![]);
    }

    #[test]
    fn entries_are_returned_oldest_first() {
        let log = SecurityEventLog::new();
        assert_eq!(log.record(entry("first")), None);
        assert_eq!(log.record(entry("second")), None);
        assert_eq!(tech_infos(&log), alloc::vec!["first", "second"]);
    }

    #[test]
    fn recording_past_capacity_evicts_the_oldest_entry() {
        let log = SecurityEventLog::with_capacity(2);
        log.record(entry("first"));
        log.record(entry("second"));
        // Full at [first, second]: the third entry evicts the first, which is handed back so the
        // caller can log that an audit-trail entry was lost.
        assert_eq!(log.record(entry("third")), Some(entry("first")));
        assert_eq!(tech_infos(&log), alloc::vec!["second", "third"]);
        assert_eq!(log.len(), 2);
    }

    #[test]
    fn capacity_is_clamped_to_at_least_one() {
        let log = SecurityEventLog::with_capacity(0);
        assert_eq!(log.capacity(), 1);
        log.record(entry("first"));
        assert_eq!(log.record(entry("second")), Some(entry("first")));
        assert_eq!(tech_infos(&log), alloc::vec!["second"]);
    }

    #[test]
    fn clearing_empties_the_log_and_reports_how_many_entries_were_discarded() {
        let log = SecurityEventLog::new();
        log.record(entry("first"));
        log.record(entry("second"));
        assert_eq!(log.clear(), 2);
        assert!(log.is_empty());
        // Clearing an already-empty log is not an error and discards nothing.
        assert_eq!(log.clear(), 0);
    }

    #[test]
    fn restoring_respects_capacity_exactly_as_live_recording_would() {
        let log = SecurityEventLog::with_capacity(2);
        let dropped = log.restore(alloc::vec![entry("first"), entry("second"), entry("third")]);
        assert_eq!(dropped, 1);
        assert_eq!(tech_infos(&log), alloc::vec!["second", "third"]);
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
    pub use with_clock::Ocpp2_1SecurityEventNotifier;

    // `SecurityEventNotificationRequest` needs a timestamp, produced from a caller-supplied
    // `Clock` - see `crate::clock::is_synchronized`'s docs for the policy on an unsynchronized
    // reading (send it as-is, warn, never fabricate or drop it).
    mod with_clock {
        use crate::clock::{Clock, is_synchronized};
        use crate::security::SecurityEventNotifier;
        use crate::security::wire_type;
        use crate::state::SecurityEventType;
        use alloc::boxed::Box;
        use ocpp_client::ClientError;
        use ocpp_client::ocpp_2_1::{OCPP2_1Client, OCPP2_1Error};
        use ocpp_client::ocpp_types::v21::SecurityEventNotificationRequest;

        /// Builds the `SecurityEventNotificationRequest` for `event_type`/`tech_info`,
        /// timestamped from `clock.now()`. Pure (no network I/O), so a fixed [`Clock`] fake can
        /// assert the exact timestamp that reaches the wire request.
        fn build_security_event_notification_request<C: Clock>(
            clock: &C,
            event_type: &SecurityEventType,
            tech_info: Option<&str>,
        ) -> SecurityEventNotificationRequest {
            let now = clock.now();
            if !is_synchronized(&now) {
                tracing::warn!(
                    timestamp = %now,
                    "SecurityEventNotification timestamp sourced from an unsynchronized clock"
                );
            }
            SecurityEventNotificationRequest {
                custom_data: None,
                // Silently dropped if it doesn't fit OCPP's 255-byte bound - the caller
                // supplied `techInfo` is free-form technical detail, not something we can
                // truncate safely mid-UTF-8, and dropping it still delivers the (bounded,
                // always-fitting) `type` below.
                tech_info: tech_info.and_then(|info| heapless::String::try_from(info).ok()),
                timestamp: now.to_rfc3339(),
                // Falls back to a fixed literal if a vendor-supplied `Other` string exceeds
                // OCPP's 50-byte bound - every standardized value fits by construction.
                r#type: heapless::String::try_from(wire_type(event_type).as_str())
                    .unwrap_or_else(|_| heapless::String::try_from("Other").unwrap()),
            }
        }

        /// Wraps an `OCPP2_1Client` with a caller-supplied [`Clock`] for the
        /// SecurityEventNotification timestamp - the no_std-reachable counterpart to this
        /// module's `std`-only `OCPP2_1Client` impl below, which exists alongside this (not
        /// instead of it) so no existing `std` caller needs a source change.
        pub struct Ocpp2_1SecurityEventNotifier<C> {
            client: OCPP2_1Client,
            clock: C,
        }

        impl<C: Clock> Ocpp2_1SecurityEventNotifier<C> {
            /// Wraps `client`, sourcing every SecurityEventNotification timestamp from `clock`.
            pub fn with_clock(client: OCPP2_1Client, clock: C) -> Self {
                Self { client, clock }
            }
        }

        #[cfg(feature = "std")]
        impl Ocpp2_1SecurityEventNotifier<crate::clock::SystemClock> {
            /// Wraps `client`, sourcing every SecurityEventNotification timestamp from
            /// [`crate::clock::SystemClock`].
            pub fn new(client: OCPP2_1Client) -> Self {
                Self::with_clock(client, crate::clock::SystemClock)
            }
        }

        #[async_trait::async_trait]
        impl<C: Clock + Send + Sync> SecurityEventNotifier for Ocpp2_1SecurityEventNotifier<C> {
            type Error = ClientError<OCPP2_1Error>;

            async fn notify_security_event(
                &self,
                event_type: &SecurityEventType,
                tech_info: Option<&str>,
            ) -> Result<(), Self::Error> {
                let request =
                    build_security_event_notification_request(&self.clock, event_type, tech_info);
                self.client
                    .send_security_event_notification(request)
                    .await?;
                Ok(())
            }
        }

        /// The `std` convenience: `OCPP2_1Client` itself implements `SecurityEventNotifier`
        /// directly (sourcing its timestamp from [`crate::clock::SystemClock`]) so existing
        /// callers that pass a bare client - e.g. [`crate::connect::connect_and_setup`] - need no
        /// source change.
        #[cfg(feature = "std")]
        #[async_trait::async_trait]
        impl SecurityEventNotifier for OCPP2_1Client {
            type Error = ClientError<OCPP2_1Error>;

            async fn notify_security_event(
                &self,
                event_type: &SecurityEventType,
                tech_info: Option<&str>,
            ) -> Result<(), Self::Error> {
                let request = build_security_event_notification_request(
                    &crate::clock::SystemClock,
                    event_type,
                    tech_info,
                );
                self.send_security_event_notification(request).await?;
                Ok(())
            }
        }

        #[cfg(test)]
        mod tests {
            use super::*;
            use chrono::{DateTime, Utc};

            /// A [`Clock`] that always reads a fixed, caller-chosen instant.
            struct FixedClock(DateTime<Utc>);

            impl Clock for FixedClock {
                fn now(&self) -> DateTime<Utc> {
                    self.0
                }
            }

            #[test]
            fn a_fixed_clocks_reading_reaches_the_wire_request_timestamp() {
                let fixed = DateTime::parse_from_rfc3339("2026-03-04T05:06:07Z")
                    .unwrap()
                    .with_timezone(&Utc);
                let clock = FixedClock(fixed);

                let request = build_security_event_notification_request(
                    &clock,
                    &SecurityEventType::TamperDetectionActivated,
                    None,
                );

                assert_eq!(request.timestamp, fixed.to_rfc3339());
            }

            #[test]
            fn an_unsynchronized_clocks_reading_is_still_sent_not_substituted_or_dropped() {
                let unset_rtc = FixedClock(DateTime::<Utc>::from_timestamp(0, 0).unwrap());
                assert!(!is_synchronized(&unset_rtc.now()));

                let request = build_security_event_notification_request(
                    &unset_rtc,
                    &SecurityEventType::TamperDetectionActivated,
                    None,
                );

                assert_eq!(request.timestamp, unset_rtc.0.to_rfc3339());
            }
        }
    }
}

// No `ocpp_2_0_1` module here (unlike every other functional block ported to 2.0.1 so far):
// `ocpp-client` 0.2.0 simply doesn't implement `SecurityEventNotification` for
// `OCPP2_0_1Client` at all - verified directly against the pinned dependency (grepped its full
// `ocpp_2_0_1::actions` list: 66 actions covering every other 2.0.1 message including all of
// Provisioning/Availability/Transactions/Remote control/Reservation/Local auth list, but no
// `SecurityEventNotification`), not assumed from the absence of a `send_security_event_
// notification` method alone. This is a real upstream gap this crate can't work around per
// `CLAUDE.md`'s "delegate wire-protocol concerns to `ocpp-client`" - there is no `Action` type or
// `send_*`/`on_*` method to call. See `docs/ROADMAP.md` §1.
