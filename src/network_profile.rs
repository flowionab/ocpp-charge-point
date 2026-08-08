//! `SetNetworkProfile`: the CSMS writing a network connection profile into a configuration slot
//! (`docs/ROADMAP.md` §2, `docs/PRODUCTION-ROADMAP.md` B1.8).
//!
//! **This block stores profiles; it does not itself switch connections.** OCPP is explicit that a
//! profile applies to a *future* connection attempt, and which slot to try is the CSMS's own
//! `OCPPCommCtrlr`/`NetworkConfigurationPriority` ordering - so storing and switching are separate
//! steps, and this is the first one. [`selected_profile`] says which slot that ordering picks, and
//! [`crate::network_switch`] (A9) is what moves the live connection there and rolls back if the
//! new address does not work.
//!
//! A charge point whose client this crate did not dial has no transport to re-point and stays
//! where its integrator connected it; only [`crate::connect::connect_and_setup`] installs the
//! redial target a switch needs.
//!
//! 2.x only - 1.6J has no `SetNetworkProfile` (see [`crate::state::network_profile`]).

use alloc::boxed::Box;

use crate::actor::ChargePointActor;
use crate::state::{ChargePointEvent, NetworkConnectionProfile, NetworkTransport};

/// The outcome of a CSMS-initiated `SetNetworkProfile`, matching OCPP's
/// `SetNetworkProfileStatusEnum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetNetworkProfileOutcome {
    /// The profile was stored in the requested slot.
    Accepted,
    /// The request was refused: a negative slot, a transport this charge point cannot speak, or a
    /// new slot beyond
    /// [`max_network_profile_slots`](crate::state::StateLimits::max_network_profile_slots).
    Rejected,
    /// The charge point tried to store it and could not. Never returned today - this crate's slot
    /// store is in memory and cannot fail once the request itself is valid - but kept because
    /// OCPP distinguishes "I won't" from "I tried and couldn't", and a durable-write failure is
    /// exactly the second case if storing ever grows a fallible step.
    Failed,
}

/// Handles a CSMS-initiated `SetNetworkProfile` against `actor`.
///
/// Refuses, rather than storing, a profile this charge point could never use:
///
/// - a **negative slot**, which addresses nothing;
/// - **SOAP transport**, which OCPP 2.x does not support and this crate cannot speak;
/// - a **new slot beyond the configured bound** (replacing an occupied slot is always allowed -
///   see [`crate::state::NetworkProfileStore::set`]).
///
/// A profile naming a security profile this crate does not implement is *stored*, not refused:
/// the charge point is not being asked to connect right now, and the CSMS is entitled to stage a
/// profile for a firmware that will support it. What this crate will not do is pretend to have
/// applied it - see the module docs.
pub async fn handle_set_network_profile(
    actor: &ChargePointActor,
    slot: i32,
    profile: NetworkConnectionProfile,
) -> SetNetworkProfileOutcome {
    if slot < 0 {
        return SetNetworkProfileOutcome::Rejected;
    }
    if profile.transport == NetworkTransport::Soap {
        tracing::warn!("refusing a SOAP network profile - OCPP 2.x is JSON over WebSocket");
        return SetNetworkProfileOutcome::Rejected;
    }

    let state = actor.state();
    let would_be_new = state.network_profiles.get(slot).is_none();
    if would_be_new && state.network_profiles.len() >= state.network_profiles.max_slots() {
        tracing::warn!(
            slot,
            max_slots = state.network_profiles.max_slots(),
            "refusing a network profile: every configuration slot is occupied"
        );
        return SetNetworkProfileOutcome::Rejected;
    }

    // F4.2/F3.3: a network profile carries the security profile and the endpoint this charge
    // point will authenticate to, so writing one *is* a reconfiguration of security parameters -
    // OCPP's own words for this event. Raised only for a profile that was actually accepted: a
    // refused write changed nothing, and reporting it would tell the CSMS its rejected profile
    // took effect.
    let security_profile = profile.security_profile;
    let _ = actor
        .send(ChargePointEvent::NetworkProfileSet {
            slot,
            profile: Box::new(profile),
        })
        .await;
    crate::security::report_security_event(
        actor,
        crate::state::SecurityEvent {
            event_type: crate::state::SecurityEventType::ReconfigurationOfSecurityParameters,
            tech_info: Some(alloc::format!(
                "network connection profile written to slot {slot} with security profile \
                 {security_profile}"
            )),
        },
    )
    .await;
    SetNetworkProfileOutcome::Accepted
}

/// The profile this charge point should be connected through, per
/// `OCPPCommCtrlr`/`NetworkConfigurationPriority` - the highest-priority slot that actually holds
/// one.
///
/// [`crate::connect::connect_and_setup`] acts on this: when the selection changes, it moves the live
/// connection there and rolls back if the new address does not work (see
/// [`crate::network_switch`]). An integrator who built their own client instead has no transport
/// this crate can re-point, and reads this to drive their own redial - rather than reimplementing
/// the priority parse against [`crate::state::NetworkProfileStore`].
///
/// `None` when no slot in the priority order is occupied, including when no profile has ever been
/// set: a charge point then stays on whatever address it was started with.
pub fn selected_profile(
    state: &crate::state::ChargePointState,
) -> Option<(i32, &NetworkConnectionProfile)> {
    let priority = priority_order(state);
    priority.into_iter().find_map(|slot| {
        state
            .network_profiles
            .get(slot)
            .map(|profile| (slot, profile))
    })
}

/// `OCPPCommCtrlr`/`NetworkConfigurationPriority` parsed into slot numbers, in the order the CSMS
/// wants them tried. Unparseable entries are skipped rather than failing the whole list - a
/// CSMS's typo in one slot should not make the charge point forget the rest of its ordering.
fn priority_order(state: &crate::state::ChargePointState) -> alloc::vec::Vec<i32> {
    let component = crate::state::Component {
        name: "OCPPCommCtrlr".into(),
        instance: None,
        evse: None,
    };
    let variable = crate::state::Variable {
        name: "NetworkConfigurationPriority".into(),
        instance: None,
    };
    state
        .device_model
        .get(&component, &variable)
        .and_then(|definition| definition.attribute(crate::state::VariableAttributeType::Actual))
        .map(|attribute| {
            attribute
                .value
                .split(',')
                .filter_map(|slot| slot.trim().parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Registers this charge point's inbound `SetNetworkProfile` handling with the CSMS connection.
/// Implemented for 2.0.1 and 2.1; 1.6J has no such message.
#[async_trait::async_trait]
pub trait SetNetworkProfileHandler {
    /// Registers a `SetNetworkProfile` handler dispatching against `actor`.
    async fn register_set_network_profile_handler(&self, actor: ChargePointActor);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::TokioExecutor;
    use crate::state::{NetworkInterface, StateLimits};

    fn profile(url: &str) -> NetworkConnectionProfile {
        NetworkConnectionProfile {
            csms_url: url.into(),
            interface: NetworkInterface::Any,
            transport: NetworkTransport::Json,
            security_profile: 1,
            message_timeout_secs: 30,
            identity: None,
        }
    }

    #[tokio::test]
    async fn a_profile_is_stored_in_the_slot_the_csms_addressed() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let outcome = handle_set_network_profile(&actor, 1, profile("wss://csms")).await;

        assert_eq!(outcome, SetNetworkProfileOutcome::Accepted);
        assert_eq!(
            actor
                .state()
                .network_profiles
                .get(1)
                .map(|profile| profile.csms_url.clone()),
            Some("wss://csms".into())
        );
    }

    #[tokio::test]
    async fn a_soap_profile_is_refused_rather_than_stored() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let mut soap = profile("wss://csms");
        soap.transport = NetworkTransport::Soap;

        assert_eq!(
            handle_set_network_profile(&actor, 1, soap).await,
            SetNetworkProfileOutcome::Rejected
        );
        assert!(actor.state().network_profiles.is_empty());
    }

    #[tokio::test]
    async fn a_negative_slot_is_refused() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        assert_eq!(
            handle_set_network_profile(&actor, -1, profile("wss://csms")).await,
            SetNetworkProfileOutcome::Rejected
        );
    }

    #[tokio::test]
    async fn a_new_slot_beyond_the_bound_is_refused_but_a_replacement_is_not() {
        let actor = ChargePointActor::spawn_with_limits(
            [1],
            &TokioExecutor,
            StateLimits::default().with_max_network_profile_slots(1),
        );
        handle_set_network_profile(&actor, 1, profile("wss://first")).await;

        assert_eq!(
            handle_set_network_profile(&actor, 2, profile("wss://second")).await,
            SetNetworkProfileOutcome::Rejected
        );
        // Replacing the occupied slot still works: the CSMS is not asking for more storage, and
        // refusing would leave this charge point holding a profile the CSMS thinks it replaced.
        assert_eq!(
            handle_set_network_profile(&actor, 1, profile("wss://replacement")).await,
            SetNetworkProfileOutcome::Accepted
        );
        assert_eq!(
            actor
                .state()
                .network_profiles
                .get(1)
                .map(|profile| profile.csms_url.clone()),
            Some("wss://replacement".into())
        );
    }

    #[tokio::test]
    async fn a_stored_profile_joins_the_priority_order_so_it_can_be_selected_at_all() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        handle_set_network_profile(&actor, 2, profile("wss://second")).await;
        handle_set_network_profile(&actor, 1, profile("wss://first")).await;

        // Appended in the order they arrived, so slot 2 - written first - is tried first. A
        // charge point must not silently reorder what the CSMS asked for.
        let state = actor.state();
        assert_eq!(
            selected_profile(&state).map(|(slot, profile)| (slot, profile.csms_url.clone())),
            Some((2, "wss://second".into()))
        );
    }

    #[tokio::test]
    async fn the_csms_ordering_is_honoured_and_never_rewritten_by_a_later_profile() {
        use crate::state::{ChargePointEvent, DeviceModelEvent, VariableAttributeType};

        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        handle_set_network_profile(&actor, 1, profile("wss://first")).await;
        handle_set_network_profile(&actor, 2, profile("wss://second")).await;

        // The CSMS reorders: try slot 2 first.
        let _ = actor
            .send(ChargePointEvent::DeviceModel(
                DeviceModelEvent::AttributeValueSet {
                    component: crate::state::Component {
                        name: "OCPPCommCtrlr".into(),
                        instance: None,
                        evse: None,
                    },
                    variable: crate::state::Variable {
                        name: "NetworkConfigurationPriority".into(),
                        instance: None,
                    },
                    attribute_type: VariableAttributeType::Actual,
                    value: "2,1".into(),
                },
            ))
            .await;
        assert_eq!(
            selected_profile(&actor.state()).map(|(slot, _)| slot),
            Some(2)
        );

        // A third profile arrives - it must append, not reset the operator's ordering.
        handle_set_network_profile(&actor, 3, profile("wss://third")).await;
        assert_eq!(
            selected_profile(&actor.state()).map(|(slot, _)| slot),
            Some(2)
        );
    }

    #[tokio::test]
    async fn a_charge_point_with_no_profiles_selects_nothing() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        assert!(selected_profile(&actor.state()).is_none());
    }

    #[tokio::test]
    async fn a_profile_for_a_security_profile_this_crate_cannot_run_is_still_stored() {
        // Staging a profile for a future firmware is a legitimate thing for a CSMS to do; what
        // this crate must not do is claim to have applied it.
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let mut tls = profile("wss://secure");
        tls.security_profile = 3;

        assert_eq!(
            handle_set_network_profile(&actor, 1, tls).await,
            SetNetworkProfileOutcome::Accepted
        );
        assert_eq!(
            actor
                .state()
                .network_profiles
                .get(1)
                .map(|profile| profile.security_profile),
            Some(3)
        );
    }
}

/// OCPP 2.1's `SetNetworkProfile`.
#[cfg(feature = "ocpp_2_1")]
mod ocpp_2_1 {
    use super::{SetNetworkProfileHandler, SetNetworkProfileOutcome, handle_set_network_profile};
    use crate::actor::ChargePointActor;
    use crate::state::{NetworkConnectionProfile, NetworkInterface, NetworkTransport};
    use alloc::boxed::Box;
    use alloc::string::ToString;
    use ocpp_client::ocpp_2_1::OCPP2_1Client;
    use ocpp_client::ocpp_types::v21::common::{
        OCPPInterfaceEnum, OCPPTransportEnum, SetNetworkProfileStatusEnum,
    };
    use ocpp_client::ocpp_types::v21::{SetNetworkProfileRequest, SetNetworkProfileResponse};

    fn map_interface(interface: &OCPPInterfaceEnum) -> NetworkInterface {
        match interface {
            OCPPInterfaceEnum::Wired0 => NetworkInterface::Wired(0),
            OCPPInterfaceEnum::Wired1 => NetworkInterface::Wired(1),
            OCPPInterfaceEnum::Wired2 => NetworkInterface::Wired(2),
            OCPPInterfaceEnum::Wired3 => NetworkInterface::Wired(3),
            OCPPInterfaceEnum::Wireless0 => NetworkInterface::Wireless(0),
            OCPPInterfaceEnum::Wireless1 => NetworkInterface::Wireless(1),
            OCPPInterfaceEnum::Wireless2 => NetworkInterface::Wireless(2),
            OCPPInterfaceEnum::Wireless3 => NetworkInterface::Wireless(3),
            _ => NetworkInterface::Any,
        }
    }

    fn map_transport(transport: &OCPPTransportEnum) -> NetworkTransport {
        match transport {
            OCPPTransportEnum::SOAP => NetworkTransport::Soap,
            _ => NetworkTransport::Json,
        }
    }

    /// Maps the wire profile onto this crate's, dropping the fields it deliberately does not
    /// keep: `apn`/`vpn` (an integrator's connectivity stack, not the OCPP application layer) and
    /// `basicAuthPassword` (a credential this crate cannot use; see
    /// [`NetworkConnectionProfile::security_profile`]).
    fn map_profile(
        connection: &ocpp_client::ocpp_types::v21::common::NetworkConnectionProfile,
    ) -> NetworkConnectionProfile {
        NetworkConnectionProfile {
            csms_url: connection.ocpp_csms_url.to_string(),
            interface: map_interface(&connection.ocpp_interface),
            transport: map_transport(&connection.ocpp_transport),
            security_profile: u8::try_from(connection.security_profile).unwrap_or(1),
            message_timeout_secs: connection.message_timeout,
            identity: connection.identity.as_ref().map(ToString::to_string),
        }
    }

    #[async_trait::async_trait]
    impl SetNetworkProfileHandler for OCPP2_1Client {
        async fn register_set_network_profile_handler(&self, actor: ChargePointActor) {
            self.on_set_network_profile(move |request: SetNetworkProfileRequest, _client| {
                let actor = actor.clone();
                async move {
                    let slot = i32::try_from(request.configuration_slot).unwrap_or(-1);
                    let outcome = handle_set_network_profile(
                        &actor,
                        slot,
                        map_profile(&request.connection_data),
                    )
                    .await;
                    Ok(SetNetworkProfileResponse {
                        custom_data: None,
                        status: match outcome {
                            SetNetworkProfileOutcome::Accepted => {
                                SetNetworkProfileStatusEnum::Accepted
                            }
                            SetNetworkProfileOutcome::Rejected => {
                                SetNetworkProfileStatusEnum::Rejected
                            }
                            SetNetworkProfileOutcome::Failed => SetNetworkProfileStatusEnum::Failed,
                        },
                        status_info: None,
                    })
                }
            })
            .await;
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn every_interface_maps_and_an_unknown_one_falls_back_to_any() {
            assert_eq!(
                map_interface(&OCPPInterfaceEnum::Wired0),
                NetworkInterface::Wired(0)
            );
            assert_eq!(
                map_interface(&OCPPInterfaceEnum::Wireless3),
                NetworkInterface::Wireless(3)
            );
            assert_eq!(
                map_interface(&OCPPInterfaceEnum::Any),
                NetworkInterface::Any
            );
        }

        #[test]
        fn soap_is_carried_through_as_soap_so_the_handler_can_refuse_it() {
            // Mapping SOAP to JSON here would let a profile this charge point cannot use slip
            // into a slot as though it were fine.
            assert_eq!(
                map_transport(&OCPPTransportEnum::SOAP),
                NetworkTransport::Soap
            );
            assert_eq!(
                map_transport(&OCPPTransportEnum::JSON),
                NetworkTransport::Json
            );
        }
    }
}

/// OCPP 2.0.1's `SetNetworkProfile`.
#[cfg(feature = "ocpp_2_0_1")]
mod ocpp_2_0_1 {
    use super::{SetNetworkProfileHandler, SetNetworkProfileOutcome, handle_set_network_profile};
    use crate::actor::ChargePointActor;
    use crate::state::{NetworkConnectionProfile, NetworkInterface, NetworkTransport};
    use alloc::boxed::Box;
    use alloc::string::ToString;
    use ocpp_client::ocpp_2_0_1::OCPP2_0_1Client;
    use ocpp_client::ocpp_types::v201::common::{
        OCPPInterfaceEnum, OCPPTransportEnum, SetNetworkProfileStatusEnum,
    };
    use ocpp_client::ocpp_types::v201::{SetNetworkProfileRequest, SetNetworkProfileResponse};

    fn map_interface(interface: &OCPPInterfaceEnum) -> NetworkInterface {
        match interface {
            OCPPInterfaceEnum::Wired0 => NetworkInterface::Wired(0),
            OCPPInterfaceEnum::Wired1 => NetworkInterface::Wired(1),
            OCPPInterfaceEnum::Wired2 => NetworkInterface::Wired(2),
            OCPPInterfaceEnum::Wired3 => NetworkInterface::Wired(3),
            OCPPInterfaceEnum::Wireless0 => NetworkInterface::Wireless(0),
            OCPPInterfaceEnum::Wireless1 => NetworkInterface::Wireless(1),
            OCPPInterfaceEnum::Wireless2 => NetworkInterface::Wireless(2),
            // 2.0.1's enum has exactly these eight values - no `Any`, which 2.1 added - so this
            // match is exhaustive with no catch-all, and a value added upstream would be a
            // compile error here rather than a silent `Any`.
            OCPPInterfaceEnum::Wireless3 => NetworkInterface::Wireless(3),
        }
    }

    fn map_transport(transport: &OCPPTransportEnum) -> NetworkTransport {
        match transport {
            OCPPTransportEnum::SOAP => NetworkTransport::Soap,
            _ => NetworkTransport::Json,
        }
    }

    /// Maps the wire profile onto this crate's, dropping the fields it deliberately does not
    /// keep: `apn`/`vpn` (an integrator's connectivity stack, not the OCPP application layer) and
    /// `basicAuthPassword` (a credential this crate cannot use; see
    /// [`NetworkConnectionProfile::security_profile`]).
    fn map_profile(
        connection: &ocpp_client::ocpp_types::v201::common::NetworkConnectionProfile,
    ) -> NetworkConnectionProfile {
        NetworkConnectionProfile {
            csms_url: connection.ocpp_csms_url.to_string(),
            interface: map_interface(&connection.ocpp_interface),
            transport: map_transport(&connection.ocpp_transport),
            security_profile: u8::try_from(connection.security_profile).unwrap_or(1),
            message_timeout_secs: connection.message_timeout,
            // 2.0.1's `NetworkConnectionProfile` has no `identity` field at all - 2.1 added it -
            // so there is nothing to carry here rather than something being dropped.
            identity: None,
        }
    }

    #[async_trait::async_trait]
    impl SetNetworkProfileHandler for OCPP2_0_1Client {
        async fn register_set_network_profile_handler(&self, actor: ChargePointActor) {
            self.on_set_network_profile(move |request: SetNetworkProfileRequest, _client| {
                let actor = actor.clone();
                async move {
                    let slot = i32::try_from(request.configuration_slot).unwrap_or(-1);
                    let outcome = handle_set_network_profile(
                        &actor,
                        slot,
                        map_profile(&request.connection_data),
                    )
                    .await;
                    Ok(SetNetworkProfileResponse {
                        custom_data: None,
                        status: match outcome {
                            SetNetworkProfileOutcome::Accepted => {
                                SetNetworkProfileStatusEnum::Accepted
                            }
                            SetNetworkProfileOutcome::Rejected => {
                                SetNetworkProfileStatusEnum::Rejected
                            }
                            SetNetworkProfileOutcome::Failed => SetNetworkProfileStatusEnum::Failed,
                        },
                        status_info: None,
                    })
                }
            })
            .await;
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn every_interface_maps_and_an_unknown_one_falls_back_to_any() {
            assert_eq!(
                map_interface(&OCPPInterfaceEnum::Wired0),
                NetworkInterface::Wired(0)
            );
            assert_eq!(
                map_interface(&OCPPInterfaceEnum::Wireless3),
                NetworkInterface::Wireless(3)
            );
            // 2.0.1's enum has no `Any` - 2.1 added it - so the fallback arm is what covers a
            // value this crate has no interface for.
            assert_eq!(
                map_interface(&OCPPInterfaceEnum::Wired1),
                NetworkInterface::Wired(1)
            );
        }

        #[test]
        fn soap_is_carried_through_as_soap_so_the_handler_can_refuse_it() {
            // Mapping SOAP to JSON here would let a profile this charge point cannot use slip
            // into a slot as though it were fine.
            assert_eq!(
                map_transport(&OCPPTransportEnum::SOAP),
                NetworkTransport::Soap
            );
            assert_eq!(
                map_transport(&OCPPTransportEnum::JSON),
                NetworkTransport::Json
            );
        }
    }
}
