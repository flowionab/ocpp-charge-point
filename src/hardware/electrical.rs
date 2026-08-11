//! The electrical facts about a charge point that only its manufacturer knows
//! (`docs/OCPP-2.1-COMPLIANCE-ROADMAP.md` CV1.5).
//!
//! OCPP marks `SupplyPhases`, `Connector.ConnectorType` and `EVSE.Power` as **required** device-
//! model variables, and none of them is derivable from anything this crate can see: how many AC
//! phases reach a connector, which physical coupler is fitted, and how much power an EVSE can
//! deliver are all facts about the box on the wall. A CSMS reads them to decide what it can ask
//! for - so getting them wrong is worse than leaving them absent, which is why the defaults here
//! say "unknown" rather than guessing something plausible.
//!
//! Declared once at start-up like [`Capabilities`](crate::hardware::Capabilities), through the
//! same "declare, don't poll" route: [`ChargePoint::electrical`](crate::hardware::ChargePoint::electrical)
//! returns one of these, `ChargePointBuilder::start` registers it, and it is not read again.

use alloc::string::String;
use alloc::vec::Vec;

/// How many AC phases are connected, using OCPP's own encoding.
///
/// OCPP's `SupplyPhases` is an integer where `1` or `3` are the AC cases and **`0` means DC** -
/// not "no supply". A null means the count is unknown. This enum makes both special cases
/// explicit rather than leaving a bare integer whose `0` reads as an error to anyone who has not
/// read the appendix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SupplyPhases {
    /// Single-phase AC.
    SinglePhaseAc,
    /// Three-phase AC.
    ThreePhaseAc,
    /// DC - no alternating phases, which OCPP encodes as `0`.
    Dc,
    /// Not declared. Reported as an absent value rather than a guess.
    #[default]
    Unknown,
}

impl SupplyPhases {
    /// OCPP's wire value, or `None` when unknown.
    pub fn as_wire_value(self) -> Option<&'static str> {
        match self {
            Self::SinglePhaseAc => Some("1"),
            Self::ThreePhaseAc => Some("3"),
            Self::Dc => Some("0"),
            Self::Unknown => None,
        }
    }
}

/// One connector's electrical identity.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConnectorElectrical {
    /// The physical coupler fitted, as a `ConnectorEnumType` value (`cType2`, `cCCS2`,
    /// `cChaoJi`, ...). Empty when the integrator has not declared one.
    ///
    /// Free text rather than an enum on purpose: OCPP 2.1 extends the type list beyond its own
    /// enum (`cGBT`, `cChaoJi`, `OppCharge` are named in the appendix as additions), and a
    /// closed enum here would refuse a coupler the spec permits.
    pub connector_type: String,
    /// Phases reaching this connector.
    pub supply_phases: SupplyPhases,
}

/// One EVSE's electrical characteristics.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EvseElectrical {
    /// Phases reaching this EVSE.
    pub supply_phases: SupplyPhases,
    /// The maximum power this EVSE can deliver, in watts.
    ///
    /// OCPP requires this as `EVSE.Power`'s `maxLimit` *characteristic* - the appendix is explicit
    /// that the limit is required while a live instantaneous reading is only desired - so this is
    /// registered as the variable's `max_limit`, not as its value.
    pub max_power_w: Option<f64>,
    /// The maximum power this EVSE can push *back*, in watts.
    ///
    /// Registered as `EVSE.DischargePower`, which the 2.1 appendix marks `Required? = V2X` -
    /// required of a station that can discharge, absent from one that cannot. So it follows
    /// [`Capabilities::supports_bidirectional_power`](crate::hardware::Capabilities::supports_bidirectional_power)
    /// rather than being registered unconditionally like the rest of this struct: advertising a
    /// discharge figure on a charge-only station would claim a capability the hardware does not
    /// have.
    ///
    /// Sign convention is OCPP's: a positive number of watts the EVSE can deliver back to the
    /// grid, not a negative charge rate.
    pub max_discharge_power_w: Option<f64>,
    /// Per-connector detail, indexed the same way this crate indexes connectors. Shorter than the
    /// EVSE's connector count is tolerated: the connectors it does not cover are left undeclared.
    pub connectors: Vec<ConnectorElectrical>,
}

/// Everything an integrator declares about the electrical shape of their charge point.
///
/// [`Default`] is entirely "unknown", which is the honest starting point for an integrator who has
/// not filled it in - and means adding this to the trait changes nothing for a station that
/// ignores it beyond the variables becoming present and empty, as OCPP requires.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ElectricalCharacteristics {
    /// Phases reaching the charge point as a whole.
    pub supply_phases: SupplyPhases,
    /// Per-EVSE detail, indexed by `evse_id`.
    pub evses: Vec<EvseElectrical>,
}

impl ElectricalCharacteristics {
    /// This EVSE's declaration, or `None` if it was not declared.
    pub fn evse(&self, evse_id: usize) -> Option<&EvseElectrical> {
        self.evses.get(evse_id)
    }

    /// This connector's declaration, or `None` if it was not declared.
    pub fn connector(&self, evse_id: usize, connector_id: usize) -> Option<&ConnectorElectrical> {
        self.evse(evse_id)?.connectors.get(connector_id)
    }
}
