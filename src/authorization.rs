//! Authorization functional block: deciding whether a presented identifier may start
//! charging, via Authorize. See `docs/ROADMAP.md` §3.

use crate::actor::ChargePointActor;
use crate::state::{
    AuthorizationRequested, AuthorizationStatus, ChargePointEvent, ConnectorEvent, EvseEvent,
    IdToken,
};
use crate::sync::BroadcastReceiver;
use alloc::boxed::Box;

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

/// Answers every authorization request received on `requests` by calling `authorizer`, and
/// feeds the decision back into the actor as `ChargingAuthorized`/`AuthorizationDenied`,
/// forever. A transport-level failure is treated as denied - erratic connectivity must not
/// leave a connector waiting indefinitely (see CLAUDE.md's error-handling guidance).
pub async fn run_authorization_requests<A: Authorizer>(
    mut requests: BroadcastReceiver<AuthorizationRequested>,
    authorizer: &A,
    actor: ChargePointActor,
) {
    while let Ok(requested) = requests.recv().await {
        let decision = match authorizer.authorize(&requested.id_token).await {
            Ok(status) => status,
            Err(err) => {
                tracing::warn!(error = %err, "authorization request failed, denying");
                AuthorizationStatus::Rejected
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

        run_authorization_requests(receiver, &authorizer, actor.clone()).await;

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

        run_authorization_requests(receiver, &authorizer, actor.clone()).await;

        assert_eq!(actor.state().evses[0].connectors[0], ConnectorState::Locked);
    }
}

#[cfg(feature = "ocpp_2_1")]
mod ocpp_2_1 {
    use super::Authorizer;
    use crate::state::{AuthorizationStatus, IdToken, IdTokenKind};
    use alloc::boxed::Box;
    use ocpp_client::ClientError;
    use ocpp_client::ocpp_2_1::{OCPP2_1Client, OCPP2_1Error};
    use ocpp_client::ocpp_types::v21::AuthorizeRequest;
    use ocpp_client::ocpp_types::v21::common::{AuthorizationStatusEnum, IdToken as WireIdToken};

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
    use alloc::boxed::Box;
    use ocpp_client::ClientError;
    use ocpp_client::ocpp_2_0_1::{OCPP2_0_1Client, OCPP2_0_1Error};
    use ocpp_client::ocpp_types::v201::AuthorizeRequest;
    use ocpp_client::ocpp_types::v201::common::{
        AuthorizationStatusEnum, IdToken as WireIdToken, IdTokenEnum,
    };

    /// Unlike 2.1's free-form `type` string (see [`super::ocpp_2_1::wire_type`]), 2.0.1's
    /// `IdTokenEnumType` is a closed 8-value enum with no catch-all - it has no
    /// `DirectPayment`/`EVCCID`/`Vin` equivalent (those were added, as free-form values, only
    /// once 2.1 dropped the enum). Each falls back to `Central` - "assigned/known centrally" is
    /// the closest existing meaning to "an identifier this crate can't name more precisely under
    /// 2.0.1" - rather than failing the whole request over a field the CSMS mostly uses for
    /// logging/UX, not authorization logic itself.
    pub(super) fn map_id_token_kind(kind: IdTokenKind) -> IdTokenEnum {
        match kind {
            IdTokenKind::Central | IdTokenKind::DirectPayment | IdTokenKind::EVCCID | IdTokenKind::Vin => {
                IdTokenEnum::Central
            }
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
            assert_eq!(map_id_token_kind(IdTokenKind::Central), IdTokenEnum::Central);
            assert_eq!(map_id_token_kind(IdTokenKind::EMAID), IdTokenEnum::EMAID);
            assert_eq!(
                map_id_token_kind(IdTokenKind::ISO14443),
                IdTokenEnum::ISO14443
            );
            assert_eq!(
                map_id_token_kind(IdTokenKind::ISO15693),
                IdTokenEnum::ISO15693
            );
            assert_eq!(map_id_token_kind(IdTokenKind::KeyCode), IdTokenEnum::KeyCode);
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
    use alloc::boxed::Box;
    use ocpp_client::ClientError;
    use ocpp_client::ocpp_1_6::{OCPP1_6Client, OCPP1_6Error};
    use ocpp_client::ocpp_types::v16::AuthorizeRequest;
    use ocpp_client::ocpp_types::v16::common::IdTagInfoStatus;

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
