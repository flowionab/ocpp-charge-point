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
    /// The periods, which [`ChargingProfile::new`] keeps sorted by `start_period_secs`.
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
}

impl ChargingProfile {
    /// The schedule expressed in `unit`, or `None` if this profile has none in that unit. See
    /// [`ChargingRateUnit`] for why this crate refuses to convert between units on its own.
    pub fn schedule_in(&self, unit: ChargingRateUnit) -> Option<&ChargingSchedule> {
        self.schedules
            .iter()
            .find(|schedule| schedule.rate_unit == unit)
    }

    /// Whether this profile applies at `now`, per its `valid_from`/`valid_to` window alone. Says
    /// nothing about whether any of its schedules covers `now` - that's
    /// [`ChargingSchedule::limit_at`].
    pub fn is_valid_at(&self, now: DateTime<Utc>) -> bool {
        self.valid_from.is_none_or(|from| now >= from) && self.valid_to.is_none_or(|to| now < to)
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
    pub fn install(
        &mut self,
        scope: ChargingProfileScope,
        profile: ChargingProfile,
    ) -> Result<(), ChargingProfileRejection> {
        if profile.schedules.is_empty() {
            return Err(ChargingProfileRejection::NoSchedule);
        }
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

    /// Removes every profile matching `criteria`, returning how many were removed (`0` means the
    /// CSMS's `ClearChargingProfile` matched nothing, which OCPP reports as `Unknown`).
    pub fn clear(&mut self, criteria: &ChargingProfileCriteria) -> usize {
        let before = self.profiles.len();
        self.profiles
            .retain(|installed| !criteria.matches(installed));
        before - self.profiles.len()
    }

    /// Every profile matching `criteria`, in installation order - what `GetChargingProfiles`
    /// reports.
    pub fn matching(&self, criteria: &ChargingProfileCriteria) -> Vec<&InstalledChargingProfile> {
        self.profiles
            .iter()
            .filter(|installed| criteria.matches(installed))
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
    }
}
