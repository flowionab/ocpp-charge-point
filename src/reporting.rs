//! Provisioning's device-model *reporting* functional block: CSMS-initiated `GetBaseReport`/
//! `GetReport`, answered via one or more `NotifyReport` messages. See `docs/ROADMAP.md` §2.
//!
//! This is the read side of the Component/Variable device model that `crate::device_model`
//! already covers for `GetVariables`/`SetVariables` (a batch of independently-resolved single
//! items, with no wire-size concerns). `GetBaseReport`/`GetReport` are different in kind: each
//! asks this charge point to describe (a subset of) its *entire* device model, which can be
//! arbitrarily large, so OCPP splits the answer across a *sequence* of `NotifyReport` messages
//! (`requestId`/`seqNo`/`tbc`) rather than a single response - see [`chunk_report`]. This module's
//! core ([`ReportBase`], [`ReportComponentCriterion`], [`handle_get_base_report`],
//! [`handle_get_report`], [`chunk_report`]) is entirely protocol-agnostic and unit-testable
//! without any wire types; the `ocpp_2_1`/`ocpp_2_0_1` submodules translate it to/from OCPP's wire
//! shapes.
//!
//! **OCPP 1.6J: not applicable.** 1.6J has no Component/Variable device model and no
//! `GetBaseReport`/`GetReport`/`NotifyReport` messages at all - its flat `GetConfiguration` already
//! covers the equivalent ground (dumping every configuration key at once in a single response, no
//! chunking/report-base/criteria concept) and is handled entirely by
//! `crate::device_model`'s own `ocpp_1_6` submodule. There is nothing for this module to project onto for that
//! version.

use crate::actor::ChargePointActor;
use crate::state::{
    Component, DeviceModel, Variable, VariableAttribute, VariableAttributeType,
    VariableCharacteristics, VariableDefinition, VariableMutability,
};
use alloc::boxed::Box;
use alloc::vec::Vec;

#[cfg(feature = "ocpp_2_0_1")]
pub use self::ocpp_2_0_1::Ocpp2_0_1ReportHandler;
#[cfg(feature = "ocpp_2_1")]
pub use self::ocpp_2_1::Ocpp2_1ReportHandler;

/// Which of OCPP's three report bases to generate for `GetBaseReport`, matching OCPP's
/// `ReportBaseEnum`. See [`handle_get_base_report`] for exactly what each includes in terms of
/// this crate's [`DeviceModel`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportBase {
    /// Every component/variable that has at least one non-`ReadOnly` attribute - i.e. everything
    /// an operator could change via `SetVariables`. Per OCPP: "all component-variables that can
    /// be set by the operator".
    ConfigurationInventory,
    /// Every registered component/variable, regardless of mutability.
    FullInventory,
    /// A best-effort summary of components/variables relating to the charge point's current
    /// availability/condition. OCPP defines this as components' `AvailabilityState` plus, for any
    /// component in an abnormal state, its diagnostic (`Active`/`Problem`/...) variables - this
    /// crate's device model has no general notion of "abnormal state" to drive that
    /// automatically, so this is deliberately simplified to every registered variable whose name
    /// is one of a small, fixed set of well-known status/control variable names (see
    /// `SUMMARY_VARIABLE_NAMES`). A hardware binding that registers those variables (e.g.
    /// `Available`/`Enabled`/`Problem` on its own components) gets a meaningful summary report; one
    /// that doesn't gets an empty, but still `Accepted`, one - see [`handle_get_base_report`]'s
    /// docs on why an empty base report is not `EmptyResultSet`.
    SummaryInventory,
}

/// The variable names [`ReportBase::SummaryInventory`] includes, if registered on a component -
/// see that variant's docs for why this is a fixed, deliberately non-exhaustive list rather than
/// something this crate can derive automatically from the device model alone.
const SUMMARY_VARIABLE_NAMES: [&str; 5] = [
    "AvailabilityState",
    "Active",
    "Available",
    "Enabled",
    "Problem",
];

/// One component control-state criterion for `GetReport`'s `componentCriteria`, matching OCPP's
/// `ComponentCriterionEnum`. Each criterion refers to a same-named `Boolean` variable on the
/// *same* component (e.g. a component matches [`Active`](Self::Active) if it has a variable
/// named `"Active"` with an `Actual` value of `"true"`) - see each variant's own docs for
/// the exact matching rules, which differ subtly per variant (per OCPP's own B08 requirements
/// table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportComponentCriterion {
    /// Matches a component whose `Active` variable is `"true"`, *or that has no `Active` variable
    /// registered at all*.
    Active,
    /// Matches a component whose `Available` variable is `"true"`, *or that has no `Available`
    /// variable registered at all*.
    Available,
    /// Matches a component whose `Enabled` variable is `"true"`, *or that has no `Enabled`
    /// variable registered at all*.
    Enabled,
    /// Matches a component whose `Problem` variable is `"true"` - unlike the other three
    /// criteria, a component with no `Problem` variable registered does **not** match.
    Problem,
}

impl ReportComponentCriterion {
    /// The name of the same-component variable this criterion checks.
    fn variable_name(self) -> &'static str {
        match self {
            Self::Active => "Active",
            Self::Available => "Available",
            Self::Enabled => "Enabled",
            Self::Problem => "Problem",
        }
    }
}

/// One entry in a `GetReport`'s `componentVariable` list: a component, and optionally one
/// specific variable on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportComponentVariable {
    /// The component to match.
    pub component: Component,
    /// The specific variable to match, or `None` to match every variable registered on
    /// `component`.
    pub variable: Option<Variable>,
}

/// One reported component/variable, carrying everything a `NotifyReport`'s `ReportData` needs:
/// its full characteristics and current attribute(s). Protocol-agnostic - the `ocpp_2_1`/
/// `ocpp_2_0_1` submodules translate this to wire `ReportData`.
#[derive(Debug, Clone, PartialEq)]
pub struct ReportEntry {
    /// The component this variable belongs to.
    pub component: Component,
    /// The reported variable.
    pub variable: Variable,
    /// The variable's fixed characteristics.
    pub characteristics: VariableCharacteristics,
    /// The variable's current attribute(s).
    pub attributes: Vec<VariableAttribute>,
}

/// The outcome of a `GetBaseReport`/`GetReport` request, matching OCPP's
/// `GenericDeviceModelStatusEnum` - this is the status carried on the *immediate*
/// `GetBaseReportResponse`/`GetReportResponse`, sent before any `NotifyReport`s follow (see
/// [`handle_get_base_report`]/[`handle_get_report`]'s docs for which variants each can produce).
#[derive(Debug, Clone, PartialEq)]
pub enum ReportOutcome {
    /// The request was understood; the enclosed entries follow in one or more `NotifyReport`s
    /// (via [`chunk_report`]). May legitimately be empty (e.g. an unpopulated
    /// [`ReportBase::SummaryInventory`]) without that meaning failure - see
    /// [`handle_get_base_report`]'s docs on why that case is `Accepted`, not `EmptyResultSet`.
    Accepted(Vec<ReportEntry>),
    /// The charge point is temporarily unable to answer. Never produced by this crate's own
    /// handlers today (a device model lookup can't fail) - modeled for wire completeness, so a
    /// future hardware binding could report a real transient failure through this path.
    Rejected,
    /// The requested report base/criteria aren't supported. Never produced by
    /// [`handle_get_base_report`] (this crate's [`ReportBase`] already covers all 3 values OCPP
    /// defines) - modeled for the same wire-completeness reason as `Rejected`.
    NotSupported,
    /// A `GetReport` filter matched nothing at all. Only ever produced by [`handle_get_report`] -
    /// see its docs.
    EmptyResultSet,
}

/// Builds the [`ReportEntry`] list for one [`ReportBase`] against `device_model` - see
/// [`ReportBase`]'s variant docs for exactly what each includes.
fn report_base_entries(device_model: &DeviceModel, base: ReportBase) -> Vec<ReportEntry> {
    device_model
        .iter()
        .filter(|(_, variable, definition)| matches_base(base, variable, definition))
        .map(|(component, variable, definition)| to_report_entry(component, variable, definition))
        .collect()
}

/// Whether `variable`/`definition` belongs in `base`'s report - see [`ReportBase`]'s variant docs.
fn matches_base(base: ReportBase, variable: &Variable, definition: &VariableDefinition) -> bool {
    match base {
        ReportBase::FullInventory => true,
        ReportBase::ConfigurationInventory => definition
            .attributes
            .iter()
            .any(|attribute| attribute.mutability != VariableMutability::ReadOnly),
        ReportBase::SummaryInventory => SUMMARY_VARIABLE_NAMES.contains(&variable.name.as_str()),
    }
}

/// Builds a [`ReportEntry`] from a `DeviceModel::iter()` item.
fn to_report_entry(
    component: &Component,
    variable: &Variable,
    definition: &VariableDefinition,
) -> ReportEntry {
    ReportEntry {
        component: component.clone(),
        variable: variable.clone(),
        characteristics: definition.characteristics.clone(),
        attributes: definition.attributes.clone(),
    }
}

/// Handles a CSMS-initiated `GetBaseReport` request against `actor`'s current device model.
/// Always `Accepted` (possibly with zero entries, e.g. an unpopulated `SummaryInventory`) - all
/// three `ReportBase` values OCPP defines are supported unconditionally, and a device model lookup
/// can't fail, so `Rejected`/`NotSupported` are unreachable here (see [`ReportOutcome`]'s docs).
/// Unlike [`handle_get_report`], an empty result is still `Accepted`, never `EmptyResultSet` - OCPP
/// defines `EmptyResultSet` only for a *filtered* request producing nothing to report, and
/// `GetBaseReport` takes no filter at all.
pub fn handle_get_base_report(actor: &ChargePointActor, base: ReportBase) -> ReportOutcome {
    let state = actor.state();
    ReportOutcome::Accepted(report_base_entries(&state.device_model, base))
}

/// Whether `component` matches `criterion` against `device_model` - i.e. whether `component` has
/// a same-named Boolean control variable (see [`ReportComponentCriterion::variable_name`]) whose
/// `Actual` value is `"true"`, or (for every criterion except
/// [`Problem`](ReportComponentCriterion::Problem)) doesn't have that variable registered at all.
/// Mirrors OCPP's own B08 requirements table exactly: `Problem` is the one criterion where a
/// missing variable does **not** count as a match.
fn component_matches_criterion(
    device_model: &DeviceModel,
    component: &Component,
    criterion: ReportComponentCriterion,
) -> bool {
    let variable = Variable {
        name: criterion.variable_name().into(),
        instance: None,
    };
    match device_model
        .get(component, &variable)
        .and_then(|definition| definition.attribute(VariableAttributeType::Actual))
    {
        Some(attribute) => attribute.value == "true",
        None => criterion != ReportComponentCriterion::Problem,
    }
}

/// Filters `device_model` per `GetReport`'s `componentCriteria`/`componentVariable` semantics: a
/// component/variable is included if it matches *any* given criterion (OCPP: "report all
/// components that have at least one of the given criteria" - a logical OR), which, per matching
/// criterion, includes every variable on that whole component - *or* it's named by an entry in
/// `component_variable` (a specific variable, or every variable on that component if the entry's
/// `variable` is `None`). If *both* `criteria` and `component_variable` are empty there's nothing
/// to filter by, so this returns the same thing [`ReportBase::FullInventory`] would (a degenerate
/// request that OCPP doesn't define behaviour for; "report everything" is a safer default than
/// "report nothing").
fn filter_report_entries(
    device_model: &DeviceModel,
    criteria: &[ReportComponentCriterion],
    component_variable: &[ReportComponentVariable],
) -> Vec<ReportEntry> {
    if criteria.is_empty() && component_variable.is_empty() {
        return report_base_entries(device_model, ReportBase::FullInventory);
    }
    device_model
        .iter()
        .filter(|(component, variable, _)| {
            let matches_criteria = !criteria.is_empty()
                && criteria.iter().any(|criterion| {
                    component_matches_criterion(device_model, component, *criterion)
                });
            let matches_component_variable = component_variable.iter().any(|entry| {
                &entry.component == *component
                    && match &entry.variable {
                        Some(wanted) => wanted == *variable,
                        None => true,
                    }
            });
            matches_criteria || matches_component_variable
        })
        .map(|(component, variable, definition)| to_report_entry(component, variable, definition))
        .collect()
}

/// Handles a CSMS-initiated `GetReport` request against `actor`'s current device model, applying
/// `criteria`/`component_variable`, combined with OR semantics. `EmptyResultSet` if the
/// combination matches nothing at all - the one case where this differs from
/// [`handle_get_base_report`] (which never produces it).
pub fn handle_get_report(
    actor: &ChargePointActor,
    criteria: &[ReportComponentCriterion],
    component_variable: &[ReportComponentVariable],
) -> ReportOutcome {
    let state = actor.state();
    let entries = filter_report_entries(&state.device_model, criteria, component_variable);
    if entries.is_empty() {
        ReportOutcome::EmptyResultSet
    } else {
        ReportOutcome::Accepted(entries)
    }
}

/// The maximum number of [`ReportEntry`] items carried in a single `NotifyReport` chunk (see
/// [`chunk_report`]). Chosen to match `ocpp-types`' own non-`alloc` `NotifyReportRequest` variant's
/// default `report_data` capacity (a `heapless::Vec<ReportData, 16>` when that crate is built
/// without its `alloc` feature). This crate always builds `ocpp-types` with `alloc` (see
/// `docs/ROADMAP.md` §0), where `report_data` is an unbounded `Vec` with no type-level cap at
/// all - but picking the same size anyway keeps every `NotifyReport` this crate ever sends small
/// enough to fit comfortably in a single WebSocket frame/OCPP-J message regardless of build
/// configuration, while still honouring OCPP's "as few messages as possible" guidance for a
/// device model that isn't pathologically large.
pub const REPORT_CHUNK_SIZE: usize = 16;

/// One `NotifyReport` message's worth of a chunked report: its `seqNo` (0-based, incrementing per
/// chunk) and `tbc` ("to be continued" - whether another chunk follows), plus the entries this
/// chunk carries. Produced by [`chunk_report`].
#[derive(Debug, Clone, PartialEq)]
pub struct ReportChunk {
    /// This chunk's sequence number - `0` for the first chunk, incrementing by one per chunk.
    pub seq_no: i64,
    /// Whether another chunk follows this one.
    pub tbc: bool,
    /// This chunk's entries (at most [`REPORT_CHUNK_SIZE`]).
    pub entries: Vec<ReportEntry>,
}

/// Splits `entries` into an ordered sequence of [`ReportChunk`]s of at most [`REPORT_CHUNK_SIZE`]
/// items each, with correctly incrementing `seq_no`/`tbc` - the core, protocol-agnostic mechanism
/// behind every multi-part `NotifyReport` this crate sends (see this module's docs). An empty
/// `entries` still produces exactly one (empty, `tbc: false`) chunk rather than zero: `seqNo` must
/// start at `0` per OCPP, which only makes sense if at least one `NotifyReport` is actually sent -
/// see [`handle_get_base_report`]'s docs for why an empty *base* report is still worth reporting
/// (as opposed to [`handle_get_report`]'s `EmptyResultSet`, for which this crate's `ocpp_2_1`/
/// `ocpp_2_0_1` adapters send no `NotifyReport` at all).
pub fn chunk_report(entries: &[ReportEntry]) -> Vec<ReportChunk> {
    if entries.is_empty() {
        return alloc::vec![ReportChunk {
            seq_no: 0,
            tbc: false,
            entries: Vec::new(),
        }];
    }
    let total = entries.chunks(REPORT_CHUNK_SIZE).count();
    entries
        .chunks(REPORT_CHUNK_SIZE)
        .enumerate()
        .map(|(index, chunk)| ReportChunk {
            seq_no: index as i64,
            tbc: index + 1 < total,
            entries: chunk.to_vec(),
        })
        .collect()
}

/// Registers this charge point's inbound `GetBaseReport` handling with the CSMS connection.
/// Implemented per protocol version (see the `ocpp_2_1` module) - not applicable to OCPP 1.6J
/// (see this module's top-level docs).
#[async_trait::async_trait]
pub trait GetBaseReportHandler {
    /// Registers a `GetBaseReport` handler with the CSMS connection that answers with
    /// [`handle_get_base_report`]'s outcome, then sends the resulting entries (if any) as one or
    /// more `NotifyReport`s.
    async fn register_get_base_report_handler(&self, actor: ChargePointActor);
}

/// Registers this charge point's inbound `GetReport` handling with the CSMS connection.
/// Implemented per protocol version (see the `ocpp_2_1` module) - not applicable to OCPP 1.6J
/// (see this module's top-level docs).
#[async_trait::async_trait]
pub trait GetReportHandler {
    /// Registers a `GetReport` handler with the CSMS connection that answers with
    /// [`handle_get_report`]'s outcome, then sends the resulting entries (if any) as one or more
    /// `NotifyReport`s.
    async fn register_get_report_handler(&self, actor: ChargePointActor);
}

#[cfg(test)]
mod tests {
    use super::{
        REPORT_CHUNK_SIZE, ReportBase, ReportComponentCriterion, ReportComponentVariable,
        ReportEntry, ReportOutcome, chunk_report, component_matches_criterion,
        filter_report_entries, handle_get_base_report, handle_get_report, report_base_entries,
    };
    use crate::actor::ChargePointActor;
    use crate::executor::TokioExecutor;
    use crate::state::{
        Component, DeviceModel, Variable, VariableAttribute, VariableAttributeType,
        VariableCharacteristics, VariableDataType, VariableMutability,
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

    fn characteristics(data_type: VariableDataType) -> VariableCharacteristics {
        VariableCharacteristics {
            data_type,
            unit: None,
            min_limit: None,
            max_limit: None,
            values_list: None,
            supports_monitoring: false,
        }
    }

    fn attribute(
        attribute_type: VariableAttributeType,
        value: &str,
        mutability: VariableMutability,
    ) -> VariableAttribute {
        VariableAttribute {
            attribute_type,
            value: value.into(),
            mutability,
            persistent: false,
            constant: false,
            requires_reboot: false,
        }
    }

    #[test]
    fn full_inventory_includes_every_registered_variable() {
        let model = DeviceModel::new();

        let entries = report_base_entries(&model, ReportBase::FullInventory);

        assert_eq!(entries.len(), 6); // the six built-in defaults
    }

    #[test]
    fn configuration_inventory_excludes_read_only_only_variables() {
        let mut model = DeviceModel::new();
        model.register(
            component("Custom"),
            variable("ReadOnlyOnly"),
            characteristics(VariableDataType::String),
            alloc::vec![attribute(
                VariableAttributeType::Actual,
                "x",
                VariableMutability::ReadOnly
            )],
        );

        let entries = report_base_entries(&model, ReportBase::ConfigurationInventory);

        // Every built-in default is ReadWrite, so they're all included; the freshly registered
        // ReadOnly-only variable is not.
        assert_eq!(entries.len(), 6);
        assert!(
            entries
                .iter()
                .all(|entry| entry.variable.name != "ReadOnlyOnly")
        );
    }

    #[test]
    fn summary_inventory_only_includes_well_known_status_variable_names() {
        let mut model = DeviceModel::new();
        model.register(
            component("Connector"),
            variable("Problem"),
            characteristics(VariableDataType::Boolean),
            alloc::vec![attribute(
                VariableAttributeType::Actual,
                "true",
                VariableMutability::ReadOnly
            )],
        );

        let entries = report_base_entries(&model, ReportBase::SummaryInventory);

        // The freshly registered `Problem`, plus the one well-known control variable the built-in
        // defaults contribute (`AuthCacheCtrlr.Enabled`) - and nothing else, which is the point:
        // `HeartbeatInterval` and friends are configuration, not status.
        let names: alloc::vec::Vec<_> = entries
            .iter()
            .map(|entry| entry.variable.name.as_str())
            .collect();
        assert_eq!(names, alloc::vec!["Enabled", "Problem"]);
    }

    /// A fresh model now has exactly one well-known status/control variable among its built-in
    /// defaults - `AuthCacheCtrlr.Enabled`, registered by B1.2 - so the summary report is no
    /// longer empty out of the box. That is the summary working, not leaking: whether the
    /// authorization cache is enabled is precisely the kind of control state this report is for.
    #[test]
    fn a_fresh_device_model_summarises_the_control_variables_it_has() {
        let model = DeviceModel::new();

        let entries = report_base_entries(&model, ReportBase::SummaryInventory);

        let names: alloc::vec::Vec<_> = entries
            .iter()
            .map(|entry| (entry.component.name.as_str(), entry.variable.name.as_str()))
            .collect();
        assert_eq!(names, alloc::vec![("AuthCacheCtrlr", "Enabled")]);
    }

    #[test]
    fn a_component_missing_an_active_variable_still_matches_active() {
        let model = DeviceModel::new();

        assert!(component_matches_criterion(
            &model,
            &component("OCPPCommCtrlr"),
            ReportComponentCriterion::Active
        ));
    }

    #[test]
    fn a_component_missing_a_problem_variable_does_not_match_problem() {
        let model = DeviceModel::new();

        assert!(!component_matches_criterion(
            &model,
            &component("OCPPCommCtrlr"),
            ReportComponentCriterion::Problem
        ));
    }

    #[test]
    fn a_component_with_problem_true_matches_problem() {
        let mut model = DeviceModel::new();
        model.register(
            component("Connector"),
            variable("Problem"),
            characteristics(VariableDataType::Boolean),
            alloc::vec![attribute(
                VariableAttributeType::Actual,
                "true",
                VariableMutability::ReadOnly
            )],
        );

        assert!(component_matches_criterion(
            &model,
            &component("Connector"),
            ReportComponentCriterion::Problem
        ));
    }

    #[test]
    fn a_component_with_active_false_does_not_match_active() {
        let mut model = DeviceModel::new();
        model.register(
            component("Connector"),
            variable("Active"),
            characteristics(VariableDataType::Boolean),
            alloc::vec![attribute(
                VariableAttributeType::Actual,
                "false",
                VariableMutability::ReadOnly
            )],
        );

        assert!(!component_matches_criterion(
            &model,
            &component("Connector"),
            ReportComponentCriterion::Active
        ));
    }

    #[test]
    fn filtering_by_a_criterion_includes_every_variable_of_a_matching_component() {
        let mut model = DeviceModel::new();
        model.register(
            component("Connector"),
            variable("Active"),
            characteristics(VariableDataType::Boolean),
            alloc::vec![attribute(
                VariableAttributeType::Actual,
                "true",
                VariableMutability::ReadOnly
            )],
        );
        model.register(
            component("Connector"),
            variable("SomethingElse"),
            characteristics(VariableDataType::String),
            alloc::vec![attribute(
                VariableAttributeType::Actual,
                "x",
                VariableMutability::ReadOnly
            )],
        );

        let entries = filter_report_entries(&model, &[ReportComponentCriterion::Active], &[]);

        let names: alloc::vec::Vec<&str> = entries
            .iter()
            .filter(|entry| entry.component.name == "Connector")
            .map(|entry| entry.variable.name.as_str())
            .collect();
        assert!(names.contains(&"Active"));
        assert!(names.contains(&"SomethingElse"));
    }

    #[test]
    fn filtering_by_component_variable_with_no_variable_matches_the_whole_component() {
        let mut model = DeviceModel::new();
        model.register(
            component("Connector"),
            variable("A"),
            characteristics(VariableDataType::String),
            alloc::vec![attribute(
                VariableAttributeType::Actual,
                "a",
                VariableMutability::ReadOnly
            )],
        );
        model.register(
            component("Connector"),
            variable("B"),
            characteristics(VariableDataType::String),
            alloc::vec![attribute(
                VariableAttributeType::Actual,
                "b",
                VariableMutability::ReadOnly
            )],
        );

        let entries = filter_report_entries(
            &model,
            &[],
            &[ReportComponentVariable {
                component: component("Connector"),
                variable: None,
            }],
        );

        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn filtering_by_component_variable_with_a_variable_matches_only_that_one() {
        let mut model = DeviceModel::new();
        model.register(
            component("Connector"),
            variable("A"),
            characteristics(VariableDataType::String),
            alloc::vec![attribute(
                VariableAttributeType::Actual,
                "a",
                VariableMutability::ReadOnly
            )],
        );
        model.register(
            component("Connector"),
            variable("B"),
            characteristics(VariableDataType::String),
            alloc::vec![attribute(
                VariableAttributeType::Actual,
                "b",
                VariableMutability::ReadOnly
            )],
        );

        let entries = filter_report_entries(
            &model,
            &[],
            &[ReportComponentVariable {
                component: component("Connector"),
                variable: Some(variable("A")),
            }],
        );

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].variable.name, "A");
    }

    #[test]
    fn no_filter_at_all_reports_everything() {
        let model = DeviceModel::new();

        let entries = filter_report_entries(&model, &[], &[]);

        assert_eq!(
            entries.len(),
            report_base_entries(&model, ReportBase::FullInventory).len()
        );
    }

    /// `GetBaseReport` is `Accepted` whatever it finds - including nothing. A summary report on a
    /// charge point whose hardware registers no status variables at all is empty but still
    /// accepted, which is the case this pins (the built-in `AuthCacheCtrlr.Enabled` is filtered
    /// out here so the assertion is about emptiness, not about which defaults exist).
    #[tokio::test]
    async fn get_base_report_is_always_accepted_even_when_it_finds_almost_nothing() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let outcome = handle_get_base_report(&actor, ReportBase::SummaryInventory);

        let ReportOutcome::Accepted(entries) = outcome else {
            panic!("a base report is always accepted");
        };
        assert!(
            entries
                .iter()
                .all(|entry| entry.component.name == "AuthCacheCtrlr"),
            "nothing but the built-in control variable should be summarised on a fresh model"
        );
    }

    #[tokio::test]
    async fn get_report_with_a_filter_matching_nothing_is_an_empty_result_set() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let outcome = handle_get_report(&actor, &[ReportComponentCriterion::Problem], &[]);

        assert_eq!(outcome, ReportOutcome::EmptyResultSet);
    }

    #[tokio::test]
    async fn get_report_with_a_matching_component_variable_is_accepted() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let outcome = handle_get_report(
            &actor,
            &[],
            &[ReportComponentVariable {
                component: component("OCPPCommCtrlr"),
                variable: None,
            }],
        );

        match outcome {
            ReportOutcome::Accepted(entries) => assert_eq!(entries.len(), 1),
            other => panic!("expected Accepted, got {other:?}"),
        }
    }

    #[test]
    fn chunking_splits_at_the_configured_size_with_correct_seq_no_and_tbc() {
        let entries: alloc::vec::Vec<ReportEntry> = (0..(REPORT_CHUNK_SIZE * 2 + 1))
            .map(|i| ReportEntry {
                component: component("C"),
                variable: Variable {
                    name: alloc::format!("V{i}"),
                    instance: None,
                },
                characteristics: characteristics(VariableDataType::String),
                attributes: alloc::vec![attribute(
                    VariableAttributeType::Actual,
                    "x",
                    VariableMutability::ReadOnly
                )],
            })
            .collect();

        let chunks = chunk_report(&entries);

        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].seq_no, 0);
        assert!(chunks[0].tbc);
        assert_eq!(chunks[0].entries.len(), REPORT_CHUNK_SIZE);
        assert_eq!(chunks[1].seq_no, 1);
        assert!(chunks[1].tbc);
        assert_eq!(chunks[1].entries.len(), REPORT_CHUNK_SIZE);
        assert_eq!(chunks[2].seq_no, 2);
        assert!(!chunks[2].tbc);
        assert_eq!(chunks[2].entries.len(), 1);
    }

    #[test]
    fn chunking_an_empty_report_still_produces_one_chunk() {
        let chunks = chunk_report(&[]);

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].seq_no, 0);
        assert!(!chunks[0].tbc);
        assert!(chunks[0].entries.is_empty());
    }

    #[test]
    fn chunking_an_exact_multiple_has_no_trailing_partial_chunk() {
        let entries: alloc::vec::Vec<ReportEntry> = (0..(REPORT_CHUNK_SIZE * 2))
            .map(|i| ReportEntry {
                component: component("C"),
                variable: Variable {
                    name: alloc::format!("V{i}"),
                    instance: None,
                },
                characteristics: characteristics(VariableDataType::String),
                attributes: Vec::new(),
            })
            .collect();

        let chunks = chunk_report(&entries);

        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].tbc);
        assert!(!chunks[1].tbc);
    }
}

#[cfg(feature = "ocpp_2_1")]
mod ocpp_2_1 {
    pub use with_clock::Ocpp2_1ReportHandler;

    use super::{
        ReportBase, ReportComponentCriterion, ReportComponentVariable, ReportEntry, ReportOutcome,
    };
    use crate::state::{
        Component, Variable, VariableAttribute, VariableAttributeType, VariableCharacteristics,
        VariableDataType, VariableMutability,
    };
    use alloc::string::ToString;
    use ocpp_client::ocpp_types::v21::common::{
        AttributeEnum, Component as WireComponent, ComponentCriterionEnum, ComponentVariable,
        DataEnum, EVSE, GenericDeviceModelStatusEnum, MutabilityEnum, ReportBaseEnum,
        ReportData as WireReportData, Variable as WireVariable,
        VariableAttribute as WireVariableAttribute,
        VariableCharacteristics as WireVariableCharacteristics,
    };

    /// Truncates/bounds `value` to fit a `heapless::String<N>` on the wire, matching this crate's
    /// established truncate-over-drop convention (see e.g.
    /// `crate::device_model::ocpp_2_1::truncate_to_byte_boundary`) - after truncating to a
    /// byte-boundary-safe prefix no longer than `N`, the conversion below can never fail.
    fn bounded_string<const N: usize>(value: &str) -> heapless::String<N> {
        let mut end = value.len().min(N);
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        heapless::String::try_from(&value[..end]).expect("truncated to fit the wire bound")
    }

    fn map_report_base(base: &ReportBaseEnum) -> ReportBase {
        match base {
            ReportBaseEnum::ConfigurationInventory => ReportBase::ConfigurationInventory,
            ReportBaseEnum::FullInventory => ReportBase::FullInventory,
            ReportBaseEnum::SummaryInventory => ReportBase::SummaryInventory,
        }
    }

    fn map_component_criterion(criterion: &ComponentCriterionEnum) -> ReportComponentCriterion {
        match criterion {
            ComponentCriterionEnum::Active => ReportComponentCriterion::Active,
            ComponentCriterionEnum::Available => ReportComponentCriterion::Available,
            ComponentCriterionEnum::Enabled => ReportComponentCriterion::Enabled,
            ComponentCriterionEnum::Problem => ReportComponentCriterion::Problem,
        }
    }

    /// Mirrors `crate::device_model::ocpp_2_1::map_component` - duplicated rather than shared,
    /// the same small-helper-duplication convention that module's own docs describe for its
    /// `ocpp_2_0_1`/`ocpp_1_6` siblings.
    fn map_component(component: &WireComponent) -> Component {
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

    fn map_variable(variable: &WireVariable) -> Variable {
        Variable {
            name: variable.name.to_string(),
            instance: variable
                .instance
                .as_ref()
                .map(|instance| instance.to_string()),
        }
    }

    fn map_component_variable(item: &ComponentVariable) -> ReportComponentVariable {
        ReportComponentVariable {
            component: map_component(&item.component),
            variable: item.variable.as_ref().map(map_variable),
        }
    }

    fn map_status(outcome: &ReportOutcome) -> GenericDeviceModelStatusEnum {
        match outcome {
            ReportOutcome::Accepted(_) => GenericDeviceModelStatusEnum::Accepted,
            ReportOutcome::Rejected => GenericDeviceModelStatusEnum::Rejected,
            ReportOutcome::NotSupported => GenericDeviceModelStatusEnum::NotSupported,
            ReportOutcome::EmptyResultSet => GenericDeviceModelStatusEnum::EmptyResultSet,
        }
    }

    fn build_component(component: &Component) -> WireComponent {
        WireComponent {
            custom_data: None,
            evse: component.evse.map(|(evse_id, connector_id)| EVSE {
                connector_id: connector_id.and_then(|id| i64::try_from(id).ok()),
                custom_data: None,
                id: i64::try_from(evse_id).unwrap_or(i64::MAX),
            }),
            instance: component.instance.as_deref().map(bounded_string::<50>),
            name: bounded_string::<50>(&component.name),
        }
    }

    fn build_variable(variable: &Variable) -> WireVariable {
        WireVariable {
            custom_data: None,
            instance: variable.instance.as_deref().map(bounded_string::<50>),
            name: bounded_string::<50>(&variable.name),
        }
    }

    fn build_mutability(mutability: VariableMutability) -> MutabilityEnum {
        match mutability {
            VariableMutability::ReadOnly => MutabilityEnum::ReadOnly,
            VariableMutability::WriteOnly => MutabilityEnum::WriteOnly,
            VariableMutability::ReadWrite => MutabilityEnum::ReadWrite,
        }
    }

    fn build_attribute_type(attribute_type: VariableAttributeType) -> AttributeEnum {
        match attribute_type {
            VariableAttributeType::Actual => AttributeEnum::Actual,
            VariableAttributeType::Target => AttributeEnum::Target,
            VariableAttributeType::MinSet => AttributeEnum::MinSet,
            VariableAttributeType::MaxSet => AttributeEnum::MaxSet,
        }
    }

    fn build_data_type(data_type: VariableDataType) -> DataEnum {
        match data_type {
            VariableDataType::String => DataEnum::String,
            VariableDataType::Decimal => DataEnum::Decimal,
            VariableDataType::Integer => DataEnum::Integer,
            VariableDataType::DateTime => DataEnum::DateTime,
            VariableDataType::Boolean => DataEnum::Boolean,
            VariableDataType::OptionList => DataEnum::OptionList,
            VariableDataType::SequenceList => DataEnum::SequenceList,
            VariableDataType::MemberList => DataEnum::MemberList,
        }
    }

    /// A `WriteOnly` attribute's value is never reported back - mirrors
    /// `crate::device_model::GetVariableOutcome::Rejected` for the same attribute (nothing
    /// meaningful to read back), and matches OCPP's own requirement that a report excludes
    /// `WriteOnly` values.
    fn build_variable_attribute(attribute: &VariableAttribute) -> WireVariableAttribute {
        WireVariableAttribute {
            constant: Some(attribute.constant),
            custom_data: None,
            mutability: Some(build_mutability(attribute.mutability)),
            persistent: Some(attribute.persistent),
            r#type: Some(build_attribute_type(attribute.attribute_type)),
            value: (attribute.mutability != VariableMutability::WriteOnly)
                .then(|| bounded_string::<2500>(&attribute.value)),
        }
    }

    fn build_characteristics(
        characteristics: &VariableCharacteristics,
    ) -> WireVariableCharacteristics {
        WireVariableCharacteristics {
            custom_data: None,
            data_type: build_data_type(characteristics.data_type),
            max_elements: None,
            max_limit: characteristics.max_limit,
            min_limit: characteristics.min_limit,
            supports_monitoring: characteristics.supports_monitoring,
            unit: characteristics.unit.as_deref().map(bounded_string::<16>),
            values_list: characteristics
                .values_list
                .as_ref()
                .map(|values| bounded_string::<1000>(&values.join(","))),
        }
    }

    /// Builds one wire `ReportData` entry - capped to at most 4 attributes (OCPP's own
    /// `variableAttribute` bound: `Actual`/`Target`/`MinSet`/`MaxSet` is the complete set of
    /// attribute types anyway, so a well-formed [`ReportEntry`] never actually has more; the cap
    /// only guards against a pathological one panicking the wire-side `heapless::Vec` build).
    fn build_report_data(entry: &ReportEntry) -> WireReportData {
        WireReportData {
            component: build_component(&entry.component),
            custom_data: None,
            variable: build_variable(&entry.variable),
            variable_attribute: entry
                .attributes
                .iter()
                .take(4)
                .map(build_variable_attribute)
                .collect(),
            variable_characteristics: Some(build_characteristics(&entry.characteristics)),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::state::VariableAttribute as InternalVariableAttribute;

        #[test]
        fn every_report_base_maps_to_the_matching_wire_value() {
            assert_eq!(
                map_report_base(&ReportBaseEnum::ConfigurationInventory),
                ReportBase::ConfigurationInventory
            );
            assert_eq!(
                map_report_base(&ReportBaseEnum::FullInventory),
                ReportBase::FullInventory
            );
            assert_eq!(
                map_report_base(&ReportBaseEnum::SummaryInventory),
                ReportBase::SummaryInventory
            );
        }

        #[test]
        fn every_component_criterion_maps_to_the_matching_wire_value() {
            assert_eq!(
                map_component_criterion(&ComponentCriterionEnum::Active),
                ReportComponentCriterion::Active
            );
            assert_eq!(
                map_component_criterion(&ComponentCriterionEnum::Available),
                ReportComponentCriterion::Available
            );
            assert_eq!(
                map_component_criterion(&ComponentCriterionEnum::Enabled),
                ReportComponentCriterion::Enabled
            );
            assert_eq!(
                map_component_criterion(&ComponentCriterionEnum::Problem),
                ReportComponentCriterion::Problem
            );
        }

        #[test]
        fn every_outcome_maps_to_the_matching_wire_status() {
            assert_eq!(
                map_status(&ReportOutcome::Accepted(Vec::new())),
                GenericDeviceModelStatusEnum::Accepted
            );
            assert_eq!(
                map_status(&ReportOutcome::Rejected),
                GenericDeviceModelStatusEnum::Rejected
            );
            assert_eq!(
                map_status(&ReportOutcome::NotSupported),
                GenericDeviceModelStatusEnum::NotSupported
            );
            assert_eq!(
                map_status(&ReportOutcome::EmptyResultSet),
                GenericDeviceModelStatusEnum::EmptyResultSet
            );
        }

        #[test]
        fn a_write_only_attributes_value_is_omitted_from_the_wire_report() {
            let attribute = InternalVariableAttribute {
                attribute_type: VariableAttributeType::Actual,
                value: "secret".into(),
                mutability: VariableMutability::WriteOnly,
                persistent: false,
                constant: false,
                requires_reboot: false,
            };

            let built = build_variable_attribute(&attribute);

            assert_eq!(built.value, None);
        }

        #[test]
        fn a_read_write_attributes_value_is_included() {
            let attribute = InternalVariableAttribute {
                attribute_type: VariableAttributeType::Actual,
                value: "60".into(),
                mutability: VariableMutability::ReadWrite,
                persistent: true,
                constant: false,
                requires_reboot: false,
            };

            let built = build_variable_attribute(&attribute);

            assert_eq!(built.value.as_deref(), Some("60"));
        }

        #[test]
        fn report_data_never_carries_more_than_four_attributes() {
            let entry = ReportEntry {
                component: Component {
                    name: "C".into(),
                    instance: None,
                    evse: None,
                },
                variable: Variable {
                    name: "V".into(),
                    instance: None,
                },
                characteristics: VariableCharacteristics {
                    data_type: VariableDataType::String,
                    unit: None,
                    min_limit: None,
                    max_limit: None,
                    values_list: None,
                    supports_monitoring: false,
                },
                attributes: alloc::vec![
                    InternalVariableAttribute {
                        attribute_type: VariableAttributeType::Actual,
                        value: "a".into(),
                        mutability: VariableMutability::ReadOnly,
                        persistent: false,
                        constant: false,
                        requires_reboot: false,
                    };
                    5
                ],
            };

            let built = build_report_data(&entry);

            assert_eq!(built.variable_attribute.len(), 4);
        }

        #[test]
        fn an_over_long_value_is_truncated_rather_than_dropped() {
            let long_value = alloc::string::String::from("a").repeat(3000);
            let attribute = InternalVariableAttribute {
                attribute_type: VariableAttributeType::Actual,
                value: long_value,
                mutability: VariableMutability::ReadWrite,
                persistent: false,
                constant: false,
                requires_reboot: false,
            };

            let built = build_variable_attribute(&attribute);

            assert_eq!(built.value.unwrap().len(), 2500);
        }
    }

    // `NotifyReportRequest.generatedAt` needs a timestamp, produced from a caller-supplied
    // `Clock` - see `crate::clock::is_synchronized`'s docs for the policy on an unsynchronized
    // reading (send it as-is, warn, never fabricate or drop it).
    mod with_clock {
        use super::{
            build_report_data, map_component_criterion, map_component_variable, map_report_base,
            map_status,
        };
        use crate::actor::ChargePointActor;
        use crate::clock::{Clock, is_synchronized};
        use crate::reporting::{
            GetBaseReportHandler, GetReportHandler, ReportEntry, ReportOutcome, chunk_report,
            handle_get_base_report, handle_get_report,
        };
        use alloc::boxed::Box;
        use alloc::vec::Vec;
        use ocpp_client::ocpp_2_1::OCPP2_1Client;
        use ocpp_client::ocpp_types::v21::{
            GetBaseReportResponse, GetReportResponse, NotifyReportRequest,
        };

        /// The `NotifyReport.generatedAt` timestamp, sourced from `clock.now()` - pure (no
        /// network I/O), so a fixed [`Clock`] fake can assert the exact value used, without
        /// needing a live `OCPP2_1Client` - see this module's tests.
        fn generated_at<C: Clock>(clock: &C) -> alloc::string::String {
            let now = clock.now();
            if !is_synchronized(&now) {
                tracing::warn!(
                    timestamp = %now,
                    "NotifyReport generatedAt sourced from an unsynchronized clock"
                );
            }
            now.to_rfc3339()
        }

        /// Sends `entries` (already decided `Accepted` by the caller) to the CSMS as one or more
        /// `NotifyReport`s, chunked via `chunk_report`, all sharing one `generatedAt` timestamp
        /// from `clock.now()`. A chunk that fails to send is logged and skipped rather than
        /// aborting the rest - the same "log and continue" treatment this crate gives every other
        /// fire-and-forget report send (e.g. `run_with_offline_queue`'s callers).
        async fn send_report_chunks<C: Clock>(
            client: &OCPP2_1Client,
            clock: &C,
            request_id: i64,
            entries: Vec<ReportEntry>,
        ) {
            let generated_at = generated_at(clock);
            for chunk in chunk_report(&entries) {
                let report_data: Vec<_> = chunk.entries.iter().map(build_report_data).collect();
                let request = NotifyReportRequest {
                    custom_data: None,
                    generated_at: generated_at.clone(),
                    report_data: (!report_data.is_empty()).then_some(report_data),
                    request_id,
                    seq_no: chunk.seq_no,
                    tbc: Some(chunk.tbc),
                };
                if let Err(err) = client.send_notify_report(request).await {
                    tracing::warn!(error = %err, "failed to send a NotifyReport chunk");
                }
            }
        }

        /// Wraps an `OCPP2_1Client` with a caller-supplied [`Clock`] for `NotifyReport`'s
        /// `generatedAt` timestamp - the no_std-reachable counterpart to this module's `std`-only
        /// `OCPP2_1Client` impls below, which exist alongside this (not instead of it) so no
        /// existing `std` caller needs a source change.
        pub struct Ocpp2_1ReportHandler<C> {
            client: OCPP2_1Client,
            clock: C,
        }

        impl<C: Clock> Ocpp2_1ReportHandler<C> {
            /// Wraps `client`, sourcing every `NotifyReport`'s `generatedAt` from `clock`.
            pub fn with_clock(client: OCPP2_1Client, clock: C) -> Self {
                Self { client, clock }
            }
        }

        #[cfg(feature = "std")]
        impl Ocpp2_1ReportHandler<crate::clock::SystemClock> {
            /// Wraps `client`, sourcing every `NotifyReport`'s `generatedAt` from
            /// [`crate::clock::SystemClock`].
            pub fn new(client: OCPP2_1Client) -> Self {
                Self::with_clock(client, crate::clock::SystemClock)
            }
        }

        #[async_trait::async_trait]
        impl<C: Clock + Clone + Send + Sync + 'static> GetBaseReportHandler for Ocpp2_1ReportHandler<C> {
            async fn register_get_base_report_handler(&self, actor: ChargePointActor) {
                let clock = self.clock.clone();
                self.client
                    .on_get_base_report(move |request, client| {
                        let actor = actor.clone();
                        let clock = clock.clone();
                        async move {
                            let base = map_report_base(&request.report_base);
                            let outcome = handle_get_base_report(&actor, base);
                            let response = GetBaseReportResponse {
                                custom_data: None,
                                status: map_status(&outcome),
                                status_info: None,
                            };
                            if let ReportOutcome::Accepted(entries) = outcome {
                                send_report_chunks(&client, &clock, request.request_id, entries)
                                    .await;
                            }
                            Ok(response)
                        }
                    })
                    .await;
            }
        }

        #[async_trait::async_trait]
        impl<C: Clock + Clone + Send + Sync + 'static> GetReportHandler for Ocpp2_1ReportHandler<C> {
            async fn register_get_report_handler(&self, actor: ChargePointActor) {
                let clock = self.clock.clone();
                self.client
                    .on_get_report(move |request, client| {
                        let actor = actor.clone();
                        let clock = clock.clone();
                        async move {
                            let criteria: Vec<_> = request
                                .component_criteria
                                .as_ref()
                                .map(|list| list.iter().map(map_component_criterion).collect())
                                .unwrap_or_default();
                            let component_variable: Vec<_> = request
                                .component_variable
                                .as_ref()
                                .map(|list| list.iter().map(map_component_variable).collect())
                                .unwrap_or_default();
                            let outcome = handle_get_report(&actor, &criteria, &component_variable);
                            let response = GetReportResponse {
                                custom_data: None,
                                status: map_status(&outcome),
                                status_info: None,
                            };
                            if let ReportOutcome::Accepted(entries) = outcome {
                                send_report_chunks(&client, &clock, request.request_id, entries)
                                    .await;
                            }
                            Ok(response)
                        }
                    })
                    .await;
            }
        }

        /// The `std` convenience: `OCPP2_1Client` itself implements `GetBaseReportHandler`/
        /// `GetReportHandler` directly (sourcing `generatedAt` from
        /// [`crate::clock::SystemClock`]) so existing callers that pass a bare client - e.g.
        /// [`crate::connect::connect_and_setup`] - need no source change.
        #[cfg(feature = "std")]
        #[async_trait::async_trait]
        impl GetBaseReportHandler for OCPP2_1Client {
            async fn register_get_base_report_handler(&self, actor: ChargePointActor) {
                self.on_get_base_report(move |request, client| {
                    let actor = actor.clone();
                    async move {
                        let base = map_report_base(&request.report_base);
                        let outcome = handle_get_base_report(&actor, base);
                        let response = GetBaseReportResponse {
                            custom_data: None,
                            status: map_status(&outcome),
                            status_info: None,
                        };
                        if let ReportOutcome::Accepted(entries) = outcome {
                            send_report_chunks(
                                &client,
                                &crate::clock::SystemClock,
                                request.request_id,
                                entries,
                            )
                            .await;
                        }
                        Ok(response)
                    }
                })
                .await;
            }
        }

        #[cfg(feature = "std")]
        #[async_trait::async_trait]
        impl GetReportHandler for OCPP2_1Client {
            async fn register_get_report_handler(&self, actor: ChargePointActor) {
                self.on_get_report(move |request, client| {
                    let actor = actor.clone();
                    async move {
                        let criteria: Vec<_> = request
                            .component_criteria
                            .as_ref()
                            .map(|list| list.iter().map(map_component_criterion).collect())
                            .unwrap_or_default();
                        let component_variable: Vec<_> = request
                            .component_variable
                            .as_ref()
                            .map(|list| list.iter().map(map_component_variable).collect())
                            .unwrap_or_default();
                        let outcome = handle_get_report(&actor, &criteria, &component_variable);
                        let response = GetReportResponse {
                            custom_data: None,
                            status: map_status(&outcome),
                            status_info: None,
                        };
                        if let ReportOutcome::Accepted(entries) = outcome {
                            send_report_chunks(
                                &client,
                                &crate::clock::SystemClock,
                                request.request_id,
                                entries,
                            )
                            .await;
                        }
                        Ok(response)
                    }
                })
                .await;
            }
        }

        #[cfg(test)]
        mod tests {
            use super::*;
            use chrono::{DateTime, Utc};

            #[test]
            fn ocpp2_1_client_implements_the_report_handler_traits() {
                fn assert_impl<T: GetBaseReportHandler + GetReportHandler>() {}
                assert_impl::<OCPP2_1Client>();
            }

            /// A [`Clock`] that always reads a fixed, caller-chosen instant.
            #[derive(Clone)]
            struct FixedClock(DateTime<Utc>);

            impl Clock for FixedClock {
                fn now(&self) -> DateTime<Utc> {
                    self.0
                }
            }

            #[test]
            fn ocpp2_1_report_handler_can_be_constructed_with_a_caller_supplied_clock() {
                // Compile-level check that `with_clock` accepts a non-`SystemClock` fake,
                // unlocking the no_std path `crate::clock::SystemClock` (a `std`-only type)
                // can't reach - the actual timestamp behavior is exercised by `generated_at`'s
                // own tests below, which don't need a live `OCPP2_1Client`.
                fn assert_ctor<
                    F: Fn(OCPP2_1Client, FixedClock) -> Ocpp2_1ReportHandler<FixedClock>,
                >(
                    _: F,
                ) {
                }
                assert_ctor(Ocpp2_1ReportHandler::with_clock);
            }

            #[test]
            fn a_fixed_clocks_reading_is_used_as_generated_at() {
                let fixed = DateTime::parse_from_rfc3339("2026-03-04T05:06:07Z")
                    .unwrap()
                    .with_timezone(&Utc);
                let clock = FixedClock(fixed);

                assert_eq!(generated_at(&clock), fixed.to_rfc3339());
            }

            #[test]
            fn an_unsynchronized_clocks_reading_is_still_used_not_substituted_or_dropped() {
                let unset_rtc = FixedClock(DateTime::<Utc>::from_timestamp(0, 0).unwrap());
                assert!(!is_synchronized(&unset_rtc.now()));

                assert_eq!(generated_at(&unset_rtc), unset_rtc.0.to_rfc3339());
            }
        }
    }
}

/// The OCPP 2.0.1 projection - identical `GetBaseReportRequest`/`GetBaseReportResponse`/
/// `GetReportRequest`/`GetReportResponse`/`NotifyReportRequest`/`NotifyReportResponse`/
/// `ReportData`/`Component`/`Variable`/`VariableAttribute`/`VariableCharacteristics`/
/// `ComponentVariable`/`ComponentCriterionEnum`/`ReportBaseEnum`/`GenericDeviceModelStatusEnum`
/// wire shapes to 2.1's (2.1 only adds an extra `maxElements` field to `VariableCharacteristics`,
/// which neither action ever transmits either way), so this is a close copy of the `ocpp_2_1`
/// module, just targeting `OCPP2_0_1Client`/`ocpp_types::v201`.
#[cfg(feature = "ocpp_2_0_1")]
mod ocpp_2_0_1 {
    pub use with_clock::Ocpp2_0_1ReportHandler;

    use super::{
        ReportBase, ReportComponentCriterion, ReportComponentVariable, ReportEntry, ReportOutcome,
    };
    use crate::state::{
        Component, Variable, VariableAttribute, VariableAttributeType, VariableCharacteristics,
        VariableDataType, VariableMutability,
    };
    use alloc::string::ToString;
    use ocpp_client::ocpp_types::v201::common::{
        AttributeEnum, Component as WireComponent, ComponentCriterionEnum, ComponentVariable,
        DataEnum, EVSE, GenericDeviceModelStatusEnum, MutabilityEnum, ReportBaseEnum,
        ReportData as WireReportData, Variable as WireVariable,
        VariableAttribute as WireVariableAttribute,
        VariableCharacteristics as WireVariableCharacteristics,
    };

    fn bounded_string<const N: usize>(value: &str) -> heapless::String<N> {
        let mut end = value.len().min(N);
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        heapless::String::try_from(&value[..end]).expect("truncated to fit the wire bound")
    }

    fn map_report_base(base: &ReportBaseEnum) -> ReportBase {
        match base {
            ReportBaseEnum::ConfigurationInventory => ReportBase::ConfigurationInventory,
            ReportBaseEnum::FullInventory => ReportBase::FullInventory,
            ReportBaseEnum::SummaryInventory => ReportBase::SummaryInventory,
        }
    }

    fn map_component_criterion(criterion: &ComponentCriterionEnum) -> ReportComponentCriterion {
        match criterion {
            ComponentCriterionEnum::Active => ReportComponentCriterion::Active,
            ComponentCriterionEnum::Available => ReportComponentCriterion::Available,
            ComponentCriterionEnum::Enabled => ReportComponentCriterion::Enabled,
            ComponentCriterionEnum::Problem => ReportComponentCriterion::Problem,
        }
    }

    /// Mirrors `super::ocpp_2_1::map_component`.
    fn map_component(component: &WireComponent) -> Component {
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

    fn map_variable(variable: &WireVariable) -> Variable {
        Variable {
            name: variable.name.to_string(),
            instance: variable
                .instance
                .as_ref()
                .map(|instance| instance.to_string()),
        }
    }

    fn map_component_variable(item: &ComponentVariable) -> ReportComponentVariable {
        ReportComponentVariable {
            component: map_component(&item.component),
            variable: item.variable.as_ref().map(map_variable),
        }
    }

    fn map_status(outcome: &ReportOutcome) -> GenericDeviceModelStatusEnum {
        match outcome {
            ReportOutcome::Accepted(_) => GenericDeviceModelStatusEnum::Accepted,
            ReportOutcome::Rejected => GenericDeviceModelStatusEnum::Rejected,
            ReportOutcome::NotSupported => GenericDeviceModelStatusEnum::NotSupported,
            ReportOutcome::EmptyResultSet => GenericDeviceModelStatusEnum::EmptyResultSet,
        }
    }

    fn build_component(component: &Component) -> WireComponent {
        WireComponent {
            custom_data: None,
            evse: component.evse.map(|(evse_id, connector_id)| EVSE {
                connector_id: connector_id.and_then(|id| i64::try_from(id).ok()),
                custom_data: None,
                id: i64::try_from(evse_id).unwrap_or(i64::MAX),
            }),
            instance: component.instance.as_deref().map(bounded_string::<50>),
            name: bounded_string::<50>(&component.name),
        }
    }

    fn build_variable(variable: &Variable) -> WireVariable {
        WireVariable {
            custom_data: None,
            instance: variable.instance.as_deref().map(bounded_string::<50>),
            name: bounded_string::<50>(&variable.name),
        }
    }

    fn build_mutability(mutability: VariableMutability) -> MutabilityEnum {
        match mutability {
            VariableMutability::ReadOnly => MutabilityEnum::ReadOnly,
            VariableMutability::WriteOnly => MutabilityEnum::WriteOnly,
            VariableMutability::ReadWrite => MutabilityEnum::ReadWrite,
        }
    }

    fn build_attribute_type(attribute_type: VariableAttributeType) -> AttributeEnum {
        match attribute_type {
            VariableAttributeType::Actual => AttributeEnum::Actual,
            VariableAttributeType::Target => AttributeEnum::Target,
            VariableAttributeType::MinSet => AttributeEnum::MinSet,
            VariableAttributeType::MaxSet => AttributeEnum::MaxSet,
        }
    }

    fn build_data_type(data_type: VariableDataType) -> DataEnum {
        match data_type {
            VariableDataType::String => DataEnum::String,
            VariableDataType::Decimal => DataEnum::Decimal,
            VariableDataType::Integer => DataEnum::Integer,
            VariableDataType::DateTime => DataEnum::DateTime,
            VariableDataType::Boolean => DataEnum::Boolean,
            VariableDataType::OptionList => DataEnum::OptionList,
            VariableDataType::SequenceList => DataEnum::SequenceList,
            VariableDataType::MemberList => DataEnum::MemberList,
        }
    }

    fn build_variable_attribute(attribute: &VariableAttribute) -> WireVariableAttribute {
        WireVariableAttribute {
            constant: Some(attribute.constant),
            custom_data: None,
            mutability: Some(build_mutability(attribute.mutability)),
            persistent: Some(attribute.persistent),
            r#type: Some(build_attribute_type(attribute.attribute_type)),
            value: (attribute.mutability != VariableMutability::WriteOnly)
                .then(|| bounded_string::<2500>(&attribute.value)),
        }
    }

    fn build_characteristics(
        characteristics: &VariableCharacteristics,
    ) -> WireVariableCharacteristics {
        WireVariableCharacteristics {
            custom_data: None,
            data_type: build_data_type(characteristics.data_type),
            max_limit: characteristics.max_limit,
            min_limit: characteristics.min_limit,
            supports_monitoring: characteristics.supports_monitoring,
            unit: characteristics.unit.as_deref().map(bounded_string::<16>),
            values_list: characteristics
                .values_list
                .as_ref()
                .map(|values| bounded_string::<1000>(&values.join(","))),
        }
    }

    fn build_report_data(entry: &ReportEntry) -> WireReportData {
        WireReportData {
            component: build_component(&entry.component),
            custom_data: None,
            variable: build_variable(&entry.variable),
            variable_attribute: entry
                .attributes
                .iter()
                .take(4)
                .map(build_variable_attribute)
                .collect(),
            variable_characteristics: Some(build_characteristics(&entry.characteristics)),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn every_report_base_maps_to_the_matching_wire_value() {
            assert_eq!(
                map_report_base(&ReportBaseEnum::FullInventory),
                ReportBase::FullInventory
            );
            assert_eq!(
                map_report_base(&ReportBaseEnum::SummaryInventory),
                ReportBase::SummaryInventory
            );
        }

        #[test]
        fn every_outcome_maps_to_the_matching_wire_status() {
            assert_eq!(
                map_status(&ReportOutcome::EmptyResultSet),
                GenericDeviceModelStatusEnum::EmptyResultSet
            );
            assert_eq!(
                map_status(&ReportOutcome::Rejected),
                GenericDeviceModelStatusEnum::Rejected
            );
        }

        #[test]
        fn a_write_only_attributes_value_is_omitted_from_the_wire_report() {
            let attribute = VariableAttribute {
                attribute_type: VariableAttributeType::Actual,
                value: "secret".into(),
                mutability: VariableMutability::WriteOnly,
                persistent: false,
                constant: false,
                requires_reboot: false,
            };

            let built = build_variable_attribute(&attribute);

            assert_eq!(built.value, None);
        }
    }

    // `NotifyReportRequest.generatedAt` needs a timestamp, produced from a caller-supplied
    // `Clock` - see `crate::clock::is_synchronized`'s docs for the policy on an unsynchronized
    // reading (send it as-is, warn, never fabricate or drop it). Mirrors
    // `super::ocpp_2_1::with_clock`.
    mod with_clock {
        use super::{
            build_report_data, map_component_criterion, map_component_variable, map_report_base,
            map_status,
        };
        use crate::actor::ChargePointActor;
        use crate::clock::{Clock, is_synchronized};
        use crate::reporting::{
            GetBaseReportHandler, GetReportHandler, ReportEntry, ReportOutcome, chunk_report,
            handle_get_base_report, handle_get_report,
        };
        use alloc::boxed::Box;
        use alloc::vec::Vec;
        use ocpp_client::ocpp_2_0_1::OCPP2_0_1Client;
        use ocpp_client::ocpp_types::v201::{
            GetBaseReportResponse, GetReportResponse, NotifyReportRequest,
        };

        fn generated_at<C: Clock>(clock: &C) -> alloc::string::String {
            let now = clock.now();
            if !is_synchronized(&now) {
                tracing::warn!(
                    timestamp = %now,
                    "NotifyReport generatedAt sourced from an unsynchronized clock"
                );
            }
            now.to_rfc3339()
        }

        async fn send_report_chunks<C: Clock>(
            client: &OCPP2_0_1Client,
            clock: &C,
            request_id: i64,
            entries: Vec<ReportEntry>,
        ) {
            let generated_at = generated_at(clock);
            for chunk in chunk_report(&entries) {
                let report_data: Vec<_> = chunk.entries.iter().map(build_report_data).collect();
                let request = NotifyReportRequest {
                    custom_data: None,
                    generated_at: generated_at.clone(),
                    report_data: (!report_data.is_empty()).then_some(report_data),
                    request_id,
                    seq_no: chunk.seq_no,
                    tbc: Some(chunk.tbc),
                };
                if let Err(err) = client.send_notify_report(request).await {
                    tracing::warn!(error = %err, "failed to send a NotifyReport chunk");
                }
            }
        }

        /// Wraps an `OCPP2_0_1Client` with a caller-supplied [`Clock`] for `NotifyReport`'s
        /// `generatedAt` timestamp - mirrors `super::ocpp_2_1::with_clock::Ocpp2_1ReportHandler`.
        pub struct Ocpp2_0_1ReportHandler<C> {
            client: OCPP2_0_1Client,
            clock: C,
        }

        impl<C: Clock> Ocpp2_0_1ReportHandler<C> {
            /// Wraps `client`, sourcing every `NotifyReport`'s `generatedAt` from `clock`.
            pub fn with_clock(client: OCPP2_0_1Client, clock: C) -> Self {
                Self { client, clock }
            }
        }

        #[cfg(feature = "std")]
        impl Ocpp2_0_1ReportHandler<crate::clock::SystemClock> {
            /// Wraps `client`, sourcing every `NotifyReport`'s `generatedAt` from
            /// [`crate::clock::SystemClock`].
            pub fn new(client: OCPP2_0_1Client) -> Self {
                Self::with_clock(client, crate::clock::SystemClock)
            }
        }

        #[async_trait::async_trait]
        impl<C: Clock + Clone + Send + Sync + 'static> GetBaseReportHandler for Ocpp2_0_1ReportHandler<C> {
            async fn register_get_base_report_handler(&self, actor: ChargePointActor) {
                let clock = self.clock.clone();
                self.client
                    .on_get_base_report(move |request, client| {
                        let actor = actor.clone();
                        let clock = clock.clone();
                        async move {
                            let base = map_report_base(&request.report_base);
                            let outcome = handle_get_base_report(&actor, base);
                            let response = GetBaseReportResponse {
                                custom_data: None,
                                status: map_status(&outcome),
                                status_info: None,
                            };
                            if let ReportOutcome::Accepted(entries) = outcome {
                                send_report_chunks(&client, &clock, request.request_id, entries)
                                    .await;
                            }
                            Ok(response)
                        }
                    })
                    .await;
            }
        }

        #[async_trait::async_trait]
        impl<C: Clock + Clone + Send + Sync + 'static> GetReportHandler for Ocpp2_0_1ReportHandler<C> {
            async fn register_get_report_handler(&self, actor: ChargePointActor) {
                let clock = self.clock.clone();
                self.client
                    .on_get_report(move |request, client| {
                        let actor = actor.clone();
                        let clock = clock.clone();
                        async move {
                            let criteria: Vec<_> = request
                                .component_criteria
                                .as_ref()
                                .map(|list| list.iter().map(map_component_criterion).collect())
                                .unwrap_or_default();
                            let component_variable: Vec<_> = request
                                .component_variable
                                .as_ref()
                                .map(|list| list.iter().map(map_component_variable).collect())
                                .unwrap_or_default();
                            let outcome = handle_get_report(&actor, &criteria, &component_variable);
                            let response = GetReportResponse {
                                custom_data: None,
                                status: map_status(&outcome),
                                status_info: None,
                            };
                            if let ReportOutcome::Accepted(entries) = outcome {
                                send_report_chunks(&client, &clock, request.request_id, entries)
                                    .await;
                            }
                            Ok(response)
                        }
                    })
                    .await;
            }
        }

        /// The `std` convenience: `OCPP2_0_1Client` itself implements `GetBaseReportHandler`/
        /// `GetReportHandler` directly (sourcing `generatedAt` from
        /// [`crate::clock::SystemClock`]) so existing callers that pass a bare client need no
        /// source change.
        #[cfg(feature = "std")]
        #[async_trait::async_trait]
        impl GetBaseReportHandler for OCPP2_0_1Client {
            async fn register_get_base_report_handler(&self, actor: ChargePointActor) {
                self.on_get_base_report(move |request, client| {
                    let actor = actor.clone();
                    async move {
                        let base = map_report_base(&request.report_base);
                        let outcome = handle_get_base_report(&actor, base);
                        let response = GetBaseReportResponse {
                            custom_data: None,
                            status: map_status(&outcome),
                            status_info: None,
                        };
                        if let ReportOutcome::Accepted(entries) = outcome {
                            send_report_chunks(
                                &client,
                                &crate::clock::SystemClock,
                                request.request_id,
                                entries,
                            )
                            .await;
                        }
                        Ok(response)
                    }
                })
                .await;
            }
        }

        #[cfg(feature = "std")]
        #[async_trait::async_trait]
        impl GetReportHandler for OCPP2_0_1Client {
            async fn register_get_report_handler(&self, actor: ChargePointActor) {
                self.on_get_report(move |request, client| {
                    let actor = actor.clone();
                    async move {
                        let criteria: Vec<_> = request
                            .component_criteria
                            .as_ref()
                            .map(|list| list.iter().map(map_component_criterion).collect())
                            .unwrap_or_default();
                        let component_variable: Vec<_> = request
                            .component_variable
                            .as_ref()
                            .map(|list| list.iter().map(map_component_variable).collect())
                            .unwrap_or_default();
                        let outcome = handle_get_report(&actor, &criteria, &component_variable);
                        let response = GetReportResponse {
                            custom_data: None,
                            status: map_status(&outcome),
                            status_info: None,
                        };
                        if let ReportOutcome::Accepted(entries) = outcome {
                            send_report_chunks(
                                &client,
                                &crate::clock::SystemClock,
                                request.request_id,
                                entries,
                            )
                            .await;
                        }
                        Ok(response)
                    }
                })
                .await;
            }
        }

        #[cfg(test)]
        mod tests {
            use super::*;
            use chrono::{DateTime, Utc};

            #[test]
            fn ocpp2_0_1_client_implements_the_report_handler_traits() {
                fn assert_impl<T: GetBaseReportHandler + GetReportHandler>() {}
                assert_impl::<OCPP2_0_1Client>();
            }

            /// A [`Clock`] that always reads a fixed, caller-chosen instant.
            #[derive(Clone)]
            struct FixedClock(DateTime<Utc>);

            impl Clock for FixedClock {
                fn now(&self) -> DateTime<Utc> {
                    self.0
                }
            }

            #[test]
            fn ocpp2_0_1_report_handler_can_be_constructed_with_a_caller_supplied_clock() {
                fn assert_ctor<
                    F: Fn(OCPP2_0_1Client, FixedClock) -> Ocpp2_0_1ReportHandler<FixedClock>,
                >(
                    _: F,
                ) {
                }
                assert_ctor(Ocpp2_0_1ReportHandler::with_clock);
            }

            #[test]
            fn a_fixed_clocks_reading_is_used_as_generated_at() {
                let fixed = DateTime::parse_from_rfc3339("2026-03-04T05:06:07Z")
                    .unwrap()
                    .with_timezone(&Utc);
                let clock = FixedClock(fixed);

                assert_eq!(generated_at(&clock), fixed.to_rfc3339());
            }

            #[test]
            fn an_unsynchronized_clocks_reading_is_still_used_not_substituted_or_dropped() {
                let unset_rtc = FixedClock(DateTime::<Utc>::from_timestamp(0, 0).unwrap());
                assert!(!is_synchronized(&unset_rtc.now()));

                assert_eq!(generated_at(&unset_rtc), unset_rtc.0.to_rfc3339());
            }
        }
    }
}
