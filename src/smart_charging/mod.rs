//! Smart Charging functional block: charging profiles, composite schedules, and the current limit
//! they project onto hardware. See `docs/ROADMAP.md` §11 and `docs/PRODUCTION-ROADMAP.md` B2.
//!
//! The pieces, in the order a limit travels through them:
//!
//! 1. The CSMS installs a profile (`SetChargingProfile`), which lands in
//!    [`crate::state::ChargingProfileStore`] via
//!    [`crate::state::ChargePointEvent::ChargingProfileSet`] (B2.1) - or an external system (a
//!    local energy manager, a DSO signal) imposes a limit through
//!    [`crate::state::ChargePointEvent::ExternalChargingLimitSet`], which [`composing_profiles`]
//!    joins onto the installed ones as a capping profile.
//! 2. [`compose`] turns whatever profiles apply to a connector into one [`CompositeSchedule`] -
//!    the single limit curve that results from purpose precedence, stack levels and capping
//!    profiles (B2.2). It is a pure function of the profiles and a [`CompositionContext`], so
//!    every rule below is testable without a clock, a CSMS or hardware.
//! 3. [`run_charging_limit_projection`] evaluates that curve for every connector with a
//!    transaction and pushes the result into the state machine as
//!    [`crate::state::ConnectorEvent::CurrentLimitComputed`], which dispatches
//!    [`crate::hardware::Connector::set_current_limit`] when - and only when - the limit actually
//!    changed (B2.4).
//!
//! # What this module deliberately does not decide
//!
//! Converting between amps and watts needs the supply voltage and phase count, which is hardware
//! knowledge this crate does not have and will not guess: a caller that wants conversion supplies
//! [`SupplyCharacteristics`], and a caller that doesn't gets profiles in the wrong unit skipped
//! (logged) rather than silently mis-scaled. See [`CompositionContext::supply`].

use alloc::vec::Vec;
use chrono::{DateTime, Duration, Utc};

use crate::state::{
    ChargePointState, ChargingProfile, ChargingProfileId, ChargingProfileKind,
    ChargingProfilePurpose, ChargingProfileScope, ChargingRateUnit, ChargingSchedule,
    ChargingSchedulePeriod, ExternalChargingLimit, InstalledChargingProfile, TransactionId,
};

mod handlers;
pub mod notifications;
#[cfg(feature = "ocpp_1_6")]
mod ocpp_1_6;
#[cfg(feature = "ocpp_2_0_1")]
mod ocpp_2_0_1;
#[cfg(feature = "ocpp_2_1")]
mod ocpp_2_1;
mod projection;

#[cfg(test)]
mod tests;

#[cfg(feature = "ocpp_1_6")]
pub use self::ocpp_1_6::Ocpp1_6SmartChargingHandler;
#[cfg(feature = "ocpp_2_0_1")]
pub use self::ocpp_2_0_1::Ocpp2_0_1SmartChargingHandler;
#[cfg(feature = "ocpp_2_1")]
pub use self::ocpp_2_1::Ocpp2_1SmartChargingHandler;

pub use self::handlers::{
    CHARGING_PROFILE_REPORT_CHUNK_SIZE, ChargingProfileReportChunk, ClearChargingProfileHandler,
    ClearChargingProfileOutcome, DynamicSchedulePuller, DynamicScheduleUpdate,
    GetChargingProfilesHandler, GetCompositeScheduleHandler, GetCompositeScheduleOutcome,
    PriorityChargingNotifier, SetChargingProfileHandler, SetChargingProfileOutcome,
    SetChargingProfileRejection, UpdateDynamicScheduleHandler, UpdateDynamicScheduleOutcome,
    UsePriorityChargingHandler, UsePriorityChargingOutcome, chunk_charging_profile_report,
    handle_clear_charging_profile, handle_get_charging_profiles, handle_get_composite_schedule,
    handle_set_charging_profile, handle_update_dynamic_schedule, handle_use_priority_charging,
    run_dynamic_schedule_pulls, run_priority_charging_notifications,
};
pub use self::notifications::{
    ChargingLimitNotifier, EVChargingNeedsOutcome, EVChargingNotifier, GenericNotifyOutcome,
    run_smart_charging_notifications,
};
pub use self::projection::{
    ChargingLimitProjection, connector_composition_context, run_charging_limit_projection,
    run_charging_limit_schedule,
};

/// How many boundaries [`compose`] will evaluate before giving up on a pathological profile set.
///
/// Composition walks the instants at which any profile's limit changes, and that set is bounded in
/// practice by the number of schedule periods installed. This cap exists so a CSMS that installs
/// profiles with thousands of periods (or a recurrence that repeats every second across a
/// month-long window) cannot make an embedded charge point spin: the composite is truncated at
/// that point and the truncation is logged, rather than the loop running unbounded on a device
/// that has other work to do.
const MAX_COMPOSITION_BOUNDARIES: usize = 512;

/// The supply characteristics needed to convert between [`ChargingRateUnit::Amps`] and
/// [`ChargingRateUnit::Watts`] - `watts = amps × nominal_voltage_v × phases`.
///
/// Supplied by the caller (ultimately the integrator, who knows the installation) rather than
/// assumed: a charge point that assumed 230 V single-phase on a 400 V three-phase supply would
/// under-limit by a factor of five, and one that assumed the reverse would over-limit by the same
/// factor - the second being a safety problem, not a billing one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupplyCharacteristics {
    /// Nominal line voltage, in volts.
    pub nominal_voltage_v: u32,
    /// Number of phases the connector supplies.
    pub phases: u8,
}

impl SupplyCharacteristics {
    /// The factor between amps and watts for this supply.
    fn watts_per_amp(&self) -> f64 {
        f64::from(self.nominal_voltage_v) * f64::from(self.phases)
    }

    /// Converts `limit` from `from` into `to`.
    fn convert(&self, limit: f64, from: ChargingRateUnit, to: ChargingRateUnit) -> f64 {
        match (from, to) {
            (ChargingRateUnit::Amps, ChargingRateUnit::Watts) => limit * self.watts_per_amp(),
            (ChargingRateUnit::Watts, ChargingRateUnit::Amps) => {
                let factor = self.watts_per_amp();
                if factor <= 0.0 { limit } else { limit / factor }
            }
            _ => limit,
        }
    }
}

/// Everything [`compose`] needs to know beyond the profiles themselves.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompositionContext {
    /// The instant composition starts from.
    pub now: DateTime<Utc>,
    /// The transaction currently running on the connector being composed for, if any.
    /// Transaction-scoped purposes ([`ChargingProfilePurpose::Tx`],
    /// [`ChargingProfilePurpose::TxDefault`], [`ChargingProfilePurpose::PriorityCharging`]) only
    /// apply while one is.
    pub transaction_id: Option<TransactionId>,
    /// When that transaction started - the anchor a [`ChargingProfileKind::Relative`] schedule is
    /// measured from.
    ///
    /// Supplied by the caller rather than read from [`crate::state::Transaction`], which carries
    /// no timestamps by design: stamping one needs a [`crate::clock::Clock`], and the state
    /// machine is deliberately clock-free (see [`crate::clock`] and
    /// [`crate::persistence::PersistedTransaction::started_at`], which makes the same split).
    /// `None` means the charge point does not know, and every `Relative` profile is then skipped
    /// rather than anchored to a guess.
    pub transaction_started_at: Option<DateTime<Utc>>,
    /// The unit to express the composite in. Profiles whose schedules are in the other unit are
    /// converted when [`Self::supply`] is set, and skipped when it isn't.
    pub rate_unit: ChargingRateUnit,
    /// How far ahead to compose, in seconds. `GetCompositeSchedule` passes the CSMS's requested
    /// duration; the projection passes a short window, since it only needs the limit now and the
    /// instant it next changes.
    pub duration_secs: u32,
    /// The supply characteristics for unit conversion, if the caller knows them - see
    /// [`SupplyCharacteristics`].
    pub supply: Option<SupplyCharacteristics>,
    /// Whether priority charging is currently active for the transaction on this connector (2.1's
    /// `UsePriorityCharging`, B2.6).
    ///
    /// [`ChargingProfilePurpose::PriorityCharging`] profiles contribute only while this is `true`.
    /// Installation is not activation: a priority-charging profile sits inert until the CSMS asks
    /// for it on a named transaction, which is what makes it a *grant* rather than just another
    /// stack level. Always `false` on 1.6J and 2.0.1, neither of which can express the purpose or
    /// the request - see [`crate::state::Transaction::priority_charging`].
    pub priority_charging: bool,
}

/// The single limit curve that results from composing every applicable profile - what
/// `GetCompositeSchedule` reports, and what the projection reads "the limit right now" out of.
#[derive(Debug, Clone, PartialEq)]
pub struct CompositeSchedule {
    /// When the composite starts. Never earlier than [`CompositionContext::now`], and later than
    /// it when nothing limits the connector yet (a profile whose schedule or validity window
    /// starts in the future).
    pub start: DateTime<Utc>,
    /// How long the composite covers, in seconds from [`Self::start`].
    pub duration_secs: u32,
    /// The unit every limit below is expressed in - always [`CompositionContext::rate_unit`].
    pub rate_unit: ChargingRateUnit,
    /// The composed periods, relative to [`Self::start`], with consecutive equal limits merged.
    pub periods: Vec<ChargingSchedulePeriod>,
    /// Whether the composite ends because nothing limits the connector after it (`true`), or
    /// merely because [`CompositionContext::duration_secs`] ran out (`false`).
    ///
    /// The difference matters to [`Self::next_change_after`], and through it to the projection's
    /// timer: the end of a *limit* is an instant hardware must be told about, while the end of the
    /// caller's *window* is an artefact of how far ahead it happened to ask.
    pub ends_limit: bool,
    /// The highest minimum charging rate any contributing schedule declared, in [`Self::rate_unit`].
    /// Reported for the CSMS's benefit; it never raises a composed limit - see
    /// [`ChargingSchedule::min_charging_rate`].
    pub min_charging_rate: Option<f64>,
}

impl CompositeSchedule {
    /// The limit in effect at `at`, or `None` if the composite doesn't cover that instant.
    pub fn limit_at(&self, at: DateTime<Utc>) -> Option<&ChargingSchedulePeriod> {
        let elapsed = (at - self.start).num_seconds();
        if elapsed < 0 || elapsed >= i64::from(self.duration_secs) {
            return None;
        }
        self.periods
            .iter()
            .rfind(|period| i64::from(period.start_period_secs) <= elapsed)
    }

    /// When the composed limit next changes after `at` - the next period boundary, or the end of
    /// the composite, whichever comes first. `None` if nothing further changes within it.
    ///
    /// This is what lets [`run_charging_limit_projection`] sleep until exactly the next boundary
    /// instead of polling (B2.4's "re-evaluated on schedule period boundary").
    pub fn next_change_after(&self, at: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let elapsed = (at - self.start).num_seconds();
        let next_period = self
            .periods
            .iter()
            .map(|period| i64::from(period.start_period_secs))
            .find(|start| *start > elapsed);
        let end = i64::from(self.duration_secs);
        let next = match (next_period, self.ends_limit) {
            (Some(next), true) => next.min(end),
            (Some(next), false) => next,
            (None, true) if end > elapsed => end,
            _ => return None,
        };
        Some(self.start + Duration::seconds(next))
    }
}

/// One profile's contribution at a single instant.
struct Contribution {
    limit: f64,
    number_phases: Option<u8>,
    min_charging_rate: Option<f64>,
}

/// Composes every profile in `profiles` into a single [`CompositeSchedule`], or `None` if none of
/// them limits the connector anywhere within the requested window.
///
/// The rules, in the order they apply at each instant:
///
/// 1. **Applicability** - a profile contributes only inside its `valid_from`/`valid_to` window,
///    only if its schedule (anchored per [`ChargingProfileKind`]) covers the instant, and, for
///    transaction-scoped purposes, only while a transaction is running. A
///    [`ChargingProfilePurpose::Tx`] profile naming a *different* transaction never contributes.
/// 2. **Selection** - among the applicable non-capping purposes, the highest
///    [`ChargingProfilePurpose`] wins (so a `TxProfile` beats a `TxDefaultProfile` whatever their
///    stack levels), and within one purpose the highest `stack_level` wins.
/// 3. **Capping** - [`ChargingProfilePurpose::caps_the_result`] purposes (the installation limit
///    and external constraints) are then applied as upper bounds, lowest binding. A cap with
///    nothing to cap *is* the limit: an installation limit with no transaction profile still
///    limits the connector.
///
/// Consecutive instants with the same resulting limit are merged, so a cap that changes without
/// ever binding doesn't split the composite into two identical periods.
pub fn compose(
    profiles: &[&InstalledChargingProfile],
    context: &CompositionContext,
) -> Option<CompositeSchedule> {
    let window_end = context.now + Duration::seconds(i64::from(context.duration_secs));
    let boundaries = composition_boundaries(profiles, context, window_end);

    let mut periods: Vec<ChargingSchedulePeriod> = Vec::new();
    let mut min_charging_rate: Option<f64> = None;
    let mut start: Option<DateTime<Utc>> = None;
    let mut end = window_end;
    let mut ends_limit = false;

    for instant in boundaries {
        let resolved = limit_at(profiles, context, instant);
        let Some(resolved) = resolved else {
            // Nothing limits the connector here. If a limit was in effect, the composite ends at
            // this instant - and ends because the *limit* did, not because the window ran out; if
            // none ever was, the composite simply hasn't started yet.
            if start.is_some() {
                end = instant;
                ends_limit = true;
                break;
            }
            continue;
        };
        let composite_start = *start.get_or_insert(instant);
        if let Some(rate) = resolved.min_charging_rate {
            min_charging_rate =
                Some(min_charging_rate.map_or(rate, |current: f64| current.max(rate)));
        }
        let start_period_secs = (instant - composite_start).num_seconds().max(0) as u32;
        let same_as_previous = periods.last().is_some_and(|previous| {
            previous.limit == resolved.limit && previous.number_phases == resolved.number_phases
        });
        if !same_as_previous {
            periods.push(ChargingSchedulePeriod {
                start_period_secs,
                limit: resolved.limit,
                number_phases: resolved.number_phases,
            });
        }
    }

    let start = start?;
    let duration_secs = (end - start).num_seconds().max(0) as u32;
    Some(CompositeSchedule {
        start,
        duration_secs,
        rate_unit: context.rate_unit,
        periods,
        ends_limit,
        min_charging_rate,
    })
}

/// The composed limit in effect *right now*, in milliamps - what
/// [`run_charging_limit_projection`] pushes at hardware.
///
/// `None` means no installed profile limits this connector at this instant, which
/// [`crate::hardware::Connector::set_current_limit`] renders as "remove any applied limit". A
/// composed limit of zero is `Some(0)`, not `None`: OCPP uses a 0 A period to suspend charging
/// without ending the transaction, and collapsing that into "no limit" would let the EV draw
/// whatever it likes - the exact opposite of what the CSMS asked for.
///
/// A negative limit (which no valid profile should contain) is clamped to zero rather than
/// wrapping through the `u32` conversion.
pub fn current_limit_ma(
    profiles: &[&InstalledChargingProfile],
    context: &CompositionContext,
) -> Option<u32> {
    let amps_context = CompositionContext {
        rate_unit: ChargingRateUnit::Amps,
        ..*context
    };
    let resolved = limit_at(profiles, &amps_context, context.now)?;
    // `f64::round` lives in `std`, and this crate compiles for bare metal - so round by adding a
    // half before the (truncating) cast, which is equivalent for the non-negative values the
    // clamp below leaves, and needs no `libm` dependency.
    let milliamps = (resolved.limit * 1_000.0).clamp(0.0, f64::from(u32::MAX));
    Some((milliamps + 0.5) as u32)
}

/// The profile id a limit from an external system composes under.
///
/// Negative, and therefore outside the range a CSMS uses (OCPP profile ids are non-negative), so
/// that a synthetic profile can share a list with real installed ones inside [`compose`] without
/// ever colliding with one. Nothing reports these ids: an external limit is not an installed
/// charging profile, and `GetChargingProfiles` must go on saying so - see
/// [`crate::state::ChargingLimitSource`].
fn external_limit_profile_id(
    scope: ChargingProfileScope,
    is_local_generation: bool,
) -> ChargingProfileId {
    // Two limits can be in force on one scope at once - a constraint and locally generated
    // capacity (K27.FR.05) - so the id has to separate them too, or the pair would look like one
    // profile installed twice. Local generation takes the even half of the negative range and
    // constraints the odd half, which keeps both unbounded in the number of EVSEs they cover.
    let offset = i32::from(is_local_generation);
    match scope {
        ChargingProfileScope::ChargePoint => ChargingProfileId(-1 - offset),
        ChargingProfileScope::Evse(evse_id) => {
            let evse_id = i32::try_from(evse_id).unwrap_or(i32::MAX / 2);
            ChargingProfileId((-3i32).saturating_sub(evse_id.saturating_mul(2) + offset))
        }
    }
}

/// The profile an external limit composes as, or `None` when the limit carries no schedule and so
/// says nothing that *could* be enforced.
///
/// The purpose follows [`ExternalChargingLimit::is_local_generation`], and the two compose in
/// opposite directions. A constraint becomes [`ChargingProfilePurpose::ExternalConstraints`] -
/// exactly what it is, a constraint the station is subject to rather than one it chose - and that
/// purpose already [caps the composed result](ChargingProfilePurpose::caps_the_result), so the
/// limit lowers whatever the CSMS asked for and never raises it. Locally generated capacity
/// becomes [`ChargingProfilePurpose::LocalGeneration`], which
/// [adds to it](ChargingProfilePurpose::adds_to_the_result) instead: K27.FR.01 has the station
/// treat an external schedule that represents local generation as exactly that profile, and
/// §K.3.6 has that capacity added on top of the composite rather than bounding it.
///
/// [`ChargingProfileKind::Absolute`], so a limit whose schedule carries a `start_schedule` is
/// honoured to the second. A schedule *without* one anchors at "now" on every evaluation, which is
/// right for the usual case (one period, in force until withdrawn) and wrong for a multi-period
/// external schedule, which would restart rather than advance. An external system that wants a
/// time-bounded limit must therefore stamp `start_schedule` itself - unlike the wire adapters,
/// which stamp it on the CSMS's behalf, the state machine here is deliberately clock-free.
fn as_composing_profile(
    limit: &ExternalChargingLimit,
    scope: ChargingProfileScope,
) -> Option<InstalledChargingProfile> {
    let schedule = limit.schedule.clone()?;
    Some(InstalledChargingProfile {
        scope,
        profile: ChargingProfile {
            id: external_limit_profile_id(scope, limit.is_local_generation),
            stack_level: 0,
            purpose: if limit.is_local_generation {
                ChargingProfilePurpose::LocalGeneration
            } else {
                ChargingProfilePurpose::ExternalConstraints
            },
            kind: ChargingProfileKind::Absolute,
            recurrency: None,
            valid_from: None,
            valid_to: None,
            transaction_id: None,
            schedules: alloc::vec![schedule],
            dyn_update_interval_secs: None,
            dyn_update_time: None,
        },
    })
}

/// The limits an external system currently imposes on `evse_id` - the station-wide one and the
/// EVSE's own, whichever are in force - as the capping profiles [`compose`] reads them through.
///
/// Built on demand rather than stored, so that the one representation of an external limit stays
/// [`ExternalChargingLimit`]: it is what the integrator pushes in, what `NotifyChargingLimit`
/// reports, and what `ClearedChargingLimit` withdraws, and a second stored copy of it could drift
/// from that. The cost is one clone of the limit's schedule per evaluation, which is the same order
/// as the composition it feeds.
///
/// A limit carrying no schedule contributes nothing here - there is no number to enforce. That case
/// is reported to the CSMS (OCPP's own `chargingSchedule` is optional) and warned about where it is
/// recorded, rather than being silently treated as a limit of zero or as no limit at all.
pub fn external_charging_limits(
    state: &ChargePointState,
    evse_id: usize,
) -> Vec<InstalledChargingProfile> {
    let station_scope = ChargingProfileScope::ChargePoint;
    let evse_scope = ChargingProfileScope::Evse(evse_id);
    let evse = state.evses.get(evse_id);
    // All four slots this EVSE composes under: the station's constraint and capacity, and its
    // own. Each is independent of the others - a site under an EMS cap can still have sun on the
    // roof - so this is a chain rather than a choice.
    [
        state
            .station_external_charging_limit
            .as_ref()
            .and_then(|limit| as_composing_profile(limit, station_scope)),
        state
            .station_local_generation_limit
            .as_ref()
            .and_then(|limit| as_composing_profile(limit, station_scope)),
        evse.and_then(|evse| evse.external_charging_limit.as_ref())
            .and_then(|limit| as_composing_profile(limit, evse_scope)),
        evse.and_then(|evse| evse.local_generation_limit.as_ref())
            .and_then(|limit| as_composing_profile(limit, evse_scope)),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// Everything that limits `evse_id`: the profiles the CSMS installed, plus the capping profiles
/// standing for whatever an external system has imposed - what [`compose`] and [`current_limit_ma`]
/// must be given if the limit they produce is to be the one the charge point will actually apply
/// (OCPP K11.FR.01, K12.FR.01, K13.FR.01, K27.FR.01).
///
/// `external` is a parameter rather than something this function derives because the two halves
/// have different lifetimes: the installed profiles are borrowed from `state`, while the external
/// ones are built on demand and must outlive the borrow. Call it as:
///
/// ```ignore
/// let external = external_charging_limits(&state, evse_id);
/// let profiles = composing_profiles(&state, evse_id, &external);
/// ```
///
/// Public for the same reason [`connector_composition_context`] is: a CSMS asking
/// `GetCompositeSchedule` "what will you do" must be answered from the same profile set that
/// decides what the charge point actually does, not from a second, subtly different derivation.
pub fn composing_profiles<'a>(
    state: &'a ChargePointState,
    evse_id: usize,
    external: &'a [InstalledChargingProfile],
) -> Vec<&'a InstalledChargingProfile> {
    let mut profiles = state.charging_profiles.applying_to(evse_id);
    profiles.extend(external);
    profiles
}

/// Every instant within the window at which some profile's contribution could change: the window
/// start, each profile's validity edges, and each schedule's period boundaries (including the
/// recurrence repetitions that land inside the window). Sorted, deduplicated and bounded by
/// [`MAX_COMPOSITION_BOUNDARIES`].
fn composition_boundaries(
    profiles: &[&InstalledChargingProfile],
    context: &CompositionContext,
    window_end: DateTime<Utc>,
) -> Vec<DateTime<Utc>> {
    let mut boundaries = alloc::vec![context.now];
    for installed in profiles {
        let profile = &installed.profile;
        for edge in [profile.valid_from, profile.valid_to].into_iter().flatten() {
            if edge > context.now && edge < window_end {
                boundaries.push(edge);
            }
        }
        let Some(schedule) = schedule_for(profile, context) else {
            continue;
        };
        let mut anchor = match schedule_anchor(profile, schedule, context) {
            Some(anchor) => anchor,
            None => continue,
        };
        let recurrence = profile
            .recurrency
            .filter(|_| profile.kind == ChargingProfileKind::Recurring)
            .map(|kind| Duration::seconds(kind.period_secs()));
        loop {
            let mut instants = alloc::vec![anchor];
            instants.extend(
                schedule
                    .periods
                    .iter()
                    .map(|period| anchor + Duration::seconds(i64::from(period.start_period_secs))),
            );
            if let Some(duration) = schedule.duration_secs {
                instants.push(anchor + Duration::seconds(i64::from(duration)));
            }
            for instant in instants {
                if instant > context.now && instant < window_end {
                    boundaries.push(instant);
                }
            }
            match recurrence {
                Some(recurrence) if anchor + recurrence < window_end => anchor += recurrence,
                _ => break,
            }
            if boundaries.len() > MAX_COMPOSITION_BOUNDARIES {
                break;
            }
        }
    }
    boundaries.sort_unstable();
    boundaries.dedup();
    if boundaries.len() > MAX_COMPOSITION_BOUNDARIES {
        tracing::warn!(
            boundaries = boundaries.len(),
            cap = MAX_COMPOSITION_BOUNDARIES,
            "truncating composite schedule composition: the installed profiles change limits more \
             often than this charge point will evaluate"
        );
        boundaries.truncate(MAX_COMPOSITION_BOUNDARIES);
    }
    boundaries
}

/// The schedule of `profile` to use for `context`'s rate unit: the one already in that unit, or -
/// when the caller supplied [`SupplyCharacteristics`] - any schedule, converted. `None` when the
/// profile has nothing usable, which drops it from the composition (logged by the caller's own
/// tracing, not here, since this runs per boundary).
fn schedule_for<'a>(
    profile: &'a crate::state::ChargingProfile,
    context: &CompositionContext,
) -> Option<&'a ChargingSchedule> {
    profile
        .schedule_in(context.rate_unit)
        .or_else(|| context.supply.and(profile.schedules.first()))
}

/// When `schedule` starts, per its profile's [`ChargingProfileKind`]:
///
/// - [`ChargingProfileKind::Absolute`]: its own `start_schedule`, or - if the CSMS omitted one,
///   which 1.6J permits - the composition start, i.e. "from now". That fallback is a last resort,
///   not the intended path: it re-anchors on every evaluation, so a schedule with a `duration`
///   would restart instead of ending. The version adapters therefore stamp the installation
///   instant into `start_schedule` when the wire message leaves it out, which is the only place
///   with both a clock and the knowledge that this profile is arriving *now*.
/// - [`ChargingProfileKind::Recurring`]: `start_schedule` (required; without it there is nothing
///   to repeat from and the profile is skipped), rewound to the most recent repetition at or
///   before `at`.
/// - [`ChargingProfileKind::Relative`]: the transaction's start
///   ([`CompositionContext::transaction_started_at`]); skipped when that isn't known.
/// - [`ChargingProfileKind::Dynamic`]: its own
///   [`dyn_update_time`](crate::state::ChargingProfile::dyn_update_time) - the instant its single
///   period last took a value, which is when it became active (OCPP K28.FR.02: a dynamic period
///   applies on receipt). Falling back to `now` when that is somehow unset keeps the profile
///   applying rather than silently dropping the CSMS's live limit.
fn schedule_anchor(
    profile: &crate::state::ChargingProfile,
    schedule: &ChargingSchedule,
    context: &CompositionContext,
) -> Option<DateTime<Utc>> {
    match profile.kind {
        ChargingProfileKind::Absolute => Some(schedule.start_schedule.unwrap_or(context.now)),
        ChargingProfileKind::Relative => context.transaction_started_at,
        ChargingProfileKind::Recurring => schedule.start_schedule,
        ChargingProfileKind::Dynamic => Some(profile.dyn_update_time.unwrap_or(context.now)),
    }
}

/// How far into `schedule` the instant `at` is, honouring recurrence.
fn elapsed_into_schedule(
    profile: &crate::state::ChargingProfile,
    schedule: &ChargingSchedule,
    context: &CompositionContext,
    at: DateTime<Utc>,
) -> Option<i64> {
    let anchor = schedule_anchor(profile, schedule, context)?;
    let elapsed = (at - anchor).num_seconds();
    match (profile.kind, profile.recurrency) {
        (ChargingProfileKind::Recurring, Some(recurrency)) => {
            let period = recurrency.period_secs();
            if period <= 0 || elapsed < 0 {
                return Some(elapsed);
            }
            Some(elapsed % period)
        }
        _ => Some(elapsed),
    }
}

/// Whether `profile` can contribute at all given what is (or isn't) charging - see [`compose`]'s
/// rule 1.
fn applies_to_transaction(
    profile: &crate::state::ChargingProfile,
    context: &CompositionContext,
) -> bool {
    match profile.purpose {
        // Local generation is a fact about the site, not about the session: the capacity is there
        // whether or not a car is plugged in, so it qualifies the connector's limit the same way
        // an installation limit does (K27 - the EMS pushes it, and no transaction is mentioned).
        ChargingProfilePurpose::ChargePointMax
        | ChargingProfilePurpose::ExternalConstraints
        | ChargingProfilePurpose::LocalGeneration => true,
        ChargingProfilePurpose::Tx => match (profile.transaction_id, context.transaction_id) {
            (Some(profile_transaction), Some(running)) => profile_transaction == running,
            // A `TxProfile` with no transaction id applies to whatever is running on the
            // connector it was installed against, per the spec's own wording.
            (None, Some(_)) => true,
            (_, None) => false,
        },
        ChargingProfilePurpose::TxDefault => context.transaction_id.is_some(),
        // Priority charging needs both: a transaction to apply to, and a CSMS grant for it. See
        // [`CompositionContext::priority_charging`].
        ChargingProfilePurpose::PriorityCharging => {
            context.transaction_id.is_some() && context.priority_charging
        }
    }
}

/// The composed limit at one instant - [`compose`]'s rules 1-3 for a single point in time.
fn limit_at(
    profiles: &[&InstalledChargingProfile],
    context: &CompositionContext,
    at: DateTime<Utc>,
) -> Option<Contribution> {
    let mut selected: Option<(ChargingProfilePurpose, u32, Contribution)> = None;
    let mut cap: Option<f64> = None;
    // Locally generated capacity: the leading `LocalGeneration` profile's limit, by stack level,
    // kept apart from `selected` because it does not compete with the transaction profiles - it
    // widens whatever they and the caps settle on (K27, §K.3.6).
    let mut headroom: Option<(u32, f64)> = None;

    for installed in profiles {
        let profile = &installed.profile;
        if !profile.is_valid_at(at) || !applies_to_transaction(profile, context) {
            continue;
        }
        let Some(schedule) = schedule_for(profile, context) else {
            continue;
        };
        let Some(elapsed) = elapsed_into_schedule(profile, schedule, context, at) else {
            continue;
        };
        let Some(period) = schedule.limit_at(elapsed) else {
            continue;
        };
        let convert = |value: f64| match context.supply {
            Some(supply) => supply.convert(value, schedule.rate_unit, context.rate_unit),
            None => value,
        };
        let limit = convert(period.limit);

        if profile.purpose.caps_the_result() {
            cap = Some(cap.map_or(limit, |current: f64| current.min(limit)));
            continue;
        }
        if profile.purpose.adds_to_the_result() {
            // Stack level picks the leading one, the same rule every other purpose gets - two
            // local generation profiles are two answers to "how much is available", not two
            // sources to be added together.
            if headroom.is_none_or(|(stack_level, _)| profile.stack_level >= stack_level) {
                headroom = Some((profile.stack_level, limit));
            }
            continue;
        }
        let contribution = Contribution {
            limit,
            number_phases: period.number_phases,
            min_charging_rate: schedule.min_charging_rate.map(convert),
        };
        let wins = selected.as_ref().is_none_or(|(purpose, stack_level, _)| {
            (profile.purpose, profile.stack_level) > (*purpose, *stack_level)
        });
        if wins {
            selected = Some((profile.purpose, profile.stack_level, contribution));
        }
    }

    let composed = match (selected, cap) {
        (Some((_, _, contribution)), Some(cap)) => Some(Contribution {
            limit: contribution.limit.min(cap),
            ..contribution
        }),
        (Some((_, _, contribution)), None) => Some(contribution),
        // A cap with nothing to cap is itself the limit: an installation limit still limits the
        // connector when no transaction profile is installed at all.
        (None, Some(cap)) => Some(Contribution {
            limit: cap,
            number_phases: None,
            min_charging_rate: None,
        }),
        (None, None) => None,
    };

    // "If a charging profile of chargingProfilePurpose = LocalGeneration is active for the EVSE,
    // then this capacity is added on top of the calculated composite schedule" (§K.3.6) - *on top
    // of*, so after the caps rather than alongside them. An installation limit describes the grid
    // connection, and locally generated power does not arrive through it.
    //
    // Headroom with nothing to add to is not a limit, which is where this parts company with the
    // capping rule directly above: "20 A is the ceiling" still bounds an otherwise unlimited
    // connector, while "2 kW is available on site" says nothing about a ceiling at all.
    match (composed, headroom) {
        (Some(contribution), Some((_, headroom))) => Some(Contribution {
            limit: contribution.limit + headroom,
            ..contribution
        }),
        (composed, _) => composed,
    }
}
