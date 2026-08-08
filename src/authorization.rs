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

use crate::actor::ChargePointActor;
use crate::clock::{Clock, is_synchronized};
use crate::state::{
    AuthorizationRequested, AuthorizationStatus, ChargePointEvent, ChargePointState, Component,
    ConnectorEvent, EvseEvent, IdToken, Variable, VariableAttributeType,
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
        None => AuthorizationStatus::Rejected,
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
pub async fn run_authorization_requests<A: Authorizer, C: Clock>(
    mut requests: BroadcastReceiver<AuthorizationRequested>,
    authorizer: &A,
    actor: ChargePointActor,
    clock: &C,
) {
    while let Ok(requested) = requests.recv().await {
        let now = clock.now();
        let now = is_synchronized(&now).then_some(now);
        let decision = match authorizer.authorize(&requested.id_token).await {
            Ok(status) => {
                let _ = actor
                    .send(ChargePointEvent::AuthorizationCached {
                        id_token: requested.id_token.clone(),
                        status,
                        cached_at: now,
                    })
                    .await;
                status
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "authorization request failed; falling back to offline authorization"
                );
                offline_decision(&actor.state(), &requested.id_token, now)
            }
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
}

#[cfg(feature = "ocpp_2_1")]
mod ocpp_2_1 {
    use super::Authorizer;
    use crate::state::{AuthorizationStatus, IdToken, IdTokenKind};
    use crate::wire::v21::AuthorizeRequest;
    use crate::wire::v21::common::{AuthorizationStatusEnum, IdToken as WireIdToken};
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

    #[async_trait::async_trait]
    impl Authorizer for OCPP2_1Client {
        type Error = ClientError<OCPP2_1Error>;

        async fn authorize(&self, id_token: &IdToken) -> Result<AuthorizationStatus, Self::Error> {
            let response = self.send_authorize(build_request(id_token)).await?;
            Ok(map_status(response.id_token_info.status))
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
    use super::Authorizer;
    use crate::state::{AuthorizationStatus, IdToken, IdTokenKind};
    use crate::wire::v201::AuthorizeRequest;
    use crate::wire::v201::common::{AuthorizationStatusEnum, IdToken as WireIdToken, IdTokenEnum};
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

    #[async_trait::async_trait]
    impl Authorizer for OCPP2_0_1Client {
        type Error = ClientError<OCPP2_0_1Error>;

        async fn authorize(&self, id_token: &IdToken) -> Result<AuthorizationStatus, Self::Error> {
            let response = self.send_authorize(build_request(id_token)).await?;
            Ok(map_status(response.id_token_info.status))
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
mod ocpp_1_6 {
    use super::Authorizer;
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
    pub(super) fn map_status(status: IdTagInfoStatus) -> AuthorizationStatus {
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
