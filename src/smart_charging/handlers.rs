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
    ChargePointEvent, ChargingProfile, ChargingProfileCriteria, ChargingProfileScope,
    ChargingRateUnit, InstalledChargingProfile,
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
/// Returns an empty vector when nothing matches, which the caller reports as OCPP's
/// `NoProfiles` status rather than sending an empty report.
pub fn handle_get_charging_profiles(
    actor: &ChargePointActor,
    criteria: ChargingProfileCriteria,
) -> Vec<InstalledChargingProfile> {
    let state = actor.state();
    if !crate::refusal::capability_present(&state.capabilities, "GetChargingProfiles") {
        return Vec::new();
    }
    state
        .charging_profiles
        .matching(&criteria)
        .into_iter()
        .cloned()
        .collect()
}

/// Registers this charge point's inbound `SetChargingProfile` handling with the CSMS connection.
/// Implemented per protocol version, mirroring [`crate::reservation::ReserveNowHandler`].
#[async_trait::async_trait]
pub trait SetChargingProfileHandler {
    /// Registers a `SetChargingProfile` handler dispatching against `actor`.
    async fn register_set_charging_profile_handler(&self, actor: ChargePointActor);
}

/// Registers this charge point's inbound `ClearChargingProfile` handling with the CSMS connection.
#[async_trait::async_trait]
pub trait ClearChargingProfileHandler {
    /// Registers a `ClearChargingProfile` handler dispatching against `actor`.
    async fn register_clear_charging_profile_handler(&self, actor: ChargePointActor);
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
    async fn getting_charging_profiles_returns_what_matches_the_criteria() {
        let actor = actor_with_smart_charging().await;
        handle_set_charging_profile(&actor, ChargingProfileScope::Evse(0), profile(1), now()).await;
        let mut second = profile(2);
        second.stack_level = 3;
        handle_set_charging_profile(&actor, ChargingProfileScope::Evse(0), second, now()).await;

        assert_eq!(
            handle_get_charging_profiles(&actor, ChargingProfileCriteria::default()).len(),
            2
        );
        assert_eq!(
            handle_get_charging_profiles(
                &actor,
                ChargingProfileCriteria {
                    stack_level: Some(3),
                    ..Default::default()
                }
            )
            .len(),
            1
        );
    }
}
