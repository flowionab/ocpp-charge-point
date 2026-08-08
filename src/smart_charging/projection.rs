//! Projecting a composite schedule onto hardware - B2.4 (`docs/PRODUCTION-ROADMAP.md`).
//!
//! Two loops rather than one, deliberately. The limit has to be re-evaluated on two unrelated
//! triggers - *something changed* (a profile installed or cleared, a transaction starting or
//! ending) and *time passed* (a schedule period boundary) - and this crate has no `select!` that
//! works on both `no_std` and std without pulling in a new dependency. Two small loops, each
//! waiting on one thing, need no select at all, and the state machine's own "only dispatch a limit
//! that actually changed" rule (see
//! [`ConnectorEvent::CurrentLimitComputed`](crate::state::ConnectorEvent::CurrentLimitComputed))
//! makes the overlap between them free: whichever loop gets there first wins, and the other's
//! duplicate is dropped without reaching hardware.

use alloc::vec::Vec;
use chrono::{DateTime, Utc};
use core::cell::RefCell;
use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;

use crate::actor::ChargePointActor;
use crate::clock::Clock;
use crate::provisioning::Backoff;
use crate::smart_charging::{CompositionContext, SupplyCharacteristics, compose, current_limit_ma};
use crate::state::{
    ChargePointEvent, ChargePointState, ChargingRateUnit, ConnectorEvent, EvseEvent, TransactionId,
};

/// How far ahead the projection composes when it is looking for the next period boundary.
///
/// 24 hours covers any schedule granularity a charge point actually runs on while bounding the
/// work each evaluation does. A boundary further out than this is simply found by a later
/// evaluation - the projection re-evaluates at least this often anyway (see
/// [`MAX_SLEEP_SECS`]), so nothing is missed, only deferred.
const LOOKAHEAD_SECS: u32 = 24 * 60 * 60;

/// The longest the timer loop sleeps when no period boundary is coming.
///
/// A charge point with no profiles installed still wakes this often, which costs almost nothing
/// and means a clock that jumps (an RTC finally syncing - see `crate::clock`) can never strand the
/// projection waiting on a boundary that is already in the past.
const MAX_SLEEP_SECS: u32 = 15 * 60;

/// Shared state for the two projection loops: the supply characteristics they compose with, and
/// when each connector's current transaction started.
///
/// The start times live here rather than on [`crate::state::Transaction`] because stamping one
/// needs a [`Clock`], and the state machine is deliberately clock-free - the same split
/// [`crate::persistence::PersistedTransaction::started_at`] makes. They are what a
/// [`ChargingProfileKind::Relative`](crate::state::ChargingProfileKind::Relative) profile is
/// anchored to.
///
/// Bounded by construction: at most one entry per connector, recorded when a transaction appears
/// on it and dropped when that transaction ends.
pub struct ChargingLimitProjection {
    supply: Option<SupplyCharacteristics>,
    #[allow(clippy::type_complexity)]
    transaction_starts: BlockingMutex<
        CriticalSectionRawMutex,
        RefCell<Vec<(usize, usize, TransactionId, DateTime<Utc>)>>,
    >,
}

impl ChargingLimitProjection {
    /// A projection that skips any profile whose schedule is not already in amps, since without
    /// [`SupplyCharacteristics`] there is no honest way to convert one - see that type's docs.
    pub fn new() -> Self {
        Self {
            supply: None,
            transaction_starts: BlockingMutex::new(RefCell::new(Vec::new())),
        }
    }

    /// A projection that converts watt-denominated schedules using `supply`. Integrators know
    /// their installation's voltage and phase count; this crate does not, and will not guess.
    pub fn with_supply(supply: SupplyCharacteristics) -> Self {
        Self {
            supply: Some(supply),
            ..Self::new()
        }
    }

    /// Records that `transaction` is running on this connector as of `now`, if this is the first
    /// time it has been seen, and returns the start time to anchor `Relative` profiles to.
    ///
    /// A transaction already running when the projection starts (a charge point restarted
    /// mid-session) is anchored to the moment the projection first *saw* it, not to when it
    /// really started - which is the honest best available, since the real start time was in the
    /// RAM that the restart lost. It is logged, so a `Relative` profile behaving oddly after a
    /// restart is explicable rather than mysterious.
    fn note_transaction(
        &self,
        evse_id: usize,
        connector_id: usize,
        transaction: TransactionId,
        now: DateTime<Utc>,
    ) -> DateTime<Utc> {
        self.transaction_starts.lock(|starts| {
            let mut starts = starts.borrow_mut();
            if let Some(entry) = starts.iter().find(|(evse, connector, id, _)| {
                (*evse, *connector, *id) == (evse_id, connector_id, transaction)
            }) {
                return entry.3;
            }
            starts.retain(|(evse, connector, _, _)| (*evse, *connector) != (evse_id, connector_id));
            starts.push((evse_id, connector_id, transaction, now));
            now
        })
    }

    /// Drops the recorded start time for a connector whose transaction has ended.
    fn forget_transaction(&self, evse_id: usize, connector_id: usize) {
        self.transaction_starts.lock(|starts| {
            starts
                .borrow_mut()
                .retain(|(evse, connector, _, _)| (*evse, *connector) != (evse_id, connector_id));
        });
    }
}

impl Default for ChargingLimitProjection {
    fn default() -> Self {
        Self::new()
    }
}

/// Builds the [`CompositionContext`] for one connector: what is charging on it, when that started,
/// and how far ahead to look.
///
/// Public because `GetCompositeSchedule` (B2.5) needs exactly the same context the projection
/// uses: a CSMS asking "what will you do" must be answered from the same rules that decide what
/// the charge point actually does, not from a second, subtly different derivation.
pub fn connector_composition_context(
    projection: &ChargingLimitProjection,
    state: &ChargePointState,
    evse_id: usize,
    connector_id: usize,
    now: DateTime<Utc>,
    duration_secs: u32,
) -> CompositionContext {
    let transaction = state
        .evses
        .get(evse_id)
        .and_then(|evse| evse.transactions.get(connector_id))
        .and_then(|transaction| transaction.as_ref());
    let transaction_started_at = transaction
        .map(|transaction| projection.note_transaction(evse_id, connector_id, transaction.id, now));
    if transaction.is_none() {
        projection.forget_transaction(evse_id, connector_id);
    }
    CompositionContext {
        now,
        transaction_id: transaction.map(|transaction| transaction.id),
        transaction_started_at,
        rate_unit: ChargingRateUnit::Amps,
        duration_secs,
        supply: projection.supply,
        priority_charging: transaction.is_some_and(|transaction| transaction.priority_charging),
    }
}

/// Computes every connector's limit from the profiles currently installed and dispatches whatever
/// changed into the actor. Returns the earliest instant at which any connector's limit next
/// changes, so the timer loop knows when to wake.
async fn evaluate_all<C: Clock>(
    actor: &ChargePointActor,
    projection: &ChargingLimitProjection,
    clock: &C,
) -> Option<DateTime<Utc>> {
    let state = actor.state();
    let now = clock.now();
    let mut next_change: Option<DateTime<Utc>> = None;

    for evse_id in 0..state.evses.len() {
        for connector_id in 0..state.evses[evse_id].connectors.len() {
            let context = connector_composition_context(
                projection,
                &state,
                evse_id,
                connector_id,
                now,
                LOOKAHEAD_SECS,
            );
            let profiles = state.charging_profiles.applying_to(evse_id);
            let limit_ma = current_limit_ma(&profiles, &context);
            if let Some(composed) = compose(&profiles, &context)
                && let Some(change) = composed.next_change_after(now)
            {
                next_change = Some(next_change.map_or(change, |current| current.min(change)));
            }
            // Unconditional: the state machine drops a limit that matches what this connector was
            // already asked for, so this only reaches hardware when it genuinely changed.
            let _ = actor
                .send(ChargePointEvent::Evse {
                    evse_id,
                    event: EvseEvent::Connector {
                        connector_id,
                        event: ConnectorEvent::CurrentLimitComputed(limit_ma),
                    },
                })
                .await;
        }
    }
    next_change
}

/// Re-evaluates every connector's charging limit whenever the charge point's state changes - a
/// profile installed or cleared, a transaction starting or ending, a connector faulting.
///
/// Runs forever. Pair it with [`run_charging_limit_schedule`], which covers the other trigger
/// (time passing); neither alone satisfies B2.4.
pub async fn run_charging_limit_projection<C: Clock>(
    actor: &ChargePointActor,
    projection: &ChargingLimitProjection,
    clock: &C,
) {
    let mut states = actor.subscribe();
    // Once up front, so a charge point that boots with profiles already restored applies them
    // without waiting for something else to happen.
    evaluate_all(actor, projection, clock).await;
    loop {
        states.changed().await;
        evaluate_all(actor, projection, clock).await;
    }
}

/// Re-evaluates every connector's charging limit when a schedule period boundary arrives, sleeping
/// until exactly that instant (or 15 minutes, whichever is sooner) rather than polling.
///
/// Runs forever. Pair it with [`run_charging_limit_projection`] - see this module's docs for why
/// the two triggers are two loops.
pub async fn run_charging_limit_schedule<C: Clock, B: Backoff>(
    actor: &ChargePointActor,
    projection: &ChargingLimitProjection,
    clock: &C,
    backoff: &B,
) {
    loop {
        let next_change = evaluate_all(actor, projection, clock).await;
        let seconds = next_change
            .map(|change| (change - clock.now()).num_seconds())
            // A boundary in the past (a clock that jumped, or an evaluation that took longer than
            // the gap) is not a reason to spin: the next evaluation happens on the next tick.
            .map(|seconds| seconds.clamp(1, i64::from(MAX_SLEEP_SECS)) as u32)
            .unwrap_or(MAX_SLEEP_SECS);
        backoff.wait(seconds).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::TokioExecutor;
    use crate::state::{
        ChargingProfile, ChargingProfileId, ChargingProfileKind, ChargingProfilePurpose,
        ChargingProfileScope, ChargingSchedule, ChargingSchedulePeriod, EvseEvent, IdToken,
        IdTokenKind,
    };
    use alloc::sync::Arc;

    /// A [`Clock`] reading a fixed instant - composition is a pure function of time, so a fixed
    /// one makes every assertion here exact.
    struct FixedClock(DateTime<Utc>);

    impl Clock for FixedClock {
        fn now(&self) -> DateTime<Utc> {
            self.0
        }
    }

    fn fixed_clock() -> FixedClock {
        FixedClock(DateTime::from_timestamp(1_800_000_000, 0).unwrap())
    }

    fn amp_profile(id: i32, limit: f64) -> ChargingProfile {
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
                    limit,
                    number_phases: None,
                }],
            }],
            dyn_update_interval_secs: None,
            dyn_update_time: None,
        }
    }

    async fn start_charging(actor: &ChargePointActor) {
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
    }

    async fn settle() {
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn an_installed_profile_reaches_the_connectors_limit_while_a_transaction_runs() {
        let actor = Arc::new(ChargePointActor::spawn([1], &TokioExecutor));
        let projection = Arc::new(ChargingLimitProjection::new());

        let task_actor = actor.clone();
        let task_projection = projection.clone();
        tokio::spawn(async move {
            run_charging_limit_projection(&task_actor, &task_projection, &fixed_clock()).await;
        });

        start_charging(&actor).await;
        let _ = actor
            .send(ChargePointEvent::ChargingProfileSet {
                scope: ChargingProfileScope::Evse(0),
                profile: alloc::boxed::Box::new(amp_profile(1, 16.0)),
            })
            .await;
        settle().await;

        assert_eq!(actor.state().evses[0].charging_limits[0], Some(16_000));
    }

    #[tokio::test]
    async fn a_dynamic_update_moves_the_connectors_limit_without_reinstalling_the_profile() {
        let actor = Arc::new(ChargePointActor::spawn([1], &TokioExecutor));
        let projection = Arc::new(ChargingLimitProjection::new());

        let task_actor = actor.clone();
        let task_projection = projection.clone();
        tokio::spawn(async move {
            run_charging_limit_projection(&task_actor, &task_projection, &fixed_clock()).await;
        });

        start_charging(&actor).await;
        let mut dynamic = amp_profile(1, 16.0);
        dynamic.kind = crate::state::ChargingProfileKind::Dynamic;
        dynamic.dyn_update_time = Some(fixed_clock().now());
        let _ = actor
            .send(ChargePointEvent::ChargingProfileSet {
                scope: ChargingProfileScope::Evse(0),
                profile: alloc::boxed::Box::new(dynamic),
            })
            .await;
        settle().await;
        assert_eq!(actor.state().evses[0].charging_limits[0], Some(16_000));

        // The whole point of a dynamic profile: a new limit without a new profile.
        let _ = actor
            .send(ChargePointEvent::DynamicScheduleUpdated {
                profile_id: crate::state::ChargingProfileId(1),
                limit: Some(10.0),
                updated_at: fixed_clock().now(),
            })
            .await;
        settle().await;
        assert_eq!(actor.state().evses[0].charging_limits[0], Some(10_000));
    }

    #[tokio::test]
    async fn a_dynamic_profile_whose_updates_stopped_releases_the_connector() {
        let actor = Arc::new(ChargePointActor::spawn([1], &TokioExecutor));
        let projection = Arc::new(ChargingLimitProjection::new());

        let task_actor = actor.clone();
        let task_projection = projection.clone();
        tokio::spawn(async move {
            run_charging_limit_projection(&task_actor, &task_projection, &fixed_clock()).await;
        });

        start_charging(&actor).await;
        let mut dynamic = amp_profile(1, 16.0);
        dynamic.kind = crate::state::ChargingProfileKind::Dynamic;
        dynamic.schedules[0].duration_secs = Some(300);
        // Last updated an hour before the projection's clock: the CSMS has gone quiet well past
        // the five minutes it said its own limit was good for (K28.FR.13).
        dynamic.dyn_update_time = Some(fixed_clock().now() - chrono::Duration::seconds(3_600));
        let _ = actor
            .send(ChargePointEvent::ChargingProfileSet {
                scope: ChargingProfileScope::Evse(0),
                profile: alloc::boxed::Box::new(dynamic),
            })
            .await;
        settle().await;
        assert_eq!(actor.state().evses[0].charging_limits[0], None);

        // K28.FR.14: an update revives it, with no reinstall from the CSMS.
        let _ = actor
            .send(ChargePointEvent::DynamicScheduleUpdated {
                profile_id: crate::state::ChargingProfileId(1),
                limit: Some(12.0),
                updated_at: fixed_clock().now(),
            })
            .await;
        settle().await;
        assert_eq!(actor.state().evses[0].charging_limits[0], Some(12_000));
    }

    #[tokio::test]
    async fn granting_priority_charging_moves_the_connectors_limit_onto_the_priority_profile() {
        let actor = Arc::new(ChargePointActor::spawn([1], &TokioExecutor));
        let projection = Arc::new(ChargingLimitProjection::new());

        let task_actor = actor.clone();
        let task_projection = projection.clone();
        tokio::spawn(async move {
            run_charging_limit_projection(&task_actor, &task_projection, &fixed_clock()).await;
        });

        start_charging(&actor).await;
        let transaction = actor.state().evses[0].transactions[0].as_ref().unwrap().id;
        let _ = actor
            .send(ChargePointEvent::ChargingProfileSet {
                scope: ChargingProfileScope::Evse(0),
                profile: alloc::boxed::Box::new(amp_profile(1, 16.0)),
            })
            .await;
        let mut priority = amp_profile(2, 32.0);
        priority.purpose = crate::state::ChargingProfilePurpose::PriorityCharging;
        let _ = actor
            .send(ChargePointEvent::ChargingProfileSet {
                scope: ChargingProfileScope::Evse(0),
                profile: alloc::boxed::Box::new(priority),
            })
            .await;
        settle().await;
        // Installed, not granted: hardware is still on the transaction default.
        assert_eq!(actor.state().evses[0].charging_limits[0], Some(16_000));

        let _ = actor
            .send(ChargePointEvent::PriorityChargingSet {
                transaction_id: transaction,
                activated: true,
                locally_initiated: false,
            })
            .await;
        settle().await;
        assert_eq!(actor.state().evses[0].charging_limits[0], Some(32_000));

        // Withdrawing it puts hardware back, without the CSMS having to reinstall anything.
        let _ = actor
            .send(ChargePointEvent::PriorityChargingSet {
                transaction_id: transaction,
                activated: false,
                locally_initiated: false,
            })
            .await;
        settle().await;
        assert_eq!(actor.state().evses[0].charging_limits[0], Some(16_000));
    }

    #[tokio::test]
    async fn a_transaction_default_profile_imposes_nothing_until_a_transaction_starts() {
        let actor = Arc::new(ChargePointActor::spawn([1], &TokioExecutor));
        let projection = Arc::new(ChargingLimitProjection::new());

        let task_actor = actor.clone();
        let task_projection = projection.clone();
        tokio::spawn(async move {
            run_charging_limit_projection(&task_actor, &task_projection, &fixed_clock()).await;
        });

        let _ = actor
            .send(ChargePointEvent::ChargingProfileSet {
                scope: ChargingProfileScope::Evse(0),
                profile: alloc::boxed::Box::new(amp_profile(1, 16.0)),
            })
            .await;
        settle().await;
        assert_eq!(actor.state().evses[0].charging_limits[0], None);

        // ...and takes effect the moment one does, without any further CSMS interaction.
        start_charging(&actor).await;
        settle().await;
        assert_eq!(actor.state().evses[0].charging_limits[0], Some(16_000));
    }

    #[tokio::test]
    async fn clearing_the_profiles_removes_the_connectors_limit_rather_than_leaving_it_applied() {
        let actor = Arc::new(ChargePointActor::spawn([1], &TokioExecutor));
        let projection = Arc::new(ChargingLimitProjection::new());

        let task_actor = actor.clone();
        let task_projection = projection.clone();
        tokio::spawn(async move {
            run_charging_limit_projection(&task_actor, &task_projection, &fixed_clock()).await;
        });

        start_charging(&actor).await;
        let _ = actor
            .send(ChargePointEvent::ChargingProfileSet {
                scope: ChargingProfileScope::Evse(0),
                profile: alloc::boxed::Box::new(amp_profile(1, 16.0)),
            })
            .await;
        settle().await;
        assert_eq!(actor.state().evses[0].charging_limits[0], Some(16_000));

        let _ = actor
            .send(ChargePointEvent::ChargingProfilesCleared {
                criteria: crate::state::ChargingProfileCriteria::default(),
            })
            .await;
        settle().await;

        // Not `Some(anything)`: with no profile installed the connector must go back to its own
        // maximum, which only the hardware knows - see `Connector::set_current_limit`.
        assert_eq!(actor.state().evses[0].charging_limits[0], None);
    }

    #[tokio::test]
    async fn a_charge_point_wide_profile_limits_a_connector_that_has_none_of_its_own() {
        let actor = Arc::new(ChargePointActor::spawn([1], &TokioExecutor));
        let projection = Arc::new(ChargingLimitProjection::new());

        let task_actor = actor.clone();
        let task_projection = projection.clone();
        tokio::spawn(async move {
            run_charging_limit_projection(&task_actor, &task_projection, &fixed_clock()).await;
        });

        let mut installation_limit = amp_profile(1, 20.0);
        installation_limit.purpose = ChargingProfilePurpose::ChargePointMax;
        let _ = actor
            .send(ChargePointEvent::ChargingProfileSet {
                scope: ChargingProfileScope::ChargePoint,
                profile: alloc::boxed::Box::new(installation_limit),
            })
            .await;
        settle().await;

        // An installation limit applies whether or not anything is charging - it is a property of
        // the supply, not of the session.
        assert_eq!(actor.state().evses[0].charging_limits[0], Some(20_000));
    }

    #[tokio::test]
    async fn a_watt_only_profile_is_skipped_without_supply_characteristics_and_applied_with_them() {
        let mut watts = amp_profile(1, 0.0);
        watts.schedules[0].rate_unit = ChargingRateUnit::Watts;
        watts.schedules[0].periods[0].limit = 7_360.0;

        // Without a supply, there is no honest conversion, so nothing is imposed.
        let actor = Arc::new(ChargePointActor::spawn([1], &TokioExecutor));
        let projection = Arc::new(ChargingLimitProjection::new());
        let task_actor = actor.clone();
        let task_projection = projection.clone();
        tokio::spawn(async move {
            run_charging_limit_projection(&task_actor, &task_projection, &fixed_clock()).await;
        });
        start_charging(&actor).await;
        let _ = actor
            .send(ChargePointEvent::ChargingProfileSet {
                scope: ChargingProfileScope::Evse(0),
                profile: alloc::boxed::Box::new(watts.clone()),
            })
            .await;
        settle().await;
        assert_eq!(actor.state().evses[0].charging_limits[0], None);

        // With one, the same profile converts to 32 A.
        let actor = Arc::new(ChargePointActor::spawn([1], &TokioExecutor));
        let projection = Arc::new(ChargingLimitProjection::with_supply(
            SupplyCharacteristics {
                nominal_voltage_v: 230,
                phases: 1,
            },
        ));
        let task_actor = actor.clone();
        let task_projection = projection.clone();
        tokio::spawn(async move {
            run_charging_limit_projection(&task_actor, &task_projection, &fixed_clock()).await;
        });
        start_charging(&actor).await;
        let _ = actor
            .send(ChargePointEvent::ChargingProfileSet {
                scope: ChargingProfileScope::Evse(0),
                profile: alloc::boxed::Box::new(watts),
            })
            .await;
        settle().await;
        assert_eq!(actor.state().evses[0].charging_limits[0], Some(32_000));
    }

    #[tokio::test]
    async fn a_relative_profile_is_anchored_to_when_the_transaction_was_first_seen() {
        let actor = Arc::new(ChargePointActor::spawn([1], &TokioExecutor));
        let projection = ChargingLimitProjection::new();
        let clock = fixed_clock();

        start_charging(&actor).await;
        let mut relative = amp_profile(1, 16.0);
        relative.kind = ChargingProfileKind::Relative;
        relative.schedules[0].periods.push(ChargingSchedulePeriod {
            start_period_secs: 600,
            limit: 32.0,
            number_phases: None,
        });
        let _ = actor
            .send(ChargePointEvent::ChargingProfileSet {
                scope: ChargingProfileScope::Evse(0),
                profile: alloc::boxed::Box::new(relative),
            })
            .await;

        let state = actor.state();
        let context = connector_composition_context(&projection, &state, 0, 0, clock.now(), 3_600);
        assert_eq!(context.transaction_started_at, Some(clock.now()));
        // The schedule is therefore at its first period, and the second one is still 600 s away.
        assert_eq!(
            current_limit_ma(&state.charging_profiles.applying_to(0), &context),
            Some(16_000)
        );
        let composed = compose(&state.charging_profiles.applying_to(0), &context).unwrap();
        assert_eq!(
            composed.next_change_after(clock.now()),
            Some(clock.now() + chrono::Duration::seconds(600))
        );
    }
}
