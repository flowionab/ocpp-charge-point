//! Tests for the certificate-management block's protocol-agnostic handlers (B4.2/B4.3).

use super::*;
use crate::executor::TokioExecutor;
use crate::hardware::{
    Capabilities, HashAlgorithm, InMemoryStorage, NoCertificateStore, StoredCertificates,
};
use crate::state::ChargePointEvent;
use alloc::boxed::Box;
use alloc::string::ToString;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

fn hash(serial: &str) -> CertificateHashData {
    CertificateHashData {
        hash_algorithm: HashAlgorithm::Sha256,
        issuer_name_hash: "aa".into(),
        issuer_key_hash: "bb".into(),
        serial_number: serial.into(),
    }
}

async fn actor_with_certificates() -> ChargePointActor {
    let actor = ChargePointActor::spawn([1], &TokioExecutor);
    let _ = actor
        .send(ChargePointEvent::CapabilitiesDeclared(Capabilities {
            certificate_management: true,
            ..Capabilities::default()
        }))
        .await;
    actor
}

fn store() -> StoredCertificates<Arc<InMemoryStorage>> {
    StoredCertificates::new(Arc::new(InMemoryStorage::new()))
}

#[tokio::test]
async fn a_certificate_the_charge_point_issues_is_never_installed_by_a_csms() {
    let actor = actor_with_certificates().await;

    // It would arrive by `CertificateSigned` in answer to a CSR. Accepting it here means
    // accepting a certificate for a key pair this charge point may not hold.
    for use_for in [
        CertificateUse::ChargingStation,
        CertificateUse::V2gCertificateChain,
    ] {
        assert_eq!(
            handle_install_certificate(&actor, &store(), use_for, "-----BEGIN-----").await,
            InstallCertificateOutcome::Rejected
        );
    }
}

#[tokio::test]
async fn installing_and_listing_go_through_the_store() {
    let actor = actor_with_certificates().await;
    let store = store();
    // Through `install_with_hash`, which is what an integrator that can parse X.509 calls; the
    // bare trait method has no hashes to work from.
    store
        .install_with_hash(CertificateUse::CsmsRoot, "root", hash("01"))
        .await;

    let listed = handle_get_installed_certificate_ids(&actor, &store, &[]).await;

    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].use_for, CertificateUse::CsmsRoot);
}

#[tokio::test]
async fn listing_narrows_to_the_uses_the_csms_named() {
    let actor = actor_with_certificates().await;
    let store = store();
    store
        .install_with_hash(CertificateUse::CsmsRoot, "a", hash("01"))
        .await;
    store
        .install_with_hash(CertificateUse::V2gRoot, "b", hash("02"))
        .await;

    let v2g =
        handle_get_installed_certificate_ids(&actor, &store, &[CertificateUse::V2gRoot]).await;

    assert_eq!(v2g.len(), 1);
    assert_eq!(v2g[0].use_for, CertificateUse::V2gRoot);
    // Absent means all, the same rule `GetChargingProfiles` follows.
    assert_eq!(
        handle_get_installed_certificate_ids(&actor, &store, &[])
            .await
            .len(),
        2
    );
}

#[tokio::test]
async fn deleting_reaches_the_store_and_reports_what_happened() {
    let actor = actor_with_certificates().await;
    let store = store();
    store
        .install_with_hash(CertificateUse::CsmsRoot, "root", hash("01"))
        .await;

    assert_eq!(
        handle_delete_certificate(&actor, &store, &hash("01")).await,
        DeleteCertificateOutcome::Accepted
    );
    // Gone, and asking again is not an error - the CSMS wanted it gone and it is.
    assert_eq!(
        handle_delete_certificate(&actor, &store, &hash("01")).await,
        DeleteCertificateOutcome::NotFound
    );
}

#[tokio::test]
async fn a_charge_point_without_the_capability_refuses_in_the_right_shape_per_message() {
    let actor = ChargePointActor::spawn([1], &TokioExecutor);
    let _ = actor
        .send(ChargePointEvent::CapabilitiesDeclared(
            Capabilities::default(),
        ))
        .await;

    assert_eq!(
        handle_install_certificate(&actor, &store(), CertificateUse::CsmsRoot, "x").await,
        InstallCertificateOutcome::Rejected
    );
    // `NotFound` rather than `Failed`: a charge point with no store genuinely does not have the
    // certificate, and `Failed` would send an operator looking for a fault.
    assert_eq!(
        handle_delete_certificate(&actor, &store(), &hash("01")).await,
        DeleteCertificateOutcome::NotFound
    );
    assert!(
        handle_get_installed_certificate_ids(&actor, &store(), &[])
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn the_default_store_refuses_an_install_it_could_not_later_address() {
    let actor = actor_with_certificates().await;

    // `StoredCertificates` does no X.509 parsing, so it cannot compute the hash a CSMS would
    // delete this certificate by - and one it cannot address is one it cannot delete. Refusing is
    // more honest than storing something unreachable.
    assert_eq!(
        handle_install_certificate(
            &actor,
            &store(),
            CertificateUse::CsmsRoot,
            "-----BEGIN-----"
        )
        .await,
        InstallCertificateOutcome::Rejected
    );
}

#[tokio::test]
async fn a_charge_point_with_no_store_at_all_still_answers_every_message() {
    let actor = actor_with_certificates().await;
    let store = NoCertificateStore;

    assert_eq!(
        handle_install_certificate(&actor, &store, CertificateUse::CsmsRoot, "x").await,
        InstallCertificateOutcome::Rejected
    );
    assert_eq!(
        handle_delete_certificate(&actor, &store, &hash("01")).await,
        DeleteCertificateOutcome::NotFound
    );
    assert!(
        handle_get_installed_certificate_ids(&actor, &store, &[])
            .await
            .is_empty()
    );
}

// --- B4.3: SignCertificate / CertificateSigned -------------------------------------------------

/// A [`CertificateStore`] that always accepts an install, so `handle_certificate_signed`'s
/// success path can be tested without needing a store that can actually compute hashes.
#[derive(Default)]
struct AcceptingStore {
    installed: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl CertificateStore for AcceptingStore {
    type Error = core::convert::Infallible;

    async fn install(
        &self,
        _use_for: CertificateUse,
        _certificate: &str,
    ) -> Result<InstallCertificateOutcome, Self::Error> {
        self.installed.store(true, Ordering::SeqCst);
        Ok(InstallCertificateOutcome::Accepted)
    }

    async fn delete(
        &self,
        _hash_data: &CertificateHashData,
    ) -> Result<DeleteCertificateOutcome, Self::Error> {
        Ok(DeleteCertificateOutcome::NotFound)
    }

    async fn installed(
        &self,
        _uses: &[CertificateUse],
    ) -> Result<Vec<InstalledCertificate>, Self::Error> {
        Ok(Vec::new())
    }

    async fn has_client_private_key(&self) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

#[tokio::test]
async fn an_unsolicited_certificate_signed_is_rejected_and_flagged() {
    let actor = actor_with_certificates().await;
    let store = AcceptingStore::default();
    let pending = PendingSignRequests::new();

    // Nothing was ever recorded as sent, so this is unsolicited.
    let outcome = handle_certificate_signed(
        &actor,
        &store,
        &pending,
        CertificateSigningPurpose::ChargingStationCertificate,
        None,
        "-----BEGIN CERTIFICATE-----",
    )
    .await;

    assert_eq!(outcome, CertificateSignedOutcome::Rejected);
    assert!(
        !store.installed.load(Ordering::SeqCst),
        "must not touch the store"
    );
}

#[tokio::test]
async fn a_solicited_certificate_signed_is_installed_in_the_right_slot() {
    let actor = actor_with_certificates().await;
    let store = AcceptingStore::default();
    let pending = PendingSignRequests::new();
    pending.record_sent(CertificateSigningPurpose::ChargingStationCertificate, None);

    let outcome = handle_certificate_signed(
        &actor,
        &store,
        &pending,
        CertificateSigningPurpose::ChargingStationCertificate,
        None,
        "-----BEGIN CERTIFICATE-----",
    )
    .await;

    assert_eq!(outcome, CertificateSignedOutcome::Accepted);
    assert!(store.installed.load(Ordering::SeqCst));
    // Consumed: a second, identical `CertificateSigned` is no longer solicited.
    let second = handle_certificate_signed(
        &actor,
        &store,
        &pending,
        CertificateSigningPurpose::ChargingStationCertificate,
        None,
        "-----BEGIN CERTIFICATE-----",
    )
    .await;
    assert_eq!(second, CertificateSignedOutcome::Rejected);
}

#[tokio::test]
async fn pending_for_one_purpose_does_not_authorize_the_other() {
    let actor = actor_with_certificates().await;
    let store = AcceptingStore::default();
    let pending = PendingSignRequests::new();
    pending.record_sent(CertificateSigningPurpose::ChargingStationCertificate, None);

    let outcome = handle_certificate_signed(
        &actor,
        &store,
        &pending,
        CertificateSigningPurpose::V2GCertificate,
        None,
        "-----BEGIN CERTIFICATE-----",
    )
    .await;

    assert_eq!(outcome, CertificateSignedOutcome::Rejected);
}

#[tokio::test]
async fn a_2_1_request_id_that_does_not_match_the_pending_one_is_rejected() {
    let actor = actor_with_certificates().await;
    let store = AcceptingStore::default();
    let pending = PendingSignRequests::new();
    pending.record_sent(
        CertificateSigningPurpose::ChargingStationCertificate,
        Some(1),
    );

    let outcome = handle_certificate_signed(
        &actor,
        &store,
        &pending,
        CertificateSigningPurpose::ChargingStationCertificate,
        Some(2),
        "-----BEGIN CERTIFICATE-----",
    )
    .await;

    assert_eq!(outcome, CertificateSignedOutcome::Rejected);
}

#[tokio::test]
async fn a_missing_request_id_still_matches_a_pending_one_since_2_0_1_has_none() {
    let actor = actor_with_certificates().await;
    let store = AcceptingStore::default();
    let pending = PendingSignRequests::new();
    pending.record_sent(
        CertificateSigningPurpose::ChargingStationCertificate,
        Some(1),
    );

    // A 2.1 CSMS that omits `requestId` on `CertificateSigned` - or a 2.0.1 connection, which has
    // no such field at all - should not be refused purely for that.
    let outcome = handle_certificate_signed(
        &actor,
        &store,
        &pending,
        CertificateSigningPurpose::ChargingStationCertificate,
        None,
        "-----BEGIN CERTIFICATE-----",
    )
    .await;

    assert_eq!(outcome, CertificateSignedOutcome::Accepted);
}

#[tokio::test]
async fn a_signed_charging_station_certificate_is_installed_into_the_real_store() {
    // F2.2: `StoredCertificates::install` used to refuse a `ChargingStation`/
    // `V2gCertificateChain` certificate unconditionally, which made this the one B4.3 outcome a
    // charge point using the built-in store could ever reach - security profile 3 was
    // undemonstrable end to end as a result. This is the fixed, intended outcome: the plain
    // trait method `handle_certificate_signed` calls now accepts the charge point's own
    // certificate via a self-computed stand-in hash - see
    // `crate::hardware::certificate::StoredCertificates::install_own_certificate`'s docs for what
    // that hash is and is not.
    let actor = actor_with_certificates().await;
    let store = store();
    let pending = PendingSignRequests::new();
    pending.record_sent(CertificateSigningPurpose::ChargingStationCertificate, None);

    let outcome = handle_certificate_signed(
        &actor,
        &store,
        &pending,
        CertificateSigningPurpose::ChargingStationCertificate,
        None,
        "-----BEGIN CERTIFICATE-----\nleaf\n-----END CERTIFICATE-----",
    )
    .await;

    assert_eq!(outcome, CertificateSignedOutcome::Accepted);
    assert_eq!(
        store
            .certificate_chain_pem(CertificateUse::ChargingStation)
            .await
            .unwrap(),
        Some("-----BEGIN CERTIFICATE-----\nleaf\n-----END CERTIFICATE-----".to_string())
    );
}

#[tokio::test]
async fn a_chain_the_store_refuses_is_reported_as_rejected() {
    // A store that is genuinely full (rather than one that refuses this use categorically, which
    // is no longer true - see the test above) is still a real refusal `handle_certificate_signed`
    // must surface as `Rejected`.
    let actor = actor_with_certificates().await;
    let store = StoredCertificates::with_limit(Arc::new(InMemoryStorage::new()), 1);
    store
        .install_with_hash(CertificateUse::CsmsRoot, "-----BEGIN-----", hash("01"))
        .await;
    let pending = PendingSignRequests::new();
    pending.record_sent(CertificateSigningPurpose::ChargingStationCertificate, None);

    let outcome = handle_certificate_signed(
        &actor,
        &store,
        &pending,
        CertificateSigningPurpose::ChargingStationCertificate,
        None,
        "-----BEGIN CERTIFICATE-----",
    )
    .await;

    assert_eq!(outcome, CertificateSignedOutcome::Rejected);
}

#[tokio::test]
async fn without_the_capability_certificate_signed_is_rejected_without_touching_pending() {
    let actor = ChargePointActor::spawn([1], &TokioExecutor);
    let _ = actor
        .send(ChargePointEvent::CapabilitiesDeclared(
            Capabilities::default(),
        ))
        .await;
    let store = AcceptingStore::default();
    let pending = PendingSignRequests::new();
    pending.record_sent(CertificateSigningPurpose::ChargingStationCertificate, None);

    let outcome = handle_certificate_signed(
        &actor,
        &store,
        &pending,
        CertificateSigningPurpose::ChargingStationCertificate,
        None,
        "-----BEGIN CERTIFICATE-----",
    )
    .await;

    assert_eq!(outcome, CertificateSignedOutcome::Rejected);
    assert!(!store.installed.load(Ordering::SeqCst));
    // The pending entry survives - an absent capability should not silently discard the
    // charge point's own record of what it asked for.
    assert!(pending.matches(CertificateSigningPurpose::ChargingStationCertificate, None));
}

#[test]
fn record_sign_certificate_sent_only_arms_pending_on_acceptance() {
    let pending = PendingSignRequests::new();

    record_sign_certificate_sent(
        &pending,
        CertificateSigningPurpose::ChargingStationCertificate,
        None,
        SignCertificateOutcome::Rejected,
    );
    assert!(!pending.matches(CertificateSigningPurpose::ChargingStationCertificate, None));

    record_sign_certificate_sent(
        &pending,
        CertificateSigningPurpose::ChargingStationCertificate,
        None,
        SignCertificateOutcome::Accepted,
    );
    assert!(pending.matches(CertificateSigningPurpose::ChargingStationCertificate, None));
}

#[test]
fn a_second_sign_certificate_for_the_same_purpose_supersedes_the_first() {
    let pending = PendingSignRequests::new();
    pending.record_sent(
        CertificateSigningPurpose::ChargingStationCertificate,
        Some(1),
    );
    pending.record_sent(
        CertificateSigningPurpose::ChargingStationCertificate,
        Some(2),
    );

    // The stale id no longer matches; only the most recent one does.
    assert!(!pending.matches(
        CertificateSigningPurpose::ChargingStationCertificate,
        Some(1)
    ));
    assert!(pending.matches(
        CertificateSigningPurpose::ChargingStationCertificate,
        Some(2)
    ));
}

#[test]
fn next_request_id_is_monotonically_increasing() {
    let pending = PendingSignRequests::new();
    let a = pending.next_request_id();
    let b = pending.next_request_id();
    assert!(b > a);
}

#[test]
fn certificate_signing_purpose_maps_to_the_right_certificate_use() {
    assert_eq!(
        CertificateSigningPurpose::ChargingStationCertificate.certificate_use(),
        CertificateUse::ChargingStation
    );
    assert_eq!(
        CertificateSigningPurpose::V2GCertificate.certificate_use(),
        CertificateUse::V2gCertificateChain
    );
}

// --- CV9: SignCertificate resend discipline (A02.FR.17-.19, A03.FR.17-.19) ---------------------

/// A02.FR.17: nothing is resent until `CertSigningWaitMinimum` has expired, and then exactly one
/// resend goes out for the CSR that is outstanding.
#[test]
fn nothing_is_resent_before_the_minimum_wait_and_exactly_once_after_it() {
    let policy = SignCertificateRetryPolicy {
        wait_minimum_secs: 30,
        repeat_times: 3,
    };
    let pending = PendingSignRequests::new();
    pending.record_sent(
        CertificateSigningPurpose::ChargingStationCertificate,
        Some(1),
    );

    assert!(pending.due_for_resend(10, policy).is_empty());
    assert!(pending.due_for_resend(10, policy).is_empty());
    // 30 s reached, inclusive of the instant itself - the same "the threshold is the moment it
    // lapses" rule `is_due_for_renewal` takes.
    assert_eq!(
        pending.due_for_resend(10, policy),
        alloc::vec![CertificateSigningPurpose::ChargingStationCertificate]
    );
    // ...and the back-off has re-armed rather than firing again on the next tick.
    assert!(pending.due_for_resend(10, policy).is_empty());
}

/// A02.FR.18: each wait is twice the last, starting at `CertSigningWaitMinimum`.
#[test]
fn the_back_off_doubles_on_every_expiry() {
    let policy = SignCertificateRetryPolicy {
        wait_minimum_secs: 10,
        repeat_times: 4,
    };
    let pending = PendingSignRequests::new();
    pending.record_sent(CertificateSigningPurpose::V2GCertificate, None);

    // Waits of 10, 20, 40 and 80 seconds; a flat interval would fire on every tenth second.
    let mut fired_at = alloc::vec::Vec::new();
    let mut elapsed = 0;
    for _ in 0..160 {
        elapsed += 1;
        if !pending.due_for_resend(1, policy).is_empty() {
            fired_at.push(elapsed);
        }
    }

    assert_eq!(fired_at, alloc::vec![10, 30, 70, 150]);
}

/// A02.FR.19: `CertSigningRepeatTimes` is a hard stop, and the pending entry survives it - a CSMS
/// that answers late is still answering a CSR this station asked for.
#[test]
fn resending_stops_at_the_repeat_count_until_a_trigger_message_restarts_it() {
    let policy = SignCertificateRetryPolicy {
        wait_minimum_secs: 10,
        repeat_times: 2,
    };
    let pending = PendingSignRequests::new();
    pending.record_sent(
        CertificateSigningPurpose::ChargingStationCertificate,
        Some(7),
    );

    let mut resends = 0;
    for _ in 0..1000 {
        resends += pending.due_for_resend(10, policy).len();
    }

    assert_eq!(resends, 2, "the back-off must stop, not slow down");
    assert!(pending.has_stopped(
        CertificateSigningPurpose::ChargingStationCertificate,
        policy
    ));
    // Still outstanding: a `CertificateSigned` arriving now is solicited, not an attack.
    assert!(pending.matches(
        CertificateSigningPurpose::ChargingStationCertificate,
        Some(7)
    ));

    // The one thing OCPP allows to resume it.
    pending.restart(CertificateSigningPurpose::ChargingStationCertificate);
    assert!(!pending.has_stopped(
        CertificateSigningPurpose::ChargingStationCertificate,
        policy
    ));
    assert_eq!(
        pending.due_for_resend(10, policy),
        alloc::vec![CertificateSigningPurpose::ChargingStationCertificate],
        "the restarted back-off starts again at CertSigningWaitMinimum"
    );
}

/// A resend goes out through the same `sign_certificate` path as the original, so `record_sent`
/// runs again for a CSR that is already counting down. If that reset the count, the station would
/// resend on a flat interval for ever and never reach A02.FR.19's stop.
#[test]
fn a_resend_continues_the_doubling_rather_than_restarting_it() {
    let policy = SignCertificateRetryPolicy {
        wait_minimum_secs: 10,
        repeat_times: 2,
    };
    let pending = PendingSignRequests::new();
    pending.record_sent(
        CertificateSigningPurpose::ChargingStationCertificate,
        Some(1),
    );

    assert_eq!(pending.due_for_resend(10, policy).len(), 1);
    // What the requester does when the resend is accepted, with a fresh 2.1 `requestId`.
    pending.record_sent(
        CertificateSigningPurpose::ChargingStationCertificate,
        Some(2),
    );

    // The second wait is 20 s, not another 10 s.
    assert!(pending.due_for_resend(10, policy).is_empty());
    assert_eq!(pending.due_for_resend(10, policy).len(), 1);
    assert!(pending.has_stopped(
        CertificateSigningPurpose::ChargingStationCertificate,
        policy
    ));
}

/// A02.FR.20: the CSMS *rejected* the request, so no back-off was ever armed - the station must
/// not resend until a `TriggerMessage` asks it to.
#[test]
fn a_rejected_sign_certificate_arms_no_back_off_at_all() {
    let policy = SignCertificateRetryPolicy::default();
    let pending = PendingSignRequests::new();

    record_sign_certificate_sent(
        &pending,
        CertificateSigningPurpose::ChargingStationCertificate,
        Some(1),
        SignCertificateOutcome::Rejected,
    );

    for _ in 0..100 {
        assert!(pending.due_for_resend(60, policy).is_empty());
    }
}

/// Each purpose counts down on its own: a V2G CSR the CSMS answered must not silence the charging
/// station certificate's resends, and vice versa.
#[test]
fn the_two_purposes_back_off_independently() {
    let policy = SignCertificateRetryPolicy {
        wait_minimum_secs: 10,
        repeat_times: 3,
    };
    let pending = PendingSignRequests::new();
    pending.record_sent(
        CertificateSigningPurpose::ChargingStationCertificate,
        Some(1),
    );
    // Five seconds into the first one's wait, a second CSR goes out for the other purpose.
    assert!(pending.due_for_resend(5, policy).is_empty());
    pending.record_sent(CertificateSigningPurpose::V2GCertificate, Some(2));

    assert_eq!(
        pending.due_for_resend(5, policy),
        alloc::vec![CertificateSigningPurpose::ChargingStationCertificate],
        "only the purpose whose own back-off expired"
    );
    assert_eq!(
        pending.due_for_resend(5, policy),
        alloc::vec![CertificateSigningPurpose::V2GCertificate]
    );
}

/// Either variable at zero switches resending off - see `SignCertificateRetryPolicy`. A zero
/// back-off would otherwise mean "resend on every tick", which is a station flooding the CSMS it
/// is waiting on.
#[test]
fn a_zero_wait_or_a_zero_repeat_count_disables_resending() {
    let pending = PendingSignRequests::new();
    pending.record_sent(
        CertificateSigningPurpose::ChargingStationCertificate,
        Some(1),
    );

    for policy in [
        SignCertificateRetryPolicy {
            wait_minimum_secs: 0,
            repeat_times: 3,
        },
        SignCertificateRetryPolicy {
            wait_minimum_secs: 30,
            repeat_times: 0,
        },
    ] {
        for _ in 0..100 {
            assert!(
                pending.due_for_resend(3600, policy).is_empty(),
                "{policy:?}"
            );
        }
    }
}

/// A CSMS may write any non-negative integer into `CertSigningRepeatTimes`, and
/// `CertSigningWaitMinimum << resends` is a shift overflow long before it runs out. The failure
/// being guarded is not the panic but the release-mode wrap: a back-off that comes back *round* to
/// a short one would have the station resend fastest exactly when it has been told to back off
/// hardest.
#[test]
fn a_back_off_that_outgrows_its_own_arithmetic_saturates_rather_than_wrapping() {
    let policy = SignCertificateRetryPolicy {
        wait_minimum_secs: 30,
        repeat_times: u32::MAX,
    };

    // Monotonic across the whole range a `resends` counter can reach, including past the shift
    // width where a bare `<<` is undefined.
    let mut previous = policy.back_off_secs(0);
    assert_eq!(previous, 30);
    for resends in 1..=64 {
        let back_off = policy.back_off_secs(resends);
        assert!(
            back_off >= previous,
            "resends={resends} went backwards: {previous} -> {back_off}"
        );
        previous = back_off;
    }
    assert_eq!(policy.back_off_secs(u32::MAX), u32::MAX);
}

/// CV9: the discipline runs on what a CSMS wrote, which is the read that makes both variables
/// `honoured` rather than decorative.
#[tokio::test]
async fn the_policy_comes_from_the_device_model_and_a_csms_write_changes_it() {
    use crate::device_model::{SetVariableOutcome, SetVariableRequest, handle_set_variables};
    use crate::state::{Component, Variable, VariableAttributeType};

    let actor = ChargePointActor::spawn([1], &TokioExecutor);

    assert_eq!(
        sign_certificate_retry_policy(&actor.state()),
        SignCertificateRetryPolicy::default(),
        "a fresh station runs this crate's registered defaults"
    );

    let write = |name: &str, value: &str| SetVariableRequest {
        component: Component {
            name: "SecurityCtrlr".into(),
            instance: None,
            evse: None,
        },
        variable: Variable {
            name: name.into(),
            instance: None,
        },
        attribute_type: VariableAttributeType::Actual,
        value: value.into(),
    };
    let outcomes = handle_set_variables(
        &actor,
        alloc::vec![
            write("CertSigningWaitMinimum", "45"),
            write("CertSigningRepeatTimes", "6"),
        ],
        &crate::hardware::NoKeyStore,
    )
    .await;

    assert_eq!(
        outcomes,
        alloc::vec![SetVariableOutcome::Accepted, SetVariableOutcome::Accepted],
        "both are honoured, so B05.FR.09 asks that the write be accepted"
    );
    assert_eq!(
        sign_certificate_retry_policy(&actor.state()),
        SignCertificateRetryPolicy {
            wait_minimum_secs: 45,
            repeat_times: 6,
        }
    );
}

/// A malformed value degrades to this crate's own discipline rather than to "never resend" -
/// silently switching the retry off would leave a CSR outstanding for ever with nothing to say
/// why. (`SetVariables` refuses such a value; a value can still arrive this way through a direct
/// device-model registration by an integrator.)
#[test]
fn an_unreadable_variable_falls_back_to_the_registered_default() {
    let mut state = crate::state::ChargePointState::new([1]);
    state.device_model.register(
        crate::state::Component {
            name: "SecurityCtrlr".into(),
            instance: None,
            evse: None,
        },
        crate::state::Variable {
            name: "CertSigningWaitMinimum".into(),
            instance: None,
        },
        crate::state::VariableCharacteristics {
            data_type: crate::state::VariableDataType::Integer,
            unit: Some("s".into()),
            min_limit: None,
            max_limit: None,
            values_list: None,
            supports_monitoring: false,
        },
        alloc::vec![crate::state::VariableAttribute {
            attribute_type: crate::state::VariableAttributeType::Actual,
            value: "not-a-number".into(),
            mutability: crate::state::VariableMutability::ReadWrite,
            persistent: false,
            constant: false,
            requires_reboot: false,
        }],
    );

    assert_eq!(
        sign_certificate_retry_policy(&state).wait_minimum_secs,
        SignCertificateRetryPolicy::default().wait_minimum_secs
    );
}

// --- CV9: MaxCertificateChainSize (A02.FR.16/A03.FR.16) ----------------------------------------

/// A02.FR.16: a chain larger than the configured ceiling is refused before the store is touched,
/// and the CSR stays outstanding so the resend discipline can ask again.
#[tokio::test]
async fn a_chain_larger_than_max_certificate_chain_size_is_refused() {
    use crate::device_model::{SetVariableRequest, handle_set_variables};
    use crate::state::{Component, Variable, VariableAttributeType};

    let actor = actor_with_certificates().await;
    let store = store();
    let pending = PendingSignRequests::new();
    pending.record_sent(
        CertificateSigningPurpose::ChargingStationCertificate,
        Some(1),
    );

    handle_set_variables(
        &actor,
        alloc::vec![SetVariableRequest {
            component: Component {
                name: "SecurityCtrlr".into(),
                instance: None,
                evse: None,
            },
            variable: Variable {
                name: "MaxCertificateChainSize".into(),
                instance: None,
            },
            attribute_type: VariableAttributeType::Actual,
            value: "64".into(),
        }],
        &crate::hardware::NoKeyStore,
    )
    .await;

    let outcome = handle_certificate_signed(
        &actor,
        &store,
        &pending,
        CertificateSigningPurpose::ChargingStationCertificate,
        Some(1),
        &"x".repeat(65),
    )
    .await;

    assert_eq!(outcome, CertificateSignedOutcome::Rejected);
    assert_eq!(
        store
            .certificate_chain_pem(CertificateUse::ChargingStation)
            .await
            .unwrap(),
        None,
        "the store must not be touched by a chain that was refused"
    );
    assert!(
        pending.matches(
            CertificateSigningPurpose::ChargingStationCertificate,
            Some(1)
        ),
        "the CSR is still outstanding, so a resend can still get a chain that fits"
    );

    // One character shorter fits, and installs.
    assert_eq!(
        handle_certificate_signed(
            &actor,
            &store,
            &pending,
            CertificateSigningPurpose::ChargingStationCertificate,
            Some(1),
            &"x".repeat(64),
        )
        .await,
        CertificateSignedOutcome::Accepted
    );
}
