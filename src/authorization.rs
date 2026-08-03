//! Authorization functional block: deciding whether a presented identifier may start
//! charging, via Authorize. See `docs/ROADMAP.md` §3.

use crate::actor::ChargePointActor;
use crate::state::{
    AuthorizationRequested, AuthorizationStatus, ChargePointEvent, ConnectorEvent, EvseEvent,
    IdToken,
};
use alloc::boxed::Box;
use tokio::sync::broadcast;

/// Decides whether an [`IdToken`] may start charging, via Authorize. Implemented per protocol
/// version (see the `ocpp_2_1` module), mirroring
/// [`crate::availability::StatusNotifier`].
#[async_trait::async_trait]
pub trait Authorizer {
    type Error: core::error::Error + Send + Sync + 'static;

    async fn authorize(&self, id_token: &IdToken) -> Result<AuthorizationStatus, Self::Error>;
}

/// Answers every authorization request received on `requests` by calling `authorizer`, and
/// feeds the decision back into the actor as `ChargingAuthorized`/`AuthorizationDenied`,
/// forever. A transport-level failure is treated as denied - erratic connectivity must not
/// leave a connector waiting indefinitely (see CLAUDE.md's error-handling guidance).
pub async fn run_authorization_requests<A: Authorizer>(
    mut requests: broadcast::Receiver<AuthorizationRequested>,
    authorizer: &A,
    actor: ChargePointActor,
) {
    loop {
        match requests.recv().await {
            Ok(requested) => {
                let decision = match authorizer.authorize(&requested.id_token).await {
                    Ok(status) => status,
                    Err(err) => {
                        tracing::warn!(error = %err, "authorization request failed, denying");
                        AuthorizationStatus::Rejected
                    }
                };
                let event = match decision {
                    AuthorizationStatus::Accepted => ConnectorEvent::ChargingAuthorized,
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
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(
                    skipped,
                    "authorization request receiver lagged, some requests were dropped"
                );
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Authorizer, run_authorization_requests};
    use crate::actor::ChargePointActor;
    use crate::state::{
        AuthorizationRequested, AuthorizationStatus, ChargePointEvent, ConnectorEvent,
        ConnectorState, EvseEvent, IdToken, IdTokenKind,
    };
    use alloc::boxed::Box;
    use tokio::sync::broadcast;

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
        let actor = ChargePointActor::spawn([1]);
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
        let (sender, receiver) = broadcast::channel(4);
        let authorizer = FixedAuthorizer(AuthorizationStatus::Accepted);

        sender
            .send(AuthorizationRequested {
                evse_id: 0,
                connector_id: 0,
                id_token: test_id_token(),
            })
            .unwrap();
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
        let (sender, receiver) = broadcast::channel(4);
        let authorizer = FixedAuthorizer(AuthorizationStatus::Rejected);

        sender
            .send(AuthorizationRequested {
                evse_id: 0,
                connector_id: 0,
                id_token: test_id_token(),
            })
            .unwrap();
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
    use alloc::string::ToString;
    use ocpp_client::ClientError;
    use ocpp_client::ocpp_2_1::{OCPP2_1Client, OCPP2_1Error};
    use ocpp_client::rust_ocpp::v2_1::datatypes::IdTokenType;
    use ocpp_client::rust_ocpp::v2_1::enumerations::AuthorizationStatusEnumType;
    use ocpp_client::rust_ocpp::v2_1::messages::authorize::AuthorizeRequest;

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
            id_token: IdTokenType {
                additional_info: None,
                id_token: id_token.value.clone(),
                type_: wire_type(id_token.kind).to_string(),
                custom_data: None,
            },
            iso15118_certificate_hash_data: None,
        }
    }

    /// Only `Accepted` maps to our `Accepted` - the wire enum's other 9 values (including
    /// `ConcurrentTx`, which per spec shouldn't necessarily stop an already-running
    /// transaction) all collapse to `Rejected` for now. See `docs/ROADMAP.md` §3.
    pub(super) fn map_status(status: AuthorizationStatusEnumType) -> AuthorizationStatus {
        match status {
            AuthorizationStatusEnumType::Accepted => AuthorizationStatus::Accepted,
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
            assert_eq!(request.id_token.type_, "ISO14443");
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
                map_status(AuthorizationStatusEnumType::Accepted),
                AuthorizationStatus::Accepted
            );
            assert_eq!(
                map_status(AuthorizationStatusEnumType::Blocked),
                AuthorizationStatus::Rejected
            );
            assert_eq!(
                map_status(AuthorizationStatusEnumType::ConcurrentTx),
                AuthorizationStatus::Rejected
            );
            assert_eq!(
                map_status(AuthorizationStatusEnumType::Unknown),
                AuthorizationStatus::Rejected
            );
        }
    }
}
