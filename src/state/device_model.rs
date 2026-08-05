use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

/// A component in the protocol-version-independent Component/Variable device model (OCPP
/// `ComponentType`), addressed the same way every other piece of this crate's state is: this
/// crate's own `(evse_id, connector_id)` `usize` indices, never OCPP's wire `EVSE` type.
///
/// `evse` distinguishes charge-point-wide components (`None`, e.g. `OCPPCommCtrlr`) from ones
/// scoped to a specific EVSE (`Some((evse_id, None))`) or a specific connector on that EVSE
/// (`Some((evse_id, Some(connector_id)))`) - mirroring OCPP's own `EVSE.connectorId` being
/// optional under a mandatory `EVSE.id`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Component {
    /// The component's name. Should be taken from OCPP's standardized component names whenever
    /// possible (e.g. `"OCPPCommCtrlr"`), but this crate doesn't enforce that.
    pub name: String,
    /// Disambiguates multiple instances of the same named component, if any.
    pub instance: Option<String>,
    /// The EVSE (and, optionally, connector) this component is scoped to, or `None` for a
    /// charge-point-wide component. See the struct docs.
    pub evse: Option<(usize, Option<usize>)>,
}

/// A variable on a [`Component`] in the device model (OCPP `VariableType`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Variable {
    /// The variable's name. Should be taken from OCPP's standardized variable names whenever
    /// possible (e.g. `"HeartbeatInterval"`), but this crate doesn't enforce that.
    pub name: String,
    /// Disambiguates multiple instances of the same named variable, if any.
    pub instance: Option<String>,
}

/// Which attribute of a [`Variable`] a [`VariableAttribute`] describes (OCPP `AttributeEnum`).
/// Most variables only ever have an `Actual` attribute; `Target`/`MinSet`/`MaxSet` exist for
/// variables representing a setpoint with configurable bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VariableAttributeType {
    /// The attribute's actual, current value.
    Actual,
    /// The attribute's target value.
    Target,
    /// The minimum value the `Target` attribute may be set to.
    MinSet,
    /// The maximum value the `Target` attribute may be set to.
    MaxSet,
}

/// Whether a [`VariableAttribute`] may be read, written, or both (OCPP `MutabilityEnum`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VariableMutability {
    /// Only `GetVariables` may read this attribute; `SetVariables` rejects writing it.
    ReadOnly,
    /// Only `SetVariables` may write this attribute; `GetVariables` rejects reading it (there's
    /// nothing meaningful to report back).
    WriteOnly,
    /// Both `GetVariables` and `SetVariables` may access this attribute.
    ReadWrite,
}

/// One attribute of a [`Variable`] - its current value plus the metadata that governs how
/// `GetVariables`/`SetVariables` (see `crate::device_model`) may access it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariableAttribute {
    /// Which attribute this is.
    pub attribute_type: VariableAttributeType,
    /// This attribute's current value, formatted as OCPP's wire representation would be (e.g.
    /// decimal text for a numeric variable). Stored as a plain string regardless of
    /// [`VariableCharacteristics::data_type`] - this crate doesn't parse/validate variable
    /// values against their declared type today.
    pub value: String,
    /// Whether this attribute may be read, written, or both.
    pub mutability: VariableMutability,
    /// Whether this attribute's value survives a reboot. Purely informational today - this
    /// crate doesn't yet persist the device model across restarts.
    pub persistent: bool,
    /// Whether this attribute's value can never be changed by `SetVariables` (OCPP: "value that
    /// will never be changed by the Charging Station at runtime"). A constant attribute is
    /// always rejected by `SetVariables` regardless of `mutability`.
    pub constant: bool,
    /// Whether successfully writing this attribute via `SetVariables` requires a `Reset` before
    /// it takes effect. Not part of OCPP's wire `VariableAttribute` (which has no such field) -
    /// this crate's own device model tracks it internally so
    /// [`crate::device_model::SetVariableOutcome::RebootRequired`] has something concrete to key
    /// off, rather than being permanently unreachable.
    pub requires_reboot: bool,
}

/// The wire data type of a [`Variable`] (OCPP `DataEnum`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VariableDataType {
    /// A free-form string.
    String,
    /// A decimal number.
    Decimal,
    /// An integer number.
    Integer,
    /// An ISO 8601 date-time.
    DateTime,
    /// A boolean.
    Boolean,
    /// A single value from [`VariableCharacteristics::values_list`].
    OptionList,
    /// An ordered subset of [`VariableCharacteristics::values_list`].
    SequenceList,
    /// An unordered subset of [`VariableCharacteristics::values_list`].
    MemberList,
}

/// Fixed, read-only metadata describing a [`Variable`] (OCPP `VariableCharacteristics`) - not
/// consumed by `GetVariables`/`SetVariables` themselves, but needed by the future
/// `GetBaseReport`/`GetReport`/`NotifyReport` functional block (see `docs/ROADMAP.md` §2), which
/// is why this model carries it now even though nothing reads it yet.
#[derive(Debug, Clone, PartialEq)]
pub struct VariableCharacteristics {
    /// This variable's wire data type.
    pub data_type: VariableDataType,
    /// This variable's unit, if it has one (e.g. `"s"` for a seconds-valued variable).
    pub unit: Option<String>,
    /// The minimum possible value of this variable, if bounded.
    pub min_limit: Option<f64>,
    /// The maximum possible value of this variable (or, for a string-shaped `data_type`, its
    /// maximum length), if bounded.
    pub max_limit: Option<f64>,
    /// The allowed values, for `OptionList`/`SequenceList`/`MemberList` variables.
    pub values_list: Option<Vec<String>>,
    /// Whether this variable supports variable monitoring (OCPP `SetVariableMonitoring`) - not
    /// implemented by this crate yet (see `docs/ROADMAP.md` §2), so always `false` today.
    pub supports_monitoring: bool,
}

/// One variable's full definition in the device model: its fixed characteristics plus its
/// current attribute(s).
#[derive(Debug, Clone, PartialEq)]
pub struct VariableDefinition {
    /// This variable's fixed, read-only metadata.
    pub characteristics: VariableCharacteristics,
    /// This variable's attribute(s). Most variables have exactly one (`Actual`); a setpoint-style
    /// variable may also have `Target`/`MinSet`/`MaxSet`.
    pub attributes: Vec<VariableAttribute>,
}

impl VariableDefinition {
    /// The attribute of type `attribute_type`, if this variable has one.
    pub fn attribute(&self, attribute_type: VariableAttributeType) -> Option<&VariableAttribute> {
        self.attributes
            .iter()
            .find(|attribute| attribute.attribute_type == attribute_type)
    }
}

/// The charge point's Component/Variable device model (OCPP's Provisioning device model - see
/// `docs/ROADMAP.md` §2), keyed by `(Component, Variable)` and iterated in a stable order
/// (components, then variables within each component) - needed by the future `GetBaseReport`
/// functional block, which reports the whole model back to the CSMS.
///
/// Owned by [`crate::state::ChargePointState`] and mutated only through
/// [`crate::state::DeviceModelEvent`] applied by [`crate::actor::ChargePointActor`], same as
/// every other piece of this crate's state - never mutate a `DeviceModel` directly. A hardware
/// binding extends it with whatever components its hardware actually exposes by pushing
/// [`crate::state::DeviceModelEvent::VariableRegistered`] via
/// [`crate::hardware::HardwareEventSender::send`] during
/// [`crate::hardware::ChargePoint::start`], the same way it pushes any other
/// [`crate::state::ChargePointEvent`].
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceModel {
    components: BTreeMap<Component, BTreeMap<Variable, VariableDefinition>>,
}

impl DeviceModel {
    /// A device model pre-populated with this crate's built-in default variables (see
    /// [`register_defaults`](Self::register_defaults)'s docs) - deliberately a minimal,
    /// non-exhaustive set. Integrators extend it with
    /// [`DeviceModelEvent::VariableRegistered`](crate::state::DeviceModelEvent::VariableRegistered)
    /// for whatever components their hardware actually exposes.
    pub fn new() -> Self {
        let mut model = Self {
            components: BTreeMap::new(),
        };
        model.register_defaults();
        model
    }

    /// Registers (or replaces) the full definition of `variable` on `component`. The only way to
    /// add or redefine a variable - see this method's caller,
    /// [`crate::state::ChargePointState::apply`].
    pub fn register(
        &mut self,
        component: Component,
        variable: Variable,
        characteristics: VariableCharacteristics,
        attributes: Vec<VariableAttribute>,
    ) {
        self.components.entry(component).or_default().insert(
            variable,
            VariableDefinition {
                characteristics,
                attributes,
            },
        );
    }

    /// The full definition of `variable` on `component`, if registered.
    pub fn get(&self, component: &Component, variable: &Variable) -> Option<&VariableDefinition> {
        self.components.get(component)?.get(variable)
    }

    /// Whether `component` has any registered variable at all - used to distinguish
    /// `UnknownComponent` from `UnknownVariable` (see `crate::device_model`).
    pub fn has_component(&self, component: &Component) -> bool {
        self.components.contains_key(component)
    }

    /// Sets the value of `variable`'s `attribute_type` attribute on `component`. Returns whether
    /// anything was actually there to set (i.e. `component`/`variable`/`attribute_type` were all
    /// already registered) - a no-op otherwise, same tolerance-of-unrecognized-events convention
    /// as [`crate::state::ChargePointState::apply`].
    pub fn set_attribute_value(
        &mut self,
        component: &Component,
        variable: &Variable,
        attribute_type: VariableAttributeType,
        value: String,
    ) -> bool {
        let Some(variables) = self.components.get_mut(component) else {
            return false;
        };
        let Some(definition) = variables.get_mut(variable) else {
            return false;
        };
        let Some(attribute) = definition
            .attributes
            .iter_mut()
            .find(|attribute| attribute.attribute_type == attribute_type)
        else {
            return false;
        };
        attribute.value = value;
        true
    }

    /// Iterates every registered `(Component, Variable, VariableDefinition)`, ordered by
    /// component then variable (the order `BTreeMap` already stores them in). Needed by the
    /// future `GetBaseReport` functional block, and by this crate's own OCPP 1.6J
    /// `GetConfiguration` projection (`crate::device_model::ocpp_1_6`) to list every
    /// charge-point-wide variable as a flat configuration key.
    pub fn iter(&self) -> impl Iterator<Item = (&Component, &Variable, &VariableDefinition)> {
        self.components.iter().flat_map(|(component, variables)| {
            variables
                .iter()
                .map(move |(variable, definition)| (component, variable, definition))
        })
    }

    /// Registers this crate's minimal built-in default variables - deliberately not exhaustive
    /// (see [`new`](Self::new)'s docs):
    ///
    /// - `OCPPCommCtrlr`/`HeartbeatInterval` (`Integer`, seconds, `ReadWrite`): the interval
    ///   `crate::provisioning::run_heartbeat` sends a `Heartbeat` at. Not yet consulted by
    ///   `run_heartbeat` itself (see `docs/ROADMAP.md` §2) - registered here so it exists in the
    ///   model and is reachable via `GetVariables`/`SetVariables` today.
    /// - `AuthCtrlr`/`AuthorizeRemoteStart` (`Boolean`, `ReadWrite`): whether a CSMS-initiated
    ///   `RequestStartTransaction` still needs an `Authorize` round trip. Not yet consulted by
    ///   `crate::remote_control` either, for the same reason.
    fn register_defaults(&mut self) {
        self.register(
            Component {
                name: "OCPPCommCtrlr".into(),
                instance: None,
                evse: None,
            },
            Variable {
                name: "HeartbeatInterval".into(),
                instance: None,
            },
            VariableCharacteristics {
                data_type: VariableDataType::Integer,
                unit: Some("s".into()),
                min_limit: None,
                max_limit: None,
                values_list: None,
                supports_monitoring: false,
            },
            vec![VariableAttribute {
                attribute_type: VariableAttributeType::Actual,
                value: "60".into(),
                mutability: VariableMutability::ReadWrite,
                persistent: true,
                constant: false,
                requires_reboot: false,
            }],
        );
        self.register(
            Component {
                name: "AuthCtrlr".into(),
                instance: None,
                evse: None,
            },
            Variable {
                name: "AuthorizeRemoteStart".into(),
                instance: None,
            },
            VariableCharacteristics {
                data_type: VariableDataType::Boolean,
                unit: None,
                min_limit: None,
                max_limit: None,
                values_list: None,
                supports_monitoring: false,
            },
            vec![VariableAttribute {
                attribute_type: VariableAttributeType::Actual,
                value: "false".into(),
                mutability: VariableMutability::ReadWrite,
                persistent: true,
                constant: false,
                requires_reboot: false,
            }],
        );
    }
}

impl Default for DeviceModel {
    fn default() -> Self {
        Self::new()
    }
}

/// An event mutating [`DeviceModel`], applied by [`crate::state::ChargePointState::apply`] via
/// [`crate::state::ChargePointEvent::DeviceModel`].
#[derive(Debug, Clone, PartialEq)]
pub enum DeviceModelEvent {
    /// Registers (or replaces) the full characteristics/attributes for one component/variable
    /// pair. A hardware binding pushes this (via
    /// [`crate::hardware::HardwareEventSender::send`], during
    /// [`crate::hardware::ChargePoint::start`]) to describe whatever components its hardware
    /// actually exposes, extending this crate's minimal built-in default set - see
    /// [`DeviceModel::new`].
    VariableRegistered {
        /// The component to register `variable` on.
        component: Component,
        /// The variable being registered.
        variable: Variable,
        /// The variable's fixed characteristics.
        characteristics: VariableCharacteristics,
        /// The variable's attribute(s).
        attributes: Vec<VariableAttribute>,
    },
    /// Sets one already-registered attribute's value (OCPP `SetVariables`, or 1.6J
    /// `ChangeConfiguration` projected onto it - see `crate::device_model`). A no-op if the
    /// component/variable/attribute-type combination isn't registered; in practice this never
    /// happens, since `crate::device_model::handle_set_variables` only ever sends this after
    /// confirming the attribute exists and is writable.
    AttributeValueSet {
        /// The already-registered component.
        component: Component,
        /// The already-registered variable.
        variable: Variable,
        /// Which attribute to set.
        attribute_type: VariableAttributeType,
        /// The new value.
        value: String,
    },
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

    fn characteristics() -> VariableCharacteristics {
        VariableCharacteristics {
            data_type: VariableDataType::String,
            unit: None,
            min_limit: None,
            max_limit: None,
            values_list: None,
            supports_monitoring: false,
        }
    }

    fn attribute(value: &str, mutability: VariableMutability) -> VariableAttribute {
        VariableAttribute {
            attribute_type: VariableAttributeType::Actual,
            value: value.into(),
            mutability,
            persistent: false,
            constant: false,
            requires_reboot: false,
        }
    }

    #[test]
    fn a_fresh_device_model_has_the_built_in_defaults() {
        let model = DeviceModel::new();

        let heartbeat = model
            .get(&component("OCPPCommCtrlr"), &variable("HeartbeatInterval"))
            .unwrap();
        assert_eq!(
            heartbeat
                .attribute(VariableAttributeType::Actual)
                .unwrap()
                .value,
            "60"
        );

        let auth = model
            .get(&component("AuthCtrlr"), &variable("AuthorizeRemoteStart"))
            .unwrap();
        assert_eq!(
            auth.attribute(VariableAttributeType::Actual).unwrap().value,
            "false"
        );
    }

    #[test]
    fn an_unregistered_component_is_not_found() {
        let model = DeviceModel::new();

        assert_eq!(model.get(&component("Nonexistent"), &variable("X")), None);
        assert!(!model.has_component(&component("Nonexistent")));
    }

    #[test]
    fn registering_a_variable_makes_it_lookupable() {
        let mut model = DeviceModel::new();

        model.register(
            component("Custom"),
            variable("Setting"),
            characteristics(),
            vec![attribute("hello", VariableMutability::ReadWrite)],
        );

        assert!(model.has_component(&component("Custom")));
        let definition = model
            .get(&component("Custom"), &variable("Setting"))
            .unwrap();
        assert_eq!(
            definition
                .attribute(VariableAttributeType::Actual)
                .unwrap()
                .value,
            "hello"
        );
    }

    #[test]
    fn setting_an_attribute_value_updates_it_in_place() {
        let mut model = DeviceModel::new();
        model.register(
            component("Custom"),
            variable("Setting"),
            characteristics(),
            vec![attribute("hello", VariableMutability::ReadWrite)],
        );

        let changed = model.set_attribute_value(
            &component("Custom"),
            &variable("Setting"),
            VariableAttributeType::Actual,
            "world".into(),
        );

        assert!(changed);
        assert_eq!(
            model
                .get(&component("Custom"), &variable("Setting"))
                .unwrap()
                .attribute(VariableAttributeType::Actual)
                .unwrap()
                .value,
            "world"
        );
    }

    #[test]
    fn setting_an_unregistered_attribute_is_a_no_op() {
        let mut model = DeviceModel::new();

        let changed = model.set_attribute_value(
            &component("Nonexistent"),
            &variable("X"),
            VariableAttributeType::Actual,
            "value".into(),
        );

        assert!(!changed);
    }

    #[test]
    fn iteration_is_ordered_by_component_then_variable() {
        let mut model = DeviceModel::new();
        model.register(
            component("Zeta"),
            variable("A"),
            characteristics(),
            vec![attribute("1", VariableMutability::ReadOnly)],
        );
        model.register(
            component("Alpha"),
            variable("B"),
            characteristics(),
            vec![attribute("2", VariableMutability::ReadOnly)],
        );

        let names: Vec<_> = model
            .iter()
            .map(|(component, _, _)| component.name.as_str())
            .collect();

        // `Alpha`, `AuthCtrlr`, `OCPPCommCtrlr`, `Zeta` - alphabetical, defaults included.
        assert_eq!(names, vec!["Alpha", "AuthCtrlr", "OCPPCommCtrlr", "Zeta"]);
    }
}
