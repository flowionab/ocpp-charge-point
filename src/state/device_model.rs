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
    /// Whether this variable supports variable monitoring (OCPP `SetVariableMonitoring`).
    /// `crate::variable_monitoring::handle_set_variable_monitoring` refuses (`Rejected`) a
    /// monitor on a variable where this is `false` - see B5.2, `docs/ROADMAP.md` §2/§14. None of
    /// this crate's own built-in default variables set it `true` today; a hardware binding
    /// registering its own variables (e.g. a temperature or voltage reading worth alerting on)
    /// decides this per variable when it registers one.
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
        for default in DEFAULT_VARIABLES {
            self.register(
                Component {
                    name: default.component.into(),
                    instance: None,
                    evse: None,
                },
                Variable {
                    name: default.variable.into(),
                    instance: default.instance.map(Into::into),
                },
                VariableCharacteristics {
                    data_type: default.data_type,
                    unit: default.unit.map(Into::into),
                    min_limit: None,
                    max_limit: None,
                    values_list: None,
                    supports_monitoring: false,
                },
                vec![VariableAttribute {
                    attribute_type: VariableAttributeType::Actual,
                    value: default.value.into(),
                    mutability: default.mutability,
                    persistent: default.persistent,
                    constant: false,
                    requires_reboot: false,
                }],
            );
        }
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

/// One variable [`DeviceModel::register_defaults`] registers on every charge point.
///
/// The table exists because these are not this crate's inventions: each is a variable OCPP itself
/// standardizes, and most are 1.6J *required* configuration keys that a CSMS may read at any time
/// (`docs/PRODUCTION-ROADMAP.md` B1.6). Registering them here is what makes them readable at all -
/// `crate::device_model`'s 1.6J adapter aliases a key onto a `(Component, Variable)` and then
/// looks it up, so an alias with nothing registered behind it answers `unknownKey`.
pub(crate) struct DefaultVariable {
    /// The 2.x component name (1.6J's flat key aliases onto it - see `crate::device_model`).
    pub component: &'static str,
    /// The 2.x variable name.
    pub variable: &'static str,
    /// The variable's instance, where OCPP disambiguates several uses of one name (e.g.
    /// `OCPPCommCtrlr.MessageAttempts[TransactionEvent]`).
    pub instance: Option<&'static str>,
    /// The variable's data type, as reported by `GetVariables`/`GetReport`.
    pub data_type: VariableDataType,
    /// The unit, where OCPP defines one.
    pub unit: Option<&'static str>,
    /// The value a charge point starts with.
    pub value: &'static str,
    /// Whether a CSMS may write it. **`ReadWrite` here means the value is writable, not that this
    /// crate acts on it** - see [`DEFAULT_VARIABLES`]' own docs for which are live.
    pub mutability: VariableMutability,
    /// Whether the value should survive a reboot (see
    /// [`VariableAttribute::persistent`](crate::state::VariableAttribute::persistent) and E2.3).
    ///
    /// True only where re-learning the value costs something a CSMS would notice: a heartbeat
    /// interval it negotiated once at BootNotification, say. Most of these are re-sent by the CSMS
    /// on connecting anyway, and a stale configuration value that outlived the CSMS's own view of
    /// it would be worse than a default.
    pub persistent: bool,
}

/// Every variable registered on a fresh [`DeviceModel`].
///
/// # Which of these actually *do* something
///
/// Being registered makes a variable readable and (where `ReadWrite`) writable. It does not by
/// itself make the charge point behave differently, and this table deliberately mixes both kinds
/// rather than pretending otherwise:
///
/// - **Live** - read by this crate on the path they govern, so a CSMS write changes behaviour on
///   the next cycle: `OCPPCommCtrlr.HeartbeatInterval` (`crate::provisioning::run_heartbeat`),
///   `AlignedDataCtrlr.Interval` (`crate::meter_values`), `AuthCacheCtrlr.Enabled`,
///   `AuthCacheCtrlr.LifeTime` and `AuthCtrlr.LocalAuthorizeOffline` (`crate::authorization`'s
///   offline path).
/// - **Recorded** - stored and reported faithfully, but nothing in this crate consults them yet.
///   They are registered because a 1.6J CSMS may *require* them to be readable, and answering
///   `unknownKey` for a required key is a compliance failure in a way that answering with an
///   honest, unacted-on value is not. Each is listed in `docs/ROADMAP.md` §2 as outstanding.
///
/// The distinction is deliberately visible rather than hidden: `crate::device_model`'s
/// `standard_key_is_honoured` reports it, and a test asserts the two lists stay in step.
pub(crate) const DEFAULT_VARIABLES: &[DefaultVariable] = &[
    // --- live: read by this crate on the path they govern ---
    DefaultVariable {
        component: "OCPPCommCtrlr",
        variable: "HeartbeatInterval",
        instance: None,
        data_type: VariableDataType::Integer,
        unit: Some("s"),
        value: "60",
        mutability: VariableMutability::ReadWrite,
        // The accepted BootNotification interval is written here after registration, and
        // re-learning it costs a CSMS round trip - one of the two values worth keeping.
        persistent: true,
    },
    DefaultVariable {
        component: "AlignedDataCtrlr",
        variable: "Interval",
        instance: None,
        data_type: VariableDataType::Integer,
        unit: Some("s"),
        // OCPP's own "clock-aligned data is disabled" - a charge point nobody configured should
        // not start reporting on a drumbeat this crate chose for it.
        value: "0",
        mutability: VariableMutability::ReadWrite,
        persistent: false,
    },
    DefaultVariable {
        component: "AuthCacheCtrlr",
        variable: "Enabled",
        instance: None,
        data_type: VariableDataType::Boolean,
        unit: None,
        value: "true",
        mutability: VariableMutability::ReadWrite,
        persistent: false,
    },
    DefaultVariable {
        component: "AuthCacheCtrlr",
        variable: "LifeTime",
        instance: None,
        data_type: VariableDataType::Integer,
        unit: Some("s"),
        // 0 = entries don't age out. They are still bounded in number and still clearable.
        value: "0",
        mutability: VariableMutability::ReadWrite,
        persistent: false,
    },
    DefaultVariable {
        component: "AuthCtrlr",
        variable: "LocalAuthorizeOffline",
        instance: None,
        data_type: VariableDataType::Boolean,
        unit: None,
        value: "true",
        mutability: VariableMutability::ReadWrite,
        persistent: false,
    },
    DefaultVariable {
        component: "AuthCtrlr",
        variable: "AuthorizeRemoteStart",
        instance: None,
        data_type: VariableDataType::Boolean,
        unit: None,
        value: "false",
        mutability: VariableMutability::ReadWrite,
        persistent: true,
    },
    // --- recorded: readable and writable, but not yet consulted by this crate ---
    DefaultVariable {
        component: "AuthCtrlr",
        variable: "LocalPreAuthorize",
        instance: None,
        data_type: VariableDataType::Boolean,
        unit: None,
        // False is the honest default for a switch this crate does not implement: claiming to
        // pre-authorize from the local list while online, and then not doing it, would be worse
        // than saying no.
        value: "false",
        mutability: VariableMutability::ReadWrite,
        persistent: false,
    },
    DefaultVariable {
        component: "AuthCtrlr",
        variable: "OfflineTxForUnknownIdEnabled",
        instance: None,
        data_type: VariableDataType::Boolean,
        unit: None,
        value: "false",
        mutability: VariableMutability::ReadWrite,
        persistent: false,
    },
    DefaultVariable {
        component: "TxCtrlr",
        variable: "EVConnectionTimeOut",
        instance: None,
        data_type: VariableDataType::Integer,
        unit: Some("s"),
        value: "120",
        mutability: VariableMutability::ReadWrite,
        persistent: false,
    },
    DefaultVariable {
        component: "TxCtrlr",
        variable: "StopTxOnEVSideDisconnect",
        instance: None,
        data_type: VariableDataType::Boolean,
        unit: None,
        value: "true",
        mutability: VariableMutability::ReadWrite,
        persistent: false,
    },
    DefaultVariable {
        component: "TxCtrlr",
        variable: "StopTxOnInvalidId",
        instance: None,
        data_type: VariableDataType::Boolean,
        unit: None,
        value: "true",
        mutability: VariableMutability::ReadWrite,
        persistent: false,
    },
    DefaultVariable {
        component: "TxCtrlr",
        variable: "MaxEnergyOnInvalidId",
        instance: None,
        data_type: VariableDataType::Integer,
        unit: Some("Wh"),
        value: "0",
        mutability: VariableMutability::ReadWrite,
        persistent: false,
    },
    DefaultVariable {
        component: "SampledDataCtrlr",
        variable: "TxUpdatedInterval",
        instance: None,
        data_type: VariableDataType::Integer,
        unit: Some("s"),
        value: "0",
        mutability: VariableMutability::ReadWrite,
        persistent: false,
    },
    DefaultVariable {
        component: "SampledDataCtrlr",
        variable: "TxUpdatedMeasurands",
        instance: None,
        data_type: VariableDataType::MemberList,
        unit: None,
        value: "Energy.Active.Import.Register",
        mutability: VariableMutability::ReadWrite,
        persistent: false,
    },
    DefaultVariable {
        component: "SampledDataCtrlr",
        variable: "TxEndedMeasurands",
        instance: None,
        data_type: VariableDataType::MemberList,
        unit: None,
        value: "Energy.Active.Import.Register",
        mutability: VariableMutability::ReadWrite,
        persistent: false,
    },
    DefaultVariable {
        component: "SampledDataCtrlr",
        variable: "TxEndedInterval",
        instance: None,
        data_type: VariableDataType::Integer,
        unit: Some("s"),
        value: "0",
        mutability: VariableMutability::ReadWrite,
        persistent: false,
    },
    DefaultVariable {
        component: "AlignedDataCtrlr",
        variable: "Measurands",
        instance: None,
        data_type: VariableDataType::MemberList,
        unit: None,
        value: "Energy.Active.Import.Register",
        mutability: VariableMutability::ReadWrite,
        persistent: false,
    },
    DefaultVariable {
        component: "AlignedDataCtrlr",
        variable: "TxEndedMeasurands",
        instance: None,
        data_type: VariableDataType::MemberList,
        unit: None,
        value: "Energy.Active.Import.Register",
        mutability: VariableMutability::ReadWrite,
        persistent: false,
    },
    DefaultVariable {
        component: "AlignedDataCtrlr",
        variable: "TxEndedInterval",
        instance: None,
        data_type: VariableDataType::Integer,
        unit: Some("s"),
        value: "0",
        mutability: VariableMutability::ReadWrite,
        persistent: false,
    },
    DefaultVariable {
        component: "OCPPCommCtrlr",
        variable: "MessageAttempts",
        instance: Some("TransactionEvent"),
        data_type: VariableDataType::Integer,
        unit: None,
        value: "3",
        mutability: VariableMutability::ReadWrite,
        persistent: false,
    },
    DefaultVariable {
        component: "OCPPCommCtrlr",
        variable: "MessageAttemptInterval",
        instance: Some("TransactionEvent"),
        data_type: VariableDataType::Integer,
        unit: Some("s"),
        value: "60",
        mutability: VariableMutability::ReadWrite,
        persistent: false,
    },
    DefaultVariable {
        component: "OCPPCommCtrlr",
        variable: "RetryBackOffWaitMinimum",
        instance: None,
        data_type: VariableDataType::Integer,
        unit: Some("s"),
        // `ocpp_client`'s own `ReconnectPolicy` default, so what a CSMS reads here is what the
        // connection actually does rather than a number this crate invented alongside it.
        value: "1",
        mutability: VariableMutability::ReadWrite,
        persistent: false,
    },
    DefaultVariable {
        component: "OCPPCommCtrlr",
        variable: "RetryBackOffRepeatTimes",
        instance: None,
        data_type: VariableDataType::Integer,
        unit: None,
        // How many times the wait doubles before it stops growing. The transport expresses the
        // same idea as a maximum delay, so this is converted rather than stored twice - see
        // `crate::connect`.
        value: "5",
        mutability: VariableMutability::ReadWrite,
        persistent: false,
    },
    DefaultVariable {
        component: "OCPPCommCtrlr",
        variable: "RetryBackOffRandomRange",
        instance: None,
        data_type: VariableDataType::Integer,
        unit: Some("s"),
        // `0` - no jitter unless the CSMS asks for some. It *is* applied now (A5:
        // `crate::network_switch::ConnectionTarget` adds the random part on every redial), so this
        // is a default rather than a disclaimer; OCPP names no default of its own, and a charge
        // point that spread its retries without being told to would be doing something its
        // operator did not ask for. An operator running a fleet against one CSMS wants this set.
        value: "0",
        mutability: VariableMutability::ReadWrite,
        persistent: false,
    },
    DefaultVariable {
        component: "OCPPCommCtrlr",
        variable: "WebSocketPingInterval",
        instance: None,
        data_type: VariableDataType::Integer,
        unit: Some("s"),
        value: "0",
        mutability: VariableMutability::ReadWrite,
        persistent: false,
    },
    DefaultVariable {
        component: "OCPPCommCtrlr",
        variable: "ResetRetries",
        instance: None,
        data_type: VariableDataType::Integer,
        unit: None,
        value: "1",
        mutability: VariableMutability::ReadWrite,
        persistent: false,
    },
    DefaultVariable {
        component: "ChargingStation",
        variable: "MinimumStatusDuration",
        instance: None,
        data_type: VariableDataType::Integer,
        unit: Some("s"),
        value: "0",
        mutability: VariableMutability::ReadWrite,
        persistent: false,
    },
    DefaultVariable {
        component: "OCPPCommCtrlr",
        variable: "UnlockOnEVSideDisconnect",
        instance: None,
        data_type: VariableDataType::Boolean,
        unit: None,
        value: "true",
        mutability: VariableMutability::ReadWrite,
        persistent: false,
    },
    DefaultVariable {
        component: "LocalAuthListCtrlr",
        variable: "Enabled",
        instance: None,
        data_type: VariableDataType::Boolean,
        unit: None,
        value: "true",
        mutability: VariableMutability::ReadWrite,
        persistent: false,
    },
    // --- OCPP 2.x required variables (B1.7): recorded, and honest about it ---
    //
    // Every row below is marked Required in the vendored 2.1 appendix
    // (`docs/OCPP-2.1/Appendices_CSV_v2.1/dm_components_vars.csv`) for a component whose
    // functionality this crate always has. Required means a CSMS may read it and expects an
    // answer; it does not mean this crate acts on it, and the ones it does act on are listed as
    // live above.
    DefaultVariable {
        component: "OCPPCommCtrlr",
        variable: "FileTransferProtocols",
        instance: None,
        data_type: VariableDataType::MemberList,
        unit: None,
        // Empty, and still truthfully so - but for a different reason since B5.1. File transfer
        // now exists as `hardware::FileTransfer`, and *which protocols* it speaks is a fact about
        // the integrator's implementation: OCPP hands over a bare URL and the scheme in it is
        // whatever the operator deployed. Naming "HTTP,HTTPS" here would be this crate
        // advertising something it neither implements nor can verify (C2's honesty criterion). An
        // integrator with a transfer binding should overwrite this with what theirs really
        // speaks; one without leaves it empty, which remains the truth.
        value: "",
        mutability: VariableMutability::ReadOnly,
        persistent: false,
    },
    DefaultVariable {
        component: "OCPPCommCtrlr",
        variable: "MessageTimeout",
        instance: Some("Default"),
        data_type: VariableDataType::Integer,
        unit: Some("s"),
        value: "30",
        mutability: VariableMutability::ReadWrite,
        persistent: false,
    },
    DefaultVariable {
        component: "OCPPCommCtrlr",
        variable: "NetworkConfigurationPriority",
        instance: None,
        data_type: VariableDataType::String,
        unit: None,
        // Slot 0, the connection the charge point was started on. This is a live value: a stored
        // profile joins the order and a vacated slot leaves it (see
        // `ChargePointState::refresh_network_configuration_priority`), and the first occupied slot
        // in it is the one `network_switch` connects to.
        value: "0",
        mutability: VariableMutability::ReadWrite,
        persistent: false,
    },
    DefaultVariable {
        component: "OCPPCommCtrlr",
        variable: "NetworkProfileConnectionAttempts",
        instance: None,
        data_type: VariableDataType::Integer,
        unit: None,
        value: "3",
        mutability: VariableMutability::ReadWrite,
        persistent: false,
    },
    DefaultVariable {
        component: "OCPPCommCtrlr",
        variable: "OfflineThreshold",
        instance: None,
        data_type: VariableDataType::Integer,
        unit: Some("s"),
        value: "60",
        mutability: VariableMutability::ReadWrite,
        persistent: false,
    },
    DefaultVariable {
        component: "SampledDataCtrlr",
        variable: "TxStartedMeasurands",
        instance: None,
        data_type: VariableDataType::MemberList,
        unit: None,
        value: "Energy.Active.Import.Register",
        mutability: VariableMutability::ReadWrite,
        persistent: false,
    },
    DefaultVariable {
        component: "TxCtrlr",
        variable: "TxStartPoint",
        instance: None,
        data_type: VariableDataType::MemberList,
        unit: None,
        // What this crate's state machine actually does: a transaction starts when the presented
        // token is authorized (see `advance_transaction`), not on plug-in or on energy flow.
        value: "Authorized",
        mutability: VariableMutability::ReadWrite,
        persistent: false,
    },
    DefaultVariable {
        component: "TxCtrlr",
        variable: "TxStopPoint",
        instance: None,
        data_type: VariableDataType::MemberList,
        unit: None,
        value: "EVConnected",
        mutability: VariableMutability::ReadWrite,
        persistent: false,
    },
    DefaultVariable {
        component: "ClockCtrlr",
        variable: "TimeSource",
        instance: None,
        data_type: VariableDataType::SequenceList,
        unit: None,
        // Heartbeat only: this crate learns the time from BootNotification/Heartbeat
        // `currentTime` (G3.2) and has no other source. An integrator with an RTC or NTP supplies
        // its own `Clock` but does not change where *this* crate's knowledge comes from.
        value: "Heartbeat",
        mutability: VariableMutability::ReadOnly,
        persistent: false,
    },
    DefaultVariable {
        component: "SecurityCtrlr",
        variable: "SecurityProfile",
        instance: None,
        data_type: VariableDataType::Integer,
        unit: None,
        // Profile 1 (basic auth over an unsecured connection) is what this crate can honestly
        // claim: TLS and certificate handling are workstream F, unimplemented. Reporting 2 or 3
        // would advertise security this charge point does not have.
        value: "1",
        mutability: VariableMutability::ReadOnly,
        persistent: false,
    },
    DefaultVariable {
        component: "SecurityCtrlr",
        variable: "AllowSecurityProfileDowngrade",
        instance: None,
        data_type: VariableDataType::Boolean,
        unit: None,
        // `false`: §A05 makes lowering the security profile an explicit operator opt-in, and it
        // only ever unlocks 3 -> 2 - dropping to profile 1 stays refused with this set. See
        // `crate::security_profile::SecurityProfileChange`.
        value: "false",
        mutability: VariableMutability::ReadWrite,
        persistent: true,
    },
    DefaultVariable {
        component: "SecurityCtrlr",
        variable: "OrganizationName",
        instance: None,
        data_type: VariableDataType::String,
        unit: None,
        // Empty until an integrator sets it - inventing an organization name would be worse than
        // an obviously-unset one, and this is exactly the kind of value a deployment configures.
        value: "",
        mutability: VariableMutability::ReadWrite,
        persistent: true,
    },
    DefaultVariable {
        component: "SecurityCtrlr",
        variable: "CertificateEntries",
        instance: None,
        data_type: VariableDataType::Integer,
        unit: None,
        // Zero until an integrator wires a `hardware::CertificateStore` (B4.1). The store now
        // exists; what this crate cannot do is know how many certificates *someone else's*
        // implementation holds without being handed one, so the honest default is none.
        value: "0",
        mutability: VariableMutability::ReadOnly,
        persistent: false,
    },
    DefaultVariable {
        component: "DeviceDataCtrlr",
        variable: "ItemsPerMessage",
        instance: Some("GetVariables"),
        data_type: VariableDataType::Integer,
        unit: None,
        value: "50",
        mutability: VariableMutability::ReadOnly,
        persistent: false,
    },
    DefaultVariable {
        component: "DeviceDataCtrlr",
        variable: "ItemsPerMessage",
        instance: Some("SetVariables"),
        data_type: VariableDataType::Integer,
        unit: None,
        value: "50",
        mutability: VariableMutability::ReadOnly,
        persistent: false,
    },
    DefaultVariable {
        component: "DeviceDataCtrlr",
        variable: "ItemsPerMessage",
        instance: Some("GetReport"),
        data_type: VariableDataType::Integer,
        unit: None,
        // `crate::reporting::REPORT_CHUNK_SIZE` - the real figure this crate chunks
        // `NotifyReport` at, not an aspiration.
        value: "16",
        mutability: VariableMutability::ReadOnly,
        persistent: false,
    },
    DefaultVariable {
        component: "DeviceDataCtrlr",
        variable: "BytesPerMessage",
        instance: Some("GetVariables"),
        data_type: VariableDataType::Integer,
        unit: None,
        value: "8192",
        mutability: VariableMutability::ReadOnly,
        persistent: false,
    },
    DefaultVariable {
        component: "DeviceDataCtrlr",
        variable: "BytesPerMessage",
        instance: Some("SetVariables"),
        data_type: VariableDataType::Integer,
        unit: None,
        value: "8192",
        mutability: VariableMutability::ReadOnly,
        persistent: false,
    },
    DefaultVariable {
        component: "DeviceDataCtrlr",
        variable: "BytesPerMessage",
        instance: Some("GetReport"),
        data_type: VariableDataType::Integer,
        unit: None,
        value: "8192",
        mutability: VariableMutability::ReadOnly,
        persistent: false,
    },
];

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
            DEFAULT_VARIABLES.len(),
            "every entry in DEFAULT_VARIABLES should be registered, and nothing else"
        );
    }

    #[test]
    fn registering_past_the_maximum_is_refused_and_leaves_the_model_alone() {
        // One slot above the built-in defaults, so exactly one custom registration fits.
        let mut model = DeviceModel::with_max_variables(DEFAULT_VARIABLES.len() + 1);
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
        assert_eq!(model.len(), DEFAULT_VARIABLES.len() + 1);
        assert_eq!(model.get(&component("Custom"), &variable("Second")), None);
    }

    /// Redefining an already-registered variable doesn't grow the model, so the bound must not
    /// block it - otherwise a full model could never have a value's characteristics corrected.
    #[test]
    fn redefining_an_existing_variable_is_allowed_at_the_maximum() {
        let mut model = DeviceModel::with_max_variables(DEFAULT_VARIABLES.len());

        let registered = model.register(
            component("OCPPCommCtrlr"),
            variable("HeartbeatInterval"),
            characteristics(),
            vec![attribute("120", VariableMutability::ReadWrite)],
        );

        assert!(registered);
        assert_eq!(model.len(), DEFAULT_VARIABLES.len());
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

        assert_eq!(model.max_variables(), DEFAULT_VARIABLES.len());
        assert_eq!(model.len(), DEFAULT_VARIABLES.len());
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

        // Alphabetical, defaults included - asserted as "sorted, with the two registered here in
        // the right places" rather than as a literal list, which would just be a transcription of
        // DEFAULT_VARIABLES that has to be re-transcribed whenever a block lands.
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
        assert_eq!(names.first(), Some(&"AlignedDataCtrlr"));
        assert_eq!(names.last(), Some(&"Zeta"));
        assert!(names.contains(&"Alpha"));
    }
}
