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
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
/// The model is bounded at construction by [`DeviceModel::max_variables`] (see
/// [`crate::state::StateLimits`]) so a hardware binding that registers variables in a loop - or one
/// driven by a runaway sensor enumeration - can't grow state without limit
/// (`docs/PRODUCTION-ROADMAP.md` §9.2, G2.2).
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceModel {
    components: BTreeMap<Component, BTreeMap<Variable, VariableDefinition>>,
    max_variables: usize,
}

impl DeviceModel {
    /// A device model pre-populated with this crate's built-in default variables (see
    /// this type's own docs) - deliberately a minimal,
    /// non-exhaustive set. Integrators extend it with
    /// [`DeviceModelEvent::VariableRegistered`](crate::state::DeviceModelEvent::VariableRegistered)
    /// for whatever components their hardware actually exposes. Holds at most
    /// [`DEFAULT_MAX_DEVICE_MODEL_VARIABLES`](crate::state::DEFAULT_MAX_DEVICE_MODEL_VARIABLES)
    /// variables.
    pub fn new() -> Self {
        Self::with_max_variables(crate::state::DEFAULT_MAX_DEVICE_MODEL_VARIABLES)
    }

    /// A device model as [`new`](Self::new), holding at most `max_variables` `(Component,
    /// Variable)` pairs.
    ///
    /// The bound is raised to whatever the built-in defaults themselves occupy if `max_variables`
    /// is smaller: a limit low enough to drop this crate's own defaults would leave
    /// `GetVariables`/`GetBaseReport` reporting an incomplete model rather than bound anything worth
    /// bounding. It is *not* raised for the `*Ctrlr.Available` capability-gate variables
    /// [`crate::device_model::capability_gate_events`] registers afterwards - those come through
    /// [`Self::register`] like any other registration, and a caller who sets a limit that can't fit
    /// them gets the documented refusal (and a warning) for the ones that don't fit.
    pub fn with_max_variables(max_variables: usize) -> Self {
        let mut model = Self {
            components: BTreeMap::new(),
            max_variables: usize::MAX,
        };
        model.register_defaults();
        model.max_variables = max_variables.max(model.len()).max(1);
        model
    }

    /// Registers (or replaces) the full definition of `variable` on `component`. The only way to
    /// add or redefine a variable - see this method's caller,
    /// [`crate::state::ChargePointState::apply`].
    ///
    /// Returns whether the model was actually changed. A registration that would push the model
    /// past [`max_variables`](Self::max_variables) is refused (and logged) rather than applied;
    /// redefining an *already registered* variable is always allowed, since it doesn't grow the
    /// model.
    pub fn register(
        &mut self,
        component: Component,
        variable: Variable,
        characteristics: VariableCharacteristics,
        attributes: Vec<VariableAttribute>,
    ) -> bool {
        let known = self
            .components
            .get(&component)
            .is_some_and(|variables| variables.contains_key(&variable));
        if !known && self.len() >= self.max_variables {
            tracing::warn!(
                component = component.name.as_str(),
                variable = variable.name.as_str(),
                max_variables = self.max_variables,
                "refusing to register a device model variable - the configured maximum is reached"
            );
            return false;
        }
        self.components.entry(component).or_default().insert(
            variable,
            VariableDefinition {
                characteristics,
                attributes,
            },
        );
        true
    }

    /// How many `(Component, Variable)` pairs are registered, counting this crate's own built-in
    /// defaults. Bounded by [`max_variables`](Self::max_variables).
    pub fn len(&self) -> usize {
        self.components.values().map(BTreeMap::len).sum()
    }

    /// Whether nothing at all is registered. Never true for a model from [`Self::new`], which
    /// registers this crate's built-in defaults.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The most `(Component, Variable)` pairs this model may hold, fixed for the life of the charge
    /// point. See [`crate::state::StateLimits::max_device_model_variables`], which is where callers
    /// configure it.
    pub fn max_variables(&self) -> usize {
        self.max_variables
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
        // Authorization caching (docs/PRODUCTION-ROADMAP.md B1.2). All three are registered
        // explicitly rather than left absent with a guessed fallback: a charge point's answer to
        // "may this card charge while you're offline?" should be readable by the CSMS, not
        // inferred from what this crate assumes when a variable is missing.
        self.register(
            Component {
                name: "AuthCacheCtrlr".into(),
                instance: None,
                evse: None,
            },
            Variable {
                name: "Enabled".into(),
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
            alloc::vec![VariableAttribute {
                attribute_type: VariableAttributeType::Actual,
                value: "true".into(),
                mutability: VariableMutability::ReadWrite,
                persistent: false,
                constant: false,
                requires_reboot: false,
            }],
        );
        self.register(
            Component {
                name: "AuthCacheCtrlr".into(),
                instance: None,
                evse: None,
            },
            Variable {
                name: "LifeTime".into(),
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
            // `0` is OCPP's "no lifetime configured", which this crate reads as "entries don't
            // expire on age". They are still bounded in number and still clearable.
            alloc::vec![VariableAttribute {
                attribute_type: VariableAttributeType::Actual,
                value: "0".into(),
                mutability: VariableMutability::ReadWrite,
                persistent: false,
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
                name: "LocalAuthorizeOffline".into(),
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
            alloc::vec![VariableAttribute {
                attribute_type: VariableAttributeType::Actual,
                value: "true".into(),
                mutability: VariableMutability::ReadWrite,
                persistent: false,
                constant: false,
                requires_reboot: false,
            }],
        );
        // Clock-aligned data (docs/PRODUCTION-ROADMAP.md B1.1). `0` is OCPP's own "disabled",
        // which is the right default for a charge point nobody has configured yet: a charge point
        // that started reporting on a 15-minute drumbeat because this crate picked a number would
        // be spending a CSMS's bandwidth on a decision it never made.
        self.register(
            Component {
                name: "AlignedDataCtrlr".into(),
                instance: None,
                evse: None,
            },
            Variable {
                name: "Interval".into(),
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
            alloc::vec![VariableAttribute {
                attribute_type: VariableAttributeType::Actual,
                value: "0".into(),
                mutability: VariableMutability::ReadWrite,
                persistent: false,
                constant: false,
                requires_reboot: false,
            }],
        );
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

    // G2.2 (docs/PRODUCTION-ROADMAP.md §9.2)
    #[test]
    fn a_fresh_model_uses_the_default_maximum() {
        let model = DeviceModel::new();

        assert_eq!(
            model.max_variables(),
            crate::state::DEFAULT_MAX_DEVICE_MODEL_VARIABLES
        );
        assert_eq!(
            model.len(),
            6,
            "the built-in defaults: HeartbeatInterval, AuthorizeRemoteStart, \
             AlignedDataCtrlr.Interval, AuthCacheCtrlr.Enabled, AuthCacheCtrlr.LifeTime, \
             AuthCtrlr.LocalAuthorizeOffline"
        );
    }

    #[test]
    fn registering_past_the_maximum_is_refused_and_leaves_the_model_alone() {
        // One slot above the built-in defaults, so exactly one custom registration fits.
        let mut model = DeviceModel::with_max_variables(7);
        assert!(model.register(
            component("Custom"),
            variable("First"),
            characteristics(),
            vec![attribute("1", VariableMutability::ReadWrite)],
        ));

        let registered = model.register(
            component("Custom"),
            variable("Second"),
            characteristics(),
            vec![attribute("2", VariableMutability::ReadWrite)],
        );

        assert!(!registered);
        assert_eq!(model.len(), 7);
        assert_eq!(model.get(&component("Custom"), &variable("Second")), None);
    }

    /// Redefining an already-registered variable doesn't grow the model, so the bound must not
    /// block it - otherwise a full model could never have a value's characteristics corrected.
    #[test]
    fn redefining_an_existing_variable_is_allowed_at_the_maximum() {
        let mut model = DeviceModel::with_max_variables(6);

        let registered = model.register(
            component("OCPPCommCtrlr"),
            variable("HeartbeatInterval"),
            characteristics(),
            vec![attribute("120", VariableMutability::ReadWrite)],
        );

        assert!(registered);
        assert_eq!(model.len(), 6);
        assert_eq!(
            model
                .get(&component("OCPPCommCtrlr"), &variable("HeartbeatInterval"))
                .unwrap()
                .attribute(VariableAttributeType::Actual)
                .unwrap()
                .value,
            "120"
        );
    }

    /// A maximum below what the built-in defaults occupy is raised to fit them - see
    /// [`DeviceModel::with_max_variables`].
    #[test]
    fn a_maximum_below_the_built_in_defaults_is_raised_to_fit_them() {
        let model = DeviceModel::with_max_variables(0);

        assert_eq!(model.max_variables(), 6);
        assert_eq!(model.len(), 6);
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

        // Alphabetical, with all three built-in defaults' components included.
        assert_eq!(
            names,
            vec![
                "AlignedDataCtrlr",
                "Alpha",
                "AuthCacheCtrlr",
                "AuthCacheCtrlr",
                "AuthCtrlr",
                "AuthCtrlr",
                "OCPPCommCtrlr",
                "Zeta"
            ]
        );
    }
}
