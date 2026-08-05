//! Provisioning's Component/Variable device model functional block: CSMS-initiated
//! `GetVariables`/`SetVariables`. See `docs/ROADMAP.md` §2.
//!
//! OCPP 1.6J has no Component/Variable device model at all - its `ocpp_1_6` submodule instead
//! projects onto 1.6J's flat `GetConfiguration`/`ChangeConfiguration` pair via a documented
//! flat-key naming convention; see that submodule's docs for the convention and its limits.

use crate::actor::ChargePointActor;
use crate::state::{
    ChargePointEvent, Component, DeviceModel, DeviceModelEvent, Variable, VariableAttributeType,
    VariableMutability,
};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

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
    let Some(definition) = state.device_model.get(&request.component, &request.variable) else {
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
        handle_get_variables, handle_set_variables, GetVariableOutcome, GetVariableRequest,
        SetVariableOutcome, SetVariableRequest,
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
        handle_get_variables, handle_set_variables, GetVariableOutcome, GetVariableRequest,
        GetVariablesHandler, SetVariableOutcome, SetVariableRequest, SetVariablesHandler,
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

        fn wire_component(evse: Option<(i64, Option<i64>)>) -> ocpp_client::ocpp_types::v21::common::Component {
            ocpp_client::ocpp_types::v21::common::Component {
                custom_data: None,
                evse: evse.map(|(id, connector_id)| ocpp_client::ocpp_types::v21::common::EVSE {
                    connector_id,
                    custom_data: None,
                    id,
                }),
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
        handle_get_variables, handle_set_variables, GetVariableOutcome, GetVariableRequest,
        GetVariablesHandler, SetVariableOutcome, SetVariableRequest, SetVariablesHandler,
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
/// A key that doesn't decode under this convention - including 1.6J's own real standard key names
/// (e.g. `"HeartbeatInterval"`, with no component prefix at all) - is reported as `unknownKey`
/// (`GetConfiguration`) or `NotSupported` (`ChangeConfiguration`). This is a real, deliberate
/// limitation of this projection: this crate's device model doesn't know any bare aliases for its
/// variables, so a 1.6J CSMS that only knows the real standard key names won't find them under
/// this scheme. Closing that gap would need a hand-maintained alias table per standard variable
/// (out of scope here - see `docs/ROADMAP.md` §2's "still missing" notes).
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
    use super::{handle_set_variables, GetVariablesHandler, SetVariableOutcome, SetVariableRequest, SetVariablesHandler};
    use crate::actor::ChargePointActor;
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

    /// Encodes `(component, variable)` into this module's flat-key convention (see the module
    /// docs), truncated to fit 1.6J's 50-byte key bound. `None` for an EVSE/connector-scoped
    /// `component` - not representable under 1.6J at all.
    fn encode_key(component: &Component, variable: &Variable) -> Option<heapless::String<50>> {
        if component.evse.is_some() {
            return None;
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

    /// Decodes a flat key back into `(Component, Variable)`, the reverse of [`encode_key`].
    /// `None` if `key` has no `.` separator - any key not produced by this module's own
    /// `encode_key` (including 1.6J's own standard key names) is simply unrepresentable under
    /// this convention.
    fn decode_key(key: &str) -> Option<(Component, Variable)> {
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

    /// Resolves a `GetConfiguration` request against `device_model`: every registered
    /// charge-point-wide variable if `keys` is `None`, or just the requested ones (with
    /// unresolved keys collected separately into the second element) otherwise. See the module
    /// docs for why this reads the device model directly rather than through
    /// [`crate::device_model::handle_get_variables`].
    fn resolve_get_configuration(
        device_model: &DeviceModel,
        keys: Option<&[heapless::String<50>]>,
    ) -> (Vec<ConfigurationKeyItem>, Vec<heapless::String<50>>) {
        match keys {
            None => {
                let configuration_key = device_model
                    .iter()
                    .filter_map(|(component, variable, definition)| {
                        build_configuration_key_item(component, variable, definition)
                    })
                    .collect();
                (configuration_key, Vec::new())
            }
            Some(keys) => {
                let mut configuration_key = Vec::new();
                let mut unknown_key = Vec::new();
                for key in keys {
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
                        resolve_get_configuration(&state.device_model, request.key.as_deref());
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
            let key = encode_key(&component("OCPPCommCtrlr"), &variable("HeartbeatInterval")).unwrap();

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
        fn a_key_with_no_dot_separator_does_not_decode() {
            assert_eq!(decode_key("HeartbeatInterval"), None);
        }

        #[test]
        fn getting_every_key_lists_the_built_in_defaults() {
            let model = DeviceModel::new();

            let (configuration_key, unknown_key) = resolve_get_configuration(&model, None);

            assert!(unknown_key.is_empty());
            assert!(configuration_key
                .iter()
                .any(|item| item.key.as_str() == "OCPPCommCtrlr.HeartbeatInterval"));
        }

        #[test]
        fn requesting_a_known_key_returns_its_current_value_and_mutability() {
            let model = DeviceModel::new();

            let (configuration_key, unknown_key) = resolve_get_configuration(
                &model,
                Some(&[heapless::String::try_from("OCPPCommCtrlr.HeartbeatInterval").unwrap()]),
            );

            assert!(unknown_key.is_empty());
            assert_eq!(configuration_key.len(), 1);
            assert_eq!(configuration_key[0].value.as_deref(), Some("60"));
            assert!(!configuration_key[0].readonly);
        }

        #[test]
        fn requesting_an_unrecognized_key_reports_it_as_unknown() {
            let model = DeviceModel::new();

            let (configuration_key, unknown_key) = resolve_get_configuration(
                &model,
                Some(&[heapless::String::try_from("HeartbeatInterval").unwrap()]),
            );

            assert!(configuration_key.is_empty());
            assert_eq!(unknown_key.len(), 1);
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
