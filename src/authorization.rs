//! Authorization functional block: deciding whether a presented identifier may start
//! charging, via Authorize. See `docs/ROADMAP.md` §3.
//!
//! # Offline authorization
//!
//! When the Authorize call itself fails - the CSMS is unreachable, the link is flapping - the
//! charge point falls back to what it already knows, in this order
//! (`docs/PRODUCTION-ROADMAP.md` B1.2):
//!
//! 1. The **local authorization list** ([`crate::local_authorization_list`]), if
//!    `AuthCtrlr`/`LocalAuthorizeOffline` permits offline answers. The CSMS pushed this list
//!    deliberately, so it outranks anything the charge point merely observed.
//! 2. The **authorization cache** ([`crate::state::AuthorizationCache`]), if that same variable
//!    permits it *and* `AuthCacheCtrlr`/`Enabled` is on. This is what the CSMS answered last time
//!    the same token was presented, subject to `AuthCacheCtrlr`/`LifeTime`.
//! 3. Otherwise **denied**, exactly as before: erratic connectivity must not leave a connector
//!    waiting indefinitely, and "we don't know" is not "yes".
//!
//! Every one of those switches is a real device-model variable a CSMS can read and write, not a
//! compile-time policy - and each is registered as a built-in default rather than left absent
//! with a guessed fallback, so "what will you do offline?" is a question the CSMS can actually
//! ask.
//!
//! # Not asking at all
//!
//! `AuthCtrlr.DisableRemoteAuthorization` (CV7) is a different question from every switch above,
//! and the difference is worth stating because the two look alike from the outside. The offline
//! switches describe what to do when the CSMS *cannot* be reached; this one is an operator
//! instruction never to reach for it, on a station whose link is perfectly healthy. When it is
//! set, [`local_only_decision`] answers from the local authorization list and the authorization
//! cache and refuses anything neither knows - and no `Authorize` is sent, which is the point.
//!
//! It is deliberately not implemented by pretending to be offline: two of [`offline_decision`]'s
//! three switches describe outage behaviour and would be wrong here, so it has its own function
//! rather than a flag threaded through that one. See [`local_only_decision`] for which switch
//! survives the distinction and why.
//!
//! # Plug & Charge (OCPP use case C07)
//!
//! An identifier that arrived with an ISO 15118 contract certificate takes a different path
//! through this module, in both directions (`docs/PRODUCTION-ROADMAP.md` B4.6):
//!
//! - **Online**, the `Authorize` additionally carries the eMAID's certificate material -
//!   [`ContractCertificate`] - and the CSMS answers about the certificate *and* the token. Both
//!   must be positive; see [`ContractAuthorization::accepted`].
//! - **Offline, the fallback above does not apply.** C07.FR.07 requires a charge point that
//!   cannot reach the CSMS to refuse a contract presentation outright unless
//!   `ISO15118Ctrlr`/`ContractValidationOffline` is set *and* it can validate the certificate
//!   itself - which this crate never can, since it does no X.509 parsing. So a Plug & Charge
//!   presentation is denied when the link is down, even for an eMAID sitting accepted in the
//!   cache or the local list. That is not this crate being cautious beyond the spec: accepting
//!   would mean starting a transaction on a contract nobody checked for revocation.
//!
//! The certificate itself is produced by the integrator's ISO 15118 stack, which presents it via
//! [`crate::state::ConnectorEvent::ContractCertificatePresented`]
//! on the same [`crate::hardware::HardwareEventSender`] every other hardware observation arrives
//! on. This crate never parses it - see [`crate::iso15118`] for where that boundary sits.

use crate::actor::ChargePointActor;
use crate::clock::{Clock, is_synchronized};
use crate::state::{
    AuthorizationRequested, AuthorizationStatus, ChargePointEvent, ChargePointState, Component,
    ConnectorEvent, ContractCertificate, ContractCertificateStatus, EvseEvent, IdToken, Variable,
    VariableAttributeType,
};
use crate::sync::BroadcastReceiver;
use alloc::boxed::Box;
use chrono::{DateTime, Utc};

/// Reads a `Boolean` device-model variable, defaulting to `default` when it is absent or
/// unparseable. Every variable this module reads is registered by
/// [`crate::state::DeviceModel::register_defaults`], so the default only applies to a charge point
/// whose binding deliberately removed one.
fn boolean_variable(
    state: &ChargePointState,
    component: &str,
    variable: &str,
    default: bool,
) -> bool {
    let component = Component {
        name: component.into(),
        instance: None,
        evse: None,
    };
    let variable = Variable {
        name: variable.into(),
        instance: None,
    };
    state
        .device_model
        .get(&component, &variable)
        .and_then(|definition| definition.attribute(VariableAttributeType::Actual))
        .and_then(|attribute| attribute.value.parse::<bool>().ok())
        .unwrap_or(default)
}

/// `AuthCacheCtrlr`/`LifeTime` in seconds, or `None` when absent/unparseable/zero - all of which
/// mean "entries don't expire on age" (see [`crate::state::AuthorizationCache::lookup`]).
fn cache_life_time_secs(state: &ChargePointState) -> Option<u32> {
    let component = Component {
        name: "AuthCacheCtrlr".into(),
        instance: None,
        evse: None,
    };
    let variable = Variable {
        name: "LifeTime".into(),
        instance: None,
    };
    let value = state
        .device_model
        .get(&component, &variable)?
        .attribute(VariableAttributeType::Actual)?
        .value
        .parse::<u32>()
        .ok()?;
    (value != 0).then_some(value)
}

/// The decision to fall back on when the CSMS can't be reached - see this module's docs for the
/// order and the switches that gate it.
///
/// `now` is the caller's clock reading, or `None` when it isn't synchronized; an unsynchronized
/// clock cannot expire a cache entry, which is deliberate (see
/// [`crate::state::AuthorizationCacheEntry::cached_at`]).
pub fn offline_decision(
    state: &ChargePointState,
    id_token: &IdToken,
    now: Option<DateTime<Utc>>,
) -> AuthorizationStatus {
    if !boolean_variable(state, "AuthCtrlr", "LocalAuthorizeOffline", true) {
        return AuthorizationStatus::Rejected;
    }
    if let Some(entry) = state
        .local_authorization_list
        .entries
        .iter()
        .find(|entry| entry.id_token.value == id_token.value)
    {
        tracing::info!("authorizing offline from the local authorization list");
        return entry.status;
    }
    if !boolean_variable(state, "AuthCacheCtrlr", "Enabled", true) {
        return AuthorizationStatus::Rejected;
    }
    match state
        .authorization_cache
        .lookup(id_token, now, cache_life_time_secs(state))
    {
        Some(entry) => {
            tracing::info!("authorizing offline from the authorization cache");
            entry.status
        }
        // C15 (CV2.9): an identifier this charge point has never seen. The list said nothing and
        // the cache said nothing, so there is no evidence either way - and
        // `AuthCtrlr.OfflineTxForUnknownIdEnabled` is the operator's answer to "start anyway?".
        //
        // Defaults to `false`, and that default is the whole point of the switch: charging an
        // unknown card offline is a decision to hand out energy that may never be billed, which is
        // an operator's call to make and not a library's to assume. An operator who wants a site
        // to keep serving drivers through a CSMS outage turns it on deliberately.
        None if boolean_variable(state, "AuthCtrlr", "OfflineTxForUnknownIdEnabled", false) => {
            tracing::info!(
                "authorizing an unknown identifier offline: OfflineTxForUnknownIdEnabled is set"
            );
            AuthorizationStatus::Accepted
        }
        None => AuthorizationStatus::Rejected,
    }
}

/// Whether `AuthCtrlr.DisableRemoteAuthorization` forbids asking the CSMS at all (CV7).
///
/// Distinct from every offline switch in this module: those describe what to do when the CSMS
/// *cannot* be reached, this one is an operator instruction not to reach for it in the first
/// place. OCPP's own note draws the same line against `AuthCacheCtrlr.DisablePostAuthorization`,
/// which only suppresses re-authorizing a token already known to be refused.
fn remote_authorization_disabled(state: &ChargePointState) -> bool {
    boolean_variable(state, "AuthCtrlr", "DisableRemoteAuthorization", false)
}

/// The decision to reach when [`remote_authorization_disabled`] holds: the local authorization
/// list, then the authorization cache, then refusal.
///
/// Deliberately **not** [`offline_decision`], though the two consult the same two sources in the
/// same order. Two of that function's three switches are about being offline and would be wrong
/// here:
///
/// - `AuthCtrlr.LocalAuthorizeOffline` answers "may this station decide for itself while the link
///   is down?". The link is not down; the operator has said what to decide from. Honouring it
///   would let an unrelated variable silently turn this one off.
/// - `AuthCtrlr.OfflineTxForUnknownIdEnabled` grants energy to an *unknown* identifier rather than
///   strand a driver mid-outage. There is no outage to be stranded by, so an identifier neither
///   source knows is simply refused.
///
/// `AuthCacheCtrlr.Enabled` *is* honoured, because a disabled cache is not a source at all.
fn local_only_decision(
    state: &ChargePointState,
    id_token: &IdToken,
    now: Option<DateTime<Utc>>,
) -> AuthorizationStatus {
    if let Some(entry) = state
        .local_authorization_list
        .entries
        .iter()
        .find(|entry| entry.id_token.value == id_token.value)
    {
        tracing::debug!("deciding from the local authorization list: the CSMS is not to be asked");
        return entry.status;
    }
    if !boolean_variable(state, "AuthCacheCtrlr", "Enabled", true) {
        return AuthorizationStatus::Rejected;
    }
    match state
        .authorization_cache
        .lookup(id_token, now, cache_life_time_secs(state))
    {
        Some(entry) => {
            tracing::debug!("deciding from the authorization cache: the CSMS is not to be asked");
            entry.status
        }
        None => AuthorizationStatus::Rejected,
    }
}

/// What the CSMS answered a Plug & Charge `Authorize` with: its decision about the eMAID, and -
/// separately - what it made of the contract certificate (OCPP use case C07).
///
/// Two answers rather than one because OCPP really does send two, and they are not redundant:
/// C07.FR.14's note points out a perfectly valid certificate can still be refused on the token
/// (`ConcurrentTx`, `NotAtThisLocation`), and C07.FR.13 covers the reverse - a valid certificate
/// whose contract has been cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContractAuthorization {
    /// The CSMS's decision about the eMAID, from `idTokenInfo.status`.
    pub status: AuthorizationStatus,
    /// What the CSMS made of the certificate, from `AuthorizeResponse.certificateStatus`.
    ///
    /// `None` means the CSMS expressed no opinion - which is the normal answer when no
    /// certificate material was sent for it to have one about, and the only possible answer from
    /// a 1.6J CSMS (see this module's `ocpp_1_6` adapter).
    pub certificate_status: Option<ContractCertificateStatus>,
}

impl ContractAuthorization {
    /// Whether charging may start: the eMAID was accepted **and** the certificate was not
    /// rejected.
    ///
    /// Both halves are checked even though C07.FR.13-17 say the CSMS always makes them agree - a
    /// station that trusted that pairing would start charging on a revoked contract the moment
    /// one CSMS got it wrong, and the check that prevents that costs nothing. An absent
    /// `certificate_status` is not a rejection: it means nothing was asked about (see that
    /// field).
    #[must_use]
    pub fn accepted(&self) -> bool {
        self.status == AuthorizationStatus::Accepted
            && !matches!(self.certificate_status, Some(status) if status != ContractCertificateStatus::Accepted)
    }
}

/// Decides whether an [`IdToken`] may start charging, via Authorize. Implemented per protocol
/// version (see the `ocpp_2_1` module), mirroring
/// [`crate::availability::StatusNotifier`].
#[async_trait::async_trait]
pub trait Authorizer {
    /// The error type returned if the Authorize request itself fails (e.g. a transport error) -
    /// distinct from the CSMS's own [`AuthorizationStatus`] decision, which is never an `Err`.
    type Error: core::error::Error + Send + Sync + 'static;

    /// Asks the CSMS whether `id_token` may start charging.
    async fn authorize(&self, id_token: &IdToken) -> Result<AuthorizationStatus, Self::Error>;

    /// Asks the CSMS whether `id_token` may start charging, presenting the ISO 15118 contract
    /// certificate it came from for validation (OCPP use case C07, `docs/PRODUCTION-ROADMAP.md`
    /// B4.6).
    ///
    /// `contract`'s [`chain_pem`](ContractCertificate::chain_pem) has already been cleared by
    /// [`run_authorization_requests`] if `ISO15118Ctrlr.CentralContractValidationAllowed` forbids
    /// sending it (C07.FR.06) - an implementation sends what it is given and makes no policy
    /// decision of its own.
    ///
    /// # Default implementation
    ///
    /// Refuses, locally, without asking anyone. A CSMS connection that has not implemented this
    /// cannot convey the certificate, and an `Authorize` carrying only the eMAID would ask the
    /// CSMS a question it cannot answer correctly - it would have to decide on an identifier
    /// whose contract nobody checked. Refusing mirrors
    /// [`crate::hardware::NoIso15118Controller`]'s honest refusal, and leaves
    /// `certificate_status` `None` rather than inventing a verdict no CSMS gave.
    async fn authorize_contract(
        &self,
        id_token: &IdToken,
        contract: &ContractCertificate,
    ) -> Result<ContractAuthorization, Self::Error> {
        let _ = (id_token, contract);
        tracing::warn!(
            "this CSMS connection cannot carry an ISO 15118 contract certificate; refusing the \
             Plug & Charge authorization rather than asking on the eMAID alone"
        );
        Ok(ContractAuthorization {
            status: AuthorizationStatus::Rejected,
            certificate_status: None,
        })
    }
}

/// The outcome of a CSMS-initiated `ClearCache`, matching OCPP's `ClearCacheStatusEnum` (2.x) /
/// `ClearCacheStatus` (1.6J).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClearCacheOutcome {
    /// The cache was emptied - including when it was already empty, which is still the state the
    /// CSMS asked for.
    Accepted,
    /// Caching is disabled on this charge point (`AuthCacheCtrlr`/`Enabled` is false), so there is
    /// no cache to clear. OCPP's own guidance for a charge point without an authorization cache.
    Rejected,
}

/// Handles a CSMS-initiated `ClearCache` against `actor`: empties the authorization cache.
///
/// Accepted even when the cache was already empty - the CSMS asked for "no cached decisions", and
/// that is the resulting state either way. Rejected only when caching is switched off entirely,
/// where accepting would imply this charge point has a cache it is keeping clear.
#[tracing::instrument(skip_all)]
pub async fn handle_clear_cache(actor: &ChargePointActor) -> ClearCacheOutcome {
    let state = actor.state();
    if !boolean_variable(&state, "AuthCacheCtrlr", "Enabled", true) {
        return ClearCacheOutcome::Rejected;
    }
    let _ = actor
        .send(ChargePointEvent::AuthorizationCacheCleared)
        .await;
    ClearCacheOutcome::Accepted
}

/// Registers this charge point's inbound `ClearCache` handling with the CSMS connection.
/// Implemented per protocol version, mirroring [`crate::reservation::CancelReservationHandler`].
#[async_trait::async_trait]
pub trait ClearCacheHandler {
    /// Registers a `ClearCache` handler dispatching against `actor`.
    async fn register_clear_cache_handler(&self, actor: ChargePointActor);
}

/// Answers every authorization request received on `requests` by calling `authorizer`, and
/// feeds the decision back into the actor as `ChargingAuthorized`/`AuthorizationDenied`,
/// forever.
///
/// A transport-level failure falls back to [`offline_decision`] rather than denying outright -
/// see this module's docs for the order and the device-model switches that gate it. When nothing
/// offline has an opinion the answer is still denial: erratic connectivity must not leave a
/// connector waiting indefinitely (see `CLAUDE.md`'s error-handling guidance), and "we don't
/// know" is not "yes".
///
/// Every decision the CSMS *does* give is remembered in the authorization cache, acceptance and
/// rejection alike - a cache that only remembered acceptances would let a revoked card in every
/// time the link drops. `clock` stamps those entries; an unsynchronized reading is recorded as
/// `None`, which makes the entry non-expiring rather than fabricating an age (see
/// [`crate::state::AuthorizationCacheEntry::cached_at`]).
pub async fn run_authorization_requests<A: Authorizer + Sync, C: Clock>(
    mut requests: BroadcastReceiver<AuthorizationRequested>,
    authorizer: &A,
    actor: ChargePointActor,
    clock: &C,
) {
    while let Ok(requested) = requests.recv().await {
        let now = clock.now();
        let now = is_synchronized(&now).then_some(now);
        let decision = if let Some(contract) = requested.contract.clone() {
            contract_decision(authorizer, &actor, &requested.id_token, contract, now).await
        } else {
            plain_decision(authorizer, &actor, &requested.id_token, now).await
        };
        let event = match decision {
            AuthorizationStatus::Accepted => {
                ConnectorEvent::ChargingAuthorized(requested.id_token.clone())
            }
            AuthorizationStatus::Rejected => ConnectorEvent::AuthorizationDenied,
        };
        let _ = actor
            .send(ChargePointEvent::Evse {
                evse_id: requested.evse_id,
                event: EvseEvent::Connector {
                    connector_id: requested.connector_id,
                    event,
                },
            })
            .await;
    }
}

/// The ordinary (card, app, EIM) authorization decision: ask the CSMS, remember what it said, and
/// fall back to [`offline_decision`] if the call itself failed.
///
/// Unless `AuthCtrlr.DisableRemoteAuthorization` says not to ask at all, in which case the answer
/// comes from [`local_only_decision`] and nothing is cached - there is no CSMS decision to record,
/// and writing the station's own local answer back into the cache would let it harden into
/// evidence it was never given.
async fn plain_decision<A: Authorizer + Sync>(
    authorizer: &A,
    actor: &ChargePointActor,
    id_token: &IdToken,
    now: Option<DateTime<Utc>>,
) -> AuthorizationStatus {
    if remote_authorization_disabled(&actor.state()) {
        return local_only_decision(&actor.state(), id_token, now);
    }
    match authorizer.authorize(id_token).await {
        Ok(status) => {
            cache_decision(actor, id_token, status, now).await;
            status
        }
        Err(err) => {
            tracing::warn!(
                error = %err,
                "authorization request failed; falling back to offline authorization"
            );
            offline_decision(&actor.state(), id_token, now)
        }
    }
}

/// The Plug & Charge decision (OCPP use case C07), which differs from [`plain_decision`] in both
/// directions.
///
/// Online, acceptance needs the CSMS to be happy with the certificate as well as the eMAID - see
/// [`ContractAuthorization::accepted`].
///
/// Offline, there is **no fallback at all**: C07.FR.07 requires a charge point to refuse charging
/// when it cannot reach the CSMS and `ISO15118Ctrlr.ContractValidationOffline` is false, and
/// FR.08-09 only permit the cache/local-list path after the station has *itself* validated the
/// contract certificate. This crate never can (no X.509 parsing - see
/// [`crate::hardware::CertificateHashData`]), so the answer is refusal either way; the variable
/// is still read so the log names the reason a CSMS could have configured differently.
async fn contract_decision<A: Authorizer + Sync>(
    authorizer: &A,
    actor: &ChargePointActor,
    id_token: &IdToken,
    contract: ContractCertificate,
    now: Option<DateTime<Utc>>,
) -> AuthorizationStatus {
    // The same refusal the offline branch below reaches, by the same reasoning: C07 puts contract
    // validation at the CSMS, and a station forbidden to ask cannot establish that the contract
    // behind the eMAID is still valid. The local list and cache hold decisions about *tokens*, and
    // an accepted eMAID is not evidence of an unrevoked contract.
    if remote_authorization_disabled(&actor.state()) {
        tracing::warn!(
            "refusing a Plug & Charge presentation: AuthCtrlr.DisableRemoteAuthorization forbids \
             asking the CSMS, and this charge point cannot validate a contract certificate itself"
        );
        return AuthorizationStatus::Rejected;
    }
    let contract = withhold_chain_unless_allowed(&actor.state(), contract);
    match authorizer.authorize_contract(id_token, &contract).await {
        Ok(authorization) => {
            let status = if authorization.accepted() {
                AuthorizationStatus::Accepted
            } else {
                AuthorizationStatus::Rejected
            };
            if let Some(certificate_status) = authorization.certificate_status {
                tracing::debug!(
                    certificate_status = certificate_status.name(),
                    "the CSMS reported a contract certificate status"
                );
            }
            cache_decision(actor, id_token, status, now).await;
            status
        }
        Err(err) => {
            tracing::warn!(
                error = %err,
                contract_validation_offline = boolean_variable(
                    &actor.state(),
                    "ISO15118Ctrlr",
                    "ContractValidationOffline",
                    false,
                ),
                "a Plug & Charge authorization could not reach the CSMS; refusing rather than \
                 falling back offline, because this charge point cannot validate a contract \
                 certificate itself"
            );
            AuthorizationStatus::Rejected
        }
    }
}

/// Clears the PEM chain unless `ISO15118Ctrlr.CentralContractValidationAllowed` permits sending it
/// (C07.FR.06), warning when it does not - a station whose HLC stack supplies a chain that is
/// then withheld will very likely be refused by the CSMS, and the operator should be able to see
/// why in the log rather than by comparing wire traces.
///
/// The OCSP data is never withheld: C07.FR.02 makes it the part every Plug & Charge `Authorize`
/// carries, and it identifies the certificate rather than reproducing it.
fn withhold_chain_unless_allowed(
    state: &ChargePointState,
    mut contract: ContractCertificate,
) -> ContractCertificate {
    if contract.chain_pem.is_some()
        && !boolean_variable(
            state,
            "ISO15118Ctrlr",
            "CentralContractValidationAllowed",
            false,
        )
    {
        tracing::warn!(
            "withholding the contract certificate chain: ISO15118Ctrlr.\
             CentralContractValidationAllowed is false, so the CSMS is asked to validate from the \
             OCSP data alone"
        );
        contract.chain_pem = None;
    }
    contract
}

/// Records a CSMS decision in the authorization cache - acceptance and rejection alike, for the
/// reason [`run_authorization_requests`] documents.
async fn cache_decision(
    actor: &ChargePointActor,
    id_token: &IdToken,
    status: AuthorizationStatus,
    now: Option<DateTime<Utc>>,
) {
    let _ = actor
        .send(ChargePointEvent::AuthorizationCached {
            id_token: id_token.clone(),
            status,
            cached_at: now,
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::{Authorizer, run_authorization_requests};
    use crate::actor::ChargePointActor;
    use crate::executor::TokioExecutor;
    use crate::state::{
        AuthorizationRequested, AuthorizationStatus, ChargePointEvent, ConnectorEvent,
        ConnectorState, EvseEvent, IdToken, IdTokenKind,
    };
    use crate::sync::broadcast_channel;
    use alloc::boxed::Box;

    fn test_id_token() -> IdToken {
        IdToken {
            value: "04A224B2".into(),
            kind: IdTokenKind::ISO14443,
        }
    }

    struct FixedAuthorizer(AuthorizationStatus);

    #[async_trait::async_trait]
    impl Authorizer for FixedAuthorizer {
        type Error = core::convert::Infallible;

        async fn authorize(&self, _id_token: &IdToken) -> Result<AuthorizationStatus, Self::Error> {
            Ok(self.0)
        }
    }

    /// Spawns an actor with connector 0 already in `Authorizing` (an id token has been
    /// presented, and the CSMS's decision is pending).
    async fn authorizing_actor() -> ChargePointActor {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        for event in [
            ConnectorEvent::CableConnected,
            ConnectorEvent::LockConfirmed,
            ConnectorEvent::IdTokenPresented(test_id_token()),
        ] {
            actor
                .send(ChargePointEvent::Evse {
                    evse_id: 0,
                    event: EvseEvent::Connector {
                        connector_id: 0,
                        event,
                    },
                })
                .await
                .unwrap();
        }
        actor
    }

    #[tokio::test]
    async fn an_accepted_decision_authorizes_charging() {
        let actor = authorizing_actor().await;
        let sender = broadcast_channel();
        let receiver = sender.subscribe();
        let authorizer = FixedAuthorizer(AuthorizationStatus::Accepted);

        sender.send(AuthorizationRequested {
            evse_id: 0,
            connector_id: 0,
            id_token: test_id_token(),
            contract: None,
        });
        // Dropping the sender closes the channel, which ends `run_authorization_requests`'s
        // loop once it has processed the request above.
        drop(sender);

        run_authorization_requests(
            receiver,
            &authorizer,
            actor.clone(),
            &crate::clock::SystemClock,
        )
        .await;

        assert_eq!(
            actor.state().evses[0].connectors[0],
            ConnectorState::Starting
        );
    }

    #[tokio::test]
    async fn a_rejected_decision_leaves_the_connector_locked() {
        let actor = authorizing_actor().await;
        let sender = broadcast_channel();
        let receiver = sender.subscribe();
        let authorizer = FixedAuthorizer(AuthorizationStatus::Rejected);

        sender.send(AuthorizationRequested {
            evse_id: 0,
            connector_id: 0,
            id_token: test_id_token(),
            contract: None,
        });
        drop(sender);

        run_authorization_requests(
            receiver,
            &authorizer,
            actor.clone(),
            &crate::clock::SystemClock,
        )
        .await;

        assert_eq!(actor.state().evses[0].connectors[0], ConnectorState::Locked);
    }

    // --- C07: authorization using contract certificates -----------------------------------

    use super::{ContractAuthorization, ContractCertificate, ContractCertificateStatus};
    use crate::hardware::{CertificateHashData, HashAlgorithm, OcspCertificateId};
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicBool, Ordering};

    fn test_contract() -> ContractCertificate {
        ContractCertificate {
            chain_pem: Some("-----BEGIN CERTIFICATE-----".into()),
            ocsp_data: alloc::vec![OcspCertificateId {
                hash_data: CertificateHashData {
                    hash_algorithm: HashAlgorithm::Sha256,
                    issuer_name_hash: "a".into(),
                    issuer_key_hash: "b".into(),
                    serial_number: "01".into(),
                },
                responder_url: "http://ocsp.example.com".into(),
            }],
        }
    }

    /// An `Authorizer` that answers contract authorizations with a fixed verdict, records the
    /// contract it was handed, and fails every *plain* authorization - so a test that ends up on
    /// the plain path fails loudly rather than passing for the wrong reason.
    struct RecordingContractAuthorizer {
        verdict: ContractAuthorization,
        seen: Arc<std::sync::Mutex<Option<ContractCertificate>>>,
    }

    #[async_trait::async_trait]
    impl Authorizer for RecordingContractAuthorizer {
        type Error = core::convert::Infallible;

        async fn authorize(&self, _id_token: &IdToken) -> Result<AuthorizationStatus, Self::Error> {
            panic!("a contract presentation must not fall through to the plain Authorize path");
        }

        async fn authorize_contract(
            &self,
            _id_token: &IdToken,
            contract: &ContractCertificate,
        ) -> Result<ContractAuthorization, Self::Error> {
            *self.seen.lock().unwrap() = Some(contract.clone());
            Ok(self.verdict)
        }
    }

    /// An `Authorizer` whose contract authorization always fails at the transport layer, standing
    /// in for an unreachable CSMS.
    struct OfflineContractAuthorizer;

    #[derive(Debug)]
    struct Unreachable;

    impl core::fmt::Display for Unreachable {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str("the CSMS is unreachable")
        }
    }

    impl core::error::Error for Unreachable {}

    #[async_trait::async_trait]
    impl Authorizer for OfflineContractAuthorizer {
        type Error = Unreachable;

        async fn authorize(&self, _id_token: &IdToken) -> Result<AuthorizationStatus, Self::Error> {
            Err(Unreachable)
        }

        async fn authorize_contract(
            &self,
            _id_token: &IdToken,
            _contract: &ContractCertificate,
        ) -> Result<ContractAuthorization, Self::Error> {
            Err(Unreachable)
        }
    }

    /// Drives one contract presentation through `run_authorization_requests` and returns the
    /// connector state it left behind.
    async fn present_contract<A: Authorizer + Sync>(
        actor: &ChargePointActor,
        authorizer: &A,
    ) -> ConnectorState {
        let sender = broadcast_channel();
        let receiver = sender.subscribe();
        sender.send(AuthorizationRequested {
            evse_id: 0,
            connector_id: 0,
            id_token: test_id_token(),
            contract: Some(test_contract()),
        });
        drop(sender);

        run_authorization_requests(
            receiver,
            authorizer,
            actor.clone(),
            &crate::clock::SystemClock,
        )
        .await;
        actor.state().evses[0].connectors[0]
    }

    #[tokio::test]
    async fn a_contract_authorization_starts_charging_when_token_and_certificate_are_both_accepted()
    {
        let actor = authorizing_actor().await;
        let authorizer = RecordingContractAuthorizer {
            verdict: ContractAuthorization {
                status: AuthorizationStatus::Accepted,
                certificate_status: Some(ContractCertificateStatus::Accepted),
            },
            seen: Arc::new(std::sync::Mutex::new(None)),
        };

        assert_eq!(
            present_contract(&actor, &authorizer).await,
            ConnectorState::Starting
        );
    }

    /// C07.FR.16 pairs a revoked certificate with an `Invalid` token, but a station that only
    /// read the token half would start charging if a CSMS ever sent `Accepted` alongside a bad
    /// certificate. It must not.
    #[tokio::test]
    async fn a_revoked_certificate_denies_charging_even_if_the_token_was_accepted() {
        let actor = authorizing_actor().await;
        let authorizer = RecordingContractAuthorizer {
            verdict: ContractAuthorization {
                status: AuthorizationStatus::Accepted,
                certificate_status: Some(ContractCertificateStatus::CertificateRevoked),
            },
            seen: Arc::new(std::sync::Mutex::new(None)),
        };

        assert_eq!(
            present_contract(&actor, &authorizer).await,
            ConnectorState::Locked
        );
    }

    /// A CSMS that says nothing about the certificate has no opinion, not a negative one - the
    /// normal answer when the charge point sent no certificate material to have one about.
    #[tokio::test]
    async fn an_absent_certificate_status_defers_to_the_token_decision() {
        assert!(
            ContractAuthorization {
                status: AuthorizationStatus::Accepted,
                certificate_status: None,
            }
            .accepted()
        );
        assert!(
            !ContractAuthorization {
                status: AuthorizationStatus::Rejected,
                certificate_status: Some(ContractCertificateStatus::Accepted),
            }
            .accepted()
        );
    }

    /// C07.FR.07: offline, with no ability to validate the contract locally, the answer is
    /// refusal - *not* the cache fallback every other kind of token gets. The cache here holds an
    /// acceptance for this very token, so a station taking the ordinary offline path would start
    /// charging.
    #[tokio::test]
    async fn an_offline_contract_authorization_refuses_instead_of_using_the_cache() {
        let actor = authorizing_actor().await;
        actor
            .send(ChargePointEvent::AuthorizationCached {
                id_token: test_id_token(),
                status: AuthorizationStatus::Accepted,
                cached_at: None,
            })
            .await
            .unwrap();

        assert_eq!(
            present_contract(&actor, &OfflineContractAuthorizer).await,
            ConnectorState::Locked
        );
    }

    /// The same offline presentation *without* a contract certificate is accepted from that same
    /// cache - which is what makes the test above a statement about C07 rather than about the
    /// cache being empty.
    #[tokio::test]
    async fn the_same_token_without_a_contract_still_authorizes_offline_from_the_cache() {
        let actor = authorizing_actor().await;
        actor
            .send(ChargePointEvent::AuthorizationCached {
                id_token: test_id_token(),
                status: AuthorizationStatus::Accepted,
                cached_at: None,
            })
            .await
            .unwrap();

        let sender = broadcast_channel();
        let receiver = sender.subscribe();
        sender.send(AuthorizationRequested {
            evse_id: 0,
            connector_id: 0,
            id_token: test_id_token(),
            contract: None,
        });
        drop(sender);

        run_authorization_requests(
            receiver,
            &OfflineContractAuthorizer,
            actor.clone(),
            &crate::clock::SystemClock,
        )
        .await;

        assert_eq!(
            actor.state().evses[0].connectors[0],
            ConnectorState::Starting
        );
    }

    /// C07.FR.06: the PEM chain goes to the CSMS only when
    /// `ISO15118Ctrlr.CentralContractValidationAllowed` says so. The OCSP data goes either way -
    /// it is what C07.FR.02 requires of every Plug & Charge Authorize.
    #[tokio::test]
    async fn the_certificate_chain_is_withheld_unless_central_validation_is_allowed() {
        let seen = Arc::new(std::sync::Mutex::new(None));
        let authorizer = RecordingContractAuthorizer {
            verdict: ContractAuthorization {
                status: AuthorizationStatus::Rejected,
                certificate_status: None,
            },
            seen: seen.clone(),
        };

        let actor = authorizing_actor().await;
        present_contract(&actor, &authorizer).await;
        let withheld = seen
            .lock()
            .unwrap()
            .clone()
            .expect("the authorizer was called");
        assert_eq!(withheld.chain_pem, None, "withheld by default");
        assert_eq!(
            withheld.ocsp_data,
            test_contract().ocsp_data,
            "the OCSP data is never withheld"
        );

        let actor = authorizing_actor().await;
        set_central_contract_validation(&actor, true).await;
        present_contract(&actor, &authorizer).await;
        assert_eq!(
            seen.lock().unwrap().clone().unwrap().chain_pem,
            test_contract().chain_pem,
            "sent once the CSMS allows central validation"
        );
    }

    /// An `Authorizer` that has not implemented C07 refuses rather than quietly asking the CSMS
    /// about a bare eMAID nobody validated.
    #[tokio::test]
    async fn the_default_contract_authorization_refuses_locally() {
        struct PlainOnly;

        #[async_trait::async_trait]
        impl Authorizer for PlainOnly {
            type Error = core::convert::Infallible;

            async fn authorize(
                &self,
                _id_token: &IdToken,
            ) -> Result<AuthorizationStatus, Self::Error> {
                Ok(AuthorizationStatus::Accepted)
            }
        }

        let called = Arc::new(AtomicBool::new(false));
        let _ = &called;
        let actor = authorizing_actor().await;

        assert_eq!(
            present_contract(&actor, &PlainOnly).await,
            ConnectorState::Locked
        );
        assert!(!called.load(Ordering::SeqCst));
    }

    async fn set_central_contract_validation(actor: &ChargePointActor, allowed: bool) {
        use crate::state::{
            Component, DeviceModelEvent, Variable, VariableAttribute, VariableAttributeType,
            VariableCharacteristics, VariableDataType, VariableMutability,
        };
        actor
            .send(ChargePointEvent::DeviceModel(
                DeviceModelEvent::VariableRegistered {
                    component: Component {
                        name: "ISO15118Ctrlr".into(),
                        instance: None,
                        evse: None,
                    },
                    variable: Variable {
                        name: "CentralContractValidationAllowed".into(),
                        instance: None,
                    },
                    characteristics: VariableCharacteristics {
                        data_type: VariableDataType::Boolean,
                        unit: None,
                        min_limit: None,
                        max_limit: None,
                        values_list: None,
                        supports_monitoring: false,
                    },
                    attributes: alloc::vec![VariableAttribute {
                        attribute_type: VariableAttributeType::Actual,
                        value: allowed.to_string(),
                        mutability: VariableMutability::ReadWrite,
                        persistent: false,
                        constant: false,
                        requires_reboot: false,
                    }],
                },
            ))
            .await
            .unwrap();
    }
}

#[cfg(feature = "ocpp_2_1")]
mod ocpp_2_1 {
    use super::{
        Authorizer, ContractAuthorization, ContractCertificate, ContractCertificateStatus,
    };
    use crate::hardware::{HashAlgorithm, OcspCertificateId};
    use crate::state::{AuthorizationStatus, IdToken, IdTokenKind};
    use crate::wire::v21::AuthorizeRequest;
    use crate::wire::v21::common::{
        AuthorizationStatusEnum, AuthorizeCertificateStatusEnum, HashAlgorithmEnum,
        IdToken as WireIdToken, OCSPRequestData,
    };
    use alloc::boxed::Box;
    use ocpp_client::ClientError;
    use ocpp_client::ocpp_2_1::{OCPP2_1Client, OCPP2_1Error};

    pub(super) fn wire_type(kind: IdTokenKind) -> &'static str {
        match kind {
            IdTokenKind::Central => "Central",
            IdTokenKind::DirectPayment => "DirectPayment",
            IdTokenKind::EMAID => "eMAID",
            IdTokenKind::EVCCID => "EVCCID",
            IdTokenKind::ISO14443 => "ISO14443",
            IdTokenKind::ISO15693 => "ISO15693",
            IdTokenKind::KeyCode => "KeyCode",
            IdTokenKind::Local => "Local",
            IdTokenKind::MacAddress => "MacAddress",
            IdTokenKind::NoAuthorization => "NoAuthorization",
            IdTokenKind::Vin => "VIN",
        }
    }

    pub(super) fn build_request(id_token: &IdToken) -> AuthorizeRequest {
        AuthorizeRequest {
            custom_data: None,
            certificate: None,
            id_token: WireIdToken {
                additional_info: None,
                // `id_token.value` is our own already-validated internal id token; the wire
                // field's 255-byte bound comfortably covers every OCPP-supported identifier
                // format (RFID UIDs, eMAIDs, etc.), so this can't fail in practice.
                id_token: heapless::String::try_from(id_token.value.as_str())
                    .expect("id token value must fit in OCPP's 255-character bound"),
                r#type: heapless::String::try_from(wire_type(id_token.kind))
                    .expect("id token type name must fit in OCPP's 20-character bound"),
                custom_data: None,
            },
            iso15118_certificate_hash_data: None,
        }
    }

    /// Only `Accepted` maps to our `Accepted` - the wire enum's other 9 values (including
    /// `ConcurrentTx`, which per spec shouldn't necessarily stop an already-running
    /// transaction) all collapse to `Rejected` for now. See `docs/ROADMAP.md` §3.
    pub(super) fn map_status(status: AuthorizationStatusEnum) -> AuthorizationStatus {
        match status {
            AuthorizationStatusEnum::Accepted => AuthorizationStatus::Accepted,
            _ => AuthorizationStatus::Rejected,
        }
    }

    fn wire_hash_algorithm(algorithm: HashAlgorithm) -> HashAlgorithmEnum {
        match algorithm {
            HashAlgorithm::Sha256 => HashAlgorithmEnum::SHA256,
            HashAlgorithm::Sha384 => HashAlgorithmEnum::SHA384,
            HashAlgorithm::Sha512 => HashAlgorithmEnum::SHA512,
        }
    }

    /// One certificate's OCSP data onto the wire, or `None` when a field exceeds its bound -
    /// mirroring `crate::certificates`' `wire_hash_data`, and dropped rather than truncated for
    /// the same reason: half a hash identifies nothing.
    fn wire_ocsp_data(id: &OcspCertificateId) -> Option<OCSPRequestData> {
        Some(OCSPRequestData {
            custom_data: None,
            hash_algorithm: wire_hash_algorithm(id.hash_data.hash_algorithm),
            issuer_key_hash: heapless::String::try_from(id.hash_data.issuer_key_hash.as_str())
                .ok()?,
            issuer_name_hash: heapless::String::try_from(id.hash_data.issuer_name_hash.as_str())
                .ok()?,
            responder_u_r_l: id.responder_url.clone(),
            serial_number: heapless::String::try_from(id.hash_data.serial_number.as_str()).ok()?,
        })
    }

    /// The Plug & Charge `Authorize` (C07.FR.02): the plain request plus whatever certificate
    /// material fits.
    ///
    /// Every "does not fit" here is reported and dropped, never truncated or silently omitted: a
    /// CSMS that receives less than it should answers `CertChainError`, and an operator reading
    /// the log needs to see that it was this charge point that withheld it.
    pub(super) fn build_contract_request(
        id_token: &IdToken,
        contract: &ContractCertificate,
    ) -> AuthorizeRequest {
        let mut request = build_request(id_token);
        // The chain's own length bound (10000 characters on 2.1) is `ocpp-types`' to enforce at
        // send time, not this crate's to second-guess - see `docs/UPSTREAM-POLICY.md`.
        request.certificate = contract.chain_pem.clone();

        let mut hash_data = heapless::Vec::new();
        for id in &contract.ocsp_data {
            let Some(wire) = wire_ocsp_data(id) else {
                tracing::warn!(
                    "a contract certificate's OCSP data exceeds OCPP's field bounds; omitting \
                     that certificate from the Authorize"
                );
                continue;
            };
            if hash_data.push(wire).is_err() {
                tracing::warn!(
                    supplied = contract.ocsp_data.len(),
                    "more contract certificates than OCPP's 4-entry bound allows; the rest are \
                     omitted from the Authorize"
                );
                break;
            }
        }
        request.iso15118_certificate_hash_data = (!hash_data.is_empty()).then_some(hash_data);
        request
    }

    /// The CSMS's verdict on the certificate. Exhaustive with no wildcard arm on purpose: an
    /// added wire status must be a compile error, not a silently-accepted certificate.
    pub(super) fn map_certificate_status(
        status: AuthorizeCertificateStatusEnum,
    ) -> ContractCertificateStatus {
        match status {
            AuthorizeCertificateStatusEnum::Accepted => ContractCertificateStatus::Accepted,
            AuthorizeCertificateStatusEnum::SignatureError => {
                ContractCertificateStatus::SignatureError
            }
            AuthorizeCertificateStatusEnum::CertificateExpired => {
                ContractCertificateStatus::CertificateExpired
            }
            AuthorizeCertificateStatusEnum::CertificateRevoked => {
                ContractCertificateStatus::CertificateRevoked
            }
            AuthorizeCertificateStatusEnum::NoCertificateAvailable => {
                ContractCertificateStatus::NoCertificateAvailable
            }
            AuthorizeCertificateStatusEnum::CertChainError => {
                ContractCertificateStatus::CertChainError
            }
            AuthorizeCertificateStatusEnum::ContractCancelled => {
                ContractCertificateStatus::ContractCancelled
            }
        }
    }

    #[async_trait::async_trait]
    impl Authorizer for OCPP2_1Client {
        type Error = ClientError<OCPP2_1Error>;

        async fn authorize(&self, id_token: &IdToken) -> Result<AuthorizationStatus, Self::Error> {
            let response = self.send_authorize(build_request(id_token)).await?;
            Ok(map_status(response.id_token_info.status))
        }

        async fn authorize_contract(
            &self,
            id_token: &IdToken,
            contract: &ContractCertificate,
        ) -> Result<ContractAuthorization, Self::Error> {
            let response = self
                .send_authorize(build_contract_request(id_token, contract))
                .await?;
            Ok(ContractAuthorization {
                status: map_status(response.id_token_info.status),
                certificate_status: response.certificate_status.map(map_certificate_status),
            })
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_request_carries_the_token_value_and_wire_type() {
            let id_token = IdToken {
                value: "04A224B2".into(),
                kind: IdTokenKind::ISO14443,
            };

            let request = build_request(&id_token);

            assert_eq!(request.id_token.id_token, "04A224B2");
            assert_eq!(request.id_token.r#type, "ISO14443");
        }

        fn test_contract() -> ContractCertificate {
            use crate::hardware::{CertificateHashData, HashAlgorithm, OcspCertificateId};
            ContractCertificate {
                chain_pem: Some("-----BEGIN CERTIFICATE-----".into()),
                ocsp_data: alloc::vec![OcspCertificateId {
                    hash_data: CertificateHashData {
                        hash_algorithm: HashAlgorithm::Sha384,
                        issuer_name_hash: "name-hash".into(),
                        issuer_key_hash: "key-hash".into(),
                        serial_number: "01AF".into(),
                    },
                    responder_url: "http://ocsp.example.com".into(),
                }],
            }
        }

        #[test]
        fn a_contract_request_carries_the_emaid_the_chain_and_the_ocsp_data() {
            let id_token = IdToken {
                value: "DE8ACC12E4321".into(),
                kind: IdTokenKind::EMAID,
            };

            let request = build_contract_request(&id_token, &test_contract());

            assert_eq!(request.id_token.r#type, "eMAID");
            assert_eq!(request.certificate, test_contract().chain_pem);
            let hash_data = request.iso15118_certificate_hash_data.unwrap();
            assert_eq!(hash_data.len(), 1);
            assert_eq!(hash_data[0].serial_number, "01AF");
            assert_eq!(hash_data[0].responder_u_r_l, "http://ocsp.example.com");
            assert!(matches!(
                hash_data[0].hash_algorithm,
                HashAlgorithmEnum::SHA384
            ));
        }

        /// OCPP bounds the hash data at four entries. A fifth certificate is dropped, not
        /// squeezed in over another one, and never silently: the CSMS validates a shorter chain
        /// than the vehicle presented and the log has to say so.
        #[test]
        fn a_fifth_certificate_is_dropped_rather_than_replacing_one_of_the_four() {
            let id_token = IdToken {
                value: "DE8ACC12E4321".into(),
                kind: IdTokenKind::EMAID,
            };
            let mut contract = test_contract();
            let entry = contract.ocsp_data[0].clone();
            contract.ocsp_data = alloc::vec![entry; 5];

            let request = build_contract_request(&id_token, &contract);

            assert_eq!(request.iso15118_certificate_hash_data.unwrap().len(), 4);
        }

        /// A contract with no material at all is still a valid Authorize - the CSMS decides on
        /// the eMAID - and must not send an empty `iso15118CertificateHashData` array, which
        /// OCPP's own schema forbids.
        #[test]
        fn a_contract_with_no_material_sends_neither_field() {
            let id_token = IdToken {
                value: "DE8ACC12E4321".into(),
                kind: IdTokenKind::EMAID,
            };

            let request = build_contract_request(&id_token, &ContractCertificate::default());

            assert_eq!(request.certificate, None);
            assert_eq!(request.iso15118_certificate_hash_data, None);
        }

        #[test]
        fn every_wire_certificate_status_maps_to_its_own_value() {
            use ContractCertificateStatus as Status;
            for (wire, expected) in [
                (AuthorizeCertificateStatusEnum::Accepted, Status::Accepted),
                (
                    AuthorizeCertificateStatusEnum::SignatureError,
                    Status::SignatureError,
                ),
                (
                    AuthorizeCertificateStatusEnum::CertificateExpired,
                    Status::CertificateExpired,
                ),
                (
                    AuthorizeCertificateStatusEnum::CertificateRevoked,
                    Status::CertificateRevoked,
                ),
                (
                    AuthorizeCertificateStatusEnum::NoCertificateAvailable,
                    Status::NoCertificateAvailable,
                ),
                (
                    AuthorizeCertificateStatusEnum::CertChainError,
                    Status::CertChainError,
                ),
                (
                    AuthorizeCertificateStatusEnum::ContractCancelled,
                    Status::ContractCancelled,
                ),
            ] {
                assert_eq!(map_certificate_status(wire), expected);
            }
        }

        #[test]
        fn every_wire_kind_has_a_wire_type_string() {
            assert_eq!(wire_type(IdTokenKind::Central), "Central");
            assert_eq!(wire_type(IdTokenKind::DirectPayment), "DirectPayment");
            assert_eq!(wire_type(IdTokenKind::EMAID), "eMAID");
            assert_eq!(wire_type(IdTokenKind::EVCCID), "EVCCID");
            assert_eq!(wire_type(IdTokenKind::ISO14443), "ISO14443");
            assert_eq!(wire_type(IdTokenKind::ISO15693), "ISO15693");
            assert_eq!(wire_type(IdTokenKind::KeyCode), "KeyCode");
            assert_eq!(wire_type(IdTokenKind::Local), "Local");
            assert_eq!(wire_type(IdTokenKind::MacAddress), "MacAddress");
            assert_eq!(wire_type(IdTokenKind::NoAuthorization), "NoAuthorization");
            assert_eq!(wire_type(IdTokenKind::Vin), "VIN");
        }

        #[test]
        fn only_accepted_maps_to_accepted() {
            assert_eq!(
                map_status(AuthorizationStatusEnum::Accepted),
                AuthorizationStatus::Accepted
            );
            assert_eq!(
                map_status(AuthorizationStatusEnum::Blocked),
                AuthorizationStatus::Rejected
            );
            assert_eq!(
                map_status(AuthorizationStatusEnum::ConcurrentTx),
                AuthorizationStatus::Rejected
            );
            assert_eq!(
                map_status(AuthorizationStatusEnum::Unknown),
                AuthorizationStatus::Rejected
            );
        }
    }
}

/// The OCPP 2.0.1 projection - identical wire shape to 2.1's `AuthorizeRequest`/
/// `AuthorizationStatusEnum` (2.1 only widened the `certificate` field's byte bound, which this
/// crate always sends `None` for anyway), so this is close to a copy of the 2.1 one, just
/// targeting `OCPP2_0_1Client`.
#[cfg(feature = "ocpp_2_0_1")]
mod ocpp_2_0_1 {
    use super::{
        Authorizer, ContractAuthorization, ContractCertificate, ContractCertificateStatus,
    };
    use crate::hardware::{HashAlgorithm, OcspCertificateId};
    use crate::state::{AuthorizationStatus, IdToken, IdTokenKind};
    use crate::wire::v201::AuthorizeRequest;
    use crate::wire::v201::common::{
        AuthorizationStatusEnum, AuthorizeCertificateStatusEnum, HashAlgorithmEnum,
        IdToken as WireIdToken, IdTokenEnum, OCSPRequestData,
    };
    use alloc::boxed::Box;
    use ocpp_client::ClientError;
    use ocpp_client::ocpp_2_0_1::{OCPP2_0_1Client, OCPP2_0_1Error};

    /// Unlike 2.1's free-form `type` string (see [`super::ocpp_2_1::wire_type`]), 2.0.1's
    /// `IdTokenEnumType` is a closed 8-value enum with no catch-all - it has no
    /// `DirectPayment`/`EVCCID`/`Vin` equivalent (those were added, as free-form values, only
    /// once 2.1 dropped the enum). Each falls back to `Central` - "assigned/known centrally" is
    /// the closest existing meaning to "an identifier this crate can't name more precisely under
    /// 2.0.1" - rather than failing the whole request over a field the CSMS mostly uses for
    /// logging/UX, not authorization logic itself.
    pub(super) fn map_id_token_kind(kind: IdTokenKind) -> IdTokenEnum {
        match kind {
            IdTokenKind::Central
            | IdTokenKind::DirectPayment
            | IdTokenKind::EVCCID
            | IdTokenKind::Vin => IdTokenEnum::Central,
            IdTokenKind::EMAID => IdTokenEnum::EMAID,
            IdTokenKind::ISO14443 => IdTokenEnum::ISO14443,
            IdTokenKind::ISO15693 => IdTokenEnum::ISO15693,
            IdTokenKind::KeyCode => IdTokenEnum::KeyCode,
            IdTokenKind::Local => IdTokenEnum::Local,
            IdTokenKind::MacAddress => IdTokenEnum::MacAddress,
            IdTokenKind::NoAuthorization => IdTokenEnum::NoAuthorization,
        }
    }

    pub(super) fn build_request(id_token: &IdToken) -> AuthorizeRequest {
        AuthorizeRequest {
            custom_data: None,
            certificate: None,
            id_token: WireIdToken {
                additional_info: None,
                id_token: heapless::String::try_from(id_token.value.as_str())
                    .expect("id token value must fit in OCPP's 255-character bound"),
                r#type: map_id_token_kind(id_token.kind),
                custom_data: None,
            },
            iso15118_certificate_hash_data: None,
        }
    }

    /// Only `Accepted` maps to our `Accepted` - see [`super::ocpp_2_1::map_status`] for why the
    /// wire enum's other values all collapse to `Rejected`.
    pub(super) fn map_status(status: AuthorizationStatusEnum) -> AuthorizationStatus {
        match status {
            AuthorizationStatusEnum::Accepted => AuthorizationStatus::Accepted,
            _ => AuthorizationStatus::Rejected,
        }
    }

    fn wire_hash_algorithm(algorithm: HashAlgorithm) -> HashAlgorithmEnum {
        match algorithm {
            HashAlgorithm::Sha256 => HashAlgorithmEnum::SHA256,
            HashAlgorithm::Sha384 => HashAlgorithmEnum::SHA384,
            HashAlgorithm::Sha512 => HashAlgorithmEnum::SHA512,
        }
    }

    /// One certificate's OCSP data onto the wire - see [`super::ocpp_2_1`]'s twin for why a value
    /// that does not fit takes its whole entry with it.
    fn wire_ocsp_data(id: &OcspCertificateId) -> Option<OCSPRequestData> {
        Some(OCSPRequestData {
            custom_data: None,
            hash_algorithm: wire_hash_algorithm(id.hash_data.hash_algorithm),
            issuer_key_hash: heapless::String::try_from(id.hash_data.issuer_key_hash.as_str())
                .ok()?,
            issuer_name_hash: heapless::String::try_from(id.hash_data.issuer_name_hash.as_str())
                .ok()?,
            // 2.0.1 bounds the responder URL at 512 characters where 2.1 leaves it unbounded -
            // one of the few places the two versions' Authorize really differ.
            responder_u_r_l: heapless::String::try_from(id.responder_url.as_str()).ok()?,
            serial_number: heapless::String::try_from(id.hash_data.serial_number.as_str()).ok()?,
        })
    }

    /// The Plug & Charge `Authorize`. Identical to 2.1's in every field that matters here - C07
    /// is one of the few 2.1 use cases that is not new in 2.1 - so see
    /// [`super::ocpp_2_1::build_contract_request`] for the reasoning; only the PEM length bound
    /// `ocpp-types` enforces differs (5500 characters here, 10000 on 2.1).
    pub(super) fn build_contract_request(
        id_token: &IdToken,
        contract: &ContractCertificate,
    ) -> AuthorizeRequest {
        let mut request = build_request(id_token);
        request.certificate = contract.chain_pem.clone();

        let mut hash_data = heapless::Vec::new();
        for id in &contract.ocsp_data {
            let Some(wire) = wire_ocsp_data(id) else {
                tracing::warn!(
                    "a contract certificate's OCSP data exceeds OCPP's field bounds; omitting \
                     that certificate from the Authorize"
                );
                continue;
            };
            if hash_data.push(wire).is_err() {
                tracing::warn!(
                    supplied = contract.ocsp_data.len(),
                    "more contract certificates than OCPP's 4-entry bound allows; the rest are \
                     omitted from the Authorize"
                );
                break;
            }
        }
        request.iso15118_certificate_hash_data = (!hash_data.is_empty()).then_some(hash_data);
        request
    }

    /// The CSMS's verdict on the certificate - the same seven values 2.1 has, exhaustively.
    pub(super) fn map_certificate_status(
        status: AuthorizeCertificateStatusEnum,
    ) -> ContractCertificateStatus {
        match status {
            AuthorizeCertificateStatusEnum::Accepted => ContractCertificateStatus::Accepted,
            AuthorizeCertificateStatusEnum::SignatureError => {
                ContractCertificateStatus::SignatureError
            }
            AuthorizeCertificateStatusEnum::CertificateExpired => {
                ContractCertificateStatus::CertificateExpired
            }
            AuthorizeCertificateStatusEnum::CertificateRevoked => {
                ContractCertificateStatus::CertificateRevoked
            }
            AuthorizeCertificateStatusEnum::NoCertificateAvailable => {
                ContractCertificateStatus::NoCertificateAvailable
            }
            AuthorizeCertificateStatusEnum::CertChainError => {
                ContractCertificateStatus::CertChainError
            }
            AuthorizeCertificateStatusEnum::ContractCancelled => {
                ContractCertificateStatus::ContractCancelled
            }
        }
    }

    #[async_trait::async_trait]
    impl Authorizer for OCPP2_0_1Client {
        type Error = ClientError<OCPP2_0_1Error>;

        async fn authorize(&self, id_token: &IdToken) -> Result<AuthorizationStatus, Self::Error> {
            let response = self.send_authorize(build_request(id_token)).await?;
            Ok(map_status(response.id_token_info.status))
        }

        async fn authorize_contract(
            &self,
            id_token: &IdToken,
            contract: &ContractCertificate,
        ) -> Result<ContractAuthorization, Self::Error> {
            let response = self
                .send_authorize(build_contract_request(id_token, contract))
                .await?;
            Ok(ContractAuthorization {
                status: map_status(response.id_token_info.status),
                certificate_status: response.certificate_status.map(map_certificate_status),
            })
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_request_carries_the_token_value_and_wire_type() {
            let id_token = IdToken {
                value: "04A224B2".into(),
                kind: IdTokenKind::ISO14443,
            };

            let request = build_request(&id_token);

            assert_eq!(request.id_token.id_token, "04A224B2");
            assert_eq!(request.id_token.r#type, IdTokenEnum::ISO14443);
        }

        #[test]
        fn every_representable_kind_maps_to_its_own_wire_variant() {
            assert_eq!(
                map_id_token_kind(IdTokenKind::Central),
                IdTokenEnum::Central
            );
            assert_eq!(map_id_token_kind(IdTokenKind::EMAID), IdTokenEnum::EMAID);
            assert_eq!(
                map_id_token_kind(IdTokenKind::ISO14443),
                IdTokenEnum::ISO14443
            );
            assert_eq!(
                map_id_token_kind(IdTokenKind::ISO15693),
                IdTokenEnum::ISO15693
            );
            assert_eq!(
                map_id_token_kind(IdTokenKind::KeyCode),
                IdTokenEnum::KeyCode
            );
            assert_eq!(map_id_token_kind(IdTokenKind::Local), IdTokenEnum::Local);
            assert_eq!(
                map_id_token_kind(IdTokenKind::MacAddress),
                IdTokenEnum::MacAddress
            );
            assert_eq!(
                map_id_token_kind(IdTokenKind::NoAuthorization),
                IdTokenEnum::NoAuthorization
            );
        }

        /// 2.0.1 carries the same Plug & Charge material as 2.1 - C07 is not a 2.1 addition -
        /// with one real difference: its responder URL is bounded, so one that does not fit takes
        /// its entry with it rather than being truncated into a URL pointing somewhere else.
        #[test]
        fn a_contract_request_carries_the_same_material_and_drops_an_oversized_responder_url() {
            use crate::hardware::{CertificateHashData, HashAlgorithm, OcspCertificateId};

            let id_token = IdToken {
                value: "DE8ACC12E4321".into(),
                kind: IdTokenKind::EMAID,
            };
            let entry = |responder_url: alloc::string::String| OcspCertificateId {
                hash_data: CertificateHashData {
                    hash_algorithm: HashAlgorithm::Sha256,
                    issuer_name_hash: "name-hash".into(),
                    issuer_key_hash: "key-hash".into(),
                    serial_number: "01AF".into(),
                },
                responder_url,
            };

            let request = build_contract_request(
                &id_token,
                &ContractCertificate {
                    chain_pem: Some("-----BEGIN CERTIFICATE-----".into()),
                    ocsp_data: alloc::vec![entry("http://ocsp.example.com".into())],
                },
            );
            assert!(matches!(request.id_token.r#type, IdTokenEnum::EMAID));
            assert!(request.certificate.is_some());
            assert_eq!(request.iso15118_certificate_hash_data.unwrap().len(), 1);

            let too_long = alloc::string::String::from_iter(core::iter::repeat_n('u', 513));
            let request = build_contract_request(
                &id_token,
                &ContractCertificate {
                    chain_pem: None,
                    ocsp_data: alloc::vec![entry(too_long)],
                },
            );
            assert_eq!(
                request.iso15118_certificate_hash_data, None,
                "the only entry did not fit, so there is nothing to send"
            );
        }

        #[test]
        fn kinds_with_no_twenty_zero_one_equivalent_fall_back_to_central() {
            assert_eq!(
                map_id_token_kind(IdTokenKind::DirectPayment),
                IdTokenEnum::Central
            );
            assert_eq!(map_id_token_kind(IdTokenKind::EVCCID), IdTokenEnum::Central);
            assert_eq!(map_id_token_kind(IdTokenKind::Vin), IdTokenEnum::Central);
        }

        #[test]
        fn only_accepted_maps_to_accepted() {
            assert_eq!(
                map_status(AuthorizationStatusEnum::Accepted),
                AuthorizationStatus::Accepted
            );
            assert_eq!(
                map_status(AuthorizationStatusEnum::Blocked),
                AuthorizationStatus::Rejected
            );
            assert_eq!(
                map_status(AuthorizationStatusEnum::ConcurrentTx),
                AuthorizationStatus::Rejected
            );
            assert_eq!(
                map_status(AuthorizationStatusEnum::Unknown),
                AuthorizationStatus::Rejected
            );
        }
    }
}

/// The OCPP 1.6J projection - the simplest `Authorize` shape of the three: 1.6J's `AuthorizeRequest`
/// carries only a bare `idTag` (no type/kind metadata at all, unlike every later version's
/// `IdTokenType`), so [`crate::id_tag::map_id_tag`] (shared with the Transactions block's 1.6J
/// adapter) covers the whole request; there's no `wire_type`/`map_id_token_kind`-style mapping to
/// write here, since there's no wire field for `IdTokenKind` to go into. `IdTagInfoStatus` is
/// also narrower than later versions' `AuthorizationStatusEnum` (5 values instead of 10 - no
/// `NotAllowedTypeEVSE`/`NotAtThisLocation`/etc., 1.6J predates EVSE-scoped authorization
/// entirely), but every value it does have already collapses to `Rejected` except `Accepted`, so
/// that narrowing doesn't change this mapping's shape at all.
#[cfg(feature = "ocpp_1_6")]
pub(crate) mod ocpp_1_6 {
    use super::{Authorizer, ContractAuthorization, ContractCertificate};
    use crate::id_tag::map_id_tag;
    use crate::state::{AuthorizationStatus, IdToken};
    use crate::wire::v16::AuthorizeRequest;
    use crate::wire::v16::common::IdTagInfoStatus;
    use alloc::boxed::Box;
    use ocpp_client::ClientError;
    use ocpp_client::ocpp_1_6::{OCPP1_6Client, OCPP1_6Error};

    pub(super) fn build_request(id_token: &IdToken) -> AuthorizeRequest {
        AuthorizeRequest {
            id_tag: map_id_tag(Some(id_token)),
        }
    }

    /// Only `Accepted` maps to our `Accepted` - mirrors [`super::ocpp_2_1::map_status`], just
    /// over 1.6J's narrower 5-value `IdTagInfoStatus`.
    pub(crate) fn map_status(status: IdTagInfoStatus) -> AuthorizationStatus {
        match status {
            IdTagInfoStatus::Accepted => AuthorizationStatus::Accepted,
            _ => AuthorizationStatus::Rejected,
        }
    }

    #[async_trait::async_trait]
    impl Authorizer for OCPP1_6Client {
        type Error = ClientError<OCPP1_6Error>;

        async fn authorize(&self, id_token: &IdToken) -> Result<AuthorizationStatus, Self::Error> {
            let response = self.send_authorize(build_request(id_token)).await?;
            Ok(map_status(response.id_tag_info.status))
        }

        /// 1.6J's `Authorize` carries an `idTag` and nothing else - no certificate, no OCSP data,
        /// no `certificateStatus` to answer with. Plug & Charge as OCPP 2.x defines it (C07)
        /// simply does not exist here.
        ///
        /// This asks anyway, on the eMAID alone, and says so in the log. That is the graceful
        /// downgrade `CLAUDE.md` asks for - project the 2.1 model onto what the negotiated
        /// version can express - and it matches what 1.6J sites running Plug & Charge actually
        /// do: the eMAID becomes an ordinary `idTag` and the CSMS decides on the contract it
        /// already knows about. **Nobody validates the contract certificate in this exchange**,
        /// which is exactly why the crate-wide default refuses instead: an `Authorizer` that has
        /// simply not implemented C07 has made no such decision, where this adapter has, on the
        /// record, with the protocol's own limits as the reason.
        async fn authorize_contract(
            &self,
            id_token: &IdToken,
            _contract: &ContractCertificate,
        ) -> Result<ContractAuthorization, Self::Error> {
            tracing::warn!(
                "authorizing an ISO 15118 contract on OCPP 1.6J: the certificate cannot be \
                 carried, so the CSMS decides on the eMAID alone and nothing validates the \
                 contract certificate"
            );
            let response = self.send_authorize(build_request(id_token)).await?;
            Ok(ContractAuthorization {
                status: map_status(response.id_tag_info.status),
                certificate_status: None,
            })
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::state::IdTokenKind;

        #[test]
        fn the_request_carries_the_token_value_with_no_type_metadata() {
            let id_token = IdToken {
                value: "04A224B2".into(),
                kind: IdTokenKind::ISO14443,
            };

            let request = build_request(&id_token);

            assert_eq!(request.id_tag.as_str(), "04A224B2");
        }

        #[test]
        fn only_accepted_maps_to_accepted() {
            assert_eq!(
                map_status(IdTagInfoStatus::Accepted),
                AuthorizationStatus::Accepted
            );
            assert_eq!(
                map_status(IdTagInfoStatus::Blocked),
                AuthorizationStatus::Rejected
            );
            assert_eq!(
                map_status(IdTagInfoStatus::Expired),
                AuthorizationStatus::Rejected
            );
            assert_eq!(
                map_status(IdTagInfoStatus::Invalid),
                AuthorizationStatus::Rejected
            );
            assert_eq!(
                map_status(IdTagInfoStatus::ConcurrentTx),
                AuthorizationStatus::Rejected
            );
        }
    }
}

#[cfg(test)]
mod loop_tests {
    use super::{Authorizer, run_authorization_requests};
    use crate::actor::ChargePointActor;
    use crate::clock::Clock;
    use crate::executor::TokioExecutor;
    use crate::state::{
        AuthorizationRequested, AuthorizationStatus, ConnectorState, IdToken, IdTokenKind,
    };
    use crate::sync::broadcast_channel;
    use alloc::boxed::Box;
    use chrono::{DateTime, Utc};

    struct FixedClock;

    impl Clock for FixedClock {
        fn now(&self) -> DateTime<Utc> {
            DateTime::from_timestamp(1_800_000_000, 0).unwrap()
        }
    }

    fn token() -> IdToken {
        IdToken {
            value: "04A224B2".into(),
            kind: IdTokenKind::ISO14443,
        }
    }

    /// An authorizer that answers `Accepted` until `offline` is set, and fails afterwards - the
    /// shape of a CSMS link that drops mid-shift.
    struct FlakyAuthorizer {
        offline: alloc::sync::Arc<core::sync::atomic::AtomicBool>,
    }

    #[derive(Debug)]
    struct Offline;

    impl core::fmt::Display for Offline {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str("the CSMS is unreachable")
        }
    }

    impl core::error::Error for Offline {}

    #[async_trait::async_trait]
    impl Authorizer for FlakyAuthorizer {
        type Error = Offline;

        async fn authorize(&self, _id_token: &IdToken) -> Result<AuthorizationStatus, Self::Error> {
            if self.offline.load(core::sync::atomic::Ordering::SeqCst) {
                return Err(Offline);
            }
            Ok(AuthorizationStatus::Accepted)
        }
    }

    async fn present_token(
        actor: &ChargePointActor,
        sender: &crate::sync::BroadcastSender<AuthorizationRequested>,
    ) {
        use crate::state::{ChargePointEvent, ConnectorEvent, EvseEvent};
        for event in [
            ConnectorEvent::CableConnected,
            ConnectorEvent::LockConfirmed,
            ConnectorEvent::IdTokenPresented(token()),
        ] {
            let _ = actor
                .send(ChargePointEvent::Evse {
                    evse_id: 0,
                    event: EvseEvent::Connector {
                        connector_id: 0,
                        event,
                    },
                })
                .await;
        }
        sender.send(AuthorizationRequested {
            evse_id: 0,
            connector_id: 0,
            id_token: token(),
            contract: None,
        });
    }

    async fn settle() {
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
    }

    /// The end-to-end guarantee B1.2 exists for: a token the CSMS accepted while online still
    /// charges when the link is down, and it does so *because it was cached*, not because failure
    /// is treated as success.
    #[tokio::test]
    async fn a_token_authorized_while_online_still_charges_once_the_link_drops() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let offline = alloc::sync::Arc::new(core::sync::atomic::AtomicBool::new(false));
        let sender = broadcast_channel();
        let receiver = sender.subscribe();
        let authorizer = FlakyAuthorizer {
            offline: offline.clone(),
        };
        let task_actor = actor.clone();
        tokio::spawn(async move {
            run_authorization_requests(receiver, &authorizer, task_actor, &FixedClock).await;
        });

        // Online: the CSMS accepts, and the decision is remembered.
        present_token(&actor, &sender).await;
        settle().await;
        assert_eq!(
            actor.state().evses[0].connectors[0],
            ConnectorState::Starting
        );
        assert_eq!(actor.state().authorization_cache.len(), 1);

        // The session ends and the link drops.
        for event in [
            crate::state::ConnectorEvent::ChargingStopped(crate::state::StopReason::Local),
            crate::state::ConnectorEvent::ContactorOpened,
            crate::state::ConnectorEvent::UnlockConfirmed,
        ] {
            let _ = actor
                .send(crate::state::ChargePointEvent::Evse {
                    evse_id: 0,
                    event: crate::state::EvseEvent::Connector {
                        connector_id: 0,
                        event,
                    },
                })
                .await;
        }
        offline.store(true, core::sync::atomic::Ordering::SeqCst);

        // Offline: the same token is presented again and still starts a session.
        present_token(&actor, &sender).await;
        settle().await;
        assert_eq!(
            actor.state().evses[0].connectors[0],
            ConnectorState::Starting
        );
    }

    #[tokio::test]
    async fn a_token_never_seen_before_is_denied_once_the_link_drops() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let offline = alloc::sync::Arc::new(core::sync::atomic::AtomicBool::new(true));
        let sender = broadcast_channel();
        let receiver = sender.subscribe();
        let authorizer = FlakyAuthorizer { offline };
        let task_actor = actor.clone();
        tokio::spawn(async move {
            run_authorization_requests(receiver, &authorizer, task_actor, &FixedClock).await;
        });

        present_token(&actor, &sender).await;
        settle().await;

        // Back to `Locked`, not `Starting`: an unreachable CSMS plus an unknown token is a denial.
        assert_eq!(actor.state().evses[0].connectors[0], ConnectorState::Locked);
        assert!(actor.state().authorization_cache.is_empty());
    }
}

#[cfg(test)]
mod offline_tests {
    use super::{ClearCacheOutcome, handle_clear_cache, offline_decision};
    use crate::actor::ChargePointActor;
    use crate::executor::TokioExecutor;
    use crate::state::{
        AuthorizationStatus, ChargePointEvent, DeviceModelEvent, IdToken, IdTokenKind,
        LocalListEntry, VariableAttributeType,
    };
    use chrono::{DateTime, Utc};

    fn token(value: &str) -> IdToken {
        IdToken {
            value: value.into(),
            kind: IdTokenKind::ISO14443,
        }
    }

    fn at(secs: i64) -> Option<DateTime<Utc>> {
        DateTime::from_timestamp(1_800_000_000 + secs, 0)
    }

    async fn set_variable(actor: &ChargePointActor, component: &str, variable: &str, value: &str) {
        let _ = actor
            .send(ChargePointEvent::DeviceModel(
                DeviceModelEvent::AttributeValueSet {
                    component: crate::state::Component {
                        name: component.into(),
                        instance: None,
                        evse: None,
                    },
                    variable: crate::state::Variable {
                        name: variable.into(),
                        instance: None,
                    },
                    attribute_type: VariableAttributeType::Actual,
                    value: value.into(),
                },
            ))
            .await;
    }

    async fn cache(actor: &ChargePointActor, value: &str, status: AuthorizationStatus) {
        let _ = actor
            .send(ChargePointEvent::AuthorizationCached {
                id_token: token(value),
                status,
                cached_at: at(0),
            })
            .await;
    }

    #[tokio::test]
    async fn an_unknown_token_is_denied_offline() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        assert_eq!(
            offline_decision(&actor.state(), &token("A"), at(1)),
            AuthorizationStatus::Rejected,
            "\"we don't know\" is not \"yes\""
        );
    }

    #[tokio::test]
    async fn a_cached_decision_answers_offline_in_both_directions() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        cache(&actor, "A", AuthorizationStatus::Accepted).await;
        cache(&actor, "B", AuthorizationStatus::Rejected).await;

        assert_eq!(
            offline_decision(&actor.state(), &token("A"), at(1)),
            AuthorizationStatus::Accepted
        );
        // A cached rejection must be honoured too - otherwise a revoked card gets in every time
        // the link drops.
        assert_eq!(
            offline_decision(&actor.state(), &token("B"), at(1)),
            AuthorizationStatus::Rejected
        );
    }

    #[tokio::test]
    async fn the_local_authorization_list_outranks_the_cache() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        cache(&actor, "A", AuthorizationStatus::Accepted).await;
        // The operator has since pushed a list that rejects it. The list is a deliberate
        // decision; the cache is an observation.
        let _ = actor
            .send(ChargePointEvent::LocalListUpdated {
                version: 1,
                entries: alloc::vec![LocalListEntry {
                    id_token: token("A"),
                    status: AuthorizationStatus::Rejected,
                }],
            })
            .await;

        assert_eq!(
            offline_decision(&actor.state(), &token("A"), at(1)),
            AuthorizationStatus::Rejected
        );
    }

    #[tokio::test]
    async fn an_expired_cache_entry_does_not_answer() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        cache(&actor, "A", AuthorizationStatus::Accepted).await;
        set_variable(&actor, "AuthCacheCtrlr", "LifeTime", "60").await;

        assert_eq!(
            offline_decision(&actor.state(), &token("A"), at(59)),
            AuthorizationStatus::Accepted
        );
        assert_eq!(
            offline_decision(&actor.state(), &token("A"), at(60)),
            AuthorizationStatus::Rejected
        );
    }

    #[tokio::test]
    async fn disabling_the_cache_stops_it_answering_but_leaves_the_list_working() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        cache(&actor, "A", AuthorizationStatus::Accepted).await;
        let _ = actor
            .send(ChargePointEvent::LocalListUpdated {
                version: 1,
                entries: alloc::vec![LocalListEntry {
                    id_token: token("B"),
                    status: AuthorizationStatus::Accepted,
                }],
            })
            .await;
        set_variable(&actor, "AuthCacheCtrlr", "Enabled", "false").await;

        assert_eq!(
            offline_decision(&actor.state(), &token("A"), at(1)),
            AuthorizationStatus::Rejected
        );
        assert_eq!(
            offline_decision(&actor.state(), &token("B"), at(1)),
            AuthorizationStatus::Accepted
        );
    }

    /// C15 (CV2.9): an identifier neither the list nor the cache knows. Denied by default -
    /// charging an unknown card offline hands out energy that may never be billed, which is an
    /// operator's decision - and accepted once the operator opts in.
    #[tokio::test]
    async fn an_unknown_identifier_offline_is_denied_unless_the_operator_opted_in() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        assert_eq!(
            offline_decision(&actor.state(), &token("NEVER-SEEN"), at(1)),
            AuthorizationStatus::Rejected,
            "the default is to refuse an identifier nothing knows about"
        );

        set_variable(&actor, "AuthCtrlr", "OfflineTxForUnknownIdEnabled", "true").await;

        assert_eq!(
            offline_decision(&actor.state(), &token("NEVER-SEEN"), at(1)),
            AuthorizationStatus::Accepted
        );
    }

    /// The switch does not override the *stronger* signals above it: an identifier the CSMS
    /// explicitly rejected stays rejected, whatever the unknown-id policy says. It answers only
    /// the "nothing knows about this one" case.
    #[tokio::test]
    async fn opting_in_to_unknown_identifiers_does_not_override_a_known_rejection() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let _ = actor
            .send(ChargePointEvent::LocalListUpdated {
                version: 1,
                entries: alloc::vec![LocalListEntry {
                    id_token: token("REFUSED"),
                    status: AuthorizationStatus::Rejected,
                }],
            })
            .await;
        set_variable(&actor, "AuthCtrlr", "OfflineTxForUnknownIdEnabled", "true").await;

        assert_eq!(
            offline_decision(&actor.state(), &token("REFUSED"), at(1)),
            AuthorizationStatus::Rejected
        );
    }

    #[tokio::test]
    async fn disabling_offline_authorization_stops_both_of_them() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        cache(&actor, "A", AuthorizationStatus::Accepted).await;
        let _ = actor
            .send(ChargePointEvent::LocalListUpdated {
                version: 1,
                entries: alloc::vec![LocalListEntry {
                    id_token: token("B"),
                    status: AuthorizationStatus::Accepted,
                }],
            })
            .await;
        set_variable(&actor, "AuthCtrlr", "LocalAuthorizeOffline", "false").await;

        for value in ["A", "B"] {
            assert_eq!(
                offline_decision(&actor.state(), &token(value), at(1)),
                AuthorizationStatus::Rejected
            );
        }
    }

    #[tokio::test]
    async fn clearing_the_cache_is_accepted_and_actually_empties_it() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        cache(&actor, "A", AuthorizationStatus::Accepted).await;

        assert_eq!(
            handle_clear_cache(&actor).await,
            ClearCacheOutcome::Accepted
        );
        assert!(actor.state().authorization_cache.is_empty());
        assert_eq!(
            offline_decision(&actor.state(), &token("A"), at(1)),
            AuthorizationStatus::Rejected
        );
    }

    #[tokio::test]
    async fn clearing_an_already_empty_cache_is_still_accepted() {
        // The CSMS asked for "no cached decisions", and that is the resulting state either way.
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        assert_eq!(
            handle_clear_cache(&actor).await,
            ClearCacheOutcome::Accepted
        );
    }

    #[tokio::test]
    async fn clearing_is_rejected_when_this_charge_point_has_no_cache_at_all() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        set_variable(&actor, "AuthCacheCtrlr", "Enabled", "false").await;

        assert_eq!(
            handle_clear_cache(&actor).await,
            ClearCacheOutcome::Rejected
        );
    }
}

/// `AuthCtrlr.DisableRemoteAuthorization` (CV7): the operator told this station to stop asking the
/// CSMS at all and decide from what it already holds.
#[cfg(test)]
mod disable_remote_authorization_tests {
    use super::{Authorizer, ContractAuthorization, run_authorization_requests};
    use crate::actor::ChargePointActor;
    use crate::executor::TokioExecutor;
    use crate::state::{
        AuthorizationRequested, AuthorizationStatus, ChargePointEvent, ConnectorEvent,
        ConnectorState, ContractCertificate, DeviceModelEvent, EvseEvent, IdToken, IdTokenKind,
        LocalListEntry, VariableAttributeType,
    };
    use crate::sync::broadcast_channel;
    use alloc::boxed::Box;
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicBool, Ordering};

    fn token(value: &str) -> IdToken {
        IdToken {
            value: value.into(),
            kind: IdTokenKind::ISO14443,
        }
    }

    /// Accepts everything, and records whether it was asked at all - which is the whole point of
    /// the variable under test: not what the CSMS would have said, but that it was never consulted.
    struct RecordingAuthorizer {
        asked: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl Authorizer for RecordingAuthorizer {
        type Error = core::convert::Infallible;

        async fn authorize(&self, _id_token: &IdToken) -> Result<AuthorizationStatus, Self::Error> {
            self.asked.store(true, Ordering::SeqCst);
            Ok(AuthorizationStatus::Accepted)
        }

        async fn authorize_contract(
            &self,
            _id_token: &IdToken,
            _contract: &ContractCertificate,
        ) -> Result<ContractAuthorization, Self::Error> {
            self.asked.store(true, Ordering::SeqCst);
            Ok(ContractAuthorization {
                status: AuthorizationStatus::Accepted,
                certificate_status: None,
            })
        }
    }

    async fn set_variable(actor: &ChargePointActor, component: &str, variable: &str, value: &str) {
        let _ = actor
            .send(ChargePointEvent::DeviceModel(
                DeviceModelEvent::AttributeValueSet {
                    component: crate::state::Component {
                        name: component.into(),
                        instance: None,
                        evse: None,
                    },
                    variable: crate::state::Variable {
                        name: variable.into(),
                        instance: None,
                    },
                    attribute_type: VariableAttributeType::Actual,
                    value: value.into(),
                },
            ))
            .await;
    }

    /// A connector in `Authorizing` with `DisableRemoteAuthorization` already set, so the only
    /// thing left to vary per test is what the station locally knows about the token.
    async fn locally_deciding_actor() -> ChargePointActor {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        set_variable(&actor, "AuthCtrlr", "DisableRemoteAuthorization", "true").await;
        for event in [
            ConnectorEvent::CableConnected,
            ConnectorEvent::LockConfirmed,
            ConnectorEvent::IdTokenPresented(token("A")),
        ] {
            actor
                .send(ChargePointEvent::Evse {
                    evse_id: 0,
                    event: EvseEvent::Connector {
                        connector_id: 0,
                        event,
                    },
                })
                .await
                .unwrap();
        }
        actor
    }

    /// Runs one authorization request to completion and reports whether the CSMS was asked.
    async fn decide(actor: &ChargePointActor, contract: Option<ContractCertificate>) -> bool {
        let asked = Arc::new(AtomicBool::new(false));
        let authorizer = RecordingAuthorizer {
            asked: Arc::clone(&asked),
        };
        let sender = broadcast_channel();
        let receiver = sender.subscribe();
        sender.send(AuthorizationRequested {
            evse_id: 0,
            connector_id: 0,
            id_token: token("A"),
            contract,
        });
        drop(sender);
        run_authorization_requests(
            receiver,
            &authorizer,
            actor.clone(),
            &crate::clock::SystemClock,
        )
        .await;
        asked.load(Ordering::SeqCst)
    }

    #[tokio::test]
    async fn the_local_authorization_list_answers_without_asking_the_csms() {
        let actor = locally_deciding_actor().await;
        let _ = actor
            .send(ChargePointEvent::LocalListUpdated {
                version: 1,
                entries: alloc::vec![LocalListEntry {
                    id_token: token("A"),
                    status: AuthorizationStatus::Accepted,
                }],
            })
            .await;

        assert!(
            !decide(&actor, None).await,
            "DisableRemoteAuthorization means no AuthorizeRequest is issued at all"
        );
        assert_eq!(
            actor.state().evses[0].connectors[0],
            ConnectorState::Starting
        );
    }

    #[tokio::test]
    async fn the_authorization_cache_answers_too() {
        let actor = locally_deciding_actor().await;
        let _ = actor
            .send(ChargePointEvent::AuthorizationCached {
                id_token: token("A"),
                status: AuthorizationStatus::Accepted,
                cached_at: None,
            })
            .await;

        assert!(!decide(&actor, None).await);
        assert_eq!(
            actor.state().evses[0].connectors[0],
            ConnectorState::Starting
        );
    }

    #[tokio::test]
    async fn an_identifier_neither_source_knows_is_refused() {
        // "We were told not to ask, and we don't know" is not "yes" - the same rule the offline
        // path follows, for the same reason.
        let actor = locally_deciding_actor().await;

        assert!(!decide(&actor, None).await);
        assert_eq!(actor.state().evses[0].connectors[0], ConnectorState::Locked);
    }

    /// `LocalAuthorizeOffline` gates what this station does *when it cannot reach the CSMS*. Here
    /// it can reach the CSMS perfectly well and has been told not to bother, so that variable has
    /// nothing to say - reusing it would let an unrelated setting silently disable the switch.
    #[tokio::test]
    async fn local_authorize_offline_does_not_gate_it() {
        let actor = locally_deciding_actor().await;
        set_variable(&actor, "AuthCtrlr", "LocalAuthorizeOffline", "false").await;
        let _ = actor
            .send(ChargePointEvent::LocalListUpdated {
                version: 1,
                entries: alloc::vec![LocalListEntry {
                    id_token: token("A"),
                    status: AuthorizationStatus::Accepted,
                }],
            })
            .await;

        assert!(!decide(&actor, None).await);
        assert_eq!(
            actor.state().evses[0].connectors[0],
            ConnectorState::Starting
        );
    }

    /// Disabling the cache still disables the cache; the list keeps answering.
    #[tokio::test]
    async fn disabling_the_cache_stops_it_answering_here_as_well() {
        let actor = locally_deciding_actor().await;
        set_variable(&actor, "AuthCacheCtrlr", "Enabled", "false").await;
        let _ = actor
            .send(ChargePointEvent::AuthorizationCached {
                id_token: token("A"),
                status: AuthorizationStatus::Accepted,
                cached_at: None,
            })
            .await;

        assert!(!decide(&actor, None).await);
        assert_eq!(actor.state().evses[0].connectors[0], ConnectorState::Locked);
    }

    /// A Plug & Charge presentation is refused rather than answered locally: C07 puts contract
    /// validation at the CSMS, and this crate parses no X.509 itself - so a station forbidden to
    /// ask has no way to establish the contract is still valid. Same reasoning as the offline
    /// contract path.
    #[tokio::test]
    async fn a_contract_presentation_is_refused_rather_than_decided_locally() {
        let actor = locally_deciding_actor().await;
        let _ = actor
            .send(ChargePointEvent::LocalListUpdated {
                version: 1,
                entries: alloc::vec![LocalListEntry {
                    id_token: token("A"),
                    status: AuthorizationStatus::Accepted,
                }],
            })
            .await;

        let contract = ContractCertificate {
            chain_pem: None,
            ocsp_data: alloc::vec![],
        };
        assert!(!decide(&actor, Some(contract)).await);
        assert_eq!(actor.state().evses[0].connectors[0], ConnectorState::Locked);
    }

    /// The switch is off by default, so an unconfigured station still asks - without this the
    /// tests above would pass just as well against a station that never asked anybody.
    #[tokio::test]
    async fn an_unconfigured_station_still_asks_the_csms() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        for event in [
            ConnectorEvent::CableConnected,
            ConnectorEvent::LockConfirmed,
            ConnectorEvent::IdTokenPresented(token("A")),
        ] {
            actor
                .send(ChargePointEvent::Evse {
                    evse_id: 0,
                    event: EvseEvent::Connector {
                        connector_id: 0,
                        event,
                    },
                })
                .await
                .unwrap();
        }

        assert!(decide(&actor, None).await);
        assert_eq!(
            actor.state().evses[0].connectors[0],
            ConnectorState::Starting
        );
    }
}

/// OCPP 2.1's `ClearCache`.
#[cfg(feature = "ocpp_2_1")]
mod clear_cache_ocpp_2_1 {
    use super::{ClearCacheHandler, ClearCacheOutcome, handle_clear_cache};
    use crate::actor::ChargePointActor;
    use crate::wire::v21::common::ClearCacheStatusEnum;
    use crate::wire::v21::{ClearCacheRequest, ClearCacheResponse};
    use alloc::boxed::Box;
    use ocpp_client::ocpp_2_1::OCPP2_1Client;

    #[async_trait::async_trait]
    impl ClearCacheHandler for OCPP2_1Client {
        async fn register_clear_cache_handler(&self, actor: ChargePointActor) {
            self.on_clear_cache(move |_request: ClearCacheRequest, _client| {
                let actor = actor.clone();
                async move {
                    Ok(ClearCacheResponse {
                        custom_data: None,
                        status: match handle_clear_cache(&actor).await {
                            ClearCacheOutcome::Accepted => ClearCacheStatusEnum::Accepted,
                            ClearCacheOutcome::Rejected => ClearCacheStatusEnum::Rejected,
                        },
                        status_info: None,
                    })
                }
            })
            .await;
        }
    }
}

/// OCPP 2.0.1's `ClearCache` - the same shape as 2.1's.
#[cfg(feature = "ocpp_2_0_1")]
mod clear_cache_ocpp_2_0_1 {
    use super::{ClearCacheHandler, ClearCacheOutcome, handle_clear_cache};
    use crate::actor::ChargePointActor;
    use crate::wire::v201::common::ClearCacheStatusEnum;
    use crate::wire::v201::{ClearCacheRequest, ClearCacheResponse};
    use alloc::boxed::Box;
    use ocpp_client::ocpp_2_0_1::OCPP2_0_1Client;

    #[async_trait::async_trait]
    impl ClearCacheHandler for OCPP2_0_1Client {
        async fn register_clear_cache_handler(&self, actor: ChargePointActor) {
            self.on_clear_cache(move |_request: ClearCacheRequest, _client| {
                let actor = actor.clone();
                async move {
                    Ok(ClearCacheResponse {
                        custom_data: None,
                        status: match handle_clear_cache(&actor).await {
                            ClearCacheOutcome::Accepted => ClearCacheStatusEnum::Accepted,
                            ClearCacheOutcome::Rejected => ClearCacheStatusEnum::Rejected,
                        },
                        status_info: None,
                    })
                }
            })
            .await;
        }
    }
}

/// OCPP 1.6J's `ClearCache` - the one message in this block with no fields at all in either
/// direction beyond its status.
#[cfg(feature = "ocpp_1_6")]
mod clear_cache_ocpp_1_6 {
    use super::{ClearCacheHandler, ClearCacheOutcome, handle_clear_cache};
    use crate::actor::ChargePointActor;
    use crate::wire::v16::common::ClearCacheResponseStatus;
    use crate::wire::v16::{ClearCacheRequest, ClearCacheResponse};
    use alloc::boxed::Box;
    use ocpp_client::ocpp_1_6::OCPP1_6Client;

    #[async_trait::async_trait]
    impl ClearCacheHandler for OCPP1_6Client {
        async fn register_clear_cache_handler(&self, actor: ChargePointActor) {
            self.on_clear_cache(move |_request: ClearCacheRequest, _client| {
                let actor = actor.clone();
                async move {
                    Ok(ClearCacheResponse {
                        status: match handle_clear_cache(&actor).await {
                            ClearCacheOutcome::Accepted => ClearCacheResponseStatus::Accepted,
                            ClearCacheOutcome::Rejected => ClearCacheResponseStatus::Rejected,
                        },
                    })
                }
            })
            .await;
        }
    }
}
