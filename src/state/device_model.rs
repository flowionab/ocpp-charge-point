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
    /// this crate's own [`DEFAULT_VARIABLES`] set it `true`; the one built-in that does is
    /// `AvailabilityState` (see [`DeviceModel::register_topology_defaults`]), which OCPP expects
    /// to carry a `Delta` monitor - G01.FR.03 onwards describe exactly that. A hardware binding
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
        Self::with_topology(max_variables, &[])
    }

    /// [`with_max_variables`](Self::with_max_variables), plus the per-EVSE and per-connector
    /// variables OCPP requires a charge point of this shape to report - see
    /// [`Self::register_topology_defaults`]. `connector_counts[evse_id]` is that EVSE's connector
    /// count, the same addressing [`crate::state::ChargePointState::new`] takes.
    ///
    /// Topology is a constructor argument rather than something registered afterwards because
    /// OCPP's `Connector`/`EVSE` components are addressed by the same `(evse_id, connector_id)`
    /// indices the rest of this state already is: a device model that didn't know the topology
    /// could not name them, and a `GetBaseReport` issued before some later registration ran would
    /// under-report the charge point.
    ///
    /// Like [`with_max_variables`](Self::with_max_variables), the bound is raised to whatever the
    /// defaults *and* these occupy if `max_variables` is smaller - a limit that dropped the
    /// components OCPP requires would bound nothing worth bounding.
    pub fn with_topology(max_variables: usize, connector_counts: &[usize]) -> Self {
        let mut model = Self {
            components: BTreeMap::new(),
            max_variables: usize::MAX,
        };
        model.register_defaults();
        model.register_topology_defaults(connector_counts);
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

    /// Removes every variable registered on `component`, returning whether anything was there.
    ///
    /// Needed because some components are not facts about the firmware but about its current
    /// configuration: a `NetworkConfiguration` instance exists only while its configuration slot
    /// is occupied (CV1.3), so vacating the slot has to take the component with it. Leaving it
    /// behind would have `GetVariables` and `GetBaseReport` reporting a CSMS URL the charge point
    /// no longer holds - worse than not reporting it, because it reads as current.
    pub fn remove_component(&mut self, component: &Component) -> bool {
        self.components.remove(component).is_some()
    }

    /// Every registered component, in the order [`Self::iter`] would visit them.
    pub fn components(&self) -> impl Iterator<Item = &Component> {
        self.components.keys()
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

    /// Registers the availability variables OCPP requires on every `ChargingStation`, `EVSE` and
    /// `Connector` component a charge point of this topology owns
    /// (`docs/OCPP-2.1-COMPLIANCE-ROADMAP.md` CV1.1). `connector_counts[evse_id]` is that EVSE's
    /// connector count.
    ///
    /// Two variables per component, and they mean different things despite the similar names:
    ///
    /// - **`Available`** (`Boolean`, constant `true`) is OCPP's *"Component exists"* - a fact about
    ///   the charge point's construction, not its current state. Registering it `false` would be
    ///   the way to say a component is fitted but not wired; a component this crate knows about at
    ///   all is, by construction, one that exists.
    /// - **`AvailabilityState`** (`OptionList`, `ReadOnly`) is the live one: the same value the
    ///   connector's `StatusNotification` carries, rolled up per EVSE and per charge point by
    ///   [`crate::state::ChargePointState`]. `ReadOnly` because it is a projection of the state
    ///   machine - a CSMS write would be overwritten by the next transition, so accepting one
    ///   would be exactly the kind of silent lie B05.FR.09 exists to prevent.
    ///
    /// The station starts `Unavailable`: a charge point that has not completed its
    /// BootNotification is not available, and `ChargePointState::apply` moves it on from there.
    /// EVSEs and connectors start `Available`, matching the state
    /// [`EvseState::new`](crate::state::EvseState::new) gives them.
    ///
    /// B07.FR.09 is what makes these load-bearing rather than decorative: `GetBaseReport`'s
    /// `SummaryInventory` base is defined as the `AvailabilityState` of the charge point, of each
    /// EVSE, and of each connector - so without these registered, `crate::reporting`'s summary
    /// base is structurally empty however healthy the charge point is.
    fn register_topology_defaults(&mut self, connector_counts: &[usize]) {
        self.register_availability_pair(
            AVAILABILITY_COMPONENT_CHARGE_POINT,
            None,
            AVAILABILITY_STATE_UNAVAILABLE,
        );
        for (evse_id, &connector_count) in connector_counts.iter().enumerate() {
            self.register_availability_pair(
                AVAILABILITY_COMPONENT_EVSE,
                Some((evse_id, None)),
                AVAILABILITY_STATE_AVAILABLE,
            );
            for connector_id in 0..connector_count {
                self.register_availability_pair(
                    AVAILABILITY_COMPONENT_CONNECTOR,
                    Some((evse_id, Some(connector_id))),
                    AVAILABILITY_STATE_AVAILABLE,
                );
                self.register_plug_retention_lock(evse_id, connector_id);
            }
        }
    }

    /// Registers one connector's `ConnectorPlugRetentionLock`/`Problem` variable - **OCPP use
    /// case G05, "Lock Failure"** (`docs/OCPP-2.1-COMPLIANCE-ROADMAP.md` CV11).
    ///
    /// G05 names this exact component and variable as the payload of the `NotifyEvent` a lock
    /// failure produces, and its prerequisites require the component to be *in the device model* -
    /// so a charge point that only faulted the connector would leave the CSMS unable to tell a
    /// lock failure from a stuck contactor. `ReadOnly` for the reason `AvailabilityState` is: it
    /// is a projection of what the hardware reported, and a CSMS write would be overwritten by the
    /// next lock attempt.
    ///
    /// Registered per connector, and not for the EVSE or the station: the lock is a property of
    /// one physical socket. `false` on a fresh model, which is the honest reading before any lock
    /// has been asked to do anything.
    fn register_plug_retention_lock(&mut self, evse_id: usize, connector_id: usize) {
        self.register(
            Component {
                name: PLUG_RETENTION_LOCK_COMPONENT.into(),
                instance: None,
                evse: Some((evse_id, Some(connector_id))),
            },
            Variable {
                name: PROBLEM_VARIABLE.into(),
                instance: None,
            },
            VariableCharacteristics {
                data_type: VariableDataType::Boolean,
                unit: None,
                min_limit: None,
                max_limit: None,
                values_list: None,
                // A CSMS may configure its own monitor on top of the hard-wired notification this
                // crate already raises - G05's prerequisites assume it can.
                supports_monitoring: true,
            },
            vec![VariableAttribute {
                attribute_type: VariableAttributeType::Actual,
                value: "false".into(),
                mutability: VariableMutability::ReadOnly,
                // Re-derived from the connector's own state on the first event after a reboot, so
                // a persisted `true` would only report a lock failure that may already be gone.
                persistent: false,
                constant: false,
                requires_reboot: false,
            }],
        );
    }

    /// Registers one component's `AvailabilityState`/`Available` pair - see
    /// [`Self::register_topology_defaults`] for what each means.
    fn register_availability_pair(
        &mut self,
        component_name: &str,
        evse: Option<(usize, Option<usize>)>,
        initial_state: &str,
    ) {
        let component = Component {
            name: component_name.into(),
            instance: None,
            evse,
        };
        self.register(
            component.clone(),
            Variable {
                name: AVAILABILITY_STATE_VARIABLE.into(),
                instance: None,
            },
            VariableCharacteristics {
                data_type: VariableDataType::OptionList,
                unit: None,
                min_limit: None,
                max_limit: None,
                // The value set OCPP's `ConnectorStatusEnumType` defines, which is what
                // `AvailabilityState` reports at every level (G01.FR.01). Declared here rather
                // than left `None` so that CV3's `SetVariables` validation - and any CSMS reading
                // the characteristics - has the real option list to work from.
                values_list: Some(
                    AVAILABILITY_STATE_VALUES
                        .iter()
                        .map(|value| String::from(*value))
                        .collect(),
                ),
                supports_monitoring: true,
            },
            vec![VariableAttribute {
                attribute_type: VariableAttributeType::Actual,
                value: initial_state.into(),
                mutability: VariableMutability::ReadOnly,
                // Recomputed from the state machine on the first event after a reboot, so
                // persisting it would only risk reporting a stale value in the window before
                // that. The *underlying* state that must survive a reboot (an EVSE left
                // Unavailable by `ChangeAvailability`, B01.FR.07/G01.FR.02) is persisted by
                // `crate::persistence`, not here.
                persistent: false,
                constant: false,
                requires_reboot: false,
            }],
        );
        self.register(
            component,
            Variable {
                name: AVAILABILITY_EXISTS_VARIABLE.into(),
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
                value: "true".into(),
                mutability: VariableMutability::ReadOnly,
                persistent: false,
                constant: true,
                requires_reboot: false,
            }],
        );
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
    ///   `crate::remote_control` either (CV2.1) - registered so it is readable, not because it is
    ///   honoured.
    ///
    /// The per-EVSE and per-connector variables are **not** here: they depend on the topology, so
    /// they live in [`Self::register_topology_defaults`] instead.
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
                default_characteristics(default),
                vec![VariableAttribute {
                    attribute_type: VariableAttributeType::Actual,
                    value: default.value.into(),
                    // CV2.1: a variable this build does not act on is registered read-only, so a
                    // `SetVariables` on it is `Rejected` (B05.FR.09) rather than accepted and
                    // ignored. See `DefaultVariable::honoured`.
                    mutability: if default.honoured {
                        default.mutability
                    } else {
                        VariableMutability::ReadOnly
                    },
                    persistent: default.persistent,
                    constant: false,
                    requires_reboot: false,
                }],
            );
        }
    }
}

/// The [`VariableCharacteristics`] one [`DefaultVariable`] is registered with, including any
/// bounds [`VARIABLE_BOUNDS`] declares for it.
fn default_characteristics(default: &DefaultVariable) -> VariableCharacteristics {
    let bounds = VARIABLE_BOUNDS.iter().find(|bounds| {
        bounds.component == default.component
            && bounds.variable == default.variable
            && bounds.instance == default.instance
    });
    VariableCharacteristics {
        data_type: default.data_type,
        unit: default.unit.map(Into::into),
        min_limit: bounds.and_then(|bounds| bounds.min),
        max_limit: bounds.and_then(|bounds| bounds.max),
        values_list: bounds.and_then(|bounds| {
            bounds
                .values
                .map(|values| values.iter().map(|value| String::from(*value)).collect())
        }),
        supports_monitoring: false,
    }
}

/// A bound OCPP defines on one [`DEFAULT_VARIABLES`] entry, so that `SetVariables` can enforce it
/// (`docs/OCPP-2.1-COMPLIANCE-ROADMAP.md` CV3, B05.FR.08).
///
/// A separate table rather than three more fields on [`DefaultVariable`] because only a minority
/// of variables have a bound OCPP actually states, and threading `None, None, None` through every
/// other row would bury the ones that matter.
///
/// **The bar for a row here is that OCPP states the bound, not that it seems sensible.** A limit
/// this crate invented would be a `Rejected` a CSMS has no way to have predicted - worse than no
/// limit, because `GetVariables` would report the value it just refused to set.
pub(crate) struct VariableBounds {
    /// The 2.x component name, matching a [`DEFAULT_VARIABLES`] row.
    pub component: &'static str,
    /// The 2.x variable name.
    pub variable: &'static str,
    /// The variable instance, matching the same row.
    pub instance: Option<&'static str>,
    /// The minimum value, for numeric types.
    pub min: Option<f64>,
    /// The maximum value; for string-shaped types OCPP reads this as a maximum *length*.
    pub max: Option<f64>,
    /// The allowed values, for `OptionList`/`MemberList`/`SequenceList` types.
    pub values: Option<&'static [&'static str]>,
}

/// The measurands this firmware actually produces - the five fields of
/// [`MeterSample`](crate::state::MeterSample), and so the only values the measurand variables
/// accept (CV2.6).
///
/// Narrower than OCPP's `MeasurandEnumType` for the same reason `TX_START_STOP_POINTS` is narrower
/// than its enum: accepting a measurand this crate cannot sample and then not sending it is the
/// silent lie B05.FR.09 forbids.
const SUPPORTED_MEASURANDS: &[&str] = &[
    "Energy.Active.Import.Register",
    "Power.Active.Import",
    "Current.Import",
    "Voltage",
    "SoC",
];

/// Every bound this crate can state on a built-in default - see [`VariableBounds`].
pub(crate) const VARIABLE_BOUNDS: &[VariableBounds] = &[
    // Intervals: seconds, and never negative. `0` is meaningful for all of these (OCPP's "off"
    // for `AlignedDataCtrlr.Interval`, "unlimited"/"unset" elsewhere - see each variable's docs),
    // so the floor is 0 and not 1.
    VariableBounds {
        component: "OCPPCommCtrlr",
        variable: "HeartbeatInterval",
        instance: None,
        min: Some(0.0),
        max: None,
        values: None,
    },
    VariableBounds {
        component: "OCPPCommCtrlr",
        variable: "OfflineThreshold",
        instance: None,
        min: Some(0.0),
        max: None,
        values: None,
    },
    VariableBounds {
        component: "OCPPCommCtrlr",
        variable: "MessageTimeout",
        instance: Some("Default"),
        min: Some(0.0),
        max: None,
        values: None,
    },
    VariableBounds {
        component: "OCPPCommCtrlr",
        variable: "MessageAttemptInterval",
        instance: Some("TransactionEvent"),
        min: Some(0.0),
        max: None,
        values: None,
    },
    VariableBounds {
        component: "OCPPCommCtrlr",
        variable: "MessageAttempts",
        instance: Some("TransactionEvent"),
        min: Some(0.0),
        max: None,
        values: None,
    },
    VariableBounds {
        component: "AlignedDataCtrlr",
        variable: "Interval",
        instance: None,
        min: Some(0.0),
        max: None,
        values: None,
    },
    VariableBounds {
        component: "AuthCacheCtrlr",
        variable: "LifeTime",
        instance: None,
        min: Some(0.0),
        max: None,
        values: None,
    },
    VariableBounds {
        component: "ChargingStation",
        variable: "MinimumStatusDuration",
        instance: None,
        min: Some(0.0),
        max: None,
        values: None,
    },
    VariableBounds {
        component: "TxCtrlr",
        variable: "EVConnectionTimeOut",
        instance: None,
        min: Some(0.0),
        max: None,
        values: None,
    },
    // The two transaction points are `MemberList`s over OCPP's `TxStartStopPointEnumType`. Their
    // value sets are the reason a `SetVariables` naming a point this crate does not implement is
    // *rejected* rather than stored (CV2.2) - see `TX_START_STOP_POINTS`.
    VariableBounds {
        component: "SampledDataCtrlr",
        variable: "TxStartedMeasurands",
        instance: None,
        min: None,
        max: None,
        values: Some(SUPPORTED_MEASURANDS),
    },
    VariableBounds {
        component: "SampledDataCtrlr",
        variable: "TxUpdatedMeasurands",
        instance: None,
        min: None,
        max: None,
        values: Some(SUPPORTED_MEASURANDS),
    },
    VariableBounds {
        component: "SampledDataCtrlr",
        variable: "TxEndedMeasurands",
        instance: None,
        min: None,
        max: None,
        values: Some(SUPPORTED_MEASURANDS),
    },
    VariableBounds {
        component: "AlignedDataCtrlr",
        variable: "Measurands",
        instance: None,
        min: None,
        max: None,
        values: Some(SUPPORTED_MEASURANDS),
    },
    VariableBounds {
        component: "AlignedDataCtrlr",
        variable: "TxEndedMeasurands",
        instance: None,
        min: None,
        max: None,
        values: Some(SUPPORTED_MEASURANDS),
    },
    VariableBounds {
        component: "TxCtrlr",
        variable: "TxStartPoint",
        instance: None,
        min: None,
        max: None,
        values: Some(TX_START_STOP_POINTS),
    },
    VariableBounds {
        component: "TxCtrlr",
        variable: "TxStopPoint",
        instance: None,
        min: None,
        max: None,
        values: Some(TX_START_STOP_POINTS),
    },
];

/// The subset of OCPP's `TxStartStopPointEnumType` this charge point can actually observe, and so
/// the only values `TxCtrlr.TxStartPoint` and `TxCtrlr.TxStopPoint` accept (CV2.2).
///
/// Shared by both variables because the answer is the same for both: a point this charge point
/// cannot see *begin* is equally one it cannot see *cease*.
///
/// **Deliberately narrower than the enum OCPP defines.** `ParkingBayOccupancy` needs a bay sensor
/// this crate has no binding for, `DataSigned` needs signed meter values it does not produce, and
/// `EnergyTransfer` needs a "current is actually flowing" signal distinct from the contactor being
/// closed. Declaring them would mean accepting a `SetVariables` and then starting transactions at
/// some other point - the silent-lie failure mode B05.FR.09 forbids. Declaring only these three
/// makes CV3's validation reject the rest with a reason, and `VariableCharacteristics::values_list`
/// is exactly where a charge point is supposed to say what it accepts.
const TX_START_STOP_POINTS: &[&str] = &["EVConnected", "Authorized", "PowerPathClosed"];

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

// --- CV1.1: the availability variables, named once ---
//
// Kept as constants rather than string literals at each use site because three different modules
// address the same `(Component, Variable)` pairs - `DeviceModel::register_topology_defaults`
// registers them, `ChargePointState` keeps them in step with the state machine, and
// `crate::reporting`'s `SummaryInventory` looks them up by name (B07.FR.09). A typo in any one of
// those would produce a report that is silently empty rather than an error.

/// OCPP's component name for the charge point as a whole.
pub(crate) const AVAILABILITY_COMPONENT_CHARGE_POINT: &str = "ChargingStation";
/// OCPP's component name for one EVSE.
pub(crate) const AVAILABILITY_COMPONENT_EVSE: &str = "EVSE";
/// OCPP's component name for one connector.
pub(crate) const AVAILABILITY_COMPONENT_CONNECTOR: &str = "Connector";

/// OCPP's component name for a connector's plug retention lock (G05, CV11) - see
/// [`DeviceModel::register_plug_retention_lock`].
pub(crate) const PLUG_RETENTION_LOCK_COMPONENT: &str = "ConnectorPlugRetentionLock";
/// OCPP's variable name for a component reporting that something is wrong with it (G05, CV11).
pub(crate) const PROBLEM_VARIABLE: &str = "Problem";
/// The live availability variable, present on all three components above.
pub(crate) const AVAILABILITY_STATE_VARIABLE: &str = "AvailabilityState";
/// The "this component exists" variable (OCPP's own wording), present on all three.
pub(crate) const AVAILABILITY_EXISTS_VARIABLE: &str = "Available";
/// `AvailabilityState`'s `Available` value.
pub(crate) const AVAILABILITY_STATE_AVAILABLE: &str = "Available";
/// `AvailabilityState`'s `Unavailable` value.
pub(crate) const AVAILABILITY_STATE_UNAVAILABLE: &str = "Unavailable";
/// How many variables [`DeviceModel::register_topology_defaults`] registers per component - the
/// `AvailabilityState`/`Available` pair. Only the bound-sizing tests count them; nothing in the
/// crate's own paths needs the number.
#[cfg(test)]
pub(crate) const AVAILABILITY_VARIABLES_PER_COMPONENT: usize = 2;
/// How many variables [`DeviceModel::register_topology_defaults`] registers per *connector* on
/// top of that pair - the `ConnectorPlugRetentionLock`/`Problem` variable (CV11). Same caveat:
/// only the bound-sizing tests count them.
#[cfg(test)]
pub(crate) const PLUG_RETENTION_LOCK_VARIABLES_PER_CONNECTOR: usize = 1;
/// How many availability variables a charge point with no EVSEs at all still has: the
/// `ChargingStation` component's own pair.
#[cfg(test)]
pub(crate) const STATION_AVAILABILITY_VARIABLES: usize = AVAILABILITY_VARIABLES_PER_COMPONENT;

/// OCPP's component name for one network configuration slot. The slot number is the component
/// *instance* - see `ChargePointState::sync_network_configuration_variables`.
pub(crate) const NETWORK_CONFIGURATION_COMPONENT: &str = "NetworkConfiguration";
/// OCPP's component name for the clock.
pub(crate) const CLOCK_COMPONENT: &str = "ClockCtrlr";
/// The charge point's current date and time (CV1.2) - see [`DEFAULT_VARIABLES`]' entry for it.
pub(crate) const CLOCK_DATE_TIME_VARIABLE: &str = "DateTime";

/// Every value `AvailabilityState` may take - OCPP's `ConnectorStatusEnumType` (G01.FR.01).
pub(crate) const AVAILABILITY_STATE_VALUES: &[&str] = &[
    "Available",
    "Occupied",
    "Reserved",
    "Unavailable",
    "Faulted",
];

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
    /// What OCPP says about writing this variable - **not** necessarily what a CSMS gets. See
    /// [`Self::honoured`], which can narrow it.
    pub mutability: VariableMutability,
    /// Whether this crate actually *acts* on the value
    /// (`docs/OCPP-2.1-COMPLIANCE-ROADMAP.md` CV2.1).
    ///
    /// **`false` forces the registration to `ReadOnly`, whatever [`Self::mutability`] says.** OCPP
    /// B05.FR.09 requires a `SetVariables` the charge point cannot honour to be `Rejected`, and
    /// silently accepting a setting that changes nothing is the worst of the three options
    /// available: worse than rejecting it (which tells the CSMS the truth) and worse than not
    /// registering the variable at all (which at least answers `UnknownVariable`). An operator who
    /// sets `StopTxOnEVSideDisconnect` and sees `Accepted` will believe cable removal now suspends
    /// rather than stops the transaction, and will find out otherwise from a driver.
    ///
    /// The two fields are kept separate rather than collapsed into one so the table records
    /// *both* facts: what OCPP intends, and what this build delivers. Making a variable live is
    /// then a one-word change here plus the read that justifies it - and the roadmap row that
    /// tracks it says which.
    pub honoured: bool,
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
        honoured: true,
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
        honoured: true,
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
        honoured: true,
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
        honoured: true,
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
        honoured: true,
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
        honoured: false,
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
        honoured: false,
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
        // CV2.9: read by `crate::authorization::offline_decision` (C15).
        honoured: true,
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
        // CV2.3: read by `crate::remote_control::run_pending_remote_start_timeouts`
        // (F02.FR.07/.08).
        honoured: true,
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
        // CV2.4: read by `ChargePointState::apply_connector_event` (E09 vs E10).
        honoured: true,
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
        // CV2.5: read into `ConnectorPolicy` and honoured on E05.
        honoured: true,
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
        // CV2.5: read into `ConnectorPolicy` and honoured on E05.
        honoured: true,
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
        honoured: false,
        persistent: false,
    },
    DefaultVariable {
        component: "SampledDataCtrlr",
        variable: "TxUpdatedMeasurands",
        instance: None,
        data_type: VariableDataType::MemberList,
        unit: None,
        value: "Energy.Active.Import.Register,Power.Active.Import",
        mutability: VariableMutability::ReadWrite,
        // CV2.6: read per `TransactionEvent(Updated)` by `transaction_event_measurands`.
        honoured: true,
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
        // CV2.6: read per `TransactionEvent(Ended)` by `transaction_event_measurands`.
        honoured: true,
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
        honoured: false,
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
        // CV2.6: read per standalone `MeterValues` by `aligned_measurands`.
        honoured: true,
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
        honoured: false,
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
        honoured: false,
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
        honoured: true,
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
        honoured: true,
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
        honoured: true,
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
        honoured: true,
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
        honoured: true,
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
        honoured: true,
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
        honoured: false,
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
        // CV2.7: read per status change by `MinimumStatusDurationNotifier` (G01).
        honoured: true,
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
        // CV2.4: read into `ConnectorPolicy::unlock_on_ev_side_disconnect` and honoured by
        // `ChargePointState::apply_connector_event` (E09.FR.02 vs E09.FR.03).
        honoured: true,
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
        honoured: false,
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
        honoured: false,
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
        honoured: true,
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
        honoured: true,
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
        honoured: true,
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
        honoured: true,
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
        // CV2.6: read per `TransactionEvent(Started)` by `transaction_event_measurands`.
        honoured: true,
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
        // CV2.2: read into `ConnectorPolicy::tx_start_point` and honoured by
        // `advance_transaction`.
        honoured: true,
        persistent: false,
    },
    DefaultVariable {
        component: "TxCtrlr",
        variable: "TxStopPoint",
        instance: None,
        data_type: VariableDataType::MemberList,
        unit: None,
        // What this crate's state machine actually does: the transaction ends when the contactor
        // confirms open (see `ends_transaction`), which is what it did before the variable was
        // honoured at all. Registering the point the firmware really uses is what lets an empty or
        // unparseable value fall back honestly.
        value: "PowerPathClosed",
        mutability: VariableMutability::ReadWrite,
        // CV2.2: read into `ConnectorPolicy::tx_stop_point` and honoured by `advance_transaction`.
        honoured: true,
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
        honoured: false,
        persistent: false,
    },
    // CV1.2. Required, and live: refreshed from the CSMS's `currentTime` on every
    // BootNotification/Heartbeat response (see `ChargePointState::apply`'s `TimeSynced` arm), so
    // it reports the charge point's own notion of the time rather than a fixed string.
    //
    // `ReadOnly`, though OCPP allows a CSMS to write it to set the station clock. This crate has
    // no settable clock: its notion of time *is* the last `currentTime` it was told (see
    // `TimeSource` above), so a write would be overwritten by the next heartbeat. B05.FR.09 says
    // reject what cannot be honoured, and `ReadOnly` is how that is said here - accepting the
    // write and ignoring it is exactly the failure mode CV2.1 exists to remove.
    //
    // Empty until the first sync, which is the truth for a charge point that has not yet spoken
    // to a CSMS and has no RTC. It is *not* a valid `dateTime`, and deliberately so: a plausible
    // but wrong timestamp is worse than an obviously absent one on a device whose logs are the
    // only diagnostic instrument anyone has.
    DefaultVariable {
        component: "ClockCtrlr",
        variable: "DateTime",
        instance: None,
        data_type: VariableDataType::DateTime,
        unit: None,
        value: "",
        mutability: VariableMutability::ReadOnly,
        honoured: true,
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
        honoured: true,
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
        honoured: true,
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
        // CV2.10: read by `crate::certificates::organization_name` when building a CSR.
        honoured: true,
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
        honoured: false,
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
        honoured: true,
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
        honoured: true,
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
        honoured: true,
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
        honoured: true,
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
        honoured: true,
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
        honoured: true,
        persistent: false,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    /// How many variables a `DeviceModel::new()`/`with_max_variables` model starts with: the
    /// defaults plus the charge-point-level availability pair every topology gets (CV1.1).
    const FRESH_MODEL_VARIABLES: usize = DEFAULT_VARIABLES.len() + STATION_AVAILABILITY_VARIABLES;

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
            DEFAULT_VARIABLES.len() + STATION_AVAILABILITY_VARIABLES,
            "every entry in DEFAULT_VARIABLES, plus the charge-point-level availability pair \
             every topology gets (CV1.1), and nothing else"
        );
    }

    #[test]
    fn registering_past_the_maximum_is_refused_and_leaves_the_model_alone() {
        // One slot above the built-in defaults, so exactly one custom registration fits.
        let mut model = DeviceModel::with_max_variables(FRESH_MODEL_VARIABLES + 1);
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
        assert_eq!(model.len(), FRESH_MODEL_VARIABLES + 1);
        assert_eq!(model.get(&component("Custom"), &variable("Second")), None);
    }

    /// Redefining an already-registered variable doesn't grow the model, so the bound must not
    /// block it - otherwise a full model could never have a value's characteristics corrected.
    #[test]
    fn redefining_an_existing_variable_is_allowed_at_the_maximum() {
        let mut model = DeviceModel::with_max_variables(FRESH_MODEL_VARIABLES);

        let registered = model.register(
            component("OCPPCommCtrlr"),
            variable("HeartbeatInterval"),
            characteristics(),
            vec![attribute("120", VariableMutability::ReadWrite)],
        );

        assert!(registered);
        assert_eq!(model.len(), FRESH_MODEL_VARIABLES);
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

        assert_eq!(model.max_variables(), FRESH_MODEL_VARIABLES);
        assert_eq!(model.len(), FRESH_MODEL_VARIABLES);
    }

    /// The same guarantee as above, for the topology variables: a caller who bounds the model
    /// below what its own EVSEs and connectors need must still get a model that can name them,
    /// because a `GetBaseReport` that omits a connector is worse than an unbounded model.
    #[test]
    fn a_maximum_below_the_topology_variables_is_raised_to_fit_them_too() {
        let model = DeviceModel::with_topology(0, &[2, 1]);

        // Three connectors and two EVSEs, each an availability pair, plus a retention-lock
        // variable per connector, on top of a fresh model.
        let expected = FRESH_MODEL_VARIABLES
            + AVAILABILITY_VARIABLES_PER_COMPONENT * 5
            + PLUG_RETENTION_LOCK_VARIABLES_PER_CONNECTOR * 3;
        assert_eq!(model.len(), expected);
        assert_eq!(model.max_variables(), expected);
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

    // --- CV1.1: availability variables (docs/OCPP-2.1-COMPLIANCE-ROADMAP.md) ---

    fn scoped(name: &str, evse: Option<(usize, Option<usize>)>) -> Component {
        Component {
            name: name.into(),
            instance: None,
            evse,
        }
    }

    fn actual_value(model: &DeviceModel, component: &Component, name: &str) -> Option<String> {
        model
            .get(component, &variable(name))?
            .attribute(VariableAttributeType::Actual)
            .map(|attribute| attribute.value.clone())
    }

    #[test]
    fn a_topology_registers_the_availability_variables_ocpp_requires_for_each_level() {
        // Two EVSEs, the first with two connectors - so the per-EVSE and per-connector scoping
        // are both exercised rather than collapsing onto one another.
        let model =
            DeviceModel::with_topology(crate::state::DEFAULT_MAX_DEVICE_MODEL_VARIABLES, &[2, 1]);

        let station = scoped("ChargingStation", None);
        assert_eq!(
            actual_value(&model, &station, "AvailabilityState").as_deref(),
            Some("Unavailable"),
            "a charge point that has not booted is not yet available"
        );
        assert_eq!(
            actual_value(&model, &station, "Available").as_deref(),
            Some("true"),
            "`Available` is OCPP's \"this component exists\", not its current availability"
        );

        for evse_id in 0..2 {
            let evse = scoped("EVSE", Some((evse_id, None)));
            assert_eq!(
                actual_value(&model, &evse, "AvailabilityState").as_deref(),
                Some("Available"),
                "EVSE {evse_id}"
            );
            assert_eq!(
                actual_value(&model, &evse, "Available").as_deref(),
                Some("true"),
                "EVSE {evse_id}"
            );
        }

        for (evse_id, connector_id) in [(0, 0), (0, 1), (1, 0)] {
            let connector = scoped("Connector", Some((evse_id, Some(connector_id))));
            assert_eq!(
                actual_value(&model, &connector, "AvailabilityState").as_deref(),
                Some("Available"),
                "connector {evse_id}/{connector_id}"
            );
            assert_eq!(
                actual_value(&model, &connector, "Available").as_deref(),
                Some("true"),
                "connector {evse_id}/{connector_id}"
            );
        }

        // The connector that doesn't exist under this topology must not be registered.
        assert!(
            model
                .get(
                    &scoped("Connector", Some((1, Some(1)))),
                    &variable("AvailabilityState")
                )
                .is_none()
        );
    }

    #[test]
    fn availability_state_is_readonly_and_available_is_constant() {
        let model =
            DeviceModel::with_topology(crate::state::DEFAULT_MAX_DEVICE_MODEL_VARIABLES, &[1]);
        let station = scoped("ChargingStation", None);

        let state = model
            .get(&station, &variable("AvailabilityState"))
            .and_then(|definition| definition.attribute(VariableAttributeType::Actual))
            .expect("registered");
        // Writable would be a lie: it is a projection of the state machine, and a CSMS write
        // would be overwritten by the next transition.
        assert_eq!(state.mutability, VariableMutability::ReadOnly);
        assert!(!state.constant, "it changes as the charge point runs");

        let available = model
            .get(&station, &variable("Available"))
            .and_then(|definition| definition.attribute(VariableAttributeType::Actual))
            .expect("registered");
        assert_eq!(available.mutability, VariableMutability::ReadOnly);
        assert!(available.constant, "a component's existence never changes");
    }

    #[test]
    fn a_charge_point_with_no_evses_still_registers_the_station_level_variables() {
        let model =
            DeviceModel::with_topology(crate::state::DEFAULT_MAX_DEVICE_MODEL_VARIABLES, &[]);

        assert!(
            actual_value(
                &model,
                &scoped("ChargingStation", None),
                "AvailabilityState"
            )
            .is_some()
        );
        assert!(
            model
                .get(&scoped("EVSE", Some((0, None))), &variable("Available"))
                .is_none()
        );
    }
}
