//! Internal types for the outbound smart-charging notifications this crate forwards to the CSMS
//! but never decides to send off its own state machine: an external charging limit (from
//! something other than a CSMS charging profile - typically a local energy-management system,
//! which [`ExternalChargingLimit`] also enforces rather than merely reporting) and
//! whatever an EV reported via ISO 15118, which this crate does not implement (see
//! `docs/PRODUCTION-ROADMAP.md` B4.5) but forwards on an integrator's behalf. See
//! `crate::smart_charging::notifications` (B2.8) for the notifier traits and per-version wire
//! mapping that act on these.

use chrono::{DateTime, Utc};

use crate::state::{ChargingLimitSource, ChargingSchedule};

/// A charging limit imposed by something other than a CSMS charging profile, reported via
/// `NotifyChargingLimit`/`ClearedChargingLimit` (2.0.1/2.1 only - 1.6J has neither message).
///
/// Deliberately **not** a [`crate::state::ChargingProfile`] and never installed into
/// [`crate::state::ChargingProfileStore`]: OCPP itself keeps the two apart (a charging profile is
/// something the CSMS installs, can query and can clear by id; an external limit is only ever
/// reported, never addressed by the CSMS at all), and conflating them would let an EMS's limit be
/// reported back to the CSMS as though the CSMS itself had set it.
///
/// **Enforced as well as reported** (OCPP K11.FR.01, K12.FR.01, K13.FR.01, K27.FR.01). Being kept
/// out of the profile store does not mean being kept out of composition:
/// [`crate::smart_charging::external_charging_limits`] turns whichever limits are in force into
/// composing profiles at composition time - [`ChargingProfilePurpose::ExternalConstraints`](crate::state::ChargingProfilePurpose::ExternalConstraints)
/// for a constraint, so it lowers whatever the CSMS's own profiles asked for and never raises it,
/// and [`LocalGeneration`](crate::state::ChargingProfilePurpose::LocalGeneration) for capacity, so
/// it does the opposite (see [`Self::is_local_generation`]). A station that reported a limit it did
/// not apply would hand the CSMS's load calculation a reduction that never happened, which is worse
/// than not supporting external limits at all.
///
/// Two cases where a limit is reported but cannot bind, both by construction rather than by
/// oversight:
///
/// - [`Self::schedule`] is `None`. There is no value to enforce; recording it warns.
/// - The schedule is in [`ChargingRateUnit::Watts`](crate::state::ChargingRateUnit::Watts) and the
///   projection was built without [`crate::smart_charging::SupplyCharacteristics`]. Converting
///   would need the installation's voltage and phase count, which this crate refuses to guess -
///   the same rule that applies to a watt-denominated CSMS profile, for the same reason.
///
/// Not persisted, unlike an installed profile: a reboot drops the limit and the enforcement of it
/// together, so the station never goes on limiting for a reason it can no longer report - but an
/// external system does have to re-push after a power cut.
#[derive(Debug, Clone, PartialEq)]
pub struct ExternalChargingLimit {
    /// Who imposed the limit - [`ChargingLimitSource::Ems`] or [`ChargingLimitSource::Other`] in
    /// practice; [`ChargingLimitSource::Cso`]/[`ChargingLimitSource::So`] name limits the CSMS
    /// already knows about through its own installed profiles, so an integrator reporting one of
    /// those here would be describing something this crate would otherwise handle through
    /// `crate::smart_charging` instead.
    pub source: ChargingLimitSource,
    /// Whether the limit is critical for grid stability - passed straight through to OCPP's own
    /// `isGridCritical`.
    pub is_grid_critical: Option<bool>,
    /// The schedule the limit imposes, if the source can express one, reusing this crate's own
    /// [`ChargingSchedule`] representation rather than a second copy of it. `None` reports that a
    /// limit is in force without saying what it is - OCPP's own `chargingSchedule` is optional for
    /// exactly this reason.
    pub schedule: Option<ChargingSchedule>,
    /// Whether this describes locally generated capacity - power from solar or a site battery -
    /// rather than a constraint on what the station may draw (2.1's `isLocalGeneration`, K27).
    ///
    /// **It inverts what the schedule means.** A constraint's schedule is a ceiling and composes
    /// as [`ChargingProfilePurpose::ExternalConstraints`](crate::state::ChargingProfilePurpose::ExternalConstraints);
    /// local generation's is headroom and composes as
    /// [`LocalGeneration`](crate::state::ChargingProfilePurpose::LocalGeneration), which *adds* to
    /// the composite rather than capping it (K27.FR.01). An integrator that sets this wrongly does
    /// not merely mislabel a report - it moves the limit the connector runs at.
    ///
    /// The CSMS is told explicitly for the same reason: K27.FR.05 notes that it "cannot deduce
    /// from the charging schedule that a schedule represents local generation or a limit". 1.6J
    /// and 2.0.1 have no field for it, so on those connections the distinction survives in what
    /// the station *does* and is lost in what it *says* - see each adapter's `wire_purpose`.
    pub is_local_generation: bool,
}

/// How the EV wants to receive energy - ISO 15118-2's `EnergyTransferModeEnum`, forwarded as part
/// of [`EVChargingNeeds`].
///
/// 2.1's own wire enum has several more values for ISO 15118-20 (bidirectional/DER transfer
/// modes); this type deliberately carries only the ISO 15118-2 set both OCPP versions this crate
/// wires the message for share - see [`EVChargingNeeds`]'s docs for why 15118-20 is out of scope
/// here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnergyTransferMode {
    /// DC charging.
    Dc,
    /// Single-phase AC charging.
    AcSinglePhase,
    /// Two-phase AC charging.
    AcTwoPhase,
    /// Three-phase AC charging.
    AcThreePhase,
}

/// AC-specific parameters of an EV's charging needs (ISO 15118-2's `AC_EVChargeParameterType`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AcChargingNeeds {
    /// Energy requested, in Wh (includes any preconditioning).
    pub energy_amount_wh: i64,
    /// The EV's maximum current per phase, in amps (includes cable capacity).
    pub ev_max_current_a: i64,
    /// The EV's maximum voltage.
    pub ev_max_voltage_v: i64,
    /// The EV's minimum current per phase, in amps.
    pub ev_min_current_a: i64,
}

/// DC-specific parameters of an EV's charging needs (ISO 15118-2's `DC_EVChargeParameterType`).
/// Every field but the two maxima is optional on the wire, which this mirrors rather than
/// inventing defaults for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DcChargingNeeds {
    /// The EV's maximum current, in amps (includes cable capacity).
    pub ev_max_current_a: i64,
    /// The EV's maximum voltage.
    pub ev_max_voltage_v: i64,
    /// Energy requested, in Wh.
    pub energy_amount_wh: Option<i64>,
    /// The EV's maximum power, in W.
    pub ev_max_power_w: Option<i64>,
    /// The EV's battery capacity, in Wh.
    pub ev_energy_capacity_wh: Option<i64>,
    /// State of charge at which the EV considers fast charging done, in percent (0-100).
    pub bulk_soc_percent: Option<i64>,
    /// State of charge at which the EV considers itself fully charged, in percent (0-100).
    pub full_soc_percent: Option<i64>,
    /// The battery's present state of charge, in percent (0-100).
    pub state_of_charge_percent: Option<i64>,
}

/// What an EV requested via ISO 15118, forwarded as OCPP's `ChargingNeeds` (`NotifyEVChargingNeeds`,
/// 2.0.1/2.1 only). This crate does not implement ISO 15118 itself
/// (`docs/PRODUCTION-ROADMAP.md` B4.5) - an integrator with a 15118 stack decodes the EV's own
/// request and pushes the fields the CSMS actually needs onto this type, the same way
/// [`crate::state::ConnectorEvent::MeterValueSampled`] lets hardware push in a meter reading
/// without this crate reading a meter itself.
///
/// Deliberately limited to ISO 15118-2's fields (`ac`/`dc`), which both OCPP versions this crate
/// wires the message for accept as `acChargingParameters`/`dcChargingParameters`. 2.1 additionally
/// accepts a much larger ISO 15118-20 shape (DER parameters, V2X, a full energy offer/price
/// schedule) that this type does not carry - a charge point whose 15118 stack is sophisticated
/// enough to produce those has outgrown what a "push the fields in" API can honestly represent,
/// and needs its own extension of this type rather than a guess made here without B4.5 to build
/// on.
#[derive(Debug, Clone, PartialEq)]
pub struct EVChargingNeeds {
    /// How the EV wants to receive energy.
    pub requested_energy_transfer: EnergyTransferMode,
    /// The EV's estimated departure time, if it reported one.
    pub departure_time: Option<DateTime<Utc>>,
    /// AC charging parameters, if `requested_energy_transfer` is an AC mode.
    pub ac: Option<AcChargingNeeds>,
    /// DC charging parameters, if `requested_energy_transfer` is `Dc`.
    pub dc: Option<DcChargingNeeds>,
    /// The most schedule tuples this charge point should offer back, if the EV capped it.
    pub max_schedule_tuples: Option<u32>,
}

/// The schedule an EV intends to follow (or has accepted), forwarded as OCPP's
/// `NotifyEVChargingSchedule` (2.0.1/2.1 only). Reuses [`ChargingSchedule`] - the schedule shape
/// this crate already carries for CSMS-installed charging profiles - since an EV's intended
/// schedule is the same kind of thing: an ordered set of periods, each with a limit.
#[derive(Debug, Clone, PartialEq)]
pub struct EVChargingScheduleReport {
    /// The schedule itself.
    pub schedule: ChargingSchedule,
    /// The instant `schedule`'s periods are relative to.
    pub time_base: DateTime<Utc>,
    /// *(2.1 only)* Whether the EV accepts power tolerance on this schedule. Ignored by the
    /// 2.0.1 adapter, which has no such field to carry it in.
    pub power_tolerance_accepted: Option<bool>,
    /// *(2.1 only)* The id of the schedule (among a `ChargingProfile`'s several) the EV selected.
    /// Ignored by the 2.0.1 adapter.
    pub selected_charging_schedule_id: Option<i32>,
}

/// One notification this crate has to forward to the CSMS but did not decide to send on its own
/// initiative: an external charging limit changing, or something an EV reported via ISO 15118.
/// Carried as a [`crate::state::ChargePointEffect::SmartChargingNotification`] and re-broadcast by
/// [`crate::actor::ChargePointActor::subscribe_smart_charging_notifications`] for
/// [`crate::smart_charging::notifications::run_smart_charging_notifications`] to forward to the
/// CSMS. See `docs/PRODUCTION-ROADMAP.md` B2.8.
#[derive(Debug, Clone, PartialEq)]
pub enum SmartChargingNotification {
    /// An external charging limit came into force.
    ExternalChargingLimitSet {
        /// The EVSE the limit applies to, or `None` for the whole charging station (OCPP's own
        /// `evseId` absent/zero case).
        evse_id: Option<usize>,
        /// The limit itself.
        limit: ExternalChargingLimit,
    },
    /// A previously reported external charging limit has gone away.
    ExternalChargingLimitCleared {
        /// The EVSE the limit applied to, or `None` for the whole charging station - mirrors
        /// [`Self::ExternalChargingLimitSet`]'s `evse_id`.
        evse_id: Option<usize>,
        /// The source of the limit that ended - `ClearedChargingLimit` has to name it even though
        /// it carries nothing else identifying which limit is meant.
        source: ChargingLimitSource,
    },
    /// An EV reported its charging needs.
    EVChargingNeedsReported {
        /// The EVSE the EV is connected to. OCPP requires this to be a real, specific EVSE (never
        /// "the whole station"), unlike an external charging limit.
        evse_id: usize,
        /// What it needs.
        needs: EVChargingNeeds,
    },
    /// An EV reported (or accepted) a charging schedule.
    EVChargingScheduleReported {
        /// The EVSE the EV is connected to.
        evse_id: usize,
        /// The schedule.
        report: EVChargingScheduleReport,
    },
}
