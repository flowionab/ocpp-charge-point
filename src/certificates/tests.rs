//! Tests for the certificate-management block's protocol-agnostic handlers (B4.2).

use super::*;
use crate::executor::TokioExecutor;
use crate::hardware::{
    Capabilities, HashAlgorithm, InMemoryStorage, NoCertificateStore, StoredCertificates,
};
use crate::state::ChargePointEvent;
use alloc::sync::Arc;

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
