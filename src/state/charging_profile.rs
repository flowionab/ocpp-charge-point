//! The charging-profile model and store - the Smart Charging functional block's state
//! (`docs/ROADMAP.md` §11, `docs/PRODUCTION-ROADMAP.md` B2.1).
//!
//! Deliberately protocol-version-independent, per `CLAUDE.md`: 1.6J, 2.0.1 and 2.1 all send
//! roughly this shape with different names and different extras, so the version adapters
//! (`crate::smart_charging`'s `ocpp_*` submodules) translate into these types rather than any of
//! them leaking a wire shape in here. Where the versions genuinely differ in *capability* rather
//! than naming - 2.x's several schedules per profile against 1.6J's single one, 2.1's extra
//! purposes - this model carries the superset and the older adapters project onto it, which is the
//! direction `CLAUDE.md` requires.

use alloc::string::String;
use alloc::vec::Vec;
use chrono::{DateTime, Utc};

use crate::state::TransactionId;

/// A charging profile's CSMS-assigned identifier. Unique across the charge point: installing a
/// profile whose id already exists replaces that profile, whatever scope it was installed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChargingProfileId(pub i32);

/// What a profile is *for*, which decides how it composes with the others - see
/// [`crate::smart_charging::compose`].
///
/// The order of this enum is the precedence order (lowest first), so `#[derive(PartialOrd)]` gives
/// composition its "which purpose wins at this instant" comparison directly rather than through a
/// hand-written match that could drift from the spec text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChargingProfilePurpose {
    /// The charge point's own installation limit (OCPP 2.x `ChargingStationMaxProfile`, 1.6J
    /// `ChargePointMaxProfile`). Caps everything else rather than competing with it: the composite
    /// limit is never above this, whatever a transaction profile asks for.
    ChargePointMax,
    /// The default limit applied to any transaction that has no [`Self::Tx`] profile of its own
    /// (OCPP `TxDefaultProfile`).
    TxDefault,
    /// A limit for one specific transaction (OCPP `TxProfile`), overriding [`Self::TxDefault`]
    /// while that transaction is running.
    Tx,
    /// An externally-imposed constraint the CSMS is relaying rather than choosing - a grid or
    /// local energy-management limit (OCPP 2.x `ChargingStationExternalConstraints`). Caps the
    /// result like [`Self::ChargePointMax`] does; 1.6J has no equivalent and its adapter never
    /// produces one.
    ExternalConstraints,
    /// 2.1's locally generated capacity (`LocalGeneration`) - power available on site, from solar
    /// or a local battery, that the grid connection never carries. **Adds to the composite result
    /// rather than capping it**: 2 kW of local generation under a 5 kW `TxDefaultProfile` is 7 kW,
    /// not 2 kW (2.1 Part 2 §K.3.6, use case K27).
    ///
    /// The distinction is the whole point of the purpose existing. An external limit *narrows*
    /// what the station may draw; local generation *widens* it, and the CSMS cannot tell which a
    /// schedule is from its numbers alone - which is why K27.FR.05 has the charge point say so
    /// explicitly. Neither 2.0.1 nor 1.6J has the purpose, so both adapters project it onto their
    /// external-constraints equivalent and say so in their module docs.
    LocalGeneration,
    /// 2.1's priority-charging profile, applied while a transaction has been granted priority
    /// (`UsePriorityCharging`). Not applicable to 1.6J or 2.0.1.
    PriorityCharging,
}

impl ChargingProfilePurpose {
    /// Whether this purpose *caps* the composite result rather than competing for it. A capping
    /// purpose's limit is applied as an upper bound on whatever the transaction-level profiles
    /// asked for; a non-capping one is chosen between by stack level.
    pub fn caps_the_result(&self) -> bool {
        matches!(self, Self::ChargePointMax | Self::ExternalConstraints)
    }

    /// Whether this purpose *adds* its limit to the composite result - true only for
    /// [`Self::LocalGeneration`], and the reason composition has three rules rather than two.
    ///
    /// Exclusive with [`Self::caps_the_result`]: a purpose either bounds the result from above,
    /// competes for it by stack level, or widens it. A test in this module asserts every variant
    /// falls into exactly one of the three, so a purpose added later cannot quietly become both.
    pub fn adds_to_the_result(&self) -> bool {
        matches!(self, Self::LocalGeneration)
    }
}

/// How a profile's schedule is anchored in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargingProfileKind {
    /// The schedule starts at its own `start_schedule` instant.
    Absolute,
    /// The schedule repeats from `start_schedule` at the [`RecurrencyKind`] interval.
    Recurring,
    /// The schedule starts when the transaction it applies to starts. A charge point has to know
    /// that start time to evaluate one - see [`crate::smart_charging::CompositionContext`], which
    /// takes it from the caller rather than from the (deliberately clock-free) state machine.
    Relative,
    /// **2.1 only.** The profile carries no schedule in the usual sense: exactly one schedule with
    /// exactly one period, whose limit the CSMS replaces as it goes rather than laying out in
    /// advance (`UpdateDynamicSchedule` pushed by the CSMS, or `PullDynamicScheduleUpdate` pulled
    /// by the charge point). OCPP's K28; see [`ChargingProfile::dyn_update_time`] for what a
    /// dynamic profile is anchored to and when it stops applying.
    ///
    /// Not projectable onto 1.6J or 2.0.1, neither of which has the kind or the messages. Their
    /// adapters never produce one, and [`crate::smart_charging`]'s reporting maps it to the
    /// nearest kind those versions can express.
    Dynamic,
}

/// How often a [`ChargingProfileKind::Recurring`] schedule repeats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecurrencyKind {
    /// Every 24 hours from `start_schedule`.
    Daily,
    /// Every 7 days from `start_schedule`.
    Weekly,
}

impl RecurrencyKind {
    /// The recurrence interval in seconds.
    pub fn period_secs(&self) -> i64 {
        match self {
            Self::Daily => 24 * 60 * 60,
            Self::Weekly => 7 * 24 * 60 * 60,
        }
    }
}

/// The unit a schedule's limits are expressed in. OCPP allows either, per schedule, and the two
/// are **not** interconvertible without knowing the supply voltage and phase count - which is
/// hardware knowledge this crate doesn't have. See
/// [`crate::smart_charging::CompositionContext::supply`] for how a caller supplies it when it
/// wants conversion, and what happens when it doesn't.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargingRateUnit {
    /// Amps per phase.
    Amps,
    /// Watts, total across phases.
    Watts,
}

/// One step of a schedule: from `start_period_secs` after the schedule's start until the next
/// period begins (or the schedule ends), the limit is `limit`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChargingSchedulePeriod {
    /// Seconds from the schedule's start at which this period takes effect.
    pub start_period_secs: u32,
    /// The limit in effect during this period, in the schedule's [`ChargingRateUnit`].
    pub limit: f64,
    /// How many phases this period permits, if the profile constrains it. `None` leaves the
    /// decision to the hardware.
    pub number_phases: Option<u8>,
}

/// A schedule: an ordered list of periods, anchored by the owning profile's
/// [`ChargingProfileKind`].
#[derive(Debug, Clone, PartialEq)]
pub struct ChargingSchedule {
    /// The CSMS's identifier for this schedule, echoed back in `GetCompositeSchedule` and
    /// `ReportChargingProfiles`. 1.6J has no schedule id; its adapter uses `0`.
    pub id: i32,
    /// When the schedule starts. Required for [`ChargingProfileKind::Absolute`] and
    /// [`ChargingProfileKind::Recurring`]; ignored for [`ChargingProfileKind::Relative`], which
    /// starts with its transaction.
    pub start_schedule: Option<DateTime<Utc>>,
    /// How long the schedule runs before it stops applying, in seconds. `None` means it applies
    /// indefinitely (or, when recurring, until the next repetition).
    pub duration_secs: Option<u32>,
    /// The unit every `limit` in `periods` (and `min_charging_rate`) is expressed in.
    pub rate_unit: ChargingRateUnit,
    /// The lowest rate the EV can usefully accept, if the CSMS supplied one. Recorded, and
    /// reported back in a composite schedule, but never used to *raise* a computed limit: a
    /// minimum that overrode a lower installation limit would be a safety problem, not a feature.
    pub min_charging_rate: Option<f64>,
    /// The periods. Each version adapter (`ocpp_1_6`/`ocpp_2_0_1`/`ocpp_2_1`) sorts these by
    /// `start_period_secs` before they reach this type - this type itself does not enforce the
    /// order, so a caller constructing one outside those adapters is responsible for the same
    /// invariant if it wants [`ChargingSchedule::limit_at`] to behave sensibly.
    pub periods: Vec<ChargingSchedulePeriod>,
}

impl ChargingSchedule {
    /// The limit in effect `elapsed_secs` into this schedule, or `None` if the schedule hasn't
    /// started yet (no period covers that instant) or has run past its `duration_secs`.
    pub fn limit_at(&self, elapsed_secs: i64) -> Option<&ChargingSchedulePeriod> {
        if elapsed_secs < 0 {
            return None;
        }
        if let Some(duration) = self.duration_secs
            && elapsed_secs >= i64::from(duration)
        {
            return None;
        }
        self.periods
            .iter()
            .rfind(|period| i64::from(period.start_period_secs) <= elapsed_secs)
    }

    /// The instant, in seconds from this schedule's start, at which the limit next changes after
    /// `elapsed_secs` - the start of the following period, or the schedule's end, whichever comes
    /// first. `None` if nothing further will change.
    ///
    /// This is what lets the projection task (`docs/PRODUCTION-ROADMAP.md` B2.4) wake up exactly
    /// at a period boundary instead of polling.
    pub fn next_change_after(&self, elapsed_secs: i64) -> Option<i64> {
        let next_period = self
            .periods
            .iter()
            .map(|period| i64::from(period.start_period_secs))
            .find(|start| *start > elapsed_secs);
        let end = self.duration_secs.map(i64::from);
        match (next_period, end) {
            (Some(next), Some(end)) => Some(next.min(end)),
            (Some(next), None) => Some(next),
            (None, Some(end)) if end > elapsed_secs => Some(end),
            _ => None,
        }
    }
}

/// A charging profile as installed on the charge point.
#[derive(Debug, Clone, PartialEq)]
pub struct ChargingProfile {
    /// The CSMS's identifier - see [`ChargingProfileId`].
    pub id: ChargingProfileId,
    /// Higher wins between profiles of the same [`ChargingProfilePurpose`]. Ties are broken by
    /// the more recently installed profile, which is also what the replacement rule in
    /// [`ChargingProfileStore::install`] makes rare.
    pub stack_level: u32,
    /// What this profile is for, and therefore how it composes - see [`ChargingProfilePurpose`].
    pub purpose: ChargingProfilePurpose,
    /// How its schedules are anchored in time.
    pub kind: ChargingProfileKind,
    /// For [`ChargingProfileKind::Recurring`], how often the schedule repeats.
    pub recurrency: Option<RecurrencyKind>,
    /// The profile does not apply before this instant. `None` means "already valid".
    pub valid_from: Option<DateTime<Utc>>,
    /// The profile does not apply after this instant. `None` means "valid indefinitely".
    pub valid_to: Option<DateTime<Utc>>,
    /// The transaction this profile applies to. Only meaningful for
    /// [`ChargingProfilePurpose::Tx`]; a `Tx` profile with no transaction id applies to whatever
    /// transaction is running on the connector it was installed against.
    pub transaction_id: Option<TransactionId>,
    /// The schedules, one per [`ChargingRateUnit`] the CSMS chose to express. 2.x permits several;
    /// 1.6J sends exactly one, so its adapter produces a single-element vector.
    pub schedules: Vec<ChargingSchedule>,
    /// **[`ChargingProfileKind::Dynamic`] only.** How often the charge point should *pull* a fresh
    /// limit for this profile (`PullDynamicScheduleUpdate`), in seconds. `None` or `0` means the
    /// CSMS pushes updates instead and the charge point never asks - OCPP's K28.FR.10 makes
    /// pulling conditional on this being greater than zero.
    ///
    /// Only meaningful on a dynamic profile: [`ChargingProfileStore::install`] refuses a
    /// non-dynamic profile that carries one, per K28.FR.04.
    pub dyn_update_interval_secs: Option<u32>,
    /// **[`ChargingProfileKind::Dynamic`] only.** When this profile's single period last took a
    /// new value - the instant it was installed, or the instant the most recent
    /// `UpdateDynamicSchedule` / `PullDynamicScheduleUpdate` response was applied (K28.FR.05,
    /// K28.FR.09).
    ///
    /// It does two jobs, which is why it is stored rather than derived. It is the schedule's
    /// **anchor**: a dynamic period is active from the moment it arrives, so there is no
    /// `start_schedule` to measure from. And it is the clock the profile's **expiry** runs on: if
    /// the schedule carries a `duration_secs` and `now` is past `dyn_update_time + duration`, the
    /// profile stops applying entirely and composition falls through to the next valid one
    /// (K28.FR.13/K28.FR.15). That is a deliberate dead-man's switch - a CSMS that stops sending
    /// updates must not leave a stale limit applied forever - and a later update makes the profile
    /// eligible again (K28.FR.14) without the CSMS having to reinstall it.
    pub dyn_update_time: Option<DateTime<Utc>>,
}

impl ChargingProfile {
    /// The schedule expressed in `unit`, or `None` if this profile has none in that unit. See
    /// [`ChargingRateUnit`] for why this crate refuses to convert between units on its own.
    pub fn schedule_in(&self, unit: ChargingRateUnit) -> Option<&ChargingSchedule> {
        self.schedules
            .iter()
            .find(|schedule| schedule.rate_unit == unit)
    }

    /// Whether this profile applies at `now`, per its `valid_from`/`valid_to` window and - for a
    /// [`ChargingProfileKind::Dynamic`] profile - whether its updates have gone stale. Says
    /// nothing about whether any of its schedules covers `now` - that's
    /// [`ChargingSchedule::limit_at`].
    pub fn is_valid_at(&self, now: DateTime<Utc>) -> bool {
        self.valid_from.is_none_or(|from| now >= from)
            && self.valid_to.is_none_or(|to| now < to)
            && !self.dynamic_updates_are_stale(now)
    }

    /// Whether this is a dynamic profile whose CSMS has stopped answering - OCPP K28.FR.13 and
    /// K28.FR.15: the schedule carries a `duration_secs`, and `now` is past
    /// [`dyn_update_time`](Self::dyn_update_time) plus that duration.
    ///
    /// A dead-man's switch, and the reason a dynamic profile's `duration` means something quite
    /// different from a scheduled one's. A scheduled profile's duration says when its curve runs
    /// out; a dynamic one's says how long a single pushed limit may be trusted without being
    /// refreshed. A CSMS that goes quiet must not leave its last limit applied indefinitely, so
    /// the profile stops applying and composition falls through to the next valid one. A later
    /// update makes it eligible again (K28.FR.14) without the CSMS reinstalling anything, because
    /// this is computed rather than latched.
    fn dynamic_updates_are_stale(&self, now: DateTime<Utc>) -> bool {
        if self.kind != ChargingProfileKind::Dynamic {
            return false;
        }
        let Some(duration) = self
            .schedules
            .first()
            .and_then(|schedule| schedule.duration_secs)
        else {
            // No duration means the CSMS set no deadline, so there is nothing to miss.
            return false;
        };
        self.dyn_update_time
            .is_some_and(|updated| now > updated + chrono::Duration::seconds(i64::from(duration)))
    }
}

/// Which connector scope a profile was installed against. OCPP addresses this as an `evseId`
/// (2.x) or `connectorId` (1.6J) where `0` means "the whole charge point"; this type makes that
/// sentinel explicit rather than leaving a magic zero to be remembered at every use site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChargingProfileScope {
    /// Installed charge-point-wide (OCPP `evseId`/`connectorId` `0`). Applies to every connector.
    ChargePoint,
    /// Installed against one EVSE, and therefore to the connectors it owns.
    Evse(usize),
}

/// A profile together with the scope it was installed at.
#[derive(Debug, Clone, PartialEq)]
pub struct InstalledChargingProfile {
    /// Where it was installed - see [`ChargingProfileScope`].
    pub scope: ChargingProfileScope,
    /// The profile itself.
    pub profile: ChargingProfile,
}

/// Which profiles a `ClearChargingProfile` request selects. Every field that is `Some` must match;
/// a request with no fields at all clears everything, which is what OCPP specifies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChargingProfileCriteria {
    /// Match one specific profile id.
    pub id: Option<ChargingProfileId>,
    /// Match only profiles installed at this scope.
    pub scope: Option<ChargingProfileScope>,
    /// Match only profiles with this purpose.
    pub purpose: Option<ChargingProfilePurpose>,
    /// Match only profiles at this stack level.
    pub stack_level: Option<u32>,
}

impl ChargingProfileCriteria {
    /// Whether `installed` is selected by these criteria.
    pub fn matches(&self, installed: &InstalledChargingProfile) -> bool {
        self.id.is_none_or(|id| installed.profile.id == id)
            && self.scope.is_none_or(|scope| installed.scope == scope)
            && self
                .purpose
                .is_none_or(|purpose| installed.profile.purpose == purpose)
            && self
                .stack_level
                .is_none_or(|level| installed.profile.stack_level == level)
    }
}

/// Who installed a charging profile, OCPP's `ChargingLimitSourceEnum`.
///
/// Every profile this crate holds is [`ChargingLimitSource::Cso`]: the only way one gets installed
/// is `SetChargingProfile`, which is the CSMS (the charge point operator) talking. The other
/// variants exist because a CSMS may *filter* on them in `GetChargingProfiles` - asking for only
/// EMS-installed profiles is a question this charge point must be able to answer truthfully, and
/// the answer is "none" rather than "all of them".
///
/// Limits arriving from somewhere other than the CSMS - a local energy manager, a DSO signal - are
/// not profiles and are not stored here. They arrive as
/// [`ExternalChargingLimit`](crate::state::ExternalChargingLimit), are reported with
/// `NotifyChargingLimit`/`ClearedChargingLimit`, and are enforced by
/// [`crate::smart_charging::composing_profiles`], which joins them onto the installed profiles as
/// capping profiles at composition time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargingLimitSource {
    /// An energy management system on site.
    Ems,
    /// The charge point operator, via the CSMS - the source of every profile stored here.
    Cso,
    /// The system operator (grid/DSO).
    So,
    /// Anything else.
    Other,
}

impl ChargingLimitSource {
    /// A stable, low-cardinality name for logging - see `CLAUDE.md`'s "fields over prose". The
    /// match is exhaustive with no wildcard arm on purpose: a new source must be a compile error
    /// rather than a mislabelled log line.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Ems => "EMS",
            Self::Cso => "CSO",
            Self::So => "SO",
            Self::Other => "Other",
        }
    }
}

impl InstalledChargingProfile {
    /// Who installed this profile - always [`ChargingLimitSource::Cso`]; see that type for why
    /// this is a fact about how profiles get here rather than a stub.
    pub fn source(&self) -> ChargingLimitSource {
        ChargingLimitSource::Cso
    }
}

/// Which profiles a `GetChargingProfiles` selects.
///
/// Deliberately not [`ChargingProfileCriteria`], which `ClearChargingProfile` uses: OCPP's *get*
/// criterion matches a **list** of profile ids and a list of limit sources, where *clear* matches
/// at most one id and no source at all. Sharing one type would mean widening the clear path to
/// fields it must never act on - clearing by a criterion the CSMS did not send is destructive in a
/// way over-reporting is not.
///
/// An empty list means "no filter on this field", matching OCPP's absent-means-all rule.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChargingProfileQuery {
    /// Match any of these profile ids.
    pub ids: Vec<ChargingProfileId>,
    /// Match only profiles installed at this scope.
    pub scope: Option<ChargingProfileScope>,
    /// Match only profiles with this purpose.
    pub purpose: Option<ChargingProfilePurpose>,
    /// Match only profiles at this stack level.
    pub stack_level: Option<u32>,
    /// Match only profiles installed by any of these sources.
    pub sources: Vec<ChargingLimitSource>,
}

impl ChargingProfileQuery {
    /// Whether `installed` is selected by this query.
    pub fn matches(&self, installed: &InstalledChargingProfile) -> bool {
        (self.ids.is_empty() || self.ids.contains(&installed.profile.id))
            && self.scope.is_none_or(|scope| installed.scope == scope)
            && self
                .purpose
                .is_none_or(|purpose| installed.profile.purpose == purpose)
            && self
                .stack_level
                .is_none_or(|level| installed.profile.stack_level == level)
            && (self.sources.is_empty() || self.sources.contains(&installed.source()))
    }
}

/// Why [`ChargingProfileStore::install`] refused a profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChargingProfileRejection {
    /// The store is already holding [`crate::state::StateLimits::max_charging_profiles`] profiles
    /// and this one is not replacing any of them (G2.2: a bound a remote peer can push past is
    /// not a bound).
    TooManyProfiles,
    /// The profile has no schedules at all, so it could never produce a limit.
    NoSchedule,
    /// The profile's purpose may not be installed at the scope requested - a `TxProfile` installed
    /// charge-point-wide has no transaction to apply to, which OCPP rejects rather than
    /// interprets.
    ScopeNotAllowedForPurpose(String),
    /// A [`ChargingProfileKind::Dynamic`] profile whose schedule isn't the single period OCPP
    /// requires - K28.FR.01/K28.FR.02. Reported to a 2.1 CSMS as `Rejected` with
    /// `statusInfo.reasonCode = "InvalidSchedule"`, which names the field to fix.
    InvalidDynamicSchedule(String),
    /// A **non**-dynamic profile carrying `dynUpdateInterval`, which only applies to dynamic ones
    /// - K28.FR.04. Reported as `Rejected` with `reasonCode = "InvalidProfile"`.
    DynUpdateIntervalOnNonDynamicProfile,
}

/// Every charging profile installed on the charge point, across every scope.
///
/// Bounded by [`crate::state::StateLimits::max_charging_profiles`] (G2.2). Owned by
/// [`crate::state::ChargePointState`] and mutated only through
/// [`crate::state::ChargePointEvent::ChargingProfileSet`] /
/// [`ChargingProfilesCleared`](crate::state::ChargePointEvent::ChargingProfilesCleared), like every
/// other piece of state in this crate.
#[derive(Debug, Clone, PartialEq)]
pub struct ChargingProfileStore {
    profiles: Vec<InstalledChargingProfile>,
    max_profiles: usize,
}

impl ChargingProfileStore {
    /// An empty store holding at most `max_profiles` profiles (clamped to at least 1).
    pub fn with_limit(max_profiles: usize) -> Self {
        Self {
            profiles: Vec::new(),
            max_profiles: max_profiles.max(1),
        }
    }

    /// The configured maximum - see [`crate::state::StateLimits::max_charging_profiles`].
    pub fn max_profiles(&self) -> usize {
        self.max_profiles
    }

    /// Every installed profile, in installation order.
    pub fn installed(&self) -> &[InstalledChargingProfile] {
        &self.profiles
    }

    /// How many profiles are installed.
    pub fn len(&self) -> usize {
        self.profiles.len()
    }

    /// Whether nothing is installed.
    pub fn is_empty(&self) -> bool {
        self.profiles.is_empty()
    }

    /// Installs `profile` at `scope`, replacing any profile it supersedes.
    ///
    /// Two replacement rules, both from the spec and both applied before the bound is checked (so
    /// a replacement can never be refused for being one profile too many):
    ///
    /// 1. **Same id** - a profile id identifies one profile on the whole charge point, so
    ///    reinstalling an id replaces whatever was there, even at a different scope.
    /// 2. **Same scope, purpose and stack level** - the CSMS is addressing one slot, so a new
    ///    profile in that slot displaces the old one rather than stacking beside it (which would
    ///    make the tie-break between them arbitrary).
    ///
    /// A [`ChargingProfileKind::Dynamic`] profile is additionally checked against OCPP's K28
    /// shape rules - see [`ChargingProfileRejection::InvalidDynamicSchedule`] and
    /// [`ChargingProfileRejection::DynUpdateIntervalOnNonDynamicProfile`].
    pub fn install(
        &mut self,
        scope: ChargingProfileScope,
        profile: ChargingProfile,
    ) -> Result<(), ChargingProfileRejection> {
        if profile.schedules.is_empty() {
            return Err(ChargingProfileRejection::NoSchedule);
        }
        Self::check_dynamic_shape(&profile)?;
        if scope == ChargingProfileScope::ChargePoint
            && profile.purpose == ChargingProfilePurpose::Tx
        {
            return Err(ChargingProfileRejection::ScopeNotAllowedForPurpose(
                "a TxProfile must be installed against an EVSE, not charge-point-wide".into(),
            ));
        }

        let superseded = |installed: &InstalledChargingProfile| {
            installed.profile.id == profile.id
                || (installed.scope == scope
                    && installed.profile.purpose == profile.purpose
                    && installed.profile.stack_level == profile.stack_level)
        };
        let replaced = self.profiles.iter().any(superseded);
        if !replaced && self.profiles.len() >= self.max_profiles {
            return Err(ChargingProfileRejection::TooManyProfiles);
        }
        self.profiles.retain(|installed| !superseded(installed));
        self.profiles
            .push(InstalledChargingProfile { scope, profile });
        Ok(())
    }

    /// OCPP's K28 shape rules for a dynamic profile, and the one rule about profiles that are
    /// *not* dynamic.
    ///
    /// A dynamic profile has no schedule to lay out - one schedule, one period, starting
    /// immediately - so anything more is the CSMS having sent a scheduled profile under a dynamic
    /// kind. Refused rather than trimmed to fit: silently dropping periods a CSMS sent would apply
    /// a limit it never asked for.
    fn check_dynamic_shape(profile: &ChargingProfile) -> Result<(), ChargingProfileRejection> {
        if profile.kind != ChargingProfileKind::Dynamic {
            // K28.FR.04.
            return match profile.dyn_update_interval_secs {
                Some(_) => Err(ChargingProfileRejection::DynUpdateIntervalOnNonDynamicProfile),
                None => Ok(()),
            };
        }
        // K28.FR.01.
        if profile.schedules.len() != 1 {
            return Err(ChargingProfileRejection::InvalidDynamicSchedule(
                "a Dynamic profile carries exactly one charging schedule".into(),
            ));
        }
        let schedule = &profile.schedules[0];
        if schedule.periods.len() != 1 {
            return Err(ChargingProfileRejection::InvalidDynamicSchedule(
                "a Dynamic profile's schedule carries exactly one period".into(),
            ));
        }
        // K28.FR.02.
        if schedule.periods[0].start_period_secs != 0 {
            return Err(ChargingProfileRejection::InvalidDynamicSchedule(
                "a Dynamic profile's period starts at 0 - it takes effect on receipt".into(),
            ));
        }
        Ok(())
    }

    /// Applies a dynamic limit update to the profile with `id`, stamping `updated_at` as its new
    /// [`dyn_update_time`](ChargingProfile::dyn_update_time) - K28.FR.06/K28.FR.08/K28.FR.09.
    ///
    /// Returns `false` when no such profile is installed, or when the one installed is not
    /// [`ChargingProfileKind::Dynamic`]: OCPP answers both with `Rejected` /
    /// `reasonCode = "InvalidProfile"` (K28.FR.11), and updating a scheduled profile in place
    /// would rewrite a curve the CSMS laid out deliberately.
    ///
    /// `limit` is `None` when the update carried only values this crate cannot project onto
    /// hardware (a setpoint, a discharge limit, a per-phase asymmetry - see
    /// [`crate::smart_charging`]). The timestamp still moves, because an update *did* arrive: the
    /// profile's K28.FR.13 expiry must not fire on a CSMS that is answering perfectly well.
    pub fn apply_dynamic_update(
        &mut self,
        id: ChargingProfileId,
        limit: Option<f64>,
        updated_at: DateTime<Utc>,
    ) -> bool {
        let Some(installed) = self
            .profiles
            .iter_mut()
            .find(|installed| installed.profile.id == id)
        else {
            return false;
        };
        if installed.profile.kind != ChargingProfileKind::Dynamic {
            return false;
        }
        if let Some(limit) = limit
            && let Some(period) = installed
                .profile
                .schedules
                .first_mut()
                .and_then(|schedule| schedule.periods.first_mut())
        {
            period.limit = limit;
        }
        installed.profile.dyn_update_time = Some(updated_at);
        true
    }

    /// Every installed dynamic profile that is due to pull a fresh limit at `now` - K28.FR.10:
    /// `chargingProfileKind = Dynamic`, `dynUpdateInterval > 0`, and `dynUpdateTime +
    /// dynUpdateInterval` reached.
    ///
    /// A profile whose `dyn_update_time` is somehow unset is treated as due: it has never been
    /// updated, so asking is exactly right.
    pub fn dynamic_pulls_due(&self, now: DateTime<Utc>) -> Vec<ChargingProfileId> {
        self.profiles
            .iter()
            .filter(|installed| installed.profile.kind == ChargingProfileKind::Dynamic)
            .filter(|installed| {
                let Some(interval) = installed
                    .profile
                    .dyn_update_interval_secs
                    .filter(|interval| *interval > 0)
                else {
                    return false;
                };
                installed.profile.dyn_update_time.is_none_or(|updated| {
                    now >= updated + chrono::Duration::seconds(i64::from(interval))
                })
            })
            .map(|installed| installed.profile.id)
            .collect()
    }

    /// Removes every profile matching `criteria`, returning how many were removed (`0` means the
    /// CSMS's `ClearChargingProfile` matched nothing, which OCPP reports as `Unknown`).
    pub fn clear(&mut self, criteria: &ChargingProfileCriteria) -> usize {
        let before = self.profiles.len();
        self.profiles
            .retain(|installed| !criteria.matches(installed));
        before - self.profiles.len()
    }

    /// Every profile matching `criteria`, in installation order - what `ClearChargingProfile`
    /// would remove.
    pub fn matching(&self, criteria: &ChargingProfileCriteria) -> Vec<&InstalledChargingProfile> {
        self.profiles
            .iter()
            .filter(|installed| criteria.matches(installed))
            .collect()
    }

    /// Every profile matching `query`, in installation order - what `GetChargingProfiles`
    /// reports. Separate from [`Self::matching`] for the reason [`ChargingProfileQuery`] is
    /// separate from [`ChargingProfileCriteria`].
    pub fn selected_by(&self, query: &ChargingProfileQuery) -> Vec<&InstalledChargingProfile> {
        self.profiles
            .iter()
            .filter(|installed| query.matches(installed))
            .collect()
    }

    /// Every profile that could apply to `evse_id` - those installed against that EVSE, plus every
    /// charge-point-wide one.
    pub fn applying_to(&self, evse_id: usize) -> Vec<&InstalledChargingProfile> {
        self.profiles
            .iter()
            .filter(|installed| match installed.scope {
                ChargingProfileScope::ChargePoint => true,
                ChargingProfileScope::Evse(id) => id == evse_id,
            })
            .collect()
    }
}

/// D2.3 re-measurement: the wire types' by-value size against `ocpp-types` 0.3.0, the version
/// actually pinned by this crate's `Cargo.lock`.
///
/// The roadmap row's headline number ("2.1 `ChargingProfile` is 56 KB by value") **does
/// reproduce** here (measured 50,584 bytes - see `docs/MEMORY.md`'s D2.3 section for the full
/// table), but its stated mechanism is only part of the story. This crate (via `ocpp-client`)
/// always builds `ocpp-types` with its `alloc` feature on (`ocpp-client`'s `Cargo.toml` requests
/// `features = ["serde", "alloc"]` unconditionally, `default-features = false`), and Cargo
/// feature unification means that applies everywhere this crate is built, including
/// `--no-default-features` MCU builds (no_std **+ alloc**, never allocator-free per `CLAUDE.md`).
/// Under `alloc`, `ChargingSchedule`'s three inlined sub-schedules are already `Option<T>` over
/// `alloc::Vec`-backed lists rather than fixed-capacity `heapless` ones - so the *specific*
/// mechanism the roadmap named (a `heapless` cap on those top-level fields) is not what is
/// compiled here.
///
/// The dominant cost (~78%, see `most_of_the_size_is_custom_data_not_array_capacity` below) is this crate's own
/// `wire.rs` binding every nested `CustomDataType` generic to the concrete, ~256-byte
/// `CustomData` struct (`ocpp-client`'s generated methods require it at the top level, and one
/// type parameter cascades through the whole tree) rather than `ocpp-types`' own zero-sized
/// `NoCustomData` default - multiplied by every spec-bounded `heapless` array the ISO 15118-20
/// price-schedule subtree still carries regardless of the `alloc` feature (`TaxRule` x 10,
/// `AdditionalSelectedServices` x 5, `OverstayRule` x 5, `PriceRule` x 8). Full writeup and a
/// drafted (not filed) upstream report: `docs/MEMORY.md`, "D2.3".
#[cfg(test)]
mod size_measurements {
    use crate::wire::{v16, v21, v201};

    #[test]
    fn print_charging_profile_and_schedule_sizes_for_every_protocol_version() {
        // 2.1.
        let profile_21 = core::mem::size_of::<v21::common::ChargingProfile>();
        let schedule_21 = core::mem::size_of::<v21::common::ChargingSchedule>();
        let absolute_price_21 = core::mem::size_of::<v21::common::AbsolutePriceSchedule>();
        let price_level_21 = core::mem::size_of::<v21::common::PriceLevelSchedule>();
        let sales_tariff_21 = core::mem::size_of::<v21::common::SalesTariff>();
        // 2.0.1.
        let profile_201 = core::mem::size_of::<v201::common::ChargingProfile>();
        let schedule_201 = core::mem::size_of::<v201::common::ChargingSchedule>();
        // 1.6J.
        let profile_16 = core::mem::size_of::<v16::common::ChargingProfile>();
        let schedule_16 = core::mem::size_of::<v16::common::ChargingSchedule>();
        // This crate's own protocol-independent internal representation (`ChargingProfile` /
        // `ChargingSchedule` in this module) - what `ChargingProfileStore` actually retains.
        let internal_profile = core::mem::size_of::<super::ChargingProfile>();
        let internal_schedule = core::mem::size_of::<super::ChargingSchedule>();

        std::eprintln!(
            "D2.3 measured sizes (ocpp-types 0.3.0, alloc feature on, as this crate always \
             builds it):\n\
             \x20 2.1  ChargingProfile          = {profile_21} bytes\n\
             \x20 2.1  ChargingSchedule         = {schedule_21} bytes\n\
             \x20 2.1  AbsolutePriceSchedule    = {absolute_price_21} bytes\n\
             \x20 2.1  PriceLevelSchedule       = {price_level_21} bytes\n\
             \x20 2.1  SalesTariff              = {sales_tariff_21} bytes\n\
             \x20 2.0.1 ChargingProfile         = {profile_201} bytes\n\
             \x20 2.0.1 ChargingSchedule        = {schedule_201} bytes\n\
             \x20 1.6J ChargingProfile          = {profile_16} bytes\n\
             \x20 1.6J ChargingSchedule         = {schedule_16} bytes\n\
             \x20 internal ChargingProfile      = {internal_profile} bytes\n\
             \x20 internal ChargingSchedule     = {internal_schedule} bytes"
        );

        // The roadmap row's headline claim reproduces: 2.1's ChargingProfile is still tens of
        // kilobytes by value under the pinned 0.3.0. This is a regression guard in the other
        // direction from what might be expected - it fails if the type unexpectedly *shrinks*
        // far below its current cost, which would mean either this crate's `wire.rs` stopped
        // binding `CustomData` (worth knowing - it changes what D2.3's mitigation options are)
        // or a future `ocpp-types` upgrade already fixed the upstream shape (worth knowing, so
        // the docs/MEMORY.md writeup and the drafted upstream report can be retired). It also
        // fails on further *growth*, which would make the transient cost in `payload_limit.rs`'s
        // documented boundary worse than measured.
        assert!(
            (30_000..80_000).contains(&profile_21),
            "2.1 ChargingProfile is {profile_21} bytes, outside the last-measured 30-80 KB \
             range - re-run the D2.3 writeup in docs/MEMORY.md, the cause may have changed"
        );
        assert!(
            (10_000..25_000).contains(&schedule_21),
            "2.1 ChargingSchedule is {schedule_21} bytes, outside the last-measured range"
        );

        // This crate's own internal model is what `ChargingProfileStore` actually retains
        // (`StateLimits::max_charging_profiles`, default 16) - and it is unaffected by whatever
        // ocpp-types does, because it never stores the wire type at all (D2.3's central finding:
        // the store was never exposed to this cost). This bound is the one that matters for this
        // crate's own retained-memory budget.
        assert!(
            internal_profile < 500,
            "internal ChargingProfile ballooned to {internal_profile} bytes - the store's \
             retained-memory budget (docs/MEMORY.md) assumes this stays small"
        );
    }

    /// Isolates how much of D2.3's cost is `CustomData`'s ~256-byte `vendor_id` propagating
    /// through every nested `customData` field, versus `ocpp-types`' own array-capacity
    /// choices, by re-measuring the same 2.1 types with the generic bound to `NoCustomData`
    /// (a zero-sized type) instead of this crate's actual `wire.rs` binding.
    ///
    /// This is what most of D2.3's cost actually is: swapping only the `customData` binding
    /// should shrink `ChargingProfile` by well over half, because `CustomData` is repeated at
    /// dozens of nesting sites inside the ISO 15118-20 price-schedule subtree (see
    /// `docs/MEMORY.md`'s D2.3 section for the full breakdown and the drafted upstream report).
    #[test]
    fn most_of_the_size_is_custom_data_not_array_capacity() {
        use ocpp_client::ocpp_types::NoCustomData;
        use ocpp_client::ocpp_types::v21::common::{AbsolutePriceSchedule, ChargingProfile};

        let profile_with_custom_data = core::mem::size_of::<v21::common::ChargingProfile>();
        let profile_without = core::mem::size_of::<ChargingProfile<NoCustomData>>();
        let schedule_without = core::mem::size_of::<AbsolutePriceSchedule<NoCustomData>>();

        std::eprintln!(
            "ChargingProfile: {profile_with_custom_data} bytes with CustomData, \
             {profile_without} bytes with NoCustomData; AbsolutePriceSchedule<NoCustomData> = \
             {schedule_without} bytes"
        );

        // Removing only the CustomData binding should cut ChargingProfile by more than half -
        // last measured at ~78% (50,584 -> 11,240). A weaker-than-2x reduction here would mean
        // the compounding this crate's docs describe no longer applies and the writeup needs
        // re-checking.
        assert!(
            profile_without * 2 < profile_with_custom_data,
            "expected NoCustomData to at least halve ChargingProfile's size: \
             {profile_with_custom_data} bytes with CustomData vs {profile_without} without"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schedule(rate_unit: ChargingRateUnit, periods: &[(u32, f64)]) -> ChargingSchedule {
        ChargingSchedule {
            id: 1,
            start_schedule: None,
            duration_secs: None,
            rate_unit,
            min_charging_rate: None,
            periods: periods
                .iter()
                .map(|(start_period_secs, limit)| ChargingSchedulePeriod {
                    start_period_secs: *start_period_secs,
                    limit: *limit,
                    number_phases: None,
                })
                .collect(),
        }
    }

    fn profile(id: i32, purpose: ChargingProfilePurpose, stack_level: u32) -> ChargingProfile {
        ChargingProfile {
            id: ChargingProfileId(id),
            stack_level,
            purpose,
            kind: ChargingProfileKind::Absolute,
            recurrency: None,
            valid_from: None,
            valid_to: None,
            transaction_id: None,
            schedules: alloc::vec![schedule(ChargingRateUnit::Amps, &[(0, 16.0)])],
            dyn_update_interval_secs: None,
            dyn_update_time: None,
        }
    }

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap()
    }

    /// The shape OCPP's K28.FR.01/K28.FR.02 require: one schedule, one period, starting at 0.
    fn dynamic_profile(id: i32) -> ChargingProfile {
        ChargingProfile {
            kind: ChargingProfileKind::Dynamic,
            dyn_update_time: Some(at(0)),
            ..profile(id, ChargingProfilePurpose::TxDefault, 0)
        }
    }

    #[test]
    fn a_period_applies_from_its_start_until_the_next_one_begins() {
        let schedule = schedule(ChargingRateUnit::Amps, &[(0, 16.0), (3_600, 32.0)]);
        assert_eq!(schedule.limit_at(0).unwrap().limit, 16.0);
        assert_eq!(schedule.limit_at(3_599).unwrap().limit, 16.0);
        assert_eq!(schedule.limit_at(3_600).unwrap().limit, 32.0);
        assert_eq!(schedule.limit_at(100_000).unwrap().limit, 32.0);
    }

    #[test]
    fn a_schedule_does_not_apply_before_it_starts_or_after_its_duration() {
        let mut schedule = schedule(ChargingRateUnit::Amps, &[(60, 16.0)]);
        schedule.duration_secs = Some(600);
        // Before the first period begins, the schedule imposes nothing.
        assert!(schedule.limit_at(0).is_none());
        assert_eq!(schedule.limit_at(60).unwrap().limit, 16.0);
        assert_eq!(schedule.limit_at(599).unwrap().limit, 16.0);
        assert!(schedule.limit_at(600).is_none());
        assert!(schedule.limit_at(-1).is_none());
    }

    #[test]
    fn the_next_change_is_the_next_period_or_the_schedules_end() {
        let mut schedule = schedule(ChargingRateUnit::Amps, &[(0, 16.0), (3_600, 32.0)]);
        assert_eq!(schedule.next_change_after(0), Some(3_600));
        assert_eq!(schedule.next_change_after(3_600), None);

        schedule.duration_secs = Some(7_200);
        assert_eq!(schedule.next_change_after(3_600), Some(7_200));
        assert_eq!(schedule.next_change_after(7_200), None);

        // A duration that lands before the next period wins - the schedule stops applying first.
        schedule.duration_secs = Some(1_800);
        assert_eq!(schedule.next_change_after(0), Some(1_800));
    }

    #[test]
    fn validity_windows_are_inclusive_of_their_start_and_exclusive_of_their_end() {
        let from = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let to = DateTime::from_timestamp(1_800_003_600, 0).unwrap();
        let mut profile = profile(1, ChargingProfilePurpose::TxDefault, 0);
        profile.valid_from = Some(from);
        profile.valid_to = Some(to);

        assert!(!profile.is_valid_at(from - chrono::Duration::seconds(1)));
        assert!(profile.is_valid_at(from));
        assert!(profile.is_valid_at(to - chrono::Duration::seconds(1)));
        assert!(!profile.is_valid_at(to));
    }

    #[test]
    fn installing_a_profile_with_an_existing_id_replaces_it_even_at_another_scope() {
        let mut store = ChargingProfileStore::with_limit(10);
        store
            .install(
                ChargingProfileScope::ChargePoint,
                profile(7, ChargingProfilePurpose::ChargePointMax, 0),
            )
            .unwrap();
        store
            .install(
                ChargingProfileScope::Evse(1),
                profile(7, ChargingProfilePurpose::TxDefault, 3),
            )
            .unwrap();

        assert_eq!(store.len(), 1);
        assert_eq!(store.installed()[0].scope, ChargingProfileScope::Evse(1));
        assert_eq!(
            store.installed()[0].profile.purpose,
            ChargingProfilePurpose::TxDefault
        );
    }

    #[test]
    fn installing_into_the_same_scope_purpose_and_stack_level_replaces_the_previous_profile() {
        let mut store = ChargingProfileStore::with_limit(10);
        store
            .install(
                ChargingProfileScope::Evse(0),
                profile(1, ChargingProfilePurpose::TxDefault, 2),
            )
            .unwrap();
        store
            .install(
                ChargingProfileScope::Evse(0),
                profile(2, ChargingProfilePurpose::TxDefault, 2),
            )
            .unwrap();

        assert_eq!(store.len(), 1);
        assert_eq!(store.installed()[0].profile.id, ChargingProfileId(2));
    }

    #[test]
    fn a_different_stack_level_or_scope_is_a_different_slot_and_stacks() {
        let mut store = ChargingProfileStore::with_limit(10);
        store
            .install(
                ChargingProfileScope::Evse(0),
                profile(1, ChargingProfilePurpose::TxDefault, 1),
            )
            .unwrap();
        store
            .install(
                ChargingProfileScope::Evse(0),
                profile(2, ChargingProfilePurpose::TxDefault, 2),
            )
            .unwrap();
        store
            .install(
                ChargingProfileScope::Evse(1),
                profile(3, ChargingProfilePurpose::TxDefault, 1),
            )
            .unwrap();

        assert_eq!(store.len(), 3);
    }

    #[test]
    fn the_profile_bound_refuses_a_new_profile_but_never_a_replacement() {
        let mut store = ChargingProfileStore::with_limit(2);
        store
            .install(
                ChargingProfileScope::Evse(0),
                profile(1, ChargingProfilePurpose::TxDefault, 1),
            )
            .unwrap();
        store
            .install(
                ChargingProfileScope::Evse(0),
                profile(2, ChargingProfilePurpose::TxDefault, 2),
            )
            .unwrap();

        assert_eq!(
            store.install(
                ChargingProfileScope::Evse(0),
                profile(3, ChargingProfilePurpose::TxDefault, 3)
            ),
            Err(ChargingProfileRejection::TooManyProfiles)
        );
        // Replacing one of the two already installed still works at the bound.
        assert_eq!(
            store.install(
                ChargingProfileScope::Evse(0),
                profile(1, ChargingProfilePurpose::TxDefault, 1)
            ),
            Ok(())
        );
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn a_profile_with_no_schedule_is_refused_rather_than_installed_uselessly() {
        let mut store = ChargingProfileStore::with_limit(10);
        let mut empty = profile(1, ChargingProfilePurpose::TxDefault, 0);
        empty.schedules.clear();
        assert_eq!(
            store.install(ChargingProfileScope::Evse(0), empty),
            Err(ChargingProfileRejection::NoSchedule)
        );
    }

    #[test]
    fn a_transaction_profile_cannot_be_installed_charge_point_wide() {
        let mut store = ChargingProfileStore::with_limit(10);
        assert!(matches!(
            store.install(
                ChargingProfileScope::ChargePoint,
                profile(1, ChargingProfilePurpose::Tx, 0)
            ),
            Err(ChargingProfileRejection::ScopeNotAllowedForPurpose(_))
        ));
    }

    #[test]
    fn clearing_with_no_criteria_at_all_clears_everything() {
        let mut store = ChargingProfileStore::with_limit(10);
        store
            .install(
                ChargingProfileScope::Evse(0),
                profile(1, ChargingProfilePurpose::TxDefault, 1),
            )
            .unwrap();
        store
            .install(
                ChargingProfileScope::ChargePoint,
                profile(2, ChargingProfilePurpose::ChargePointMax, 1),
            )
            .unwrap();

        assert_eq!(store.clear(&ChargingProfileCriteria::default()), 2);
        assert!(store.is_empty());
    }

    #[test]
    fn every_criterion_must_match_for_a_profile_to_be_cleared() {
        let mut store = ChargingProfileStore::with_limit(10);
        store
            .install(
                ChargingProfileScope::Evse(0),
                profile(1, ChargingProfilePurpose::TxDefault, 1),
            )
            .unwrap();
        store
            .install(
                ChargingProfileScope::Evse(1),
                profile(2, ChargingProfilePurpose::TxDefault, 1),
            )
            .unwrap();

        // Purpose matches both, but the scope narrows it to one.
        assert_eq!(
            store.clear(&ChargingProfileCriteria {
                purpose: Some(ChargingProfilePurpose::TxDefault),
                scope: Some(ChargingProfileScope::Evse(1)),
                ..Default::default()
            }),
            1
        );
        assert_eq!(store.len(), 1);
        assert_eq!(store.installed()[0].profile.id, ChargingProfileId(1));

        // Nothing matches: the CSMS gets "Unknown" rather than a silent success.
        assert_eq!(
            store.clear(&ChargingProfileCriteria {
                id: Some(ChargingProfileId(99)),
                ..Default::default()
            }),
            0
        );
    }

    #[test]
    fn profiles_applying_to_an_evse_include_the_charge_point_wide_ones() {
        let mut store = ChargingProfileStore::with_limit(10);
        store
            .install(
                ChargingProfileScope::ChargePoint,
                profile(1, ChargingProfilePurpose::ChargePointMax, 0),
            )
            .unwrap();
        store
            .install(
                ChargingProfileScope::Evse(0),
                profile(2, ChargingProfilePurpose::TxDefault, 0),
            )
            .unwrap();
        store
            .install(
                ChargingProfileScope::Evse(1),
                profile(3, ChargingProfilePurpose::TxDefault, 0),
            )
            .unwrap();

        let applying: Vec<i32> = store
            .applying_to(0)
            .iter()
            .map(|installed| installed.profile.id.0)
            .collect();
        assert_eq!(applying, alloc::vec![1, 2]);
    }

    #[test]
    fn a_schedule_is_selected_by_its_rate_unit_not_converted_into_one() {
        let mut watts = profile(1, ChargingProfilePurpose::TxDefault, 0);
        watts.schedules = alloc::vec![schedule(ChargingRateUnit::Watts, &[(0, 7_400.0)])];
        assert!(watts.schedule_in(ChargingRateUnit::Amps).is_none());
        assert_eq!(
            watts.schedule_in(ChargingRateUnit::Watts).unwrap().periods[0].limit,
            7_400.0
        );
    }

    #[test]
    fn purposes_that_cap_the_result_are_exactly_the_installation_and_external_limits() {
        assert!(ChargingProfilePurpose::ChargePointMax.caps_the_result());
        assert!(ChargingProfilePurpose::ExternalConstraints.caps_the_result());
        assert!(!ChargingProfilePurpose::TxDefault.caps_the_result());
        assert!(!ChargingProfilePurpose::Tx.caps_the_result());
        assert!(!ChargingProfilePurpose::PriorityCharging.caps_the_result());
        assert!(!ChargingProfilePurpose::LocalGeneration.caps_the_result());
    }

    /// The claim `adds_to_the_result`'s docs make, as a test: every purpose is capping, competing
    /// or adding, and never two of those. A purpose that was both would make composition's answer
    /// depend on the order the two rules happened to be written in.
    #[test]
    fn every_purpose_caps_competes_or_adds_and_never_two_of_them() {
        for purpose in [
            ChargingProfilePurpose::ChargePointMax,
            ChargingProfilePurpose::TxDefault,
            ChargingProfilePurpose::Tx,
            ChargingProfilePurpose::ExternalConstraints,
            ChargingProfilePurpose::LocalGeneration,
            ChargingProfilePurpose::PriorityCharging,
        ] {
            assert!(
                !(purpose.caps_the_result() && purpose.adds_to_the_result()),
                "{purpose:?} both caps and adds"
            );
        }

        // And adding is exactly one purpose - the one whose whole point is that it is not a limit.
        assert!(ChargingProfilePurpose::LocalGeneration.adds_to_the_result());
        assert!(!ChargingProfilePurpose::ChargePointMax.adds_to_the_result());
        assert!(!ChargingProfilePurpose::ExternalConstraints.adds_to_the_result());
        assert!(!ChargingProfilePurpose::TxDefault.adds_to_the_result());
        assert!(!ChargingProfilePurpose::Tx.adds_to_the_result());
        assert!(!ChargingProfilePurpose::PriorityCharging.adds_to_the_result());
    }

    #[test]
    fn a_dynamic_profile_must_be_one_schedule_of_one_period_starting_immediately() {
        let mut store = ChargingProfileStore::with_limit(10);

        // Two schedules - K28.FR.01.
        let mut two_schedules = dynamic_profile(1);
        two_schedules
            .schedules
            .push(schedule(ChargingRateUnit::Watts, &[(0, 7_400.0)]));
        assert!(matches!(
            store.install(ChargingProfileScope::Evse(0), two_schedules),
            Err(ChargingProfileRejection::InvalidDynamicSchedule(_))
        ));

        // Two periods - K28.FR.01.
        let mut two_periods = dynamic_profile(1);
        two_periods.schedules =
            alloc::vec![schedule(ChargingRateUnit::Amps, &[(0, 16.0), (60, 32.0)])];
        assert!(matches!(
            store.install(ChargingProfileScope::Evse(0), two_periods),
            Err(ChargingProfileRejection::InvalidDynamicSchedule(_))
        ));

        // A period that does not start at 0 - K28.FR.02. A dynamic limit takes effect on receipt;
        // one starting later is a scheduled profile wearing the wrong kind.
        let mut late_start = dynamic_profile(1);
        late_start.schedules = alloc::vec![schedule(ChargingRateUnit::Amps, &[(60, 16.0)])];
        assert!(matches!(
            store.install(ChargingProfileScope::Evse(0), late_start),
            Err(ChargingProfileRejection::InvalidDynamicSchedule(_))
        ));

        assert_eq!(
            store.install(ChargingProfileScope::Evse(0), dynamic_profile(1)),
            Ok(())
        );
    }

    #[test]
    fn a_dyn_update_interval_on_a_scheduled_profile_is_refused() {
        let mut store = ChargingProfileStore::with_limit(10);
        let mut scheduled = profile(1, ChargingProfilePurpose::TxDefault, 0);
        scheduled.dyn_update_interval_secs = Some(60);

        // K28.FR.04: the field only means anything on a dynamic profile, so accepting it here
        // would leave the CSMS believing this profile would be refreshed when nothing will.
        assert_eq!(
            store.install(ChargingProfileScope::Evse(0), scheduled),
            Err(ChargingProfileRejection::DynUpdateIntervalOnNonDynamicProfile)
        );
    }

    #[test]
    fn a_dynamic_update_replaces_the_single_periods_limit_and_moves_the_anchor() {
        let mut store = ChargingProfileStore::with_limit(10);
        store
            .install(ChargingProfileScope::Evse(0), dynamic_profile(1))
            .unwrap();

        assert!(store.apply_dynamic_update(ChargingProfileId(1), Some(24.0), at(300)));

        let installed = &store.installed()[0].profile;
        assert_eq!(installed.schedules[0].periods[0].limit, 24.0);
        assert_eq!(installed.dyn_update_time, Some(at(300)));
    }

    #[test]
    fn an_update_carrying_nothing_this_crate_can_project_still_moves_the_anchor() {
        let mut store = ChargingProfileStore::with_limit(10);
        store
            .install(ChargingProfileScope::Evse(0), dynamic_profile(1))
            .unwrap();

        // A setpoint-only update: the CSMS answered, so the K28.FR.13 deadline must reset even
        // though the limit this crate projects is unchanged.
        assert!(store.apply_dynamic_update(ChargingProfileId(1), None, at(300)));

        let installed = &store.installed()[0].profile;
        assert_eq!(installed.schedules[0].periods[0].limit, 16.0);
        assert_eq!(installed.dyn_update_time, Some(at(300)));
    }

    #[test]
    fn a_dynamic_update_for_a_scheduled_or_absent_profile_is_refused() {
        let mut store = ChargingProfileStore::with_limit(10);
        store
            .install(
                ChargingProfileScope::Evse(0),
                profile(1, ChargingProfilePurpose::TxDefault, 0),
            )
            .unwrap();

        // K28.FR.11: updating a scheduled profile in place would rewrite a curve the CSMS laid
        // out deliberately.
        assert!(!store.apply_dynamic_update(ChargingProfileId(1), Some(24.0), at(300)));
        assert_eq!(
            store.installed()[0].profile.schedules[0].periods[0].limit,
            16.0
        );

        assert!(!store.apply_dynamic_update(ChargingProfileId(99), Some(24.0), at(300)));
    }

    #[test]
    fn a_dynamic_profile_stops_applying_once_its_updates_go_stale() {
        let mut dynamic = dynamic_profile(1);
        dynamic.schedules[0].duration_secs = Some(600);
        dynamic.dyn_update_time = Some(at(0));

        assert!(dynamic.is_valid_at(at(599)));
        assert!(dynamic.is_valid_at(at(600)));
        // K28.FR.13: past the deadline the CSMS set itself, the last limit is no longer trusted.
        assert!(!dynamic.is_valid_at(at(601)));

        // K28.FR.14: a fresh update makes it eligible again, with no reinstall.
        dynamic.dyn_update_time = Some(at(600));
        assert!(dynamic.is_valid_at(at(601)));
    }

    #[test]
    fn a_dynamic_profile_with_no_duration_never_goes_stale() {
        let dynamic = dynamic_profile(1);
        assert_eq!(dynamic.schedules[0].duration_secs, None);

        // No duration is the CSMS setting no deadline - there is nothing to miss.
        assert!(dynamic.is_valid_at(at(10_000_000)));
    }

    #[test]
    fn only_dynamic_profiles_with_a_positive_interval_are_ever_pulled() {
        let mut store = ChargingProfileStore::with_limit(10);
        // Pushed, not pulled: no interval at all.
        store
            .install(ChargingProfileScope::Evse(0), dynamic_profile(1))
            .unwrap();
        // Explicitly zero, which OCPP's K28.FR.10 reads the same way.
        let mut zero = dynamic_profile(2);
        zero.stack_level = 1;
        zero.dyn_update_interval_secs = Some(0);
        store.install(ChargingProfileScope::Evse(0), zero).unwrap();
        // A scheduled profile can never carry an interval (K28.FR.04), so none is pullable.
        store
            .install(
                ChargingProfileScope::Evse(1),
                profile(3, ChargingProfilePurpose::TxDefault, 0),
            )
            .unwrap();
        let mut pulled = dynamic_profile(4);
        pulled.stack_level = 2;
        pulled.dyn_update_interval_secs = Some(60);
        store
            .install(ChargingProfileScope::Evse(0), pulled)
            .unwrap();

        assert!(store.dynamic_pulls_due(at(59)).is_empty());
        assert_eq!(
            store.dynamic_pulls_due(at(60)),
            alloc::vec![ChargingProfileId(4)]
        );

        // Once answered, the next pull is an interval away rather than every sweep.
        store.apply_dynamic_update(ChargingProfileId(4), Some(24.0), at(60));
        assert!(store.dynamic_pulls_due(at(119)).is_empty());
        assert_eq!(
            store.dynamic_pulls_due(at(120)),
            alloc::vec![ChargingProfileId(4)]
        );
    }
}
