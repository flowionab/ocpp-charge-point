//! Provisioning's Component/Variable device model functional block: CSMS-initiated
//! `GetVariables`/`SetVariables`. See `docs/ROADMAP.md` §2.
//!
//! OCPP 1.6J has no Component/Variable device model at all - its `ocpp_1_6` submodule instead
//! projects onto 1.6J's flat `GetConfiguration`/`ChangeConfiguration` pair via a documented
//! flat-key naming convention; see that submodule's docs for the convention and its limits.

use crate::actor::ChargePointActor;
use crate::hardware::{CAPABILITY_GATES, Capabilities};
use crate::state::{
    ChargePointEvent, Component, DeviceModel, DeviceModelEvent, Variable, VariableAttribute,
    VariableAttributeType, VariableCharacteristics, VariableDataType, VariableMutability,
};
use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

/// Builds the `DeviceModelEvent::VariableRegistered` events that advertise every
/// [`CAPABILITY_GATES`] entry's `*Ctrlr.Available` variable, reflecting `capabilities` (C3.2/C3.4,
/// `docs/PRODUCTION-ROADMAP.md` §5.3) - registered for every gate regardless of whether the
/// capability is present, so `GetBaseReport`/`GetVariables` can truthfully report `Available:
/// false` rather than the component not existing at all (both 2.1 Part 2 and the 1.6J projection
/// distinguish "not supported" from "unknown component"). Entries with no `ctrlr_component` (see
/// that field's docs) contribute nothing - there's no standardized component to register.
///
/// This is the single place all four C3 propagation surfaces ultimately agree through: the
/// handler-registration skip in [`crate::setup::setup`], the `SupportedFeatureProfiles` value from
/// [`crate::hardware::supported_feature_profiles_1_6`], and this device model both read
/// [`CAPABILITY_GATES`] and `capabilities` directly, so a gate added to the table is picked up by
/// all of them at once.
pub fn capability_gate_events(capabilities: &Capabilities) -> Vec<ChargePointEvent> {
    CAPABILITY_GATES
        .iter()
        .filter_map(|gate| {
            let ctrlr_component = gate.ctrlr_component?;
            let available = (gate.enabled)(capabilities);
            Some(ChargePointEvent::DeviceModel(
                DeviceModelEvent::VariableRegistered {
                    component: Component {
                        name: ctrlr_component.to_string(),
                        instance: None,
                        evse: None,
                    },
                    variable: Variable {
                        name: "Available".to_string(),
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
                    attributes: vec![VariableAttribute {
                        attribute_type: VariableAttributeType::Actual,
                        value: available.to_string(),
                        mutability: VariableMutability::ReadOnly,
                        persistent: false,
                        constant: true,
                        requires_reboot: false,
                    }],
                },
            ))
        })
        .collect()
}

/// One requested attribute in a `GetVariables` request: which component/variable/attribute-type
/// to read (OCPP `GetVariableData`, minus wire-only bookkeeping fields).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GetVariableRequest {
    /// The component to read from.
    pub component: Component,
    /// The variable to read.
    pub variable: Variable,
    /// Which attribute of `variable` to read.
    pub attribute_type: VariableAttributeType,
}

/// The outcome of resolving one [`GetVariableRequest`], matching OCPP's `GetVariableStatusEnum`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GetVariableOutcome {
    /// The attribute exists and is readable; carries its current value.
    Accepted(String),
    /// The attribute exists, but is `WriteOnly` - there's nothing to read back.
    Rejected,
    /// `component` isn't registered in the device model at all.
    UnknownComponent,
    /// `component` is registered, but not with this `variable`.
    UnknownVariable,
    /// `variable` is registered, but doesn't have this `attribute_type`.
    NotSupportedAttributeType,
}

/// Handles a CSMS-initiated `GetVariables` request against `actor`'s current device model,
/// resolving every requested item independently and in order - a batch request never fails
/// outright, each item gets its own [`GetVariableOutcome`], per OCPP. A pure read: unlike every
/// mutating handler in this crate, this needs no actor round-trip.
pub fn handle_get_variables(
    actor: &ChargePointActor,
    requests: Vec<GetVariableRequest>,
) -> Vec<GetVariableOutcome> {
    let state = actor.state();
    requests
        .iter()
        .map(|request| {
            resolve_get(
                &state.device_model,
                &request.component,
                &request.variable,
                request.attribute_type,
            )
        })
        .collect()
}

/// Resolves a single component/variable/attribute-type read against `device_model`: unknown
/// component/variable/attribute-type combinations are reported precisely (in that priority
/// order), a `WriteOnly` attribute is `Rejected` (nothing to read back), otherwise its current
/// value is returned.
fn resolve_get(
    device_model: &DeviceModel,
    component: &Component,
    variable: &Variable,
    attribute_type: VariableAttributeType,
) -> GetVariableOutcome {
    let Some(definition) = device_model.get(component, variable) else {
        return if device_model.has_component(component) {
            GetVariableOutcome::UnknownVariable
        } else {
            GetVariableOutcome::UnknownComponent
        };
    };
    let Some(attribute) = definition.attribute(attribute_type) else {
        return GetVariableOutcome::NotSupportedAttributeType;
    };
    if attribute.mutability == VariableMutability::WriteOnly {
        return GetVariableOutcome::Rejected;
    }
    GetVariableOutcome::Accepted(attribute.value.clone())
}

/// Registers this charge point's inbound `GetVariables` handling with the CSMS connection.
/// Implemented per protocol version (see the `ocpp_2_1` module); OCPP 1.6J projects this onto
/// `GetConfiguration` instead (see the `ocpp_1_6` module).
#[async_trait::async_trait]
pub trait GetVariablesHandler {
    /// Registers a `GetVariables` handler with the CSMS connection that dispatches incoming
    /// requests to [`handle_get_variables`] against `actor`.
    async fn register_get_variables_handler(&self, actor: ChargePointActor);
}

/// One requested attribute write in a `SetVariables` request (OCPP `SetVariableData`, minus
/// wire-only bookkeeping fields).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetVariableRequest {
    /// The component to write to.
    pub component: Component,
    /// The variable to write.
    pub variable: Variable,
    /// Which attribute of `variable` to write.
    pub attribute_type: VariableAttributeType,
    /// The value to assign.
    pub value: String,
}

/// The outcome of resolving one [`SetVariableRequest`], matching OCPP's `SetVariableStatusEnum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetVariableOutcome {
    /// The attribute was written.
    Accepted,
    /// The attribute exists, but is `ReadOnly` or `constant` - it can never be written by
    /// `SetVariables`.
    Rejected,
    /// `component` isn't registered in the device model at all.
    UnknownComponent,
    /// `component` is registered, but not with this `variable`.
    UnknownVariable,
    /// `variable` is registered, but doesn't have this `attribute_type`.
    NotSupportedAttributeType,
    /// The attribute was written, but only takes effect after a `Reset` (see
    /// [`crate::state::VariableAttribute::requires_reboot`]).
    RebootRequired,
}

/// Handles a CSMS-initiated `SetVariables` request against `actor`, resolving every requested
/// item independently and in order - mirroring [`handle_get_variables`]'s batch semantics. Each
/// accepted item is applied to the device model (via
/// [`crate::state::DeviceModelEvent::AttributeValueSet`]) before moving on to the next, so a
/// later item in the same batch already observes an earlier one's effect (e.g. writing the same
/// attribute twice in one request applies both, in order).
pub async fn handle_set_variables(
    actor: &ChargePointActor,
    requests: Vec<SetVariableRequest>,
) -> Vec<SetVariableOutcome> {
    let mut outcomes = Vec::with_capacity(requests.len());
    for request in requests {
        outcomes.push(resolve_and_apply_set(actor, &request).await);
    }
    outcomes
}

/// Resolves a single component/variable/attribute-type write against `actor`'s current device
/// model and, if accepted, applies it. Mirrors [`resolve_get`]'s unknown-component/-variable/
/// -attribute-type priority, additionally rejecting a `ReadOnly` or `constant` attribute (neither
/// of which `SetVariables` may ever write), and reports `RebootRequired` instead of `Accepted`
/// when the attribute is marked as needing one.
async fn resolve_and_apply_set(
    actor: &ChargePointActor,
    request: &SetVariableRequest,
) -> SetVariableOutcome {
    let state = actor.state();
    let Some(definition) = state
        .device_model
        .get(&request.component, &request.variable)
    else {
        return if state.device_model.has_component(&request.component) {
            SetVariableOutcome::UnknownVariable
        } else {
            SetVariableOutcome::UnknownComponent
        };
    };
    let Some(attribute) = definition.attribute(request.attribute_type) else {
        return SetVariableOutcome::NotSupportedAttributeType;
    };
    if attribute.mutability == VariableMutability::ReadOnly || attribute.constant {
        return SetVariableOutcome::Rejected;
    }
    let requires_reboot = attribute.requires_reboot;

    let _ = actor
        .send(ChargePointEvent::DeviceModel(
            DeviceModelEvent::AttributeValueSet {
                component: request.component.clone(),
                variable: request.variable.clone(),
                attribute_type: request.attribute_type,
                value: request.value.clone(),
            },
        ))
        .await;

    if requires_reboot {
        SetVariableOutcome::RebootRequired
    } else {
        SetVariableOutcome::Accepted
    }
}

/// Registers this charge point's inbound `SetVariables` handling with the CSMS connection.
/// Implemented per protocol version (see the `ocpp_2_1` module); OCPP 1.6J projects this onto
/// `ChangeConfiguration` instead (see the `ocpp_1_6` module).
#[async_trait::async_trait]
pub trait SetVariablesHandler {
    /// Registers a `SetVariables` handler with the CSMS connection that dispatches incoming
    /// requests to [`handle_set_variables`] against `actor`.
    async fn register_set_variables_handler(&self, actor: ChargePointActor);
}

#[cfg(test)]
mod tests {
    use super::{
        GetVariableOutcome, GetVariableRequest, SetVariableOutcome, SetVariableRequest,
        handle_get_variables, handle_set_variables,
    };
    use crate::actor::ChargePointActor;
    use crate::executor::TokioExecutor;
    use crate::state::{
        ChargePointEvent, Component, DeviceModelEvent, Variable, VariableAttribute,
        VariableAttributeType, VariableCharacteristics, VariableDataType, VariableMutability,
    };

    fn component(name: &str) -> Component {
        Component {
            name: name.into(),
            instance: None,
            evse: None,
        }
    }

    fn variable(name: &str) -> Variable {
        Variable {
            name: name.into(),
            instance: None,
        }
    }

    async fn register_custom_variable(
        actor: &ChargePointActor,
        component_name: &str,
        variable_name: &str,
        mutability: VariableMutability,
        value: &str,
        requires_reboot: bool,
    ) {
        actor
            .send(ChargePointEvent::DeviceModel(
                DeviceModelEvent::VariableRegistered {
                    component: component(component_name),
                    variable: variable(variable_name),
                    characteristics: VariableCharacteristics {
                        data_type: VariableDataType::String,
                        unit: None,
                        min_limit: None,
                        max_limit: None,
                        values_list: None,
                        supports_monitoring: false,
                    },
                    attributes: alloc::vec![VariableAttribute {
                        attribute_type: VariableAttributeType::Actual,
                        value: value.into(),
                        mutability,
                        persistent: false,
                        constant: false,
                        requires_reboot,
                    }],
                },
            ))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn getting_a_known_readable_variable_returns_its_value() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let outcomes = handle_get_variables(
            &actor,
            alloc::vec![GetVariableRequest {
                component: component("OCPPCommCtrlr"),
                variable: variable("HeartbeatInterval"),
                attribute_type: VariableAttributeType::Actual,
            }],
        );

        assert_eq!(
            outcomes,
            alloc::vec![GetVariableOutcome::Accepted("60".into())]
        );
    }

    #[tokio::test]
    async fn getting_an_unknown_component_is_reported() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let outcomes = handle_get_variables(
            &actor,
            alloc::vec![GetVariableRequest {
                component: component("Nonexistent"),
                variable: variable("X"),
                attribute_type: VariableAttributeType::Actual,
            }],
        );

        assert_eq!(outcomes, alloc::vec![GetVariableOutcome::UnknownComponent]);
    }

    #[tokio::test]
    async fn getting_an_unknown_variable_on_a_known_component_is_reported() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let outcomes = handle_get_variables(
            &actor,
            alloc::vec![GetVariableRequest {
                component: component("OCPPCommCtrlr"),
                variable: variable("Nonexistent"),
                attribute_type: VariableAttributeType::Actual,
            }],
        );

        assert_eq!(outcomes, alloc::vec![GetVariableOutcome::UnknownVariable]);
    }

    #[tokio::test]
    async fn getting_an_unsupported_attribute_type_is_reported() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let outcomes = handle_get_variables(
            &actor,
            alloc::vec![GetVariableRequest {
                component: component("OCPPCommCtrlr"),
                variable: variable("HeartbeatInterval"),
                attribute_type: VariableAttributeType::Target,
            }],
        );

        assert_eq!(
            outcomes,
            alloc::vec![GetVariableOutcome::NotSupportedAttributeType]
        );
    }

    #[tokio::test]
    async fn getting_a_write_only_attribute_is_rejected() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        register_custom_variable(
            &actor,
            "Custom",
            "Secret",
            VariableMutability::WriteOnly,
            "hidden",
            false,
        )
        .await;

        let outcomes = handle_get_variables(
            &actor,
            alloc::vec![GetVariableRequest {
                component: component("Custom"),
                variable: variable("Secret"),
                attribute_type: VariableAttributeType::Actual,
            }],
        );

        assert_eq!(outcomes, alloc::vec![GetVariableOutcome::Rejected]);
    }

    #[tokio::test]
    async fn a_batch_resolves_every_item_independently_and_in_order() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let outcomes = handle_get_variables(
            &actor,
            alloc::vec![
                GetVariableRequest {
                    component: component("OCPPCommCtrlr"),
                    variable: variable("HeartbeatInterval"),
                    attribute_type: VariableAttributeType::Actual,
                },
                GetVariableRequest {
                    component: component("Nonexistent"),
                    variable: variable("X"),
                    attribute_type: VariableAttributeType::Actual,
                },
            ],
        );

        assert_eq!(
            outcomes,
            alloc::vec![
                GetVariableOutcome::Accepted("60".into()),
                GetVariableOutcome::UnknownComponent,
            ]
        );
    }

    #[tokio::test]
    async fn setting_a_read_write_variable_updates_it_and_is_visible_afterwards() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let outcomes = handle_set_variables(
            &actor,
            alloc::vec![SetVariableRequest {
                component: component("OCPPCommCtrlr"),
                variable: variable("HeartbeatInterval"),
                attribute_type: VariableAttributeType::Actual,
                value: "120".into(),
            }],
        )
        .await;

        assert_eq!(outcomes, alloc::vec![SetVariableOutcome::Accepted]);
        let get_outcomes = handle_get_variables(
            &actor,
            alloc::vec![GetVariableRequest {
                component: component("OCPPCommCtrlr"),
                variable: variable("HeartbeatInterval"),
                attribute_type: VariableAttributeType::Actual,
            }],
        );
        assert_eq!(
            get_outcomes,
            alloc::vec![GetVariableOutcome::Accepted("120".into())]
        );
    }

    #[tokio::test]
    async fn setting_a_read_only_variable_is_rejected() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        register_custom_variable(
            &actor,
            "Custom",
            "Fixed",
            VariableMutability::ReadOnly,
            "fixed",
            false,
        )
        .await;

        let outcomes = handle_set_variables(
            &actor,
            alloc::vec![SetVariableRequest {
                component: component("Custom"),
                variable: variable("Fixed"),
                attribute_type: VariableAttributeType::Actual,
                value: "changed".into(),
            }],
        )
        .await;

        assert_eq!(outcomes, alloc::vec![SetVariableOutcome::Rejected]);
    }

    #[tokio::test]
    async fn setting_a_variable_that_requires_a_reboot_reports_it_and_still_applies() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        register_custom_variable(
            &actor,
            "Custom",
            "NeedsReboot",
            VariableMutability::ReadWrite,
            "old",
            true,
        )
        .await;

        let outcomes = handle_set_variables(
            &actor,
            alloc::vec![SetVariableRequest {
                component: component("Custom"),
                variable: variable("NeedsReboot"),
                attribute_type: VariableAttributeType::Actual,
                value: "new".into(),
            }],
        )
        .await;

        assert_eq!(outcomes, alloc::vec![SetVariableOutcome::RebootRequired]);
        let get_outcomes = handle_get_variables(
            &actor,
            alloc::vec![GetVariableRequest {
                component: component("Custom"),
                variable: variable("NeedsReboot"),
                attribute_type: VariableAttributeType::Actual,
            }],
        );
        assert_eq!(
            get_outcomes,
            alloc::vec![GetVariableOutcome::Accepted("new".into())]
        );
    }

    #[tokio::test]
    async fn setting_an_unknown_component_is_reported() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let outcomes = handle_set_variables(
            &actor,
            alloc::vec![SetVariableRequest {
                component: component("Nonexistent"),
                variable: variable("X"),
                attribute_type: VariableAttributeType::Actual,
                value: "1".into(),
            }],
        )
        .await;

        assert_eq!(outcomes, alloc::vec![SetVariableOutcome::UnknownComponent]);
    }
}

#[cfg(feature = "ocpp_2_1")]
mod ocpp_2_1 {
    use super::{
        GetVariableOutcome, GetVariableRequest, GetVariablesHandler, SetVariableOutcome,
        SetVariableRequest, SetVariablesHandler, handle_get_variables, handle_set_variables,
    };
    use crate::actor::ChargePointActor;
    use crate::state::{Component, Variable, VariableAttributeType};
    use alloc::boxed::Box;
    use alloc::string::ToString;
    use alloc::vec::Vec;
    use ocpp_client::ocpp_2_1::OCPP2_1Client;
    use ocpp_client::ocpp_types::v21::common::{
        AttributeEnum, GetVariableData, GetVariableResult, GetVariableStatusEnum, SetVariableData,
        SetVariableResult, SetVariableStatusEnum,
    };
    use ocpp_client::ocpp_types::v21::{GetVariablesResponse, SetVariablesResponse};

    /// The largest byte-boundary-safe prefix of `value` no longer than `max_bytes` - mirrors
    /// `crate::id_tag`'s private helper of the same shape; duplicated here (and in the
    /// `ocpp_2_0_1`/`ocpp_1_6` siblings) since that module only compiles under the `ocpp_1_6`
    /// feature and this one doesn't depend on it.
    fn truncate_to_byte_boundary(value: &str, max_bytes: usize) -> &str {
        if value.len() <= max_bytes {
            return value;
        }
        let mut end = max_bytes;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        &value[..end]
    }

    /// `None` (attribute type omitted) means `Actual`, per the wire field's own doc comment.
    fn map_attribute_type(attribute_type: Option<AttributeEnum>) -> VariableAttributeType {
        match attribute_type {
            Some(AttributeEnum::Target) => VariableAttributeType::Target,
            Some(AttributeEnum::MinSet) => VariableAttributeType::MinSet,
            Some(AttributeEnum::MaxSet) => VariableAttributeType::MaxSet,
            None | Some(AttributeEnum::Actual) => VariableAttributeType::Actual,
        }
    }

    /// Maps a wire `Component` to this crate's internal representation. A negative/unrepresentable
    /// `evse.id`/`connectorId` maps to `usize::MAX`, which - since no real charge point has that
    /// many EVSEs - simply never matches a registered component, resolving to `UnknownComponent`
    /// rather than needing a separate fallible parse path (the same "let it resolve to the
    /// correct-anyway outcome" reasoning behind truncating an over-long string elsewhere in this
    /// crate, rather than dropping the whole message).
    fn map_component(component: &ocpp_client::ocpp_types::v21::common::Component) -> Component {
        let evse = component.evse.as_ref().map(|evse| {
            let evse_id = usize::try_from(evse.id).unwrap_or(usize::MAX);
            let connector_id = evse.connector_id.and_then(|id| usize::try_from(id).ok());
            (evse_id, connector_id)
        });
        Component {
            name: component.name.to_string(),
            instance: component
                .instance
                .as_ref()
                .map(|instance| instance.to_string()),
            evse,
        }
    }

    fn map_variable(variable: &ocpp_client::ocpp_types::v21::common::Variable) -> Variable {
        Variable {
            name: variable.name.to_string(),
            instance: variable
                .instance
                .as_ref()
                .map(|instance| instance.to_string()),
        }
    }

    fn parse_get_variable_data(item: &GetVariableData) -> GetVariableRequest {
        GetVariableRequest {
            component: map_component(&item.component),
            variable: map_variable(&item.variable),
            attribute_type: map_attribute_type(item.attribute_type.clone()),
        }
    }

    pub(super) fn map_get_variable_status(outcome: &GetVariableOutcome) -> GetVariableStatusEnum {
        match outcome {
            GetVariableOutcome::Accepted(_) => GetVariableStatusEnum::Accepted,
            GetVariableOutcome::Rejected => GetVariableStatusEnum::Rejected,
            GetVariableOutcome::UnknownComponent => GetVariableStatusEnum::UnknownComponent,
            GetVariableOutcome::UnknownVariable => GetVariableStatusEnum::UnknownVariable,
            GetVariableOutcome::NotSupportedAttributeType => {
                GetVariableStatusEnum::NotSupportedAttributeType
            }
        }
    }

    fn build_get_variable_result(
        item: &GetVariableData,
        outcome: GetVariableOutcome,
    ) -> GetVariableResult {
        let attribute_status = map_get_variable_status(&outcome);
        let attribute_value = match outcome {
            GetVariableOutcome::Accepted(value) => {
                heapless::String::try_from(truncate_to_byte_boundary(&value, 2500)).ok()
            }
            _ => None,
        };
        GetVariableResult {
            attribute_status,
            attribute_status_info: None,
            attribute_type: item.attribute_type.clone(),
            attribute_value,
            component: item.component.clone(),
            custom_data: None,
            variable: item.variable.clone(),
        }
    }

    pub(super) fn map_set_variable_status(outcome: SetVariableOutcome) -> SetVariableStatusEnum {
        match outcome {
            SetVariableOutcome::Accepted => SetVariableStatusEnum::Accepted,
            SetVariableOutcome::Rejected => SetVariableStatusEnum::Rejected,
            SetVariableOutcome::UnknownComponent => SetVariableStatusEnum::UnknownComponent,
            SetVariableOutcome::UnknownVariable => SetVariableStatusEnum::UnknownVariable,
            SetVariableOutcome::NotSupportedAttributeType => {
                SetVariableStatusEnum::NotSupportedAttributeType
            }
            SetVariableOutcome::RebootRequired => SetVariableStatusEnum::RebootRequired,
        }
    }

    fn parse_set_variable_data(item: &SetVariableData) -> SetVariableRequest {
        SetVariableRequest {
            component: map_component(&item.component),
            variable: map_variable(&item.variable),
            attribute_type: map_attribute_type(item.attribute_type.clone()),
            value: item.attribute_value.to_string(),
        }
    }

    fn build_set_variable_result(
        item: &SetVariableData,
        outcome: SetVariableOutcome,
    ) -> SetVariableResult {
        SetVariableResult {
            attribute_status: map_set_variable_status(outcome),
            attribute_status_info: None,
            attribute_type: item.attribute_type.clone(),
            component: item.component.clone(),
            custom_data: None,
            variable: item.variable.clone(),
        }
    }

    #[async_trait::async_trait]
    impl GetVariablesHandler for OCPP2_1Client {
        async fn register_get_variables_handler(&self, actor: ChargePointActor) {
            self.on_get_variables(move |request, _client| {
                let actor = actor.clone();
                async move {
                    let parsed: Vec<GetVariableRequest> = request
                        .get_variable_data
                        .iter()
                        .map(parse_get_variable_data)
                        .collect();
                    let outcomes = handle_get_variables(&actor, parsed);
                    let get_variable_result = request
                        .get_variable_data
                        .iter()
                        .zip(outcomes)
                        .map(|(item, outcome)| build_get_variable_result(item, outcome))
                        .collect();
                    Ok(GetVariablesResponse {
                        custom_data: None,
                        get_variable_result,
                    })
                }
            })
            .await;
        }
    }

    #[async_trait::async_trait]
    impl SetVariablesHandler for OCPP2_1Client {
        async fn register_set_variables_handler(&self, actor: ChargePointActor) {
            self.on_set_variables(move |request, _client| {
                let actor = actor.clone();
                async move {
                    let parsed: Vec<SetVariableRequest> = request
                        .set_variable_data
                        .iter()
                        .map(parse_set_variable_data)
                        .collect();
                    let outcomes = handle_set_variables(&actor, parsed).await;
                    let set_variable_result = request
                        .set_variable_data
                        .iter()
                        .zip(outcomes)
                        .map(|(item, outcome)| build_set_variable_result(item, outcome))
                        .collect();
                    Ok(SetVariablesResponse {
                        custom_data: None,
                        set_variable_result,
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
        fn every_get_variable_outcome_maps_to_the_matching_wire_status() {
            assert_eq!(
                map_get_variable_status(&GetVariableOutcome::Accepted("x".into())),
                GetVariableStatusEnum::Accepted
            );
            assert_eq!(
                map_get_variable_status(&GetVariableOutcome::Rejected),
                GetVariableStatusEnum::Rejected
            );
            assert_eq!(
                map_get_variable_status(&GetVariableOutcome::UnknownComponent),
                GetVariableStatusEnum::UnknownComponent
            );
            assert_eq!(
                map_get_variable_status(&GetVariableOutcome::UnknownVariable),
                GetVariableStatusEnum::UnknownVariable
            );
            assert_eq!(
                map_get_variable_status(&GetVariableOutcome::NotSupportedAttributeType),
                GetVariableStatusEnum::NotSupportedAttributeType
            );
        }

        #[test]
        fn every_set_variable_outcome_maps_to_the_matching_wire_status() {
            assert_eq!(
                map_set_variable_status(SetVariableOutcome::Accepted),
                SetVariableStatusEnum::Accepted
            );
            assert_eq!(
                map_set_variable_status(SetVariableOutcome::Rejected),
                SetVariableStatusEnum::Rejected
            );
            assert_eq!(
                map_set_variable_status(SetVariableOutcome::UnknownComponent),
                SetVariableStatusEnum::UnknownComponent
            );
            assert_eq!(
                map_set_variable_status(SetVariableOutcome::UnknownVariable),
                SetVariableStatusEnum::UnknownVariable
            );
            assert_eq!(
                map_set_variable_status(SetVariableOutcome::NotSupportedAttributeType),
                SetVariableStatusEnum::NotSupportedAttributeType
            );
            assert_eq!(
                map_set_variable_status(SetVariableOutcome::RebootRequired),
                SetVariableStatusEnum::RebootRequired
            );
        }

        fn wire_component(
            evse: Option<(i64, Option<i64>)>,
        ) -> ocpp_client::ocpp_types::v21::common::Component {
            ocpp_client::ocpp_types::v21::common::Component {
                custom_data: None,
                evse: evse.map(
                    |(id, connector_id)| ocpp_client::ocpp_types::v21::common::EVSE {
                        connector_id,
                        custom_data: None,
                        id,
                    },
                ),
                instance: None,
                name: heapless::String::try_from("OCPPCommCtrlr").unwrap(),
            }
        }

        #[test]
        fn a_charge_point_wide_component_maps_with_no_evse_addressing() {
            let mapped = map_component(&wire_component(None));

            assert_eq!(mapped.name, "OCPPCommCtrlr");
            assert_eq!(mapped.evse, None);
        }

        #[test]
        fn an_evse_scoped_component_maps_its_addressing() {
            let mapped = map_component(&wire_component(Some((1, Some(2)))));

            assert_eq!(mapped.evse, Some((1, Some(2))));
        }

        #[test]
        fn a_negative_evse_id_maps_to_a_sentinel_that_never_matches() {
            let mapped = map_component(&wire_component(Some((-1, None))));

            assert_eq!(mapped.evse, Some((usize::MAX, None)));
        }

        #[test]
        fn an_omitted_attribute_type_defaults_to_actual() {
            assert_eq!(map_attribute_type(None), VariableAttributeType::Actual);
        }

        #[test]
        fn an_over_long_value_is_truncated_rather_than_dropped() {
            let item = GetVariableData {
                attribute_type: None,
                component: wire_component(None),
                custom_data: None,
                variable: ocpp_client::ocpp_types::v21::common::Variable {
                    custom_data: None,
                    instance: None,
                    name: heapless::String::try_from("HeartbeatInterval").unwrap(),
                },
            };
            let long_value = alloc::string::String::from("a").repeat(3000);

            let result = build_get_variable_result(&item, GetVariableOutcome::Accepted(long_value));

            assert_eq!(result.attribute_value.unwrap().len(), 2500);
        }
    }
}

/// The OCPP 2.0.1 projection - identical `GetVariablesRequest`/`GetVariablesResponse`/
/// `SetVariablesRequest`/`SetVariablesResponse`/`GetVariableData`/`GetVariableResult`/
/// `SetVariableData`/`SetVariableResult`/`Component`/`Variable`/`AttributeEnum`/
/// `GetVariableStatusEnum`/`SetVariableStatusEnum` wire shapes to 2.1's (2.1 only adds an extra
/// `maxElements` field to `VariableCharacteristics`, which neither action ever transmits), so
/// this is a close copy of the `ocpp_2_1` module - the only real difference is 2.0.1's
/// `SetVariableData.attributeValue` bound being 1000 bytes instead of 2500, which doesn't affect
/// this crate's own code either way (we only ever read that bounded string, never construct one).
#[cfg(feature = "ocpp_2_0_1")]
mod ocpp_2_0_1 {
    use super::{
        GetVariableOutcome, GetVariableRequest, GetVariablesHandler, SetVariableOutcome,
        SetVariableRequest, SetVariablesHandler, handle_get_variables, handle_set_variables,
    };
    use crate::actor::ChargePointActor;
    use crate::state::{Component, Variable, VariableAttributeType};
    use alloc::boxed::Box;
    use alloc::string::ToString;
    use alloc::vec::Vec;
    use ocpp_client::ocpp_2_0_1::OCPP2_0_1Client;
    use ocpp_client::ocpp_types::v201::common::{
        AttributeEnum, GetVariableData, GetVariableResult, GetVariableStatusEnum, SetVariableData,
        SetVariableResult, SetVariableStatusEnum,
    };
    use ocpp_client::ocpp_types::v201::{GetVariablesResponse, SetVariablesResponse};

    fn truncate_to_byte_boundary(value: &str, max_bytes: usize) -> &str {
        if value.len() <= max_bytes {
            return value;
        }
        let mut end = max_bytes;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        &value[..end]
    }

    fn map_attribute_type(attribute_type: Option<AttributeEnum>) -> VariableAttributeType {
        match attribute_type {
            Some(AttributeEnum::Target) => VariableAttributeType::Target,
            Some(AttributeEnum::MinSet) => VariableAttributeType::MinSet,
            Some(AttributeEnum::MaxSet) => VariableAttributeType::MaxSet,
            None | Some(AttributeEnum::Actual) => VariableAttributeType::Actual,
        }
    }

    /// Mirrors [`super::ocpp_2_1::map_component`].
    fn map_component(component: &ocpp_client::ocpp_types::v201::common::Component) -> Component {
        let evse = component.evse.as_ref().map(|evse| {
            let evse_id = usize::try_from(evse.id).unwrap_or(usize::MAX);
            let connector_id = evse.connector_id.and_then(|id| usize::try_from(id).ok());
            (evse_id, connector_id)
        });
        Component {
            name: component.name.to_string(),
            instance: component
                .instance
                .as_ref()
                .map(|instance| instance.to_string()),
            evse,
        }
    }

    fn map_variable(variable: &ocpp_client::ocpp_types::v201::common::Variable) -> Variable {
        Variable {
            name: variable.name.to_string(),
            instance: variable
                .instance
                .as_ref()
                .map(|instance| instance.to_string()),
        }
    }

    fn parse_get_variable_data(item: &GetVariableData) -> GetVariableRequest {
        GetVariableRequest {
            component: map_component(&item.component),
            variable: map_variable(&item.variable),
            attribute_type: map_attribute_type(item.attribute_type.clone()),
        }
    }

    pub(super) fn map_get_variable_status(outcome: &GetVariableOutcome) -> GetVariableStatusEnum {
        match outcome {
            GetVariableOutcome::Accepted(_) => GetVariableStatusEnum::Accepted,
            GetVariableOutcome::Rejected => GetVariableStatusEnum::Rejected,
            GetVariableOutcome::UnknownComponent => GetVariableStatusEnum::UnknownComponent,
            GetVariableOutcome::UnknownVariable => GetVariableStatusEnum::UnknownVariable,
            GetVariableOutcome::NotSupportedAttributeType => {
                GetVariableStatusEnum::NotSupportedAttributeType
            }
        }
    }

    fn build_get_variable_result(
        item: &GetVariableData,
        outcome: GetVariableOutcome,
    ) -> GetVariableResult {
        let attribute_status = map_get_variable_status(&outcome);
        let attribute_value = match outcome {
            GetVariableOutcome::Accepted(value) => {
                heapless::String::try_from(truncate_to_byte_boundary(&value, 2500)).ok()
            }
            _ => None,
        };
        GetVariableResult {
            attribute_status,
            attribute_status_info: None,
            attribute_type: item.attribute_type.clone(),
            attribute_value,
            component: item.component.clone(),
            custom_data: None,
            variable: item.variable.clone(),
        }
    }

    pub(super) fn map_set_variable_status(outcome: SetVariableOutcome) -> SetVariableStatusEnum {
        match outcome {
            SetVariableOutcome::Accepted => SetVariableStatusEnum::Accepted,
            SetVariableOutcome::Rejected => SetVariableStatusEnum::Rejected,
            SetVariableOutcome::UnknownComponent => SetVariableStatusEnum::UnknownComponent,
            SetVariableOutcome::UnknownVariable => SetVariableStatusEnum::UnknownVariable,
            SetVariableOutcome::NotSupportedAttributeType => {
                SetVariableStatusEnum::NotSupportedAttributeType
            }
            SetVariableOutcome::RebootRequired => SetVariableStatusEnum::RebootRequired,
        }
    }

    fn parse_set_variable_data(item: &SetVariableData) -> SetVariableRequest {
        SetVariableRequest {
            component: map_component(&item.component),
            variable: map_variable(&item.variable),
            attribute_type: map_attribute_type(item.attribute_type.clone()),
            value: item.attribute_value.to_string(),
        }
    }

    fn build_set_variable_result(
        item: &SetVariableData,
        outcome: SetVariableOutcome,
    ) -> SetVariableResult {
        SetVariableResult {
            attribute_status: map_set_variable_status(outcome),
            attribute_status_info: None,
            attribute_type: item.attribute_type.clone(),
            component: item.component.clone(),
            custom_data: None,
            variable: item.variable.clone(),
        }
    }

    #[async_trait::async_trait]
    impl GetVariablesHandler for OCPP2_0_1Client {
        async fn register_get_variables_handler(&self, actor: ChargePointActor) {
            self.on_get_variables(move |request, _client| {
                let actor = actor.clone();
                async move {
                    let parsed: Vec<GetVariableRequest> = request
                        .get_variable_data
                        .iter()
                        .map(parse_get_variable_data)
                        .collect();
                    let outcomes = handle_get_variables(&actor, parsed);
                    let get_variable_result = request
                        .get_variable_data
                        .iter()
                        .zip(outcomes)
                        .map(|(item, outcome)| build_get_variable_result(item, outcome))
                        .collect();
                    Ok(GetVariablesResponse {
                        custom_data: None,
                        get_variable_result,
                    })
                }
            })
            .await;
        }
    }

    #[async_trait::async_trait]
    impl SetVariablesHandler for OCPP2_0_1Client {
        async fn register_set_variables_handler(&self, actor: ChargePointActor) {
            self.on_set_variables(move |request, _client| {
                let actor = actor.clone();
                async move {
                    let parsed: Vec<SetVariableRequest> = request
                        .set_variable_data
                        .iter()
                        .map(parse_set_variable_data)
                        .collect();
                    let outcomes = handle_set_variables(&actor, parsed).await;
                    let set_variable_result = request
                        .set_variable_data
                        .iter()
                        .zip(outcomes)
                        .map(|(item, outcome)| build_set_variable_result(item, outcome))
                        .collect();
                    Ok(SetVariablesResponse {
                        custom_data: None,
                        set_variable_result,
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
        fn every_get_variable_outcome_maps_to_the_matching_wire_status() {
            assert_eq!(
                map_get_variable_status(&GetVariableOutcome::Accepted("x".into())),
                GetVariableStatusEnum::Accepted
            );
            assert_eq!(
                map_get_variable_status(&GetVariableOutcome::NotSupportedAttributeType),
                GetVariableStatusEnum::NotSupportedAttributeType
            );
        }

        #[test]
        fn every_set_variable_outcome_maps_to_the_matching_wire_status() {
            assert_eq!(
                map_set_variable_status(SetVariableOutcome::RebootRequired),
                SetVariableStatusEnum::RebootRequired
            );
            assert_eq!(
                map_set_variable_status(SetVariableOutcome::Rejected),
                SetVariableStatusEnum::Rejected
            );
        }

        #[test]
        fn an_omitted_attribute_type_defaults_to_actual() {
            assert_eq!(map_attribute_type(None), VariableAttributeType::Actual);
        }

        #[test]
        fn a_negative_evse_id_maps_to_a_sentinel_that_never_matches() {
            let component = ocpp_client::ocpp_types::v201::common::Component {
                custom_data: None,
                evse: Some(ocpp_client::ocpp_types::v201::common::EVSE {
                    connector_id: None,
                    custom_data: None,
                    id: -1,
                }),
                instance: None,
                name: heapless::String::try_from("X").unwrap(),
            };

            let mapped = map_component(&component);

            assert_eq!(mapped.evse, Some((usize::MAX, None)));
        }
    }
}

/// The OCPP 1.6J projection of [`GetVariablesHandler`]/[`SetVariablesHandler`]. 1.6J has no
/// Component/Variable device model at all - only a flat `key: String<50>` / `value: String<500>`
/// pair per `GetConfiguration`/`ChangeConfiguration` call, with no separate `Actual`/`Target`/
/// `MinSet`/`MaxSet` attribute concept (a 1.6J key has exactly one value) and no way to address a
/// specific EVSE/connector at all.
///
/// # Flat-key convention
///
/// A `(Component, Variable)` pair encodes to a 1.6J key as
/// `"{component.name}[#{component.instance}].{variable.name}[#{variable.instance}]"` - the `#`
/// suffix only appears when the respective `instance` is `Some`. [`decode_key`] is the exact
/// reverse: split on the first `.` into a component part and a variable part, then split each
/// part on `#` into name/instance. The encoded key is truncated to fit 1.6J's 50-byte key bound
/// if needed (truncating rather than failing outright, the same "sane over dropping the whole
/// message" call `crate::id_tag` makes for id tokens).
///
/// Both directions only ever touch charge-point-wide variables (`component.evse.is_none()`) -
/// **EVSE/connector-scoped components have no representation in 1.6J under this convention at
/// all** (1.6J's flat keys have no addressing mechanism for them), so they're simply never listed
/// by `GetConfiguration` and never resolve from a requested key. Only the `Actual` attribute is
/// exposed - the only one 1.6J's single-valued keys can express.
///
/// # Standard key aliases
///
/// 1.6J's own real standard configuration key names (e.g. `"HeartbeatInterval"`,
/// `"AuthorizeRemoteTxRequests"`) have no component prefix at all, so they don't decode under the
/// flat-key convention above by themselves. `STANDARD_KEY_ALIASES` is a hand-maintained table
/// mapping a subset of those standard key names directly to the `(Component, Variable)` pair that
/// OCPP 2.0.1 Part 2's "Referenced Components and Variables" appendix documents as replacing them
/// (mirrored, in CSV form, at `docs/OCPP-2.0.1/Appendices_CSV_v1.5/dm_components_vars.csv`). Both
/// [`encode_key`] and [`decode_key`] consult this table before falling back to the dotted
/// convention, so `GetConfiguration`/`ChangeConfiguration` recognise a real 1.6J CSMS's own key
/// names for whatever standard key is in the table - and `GetConfiguration` with no `key` filter
/// reports those aliased variables under their standard name rather than the dotted form.
///
/// The table is **deliberately partial** - it only covers standard 1.6J keys this crate's device
/// model can plausibly own today (this crate's built-in defaults - see
/// [`DeviceModel::register_defaults`] - plus whatever a hardware binding registers), not the
/// entirety of OCPP 1.6's Appendix 1 of standard configuration keys. Notably absent:
/// `ConnectorPhaseRotation`, whose single 1.6J key packs a per-connector list
/// (`"0.RST,1.RST,..."`) into one string, where 2.0.1 models `PhaseRotation` as one variable per
/// connector - collapsing that fan-out needs more than a static `key -> (Component, Variable)`
/// entry, so it's left as a real gap rather than modeled incorrectly. A standard key that isn't in
/// the table - or any non-standard/vendor key - still falls back to the dotted convention,
/// degrading to `unknownKey`/`NotSupported` exactly as before rather than breaking; extend the
/// table by adding a `StandardKeyAlias` entry as more of the device model grows in.
///
/// `GetConfiguration` is answered directly against the device model - it needs the `readonly` bit
/// [`crate::device_model::GetVariableOutcome`] doesn't carry, and its "return everything" shape
/// when `key` is omitted has no equivalent in the batch-of-typed-requests `GetVariables` takes -
/// so, unlike the `ocpp_2_1`/`ocpp_2_0_1` adapters, it does not call
/// [`crate::device_model::handle_get_variables`]. `ChangeConfiguration` *does* reuse
/// [`crate::device_model::handle_set_variables`] directly, since its single accept/reject/
/// reboot-required decision maps onto ours exactly (just collapsing every "unknown"-shaped
/// outcome to 1.6J's single `NotSupported`, since 1.6J's `ChangeConfigurationResponseStatus` has
/// no equivalent of `UnknownComponent`/`UnknownVariable`/`NotSupportedAttributeType`).
#[cfg(feature = "ocpp_1_6")]
mod ocpp_1_6 {
    use super::{
        GetVariablesHandler, SetVariableOutcome, SetVariableRequest, SetVariablesHandler,
        handle_set_variables,
    };
    use crate::actor::ChargePointActor;
    use crate::hardware::Capabilities;
    use crate::state::{
        Component, DeviceModel, Variable, VariableAttributeType, VariableDefinition,
        VariableMutability,
    };
    use alloc::boxed::Box;
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use ocpp_client::ocpp_1_6::OCPP1_6Client;
    use ocpp_client::ocpp_types::v16::common::{
        ChangeConfigurationResponseStatus, ConfigurationKeyItem,
    };
    use ocpp_client::ocpp_types::v16::{ChangeConfigurationResponse, GetConfigurationResponse};

    /// The largest byte-boundary-safe prefix of `value` no longer than `max_bytes` - mirrors
    /// `crate::id_tag`'s private helper of the same shape (a small intentional duplicate rather
    /// than a shared dependency between two otherwise-unrelated modules).
    fn truncate_to_byte_boundary(value: &str, max_bytes: usize) -> &str {
        if value.len() <= max_bytes {
            return value;
        }
        let mut end = max_bytes;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        &value[..end]
    }

    /// Splits `part` (one half of a flat key) into its name and, if present, its `#`-separated
    /// instance suffix.
    fn split_instance(part: &str) -> (String, Option<String>) {
        match part.split_once('#') {
            Some((name, instance)) => (name.into(), Some(instance.into())),
            None => (part.into(), None),
        }
    }

    /// One entry in [`STANDARD_KEY_ALIASES`]: a 1.6J standard configuration key name and the
    /// `(Component, Variable)` pair it maps to in the 2.x device model. See the module docs'
    /// "Standard key aliases" section.
    struct StandardKeyAlias {
        /// The 1.6J standard configuration key name, exactly as OCPP 1.6's Appendix 1 spells it.
        key: &'static str,
        /// The device model component name this key maps to. Every entry today is charge-point-
        /// wide (no component instance, no EVSE/connector scoping), matching 1.6J's own lack of
        /// addressing.
        component: &'static str,
        /// The device model variable name this key maps to.
        variable: &'static str,
        /// The variable's instance, if the 2.x variable disambiguates with one (e.g.
        /// `MessageAttempts[TransactionEvent]`).
        instance: Option<&'static str>,
    }

    /// The standard 1.6J configuration keys this crate knows a device model alias for, sourced
    /// from `docs/OCPP-2.0.1/Appendices_CSV_v1.5/dm_components_vars.csv` (row numbers as of that
    /// file's current revision) - see the module docs' "Standard key aliases" section for how
    /// this table is used and why it's deliberately partial.
    const STANDARD_KEY_ALIASES: &[StandardKeyAlias] = &[
        // dm_components_vars.csv:232
        StandardKeyAlias {
            key: "HeartbeatInterval",
            component: "OCPPCommCtrlr",
            variable: "HeartbeatInterval",
            instance: None,
        },
        // dm_components_vars.csv:94 - 1.6's "AuthorizeRemoteTxRequests" was renamed
        // "AuthorizeRemoteStart" in 2.0.1.
        StandardKeyAlias {
            key: "AuthorizeRemoteTxRequests",
            component: "AuthCtrlr",
            variable: "AuthorizeRemoteStart",
            instance: None,
        },
        // dm_components_vars.csv:93 - 1.6's "AuthorizationCacheEnabled" became
        // "AuthCacheCtrlr.Enabled". Live rather than decorative: it is what
        // `crate::authorization` consults before answering from the cache while offline.
        StandardKeyAlias {
            key: "AuthorizationCacheEnabled",
            component: "AuthCacheCtrlr",
            variable: "Enabled",
            instance: None,
        },
        // dm_components_vars.csv:60 - 1.6's "ClockAlignedDataInterval" became
        // "AlignedDataCtrlr.Interval". This one is live rather than decorative: it is what
        // `crate::meter_values::run_aligned_meter_values` reads on every cycle, so a 1.6J CSMS
        // configuring it through `ChangeConfiguration` actually changes when readings arrive.
        StandardKeyAlias {
            key: "ClockAlignedDataInterval",
            component: "AlignedDataCtrlr",
            variable: "Interval",
            instance: None,
        },
        // dm_components_vars.csv:260 - 1.6's "MeterValueSampleInterval" became
        // "SampledDataCtrlr.TxUpdatedInterval".
        StandardKeyAlias {
            key: "MeterValueSampleInterval",
            component: "SampledDataCtrlr",
            variable: "TxUpdatedInterval",
            instance: None,
        },
        // dm_components_vars.csv:96
        StandardKeyAlias {
            key: "LocalAuthorizeOffline",
            component: "AuthCtrlr",
            variable: "LocalAuthorizeOffline",
            instance: None,
        },
        // dm_components_vars.csv:97
        StandardKeyAlias {
            key: "LocalPreAuthorize",
            component: "AuthCtrlr",
            variable: "LocalPreAuthorize",
            instance: None,
        },
        // dm_components_vars.csv:99
        StandardKeyAlias {
            key: "AllowOfflineTxForUnknownId",
            component: "AuthCtrlr",
            variable: "OfflineTxForUnknownIdEnabled",
            instance: None,
        },
        // dm_components_vars.csv:319
        StandardKeyAlias {
            key: "StopTransactionOnInvalidId",
            component: "TxCtrlr",
            variable: "StopTxOnInvalidId",
            instance: None,
        },
        // dm_components_vars.csv:245
        StandardKeyAlias {
            key: "UnlockConnectorOnEVSideDisconnect",
            component: "OCPPCommCtrlr",
            variable: "UnlockOnEVSideDisconnect",
            instance: None,
        },
        // dm_components_vars.csv:235
        StandardKeyAlias {
            key: "TransactionMessageAttempts",
            component: "OCPPCommCtrlr",
            variable: "MessageAttempts",
            instance: Some("TransactionEvent"),
        },
        // dm_components_vars.csv:234
        StandardKeyAlias {
            key: "TransactionMessageRetryInterval",
            component: "OCPPCommCtrlr",
            variable: "MessageAttemptInterval",
            instance: Some("TransactionEvent"),
        },
        // dm_components_vars.csv:241
        StandardKeyAlias {
            key: "ResetRetries",
            component: "OCPPCommCtrlr",
            variable: "ResetRetries",
            instance: None,
        },
        // dm_components_vars.csv:246
        StandardKeyAlias {
            key: "WebSocketPingInterval",
            component: "OCPPCommCtrlr",
            variable: "WebSocketPingInterval",
            instance: None,
        },
        // dm_components_vars.csv:316 - 1.6's "ConnectionTimeOut" became
        // "TxCtrlr.EVConnectionTimeOut".
        StandardKeyAlias {
            key: "ConnectionTimeOut",
            component: "TxCtrlr",
            variable: "EVConnectionTimeOut",
            instance: None,
        },
        // dm_components_vars.csv:318
        StandardKeyAlias {
            key: "StopTransactionOnEVSideDisconnect",
            component: "TxCtrlr",
            variable: "StopTxOnEVSideDisconnect",
            instance: None,
        },
        // dm_components_vars.csv:317
        StandardKeyAlias {
            key: "MaxEnergyOnInvalidId",
            component: "TxCtrlr",
            variable: "MaxEnergyOnInvalidId",
            instance: None,
        },
        // dm_components_vars.csv:261 - 1.6's per-transaction sampled measurand list.
        StandardKeyAlias {
            key: "MeterValuesSampledData",
            component: "SampledDataCtrlr",
            variable: "TxUpdatedMeasurands",
            instance: None,
        },
        // dm_components_vars.csv:258 - 1.6's end-of-transaction sampled measurand list.
        StandardKeyAlias {
            key: "StopTxnSampledData",
            component: "SampledDataCtrlr",
            variable: "TxEndedMeasurands",
            instance: None,
        },
        // dm_components_vars.csv:79 - 1.6's clock-aligned measurand list.
        StandardKeyAlias {
            key: "MeterValuesAlignedData",
            component: "AlignedDataCtrlr",
            variable: "Measurands",
            instance: None,
        },
        // dm_components_vars.csv:84 - 1.6's end-of-transaction clock-aligned measurand list.
        StandardKeyAlias {
            key: "StopTxnAlignedData",
            component: "AlignedDataCtrlr",
            variable: "TxEndedMeasurands",
            instance: None,
        },
        // dm_components_vars.csv:41 - listed against `<generic>` (it applies to any
        // component); this crate registers it charge-point-wide, which is the only scope 1.6J's
        // flat key can address anyway.
        StandardKeyAlias {
            key: "MinimumStatusDuration",
            component: "ChargingStation",
            variable: "MinimumStatusDuration",
            instance: None,
        },
        // dm_components_vars.csv:211
        StandardKeyAlias {
            key: "LocalAuthListEnabled",
            component: "LocalAuthListCtrlr",
            variable: "Enabled",
            instance: None,
        },
    ];

    /// Resolves a bare 1.6J standard configuration key name (e.g. `"HeartbeatInterval"`) to its
    /// device model `(Component, Variable)` pair via [`STANDARD_KEY_ALIASES`], if it's in there.
    fn decode_standard_key(key: &str) -> Option<(Component, Variable)> {
        let alias = STANDARD_KEY_ALIASES.iter().find(|alias| alias.key == key)?;
        Some((
            Component {
                name: alias.component.into(),
                instance: None,
                evse: None,
            },
            Variable {
                name: alias.variable.into(),
                instance: alias.instance.map(Into::into),
            },
        ))
    }

    /// The 1.6J standard key name for `(component, variable)`, if [`STANDARD_KEY_ALIASES`] has a
    /// matching entry. Only ever matches an un-instanced `component` - every alias in the table
    /// today is charge-point-wide, matching 1.6J's own lack of component-instance addressing.
    fn encode_standard_key(component: &Component, variable: &Variable) -> Option<&'static str> {
        STANDARD_KEY_ALIASES
            .iter()
            .find(|alias| {
                component.instance.is_none()
                    && component.name == alias.component
                    && variable.name == alias.variable
                    && variable.instance.as_deref() == alias.instance
            })
            .map(|alias| alias.key)
    }

    /// Encodes `(component, variable)` into a 1.6J key: [`STANDARD_KEY_ALIASES`]'s standard key
    /// name if it has one (see the module docs), otherwise this module's own dotted flat-key
    /// convention, truncated to fit 1.6J's 50-byte key bound. `None` for an EVSE/connector-scoped
    /// `component` - not representable under 1.6J at all.
    fn encode_key(component: &Component, variable: &Variable) -> Option<heapless::String<50>> {
        if component.evse.is_some() {
            return None;
        }
        if let Some(standard) = encode_standard_key(component, variable) {
            return heapless::String::try_from(standard).ok();
        }
        let mut key = String::new();
        key.push_str(&component.name);
        if let Some(instance) = &component.instance {
            key.push('#');
            key.push_str(instance);
        }
        key.push('.');
        key.push_str(&variable.name);
        if let Some(instance) = &variable.instance {
            key.push('#');
            key.push_str(instance);
        }
        heapless::String::try_from(truncate_to_byte_boundary(&key, 50)).ok()
    }

    /// Decodes a flat key back into `(Component, Variable)`, the reverse of [`encode_key`]:
    /// [`STANDARD_KEY_ALIASES`] first (see the module docs), then this module's own dotted
    /// convention. `None` if `key` matches neither - any key not produced by this module's own
    /// `encode_key` and not a known standard alias is simply unrepresentable under this
    /// convention.
    fn decode_key(key: &str) -> Option<(Component, Variable)> {
        if let Some(pair) = decode_standard_key(key) {
            return Some(pair);
        }
        let (component_part, variable_part) = key.split_once('.')?;
        let (component_name, component_instance) = split_instance(component_part);
        let (variable_name, variable_instance) = split_instance(variable_part);
        Some((
            Component {
                name: component_name,
                instance: component_instance,
                evse: None,
            },
            Variable {
                name: variable_name,
                instance: variable_instance,
            },
        ))
    }

    /// Builds a 1.6J `ConfigurationKeyItem` for `(component, variable)`'s `Actual` attribute, if
    /// it has one and `component` is representable as a flat key at all.
    fn build_configuration_key_item(
        component: &Component,
        variable: &Variable,
        definition: &VariableDefinition,
    ) -> Option<ConfigurationKeyItem> {
        let key = encode_key(component, variable)?;
        let attribute = definition.attribute(VariableAttributeType::Actual)?;
        let readonly = attribute.mutability == VariableMutability::ReadOnly;
        let value = if attribute.mutability == VariableMutability::WriteOnly {
            None
        } else {
            heapless::String::try_from(truncate_to_byte_boundary(&attribute.value, 500)).ok()
        };
        Some(ConfigurationKeyItem {
            key,
            readonly,
            value,
        })
    }

    /// A 1.6J standard configuration key this crate answers from *live state* rather than from a
    /// stored device-model variable.
    ///
    /// Two different reasons land a key here, and the distinction is worth keeping straight:
    ///
    /// - **Derived** - the answer already exists somewhere authoritative, and storing a second
    ///   copy would let the two disagree. `NumberOfConnectors` is the hardware topology;
    ///   `LocalAuthListMaxLength`, `SendLocalListMaxLength` and `MaxChargingProfilesInstalled` are
    ///   [`crate::state::StateLimits`]; `SupportedFeatureProfiles` is
    ///   [`crate::hardware::Capabilities`]. A CSMS reading these gets what the charge point will
    ///   actually do, always.
    /// - **Advisory** - this crate imposes no limit at all, but 1.6J *requires* the key, so
    ///   refusing to answer would be a compliance failure. `GetConfigurationMaxKeys`,
    ///   `ChargeProfileMaxStackLevel` and `ChargingScheduleMaxPeriods` report a documented figure
    ///   a CSMS can size its requests against; exceeding it is accepted anyway. Reporting a real
    ///   bound this crate does not enforce would be the dishonest option, so each says so here.
    ///
    /// All are read-only: a `ChangeConfiguration` on one is `Rejected` (it exists, it just can't
    /// be written) rather than `NotSupported`, which would claim the charge point had never heard
    /// of it.
    struct DerivedKey {
        /// The 1.6J key name.
        key: &'static str,
        /// Computes the value from live state.
        value: fn(&crate::state::ChargePointState) -> String,
    }

    /// How many keys a `GetConfiguration` may request before this crate stops promising to answer
    /// them all. Purely advisory: nothing here rejects a larger request - see [`DerivedKey`].
    const GET_CONFIGURATION_MAX_KEYS: usize = 100;

    /// Advisory `ChargeProfileMaxStackLevel`/`ChargingScheduleMaxPeriods` figures - see
    /// [`DerivedKey`]. The charging profile store accepts any stack level and any number of
    /// schedule periods that fits its own bound, so these describe what a sane CSMS should send
    /// rather than what this charge point enforces.
    const ADVISORY_MAX_STACK_LEVEL: u32 = 8;
    const ADVISORY_MAX_SCHEDULE_PERIODS: u32 = 24;

    /// Every [`DerivedKey`] this module answers.
    const DERIVED_KEYS: &[DerivedKey] = &[
        DerivedKey {
            key: "NumberOfConnectors",
            value: |state| {
                let connectors: usize = state.evses.iter().map(|evse| evse.connectors.len()).sum();
                connectors.to_string()
            },
        },
        DerivedKey {
            key: "GetConfigurationMaxKeys",
            value: |_state| GET_CONFIGURATION_MAX_KEYS.to_string(),
        },
        DerivedKey {
            key: "LocalAuthListMaxLength",
            value: |state| state.local_authorization_list.max_entries.to_string(),
        },
        DerivedKey {
            key: "SendLocalListMaxLength",
            // The same bound: a `SendLocalList` that would exceed the list's capacity is refused
            // whole (see `crate::local_authorization_list`), so the two figures cannot differ.
            value: |state| state.local_authorization_list.max_entries.to_string(),
        },
        DerivedKey {
            key: "MaxChargingProfilesInstalled",
            value: |state| state.charging_profiles.max_profiles().to_string(),
        },
        DerivedKey {
            key: "ChargeProfileMaxStackLevel",
            value: |_state| ADVISORY_MAX_STACK_LEVEL.to_string(),
        },
        DerivedKey {
            key: "ChargingScheduleMaxPeriods",
            value: |_state| ADVISORY_MAX_SCHEDULE_PERIODS.to_string(),
        },
        DerivedKey {
            key: "ChargingScheduleAllowedChargingRateUnit",
            // Both, and genuinely: `crate::smart_charging::compose` reads whichever unit a
            // schedule is expressed in (converting only when the integrator supplied the supply
            // characteristics that make conversion honest).
            value: |_state| "Current,Power".into(),
        },
        DerivedKey {
            key: "ReserveConnectorZeroSupported",
            // 1.6J's connector 0 means "any connector on the charge point";
            // `crate::reservation::handle_reserve_now` picks a specific connector instead, so a
            // reservation is always against one connector. Answering `true` would promise a
            // behaviour this crate does not have.
            value: |_state| "false".into(),
        },
        DerivedKey {
            key: "ConnectorSwitch3to1PhaseSupported",
            // Phase switching is hardware this crate has no binding for at all.
            value: |_state| "false".into(),
        },
    ];

    /// Builds the read-only [`ConfigurationKeyItem`] for a derived key.
    fn derived_key_item(
        derived: &DerivedKey,
        state: &crate::state::ChargePointState,
    ) -> ConfigurationKeyItem {
        let value = (derived.value)(state);
        ConfigurationKeyItem {
            key: heapless::String::try_from(derived.key).unwrap_or_default(),
            readonly: true,
            value: heapless::String::try_from(truncate_to_byte_boundary(&value, 500)).ok(),
        }
    }

    /// The 1.6J standard `GetConfiguration` key name for `SupportedFeatureProfiles` - a
    /// comma-separated list of every functional-block profile this charge point genuinely
    /// supports (Core, FirmwareManagement, LocalAuthListManagement, Reservation, SmartCharging,
    /// RemoteTrigger - OCPP 1.6J Appendix 1). Synthetic: unlike every other key this module
    /// answers, it has no device model backing at all (1.6J has no Component/Variable model to
    /// register it against) - it's computed fresh from [`crate::hardware::Capabilities`] on every
    /// request via [`crate::hardware::supported_feature_profiles_1_6`] (C3.3,
    /// `docs/PRODUCTION-ROADMAP.md` §5.3), so it can never drift from what
    /// [`super::capability_gate_events`] advertised in the 2.x device model or what
    /// [`crate::setup::setup`] actually registered handlers for - all three read
    /// [`crate::hardware::CAPABILITY_GATES`]/`Capabilities` directly.
    const SUPPORTED_FEATURE_PROFILES_KEY: &str = "SupportedFeatureProfiles";

    /// Builds the synthetic `SupportedFeatureProfiles` [`ConfigurationKeyItem`] - see
    /// [`SUPPORTED_FEATURE_PROFILES_KEY`]'s docs.
    fn supported_feature_profiles_item(capabilities: &Capabilities) -> ConfigurationKeyItem {
        let value = crate::hardware::supported_feature_profiles_1_6(capabilities);
        ConfigurationKeyItem {
            key: heapless::String::try_from(SUPPORTED_FEATURE_PROFILES_KEY).unwrap(),
            readonly: true,
            value: heapless::String::try_from(truncate_to_byte_boundary(&value, 500)).ok(),
        }
    }

    /// Resolves a `GetConfiguration` request against `device_model`/`capabilities`: every
    /// registered charge-point-wide variable plus the synthetic `SupportedFeatureProfiles` key if
    /// `keys` is `None`, or just the requested ones (with unresolved keys collected separately
    /// into the second element) otherwise. See the module docs for why this reads the device
    /// model directly rather than through [`crate::device_model::handle_get_variables`].
    fn resolve_get_configuration(
        state: &crate::state::ChargePointState,
        keys: Option<&[heapless::String<50>]>,
    ) -> (Vec<ConfigurationKeyItem>, Vec<heapless::String<50>>) {
        let device_model: &DeviceModel = &state.device_model;
        let capabilities: &Capabilities = &state.capabilities;
        match keys {
            None => {
                let mut configuration_key: Vec<ConfigurationKeyItem> = device_model
                    .iter()
                    .filter_map(|(component, variable, definition)| {
                        build_configuration_key_item(component, variable, definition)
                    })
                    .collect();
                configuration_key.push(supported_feature_profiles_item(capabilities));
                configuration_key.extend(
                    DERIVED_KEYS
                        .iter()
                        .map(|derived| derived_key_item(derived, state)),
                );
                (configuration_key, Vec::new())
            }
            Some(keys) => {
                let mut configuration_key = Vec::new();
                let mut unknown_key = Vec::new();
                for key in keys {
                    if key.as_str() == SUPPORTED_FEATURE_PROFILES_KEY {
                        configuration_key.push(supported_feature_profiles_item(capabilities));
                        continue;
                    }
                    if let Some(derived) = DERIVED_KEYS
                        .iter()
                        .find(|derived| derived.key == key.as_str())
                    {
                        configuration_key.push(derived_key_item(derived, state));
                        continue;
                    }
                    let resolved = decode_key(key.as_str()).and_then(|(component, variable)| {
                        let definition = device_model.get(&component, &variable)?;
                        build_configuration_key_item(&component, &variable, definition)
                    });
                    match resolved {
                        Some(item) => configuration_key.push(item),
                        None => unknown_key.push(key.clone()),
                    }
                }
                (configuration_key, unknown_key)
            }
        }
    }

    /// Whether `key` is one this module answers from live state rather than the device model -
    /// i.e. a read-only key a `ChangeConfiguration` must be told it cannot write, rather than told
    /// it has never heard of. See [`DerivedKey`].
    fn is_read_only_synthetic_key(key: &str) -> bool {
        key == SUPPORTED_FEATURE_PROFILES_KEY
            || DERIVED_KEYS.iter().any(|derived| derived.key == key)
    }

    /// Collapses a [`SetVariableOutcome`] onto 1.6J's `ChangeConfigurationResponseStatus`: every
    /// "unknown"-shaped outcome becomes `NotSupported`, since 1.6J has no equivalent of
    /// `UnknownComponent`/`UnknownVariable`/`NotSupportedAttributeType`.
    pub(super) fn map_set_variable_outcome(
        outcome: SetVariableOutcome,
    ) -> ChangeConfigurationResponseStatus {
        match outcome {
            SetVariableOutcome::Accepted => ChangeConfigurationResponseStatus::Accepted,
            SetVariableOutcome::Rejected => ChangeConfigurationResponseStatus::Rejected,
            SetVariableOutcome::RebootRequired => ChangeConfigurationResponseStatus::RebootRequired,
            SetVariableOutcome::UnknownComponent
            | SetVariableOutcome::UnknownVariable
            | SetVariableOutcome::NotSupportedAttributeType => {
                ChangeConfigurationResponseStatus::NotSupported
            }
        }
    }

    #[async_trait::async_trait]
    impl GetVariablesHandler for OCPP1_6Client {
        async fn register_get_variables_handler(&self, actor: ChargePointActor) {
            self.on_get_configuration(move |request, _client| {
                let actor = actor.clone();
                async move {
                    let state = actor.state();
                    let (configuration_key, unknown_key) =
                        resolve_get_configuration(&state, request.key.as_deref());
                    Ok(GetConfigurationResponse {
                        configuration_key: (!configuration_key.is_empty())
                            .then_some(configuration_key),
                        unknown_key: (!unknown_key.is_empty()).then_some(unknown_key),
                    })
                }
            })
            .await;
        }
    }

    #[async_trait::async_trait]
    impl SetVariablesHandler for OCPP1_6Client {
        async fn register_set_variables_handler(&self, actor: ChargePointActor) {
            self.on_change_configuration(move |request, _client| {
                let actor = actor.clone();
                async move {
                    // A key this charge point answers from live state exists but cannot be
                    // written - `Rejected` says exactly that, where `NotSupported` would claim it
                    // had never heard of a key it just reported a value for.
                    if is_read_only_synthetic_key(request.key.as_str()) {
                        return Ok(ChangeConfigurationResponse {
                            status: ChangeConfigurationResponseStatus::Rejected,
                        });
                    }
                    let outcome = match decode_key(request.key.as_str()) {
                        Some((component, variable)) => handle_set_variables(
                            &actor,
                            alloc::vec![SetVariableRequest {
                                component,
                                variable,
                                attribute_type: VariableAttributeType::Actual,
                                value: request.value.to_string(),
                            }],
                        )
                        .await
                        .remove(0),
                        None => SetVariableOutcome::UnknownComponent,
                    };
                    Ok(ChangeConfigurationResponse {
                        status: map_set_variable_outcome(outcome),
                    })
                }
            })
            .await;
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn component(name: &str) -> Component {
            Component {
                name: name.into(),
                instance: None,
                evse: None,
            }
        }

        fn variable(name: &str) -> Variable {
            Variable {
                name: name.into(),
                instance: None,
            }
        }

        #[test]
        fn a_charge_point_wide_pair_round_trips_through_encode_and_decode() {
            let key =
                encode_key(&component("OCPPCommCtrlr"), &variable("HeartbeatInterval")).unwrap();

            let decoded = decode_key(key.as_str()).unwrap();

            assert_eq!(decoded.0, component("OCPPCommCtrlr"));
            assert_eq!(decoded.1, variable("HeartbeatInterval"));
        }

        #[test]
        fn instances_round_trip_too() {
            let component_with_instance = Component {
                name: "Comp".into(),
                instance: Some("1".into()),
                evse: None,
            };
            let variable_with_instance = Variable {
                name: "Var".into(),
                instance: Some("2".into()),
            };

            let key = encode_key(&component_with_instance, &variable_with_instance).unwrap();
            let decoded = decode_key(key.as_str()).unwrap();

            assert_eq!(decoded.0, component_with_instance);
            assert_eq!(decoded.1, variable_with_instance);
        }

        #[test]
        fn an_evse_scoped_component_has_no_flat_key() {
            let scoped = Component {
                name: "Connector".into(),
                instance: None,
                evse: Some((0, Some(0))),
            };

            assert_eq!(encode_key(&scoped, &variable("X")), None);
        }

        #[test]
        fn a_key_with_no_dot_separator_and_no_standard_alias_does_not_decode() {
            assert_eq!(decode_key("TotallyUnknownVendorKey"), None);
        }

        #[test]
        fn a_bare_standard_1_6_key_decodes_to_its_device_model_pair() {
            let decoded = decode_key("HeartbeatInterval").unwrap();

            assert_eq!(decoded.0, component("OCPPCommCtrlr"));
            assert_eq!(decoded.1, variable("HeartbeatInterval"));
        }

        #[test]
        fn a_renamed_standard_1_6_key_decodes_to_its_2_x_pair() {
            // 1.6J calls this key "AuthorizeRemoteTxRequests"; 2.0.1 renamed the variable to
            // "AuthorizeRemoteStart" on the "AuthCtrlr" component.
            let decoded = decode_key("AuthorizeRemoteTxRequests").unwrap();

            assert_eq!(decoded.0, component("AuthCtrlr"));
            assert_eq!(decoded.1, variable("AuthorizeRemoteStart"));
        }

        #[test]
        fn a_standard_key_with_a_variable_instance_decodes_it_too() {
            let decoded = decode_key("TransactionMessageAttempts").unwrap();

            assert_eq!(decoded.0, component("OCPPCommCtrlr"));
            assert_eq!(
                decoded.1,
                Variable {
                    name: "MessageAttempts".into(),
                    instance: Some("TransactionEvent".into()),
                }
            );
        }

        #[test]
        fn encoding_an_aliased_pair_emits_the_standard_key_name_not_the_dotted_form() {
            let key =
                encode_key(&component("OCPPCommCtrlr"), &variable("HeartbeatInterval")).unwrap();

            assert_eq!(key.as_str(), "HeartbeatInterval");
        }

        #[test]
        fn an_aliased_pair_round_trips_through_encode_and_decode() {
            let key =
                encode_key(&component("AuthCtrlr"), &variable("AuthorizeRemoteStart")).unwrap();

            let decoded = decode_key(key.as_str()).unwrap();

            assert_eq!(decoded.0, component("AuthCtrlr"));
            assert_eq!(decoded.1, variable("AuthorizeRemoteStart"));
        }

        #[test]
        fn a_standard_alias_key_round_trips_back_to_the_same_key() {
            // Decoding a real 1.6J standard key and re-encoding the resulting pair should
            // reproduce the same standard key, not fall through to the dotted convention.
            let (decoded_component, decoded_variable) =
                decode_key("MeterValueSampleInterval").unwrap();

            let re_encoded = encode_key(&decoded_component, &decoded_variable).unwrap();

            assert_eq!(re_encoded.as_str(), "MeterValueSampleInterval");
        }

        #[test]
        fn a_non_standard_component_is_unaffected_by_the_alias_table() {
            // "OCPPCommCtrlr.SomethingElse" isn't a standard 1.6J key alias, so it still only
            // decodes/encodes via the dotted convention.
            assert_eq!(
                encode_standard_key(&component("OCPPCommCtrlr"), &variable("SomethingElse")),
                None
            );
        }

        /// A charge point with one EVSE of one connector - `resolve_get_configuration` now reads
        /// the whole state, since several 1.6J keys are derived from topology and limits rather
        /// than stored (see [`DerivedKey`]).
        fn test_state() -> crate::state::ChargePointState {
            crate::state::ChargePointState::new([1])
        }

        /// OCPP 1.6J Appendix 1's **required** Core-profile configuration keys, plus the required
        /// keys of the profiles this crate implements (LocalAuthListManagement, Reservation,
        /// SmartCharging). A CSMS may read any of these at any time, and answering `unknownKey`
        /// for one is a compliance failure - so this list is the contract B1.6 exists to meet.
        ///
        /// `ConnectorPhaseRotation` is deliberately absent: 1.6 packs a per-connector list into a
        /// single key while 2.x models `PhaseRotation` per connector, and that fan-out doesn't fit
        /// a static key -> `(Component, Variable)` alias. It is the one required Core key this
        /// crate does not answer, and it is excluded explicitly here rather than quietly missing.
        const REQUIRED_1_6_KEYS: &[&str] = &[
            // Core
            "AuthorizeRemoteTxRequests",
            "ClockAlignedDataInterval",
            "ConnectionTimeOut",
            "GetConfigurationMaxKeys",
            "HeartbeatInterval",
            "LocalAuthorizeOffline",
            "LocalPreAuthorize",
            "MeterValuesAlignedData",
            "MeterValuesSampledData",
            "MeterValueSampleInterval",
            "NumberOfConnectors",
            "ResetRetries",
            "StopTransactionOnEVSideDisconnect",
            "StopTransactionOnInvalidId",
            "StopTxnAlignedData",
            "StopTxnSampledData",
            "SupportedFeatureProfiles",
            "TransactionMessageAttempts",
            "TransactionMessageRetryInterval",
            "UnlockConnectorOnEVSideDisconnect",
            // LocalAuthListManagement
            "LocalAuthListEnabled",
            "LocalAuthListMaxLength",
            "SendLocalListMaxLength",
            // SmartCharging
            "ChargeProfileMaxStackLevel",
            "ChargingScheduleAllowedChargingRateUnit",
            "ChargingScheduleMaxPeriods",
            "MaxChargingProfilesInstalled",
        ];

        /// B1.6's actual requirement, as a test rather than a claim: every required 1.6J key is
        /// readable on a charge point straight out of `ChargePointState::new` - no hardware
        /// binding, no CSMS configuration, nothing registered by anything but this crate's own
        /// defaults.
        #[test]
        fn every_required_1_6j_configuration_key_is_readable_on_a_fresh_charge_point() {
            let state = test_state();

            for key in REQUIRED_1_6_KEYS {
                let requested = alloc::vec![heapless::String::try_from(*key).unwrap()];
                let (configuration_key, unknown_key) =
                    resolve_get_configuration(&state, Some(&requested));

                assert!(
                    unknown_key.is_empty(),
                    "required 1.6J key `{key}` answered unknownKey"
                );
                assert_eq!(configuration_key.len(), 1, "`{key}` resolved oddly");
                assert!(
                    configuration_key[0].value.is_some(),
                    "required 1.6J key `{key}` has no value"
                );
            }
        }

        /// The other half of readability: an unfiltered `GetConfiguration` must *list* them, not
        /// merely answer when asked by name. A CSMS discovering a charge point reads it this way.
        #[test]
        fn an_unfiltered_get_configuration_lists_every_required_1_6j_key() {
            let (configuration_key, _) = resolve_get_configuration(&test_state(), None);

            for key in REQUIRED_1_6_KEYS {
                assert!(
                    configuration_key
                        .iter()
                        .any(|item| item.key.as_str() == *key),
                    "required 1.6J key `{key}` missing from an unfiltered GetConfiguration"
                );
            }
        }

        #[test]
        fn every_alias_resolves_to_a_variable_this_crate_actually_registers() {
            // An alias with nothing registered behind it is worse than no alias at all: the key
            // looks supported in this table and answers `unknownKey` on the wire.
            let state = test_state();
            for alias in STANDARD_KEY_ALIASES {
                let requested = alloc::vec![heapless::String::try_from(alias.key).unwrap()];
                let (configuration_key, unknown_key) =
                    resolve_get_configuration(&state, Some(&requested));
                assert!(
                    unknown_key.is_empty() && configuration_key.len() == 1,
                    "alias `{}` has no registered variable behind it",
                    alias.key
                );
            }
        }

        #[test]
        fn a_derived_key_cannot_be_written_and_says_so_specifically() {
            // `Rejected` ("it exists, you can't write it"), not `NotSupported` ("never heard of
            // it") - the charge point just reported a value for it.
            for key in ["NumberOfConnectors", "SupportedFeatureProfiles"] {
                assert!(is_read_only_synthetic_key(key), "`{key}` should be derived");
            }
            assert!(!is_read_only_synthetic_key("HeartbeatInterval"));
        }

        #[test]
        fn derived_keys_report_this_charge_points_real_topology_and_limits() {
            let state = crate::state::ChargePointState::with_limits(
                [2, 2],
                crate::state::StateLimits::default()
                    .with_max_local_authorization_list_entries(25)
                    .with_max_charging_profiles(4),
            );

            let value = |key: &str| {
                let requested = alloc::vec![heapless::String::try_from(key).unwrap()];
                let (items, _) = resolve_get_configuration(&state, Some(&requested));
                items[0]
                    .value
                    .as_deref()
                    .map(alloc::string::ToString::to_string)
            };

            assert_eq!(value("NumberOfConnectors").as_deref(), Some("4"));
            assert_eq!(value("LocalAuthListMaxLength").as_deref(), Some("25"));
            assert_eq!(value("SendLocalListMaxLength").as_deref(), Some("25"));
            assert_eq!(value("MaxChargingProfilesInstalled").as_deref(), Some("4"));
        }

        #[test]
        fn getting_every_key_lists_the_built_in_defaults_under_their_standard_names() {
            let (configuration_key, unknown_key) = resolve_get_configuration(&test_state(), None);

            assert!(unknown_key.is_empty());
            assert!(
                configuration_key
                    .iter()
                    .any(|item| item.key.as_str() == "HeartbeatInterval")
            );
            assert!(
                configuration_key
                    .iter()
                    .any(|item| item.key.as_str() == "AuthorizeRemoteTxRequests")
            );
            // Neither built-in default is listed under the old dotted form now that it has a
            // standard alias.
            assert!(
                !configuration_key
                    .iter()
                    .any(|item| item.key.as_str() == "OCPPCommCtrlr.HeartbeatInterval")
            );
        }

        #[test]
        fn requesting_a_known_key_by_its_dotted_form_still_works() {
            let (configuration_key, unknown_key) = resolve_get_configuration(
                &test_state(),
                Some(&[heapless::String::try_from("OCPPCommCtrlr.HeartbeatInterval").unwrap()]),
            );

            assert!(unknown_key.is_empty());
            assert_eq!(configuration_key.len(), 1);
            assert_eq!(configuration_key[0].value.as_deref(), Some("60"));
            assert!(!configuration_key[0].readonly);
        }

        #[test]
        fn requesting_a_known_key_by_its_standard_alias_resolves_the_same_variable() {
            let (configuration_key, unknown_key) = resolve_get_configuration(
                &test_state(),
                Some(&[heapless::String::try_from("HeartbeatInterval").unwrap()]),
            );

            assert!(unknown_key.is_empty());
            assert_eq!(configuration_key.len(), 1);
            assert_eq!(configuration_key[0].value.as_deref(), Some("60"));
            assert!(!configuration_key[0].readonly);
        }

        #[test]
        fn requesting_an_unrecognized_key_reports_it_as_unknown() {
            let (configuration_key, unknown_key) = resolve_get_configuration(
                &test_state(),
                Some(&[heapless::String::try_from("TotallyUnknownVendorKey").unwrap()]),
            );

            assert!(configuration_key.is_empty());
            assert_eq!(unknown_key.len(), 1);
        }

        #[tokio::test]
        async fn changing_configuration_by_a_standard_alias_key_updates_the_device_model() {
            use crate::actor::ChargePointActor;
            use crate::executor::TokioExecutor;

            let actor = ChargePointActor::spawn([1], &TokioExecutor);

            let outcome = match decode_key("HeartbeatInterval") {
                Some((decoded_component, decoded_variable)) => handle_set_variables(
                    &actor,
                    alloc::vec![SetVariableRequest {
                        component: decoded_component,
                        variable: decoded_variable,
                        attribute_type: VariableAttributeType::Actual,
                        value: "120".into(),
                    }],
                )
                .await
                .remove(0),
                None => panic!("standard alias key failed to decode"),
            };

            assert_eq!(outcome, SetVariableOutcome::Accepted);

            let (configuration_key, _) = resolve_get_configuration(
                &actor.state(),
                Some(&[heapless::String::try_from("HeartbeatInterval").unwrap()]),
            );
            assert_eq!(configuration_key[0].value.as_deref(), Some("120"));
        }

        #[test]
        fn every_set_variable_outcome_maps_to_a_wire_status() {
            assert_eq!(
                map_set_variable_outcome(SetVariableOutcome::Accepted),
                ChangeConfigurationResponseStatus::Accepted
            );
            assert_eq!(
                map_set_variable_outcome(SetVariableOutcome::Rejected),
                ChangeConfigurationResponseStatus::Rejected
            );
            assert_eq!(
                map_set_variable_outcome(SetVariableOutcome::RebootRequired),
                ChangeConfigurationResponseStatus::RebootRequired
            );
            assert_eq!(
                map_set_variable_outcome(SetVariableOutcome::UnknownComponent),
                ChangeConfigurationResponseStatus::NotSupported
            );
            assert_eq!(
                map_set_variable_outcome(SetVariableOutcome::UnknownVariable),
                ChangeConfigurationResponseStatus::NotSupported
            );
            assert_eq!(
                map_set_variable_outcome(SetVariableOutcome::NotSupportedAttributeType),
                ChangeConfigurationResponseStatus::NotSupported
            );
        }

        #[test]
        fn ocpp1_6_client_implements_the_handler_traits() {
            fn assert_impl<T: GetVariablesHandler + SetVariablesHandler>() {}
            assert_impl::<OCPP1_6Client>();
        }
    }
}
