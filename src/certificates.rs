//! Certificate management messages: `InstallCertificate`, `DeleteCertificate` and
//! `GetInstalledCertificateIds` (`docs/ROADMAP.md` §1/§13, `docs/PRODUCTION-ROADMAP.md` B4.2).
//!
//! **2.x only.** 1.6J has no certificate messages in its core specification - they arrive with the
//! Security Whitepaper, whose message set `ocpp-types` does not generate (D2.2), the same reason
//! `SecurityEventNotification` is 2.x-only here.
//!
//! Every handler is a thin decision over [`crate::hardware::CertificateStore`], which is where the
//! actual work lives: parsing X.509, computing hashes and holding keys all belong to the
//! integrator, and possibly to a secure element (see that trait's docs). What this module adds is
//! the OCPP-shaped part - the status a refusal takes, the absent-means-all filter, and the
//! capability gate.
//!
//! # A store that refuses everything is a valid store
//!
//! [`crate::hardware::StoredCertificates`] - the `Storage`-backed default - refuses
//! `InstallCertificate`, because it cannot compute the hash a CSMS would later address the
//! certificate by, and one it cannot address is one it cannot delete. That is a real limitation of
//! *that implementation*, not of this block: an integrator who can parse X.509 implements the
//! trait (or calls `install_with_hash`) and installation works. The refusal reaches the CSMS as
//! `Rejected`, which is what OCPP has for "this charge point will not take that certificate".

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::actor::ChargePointActor;
use crate::hardware::{
    CertificateHashData, CertificateStore, CertificateUse, DeleteCertificateOutcome,
    InstallCertificateOutcome, InstalledCertificate,
};

/// Handles a CSMS-initiated `InstallCertificate`.
///
/// Refuses before touching the store when the capability is absent (C5) or the use is one OCPP
/// does not allow to be installed - the charge point's own certificates, which arrive by
/// `CertificateSigned` in answer to a CSR rather than by this message.
pub async fn handle_install_certificate<S: CertificateStore>(
    actor: &ChargePointActor,
    store: &S,
    use_for: CertificateUse,
    certificate: &str,
) -> InstallCertificateOutcome {
    if !crate::refusal::capability_present(&actor.state().capabilities, "InstallCertificate") {
        return InstallCertificateOutcome::Rejected;
    }
    if !use_for.is_installable() {
        tracing::warn!(
            ?use_for,
            "refusing to install a certificate this charge point issues rather than receives"
        );
        return InstallCertificateOutcome::Rejected;
    }
    match store.install(use_for, certificate).await {
        Ok(outcome) => outcome,
        Err(err) => {
            // Degraded rather than propagated, per `CLAUDE.md`: a store that cannot be written is
            // a charge point that keeps running with the certificates it already has, not one
            // that falls over.
            tracing::warn!(error = %err, "the certificate store refused an install");
            InstallCertificateOutcome::Failed
        }
    }
}

/// Handles a CSMS-initiated `DeleteCertificate`.
///
/// An absent capability answers `NotFound` rather than `Failed`: a charge point with no
/// certificate store genuinely does not have the certificate, and `Failed` would send an operator
/// looking for a fault.
pub async fn handle_delete_certificate<S: CertificateStore>(
    actor: &ChargePointActor,
    store: &S,
    hash_data: &CertificateHashData,
) -> DeleteCertificateOutcome {
    if !crate::refusal::capability_present(&actor.state().capabilities, "DeleteCertificate") {
        return DeleteCertificateOutcome::NotFound;
    }
    match store.delete(hash_data).await {
        Ok(outcome) => outcome,
        Err(err) => {
            tracing::warn!(error = %err, "the certificate store refused a delete");
            DeleteCertificateOutcome::Failed
        }
    }
}

/// Handles a CSMS-initiated `GetInstalledCertificateIds`, returning what matches.
///
/// An empty `uses` means every use - OCPP's absent-means-all rule, the same one
/// `GetChargingProfiles` follows. An empty *result* is reported by the caller as `NotFound`, which
/// is OCPP's way of saying "none installed" rather than an error.
pub async fn handle_get_installed_certificate_ids<S: CertificateStore>(
    actor: &ChargePointActor,
    store: &S,
    uses: &[CertificateUse],
) -> Vec<InstalledCertificate> {
    if !crate::refusal::capability_present(
        &actor.state().capabilities,
        "GetInstalledCertificateIds",
    ) {
        return Vec::new();
    }
    match store.installed(uses).await {
        Ok(installed) => installed,
        Err(err) => {
            tracing::warn!(error = %err, "could not list installed certificates");
            Vec::new()
        }
    }
}

/// Registers this charge point's inbound certificate-management handling.
///
/// One trait for all three messages, unlike most blocks here: they share a store, and a CSMS
/// client that can answer one can answer all of them.
#[async_trait::async_trait]
pub trait CertificateHandler {
    /// Registers `InstallCertificate`, `DeleteCertificate` and `GetInstalledCertificateIds`
    /// handlers dispatching against `actor` and `store`.
    async fn register_certificate_handlers<S>(&self, actor: ChargePointActor, store: S)
    where
        S: CertificateStore + Send + Sync + 'static;
}

#[cfg(feature = "ocpp_2_0_1")]
mod ocpp_2_0_1;
#[cfg(feature = "ocpp_2_1")]
mod ocpp_2_1;

#[cfg(test)]
mod tests;
