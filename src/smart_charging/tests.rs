//! Composition tests (B2.2). Kept in their own file because the composition rules - purpose
//! precedence, stack levels, capping profiles, recurrence, unit conversion - are where the whole
//! block's correctness lives, and one flat list of cases per rule reads better than one giant
//! module beside the implementation.

use super::*;
use crate::state::{
    ChargingProfile, ChargingProfileId, ChargingProfileKind, ChargingProfilePurpose,
    ChargingProfileScope, ChargingRateUnit, ChargingSchedule, ChargingSchedulePeriod,
    InstalledChargingProfile, RecurrencyKind, TransactionId,
};
use alloc::vec;
use alloc::vec::Vec;
use chrono::{DateTime, Duration, Utc};

fn at(secs: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap()
}

fn schedule(periods: &[(u32, f64)]) -> ChargingSchedule {
    ChargingSchedule {
        id: 1,
        start_schedule: None,
        duration_secs: None,
        rate_unit: ChargingRateUnit::Amps,
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

fn profile(
    id: i32,
    purpose: ChargingProfilePurpose,
    stack_level: u32,
    schedule: ChargingSchedule,
) -> InstalledChargingProfile {
    InstalledChargingProfile {
        scope: ChargingProfileScope::Evse(0),
        profile: ChargingProfile {
            id: ChargingProfileId(id),
            stack_level,
            purpose,
            kind: ChargingProfileKind::Absolute,
            recurrency: None,
            valid_from: None,
            valid_to: None,
            transaction_id: None,
            schedules: vec![schedule],
            dyn_update_interval_secs: None,
            dyn_update_time: None,
        },
    }
}

/// A context with a transaction running, since that is what most rules are about.
fn context() -> CompositionContext {
    CompositionContext {
        now: at(0),
        transaction_id: Some(TransactionId(1)),
        transaction_started_at: Some(at(0)),
        rate_unit: ChargingRateUnit::Amps,
        duration_secs: 3_600,
        supply: None,
        priority_charging: false,
    }
}

fn limits(composed: &CompositeSchedule) -> Vec<(u32, f64)> {
    composed
        .periods
        .iter()
        .map(|period| (period.start_period_secs, period.limit))
        .collect()
}

#[test]
fn no_profiles_at_all_compose_to_no_limit() {
    assert_eq!(compose(&[], &context()), None);
}

#[test]
fn a_single_profile_composes_to_its_own_periods() {
    let profiles = [profile(
        1,
        ChargingProfilePurpose::TxDefault,
        0,
        schedule(&[(0, 16.0), (1_800, 32.0)]),
    )];

    let composed = compose(&profiles.iter().collect::<Vec<_>>(), &context()).unwrap();

    assert_eq!(composed.start, at(0));
    assert_eq!(limits(&composed), vec![(0, 16.0), (1_800, 32.0)]);
}

#[test]
fn the_highest_stack_level_wins_within_a_purpose() {
    let profiles = [
        profile(
            1,
            ChargingProfilePurpose::TxDefault,
            0,
            schedule(&[(0, 16.0)]),
        ),
        profile(
            2,
            ChargingProfilePurpose::TxDefault,
            5,
            schedule(&[(0, 10.0)]),
        ),
    ];

    let composed = compose(&profiles.iter().collect::<Vec<_>>(), &context()).unwrap();

    // Not the lower limit - the higher *stack level*. A stack level is precedence, not a
    // safety ranking, and picking the smaller number here would be a different rule that
    // happens to agree on this input.
    assert_eq!(limits(&composed), vec![(0, 10.0)]);
}

#[test]
fn a_transaction_profile_overrides_the_transaction_default_even_at_a_lower_stack_level() {
    let profiles = [
        profile(
            1,
            ChargingProfilePurpose::TxDefault,
            9,
            schedule(&[(0, 32.0)]),
        ),
        profile(2, ChargingProfilePurpose::Tx, 0, schedule(&[(0, 20.0)])),
    ];

    let composed = compose(&profiles.iter().collect::<Vec<_>>(), &context()).unwrap();

    assert_eq!(limits(&composed), vec![(0, 20.0)]);
}

#[test]
fn a_charge_point_max_profile_caps_the_result_rather_than_competing_with_it() {
    let profiles = [
        profile(
            1,
            ChargingProfilePurpose::TxDefault,
            9,
            schedule(&[(0, 32.0)]),
        ),
        profile(
            2,
            ChargingProfilePurpose::ChargePointMax,
            0,
            schedule(&[(0, 20.0)]),
        ),
    ];

    let composed = compose(&profiles.iter().collect::<Vec<_>>(), &context()).unwrap();

    assert_eq!(limits(&composed), vec![(0, 20.0)]);
}

#[test]
fn a_cap_above_the_transaction_limit_does_not_raise_it() {
    let profiles = [
        profile(
            1,
            ChargingProfilePurpose::TxDefault,
            0,
            schedule(&[(0, 16.0)]),
        ),
        profile(
            2,
            ChargingProfilePurpose::ChargePointMax,
            0,
            schedule(&[(0, 32.0)]),
        ),
    ];

    let composed = compose(&profiles.iter().collect::<Vec<_>>(), &context()).unwrap();

    assert_eq!(limits(&composed), vec![(0, 16.0)]);
}

#[test]
fn a_cap_with_nothing_to_cap_is_itself_the_limit() {
    let profiles = [profile(
        1,
        ChargingProfilePurpose::ChargePointMax,
        0,
        schedule(&[(0, 20.0)]),
    )];

    let composed = compose(&profiles.iter().collect::<Vec<_>>(), &context()).unwrap();

    assert_eq!(limits(&composed), vec![(0, 20.0)]);
}

/// K27's own worked example, and the reason CV17 is a behaviour fix rather than a reporting one:
/// 2 kW of local generation on top of a 5 kW `TxDefaultProfile` is 7 kW, not 2 kW. "If a charging
/// profile of chargingProfilePurpose = LocalGeneration is active for the EVSE, then this capacity
/// is added on top of the calculated composite schedule" (2.1 Part 2 §K.3.6).
#[test]
fn local_generation_adds_its_capacity_on_top_rather_than_capping_the_result() {
    let profiles = [
        profile(
            1,
            ChargingProfilePurpose::TxDefault,
            0,
            schedule(&[(0, 5_000.0)]),
        ),
        profile(
            2,
            ChargingProfilePurpose::LocalGeneration,
            0,
            schedule(&[(0, 2_000.0)]),
        ),
    ];

    let composed = compose(&profiles.iter().collect::<Vec<_>>(), &context()).unwrap();

    assert_eq!(limits(&composed), vec![(0, 7_000.0)]);
}

/// The other half of "adds on top": it is added *after* the minimum across purposes, so an
/// installation limit does not clip the locally generated capacity away. K27's grid connection is
/// 7 kW precisely because the 2 kW never crosses it.
#[test]
fn local_generation_is_added_after_the_caps_not_before_them() {
    let profiles = [
        profile(
            1,
            ChargingProfilePurpose::TxDefault,
            0,
            schedule(&[(0, 8_000.0)]),
        ),
        profile(
            2,
            ChargingProfilePurpose::ChargePointMax,
            0,
            schedule(&[(0, 5_000.0)]),
        ),
        profile(
            3,
            ChargingProfilePurpose::LocalGeneration,
            0,
            schedule(&[(0, 2_000.0)]),
        ),
    ];

    let composed = compose(&profiles.iter().collect::<Vec<_>>(), &context()).unwrap();

    assert_eq!(limits(&composed), vec![(0, 7_000.0)]);
}

/// Local generation with nothing to add to leaves the connector unlimited rather than becoming
/// the limit itself - the mirror image of `a_cap_with_nothing_to_cap_is_itself_the_limit`, and the
/// opposite answer, because "2 kW of headroom" says nothing about the ceiling.
#[test]
fn local_generation_with_nothing_to_add_to_does_not_become_the_limit() {
    let profiles = [profile(
        1,
        ChargingProfilePurpose::LocalGeneration,
        0,
        schedule(&[(0, 2_000.0)]),
    )];

    assert!(compose(&profiles.iter().collect::<Vec<_>>(), &context()).is_none());
}

/// Stack level disambiguates local generation exactly as it does every other purpose: §K.3.6's
/// "leading charging schedule for that purpose is the one ... with the highest stack level" is not
/// qualified by purpose, so two local generation profiles are two candidates and the higher stack
/// level leads. Summing them would be a rule invented here - and would double-count an EMS that
/// re-sent a revised figure at a new stack level.
#[test]
fn the_highest_stacked_local_generation_profile_leads_rather_than_summing() {
    let profiles = [
        profile(
            1,
            ChargingProfilePurpose::TxDefault,
            0,
            schedule(&[(0, 5_000.0)]),
        ),
        profile(
            2,
            ChargingProfilePurpose::LocalGeneration,
            0,
            schedule(&[(0, 2_000.0)]),
        ),
        profile(
            3,
            ChargingProfilePurpose::LocalGeneration,
            1,
            schedule(&[(0, 1_000.0)]),
        ),
    ];

    let composed = compose(&profiles.iter().collect::<Vec<_>>(), &context()).unwrap();

    assert_eq!(limits(&composed), vec![(0, 6_000.0)]);
}

#[test]
fn external_constraints_cap_the_result_the_same_way_an_installation_limit_does() {
    let profiles = [
        profile(
            1,
            ChargingProfilePurpose::TxDefault,
            0,
            schedule(&[(0, 32.0)]),
        ),
        profile(
            2,
            ChargingProfilePurpose::ExternalConstraints,
            0,
            schedule(&[(0, 12.0)]),
        ),
        profile(
            3,
            ChargingProfilePurpose::ChargePointMax,
            0,
            schedule(&[(0, 20.0)]),
        ),
    ];

    let composed = compose(&profiles.iter().collect::<Vec<_>>(), &context()).unwrap();

    // Both caps apply; the lower one binds.
    assert_eq!(limits(&composed), vec![(0, 12.0)]);
}

#[test]
fn transaction_scoped_profiles_do_not_apply_without_a_transaction() {
    let profiles = [
        profile(
            1,
            ChargingProfilePurpose::TxDefault,
            0,
            schedule(&[(0, 16.0)]),
        ),
        profile(2, ChargingProfilePurpose::Tx, 0, schedule(&[(0, 20.0)])),
    ];
    let context = CompositionContext {
        transaction_id: None,
        transaction_started_at: None,
        ..context()
    };

    assert_eq!(
        compose(&profiles.iter().collect::<Vec<_>>(), &context),
        None
    );
}

#[test]
fn a_priority_charging_profile_does_nothing_until_priority_charging_is_activated() {
    let profiles = [
        profile(
            1,
            ChargingProfilePurpose::TxDefault,
            0,
            schedule(&[(0, 16.0)]),
        ),
        profile(
            2,
            ChargingProfilePurpose::PriorityCharging,
            0,
            schedule(&[(0, 32.0)]),
        ),
    ];

    // Installed but not activated: the transaction default still decides. A priority-charging
    // profile that applied on installation would silently raise every session's limit, which is
    // the opposite of `UsePriorityCharging` being a decision the CSMS makes per transaction.
    let composed = compose(&profiles.iter().collect::<Vec<_>>(), &context()).unwrap();
    assert_eq!(limits(&composed), vec![(0, 16.0)]);

    let activated = CompositionContext {
        priority_charging: true,
        ..context()
    };
    let composed = compose(&profiles.iter().collect::<Vec<_>>(), &activated).unwrap();
    assert_eq!(limits(&composed), vec![(0, 32.0)]);
}

#[test]
fn activating_priority_charging_without_a_transaction_still_applies_nothing() {
    let profiles = [profile(
        1,
        ChargingProfilePurpose::PriorityCharging,
        0,
        schedule(&[(0, 32.0)]),
    )];
    let context = CompositionContext {
        transaction_id: None,
        transaction_started_at: None,
        priority_charging: true,
        ..context()
    };

    assert_eq!(
        compose(&profiles.iter().collect::<Vec<_>>(), &context),
        None
    );
}

#[test]
fn a_transaction_profile_for_a_different_transaction_is_ignored() {
    let mut other = profile(1, ChargingProfilePurpose::Tx, 0, schedule(&[(0, 20.0)]));
    other.profile.transaction_id = Some(TransactionId(999));
    let profiles = [
        other,
        profile(
            2,
            ChargingProfilePurpose::TxDefault,
            0,
            schedule(&[(0, 16.0)]),
        ),
    ];

    let composed = compose(&profiles.iter().collect::<Vec<_>>(), &context()).unwrap();

    assert_eq!(limits(&composed), vec![(0, 16.0)]);
}

#[test]
fn a_profile_outside_its_validity_window_contributes_nothing_while_the_window_is_shut() {
    let mut later = profile(
        1,
        ChargingProfilePurpose::TxDefault,
        0,
        schedule(&[(0, 16.0)]),
    );
    later.profile.valid_from = Some(at(1_800));
    let profiles = [later];

    let composed = compose(&profiles.iter().collect::<Vec<_>>(), &context()).unwrap();

    // Nothing until the window opens; the composite then starts there, and its periods are
    // measured from its own start rather than from `now`.
    assert_eq!(composed.start, at(1_800));
    assert_eq!(limits(&composed), vec![(0, 16.0)]);
}

#[test]
fn an_absolute_schedule_is_anchored_to_its_own_start_not_to_now() {
    let mut absolute = profile(
        1,
        ChargingProfilePurpose::TxDefault,
        0,
        schedule(&[(0, 16.0), (600, 32.0)]),
    );
    absolute.profile.schedules[0].start_schedule = Some(at(300));
    let profiles = [absolute];

    let composed = compose(&profiles.iter().collect::<Vec<_>>(), &context()).unwrap();

    // The schedule starts 300 s from now, and its periods are then measured from that start.
    assert_eq!(composed.start, at(300));
    assert_eq!(limits(&composed), vec![(0, 16.0), (600, 32.0)]);
}

#[test]
fn a_relative_schedule_is_anchored_to_the_transactions_start() {
    let mut relative = profile(
        1,
        ChargingProfilePurpose::TxDefault,
        0,
        schedule(&[(0, 16.0), (600, 32.0)]),
    );
    relative.profile.kind = ChargingProfileKind::Relative;
    let profiles = [relative];
    // The transaction started 300 s before this composition begins, so the schedule is already
    // 300 s in and its second period arrives 300 s from now, not 600.
    let context = CompositionContext {
        transaction_started_at: Some(at(-300)),
        ..context()
    };

    let composed = compose(&profiles.iter().collect::<Vec<_>>(), &context).unwrap();

    assert_eq!(limits(&composed), vec![(0, 16.0), (300, 32.0)]);
}

#[test]
fn a_relative_schedule_with_no_known_transaction_start_is_skipped_rather_than_guessed_at() {
    let mut relative = profile(
        1,
        ChargingProfilePurpose::TxDefault,
        0,
        schedule(&[(0, 16.0)]),
    );
    relative.profile.kind = ChargingProfileKind::Relative;
    let profiles = [relative];
    let context = CompositionContext {
        transaction_started_at: None,
        ..context()
    };

    assert_eq!(
        compose(&profiles.iter().collect::<Vec<_>>(), &context),
        None
    );
}

#[test]
fn a_daily_recurring_schedule_repeats_from_its_anchor() {
    let mut recurring = profile(
        1,
        ChargingProfilePurpose::TxDefault,
        0,
        schedule(&[(0, 10.0), (3_600, 32.0)]),
    );
    recurring.profile.kind = ChargingProfileKind::Recurring;
    recurring.profile.recurrency = Some(RecurrencyKind::Daily);
    // Anchored a day and a half back: the schedule is currently 12 h into its repetition.
    recurring.profile.schedules[0].start_schedule = Some(at(-36 * 3_600));
    recurring.profile.schedules[0].duration_secs = Some(7_200);
    let profiles = [recurring];
    let context = CompositionContext {
        duration_secs: 13 * 3_600,
        ..context()
    };

    let composed = compose(&profiles.iter().collect::<Vec<_>>(), &context).unwrap();

    // 12 h into the day, the schedule's 2 h window is long over; it comes back 12 h later.
    assert_eq!(composed.start, at(12 * 3_600));
    assert_eq!(limits(&composed), vec![(0, 10.0)]);
}

#[test]
fn consecutive_periods_with_the_same_limit_are_merged() {
    let profiles = [
        profile(
            1,
            ChargingProfilePurpose::TxDefault,
            0,
            schedule(&[(0, 16.0), (600, 16.0), (1_200, 16.0)]),
        ),
        // A cap that changes at 900 s but never actually binds - it must not split the result.
        profile(
            2,
            ChargingProfilePurpose::ChargePointMax,
            0,
            schedule(&[(0, 32.0), (900, 30.0)]),
        ),
    ];

    let composed = compose(&profiles.iter().collect::<Vec<_>>(), &context()).unwrap();

    assert_eq!(limits(&composed), vec![(0, 16.0)]);
}

#[test]
fn a_schedule_in_another_unit_is_skipped_when_no_supply_characteristics_are_supplied() {
    let mut watts = profile(
        1,
        ChargingProfilePurpose::TxDefault,
        0,
        schedule(&[(0, 7_360.0)]),
    );
    watts.profile.schedules[0].rate_unit = ChargingRateUnit::Watts;
    let profiles = [watts];

    assert_eq!(
        compose(&profiles.iter().collect::<Vec<_>>(), &context()),
        None
    );
}

#[test]
fn a_schedule_in_another_unit_is_converted_when_the_supply_is_known() {
    let mut watts = profile(
        1,
        ChargingProfilePurpose::TxDefault,
        0,
        schedule(&[(0, 7_360.0)]),
    );
    watts.profile.schedules[0].rate_unit = ChargingRateUnit::Watts;
    let profiles = [watts];
    let context = CompositionContext {
        supply: Some(SupplyCharacteristics {
            nominal_voltage_v: 230,
            phases: 1,
        }),
        ..context()
    };

    let composed = compose(&profiles.iter().collect::<Vec<_>>(), &context).unwrap();

    // 7360 W at 230 V single-phase is 32 A.
    assert_eq!(limits(&composed), vec![(0, 32.0)]);
}

#[test]
fn composing_in_watts_converts_an_amp_schedule_the_other_way() {
    let profiles = [profile(
        1,
        ChargingProfilePurpose::TxDefault,
        0,
        schedule(&[(0, 32.0)]),
    )];
    let context = CompositionContext {
        rate_unit: ChargingRateUnit::Watts,
        supply: Some(SupplyCharacteristics {
            nominal_voltage_v: 230,
            phases: 3,
        }),
        ..context()
    };

    let composed = compose(&profiles.iter().collect::<Vec<_>>(), &context).unwrap();

    assert_eq!(composed.rate_unit, ChargingRateUnit::Watts);
    assert_eq!(limits(&composed), vec![(0, 32.0 * 230.0 * 3.0)]);
}

#[test]
fn composition_never_runs_past_the_requested_duration() {
    let profiles = [profile(
        1,
        ChargingProfilePurpose::TxDefault,
        0,
        schedule(&[(0, 16.0), (7_200, 32.0)]),
    )];
    let context = CompositionContext {
        duration_secs: 3_600,
        ..context()
    };

    let composed = compose(&profiles.iter().collect::<Vec<_>>(), &context).unwrap();

    assert_eq!(composed.duration_secs, 3_600);
    assert_eq!(limits(&composed), vec![(0, 16.0)]);
}

#[test]
fn the_current_limit_in_milliamps_is_the_composite_limit_now() {
    let profiles = [profile(
        1,
        ChargingProfilePurpose::TxDefault,
        0,
        schedule(&[(0, 16.0), (600, 32.0)]),
    )];

    assert_eq!(
        current_limit_ma(&profiles.iter().collect::<Vec<_>>(), &context()),
        Some(16_000)
    );
}

#[test]
fn a_zero_limit_is_a_real_limit_not_an_absent_one() {
    // OCPP uses a 0 A period to suspend charging without ending the transaction, so this must
    // reach hardware as `Some(0)` - `None` would mean "no limit at all" and let the EV draw
    // whatever it likes, the exact opposite.
    let profiles = [profile(
        1,
        ChargingProfilePurpose::TxDefault,
        0,
        schedule(&[(0, 0.0)]),
    )];

    assert_eq!(
        current_limit_ma(&profiles.iter().collect::<Vec<_>>(), &context()),
        Some(0)
    );
}

#[test]
fn a_negative_limit_is_clamped_to_zero_rather_than_wrapping() {
    let profiles = [profile(
        1,
        ChargingProfilePurpose::TxDefault,
        0,
        schedule(&[(0, -5.0)]),
    )];

    assert_eq!(
        current_limit_ma(&profiles.iter().collect::<Vec<_>>(), &context()),
        Some(0)
    );
}

#[test]
fn the_next_change_is_when_the_composite_limit_actually_moves() {
    let profiles = [profile(
        1,
        ChargingProfilePurpose::TxDefault,
        0,
        schedule(&[(0, 16.0), (600, 32.0)]),
    )];

    let composed = compose(&profiles.iter().collect::<Vec<_>>(), &context()).unwrap();

    assert_eq!(composed.next_change_after(at(0)), Some(at(600)));
    assert_eq!(composed.next_change_after(at(600)), None);
}

#[test]
fn the_winning_periods_phase_count_is_carried_through() {
    let mut three_phase = profile(
        1,
        ChargingProfilePurpose::TxDefault,
        0,
        schedule(&[(0, 16.0)]),
    );
    three_phase.profile.schedules[0].periods[0].number_phases = Some(3);
    let profiles = [three_phase];

    let composed = compose(&profiles.iter().collect::<Vec<_>>(), &context()).unwrap();

    assert_eq!(composed.periods[0].number_phases, Some(3));
}

#[test]
fn a_minimum_charging_rate_is_reported_but_never_raises_the_limit() {
    let mut with_minimum = profile(
        1,
        ChargingProfilePurpose::TxDefault,
        0,
        schedule(&[(0, 6.0)]),
    );
    with_minimum.profile.schedules[0].min_charging_rate = Some(10.0);
    let profiles = [
        with_minimum,
        profile(
            2,
            ChargingProfilePurpose::ChargePointMax,
            0,
            schedule(&[(0, 8.0)]),
        ),
    ];

    let composed = compose(&profiles.iter().collect::<Vec<_>>(), &context()).unwrap();

    assert_eq!(composed.min_charging_rate, Some(10.0));
    assert_eq!(limits(&composed), vec![(0, 6.0)]);
}

#[test]
fn a_profile_whose_schedule_has_not_started_yet_contributes_from_its_start() {
    let mut later = profile(
        1,
        ChargingProfilePurpose::TxDefault,
        0,
        schedule(&[(0, 16.0)]),
    );
    later.profile.schedules[0].start_schedule = Some(at(1_200));
    let profiles = [later];

    let composed = compose(&profiles.iter().collect::<Vec<_>>(), &context()).unwrap();

    assert_eq!(composed.start, at(1_200));
    assert_eq!(limits(&composed), vec![(0, 16.0)]);
    assert_eq!(
        current_limit_ma(&profiles.iter().collect::<Vec<_>>(), &context()),
        None,
        "nothing limits the connector until the schedule starts"
    );
}

#[test]
fn a_schedule_that_ends_leaves_no_limit_behind() {
    let mut ending = profile(
        1,
        ChargingProfilePurpose::TxDefault,
        0,
        schedule(&[(0, 16.0)]),
    );
    // Anchored explicitly: an `Absolute` schedule with no `start_schedule` falls back to "starts
    // whenever you ask", which would restart on every evaluation and never end. The version
    // adapters stamp the installation time for exactly this reason - see `schedule_anchor`.
    ending.profile.schedules[0].start_schedule = Some(at(0));
    ending.profile.schedules[0].duration_secs = Some(600);
    let profiles = [ending];

    let composed = compose(&profiles.iter().collect::<Vec<_>>(), &context()).unwrap();

    assert_eq!(limits(&composed), vec![(0, 16.0)]);
    assert_eq!(composed.duration_secs, 600);
    assert_eq!(composed.next_change_after(at(0)), Some(at(600)));
    // And after it ends, nothing is limiting the connector any more.
    let after = CompositionContext {
        now: at(600) + Duration::seconds(1),
        ..context()
    };
    assert_eq!(
        current_limit_ma(&profiles.iter().collect::<Vec<_>>(), &after),
        None
    );
}

#[test]
fn a_dynamic_schedule_is_anchored_to_when_its_limit_last_arrived() {
    let mut dynamic = profile(
        1,
        ChargingProfilePurpose::TxDefault,
        0,
        schedule(&[(0, 16.0)]),
    );
    dynamic.profile.kind = ChargingProfileKind::Dynamic;
    // Updated 10 minutes ago. A dynamic period has no `startSchedule` to measure from - it took
    // effect when it arrived - so it must be limiting right now rather than waiting for anything.
    dynamic.profile.dyn_update_time = Some(at(-600));
    let profiles = [dynamic];

    let composed = compose(&profiles.iter().collect::<Vec<_>>(), &context()).unwrap();

    assert_eq!(limits(&composed), vec![(0, 16.0)]);
}

#[test]
fn a_dynamic_profile_whose_csms_went_quiet_falls_through_to_the_next_profile() {
    let mut dynamic = profile(
        1,
        ChargingProfilePurpose::TxDefault,
        5,
        schedule(&[(0, 32.0)]),
    );
    dynamic.profile.kind = ChargingProfileKind::Dynamic;
    dynamic.profile.schedules[0].duration_secs = Some(300);
    // Last updated 10 minutes ago against a 5-minute deadline the CSMS set itself: K28.FR.13.
    dynamic.profile.dyn_update_time = Some(at(-600));
    let profiles = [
        dynamic,
        profile(
            2,
            ChargingProfilePurpose::TxDefault,
            0,
            schedule(&[(0, 8.0)]),
        ),
    ];

    // The stale profile outranks the fallback on stack level, so this is the expiry deciding -
    // not precedence.
    let composed = compose(&profiles.iter().collect::<Vec<_>>(), &context()).unwrap();
    assert_eq!(limits(&composed), vec![(0, 8.0)]);
}

/// CV13's and CV17's cases: an external charging limit is not a stored profile, so it reaches
/// composition through [`external_charging_limits`]/[`composing_profiles`] rather than through the
/// store. What it does once it gets there is one of the two rules already covered above - the
/// [`ChargingProfilePurpose::ExternalConstraints`] cap for a constraint, the
/// [`ChargingProfilePurpose::LocalGeneration`] addition for capacity - so these check the joining,
/// the two ways a limit can fail to reach it, and that the two kinds stay apart.
mod external_charging_limits_reach_composition {
    use super::*;
    use crate::state::{ChargePointEvent, ChargePointState, ChargingLimitSource};

    fn ems_limit(schedule: Option<ChargingSchedule>) -> crate::state::ExternalChargingLimit {
        crate::state::ExternalChargingLimit {
            is_local_generation: false,
            source: ChargingLimitSource::Ems,
            is_grid_critical: None,
            schedule,
        }
    }

    /// The same EMS reporting locally generated capacity rather than a constraint (CV17) - same
    /// source, opposite meaning, which is exactly why the flag has to be carried and why the two
    /// cannot share a slot.
    fn local_generation(schedule: Option<ChargingSchedule>) -> crate::state::ExternalChargingLimit {
        crate::state::ExternalChargingLimit {
            is_local_generation: true,
            ..ems_limit(schedule)
        }
    }

    /// A charge point with one 32 A `TxDefault` profile installed on EVSE 0, which every case here
    /// then constrains (or fails to).
    fn state_with_a_32a_profile() -> ChargePointState {
        let mut state = ChargePointState::new([1]);
        state.apply(ChargePointEvent::ChargingProfileSet {
            scope: ChargingProfileScope::Evse(0),
            profile: alloc::boxed::Box::new(
                profile(
                    1,
                    ChargingProfilePurpose::TxDefault,
                    0,
                    schedule(&[(0, 32.0)]),
                )
                .profile,
            ),
        });
        state
    }

    fn composed_limit_ma(state: &ChargePointState) -> Option<u32> {
        let external = external_charging_limits(state, 0);
        current_limit_ma(&composing_profiles(state, 0, &external), &context())
    }

    #[test]
    fn an_evse_limit_caps_the_profile_the_csms_installed() {
        let mut state = state_with_a_32a_profile();
        state.apply(ChargePointEvent::ExternalChargingLimitSet {
            evse_id: Some(0),
            limit: ems_limit(Some(schedule(&[(0, 6.0)]))),
        });

        assert_eq!(composed_limit_ma(&state), Some(6_000));
    }

    #[test]
    fn a_station_wide_limit_and_an_evse_limit_both_bind_and_the_lowest_wins() {
        let mut state = state_with_a_32a_profile();
        state.apply(ChargePointEvent::ExternalChargingLimitSet {
            evse_id: None,
            limit: ems_limit(Some(schedule(&[(0, 10.0)]))),
        });
        state.apply(ChargePointEvent::ExternalChargingLimitSet {
            evse_id: Some(0),
            limit: ems_limit(Some(schedule(&[(0, 16.0)]))),
        });

        assert_eq!(composed_limit_ma(&state), Some(10_000));
    }

    /// An external limit never *raises* what the CSMS asked for: it is a constraint the station is
    /// under, not a permission it was granted.
    #[test]
    fn a_limit_above_the_installed_profile_changes_nothing() {
        let mut state = state_with_a_32a_profile();
        state.apply(ChargePointEvent::ExternalChargingLimitSet {
            evse_id: Some(0),
            limit: ems_limit(Some(schedule(&[(0, 63.0)]))),
        });

        assert_eq!(composed_limit_ma(&state), Some(32_000));
    }

    /// OCPP's `chargingSchedule` is optional on `NotifyChargingLimit`, so an external system may
    /// say "you are limited" without saying to what. There is then no number to enforce, and
    /// inventing one - zero, or the profile's own limit - would be worse than the honest nothing
    /// this does. Recording it warns for exactly that reason.
    #[test]
    fn a_limit_with_no_schedule_reports_but_does_not_cap() {
        let mut state = state_with_a_32a_profile();
        state.apply(ChargePointEvent::ExternalChargingLimitSet {
            evse_id: Some(0),
            limit: ems_limit(None),
        });

        assert!(state.evses[0].external_charging_limit.is_some());
        assert_eq!(composed_limit_ma(&state), Some(32_000));
    }

    /// K13.FR.01: once withdrawn, the station must stop limiting on it.
    #[test]
    fn a_cleared_limit_stops_capping() {
        let mut state = state_with_a_32a_profile();
        state.apply(ChargePointEvent::ExternalChargingLimitSet {
            evse_id: Some(0),
            limit: ems_limit(Some(schedule(&[(0, 6.0)]))),
        });
        state.apply(ChargePointEvent::ExternalChargingLimitCleared {
            is_local_generation: false,
            evse_id: Some(0),
            source: ChargingLimitSource::Ems,
        });

        assert_eq!(composed_limit_ma(&state), Some(32_000));
    }

    /// CV17, through the same path CV13 built: an external schedule flagged as locally generated
    /// capacity is treated internally as a `LocalGeneration` profile (K27.FR.01), so it *widens*
    /// the connector's limit instead of narrowing it. 32 A installed plus 6 A of sun is 38 A.
    #[test]
    fn a_local_generation_limit_widens_the_profile_the_csms_installed() {
        let mut state = state_with_a_32a_profile();
        state.apply(ChargePointEvent::ExternalChargingLimitSet {
            evse_id: Some(0),
            limit: local_generation(Some(schedule(&[(0, 6.0)]))),
        });

        assert_eq!(composed_limit_ma(&state), Some(38_000));
    }

    /// K27.FR.05's premise, which is what forced the second slot: a station can be under an EMS
    /// constraint *and* have local generation at the same time, and the two do opposite things.
    /// 32 A installed, capped to 10 A by the EMS, plus 6 A generated on site, is 16 A.
    #[test]
    fn a_constraint_and_local_generation_are_both_in_force_at_once() {
        let mut state = state_with_a_32a_profile();
        state.apply(ChargePointEvent::ExternalChargingLimitSet {
            evse_id: Some(0),
            limit: ems_limit(Some(schedule(&[(0, 10.0)]))),
        });
        state.apply(ChargePointEvent::ExternalChargingLimitSet {
            evse_id: Some(0),
            limit: local_generation(Some(schedule(&[(0, 6.0)]))),
        });

        // Neither evicted the other - the bug a single slot would have had.
        assert!(state.evses[0].external_charging_limit.is_some());
        assert!(state.evses[0].local_generation_limit.is_some());
        assert_eq!(composed_limit_ma(&state), Some(16_000));
    }

    /// The two slots clear independently, which is why the event says which one it means: both
    /// limits here are from `Ems`, so the source alone could not have chosen between them.
    #[test]
    fn clearing_the_generation_leaves_the_constraint_standing() {
        let mut state = state_with_a_32a_profile();
        state.apply(ChargePointEvent::ExternalChargingLimitSet {
            evse_id: Some(0),
            limit: ems_limit(Some(schedule(&[(0, 10.0)]))),
        });
        state.apply(ChargePointEvent::ExternalChargingLimitSet {
            evse_id: Some(0),
            limit: local_generation(Some(schedule(&[(0, 6.0)]))),
        });
        state.apply(ChargePointEvent::ExternalChargingLimitCleared {
            evse_id: Some(0),
            source: ChargingLimitSource::Ems,
            is_local_generation: true,
        });

        assert!(state.evses[0].external_charging_limit.is_some());
        assert!(state.evses[0].local_generation_limit.is_none());
        assert_eq!(composed_limit_ma(&state), Some(10_000));
    }

    /// Station-wide generation reaches an EVSE the same way a station-wide constraint does, and
    /// the four slots (two scopes × two kinds) all compose together rather than shadowing one
    /// another: 32 A installed, capped to 20 A station-wide, plus 2 A of station generation and
    /// 3 A of this EVSE's own.
    #[test]
    fn all_four_slots_compose_together() {
        let mut state = state_with_a_32a_profile();
        state.apply(ChargePointEvent::ExternalChargingLimitSet {
            evse_id: None,
            limit: ems_limit(Some(schedule(&[(0, 20.0)]))),
        });
        state.apply(ChargePointEvent::ExternalChargingLimitSet {
            evse_id: None,
            limit: local_generation(Some(schedule(&[(0, 2.0)]))),
        });
        state.apply(ChargePointEvent::ExternalChargingLimitSet {
            evse_id: Some(0),
            limit: local_generation(Some(schedule(&[(0, 3.0)]))),
        });

        // The two generation slots are two candidates at the same stack level, not two sources to
        // add up - the EVSE's own is written last and leads.
        assert_eq!(composed_limit_ma(&state), Some(23_000));
    }

    /// A limit on one EVSE is not a limit on another - only the station-wide slot crosses.
    #[test]
    fn an_evse_limit_does_not_reach_another_evse() {
        let mut state = ChargePointState::new([1, 1]);
        state.apply(ChargePointEvent::ChargingProfileSet {
            scope: ChargingProfileScope::ChargePoint,
            profile: alloc::boxed::Box::new(
                profile(
                    1,
                    ChargingProfilePurpose::TxDefault,
                    0,
                    schedule(&[(0, 32.0)]),
                )
                .profile,
            ),
        });
        state.apply(ChargePointEvent::ExternalChargingLimitSet {
            evse_id: Some(0),
            limit: ems_limit(Some(schedule(&[(0, 6.0)]))),
        });

        assert_eq!(composed_limit_ma(&state), Some(6_000));
        let external = external_charging_limits(&state, 1);
        assert_eq!(
            current_limit_ma(&composing_profiles(&state, 1, &external), &context()),
            Some(32_000)
        );
    }
}
