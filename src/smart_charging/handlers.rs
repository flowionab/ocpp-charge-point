//! CSMS-initiated Smart Charging messages, protocol-agnostic: `SetChargingProfile`,
//! `ClearChargingProfile`, `GetCompositeSchedule` and `GetChargingProfiles`
//! (`docs/PRODUCTION-ROADMAP.md` B2.5).
//!
//! Each handler decides the outcome against the real store *before* dispatching anything into the
//! actor, so the status the CSMS receives is what actually happened rather than an optimistic
//! guess - the same discipline `crate::local_authorization_list` follows for `SendLocalList`.

use alloc::boxed::Box;
use alloc::vec::Vec;
use chrono::{DateTime, Utc};

use crate::actor::ChargePointActor;
use crate::clock::Clock;
use crate::smart_charging::{
    ChargingLimitProjection, CompositeSchedule, compose, connector_composition_context,
};
use crate::state::{
    ChargePointEvent, ChargingLimitSource, ChargingProfile, ChargingProfileCriteria,
    ChargingProfileQuery, ChargingProfileScope, ChargingRateUnit, InstalledChargingProfile,
};

/// The outcome of a CSMS-initiated `SetChargingProfile`, matching OCPP's
/// `ChargingProfileStatusEnum` (2.x) / `ChargingProfileStatus` (1.6J).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetChargingProfileOutcome {
    /// The profile was installed.
    Accepted,
    /// The profile was refused - because the scope doesn't address anything on this charge point,
    /// because the store is full, or because the profile itself is unusable. The reason is
    /// carried for logging, not for the wire: every version collapses refusal to a single
    /// `Rejected` status.
    Rejected(&'static str),
}

/// Handles a CSMS-initiated `SetChargingProfile` against `actor`.
///
/// Validation happens against a copy of the live store, so the CSMS's status reflects the real
/// replacement rules and the real
/// [`max_charging_profiles`](crate::state::StateLimits::max_charging_profiles) bound (B2.1). Only
/// a profile that would actually install is dispatched.
///
/// `installed_at` is used to stamp any schedule the CSMS left without a `start_schedule`: an
/// `Absolute` schedule with no start means "from when you received this", and the receiving
/// adapter is the only place that knows when that was - see
/// [`schedule_anchor`](crate::smart_charging) for what happens without it.
pub async fn handle_set_charging_profile(
    actor: &ChargePointActor,
    scope: ChargingProfileScope,
    mut profile: ChargingProfile,
    installed_at: DateTime<Utc>,
) -> SetChargingProfileOutcome {
    let state = actor.state();
    // C5 (docs/PRODUCTION-ROADMAP.md §5.5): registered whenever the `smart-charging` feature is
    // on, but the hardware may still declare the capability runtime-absent.
    if !crate::refusal::capability_present(&state.capabilities, "SetChargingProfile") {
        return SetChargingProfileOutcome::Rejected("smart charging is not available");
    }
    if let ChargingProfileScope::Evse(evse_id) = scope
        && evse_id >= state.evses.len()
    {
        return SetChargingProfileOutcome::Rejected("no such EVSE");
    }
    for schedule in &mut profile.schedules {
        schedule.start_schedule.get_or_insert(installed_at);
    }

    let mut trial = state.charging_profiles.clone();
    if let Err(rejection) = trial.install(scope, profile.clone()) {
        tracing::warn!(?rejection, "refusing a charging profile");
        return SetChargingProfileOutcome::Rejected("the charge point cannot hold this profile");
    }

    let _ = actor
        .send(ChargePointEvent::ChargingProfileSet {
            scope,
            profile: Box::new(profile),
        })
        .await;
    SetChargingProfileOutcome::Accepted
}

/// The outcome of a CSMS-initiated `ClearChargingProfile`, matching OCPP's
/// `ClearChargingProfileStatusEnum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClearChargingProfileOutcome {
    /// At least one profile matched and was cleared.
    Accepted,
    /// Nothing matched the criteria - OCPP's `Unknown`, which is not an error: the CSMS asked for
    /// profiles that this charge point doesn't have, which is exactly the state it wanted.
    Unknown,
}

/// Handles a CSMS-initiated `ClearChargingProfile` against `actor`. Reports `Unknown` when the
/// criteria match nothing, so a CSMS can tell "cleared" from "there was nothing to clear".
pub async fn handle_clear_charging_profile(
    actor: &ChargePointActor,
    criteria: ChargingProfileCriteria,
) -> ClearChargingProfileOutcome {
    let state = actor.state();
    if !crate::refusal::capability_present(&state.capabilities, "ClearChargingProfile") {
        return ClearChargingProfileOutcome::Unknown;
    }
    if state.charging_profiles.matching(&criteria).is_empty() {
        return ClearChargingProfileOutcome::Unknown;
    }
    let _ = actor
        .send(ChargePointEvent::ChargingProfilesCleared { criteria })
        .await;
    ClearChargingProfileOutcome::Accepted
}

/// The outcome of a CSMS-initiated `GetCompositeSchedule`.
#[derive(Debug, Clone, PartialEq)]
pub enum GetCompositeScheduleOutcome {
    /// The schedule was computed. `None` inside means no profile limits that EVSE at all over the
    /// requested window - which OCPP still reports as `Accepted`, with no schedule attached.
    Accepted(Option<CompositeSchedule>),
    /// The request doesn't address an EVSE on this charge point, or smart charging isn't
    /// available.
    Rejected,
}

/// Handles a CSMS-initiated `GetCompositeSchedule` against `actor`, composing exactly the same way
/// the projection that drives hardware does - see [`connector_composition_context`] for why that
/// sharing matters.
///
/// OCPP addresses this at EVSE granularity, so the composite is computed for that EVSE's first
/// connector; on a multi-connector EVSE the connectors share the EVSE's profiles, and what differs
/// between them (which transaction is running) is a distinction OCPP's request cannot express.
pub async fn handle_get_composite_schedule<C: Clock>(
    actor: &ChargePointActor,
    projection: &ChargingLimitProjection,
    clock: &C,
    evse_id: usize,
    duration_secs: u32,
    rate_unit: ChargingRateUnit,
) -> GetCompositeScheduleOutcome {
    let state = actor.state();
    if !crate::refusal::capability_present(&state.capabilities, "GetCompositeSchedule") {
        return GetCompositeScheduleOutcome::Rejected;
    }
    let Some(evse) = state.evses.get(evse_id) else {
        return GetCompositeScheduleOutcome::Rejected;
    };
    if evse.connectors.is_empty() {
        return GetCompositeScheduleOutcome::Rejected;
    }
    let mut context =
        connector_composition_context(projection, &state, evse_id, 0, clock.now(), duration_secs);
    context.rate_unit = rate_unit;
    let profiles = state.charging_profiles.applying_to(evse_id);
    GetCompositeScheduleOutcome::Accepted(compose(&profiles, &context))
}

/// Handles a CSMS-initiated `GetChargingProfiles` against `actor`, returning the profiles that
/// match - what the charge point then reports back in one or more `ReportChargingProfiles`.
///
/// Returns an empty vector when nothing matches, which the caller reports as OCPP's `NoProfiles`
/// status and sends **no** report for. That is the opposite of
/// [`crate::reporting::chunk_report`], which emits one empty `NotifyReport` - and the difference
/// is OCPP's, not this crate's: `GetBaseReport` has no "nothing matched" status to answer with, so
/// its emptiness has to be carried by a message, while `GetChargingProfiles` says it in the
/// response and a report afterwards would contradict it.
pub fn handle_get_charging_profiles(
    actor: &ChargePointActor,
    query: &ChargingProfileQuery,
) -> Vec<InstalledChargingProfile> {
    let state = actor.state();
    if !crate::refusal::capability_present(&state.capabilities, "GetChargingProfiles") {
        return Vec::new();
    }
    state
        .charging_profiles
        .selected_by(query)
        .into_iter()
        .cloned()
        .collect()
}

/// The outcome of a CSMS-initiated `UsePriorityCharging` (2.1), matching OCPP's
/// `PriorityChargingStatusEnum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsePriorityChargingOutcome {
    /// Priority charging was granted (or withdrawn) for the named transaction.
    Accepted,
    /// The transaction exists, but no [`ChargingProfilePurpose::PriorityCharging`] profile is
    /// installed for the EVSE running it, so granting priority would change nothing.
    ///
    /// OCPP gives this its own status rather than folding it into `Rejected` because it is the one
    /// refusal the CSMS can fix: install the profile, then ask again.
    NoProfile,
    /// The request could not be honoured - smart charging isn't available, or no transaction with
    /// that id is running.
    Rejected,
}

/// Handles a CSMS-initiated `UsePriorityCharging` against `actor` (2.1 only;
/// `docs/PRODUCTION-ROADMAP.md` B2.6).
///
/// Like every handler here, the outcome is decided against the real state before anything is
/// dispatched, so the status the CSMS receives is what actually happened.
///
/// **Deactivation is never `NoProfile`.** Withdrawing a grant is meaningful whether or not a
/// priority profile is still installed - the profile may have been cleared while the grant stood -
/// and answering `NoProfile` would leave the CSMS believing a transaction still holds a priority
/// it does not.
pub async fn handle_use_priority_charging(
    actor: &ChargePointActor,
    transaction_id: crate::state::TransactionId,
    activate: bool,
) -> UsePriorityChargingOutcome {
    let state = actor.state();
    if !crate::refusal::capability_present(&state.capabilities, "UsePriorityCharging") {
        return UsePriorityChargingOutcome::Rejected;
    }
    let running = state.evses.iter().enumerate().find_map(|(evse_id, evse)| {
        evse.transactions
            .iter()
            .flatten()
            .any(|transaction| transaction.id == transaction_id)
            .then_some(evse_id)
    });
    let Some(evse_id) = running else {
        return UsePriorityChargingOutcome::Rejected;
    };
    if activate
        && !state
            .charging_profiles
            .applying_to(evse_id)
            .iter()
            .any(|installed| {
                installed.profile.purpose == crate::state::ChargingProfilePurpose::PriorityCharging
            })
    {
        return UsePriorityChargingOutcome::NoProfile;
    }

    let _ = actor
        .send(ChargePointEvent::PriorityChargingSet {
            transaction_id,
            activated: activate,
            locally_initiated: false,
        })
        .await;
    UsePriorityChargingOutcome::Accepted
}

/// Reports a priority-charging change the charge point made itself, as OCPP 2.1's
/// `NotifyPriorityCharging`.
///
/// 2.1 only. Neither 1.6J nor 2.0.1 has the message or the profile purpose behind it, so on those
/// versions the change is simply not reportable - a version difference, not a gap here.
#[async_trait::async_trait]
pub trait PriorityChargingNotifier {
    /// What went wrong reporting the change.
    type Error: core::fmt::Display;

    /// Reports that priority charging is now `change.activated` for `change.transaction_id`.
    async fn notify_priority_charging(
        &self,
        change: crate::state::PriorityChargingChange,
    ) -> Result<(), Self::Error>;
}

#[async_trait::async_trait]
impl<T: PriorityChargingNotifier + Send + Sync + ?Sized> PriorityChargingNotifier
    for alloc::sync::Arc<T>
{
    type Error = T::Error;

    async fn notify_priority_charging(
        &self,
        change: crate::state::PriorityChargingChange,
    ) -> Result<(), Self::Error> {
        (**self).notify_priority_charging(change).await
    }
}

/// Forwards every priority-charging change the charge point made on its own to the CSMS, until the
/// actor stops.
///
/// A failed send is logged and dropped rather than queued, for the same reason
/// [`crate::reservation::run_reservation_status_updates`] drops one: delivered after an outage it
/// would announce a priority on a transaction that has very likely ended, and the CSMS has since
/// re-learned the real state from the queued, ordered `TransactionEvent` behind it.
pub async fn run_priority_charging_notifications<N: PriorityChargingNotifier>(
    mut changes: crate::sync::BroadcastReceiver<crate::state::PriorityChargingChange>,
    csms: &N,
) {
    while let Ok(change) = changes.recv().await {
        if let Err(err) = csms.notify_priority_charging(change).await {
            tracing::warn!(
                error = %err,
                transaction_id = change.transaction_id.0,
                "failed to report a priority-charging change"
            );
        }
    }
}

/// The most profiles carried in a single `ReportChargingProfiles` message (see
/// [`chunk_charging_profile_report`]).
///
/// Far smaller than [`crate::reporting::REPORT_CHUNK_SIZE`], and deliberately: one `ReportEntry` is
/// a component, a variable and a value, while one charging profile carries a whole set of
/// schedules, each with as many periods as the CSMS chose to send. Sizing both the same would make
/// this the one report that can overflow a frame. Four profiles' worth of schedules stays
/// comfortably inside an OCPP-J message even when each is at its most verbose, and a charge point
/// bounded to [`max_charging_profiles`](crate::state::StateLimits::max_charging_profiles) never
/// needs many messages to get through them all.
pub const CHARGING_PROFILE_REPORT_CHUNK_SIZE: usize = 4;

/// One `ReportChargingProfiles` message's worth of a chunked charging-profile report.
///
/// Grouped by scope *and* source because the OCPP message carries a single `evseId` and a single
/// `chargingLimitSource` for everything in it - profiles from two different EVSEs cannot share a
/// message however small they are.
#[derive(Debug, Clone, PartialEq)]
pub struct ChargingProfileReportChunk {
    /// The scope every profile in this chunk was installed at.
    pub scope: ChargingProfileScope,
    /// The source that installed every profile in this chunk.
    pub source: ChargingLimitSource,
    /// Whether another message follows this one - across the *whole* report, not just this scope.
    pub tbc: bool,
    /// This message's profiles (at most [`CHARGING_PROFILE_REPORT_CHUNK_SIZE`]).
    pub profiles: Vec<InstalledChargingProfile>,
}

/// Splits `profiles` into the sequence of `ReportChargingProfiles` messages that answers one
/// `GetChargingProfiles`, with `tbc` false on exactly the last one.
///
/// An empty input produces **no** chunks: see [`handle_get_charging_profiles`] for why an empty
/// charging-profile report is silence rather than an empty message.
pub fn chunk_charging_profile_report(
    profiles: &[InstalledChargingProfile],
) -> Vec<ChargingProfileReportChunk> {
    let mut groups: Vec<(ChargingProfileScope, ChargingLimitSource, Vec<_>)> = Vec::new();
    for profile in profiles {
        let key = (profile.scope, profile.source());
        // Linear rather than a map: this runs over at most `max_charging_profiles` items, and
        // preserving first-seen group order keeps the report deterministic (and its tests
        // readable) where a hash map would not.
        match groups
            .iter_mut()
            .find(|(scope, source, _)| (*scope, *source) == key)
        {
            Some((_, _, members)) => members.push(profile.clone()),
            None => groups.push((key.0, key.1, alloc::vec![profile.clone()])),
        }
    }

    let mut chunks: Vec<ChargingProfileReportChunk> = groups
        .into_iter()
        .flat_map(|(scope, source, members)| {
            members
                .chunks(CHARGING_PROFILE_REPORT_CHUNK_SIZE)
                .map(|chunk| ChargingProfileReportChunk {
                    scope,
                    source,
                    // Fixed up below: only the final chunk of the whole report ends it, and a
                    // group cannot know whether another group follows.
                    tbc: true,
                    profiles: chunk.to_vec(),
                })
                .collect::<Vec<_>>()
        })
        .collect();
    if let Some(last) = chunks.last_mut() {
        last.tbc = false;
    }
    chunks
}

/// Registers this charge point's inbound `SetChargingProfile` handling with the CSMS connection.
/// Implemented per protocol version, mirroring [`crate::reservation::ReserveNowHandler`].
#[async_trait::async_trait]
pub trait SetChargingProfileHandler {
    /// Registers a `SetChargingProfile` handler dispatching against `actor`.
    async fn register_set_charging_profile_handler(&self, actor: ChargePointActor);
}

/// Registers this charge point's inbound `UsePriorityCharging` handling with the CSMS connection.
///
/// 2.1 only - see [`PriorityChargingNotifier`] for why the other two versions have nothing to
/// register.
#[async_trait::async_trait]
pub trait UsePriorityChargingHandler {
    /// Registers a `UsePriorityCharging` handler dispatching against `actor`.
    async fn register_use_priority_charging_handler(&self, actor: ChargePointActor);
}

/// Registers this charge point's inbound `ClearChargingProfile` handling with the CSMS connection.
#[async_trait::async_trait]
pub trait ClearChargingProfileHandler {
    /// Registers a `ClearChargingProfile` handler dispatching against `actor`.
    async fn register_clear_charging_profile_handler(&self, actor: ChargePointActor);
}

/// Registers this charge point's inbound `GetChargingProfiles` handling with the CSMS connection.
///
/// 2.x only - 1.6J has no `GetChargingProfiles`, and no way to ask what is installed at all.
#[async_trait::async_trait]
pub trait GetChargingProfilesHandler {
    /// Registers a `GetChargingProfiles` handler dispatching against `actor`.
    async fn register_get_charging_profiles_handler(&self, actor: ChargePointActor);
}

/// Registers this charge point's inbound `GetCompositeSchedule` handling with the CSMS connection.
///
/// Takes the same [`ChargingLimitProjection`] the projection loops use, so the schedule the CSMS
/// is told about is composed from the same context that decides what hardware actually does.
#[async_trait::async_trait]
pub trait GetCompositeScheduleHandler {
    /// Registers a `GetCompositeSchedule` handler dispatching against `actor`.
    async fn register_get_composite_schedule_handler(
        &self,
        actor: ChargePointActor,
        projection: alloc::sync::Arc<ChargingLimitProjection>,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::TokioExecutor;
    use crate::hardware::Capabilities;
    use crate::state::{
        ChargingProfileId, ChargingProfileKind, ChargingProfilePurpose, ChargingSchedule,
        ChargingSchedulePeriod, StateLimits,
    };

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).unwrap()
    }

    fn profile(id: i32) -> ChargingProfile {
        ChargingProfile {
            id: ChargingProfileId(id),
            stack_level: 0,
            purpose: ChargingProfilePurpose::TxDefault,
            kind: ChargingProfileKind::Absolute,
            recurrency: None,
            valid_from: None,
            valid_to: None,
            transaction_id: None,
            schedules: alloc::vec![ChargingSchedule {
                id: 1,
                start_schedule: None,
                duration_secs: None,
                rate_unit: ChargingRateUnit::Amps,
                min_charging_rate: None,
                periods: alloc::vec![ChargingSchedulePeriod {
                    start_period_secs: 0,
                    limit: 16.0,
                    number_phases: None,
                }],
            }],
        }
    }

    async fn actor_with_smart_charging() -> ChargePointActor {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let _ = actor
            .send(ChargePointEvent::CapabilitiesDeclared(Capabilities {
                smart_charging: true,
                ..Capabilities::default()
            }))
            .await;
        actor
    }

    #[tokio::test]
    async fn a_profile_the_charge_point_can_hold_is_accepted_and_installed() {
        let actor = actor_with_smart_charging().await;

        let outcome =
            handle_set_charging_profile(&actor, ChargingProfileScope::Evse(0), profile(1), now())
                .await;

        assert_eq!(outcome, SetChargingProfileOutcome::Accepted);
        assert_eq!(actor.state().charging_profiles.len(), 1);
    }

    #[tokio::test]
    async fn a_missing_schedule_start_is_stamped_with_the_moment_the_profile_arrived() {
        let actor = actor_with_smart_charging().await;

        handle_set_charging_profile(&actor, ChargingProfileScope::Evse(0), profile(1), now()).await;

        let installed = actor.state().charging_profiles.installed()[0].clone();
        assert_eq!(installed.profile.schedules[0].start_schedule, Some(now()));
    }

    #[tokio::test]
    async fn a_profile_for_an_evse_this_charge_point_does_not_have_is_rejected() {
        let actor = actor_with_smart_charging().await;

        let outcome =
            handle_set_charging_profile(&actor, ChargingProfileScope::Evse(7), profile(1), now())
                .await;

        assert!(matches!(outcome, SetChargingProfileOutcome::Rejected(_)));
        assert!(actor.state().charging_profiles.is_empty());
    }

    #[tokio::test]
    async fn a_profile_that_would_exceed_the_bound_is_rejected_rather_than_optimistically_accepted()
    {
        let actor = ChargePointActor::spawn_with_limits(
            [1],
            &TokioExecutor,
            StateLimits::default().with_max_charging_profiles(1),
        );
        let _ = actor
            .send(ChargePointEvent::CapabilitiesDeclared(Capabilities {
                smart_charging: true,
                ..Capabilities::default()
            }))
            .await;

        handle_set_charging_profile(&actor, ChargingProfileScope::Evse(0), profile(1), now()).await;
        let mut second = profile(2);
        second.stack_level = 5;
        let outcome =
            handle_set_charging_profile(&actor, ChargingProfileScope::Evse(0), second, now()).await;

        assert!(matches!(outcome, SetChargingProfileOutcome::Rejected(_)));
        assert_eq!(actor.state().charging_profiles.len(), 1);
    }

    #[tokio::test]
    async fn smart_charging_declared_absent_refuses_every_request() {
        // The capability is compiled in but the hardware says it has no way to limit current -
        // C5's runtime-absent case.
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let outcome =
            handle_set_charging_profile(&actor, ChargingProfileScope::Evse(0), profile(1), now())
                .await;
        assert!(matches!(outcome, SetChargingProfileOutcome::Rejected(_)));

        assert_eq!(
            handle_clear_charging_profile(&actor, ChargingProfileCriteria::default()).await,
            ClearChargingProfileOutcome::Unknown
        );
    }

    #[tokio::test]
    async fn clearing_reports_unknown_when_nothing_matched_and_accepted_when_something_did() {
        let actor = actor_with_smart_charging().await;
        handle_set_charging_profile(&actor, ChargingProfileScope::Evse(0), profile(1), now()).await;

        assert_eq!(
            handle_clear_charging_profile(
                &actor,
                ChargingProfileCriteria {
                    id: Some(ChargingProfileId(99)),
                    ..Default::default()
                }
            )
            .await,
            ClearChargingProfileOutcome::Unknown
        );
        assert_eq!(actor.state().charging_profiles.len(), 1);

        assert_eq!(
            handle_clear_charging_profile(
                &actor,
                ChargingProfileCriteria {
                    id: Some(ChargingProfileId(1)),
                    ..Default::default()
                }
            )
            .await,
            ClearChargingProfileOutcome::Accepted
        );
        assert!(actor.state().charging_profiles.is_empty());
    }

    #[tokio::test]
    async fn a_composite_schedule_is_composed_for_an_addressable_evse_and_refused_otherwise() {
        struct FixedClock;
        impl Clock for FixedClock {
            fn now(&self) -> DateTime<Utc> {
                DateTime::from_timestamp(1_800_000_000, 0).unwrap()
            }
        }

        let actor = actor_with_smart_charging().await;
        let projection = ChargingLimitProjection::new();
        let mut installation_limit = profile(1);
        installation_limit.purpose = ChargingProfilePurpose::ChargePointMax;
        handle_set_charging_profile(
            &actor,
            ChargingProfileScope::ChargePoint,
            installation_limit,
            now(),
        )
        .await;

        let outcome = handle_get_composite_schedule(
            &actor,
            &projection,
            &FixedClock,
            0,
            3_600,
            ChargingRateUnit::Amps,
        )
        .await;
        let GetCompositeScheduleOutcome::Accepted(Some(composed)) = outcome else {
            panic!("expected a composed schedule, got {outcome:?}");
        };
        assert_eq!(composed.periods[0].limit, 16.0);

        assert_eq!(
            handle_get_composite_schedule(
                &actor,
                &projection,
                &FixedClock,
                9,
                3_600,
                ChargingRateUnit::Amps
            )
            .await,
            GetCompositeScheduleOutcome::Rejected
        );
    }

    #[tokio::test]
    async fn getting_charging_profiles_returns_what_matches_the_query() {
        let actor = actor_with_smart_charging().await;
        handle_set_charging_profile(&actor, ChargingProfileScope::Evse(0), profile(1), now()).await;
        let mut second = profile(2);
        second.stack_level = 3;
        handle_set_charging_profile(&actor, ChargingProfileScope::Evse(0), second, now()).await;

        assert_eq!(
            handle_get_charging_profiles(&actor, &ChargingProfileQuery::default()).len(),
            2
        );
        assert_eq!(
            handle_get_charging_profiles(
                &actor,
                &ChargingProfileQuery {
                    stack_level: Some(3),
                    ..Default::default()
                }
            )
            .len(),
            1
        );
    }

    #[tokio::test]
    async fn a_query_naming_several_ids_matches_any_of_them() {
        let actor = actor_with_smart_charging().await;
        for id in 1..=3 {
            // Distinct stack levels, or `install`'s replacement rule (same purpose and level at
            // the same scope supersedes) would leave only the last one.
            let mut installed = profile(id);
            installed.stack_level = id as u32;
            handle_set_charging_profile(&actor, ChargingProfileScope::Evse(0), installed, now())
                .await;
        }

        let matched = handle_get_charging_profiles(
            &actor,
            &ChargingProfileQuery {
                ids: alloc::vec![
                    crate::state::ChargingProfileId(1),
                    crate::state::ChargingProfileId(3),
                ],
                ..Default::default()
            },
        );

        assert_eq!(matched.len(), 2);
        assert!(matched.iter().all(|installed| installed.profile.id.0 != 2));
    }

    #[tokio::test]
    async fn asking_for_profiles_from_a_source_that_installs_none_here_matches_nothing() {
        let actor = actor_with_smart_charging().await;
        handle_set_charging_profile(&actor, ChargingProfileScope::Evse(0), profile(1), now()).await;

        // Everything here arrived by SetChargingProfile, so it is CSO-installed. A CSMS asking
        // for EMS-installed profiles must be told there are none, not handed these.
        assert!(
            handle_get_charging_profiles(
                &actor,
                &ChargingProfileQuery {
                    sources: alloc::vec![ChargingLimitSource::Ems],
                    ..Default::default()
                }
            )
            .is_empty()
        );
        assert_eq!(
            handle_get_charging_profiles(
                &actor,
                &ChargingProfileQuery {
                    sources: alloc::vec![ChargingLimitSource::Cso],
                    ..Default::default()
                }
            )
            .len(),
            1
        );
    }

    fn installed(scope: ChargingProfileScope, id: i32) -> InstalledChargingProfile {
        InstalledChargingProfile {
            scope,
            profile: profile(id),
        }
    }

    #[test]
    fn an_empty_charging_profile_report_is_no_messages_at_all() {
        assert!(chunk_charging_profile_report(&[]).is_empty());
    }

    #[test]
    fn profiles_are_split_by_scope_because_one_message_carries_one_evse() {
        let chunks = chunk_charging_profile_report(&[
            installed(ChargingProfileScope::Evse(0), 1),
            installed(ChargingProfileScope::ChargePoint, 2),
            installed(ChargingProfileScope::Evse(0), 3),
        ]);

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].scope, ChargingProfileScope::Evse(0));
        assert_eq!(chunks[0].profiles.len(), 2);
        assert_eq!(chunks[1].scope, ChargingProfileScope::ChargePoint);
        assert_eq!(chunks[1].profiles.len(), 1);
    }

    #[test]
    fn only_the_final_message_of_the_whole_report_clears_tbc() {
        let profiles: Vec<_> = (0..CHARGING_PROFILE_REPORT_CHUNK_SIZE + 1)
            .map(|index| installed(ChargingProfileScope::Evse(0), index as i32))
            .chain(core::iter::once(installed(
                ChargingProfileScope::ChargePoint,
                99,
            )))
            .collect();

        let chunks = chunk_charging_profile_report(&profiles);

        // Two messages for the EVSE (size + 1 profiles), one for the charge-point scope.
        assert_eq!(chunks.len(), 3);
        assert_eq!(
            chunks.iter().map(|chunk| chunk.tbc).collect::<Vec<_>>(),
            alloc::vec![true, true, false],
            "a `tbc` that clears at the end of a scope would tell the CSMS the report finished \
             while another scope was still coming"
        );
    }

    #[test]
    fn every_reported_profile_is_cso_installed() {
        let chunks =
            chunk_charging_profile_report(&[installed(ChargingProfileScope::ChargePoint, 1)]);

        assert_eq!(chunks[0].source, ChargingLimitSource::Cso);
    }

    async fn start_charging(actor: &ChargePointActor) -> crate::state::TransactionId {
        use crate::state::{ConnectorEvent, EvseEvent, IdToken, IdTokenKind};
        let id_token = IdToken {
            value: "04A224B2".into(),
            kind: IdTokenKind::ISO14443,
        };
        for event in [
            ConnectorEvent::CableConnected,
            ConnectorEvent::LockConfirmed,
            ConnectorEvent::IdTokenPresented(id_token.clone()),
            ConnectorEvent::ChargingAuthorized(id_token),
            ConnectorEvent::ContactorClosed,
        ] {
            let _ = actor
                .send(ChargePointEvent::Evse {
                    evse_id: 0,
                    event: EvseEvent::Connector {
                        connector_id: 0,
                        event,
                    },
                })
                .await;
        }
        actor.state().evses[0].transactions[0]
            .as_ref()
            .expect("a transaction should be running")
            .id
    }

    fn priority_profile(id: i32) -> ChargingProfile {
        ChargingProfile {
            purpose: ChargingProfilePurpose::PriorityCharging,
            ..profile(id)
        }
    }

    #[tokio::test]
    async fn priority_charging_is_granted_when_a_profile_is_installed_to_grant() {
        let actor = actor_with_smart_charging().await;
        let transaction = start_charging(&actor).await;
        handle_set_charging_profile(
            &actor,
            ChargingProfileScope::Evse(0),
            priority_profile(1),
            now(),
        )
        .await;

        let outcome = handle_use_priority_charging(&actor, transaction, true).await;

        assert_eq!(outcome, UsePriorityChargingOutcome::Accepted);
        assert!(
            actor.state().evses[0].transactions[0]
                .as_ref()
                .unwrap()
                .priority_charging
        );
    }

    #[tokio::test]
    async fn granting_priority_with_no_priority_profile_installed_says_so_rather_than_rejecting() {
        let actor = actor_with_smart_charging().await;
        let transaction = start_charging(&actor).await;
        // A transaction-default profile is installed, but that is not a priority-charging one.
        handle_set_charging_profile(&actor, ChargingProfileScope::Evse(0), profile(1), now()).await;

        let outcome = handle_use_priority_charging(&actor, transaction, true).await;

        assert_eq!(outcome, UsePriorityChargingOutcome::NoProfile);
        assert!(
            !actor.state().evses[0].transactions[0]
                .as_ref()
                .unwrap()
                .priority_charging
        );
    }

    #[tokio::test]
    async fn withdrawing_priority_is_accepted_even_once_the_profile_is_gone() {
        let actor = actor_with_smart_charging().await;
        let transaction = start_charging(&actor).await;
        handle_set_charging_profile(
            &actor,
            ChargingProfileScope::Evse(0),
            priority_profile(1),
            now(),
        )
        .await;
        handle_use_priority_charging(&actor, transaction, true).await;
        handle_clear_charging_profile(&actor, ChargingProfileCriteria::default()).await;

        let outcome = handle_use_priority_charging(&actor, transaction, false).await;

        assert_eq!(outcome, UsePriorityChargingOutcome::Accepted);
        assert!(
            !actor.state().evses[0].transactions[0]
                .as_ref()
                .unwrap()
                .priority_charging
        );
    }

    #[tokio::test]
    async fn priority_charging_for_a_transaction_that_is_not_running_is_rejected() {
        let actor = actor_with_smart_charging().await;
        start_charging(&actor).await;

        let outcome =
            handle_use_priority_charging(&actor, crate::state::TransactionId(999), true).await;

        assert_eq!(outcome, UsePriorityChargingOutcome::Rejected);
    }

    #[tokio::test]
    async fn priority_charging_is_rejected_when_smart_charging_is_not_available() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let _ = actor
            .send(ChargePointEvent::CapabilitiesDeclared(Capabilities {
                smart_charging: false,
                ..Capabilities::default()
            }))
            .await;
        let transaction = start_charging(&actor).await;

        let outcome = handle_use_priority_charging(&actor, transaction, true).await;

        assert_eq!(outcome, UsePriorityChargingOutcome::Rejected);
    }
}
