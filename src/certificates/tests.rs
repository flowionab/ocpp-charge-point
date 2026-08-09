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

    async fn has_private_key(&self) -> Result<bool, Self::Error> {
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
