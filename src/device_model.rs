//! Provisioning's Component/Variable device model functional block: CSMS-initiated
//! `GetVariables`/`SetVariables`. See `docs/ROADMAP.md` §2.
//!
//! OCPP 1.6J has no Component/Variable device model at all - its `ocpp_1_6` submodule instead
//! projects onto 1.6J's flat `GetConfiguration`/`ChangeConfiguration` pair via a documented
//! flat-key naming convention; see that submodule's docs for the convention and its limits.

use crate::actor::ChargePointActor;
use crate::hardware::{CAPABILITY_GATES, Capabilities, KeyStore};
use crate::security_profile::BasicAuthPassword;
use crate::state::{
    ChargePointEvent, Component, DeviceModel, DeviceModelEvent, SecurityEvent, SecurityEventType,
    Variable, VariableAttribute, VariableAttributeType, VariableCharacteristics, VariableDataType,
    VariableMutability,
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
/// One OCPP 2.x **required** variable that belongs to a capability-gated component - see
/// [`CAPABILITY_GATED_VARIABLES`].
struct CapabilityGatedVariable {
    /// The `*Ctrlr` component, matching a [`CAPABILITY_GATES`] row's `ctrlr_component`.
    component: &'static str,
    /// The variable name.
    variable: &'static str,
    /// The variable's instance, where OCPP defines one.
    instance: Option<&'static str>,
    /// The variable's data type.
    data_type: VariableDataType,
    /// The value to register.
    value: &'static str,
    /// What OCPP says about writing it - **not** necessarily what a CSMS gets. See
    /// [`Self::honoured`], which can narrow it.
    mutability: VariableMutability,
    /// Whether this build makes the value mean anything
    /// (`docs/OCPP-2.1-COMPLIANCE-ROADMAP.md` CV14) - the same question
    /// [`DefaultVariable::honoured`](crate::state::DefaultVariable) asks of the other table, asked
    /// here too because it was never asked when this one was written.
    ///
    /// **`false` forces the registration to `ReadOnly`, whatever [`Self::mutability`] says**, so a
    /// `SetVariables` is `Rejected` rather than accepted and discarded (B05.FR.09). Two different
    /// facts make a writable row unhonoured, and both get the same refusal:
    ///
    /// - **Decorative** - nothing reads the value. A CSMS setting
    ///   `SmartChargingCtrlr.LimitChangeSignificance` to 20% was told the threshold took, and the
    ///   station went on reporting every composed change however small.
    /// - **Station-written** - the value is a fact about the hardware that this crate keeps in
    ///   step itself, so a CSMS write survives only until the next sweep overwrites it. The five
    ///   `PaymentCtrlr.Merchant` instances are `crate::payment`'s to fill in, exactly as
    ///   `ClockCtrlr.DateTime` is the clock's (CV1.2 settled that one the same way).
    ///
    /// On a row OCPP already makes `ReadOnly` the field changes nothing and records the same
    /// distinction the other table's read-only rows record: `true` where this build enforces the
    /// value or keeps it in step with the fact it reports, `false` where it is a placeholder
    /// registered so the component is complete. `WriteOnly` is not reachable by this lever at all
    /// - see [`REFUSED_WRITE_ONLY_VARIABLES`].
    honoured: bool,
}

/// The OCPP 2.x variables the vendored 2.1 appendix marks **Required** for components this crate
/// only has when a capability is present (`docs/PRODUCTION-ROADMAP.md` B1.7).
///
/// Registered only when [`CAPABILITY_GATES`] says that capability is on, which is exactly C3's
/// rule: *a build without Smart Charging owes no `SmartChargingCtrlr` variables*. A charge point
/// that declares the capability answers every required variable for it; one that doesn't declares
/// the component unavailable and registers nothing beyond that, rather than advertising
/// configuration for a block it cannot run.
///
/// `PaymentCtrlr` (B7.2) owns 22 of those required rows and is registered below, gated by
/// [`crate::hardware::Capabilities::payment`]; `ISO15118Ctrlr` owns exactly one (B4.5), gated by
/// [`crate::hardware::Capabilities::iso15118_support`] being anything but
/// [`Iso15118SupportLevel::None`](crate::hardware::Iso15118SupportLevel::None). Components whose
/// blocks still don't exist at all - `WebPaymentsCtrlr`, `DCDERCtrlr`/`ACDERCtrlr`,
/// `V2XChargingCtrlr`, `NetworkConfiguration` - own the remaining 34 of the appendix's 122
/// required rows between them and appear nowhere here. That is the same rule applied
/// consistently, not an omission: their capabilities are `false`, so nothing is owed.
///
/// # Which of these this build acts on
///
/// [`CapabilityGatedVariable::honoured`] records it per row, and CV14 swept the table the way
/// CV2.1 swept `DEFAULT_VARIABLES`. Of the 26 rows OCPP makes `ReadWrite`, two are honoured -
/// `ISO15118Ctrlr`'s pair, read by `crate::authorization` before it decides how a contract
/// certificate gets validated. The other 24 are registered `ReadOnly` so the write is refused:
/// five `PaymentCtrlr.Merchant` instances the terminal owns, and 19 that nothing reads at all.
///
/// The 19 are not one problem but three, and the refusal is the honest answer to each:
///
/// - **`SmartChargingCtrlr.LimitChangeSignificance`** and **`DisplayMessageCtrlr.Language`** -
///   blocks that exist and work, with one configuration knob apiece that isn't wired up.
/// - **`TariffCostCtrlr.{Currency,TariffFallbackMessage,TotalCostFallbackMessage}`** and the seven
///   `PaymentCtrlr` settings - the parts of two blocks CV8 and CV2.11 left for later. CV8 recorded
///   them as unhonoured already; CV14 is about *refusing* them, not about honouring them.
/// - **`V2XChargingCtrlr.Enabled`** and the four `WebPaymentsCtrlr` settings - configuration for
///   blocks with no implementation behind them (Q02-Q08 and C25 respectively). A CSMS cannot
///   switch on what isn't there, and telling it otherwise is the worst available answer.
const CAPABILITY_GATED_VARIABLES: &[CapabilityGatedVariable] = &[
    CapabilityGatedVariable {
        component: "LocalAuthListCtrlr",
        variable: "Entries",
        instance: None,
        data_type: VariableDataType::Integer,
        // How many entries the list currently holds - or would, if anything kept it in step.
        // `replace_local_authorization_list` does not touch this variable, so it reads 0 however
        // many entries a `SendLocalList` installed. CV14's sweep found the claim that it did
        // (this comment used to name `ChargePointState::apply`'s `LocalListUpdated` arm) and
        // recorded the truth instead; see that roadmap row for the follow-up.
        value: "0",
        mutability: VariableMutability::ReadOnly,
        honoured: false,
    },
    // D01.FR.11: one ceiling for the whole block rather than one per message, hence no instance.
    // Enforced by `crate::message_limits` on every `SendLocalList` (CV2.8).
    CapabilityGatedVariable {
        component: "LocalAuthListCtrlr",
        variable: "ItemsPerMessage",
        instance: None,
        data_type: VariableDataType::Integer,
        value: "50",
        mutability: VariableMutability::ReadOnly,
        honoured: true,
    },
    CapabilityGatedVariable {
        component: "LocalAuthListCtrlr",
        variable: "BytesPerMessage",
        instance: None,
        data_type: VariableDataType::Integer,
        value: "8192",
        mutability: VariableMutability::ReadOnly,
        honoured: true,
    },
    CapabilityGatedVariable {
        component: "SmartChargingCtrlr",
        variable: "Entries",
        instance: Some("ChargingProfiles"),
        data_type: VariableDataType::Integer,
        // The same stale counter as `LocalAuthListCtrlr.Entries` above: nothing updates it as
        // profiles are installed or cleared, so it reads 0 for a station holding a full stack.
        value: "0",
        mutability: VariableMutability::ReadOnly,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "SmartChargingCtrlr",
        variable: "ProfileStackLevel",
        instance: None,
        data_type: VariableDataType::Integer,
        // The advisory figure the 1.6J adapter reports for `ChargeProfileMaxStackLevel`, kept in
        // step: the profile store accepts any stack level, so both are guidance rather than a
        // bound this crate enforces.
        value: "8",
        mutability: VariableMutability::ReadOnly,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "SmartChargingCtrlr",
        variable: "PeriodsPerSchedule",
        instance: None,
        data_type: VariableDataType::Integer,
        // Advisory in the same way `ProfileStackLevel` is: nothing checks a schedule's period
        // count against it (the only period-count rule in `charging_profile` is K28.FR.01's
        // "a Dynamic profile carries exactly one"), so it is a figure a CSMS can read rather
        // than one this crate enforces.
        value: "24",
        mutability: VariableMutability::ReadOnly,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "SmartChargingCtrlr",
        variable: "RateUnit",
        instance: None,
        data_type: VariableDataType::MemberList,
        // Both, genuinely - `crate::smart_charging::compose` reads whichever unit a schedule is
        // expressed in.
        value: "A,W",
        mutability: VariableMutability::ReadOnly,
        honoured: true,
    },
    CapabilityGatedVariable {
        component: "SmartChargingCtrlr",
        variable: "LimitChangeSignificance",
        instance: None,
        data_type: VariableDataType::Decimal,
        // 0: this crate reports every composed limit change to hardware, however small, rather
        // than filtering insignificant ones. Claiming a threshold it does not apply would be
        // worse than admitting there isn't one - which is also why the write is refused (CV14).
        // The appendix marks this one `Required? = yes` and OCPP makes it writable; a CSMS that
        // set it to 20% and saw `Accepted` would believe the station had stopped reporting small
        // changes, and would be reading the resulting traffic as significant.
        value: "0",
        mutability: VariableMutability::ReadWrite,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "DisplayMessageCtrlr",
        variable: "DisplayMessages",
        instance: None,
        data_type: VariableDataType::Integer,
        // How many messages are currently installed (OCPP: "Amount of different messages that
        // are currently configured ... via SetDisplayMessageRequest"). Registered at 0 and, like
        // `LocalAuthListCtrlr.Entries`, not yet kept live as messages are set/cleared - B6's
        // known gap; see `docs/PRODUCTION-ROADMAP.md`.
        value: "0",
        mutability: VariableMutability::ReadOnly,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "DisplayMessageCtrlr",
        variable: "SupportedFormats",
        instance: None,
        data_type: VariableDataType::MemberList,
        // Empty until a `hardware::Display` is registered, then overwritten with what that screen
        // can really render - `ChargePointBuilder::display_messages` does it, the same way
        // `ChargePointBuilder::payment` fills `PaymentCtrlr`'s identity. Empty is also the honest
        // answer for a charge point that never registers one: `NoDisplay::supported_formats` is
        // empty too, and `handle_set_display_message` refuses every format against it.
        value: "",
        mutability: VariableMutability::ReadOnly,
        honoured: true,
    },
    CapabilityGatedVariable {
        component: "DisplayMessageCtrlr",
        variable: "SupportedPriorities",
        instance: None,
        data_type: VariableDataType::MemberList,
        // All three, and unlike `SupportedFormats` this is a *software* fact:
        // `crate::display_message::current_message` implements the whole priority ordering
        // itself, with no help from the screen. Kept in step with `MessagePriority::ALL` by a
        // test in this module.
        value: "AlwaysFront,InFront,NormalCycle",
        mutability: VariableMutability::ReadOnly,
        honoured: true,
    },
    CapabilityGatedVariable {
        component: "DisplayMessageCtrlr",
        variable: "SupportedStates",
        instance: None,
        data_type: VariableDataType::MemberList,
        // `SupportedStates` and `Language` below are new in 2.1 and not required by 2.0.1; both
        // are registered unconditionally because the device model is protocol-version-independent
        // (`CLAUDE.md`) and a 2.0.1 CSMS simply sees a variable it did not ask about.
        //
        // The four states `MessageState` models. 2.1's `Suspended`/`Discharging` are absent
        // because a `SetDisplayMessage` naming one is refused with `NotSupportedState` - saying
        // so here is what lets a CSMS find that out before sending it. Kept in step with
        // `MessageState::ALL` by a test in this module.
        value: "Charging,Faulted,Idle,Unavailable",
        mutability: VariableMutability::ReadOnly,
        honoured: true,
    },
    CapabilityGatedVariable {
        component: "DisplayMessageCtrlr",
        variable: "Language",
        instance: None,
        data_type: VariableDataType::OptionList,
        // Empty rather than guessed, exactly like `TariffCostCtrlr.Currency` below: this crate
        // renders nothing itself and has no way to know what language the integrator's screen is
        // set to. `ReadWrite` per the spec, so a CSMS can configure the default.
        value: "",
        mutability: VariableMutability::ReadWrite,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "TariffCostCtrlr",
        variable: "Currency",
        instance: None,
        data_type: VariableDataType::String,
        // Empty until configured: a CSMS-reported `CostUpdated` carries no currency, and
        // inventing one (EUR? USD?) would put a unit on a number this crate never checks. A
        // *tariff* carries its own currency and never consults this.
        value: "",
        mutability: VariableMutability::ReadWrite,
        honoured: false,
    },
    // The two bounds `SetDefaultTariff`/`ChangeTransactionTariff` refuse against (I07.FR.02/.03,
    // I11.FR.02/.03). Both are `ReadOnly` in the appendix and both are read on the path that
    // enforces them - see `crate::tariff::max_price_elements`/`conditions_supported` - so unlike
    // `Currency` above they are live rather than merely registered.
    CapabilityGatedVariable {
        component: "TariffCostCtrlr",
        variable: "MaxElements",
        instance: Some("Tariff"),
        data_type: VariableDataType::Integer,
        // Kept in step with `crate::tariff::MAX_TARIFF_PRICE_ELEMENTS` by a test in that module,
        // which is where the reasoning for the number itself lives.
        value: "16",
        mutability: VariableMutability::ReadOnly,
        honoured: true,
    },
    CapabilityGatedVariable {
        component: "TariffCostCtrlr",
        variable: "ConditionsSupported",
        instance: Some("Tariff"),
        data_type: VariableDataType::Boolean,
        // True, and meant: `crate::pricing` evaluates every field of `TariffConditionsType` and
        // `TariffConditionsFixedType` that this station can observe, and treats one it cannot
        // observe as unmet rather than ignoring it.
        value: "true",
        mutability: VariableMutability::ReadOnly,
        honoured: true,
    },
    CapabilityGatedVariable {
        component: "TariffCostCtrlr",
        variable: "TariffFallbackMessage",
        instance: None,
        data_type: VariableDataType::String,
        value: "",
        mutability: VariableMutability::ReadWrite,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "TariffCostCtrlr",
        variable: "TotalCostFallbackMessage",
        instance: None,
        data_type: VariableDataType::String,
        value: "",
        mutability: VariableMutability::ReadWrite,
        honoured: false,
    },
    // `PaymentCtrlr`'s 22 required variables (B7.2), gated by `Capabilities::payment` - see
    // `docs/PRODUCTION-ROADMAP.md` B7.2's task notes for where the source CSV rows this mirrors
    // can be found (this checkout's `docs/OCPP-2.1/` is gitignored). Identity fields
    // (`VendorName`/`Model`/`SerialNumber`/`FirmwareVersion`/`TerminalID`/
    // `PaymentServiceProvider`) start as empty placeholders here, same convention as
    // `TariffCostCtrlr.Currency` above; `ChargePointBuilder::payment` overwrites them with real
    // values from a `hardware::PaymentTerminal` when one is registered.
    CapabilityGatedVariable {
        component: "PaymentCtrlr",
        variable: "Enabled",
        instance: None,
        data_type: VariableDataType::Boolean,
        value: "true",
        mutability: VariableMutability::ReadWrite,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "PaymentCtrlr",
        variable: "Problem",
        instance: None,
        data_type: VariableDataType::Boolean,
        value: "false",
        mutability: VariableMutability::ReadOnly,
        honoured: true,
    },
    CapabilityGatedVariable {
        component: "PaymentCtrlr",
        variable: "AuthorizeDirectPayment",
        instance: None,
        data_type: VariableDataType::Boolean,
        // Conservative default, like every other `Capabilities`-gated default (see
        // `Capabilities::default`'s docs): a direct payment does not require an extra
        // `AuthorizeRequest` round trip unless configured to.
        value: "false",
        mutability: VariableMutability::ReadWrite,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "PaymentCtrlr",
        variable: "AuthorizationAmount",
        instance: None,
        data_type: VariableDataType::Decimal,
        value: "0",
        mutability: VariableMutability::ReadWrite,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "PaymentCtrlr",
        variable: "PaymentDetails",
        instance: None,
        data_type: VariableDataType::MemberList,
        // Unknown until the integrator configures which `idToken.additionalInfo` details their
        // terminal can actually supply - empty rather than guessed, mirroring
        // `TariffCostCtrlr.Currency` above.
        value: "",
        mutability: VariableMutability::ReadWrite,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "PaymentCtrlr",
        variable: "SettlementByCSMS",
        instance: None,
        data_type: VariableDataType::Boolean,
        // Conservative default: the terminal/charge point handles settlement itself unless told
        // otherwise.
        value: "false",
        mutability: VariableMutability::ReadWrite,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "PaymentCtrlr",
        variable: "ReceiptServerUrl",
        instance: None,
        data_type: VariableDataType::String,
        value: "",
        mutability: VariableMutability::ReadWrite,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "PaymentCtrlr",
        variable: "ReceiptByCSMS",
        instance: None,
        data_type: VariableDataType::Boolean,
        value: "false",
        mutability: VariableMutability::ReadWrite,
        honoured: false,
    },
    // The five `Merchant` instances are the station-written case of `honoured: false` (CV14): OCPP
    // makes them writable, but `crate::payment::apply_payment_terminal_status` fills all five from
    // the terminal - once when it is registered, and again on every sweep a
    // `ChargePointBuilder::payment_status_updates` configures. A CSMS write would be accepted and
    // then silently replaced by whatever the terminal last said, so it is refused instead, exactly
    // as CV1.2 settled `ClockCtrlr.DateTime`.
    CapabilityGatedVariable {
        component: "PaymentCtrlr",
        variable: "Merchant",
        instance: Some("Id"),
        data_type: VariableDataType::String,
        value: "",
        mutability: VariableMutability::ReadWrite,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "PaymentCtrlr",
        variable: "Merchant",
        instance: Some("TaxId"),
        data_type: VariableDataType::String,
        value: "",
        mutability: VariableMutability::ReadWrite,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "PaymentCtrlr",
        variable: "Merchant",
        instance: Some("Name"),
        data_type: VariableDataType::String,
        value: "",
        mutability: VariableMutability::ReadWrite,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "PaymentCtrlr",
        variable: "Merchant",
        instance: Some("Address"),
        data_type: VariableDataType::String,
        value: "",
        mutability: VariableMutability::ReadWrite,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "PaymentCtrlr",
        variable: "Merchant",
        instance: Some("City"),
        data_type: VariableDataType::String,
        value: "",
        mutability: VariableMutability::ReadWrite,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "PaymentCtrlr",
        variable: "TerminalID",
        instance: None,
        data_type: VariableDataType::String,
        value: "",
        mutability: VariableMutability::ReadOnly,
        honoured: true,
    },
    CapabilityGatedVariable {
        component: "PaymentCtrlr",
        variable: "PaymentServiceProvider",
        instance: None,
        data_type: VariableDataType::String,
        value: "",
        mutability: VariableMutability::ReadOnly,
        honoured: true,
    },
    CapabilityGatedVariable {
        component: "PaymentCtrlr",
        variable: "VendorName",
        instance: None,
        data_type: VariableDataType::String,
        value: "",
        mutability: VariableMutability::ReadOnly,
        honoured: true,
    },
    CapabilityGatedVariable {
        component: "PaymentCtrlr",
        variable: "Model",
        instance: None,
        data_type: VariableDataType::String,
        value: "",
        mutability: VariableMutability::ReadOnly,
        honoured: true,
    },
    CapabilityGatedVariable {
        component: "PaymentCtrlr",
        variable: "SerialNumber",
        instance: None,
        data_type: VariableDataType::String,
        value: "",
        mutability: VariableMutability::ReadOnly,
        honoured: true,
    },
    CapabilityGatedVariable {
        component: "PaymentCtrlr",
        variable: "FirmwareVersion",
        instance: None,
        data_type: VariableDataType::String,
        value: "",
        mutability: VariableMutability::ReadOnly,
        honoured: true,
    },
    CapabilityGatedVariable {
        component: "PaymentCtrlr",
        variable: "IMSI",
        instance: None,
        data_type: VariableDataType::String,
        value: "",
        mutability: VariableMutability::ReadOnly,
        honoured: true,
    },
    CapabilityGatedVariable {
        component: "PaymentCtrlr",
        variable: "ICCID",
        instance: None,
        data_type: VariableDataType::String,
        value: "",
        mutability: VariableMutability::ReadOnly,
        honoured: true,
    },
    CapabilityGatedVariable {
        component: "PaymentCtrlr",
        variable: "Connected",
        instance: None,
        data_type: VariableDataType::Boolean,
        value: "false",
        mutability: VariableMutability::ReadOnly,
        honoured: true,
    },
    // `ISO15118Ctrlr`'s required variable (B4.5) and the one optional one this crate can act on
    // (B4.6), both gated by `Capabilities::iso15118_support`. The appendix lists fifteen further
    // optional variables (`SeccId`, `PnCEnabled`, `ProtocolSupported`, ...) which describe the
    // *HLC stack*, living behind `hardware::Iso15118Controller` rather than in this crate -
    // registering guesses at them would advertise a session state machine this crate does not own
    // (see `crate::iso15118`'s "what this crate does and does not do").
    CapabilityGatedVariable {
        component: "ISO15118Ctrlr",
        variable: "ContractValidationOffline",
        instance: None,
        data_type: VariableDataType::Boolean,
        // `false`, and defensibly so: validating a contract certificate while offline means
        // parsing and path-checking it locally, which needs the HLC stack and a V2G trust chain
        // this crate has neither of - it forwards the EXI blob to the CSMS and relays the answer
        // back. An integrator whose controller *can* do it has the CSMS flip this on, which is
        // why the spec's `ReadWrite` mutability is kept rather than narrowed to `ReadOnly`:
        // OCPP 2.1 Part 2 §2.15.3 defines it as a configuration variable, and a station that
        // refused the write would misreport a stack limitation as a spec deviation.
        value: "false",
        mutability: VariableMutability::ReadWrite,
        honoured: true,
    },
    CapabilityGatedVariable {
        component: "ISO15118Ctrlr",
        variable: "CentralContractValidationAllowed",
        instance: None,
        data_type: VariableDataType::Boolean,
        // Optional in the appendix, but registered because this crate *acts* on it: it is the
        // C07.FR.06 switch deciding whether a contract certificate chain this charge point cannot
        // validate may be forwarded to the CSMS to validate instead (see
        // `crate::authorization`'s contract-authorization path).
        //
        // `false`, conservatively, like every other capability-gated default - but note what that
        // costs here: this crate can *never* validate a contract certificate locally, so a
        // station left at `false` asks the CSMS to decide from the OCSP data alone. Turning it on
        // is how an operator enables full Plug & Charge; the withheld chain is logged each time
        // so the misconfiguration is visible rather than silent.
        value: "false",
        mutability: VariableMutability::ReadWrite,
        honoured: true,
    },
    // --- CV1.6: the last of OCPP's required capability-gated variables ---
    //
    // Each is `Required? = yes` in the 2.1 appendix for a component this crate only advertises
    // when the matching capability is declared, so they arrive with it and not before.
    //
    // **Every value is empty or zero, and that is the point.** These are hardware nameplate
    // figures and deployment settings - an inverter's manufacturer and power limits, which DER
    // modes the hardware implements, the QR-code URL a driver is sent to. A plausible-looking
    // invention would be worse than an obviously-unset value a CSMS can see it must configure.
    //
    // `SharedSecret` is `WriteOnly` for the same reason `NetworkConfiguration.BasicAuthPassword`
    // is (CV1.3): a secret `GetVariables` can read is not a secret.
    //
    // The two `TariffCostCtrlr` messages are what a driver is shown when no tariff or no running
    // cost is available (I04/I05). OCPP instances them per language; the unkeyed instance is the
    // fallback for any language, which is the only one this crate can offer without a
    // localisation story.
    CapabilityGatedVariable {
        component: "TariffCostCtrlr",
        variable: "TariffFallbackMessage",
        instance: None,
        data_type: VariableDataType::String,
        value: "",
        mutability: VariableMutability::ReadWrite,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "TariffCostCtrlr",
        variable: "TotalCostFallbackMessage",
        instance: None,
        data_type: VariableDataType::String,
        value: "",
        mutability: VariableMutability::ReadWrite,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "V2XChargingCtrlr",
        variable: "Enabled",
        instance: None,
        data_type: VariableDataType::Boolean,
        value: "false",
        mutability: VariableMutability::ReadWrite,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "V2XChargingCtrlr",
        variable: "SupportedOperationModes",
        instance: None,
        data_type: VariableDataType::MemberList,
        value: "",
        mutability: VariableMutability::ReadOnly,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "V2XChargingCtrlr",
        variable: "SupportedEnergyTransferModes",
        instance: None,
        data_type: VariableDataType::MemberList,
        value: "",
        mutability: VariableMutability::ReadOnly,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "ACDERCtrlr",
        variable: "ModesSupported",
        instance: None,
        data_type: VariableDataType::MemberList,
        value: "",
        mutability: VariableMutability::ReadOnly,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "DCDERCtrlr",
        variable: "InverterManufacturer",
        instance: None,
        data_type: VariableDataType::String,
        value: "",
        mutability: VariableMutability::ReadOnly,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "DCDERCtrlr",
        variable: "InverterModel",
        instance: None,
        data_type: VariableDataType::String,
        value: "",
        mutability: VariableMutability::ReadOnly,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "DCDERCtrlr",
        variable: "InverterSwVersion",
        instance: None,
        data_type: VariableDataType::String,
        value: "",
        mutability: VariableMutability::ReadOnly,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "DCDERCtrlr",
        variable: "InverterHwVersion",
        instance: None,
        data_type: VariableDataType::String,
        value: "",
        mutability: VariableMutability::ReadOnly,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "DCDERCtrlr",
        variable: "ModesSupported",
        instance: None,
        data_type: VariableDataType::MemberList,
        value: "",
        mutability: VariableMutability::ReadOnly,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "DCDERCtrlr",
        variable: "MaxW",
        instance: None,
        data_type: VariableDataType::Decimal,
        value: "",
        mutability: VariableMutability::ReadOnly,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "DCDERCtrlr",
        variable: "MaxVA",
        instance: None,
        data_type: VariableDataType::Decimal,
        value: "",
        mutability: VariableMutability::ReadOnly,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "DCDERCtrlr",
        variable: "MaxVar",
        instance: None,
        data_type: VariableDataType::Decimal,
        value: "",
        mutability: VariableMutability::ReadOnly,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "DCDERCtrlr",
        variable: "MaxVarNeg",
        instance: None,
        data_type: VariableDataType::Decimal,
        value: "",
        mutability: VariableMutability::ReadOnly,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "DCDERCtrlr",
        variable: "MaxChargeRateW",
        instance: None,
        data_type: VariableDataType::Decimal,
        value: "",
        mutability: VariableMutability::ReadOnly,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "DCDERCtrlr",
        variable: "MaxChargeRateVA",
        instance: None,
        data_type: VariableDataType::Decimal,
        value: "",
        mutability: VariableMutability::ReadOnly,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "DCDERCtrlr",
        variable: "OverExcitedPF",
        instance: None,
        data_type: VariableDataType::Decimal,
        value: "",
        mutability: VariableMutability::ReadOnly,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "DCDERCtrlr",
        variable: "OverExcitedW",
        instance: None,
        data_type: VariableDataType::Decimal,
        value: "",
        mutability: VariableMutability::ReadOnly,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "DCDERCtrlr",
        variable: "UnderExcitedPF",
        instance: None,
        data_type: VariableDataType::Decimal,
        value: "",
        mutability: VariableMutability::ReadOnly,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "DCDERCtrlr",
        variable: "UnderExcitedW",
        instance: None,
        data_type: VariableDataType::Decimal,
        value: "",
        mutability: VariableMutability::ReadOnly,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "DCDERCtrlr",
        variable: "ReactiveSusceptance",
        instance: None,
        data_type: VariableDataType::Decimal,
        value: "",
        mutability: VariableMutability::ReadOnly,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "WebPaymentsCtrlr",
        variable: "URLTemplate",
        instance: None,
        data_type: VariableDataType::String,
        value: "",
        mutability: VariableMutability::ReadWrite,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "WebPaymentsCtrlr",
        variable: "SharedSecret",
        instance: None,
        data_type: VariableDataType::String,
        value: "",
        mutability: VariableMutability::WriteOnly,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "WebPaymentsCtrlr",
        variable: "TOTPVersion",
        instance: None,
        data_type: VariableDataType::String,
        value: "",
        mutability: VariableMutability::ReadWrite,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "WebPaymentsCtrlr",
        variable: "Length",
        instance: None,
        data_type: VariableDataType::Integer,
        value: "0",
        mutability: VariableMutability::ReadWrite,
        honoured: false,
    },
    CapabilityGatedVariable {
        component: "WebPaymentsCtrlr",
        variable: "ValidityTime",
        instance: None,
        data_type: VariableDataType::Integer,
        value: "0",
        mutability: VariableMutability::ReadWrite,
        honoured: false,
    },
    // --- CV1.4: MonitoringCtrlr's required message-size variables ---
    //
    // Both are `Required? = yes` in the 2.1 appendix for a component this crate advertises
    // whenever `variable_monitoring` is declared, so they belong here rather than in
    // `DEFAULT_VARIABLES` - a build with the block compiled out owes neither.
    //
    // The figures match `DeviceDataCtrlr`'s (50 items, 8 KiB), which is the same question asked
    // about a different message. Enforced since CV2.8: a `SetVariableMonitoring` over either
    // ceiling is refused with `OccurrenceConstraintViolation`/`FormatViolation` (N04.FR.09) rather
    // than attempted - see `crate::message_limits`.
    CapabilityGatedVariable {
        component: "MonitoringCtrlr",
        variable: "ItemsPerMessage",
        instance: Some("SetVariableMonitoring"),
        data_type: VariableDataType::Integer,
        value: "50",
        mutability: VariableMutability::ReadOnly,
        honoured: true,
    },
    CapabilityGatedVariable {
        component: "MonitoringCtrlr",
        variable: "BytesPerMessage",
        instance: Some("SetVariableMonitoring"),
        data_type: VariableDataType::Integer,
        value: "8192",
        mutability: VariableMutability::ReadOnly,
        honoured: true,
    },
];

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
///
/// Since B1.7 this also brings each gated component's **required** variables (see this module's
/// `CAPABILITY_GATED_VARIABLES`), which is the same rule applied one level deeper: a capability
/// that is off owes no configuration, only an honest `Available: false`.
pub fn capability_gate_events(capabilities: &Capabilities) -> Vec<ChargePointEvent> {
    let gated = CAPABILITY_GATES.iter().flat_map(|gate| {
        let available = (gate.enabled)(capabilities);
        CAPABILITY_GATED_VARIABLES
            .iter()
            .filter(move |variable| available && gate.ctrlr_component == Some(variable.component))
            .map(|variable| {
                ChargePointEvent::DeviceModel(DeviceModelEvent::VariableRegistered {
                    component: Component {
                        name: variable.component.to_string(),
                        instance: None,
                        evse: None,
                    },
                    variable: Variable {
                        name: variable.variable.to_string(),
                        instance: variable.instance.map(ToString::to_string),
                    },
                    characteristics: VariableCharacteristics {
                        data_type: variable.data_type,
                        unit: None,
                        min_limit: None,
                        max_limit: None,
                        values_list: None,
                        supports_monitoring: false,
                    },
                    attributes: vec![VariableAttribute {
                        attribute_type: VariableAttributeType::Actual,
                        value: variable.value.to_string(),
                        // CV14, the same narrowing `DeviceModel::register_defaults` applies to
                        // `DEFAULT_VARIABLES`: a variable this build does not act on is
                        // registered read-only, so a `SetVariables` on it is `Rejected`
                        // (B05.FR.09) rather than accepted and ignored. See
                        // `CapabilityGatedVariable::honoured`.
                        mutability: if variable.honoured {
                            variable.mutability
                        } else {
                            VariableMutability::ReadOnly
                        },
                        persistent: false,
                        constant: false,
                        requires_reboot: false,
                    }],
                })
            })
    });

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
        .chain(gated)
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

/// The `WriteOnly` variables a `SetVariables` is refused on outright, because this build cannot
/// act on the value it would be handed at all (`docs/OCPP-2.1-COMPLIANCE-ROADMAP.md` CV10).
///
/// `DefaultVariable::honoured` (CV2.1) and [`CapabilityGatedVariable::honoured`] (CV14) each force
/// an unhonoured variable in their own table to
/// `ReadOnly`, which is how `SetVariables` comes to refuse it (B05.FR.09). That lever does not
/// reach `WriteOnly` variables: OCPP requires `NetworkConfiguration.BasicAuthPassword` to be
/// reported `WriteOnly` (B09.FR.10), and this crate registers `WebPaymentsCtrlr.SharedSecret` the
/// same way, for the reason both exist - a secret `GetVariables` can read is not a secret. So a
/// blanket refusal for a `WriteOnly` variable this build genuinely cannot honour is spelled here
/// instead of in the mutability.
///
/// **`BasicAuthPassword` is no longer in this table.** CV10's write is real now -
/// [`resolve_and_apply_set`] special-cases it before ever reaching this list, validating with
/// [`BasicAuthPassword::new`] and persisting through `hardware::KeyStore` (see
/// `crate::basic_auth_credential`) rather than refusing outright. `WebPaymentsCtrlr.SharedSecret`
/// stays refused: it is a different item (Web Payments TOTP, not HTTP Basic) with no consumer at
/// all yet, so the same two reasons the old `BasicAuthPassword` entry gave still apply to it -
/// accepting it would put a credential in `ChargePointState` (which `trace!` prints whole), and a
/// CSMS that saw `Accepted` for a rotation it could not actually carry would have no way back in.
const REFUSED_WRITE_ONLY_VARIABLES: &[(&str, &str)] = &[("WebPaymentsCtrlr", "SharedSecret")];

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
///
/// `key_store` is where a `NetworkConfiguration.BasicAuthPassword` rotation is persisted (CV10) -
/// see [`resolve_and_apply_set`]. Every other variable ignores it entirely; a caller with no real
/// `hardware::KeyStore` to offer (or that deliberately doesn't want rotation applied) passes
/// [`crate::hardware::NoKeyStore`], which makes that one write fail exactly as it did before CV10
/// closed this gap - `Rejected`, not a silent no-op.
#[tracing::instrument(skip_all)]
pub async fn handle_set_variables<K: KeyStore>(
    actor: &ChargePointActor,
    requests: Vec<SetVariableRequest>,
    key_store: &K,
) -> Vec<SetVariableOutcome> {
    let mut outcomes = Vec::with_capacity(requests.len());
    for request in requests {
        outcomes.push(resolve_and_apply_set(actor, &request, key_store).await);
    }
    outcomes
}

/// Resolves a single component/variable/attribute-type write against `actor`'s current device
/// model and, if accepted, applies it. Mirrors [`resolve_get`]'s unknown-component/-variable/
/// -attribute-type priority, additionally rejecting a `ReadOnly` or `constant` attribute (neither
/// of which `SetVariables` may ever write), and reports `RebootRequired` instead of `Accepted`
/// when the attribute is marked as needing one.
async fn resolve_and_apply_set<K: KeyStore>(
    actor: &ChargePointActor,
    request: &SetVariableRequest,
    key_store: &K,
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
    // CV10: `NetworkConfiguration.BasicAuthPassword` never reaches `DeviceModelEvent::
    // AttributeValueSet` below - doing so would put the credential in `ChargePointState`, which
    // `trace!` prints whole (A01.FR.12) - so it is resolved and returned here, before the generic
    // path (`REFUSED_WRITE_ONLY_VARIABLES`, `validate_value`, the `AttributeValueSet` send) ever
    // sees it.
    if request.component.name == "NetworkConfiguration"
        && request.variable.name == "BasicAuthPassword"
    {
        return handle_basic_auth_password_write(
            actor,
            request.component.instance.as_deref(),
            &request.value,
            key_store,
        )
        .await;
    }
    // CV10: a credential this build could store but not use - see `REFUSED_WRITE_ONLY_VARIABLES`.
    // The log line names the variable and nothing else: not the value, and not its length either,
    // which would narrow a guess (A01.FR.12).
    if REFUSED_WRITE_ONLY_VARIABLES
        .iter()
        .any(|(component, variable)| {
            *component == request.component.name && *variable == request.variable.name
        })
    {
        tracing::warn!(
            component = %request.component.name,
            variable = %request.variable.name,
            "refusing a credential write this build cannot carry onto the connection"
        );
        return SetVariableOutcome::Rejected;
    }
    // B05.FR.07 (badly formatted) and B05.FR.08 (out of range) - CV3. Both answer `Rejected`;
    // OCPP's `statusInfo` may carry the detail and is optional, so the reason is logged rather
    // than put on the wire (see `ValueRejection`).
    if let Err(rejection) = validate_value(&definition.characteristics, &request.value) {
        tracing::warn!(
            component = %request.component.name,
            variable = %request.variable.name,
            reason = rejection.reason(),
            "refusing a SetVariables value the variable cannot hold"
        );
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

/// Resolves a `NetworkConfiguration[slot].BasicAuthPassword` write (CV10, A01.FR.02/.04/.11/.12).
///
/// `instance` is the slot the write addressed (`Component::instance`, present because
/// [`resolve_and_apply_set`] only reaches here once `state.device_model.get` has already found a
/// `NetworkConfiguration[slot]` component - which is only ever registered for an *occupied* slot,
/// so a slot number this crate has no stored profile for cannot reach this function at all).
///
/// 1. **Validate** with [`BasicAuthPassword::new`] (A00.FR.205) - this is the one check this
///    function owns; everything OCPP says about the value's shape lives there, not here.
/// 2. **Persist** through `key_store` (`crate::basic_auth_credential::rotate`), keeping the
///    password this station was using before as a fallback - never through
///    [`crate::hardware::Storage`]/`ChargePointState`, which `crate::persistence` writes as a plain
///    JSON blob and `trace!` prints whole.
/// 3. **Log** that the password changed, for which slot, and when - never the value, which never
///    reaches this function's log lines, the security log, or (per `WriteOnly`) `GetVariables`.
///
/// Applying the new password to a *live* connection, and rolling back after repeated
/// authentication failure (A01.FR.04), is [`crate::network_switch::ConnectionTarget`]'s job - it
/// reads whatever `crate::basic_auth_credential::current` returns on every redial, so persisting
/// here is sufficient for "apply on next connect" without this function reaching into the
/// transport at all.
async fn handle_basic_auth_password_write<K: KeyStore>(
    actor: &ChargePointActor,
    instance: Option<&str>,
    value: &str,
    key_store: &K,
) -> SetVariableOutcome {
    let Some(slot) = instance.and_then(|instance| instance.parse::<i32>().ok()) else {
        tracing::warn!("refusing a BasicAuthPassword write with no readable configuration slot");
        return SetVariableOutcome::Rejected;
    };
    let password = match BasicAuthPassword::new(value) {
        Ok(password) => password,
        Err(error) => {
            // A01.FR.12: the reason is a shape (too short/too long), never the value or its
            // length - length alone would still narrow a guess at the string this refused.
            tracing::warn!(
                slot,
                ?error,
                "refusing a BasicAuthPassword that fails A00.FR.205"
            );
            return SetVariableOutcome::Rejected;
        }
    };
    if let Err(error) = crate::basic_auth_credential::rotate(key_store, slot, &password).await {
        tracing::warn!(slot, %error, "could not persist a rotated BasicAuthPassword");
        return SetVariableOutcome::Rejected;
    }
    crate::security::report_security_event(
        actor,
        SecurityEvent {
            event_type: SecurityEventType::ReconfigurationOfSecurityParameters,
            tech_info: Some(alloc::format!(
                "BasicAuthPassword rotated for NetworkConfiguration slot {slot}"
            )),
        },
    )
    .await;
    SetVariableOutcome::Accepted
}

/// Why [`validate_value`] refused a `SetVariables` value. Both map to OCPP's `Rejected`
/// `attributeStatus`; the distinction is for the log line and for the tests, not for the wire -
/// B05.FR.07 and B05.FR.08 name the same status and leave the detail to an optional `statusInfo`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ValueRejection {
    /// B05.FR.07 - the value is not a well-formed instance of the variable's `data_type`.
    Malformed,
    /// B05.FR.08 - well-formed, but outside `min_limit`/`max_limit`.
    OutOfRange,
    /// B05.FR.07 - not one of the variable's `values_list` entries.
    NotAnAllowedValue,
}

impl ValueRejection {
    /// A low-cardinality `&'static str` for the log field - see CLAUDE.md on fields over prose.
    fn reason(self) -> &'static str {
        match self {
            Self::Malformed => "malformed",
            Self::OutOfRange => "out-of-range",
            Self::NotAnAllowedValue => "not-an-allowed-value",
        }
    }
}

/// Checks a `SetVariables` value against what the variable declares it can hold (CV3).
///
/// This is the check whose absence made every `SetVariables` succeed: before it,
/// `HeartbeatInterval = "banana"` answered `Accepted` and was stored verbatim, so a CSMS had no
/// way to learn that the charge point had not understood it.
///
/// What is checked, in the order a value fails:
///
/// - **Type** ([`VariableDataType`]). `Integer`/`Decimal` must parse; `Boolean` must be exactly
///   `true` or `false` (OCPP's wire spelling - not `1`/`0`, not `True`); `DateTime` must be
///   RFC 3339. `String` accepts anything, which is the point of the type.
/// - **Range** (`min_limit`/`max_limit`), for the numeric types only. OCPP overloads `max_limit`
///   to mean *maximum length* for string-shaped types, so it is applied that way there.
/// - **Membership** (`values_list`), for `OptionList` (exactly one entry), `MemberList` and
///   `SequenceList` (a comma-separated subset, every element of which must be an entry).
///
/// A characteristic that is absent constrains nothing: a variable with no `values_list` accepts
/// any string for its list type, and one with no limits accepts any number. That is deliberate -
/// this crate registers real bounds where OCPP defines them and leaves the rest open rather than
/// inventing a bound a CSMS would then be refused by.
fn validate_value(
    characteristics: &VariableCharacteristics,
    value: &str,
) -> Result<(), ValueRejection> {
    let in_range = |number: f64| -> Result<(), ValueRejection> {
        if characteristics.min_limit.is_some_and(|min| number < min)
            || characteristics.max_limit.is_some_and(|max| number > max)
        {
            return Err(ValueRejection::OutOfRange);
        }
        Ok(())
    };

    match characteristics.data_type {
        VariableDataType::Integer => {
            let parsed: i64 = value
                .trim()
                .parse()
                .map_err(|_| ValueRejection::Malformed)?;
            in_range(parsed as f64)
        }
        VariableDataType::Decimal => {
            let parsed: f64 = value
                .trim()
                .parse()
                .map_err(|_| ValueRejection::Malformed)?;
            if !parsed.is_finite() {
                return Err(ValueRejection::Malformed);
            }
            in_range(parsed)
        }
        VariableDataType::Boolean => match value {
            "true" | "false" => Ok(()),
            _ => Err(ValueRejection::Malformed),
        },
        VariableDataType::DateTime => chrono::DateTime::parse_from_rfc3339(value)
            .map(|_| ())
            .map_err(|_| ValueRejection::Malformed),
        VariableDataType::String => {
            // OCPP reads `max_limit` on a string-shaped variable as its maximum *length*.
            if characteristics
                .max_limit
                .is_some_and(|max| value.chars().count() as f64 > max)
            {
                return Err(ValueRejection::OutOfRange);
            }
            Ok(())
        }
        VariableDataType::OptionList => match &characteristics.values_list {
            Some(allowed) if !allowed.iter().any(|option| option == value) => {
                Err(ValueRejection::NotAnAllowedValue)
            }
            _ => Ok(()),
        },
        VariableDataType::MemberList | VariableDataType::SequenceList => {
            let Some(allowed) = &characteristics.values_list else {
                return Ok(());
            };
            // An empty list is a legitimate value for both - "no measurands", say - and would
            // otherwise fail as a single empty-string member.
            if value.is_empty() {
                return Ok(());
            }
            for member in value.split(',') {
                let member = member.trim();
                if !allowed.iter().any(|option| option == member) {
                    return Err(ValueRejection::NotAnAllowedValue);
                }
            }
            Ok(())
        }
    }
}

/// Registers this charge point's inbound `SetVariables` handling with the CSMS connection.
/// Implemented per protocol version (see the `ocpp_2_1` module); OCPP 1.6J projects this onto
/// `ChangeConfiguration` instead (see the `ocpp_1_6` module).
#[async_trait::async_trait]
pub trait SetVariablesHandler {
    /// Registers a `SetVariables` handler with the CSMS connection that dispatches incoming
    /// requests to [`handle_set_variables`] against `actor`, threading `key_store` through for a
    /// `NetworkConfiguration.BasicAuthPassword` rotation (CV10).
    ///
    /// `crate::builder::ChargePointBuilder::configuration`/`device_model` pass
    /// [`crate::hardware::NoKeyStore`] here for a caller who hasn't opted into rotation - see
    /// `crate::builder::ChargePointBuilder::basic_auth_password_rotation` for the one that does.
    async fn register_set_variables_handler<K: crate::hardware::KeyStore + Send + Sync + 'static>(
        &self,
        actor: ChargePointActor,
        key_store: K,
    );
}

#[cfg(test)]
mod tests {
    use super::{
        GetVariableOutcome, GetVariableRequest, SetVariableOutcome, SetVariableRequest,
        ValueRejection, handle_get_variables, handle_set_variables, validate_value,
    };
    use crate::actor::ChargePointActor;
    use crate::executor::TokioExecutor;
    use crate::hardware::{InMemoryStorage, NoKeyStore, SoftKeyStore, SoftwareCrypto};
    use crate::state::{
        ChargePointEvent, Component, DeviceModelEvent, Variable, VariableAttribute,
        VariableAttributeType, VariableCharacteristics, VariableDataType, VariableMutability,
    };
    use alloc::sync::Arc;

    /// A [`SoftwareCrypto`] that panics if ever asked to touch key material - none of this
    /// module's tests exercise `KeyStore::generate_key_pair`/`sign`, only
    /// `store_credential`/`load_credential`, so a call reaching this would mean a test wired the
    /// wrong path.
    #[derive(Debug, Default)]
    struct UnusedCrypto;
    impl SoftwareCrypto for UnusedCrypto {
        type Error = core::convert::Infallible;
        fn generate_key_pair(
            &self,
            _algorithm: crate::hardware::SignatureAlgorithm,
        ) -> Result<(alloc::vec::Vec<u8>, crate::hardware::PublicKey), Self::Error> {
            unreachable!("this test module never generates a key pair")
        }
        fn sign(
            &self,
            _algorithm: crate::hardware::SignatureAlgorithm,
            _private_key: &[u8],
            _digest: &[u8],
        ) -> Result<alloc::vec::Vec<u8>, Self::Error> {
            unreachable!("this test module never signs")
        }
        fn supported_algorithms(&self) -> &[crate::hardware::SignatureAlgorithm] {
            &[]
        }
    }

    /// A real (in-memory) `KeyStore`, for the tests proving CV10's write actually persists a
    /// rotation - as opposed to `NoKeyStore`, which every other test in this module passes to
    /// prove the ordinary paths are unaffected.
    fn key_store() -> SoftKeyStore<Arc<InMemoryStorage>, UnusedCrypto> {
        SoftKeyStore::new(Arc::new(InMemoryStorage::new()), UnusedCrypto)
    }

    /// B1.7's rule, as a test rather than a claim: a capability that is *on* brings every required
    /// variable its component owes, and one that is *off* brings none of them.
    #[test]
    fn required_variables_arrive_with_their_capability_and_only_with_it() {
        use super::{CAPABILITY_GATED_VARIABLES, capability_gate_events};
        use crate::hardware::Capabilities;
        use crate::state::{ChargePointEvent, DeviceModelEvent};
        let _ = CAPABILITY_GATED_VARIABLES;

        let registered = |capabilities: &Capabilities| -> alloc::vec::Vec<(String, String)> {
            capability_gate_events(capabilities)
                .into_iter()
                .filter_map(|event| match event {
                    ChargePointEvent::DeviceModel(DeviceModelEvent::VariableRegistered {
                        component,
                        variable,
                        ..
                    }) => Some((component.name, variable.name)),
                    _ => None,
                })
                .collect()
        };

        // Nothing declared: every `*Ctrlr.Available` still says so (that is C3.4's job), but no
        // component's required configuration comes with it.
        let none = registered(&Capabilities::default());
        assert!(
            none.iter().all(|(_, variable)| variable == "Available"),
            "a charge point declaring no capabilities owes no capability-gated configuration"
        );

        // Smart charging declared: its five required variables arrive, and no other component's.
        let smart = registered(&Capabilities {
            smart_charging: true,
            ..Capabilities::default()
        });
        for variable in [
            "Entries",
            "ProfileStackLevel",
            "PeriodsPerSchedule",
            "RateUnit",
            "LimitChangeSignificance",
        ] {
            assert!(
                smart
                    .iter()
                    .any(|(component, name)| component == "SmartChargingCtrlr" && name == variable),
                "SmartChargingCtrlr.{variable} should arrive with the capability"
            );
        }
        assert_eq!(
            smart
                .iter()
                .filter(|(component, _)| component == "TariffCostCtrlr")
                .count(),
            1,
            "only TariffCostCtrlr.Available - a build without tariff support owes no tariff \
             configuration"
        );
    }

    /// `MonitoringCtrlr` owes the two message-size variables the 2.1 appendix marks required
    /// (CV1.4). They are `Required? = yes` on a component this crate only advertises when the
    /// capability is declared, so they arrive with it and not before.
    #[test]
    fn the_required_monitoring_message_size_variables_arrive_with_the_capability() {
        use super::capability_gate_events;
        use crate::hardware::Capabilities;
        use crate::state::{ChargePointEvent, DeviceModelEvent};

        let registered =
            |capabilities: &Capabilities| -> alloc::vec::Vec<(String, Option<String>)> {
                capability_gate_events(capabilities)
                    .into_iter()
                    .filter_map(|event| match event {
                        ChargePointEvent::DeviceModel(DeviceModelEvent::VariableRegistered {
                            component,
                            variable,
                            ..
                        }) if component.name == "MonitoringCtrlr" => {
                            Some((variable.name, variable.instance))
                        }
                        _ => None,
                    })
                    .collect()
            };

        let with = registered(&Capabilities {
            variable_monitoring: true,
            ..Capabilities::default()
        });
        for name in ["ItemsPerMessage", "BytesPerMessage"] {
            assert!(
                with.iter().any(|(variable, instance)| {
                    variable == name && instance.as_deref() == Some("SetVariableMonitoring")
                }),
                "MonitoringCtrlr.{name}[SetVariableMonitoring] should arrive with the capability, \
                 got {with:?}"
            );
        }

        let without = registered(&Capabilities::default());
        assert!(
            without.iter().all(|(variable, _)| variable == "Available"),
            "a build without variable monitoring owes no monitoring configuration, got {without:?}"
        );
    }

    /// `DisplayMessageCtrlr` owes five required variables, not one: B1.7 registered
    /// `DisplayMessages` and 2.1 added `SupportedStates`/`Language` to the three
    /// `Supported*` lists a CSMS needs before it can compose a `SetDisplayMessage` this charge
    /// point will accept.
    #[test]
    fn every_required_display_message_variable_arrives_with_the_capability() {
        use super::capability_gate_events;
        use crate::hardware::Capabilities;
        use crate::state::{ChargePointEvent, DeviceModelEvent};

        let registered: alloc::vec::Vec<String> = capability_gate_events(&Capabilities {
            has_display: true,
            ..Capabilities::default()
        })
        .into_iter()
        .filter_map(|event| match event {
            ChargePointEvent::DeviceModel(DeviceModelEvent::VariableRegistered {
                component,
                variable,
                ..
            }) if component.name == "DisplayMessageCtrlr" => Some(variable.name),
            _ => None,
        })
        .collect();

        for required in [
            "DisplayMessages",
            "SupportedFormats",
            "SupportedPriorities",
            "SupportedStates",
            "Language",
        ] {
            assert!(
                registered.iter().any(|name| name == required),
                "DisplayMessageCtrlr.{required} is required by the 2.1 appendix and should \
                 arrive with `has_display`, got {registered:?}"
            );
        }
    }

    /// The `Supported*` member lists must name exactly what this crate models - a hand-written
    /// string that drifts from the enum would advertise a state or priority a
    /// `SetDisplayMessage` is then refused for (or, worse, hide one that works).
    #[test]
    fn the_supported_member_lists_match_what_this_crate_models() {
        use crate::state::{MessagePriority, MessageState};

        let value = |variable: &str| {
            super::CAPABILITY_GATED_VARIABLES
                .iter()
                .find(|entry| {
                    entry.component == "DisplayMessageCtrlr" && entry.variable == variable
                })
                .unwrap_or_else(|| panic!("DisplayMessageCtrlr.{variable} should be registered"))
                .value
        };

        assert_eq!(
            value("SupportedPriorities"),
            MessagePriority::ALL
                .iter()
                .map(|priority| priority.name())
                .collect::<alloc::vec::Vec<_>>()
                .join(","),
        );
        assert_eq!(
            value("SupportedStates"),
            MessageState::ALL
                .iter()
                .map(|state| state.name())
                .collect::<alloc::vec::Vec<_>>()
                .join(","),
        );
    }

    /// B1.7's rule applied to the one capability that is an *enum* rather than a bool: either
    /// ISO 15118 support level brings `ISO15118Ctrlr`'s required configuration,
    /// `Iso15118SupportLevel::None` brings none of it - see `crate::iso15118`'s capability-gating
    /// docs for why the level does not change the answer.
    #[test]
    fn iso15118_required_variables_arrive_with_either_support_level() {
        use super::capability_gate_events;
        use crate::hardware::{Capabilities, Iso15118SupportLevel};
        use crate::state::{ChargePointEvent, DeviceModelEvent};

        let registered = |level: Iso15118SupportLevel| -> alloc::vec::Vec<(String, String)> {
            capability_gate_events(&Capabilities::default().with_iso15118_support(level))
                .into_iter()
                .filter_map(|event| match event {
                    ChargePointEvent::DeviceModel(DeviceModelEvent::VariableRegistered {
                        component,
                        variable,
                        ..
                    }) => Some((component.name, variable.name)),
                    _ => None,
                })
                .filter(|(component, _)| component == "ISO15118Ctrlr")
                .collect()
        };

        for level in [
            Iso15118SupportLevel::Iso15118_2,
            Iso15118SupportLevel::Iso15118_20,
        ] {
            for variable in [
                // B4.5: the component's one *required* variable.
                "ContractValidationOffline",
                // B4.6: optional in the appendix, but the switch the contract-authorization path
                // reads before forwarding a chain to the CSMS - a variable this crate acts on
                // must be one a CSMS can see and set.
                "CentralContractValidationAllowed",
            ] {
                assert!(
                    registered(level).iter().any(|(_, name)| name == variable),
                    "ISO15118Ctrlr.{variable} should arrive with {level:?}"
                );
            }
        }

        // No support declared: `Available: false` and nothing else - a charge point with no PLC
        // modem owes no ISO 15118 configuration.
        assert_eq!(
            registered(Iso15118SupportLevel::None),
            alloc::vec![("ISO15118Ctrlr".to_string(), "Available".to_string())],
        );
    }

    /// Every capability this crate gates a variable behind, all on at once - so a sweep over the
    /// gated table registers every row rather than silently skipping the ones behind a capability
    /// the test forgot. A row this misses fails
    /// `an_unhonoured_gated_variable_reaches_the_device_model_read_only` as "never registered"
    /// rather than passing vacuously.
    fn all_capabilities() -> crate::hardware::Capabilities {
        crate::hardware::Capabilities {
            has_display: true,
            local_auth_list: true,
            smart_charging: true,
            variable_monitoring: true,
            tariff_and_cost: true,
            payment: true,
            der_control: true,
            supports_bidirectional_power: true,
            iso15118_support: crate::hardware::Iso15118SupportLevel::Iso15118_20,
            ..Default::default()
        }
    }

    /// An actor with every gated component registered, so a `SetVariables` against one resolves
    /// against a real registration rather than `UnknownComponent`.
    async fn actor_with_every_capability() -> ChargePointActor {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let capabilities = all_capabilities();
        actor
            .send(crate::state::ChargePointEvent::CapabilitiesDeclared(
                capabilities,
            ))
            .await
            .unwrap();
        for event in super::capability_gate_events(&capabilities) {
            actor.send(event).await.unwrap();
        }
        actor
    }

    /// CV14, the structural half: a gated variable this build does not act on must reach the
    /// device model `ReadOnly`, whatever OCPP says about writing it - the same lever CV2.1 pulls
    /// on `DEFAULT_VARIABLES`, applied to the second table. Asserted over the events rather than
    /// the table alone, because the registration is where the narrowing either happens or doesn't.
    #[test]
    fn an_unhonoured_gated_variable_reaches_the_device_model_read_only() {
        use super::{CAPABILITY_GATED_VARIABLES, capability_gate_events};
        use crate::state::{ChargePointEvent, DeviceModelEvent};

        let registered: alloc::vec::Vec<_> = capability_gate_events(&all_capabilities())
            .into_iter()
            .filter_map(|event| match event {
                ChargePointEvent::DeviceModel(DeviceModelEvent::VariableRegistered {
                    component,
                    variable,
                    attributes,
                    ..
                }) => Some((component.name, variable.name, variable.instance, attributes)),
                _ => None,
            })
            .collect();

        // Driven from the table rather than from the events, so a row behind a capability
        // `all_capabilities` forgot fails here as "never registered" instead of being skipped -
        // the failure mode a loop over the events would have hidden.
        for row in CAPABILITY_GATED_VARIABLES
            .iter()
            .filter(|row| !row.honoured && row.mutability != VariableMutability::ReadOnly)
        {
            let mut found = false;
            for (component, variable, instance, attributes) in &registered {
                if component != row.component
                    || variable != row.variable
                    || instance.as_deref() != row.instance
                {
                    continue;
                }
                found = true;
                for attribute in attributes {
                    assert_eq!(
                        attribute.mutability,
                        VariableMutability::ReadOnly,
                        "{}.{} is not honoured, so a CSMS must not be told it can write it",
                        row.component,
                        row.variable,
                    );
                }
            }
            assert!(
                found,
                "{}.{} never registered - `all_capabilities` is missing the capability that gates \
                 it, so this test proves nothing about it",
                row.component, row.variable,
            );
        }
    }

    /// CV14, the behavioural half: `SmartChargingCtrlr.LimitChangeSignificance` is the row with a
    /// consequence beyond the false `Accepted` - a CSMS setting it to 20% was told the threshold
    /// took, and the station went on reporting every composed change however small.
    #[tokio::test]
    async fn setting_a_decorative_gated_variable_is_refused_rather_than_silently_accepted() {
        let actor = actor_with_every_capability().await;

        let outcomes = handle_set_variables(
            &actor,
            alloc::vec![SetVariableRequest {
                component: component("SmartChargingCtrlr"),
                variable: variable("LimitChangeSignificance"),
                attribute_type: VariableAttributeType::Actual,
                value: "20".into(),
            }],
            &NoKeyStore,
        )
        .await;

        assert_eq!(outcomes, alloc::vec![SetVariableOutcome::Rejected]);
    }

    /// The station-written half of the same rule, and the reason it is a separate test: OCPP makes
    /// `PaymentCtrlr.Merchant[Name]` writable, but `crate::payment`'s status poll overwrites it
    /// from the terminal on every sweep. Accepting the write and clobbering it minutes later is
    /// the `ClockCtrlr.DateTime` situation CV1.2 already settled (B05.FR.09).
    #[tokio::test]
    async fn setting_a_station_written_gated_variable_is_refused() {
        let actor = actor_with_every_capability().await;

        let outcomes = handle_set_variables(
            &actor,
            alloc::vec![SetVariableRequest {
                component: component("PaymentCtrlr"),
                variable: Variable {
                    name: "Merchant".into(),
                    instance: Some("Name".into()),
                },
                attribute_type: VariableAttributeType::Actual,
                value: "A Charging Company".into(),
            }],
            &NoKeyStore,
        )
        .await;

        assert_eq!(outcomes, alloc::vec![SetVariableOutcome::Rejected]);
    }

    /// The other side of the contract, exactly as CV2.1 has it for the defaults: CV14 must not be
    /// satisfiable by freezing the whole gated table. `ISO15118Ctrlr.ContractValidationOffline` is
    /// read by `crate::authorization` before it validates a contract certificate offline, so it
    /// stays writable.
    #[tokio::test]
    async fn a_gated_variable_this_build_acts_on_is_still_writable() {
        let actor = actor_with_every_capability().await;

        let outcomes = handle_set_variables(
            &actor,
            alloc::vec![SetVariableRequest {
                component: component("ISO15118Ctrlr"),
                variable: variable("ContractValidationOffline"),
                attribute_type: VariableAttributeType::Actual,
                value: "true".into(),
            }],
            &NoKeyStore,
        )
        .await;

        assert_eq!(outcomes, alloc::vec![SetVariableOutcome::Accepted]);
    }

    /// Pins the sweep's own arithmetic, the way CV2.1's "30 of 49" pins the defaults': a new
    /// writable row cannot be added without someone answering whether this build acts on it,
    /// because the count moves and this test fails.
    #[test]
    fn the_gated_table_records_which_writable_rows_this_build_acts_on() {
        let writable = super::CAPABILITY_GATED_VARIABLES
            .iter()
            .filter(|row| row.mutability == VariableMutability::ReadWrite);

        assert_eq!(writable.clone().count(), 26);
        assert_eq!(
            writable
                .filter(|row| row.honoured)
                .map(|row| row.variable)
                .collect::<alloc::vec::Vec<_>>(),
            alloc::vec![
                "ContractValidationOffline",
                "CentralContractValidationAllowed"
            ],
        );
    }

    /// Every entry in `CAPABILITY_GATED_VARIABLES` must name a component some `CAPABILITY_GATES`
    /// row actually gates - otherwise it is dead data that never registers, indistinguishable
    /// from a typo.
    #[test]
    fn every_capability_gated_variable_belongs_to_a_real_gate() {
        for variable in super::CAPABILITY_GATED_VARIABLES {
            assert!(
                crate::hardware::CAPABILITY_GATES
                    .iter()
                    .any(|gate| gate.ctrlr_component == Some(variable.component)),
                "`{}` is not a component any capability gates",
                variable.component
            );
        }
    }

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
            &NoKeyStore,
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
            &NoKeyStore,
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
            &NoKeyStore,
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

    // --- CV3: B05.FR.07 (malformed) and B05.FR.08 (out of range) ---

    /// The case that made this whole workstream necessary: before CV3, this answered `Accepted`
    /// and stored `"banana"` as the heartbeat interval, so the CSMS had no way to learn the charge
    /// point had not understood it.
    #[tokio::test]
    async fn a_value_that_is_not_of_the_variables_type_is_rejected_and_not_stored() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let outcomes = handle_set_variables(
            &actor,
            alloc::vec![SetVariableRequest {
                component: component("OCPPCommCtrlr"),
                variable: variable("HeartbeatInterval"),
                attribute_type: VariableAttributeType::Actual,
                value: "banana".into(),
            }],
            &NoKeyStore,
        )
        .await;

        assert_eq!(outcomes, alloc::vec![SetVariableOutcome::Rejected]);
        assert_eq!(
            actor
                .state()
                .device_model
                .get(&component("OCPPCommCtrlr"), &variable("HeartbeatInterval"))
                .and_then(|definition| definition.attribute(VariableAttributeType::Actual))
                .map(|attribute| attribute.value.clone()),
            Some("60".into()),
            "a refused write must leave the previous value in place"
        );
    }

    /// B05.FR.08. The floor is `0` rather than `1` because `0` is meaningful for these
    /// intervals - see `VARIABLE_BOUNDS`.
    #[tokio::test]
    async fn a_numeric_value_below_the_variables_minimum_is_rejected() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let set = |value: &str| SetVariableRequest {
            component: component("OCPPCommCtrlr"),
            variable: variable("HeartbeatInterval"),
            attribute_type: VariableAttributeType::Actual,
            value: value.into(),
        };

        assert_eq!(
            handle_set_variables(&actor, alloc::vec![set("-1")], &NoKeyStore).await,
            alloc::vec![SetVariableOutcome::Rejected]
        );
        assert_eq!(
            handle_set_variables(&actor, alloc::vec![set("0")], &NoKeyStore).await,
            alloc::vec![SetVariableOutcome::Accepted],
            "zero is a real setting, not an out-of-range one"
        );
    }

    /// A `Boolean` takes OCPP's wire spelling and nothing else - not `1`, not `True`.
    #[tokio::test]
    async fn a_boolean_variable_takes_only_true_or_false() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let set = |value: &str| SetVariableRequest {
            component: component("AuthCacheCtrlr"),
            variable: variable("Enabled"),
            attribute_type: VariableAttributeType::Actual,
            value: value.into(),
        };

        assert_eq!(
            handle_set_variables(&actor, alloc::vec![set("false")], &NoKeyStore).await,
            alloc::vec![SetVariableOutcome::Accepted]
        );
        for bad in ["1", "True", "yes", ""] {
            assert_eq!(
                handle_set_variables(&actor, alloc::vec![set(bad)], &NoKeyStore).await,
                alloc::vec![SetVariableOutcome::Rejected],
                "{bad:?} is not a boolean"
            );
        }
    }

    /// A `MemberList` must be a subset of its `values_list`, element by element - so one bad
    /// member rejects the whole write rather than being silently dropped. Exercised against
    /// [`validate_value`] directly because the two variables that carry a `values_list` today
    /// (`TxStartPoint`/`TxStopPoint`) are registered read-only under CV2.1, so a handler-level
    /// test would be rejected before it reached the value check.
    #[test]
    fn a_member_list_is_checked_element_by_element_against_its_allowed_values() {
        let characteristics = VariableCharacteristics {
            data_type: VariableDataType::MemberList,
            unit: None,
            min_limit: None,
            max_limit: None,
            values_list: Some(alloc::vec![
                "EVConnected".into(),
                "Authorized".into(),
                "PowerPathClosed".into()
            ]),
            supports_monitoring: false,
        };

        assert!(validate_value(&characteristics, "EVConnected,Authorized").is_ok());
        assert_eq!(
            validate_value(&characteristics, "Authorized,Teleported"),
            Err(ValueRejection::NotAnAllowedValue),
            "one member outside the allowed set rejects the whole write"
        );
        // An empty list is a real value - "no members" - not a single empty-string member.
        assert!(validate_value(&characteristics, "").is_ok());
    }

    /// CV2.1/B05.FR.09: a variable this build does not act on must be *refused*, not accepted and
    /// ignored. `AuthCtrlr.LocalPreAuthorize` is a good example - OCPP defines it as writable, an
    /// operator setting it would reasonably believe the station now starts sessions from its local
    /// list without asking the CSMS, and nothing in this crate reads it (C14, roadmap CV2).
    #[tokio::test]
    async fn a_variable_this_build_does_not_act_on_is_refused_rather_than_silently_accepted() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let outcomes = handle_set_variables(
            &actor,
            alloc::vec![SetVariableRequest {
                component: component("AuthCtrlr"),
                variable: variable("LocalPreAuthorize"),
                attribute_type: VariableAttributeType::Actual,
                // A perfectly well-formed boolean - refused for what this build does with it,
                // not for what it says.
                value: "true".into(),
            }],
            &NoKeyStore,
        )
        .await;

        assert_eq!(outcomes, alloc::vec![SetVariableOutcome::Rejected]);
    }

    /// The other side of the same contract: a variable that *is* live stays writable, so CV2.1
    /// cannot be satisfied by simply freezing the whole device model.
    #[tokio::test]
    async fn a_variable_this_build_acts_on_is_still_writable() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let outcomes = handle_set_variables(
            &actor,
            alloc::vec![SetVariableRequest {
                component: component("AuthCtrlr"),
                variable: variable("LocalAuthorizeOffline"),
                attribute_type: VariableAttributeType::Actual,
                value: "false".into(),
            }],
            &NoKeyStore,
        )
        .await;

        assert_eq!(outcomes, alloc::vec![SetVariableOutcome::Accepted]);
    }

    /// A variable with no declared bound is unconstrained beyond its type - this crate does not
    /// invent limits a CSMS could not have predicted. `NetworkConfigurationPriority` is a
    /// `String` with no `max_limit`, so any string goes.
    #[tokio::test]
    async fn a_variable_with_no_declared_bounds_accepts_anything_of_its_type() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let outcomes = handle_set_variables(
            &actor,
            alloc::vec![SetVariableRequest {
                component: component("OCPPCommCtrlr"),
                variable: variable("NetworkConfigurationPriority"),
                attribute_type: VariableAttributeType::Actual,
                value: "0,1,2".into(),
            }],
            &NoKeyStore,
        )
        .await;

        assert_eq!(outcomes, alloc::vec![SetVariableOutcome::Accepted]);
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
            &NoKeyStore,
        )
        .await;

        assert_eq!(outcomes, alloc::vec![SetVariableOutcome::UnknownComponent]);
    }

    // --- CV10: the credential writes this build refuses (A01.FR.02/.03/.12) ---

    fn slot_component(name: &str, instance: &str) -> Component {
        Component {
            name: name.into(),
            instance: Some(instance.into()),
            evse: None,
        }
    }

    async fn actor_with_a_network_profile() -> ChargePointActor {
        use crate::state::{NetworkConnectionProfile, NetworkInterface, NetworkTransport};

        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let _ = actor
            .send(ChargePointEvent::NetworkProfileSet {
                slot: 1,
                profile: alloc::boxed::Box::new(NetworkConnectionProfile {
                    csms_url: "wss://csms.example/ocpp".into(),
                    interface: NetworkInterface::Any,
                    transport: NetworkTransport::Json,
                    security_profile: 1,
                    message_timeout_secs: 30,
                    identity: None,
                }),
            })
            .await;
        actor
    }

    /// Before CV10's second half, `NoKeyStore` is the honest stand-in for "this build has nothing
    /// to persist a rotation through", and the outcome must still be `Rejected` rather than an
    /// `Accepted` this crate cannot actually carry.
    ///
    /// The status matters more than it looks: A01.FR.03 has a CSMS that sees `Accepted` stop
    /// accepting the old password at once, so a station that accepted a rotation it then discarded
    /// would lock itself out of the only peer that could put it right. `Rejected` keeps the old
    /// credentials in force (A01.FR.04).
    #[tokio::test]
    async fn rotating_the_basic_auth_password_with_no_key_store_is_refused_rather_than_silently_accepted()
     {
        let actor = actor_with_a_network_profile().await;

        let outcomes = handle_set_variables(
            &actor,
            alloc::vec![SetVariableRequest {
                component: slot_component("NetworkConfiguration", "1"),
                variable: variable("BasicAuthPassword"),
                attribute_type: VariableAttributeType::Actual,
                value: "a-perfectly-valid-rotation".into(),
            }],
            &NoKeyStore,
        )
        .await;

        assert_eq!(outcomes, alloc::vec![SetVariableOutcome::Rejected]);
    }

    /// CV10's write is real now: a well-formed rotation against a real `KeyStore` is `Accepted`,
    /// and the value it persisted is exactly what a later dial would read back through
    /// `crate::basic_auth_credential::current`.
    #[tokio::test]
    async fn rotating_the_basic_auth_password_with_a_key_store_persists_it_and_is_accepted() {
        let actor = actor_with_a_network_profile().await;
        let key_store = key_store();

        let outcomes = handle_set_variables(
            &actor,
            alloc::vec![SetVariableRequest {
                component: slot_component("NetworkConfiguration", "1"),
                variable: variable("BasicAuthPassword"),
                attribute_type: VariableAttributeType::Actual,
                value: "a-perfectly-valid-rotation".into(),
            }],
            &key_store,
        )
        .await;

        assert_eq!(outcomes, alloc::vec![SetVariableOutcome::Accepted]);
        assert_eq!(
            crate::basic_auth_credential::current(&key_store, 1)
                .await
                .as_deref(),
            Some("a-perfectly-valid-rotation")
        );
    }

    /// A00.FR.205 is enforced through `BasicAuthPassword::new` before anything is persisted -
    /// even with a real `KeyStore` behind it, a too-short value is refused and leaves no record.
    #[tokio::test]
    async fn a_password_below_ocpps_floor_is_rejected_even_with_a_key_store() {
        let actor = actor_with_a_network_profile().await;
        let key_store = key_store();

        let outcomes = handle_set_variables(
            &actor,
            alloc::vec![SetVariableRequest {
                component: slot_component("NetworkConfiguration", "1"),
                variable: variable("BasicAuthPassword"),
                attribute_type: VariableAttributeType::Actual,
                value: "too-short".into(),
            }],
            &key_store,
        )
        .await;

        assert_eq!(outcomes, alloc::vec![SetVariableOutcome::Rejected]);
        assert_eq!(
            crate::basic_auth_credential::current(&key_store, 1).await,
            None
        );
    }

    /// A01.FR.04: rotating a slot's password twice must not lose the *original* password - a
    /// station that only remembered the last two values would still brick itself if the CSMS
    /// wrote a bad rotation right after a good one.
    #[tokio::test]
    async fn a_second_rotation_still_leaves_the_first_password_recoverable() {
        let actor = actor_with_a_network_profile().await;
        let key_store = key_store();

        for value in ["first-password-16", "second-password-16"] {
            let outcomes = handle_set_variables(
                &actor,
                alloc::vec![SetVariableRequest {
                    component: slot_component("NetworkConfiguration", "1"),
                    variable: variable("BasicAuthPassword"),
                    attribute_type: VariableAttributeType::Actual,
                    value: value.into(),
                }],
                &key_store,
            )
            .await;
            assert_eq!(outcomes, alloc::vec![SetVariableOutcome::Accepted]);
        }

        assert!(crate::basic_auth_credential::rollback(&key_store, 1).await);
        assert_eq!(
            crate::basic_auth_credential::current(&key_store, 1)
                .await
                .as_deref(),
            Some("first-password-16")
        );
    }

    /// A01.FR.11/.12: a successful rotation is logged - that the password changed, for which
    /// slot, and nothing else. The security log is where an operator (or an auditor) finds out a
    /// credential rotated at all; the value must never be one of the fields it carries.
    #[tokio::test]
    async fn a_successful_rotation_is_logged_without_the_value() {
        let actor = actor_with_a_network_profile().await;
        let mut events = actor.subscribe_security_events();

        let outcomes = handle_set_variables(
            &actor,
            alloc::vec![SetVariableRequest {
                component: slot_component("NetworkConfiguration", "1"),
                variable: variable("BasicAuthPassword"),
                attribute_type: VariableAttributeType::Actual,
                value: "correct-horse-battery-staple".into(),
            }],
            &key_store(),
        )
        .await;
        assert_eq!(outcomes, alloc::vec![SetVariableOutcome::Accepted]);

        let received = events.recv().await.unwrap();
        assert_eq!(
            received.event_type,
            crate::state::SecurityEventType::ReconfigurationOfSecurityParameters
        );
        let tech_info = received.tech_info.expect("a slot should be named");
        assert!(
            tech_info.contains('1'),
            "the slot should be named: {tech_info}"
        );
        assert!(
            !tech_info.contains("correct-horse-battery-staple"),
            "the password reached the security log: {tech_info}"
        );
    }

    /// A rotated password is never parked in `ChargePointState` - it goes to `key_store`, not
    /// `DeviceModelEvent::AttributeValueSet`. This covers the door `WriteOnly` alone does not:
    /// `GetVariables` already refuses to read it, but an `Accepted` write must not have left the
    /// plaintext somewhere `ChargePointState`'s `Debug` (what `trace!` prints whole) would show
    /// it (A01.FR.12).
    #[tokio::test]
    async fn a_rotated_password_never_reaches_the_state_a_trace_log_would_print() {
        let actor = actor_with_a_network_profile().await;

        let outcomes = handle_set_variables(
            &actor,
            alloc::vec![SetVariableRequest {
                component: slot_component("NetworkConfiguration", "1"),
                variable: variable("BasicAuthPassword"),
                attribute_type: VariableAttributeType::Actual,
                value: "correct-horse-battery-staple".into(),
            }],
            &key_store(),
        )
        .await;
        assert_eq!(outcomes, alloc::vec![SetVariableOutcome::Accepted]);

        let printed = alloc::format!("{:?}", actor.state());
        assert!(
            !printed.contains("correct-horse-battery-staple"),
            "a credential reached the state a trace log prints"
        );
        // ...and it is still unreadable through the front door - `WriteOnly` blocks reads
        // regardless of whether the write behind it succeeded.
        assert_eq!(
            handle_get_variables(
                &actor,
                alloc::vec![GetVariableRequest {
                    component: slot_component("NetworkConfiguration", "1"),
                    variable: variable("BasicAuthPassword"),
                    attribute_type: VariableAttributeType::Actual,
                }]
            ),
            alloc::vec![GetVariableOutcome::Rejected]
        );
    }

    /// A slot the CSMS never wrote a profile into has no `NetworkConfiguration[slot]` component
    /// at all, so the write cannot reach `handle_basic_auth_password_write` in the first place -
    /// it fails the same `UnknownComponent` every other variable on an unoccupied slot would.
    #[tokio::test]
    async fn a_password_for_an_unoccupied_slot_is_unknown_component_not_a_persisted_rotation() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let key_store = key_store();

        let outcomes = handle_set_variables(
            &actor,
            alloc::vec![SetVariableRequest {
                component: slot_component("NetworkConfiguration", "9"),
                variable: variable("BasicAuthPassword"),
                attribute_type: VariableAttributeType::Actual,
                value: "a-perfectly-valid-rotation".into(),
            }],
            &key_store,
        )
        .await;

        assert_eq!(outcomes, alloc::vec![SetVariableOutcome::UnknownComponent]);
        assert_eq!(
            crate::basic_auth_credential::current(&key_store, 9).await,
            None
        );
    }

    /// The same hole, the same shape: `WebPaymentsCtrlr.SharedSecret` is `WriteOnly` for the same
    /// reason and is equally unacted-on, so it takes the same refusal. Unlike the password's
    /// component this one is *not* re-derived per event, so an accepted write would have kept the
    /// secret in `ChargePointState` indefinitely. Unlike `BasicAuthPassword`, a real `KeyStore`
    /// changes nothing here - this variable has no consumer at all yet (see
    /// `REFUSED_WRITE_ONLY_VARIABLES`'s docs).
    #[tokio::test]
    async fn the_web_payments_shared_secret_is_refused_on_the_same_grounds() {
        use crate::hardware::Capabilities;

        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let _ = actor
            .send(ChargePointEvent::CapabilitiesDeclared(Capabilities {
                payment: true,
                ..Capabilities::default()
            }))
            .await;
        for event in super::capability_gate_events(&actor.state().capabilities) {
            let _ = actor.send(event).await;
        }

        let outcomes = handle_set_variables(
            &actor,
            alloc::vec![SetVariableRequest {
                component: component("WebPaymentsCtrlr"),
                variable: variable("SharedSecret"),
                attribute_type: VariableAttributeType::Actual,
                value: "a-shared-secret-value".into(),
            }],
            &key_store(),
        )
        .await;

        assert_eq!(outcomes, alloc::vec![SetVariableOutcome::Rejected]);
        assert!(!alloc::format!("{:?}", actor.state()).contains("a-shared-secret-value"));
    }

    /// The refusal is aimed at one named credential now, not at every `NetworkConfiguration`
    /// write - a variable name not in the table (or, since CV10, `BasicAuthPassword` itself) must
    /// not be blocked by it.
    #[tokio::test]
    async fn the_refusal_does_not_spill_onto_other_variables_of_the_same_component() {
        let actor = actor_with_a_network_profile().await;

        // `OcppCsmsUrl` is `ReadOnly` (CV1.3), so it is refused for its own reason - and reports
        // the same `Rejected`. What this pins is that a *variable name* not in the table takes
        // the ordinary path: an unknown one still answers `UnknownVariable`.
        let outcomes = handle_set_variables(
            &actor,
            alloc::vec![SetVariableRequest {
                component: slot_component("NetworkConfiguration", "1"),
                variable: variable("NotAVariable"),
                attribute_type: VariableAttributeType::Actual,
                value: "x".into(),
            }],
            &NoKeyStore,
        )
        .await;

        assert_eq!(outcomes, alloc::vec![SetVariableOutcome::UnknownVariable]);
    }
}

#[cfg(feature = "ocpp_2_1")]
mod ocpp_2_1 {
    use super::{
        GetVariableOutcome, GetVariableRequest, GetVariablesHandler, SetVariableOutcome,
        SetVariableRequest, SetVariablesHandler, handle_get_variables, handle_set_variables,
    };
    use crate::actor::ChargePointActor;
    use crate::hardware::KeyStore;
    use crate::state::{Component, Variable, VariableAttributeType};
    use crate::wire::v21::common::{
        AttributeEnum, GetVariableData, GetVariableResult, GetVariableStatusEnum, SetVariableData,
        SetVariableResult, SetVariableStatusEnum,
    };
    use crate::wire::v21::{GetVariablesResponse, SetVariablesResponse};
    use alloc::boxed::Box;
    use alloc::string::ToString;
    use alloc::vec::Vec;
    use ocpp_client::ocpp_2_1::OCPP2_1Client;

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
    fn map_component(component: &crate::wire::v21::common::Component) -> Component {
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

    fn map_variable(variable: &crate::wire::v21::common::Variable) -> Variable {
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
                // `attributeValue` is an unbounded `String` since `ocpp-types` 0.2.0 (its
                // length is a device-model variable, not a fixed cap), so this truncates to the
                // same 2500 bytes the `heapless` capacity used to enforce and cannot fail.
                Some(truncate_to_byte_boundary(&value, 2500).into())
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
                    // B06.FR.16/.17 (CV2.8): refuse before decoding a request larger than the
                    // ceiling `DeviceDataCtrlr` publishes for this message.
                    if let Err(violation) = crate::message_limits::check_message_size(
                        &actor,
                        "DeviceDataCtrlr",
                        Some("GetVariables"),
                        request.get_variable_data.len(),
                        &request,
                    ) {
                        return Err(crate::message_limits::ocpp_2_1_too_large(
                            "GetVariables",
                            violation,
                        ));
                    }
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
        async fn register_set_variables_handler<K: KeyStore + Send + Sync + 'static>(
            &self,
            actor: ChargePointActor,
            key_store: K,
        ) {
            let key_store = alloc::sync::Arc::new(key_store);
            self.on_set_variables(move |request, _client| {
                let actor = actor.clone();
                let key_store = key_store.clone();
                async move {
                    // B05.FR.11 + B06.FR.16/.17 (CV2.8), same ceiling under a different instance.
                    if let Err(violation) = crate::message_limits::check_message_size(
                        &actor,
                        "DeviceDataCtrlr",
                        Some("SetVariables"),
                        request.set_variable_data.len(),
                        &request,
                    ) {
                        return Err(crate::message_limits::ocpp_2_1_too_large(
                            "SetVariables",
                            violation,
                        ));
                    }
                    let parsed: Vec<SetVariableRequest> = request
                        .set_variable_data
                        .iter()
                        .map(parse_set_variable_data)
                        .collect();
                    let outcomes = handle_set_variables(&actor, parsed, &key_store).await;
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

        fn wire_component(evse: Option<(i64, Option<i64>)>) -> crate::wire::v21::common::Component {
            crate::wire::v21::common::Component {
                custom_data: None,
                evse: evse.map(|(id, connector_id)| crate::wire::v21::common::EVSE {
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
                variable: crate::wire::v21::common::Variable {
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
    use crate::hardware::KeyStore;
    use crate::state::{Component, Variable, VariableAttributeType};
    use crate::wire::v201::common::{
        AttributeEnum, GetVariableData, GetVariableResult, GetVariableStatusEnum, SetVariableData,
        SetVariableResult, SetVariableStatusEnum,
    };
    use crate::wire::v201::{GetVariablesResponse, SetVariablesResponse};
    use alloc::boxed::Box;
    use alloc::string::ToString;
    use alloc::vec::Vec;
    use ocpp_client::ocpp_2_0_1::OCPP2_0_1Client;

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
    fn map_component(component: &crate::wire::v201::common::Component) -> Component {
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

    fn map_variable(variable: &crate::wire::v201::common::Variable) -> Variable {
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
                // `attributeValue` is an unbounded `String` since `ocpp-types` 0.2.0 (its
                // length is a device-model variable, not a fixed cap), so this truncates to the
                // same 2500 bytes the `heapless` capacity used to enforce and cannot fail.
                Some(truncate_to_byte_boundary(&value, 2500).into())
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
                    // B06.FR.16/.17 (CV2.8): refuse before decoding a request larger than the
                    // ceiling `DeviceDataCtrlr` publishes for this message.
                    if let Err(violation) = crate::message_limits::check_message_size(
                        &actor,
                        "DeviceDataCtrlr",
                        Some("GetVariables"),
                        request.get_variable_data.len(),
                        &request,
                    ) {
                        return Err(crate::message_limits::ocpp_2_0_1_too_large(
                            "GetVariables",
                            violation,
                        ));
                    }
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
        async fn register_set_variables_handler<K: KeyStore + Send + Sync + 'static>(
            &self,
            actor: ChargePointActor,
            key_store: K,
        ) {
            let key_store = alloc::sync::Arc::new(key_store);
            self.on_set_variables(move |request, _client| {
                let actor = actor.clone();
                let key_store = key_store.clone();
                async move {
                    // B05.FR.11 + B06.FR.16/.17 (CV2.8), same ceiling under a different instance.
                    if let Err(violation) = crate::message_limits::check_message_size(
                        &actor,
                        "DeviceDataCtrlr",
                        Some("SetVariables"),
                        request.set_variable_data.len(),
                        &request,
                    ) {
                        return Err(crate::message_limits::ocpp_2_0_1_too_large(
                            "SetVariables",
                            violation,
                        ));
                    }
                    let parsed: Vec<SetVariableRequest> = request
                        .set_variable_data
                        .iter()
                        .map(parse_set_variable_data)
                        .collect();
                    let outcomes = handle_set_variables(&actor, parsed, &key_store).await;
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
            let component = crate::wire::v201::common::Component {
                custom_data: None,
                evse: Some(crate::wire::v201::common::EVSE {
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
    use crate::hardware::{Capabilities, KeyStore};
    use crate::state::{
        Component, DeviceModel, Variable, VariableAttributeType, VariableDefinition,
        VariableMutability,
    };
    use crate::wire::v16::common::{ChangeConfigurationResponseStatus, ConfigurationKeyItem};
    use crate::wire::v16::{ChangeConfigurationResponse, GetConfigurationResponse};
    use alloc::boxed::Box;
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use ocpp_client::ocpp_1_6::OCPP1_6Client;

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

    // G4.2: the conversion below cannot fail, and this is what makes that a fact rather than a
    // claim. If the key were ever renamed past the wire field's 50-byte bound, this fails the
    // *build* - where an `unwrap()` would have failed a charge point in the field, on a code path
    // a CSMS reaches simply by sending GetConfiguration.
    const _: () = assert!(SUPPORTED_FEATURE_PROFILES_KEY.len() <= 50);

    /// Builds the synthetic `SupportedFeatureProfiles` [`ConfigurationKeyItem`] - see
    /// [`SUPPORTED_FEATURE_PROFILES_KEY`]'s docs.
    fn supported_feature_profiles_item(capabilities: &Capabilities) -> ConfigurationKeyItem {
        let value = crate::hardware::supported_feature_profiles_1_6(capabilities);
        ConfigurationKeyItem {
            // Infallible by the const assertion above; `unwrap_or_default` rather than `unwrap`
            // so no panic exists on this path at all.
            key: heapless::String::try_from(SUPPORTED_FEATURE_PROFILES_KEY).unwrap_or_default(),
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
        async fn register_set_variables_handler<K: KeyStore + Send + Sync + 'static>(
            &self,
            actor: ChargePointActor,
            key_store: K,
        ) {
            let key_store = alloc::sync::Arc::new(key_store);
            self.on_change_configuration(move |request, _client| {
                let actor = actor.clone();
                let key_store = key_store.clone();
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
                            &key_store,
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
            use crate::hardware::NoKeyStore;

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
                    &NoKeyStore,
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
