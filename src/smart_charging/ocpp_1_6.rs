//! OCPP 1.6J wire adapters for the Smart Charging block (B2.5).
//!
//! 1.6J's differences from 2.x run deeper here than naming, and each is handled explicitly:
//!
//! - **No EVSE concept.** Profiles are addressed by a single flat `connectorId`, where `0` means
//!   the whole charge point. This module resolves one through
//!   [`crate::topology::unflatten_ocpp_1_6_connector_id`] and installs at the owning EVSE's scope -
//!   the same deliberate reduction `crate::remote_control`/`crate::reservation`'s 1.6J handlers
//!   make, and for the same reason: this crate's profile store scopes at EVSE granularity, and
//!   widening it for one version would push a 1.6J-shaped concern into the version-independent
//!   core.
//! - **One schedule per profile**, not up to three - so there is never a rate unit to choose
//!   between when mapping in, and only one to report back out.
//! - **Three purposes, not five**: no external constraints and no priority charging. On the way
//!   out an external constraint reports as `ChargePointMaxProfile` and priority charging as
//!   `TxProfile` (see [`wire_purpose`]) - lossy, and documented as such.
//! - **`transactionId` is an integer**, which happens to match this crate's own `TransactionId`
//!   exactly, so 1.6J is the one version where a `TxProfile`'s transaction matching is exact
//!   rather than a parse-and-hope of a free-form string.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use chrono::{DateTime, Utc};

use ocpp_client::ocpp_1_6::OCPP1_6Client;
use ocpp_client::ocpp_types::v16::common::{
    ChargingProfileKind as WireKind, ChargingProfilePurpose as WirePurpose,
    ChargingRateUnit as WireRateUnit, ChargingSchedule as WireSchedule,
    ChargingSchedulePeriodItem as WirePeriod, ClearChargingProfileResponseStatus,
    CsChargingProfiles, GetCompositeScheduleResponseStatus, RecurrencyKind as WireRecurrency,
    SetChargingProfileResponseStatus,
};
use ocpp_client::ocpp_types::v16::{
    ClearChargingProfileRequest, ClearChargingProfileResponse, GetCompositeScheduleRequest,
    GetCompositeScheduleResponse, SetChargingProfileRequest, SetChargingProfileResponse,
};

use crate::actor::ChargePointActor;
use crate::clock::Clock;
use crate::smart_charging::{
    ChargingLimitProjection, ClearChargingProfileHandler, ClearChargingProfileOutcome,
    CompositeSchedule, GetCompositeScheduleHandler, GetCompositeScheduleOutcome,
    SetChargingProfileHandler, SetChargingProfileOutcome, handle_clear_charging_profile,
    handle_get_composite_schedule, handle_set_charging_profile,
};
use crate::state::{
    ChargingProfile, ChargingProfileCriteria, ChargingProfileId, ChargingProfileKind,
    ChargingProfilePurpose, ChargingProfileScope, ChargingRateUnit, ChargingSchedule,
    ChargingSchedulePeriod, RecurrencyKind, TransactionId,
};
use crate::topology::{flatten_ocpp_1_6_connector_id, unflatten_ocpp_1_6_connector_id};

fn map_purpose(purpose: &WirePurpose) -> ChargingProfilePurpose {
    match purpose {
        WirePurpose::ChargePointMaxProfile => ChargingProfilePurpose::ChargePointMax,
        WirePurpose::TxProfile => ChargingProfilePurpose::Tx,
        _ => ChargingProfilePurpose::TxDefault,
    }
}

/// This crate's purpose enum back onto 1.6J's - lossy, and the loss is stated rather than hidden.
///
/// 1.6J has neither an external-constraints purpose nor 2.1's priority charging. An external
/// constraint reports as `ChargePointMaxProfile`: both are station-level caps that the driver and
/// the CSMS should read the same way, and the alternative (omitting it) would understate the limit
/// actually in force. Priority charging reports as `TxProfile`, being a transaction-scoped limit.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn wire_purpose(purpose: ChargingProfilePurpose) -> WirePurpose {
    match purpose {
        ChargingProfilePurpose::ChargePointMax | ChargingProfilePurpose::ExternalConstraints => {
            WirePurpose::ChargePointMaxProfile
        }
        ChargingProfilePurpose::TxDefault => WirePurpose::TxDefaultProfile,
        ChargingProfilePurpose::Tx | ChargingProfilePurpose::PriorityCharging => {
            WirePurpose::TxProfile
        }
    }
}

fn map_kind(kind: &WireKind) -> ChargingProfileKind {
    match kind {
        WireKind::Absolute => ChargingProfileKind::Absolute,
        WireKind::Recurring => ChargingProfileKind::Recurring,
        _ => ChargingProfileKind::Relative,
    }
}

fn map_recurrency(kind: &WireRecurrency) -> RecurrencyKind {
    match kind {
        WireRecurrency::Weekly => RecurrencyKind::Weekly,
        _ => RecurrencyKind::Daily,
    }
}

fn map_rate_unit(unit: &WireRateUnit) -> ChargingRateUnit {
    match unit {
        WireRateUnit::W => ChargingRateUnit::Watts,
        _ => ChargingRateUnit::Amps,
    }
}

fn wire_rate_unit(unit: ChargingRateUnit) -> WireRateUnit {
    match unit {
        ChargingRateUnit::Amps => WireRateUnit::A,
        ChargingRateUnit::Watts => WireRateUnit::W,
    }
}

fn parse_time(raw: &Option<alloc::string::String>) -> Option<DateTime<Utc>> {
    raw.as_ref()
        .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        .map(|parsed| parsed.with_timezone(&Utc))
}

fn map_schedule(schedule: &WireSchedule) -> ChargingSchedule {
    ChargingSchedule {
        // 1.6J schedules have no id of their own; `0` is this crate's documented stand-in (see
        // `ChargingSchedule::id`).
        id: 0,
        start_schedule: parse_time(&schedule.start_schedule),
        duration_secs: schedule.duration.and_then(|d| u32::try_from(d).ok()),
        rate_unit: map_rate_unit(&schedule.charging_rate_unit),
        min_charging_rate: schedule.min_charging_rate,
        periods: {
            let mut periods: Vec<ChargingSchedulePeriod> = schedule
                .charging_schedule_period
                .iter()
                .filter_map(|period| {
                    Some(ChargingSchedulePeriod {
                        start_period_secs: u32::try_from(period.start_period).ok()?,
                        limit: period.limit,
                        number_phases: period.number_phases.and_then(|n| u8::try_from(n).ok()),
                    })
                })
                .collect();
            periods.sort_by_key(|period| period.start_period_secs);
            periods
        },
    }
}

fn map_profile(profile: &CsChargingProfiles) -> ChargingProfile {
    ChargingProfile {
        id: ChargingProfileId(profile.charging_profile_id as i32),
        stack_level: u32::try_from(profile.stack_level).unwrap_or(0),
        purpose: map_purpose(&profile.charging_profile_purpose),
        kind: map_kind(&profile.charging_profile_kind),
        recurrency: profile.recurrency_kind.as_ref().map(map_recurrency),
        valid_from: parse_time(&profile.valid_from),
        valid_to: parse_time(&profile.valid_to),
        // The one place 1.6J is *less* lossy than 2.x: its transaction id is already an integer,
        // so no parse can fail here.
        transaction_id: profile
            .transaction_id
            .and_then(|id| u64::try_from(id).ok())
            .map(TransactionId),
        schedules: alloc::vec![map_schedule(&profile.charging_schedule)],
        // 1.6J has no dynamic charging profiles (OCPP K28 is 2.1-only), so nothing on this wire
        // can produce one.
        dyn_update_interval_secs: None,
        dyn_update_time: None,
    }
}

/// This crate's composite schedule onto 1.6J's, which carries its start separately from the
/// schedule body.
fn wire_composite_schedule(composed: &CompositeSchedule) -> WireSchedule {
    WireSchedule {
        charging_rate_unit: wire_rate_unit(composed.rate_unit),
        charging_schedule_period: composed
            .periods
            .iter()
            .map(|period| WirePeriod {
                limit: period.limit,
                number_phases: period.number_phases.map(i64::from),
                start_period: i64::from(period.start_period_secs),
            })
            .collect(),
        duration: Some(i64::from(composed.duration_secs)),
        min_charging_rate: composed.min_charging_rate,
        start_schedule: Some(composed.start.to_rfc3339()),
    }
}

/// Wraps an [`OCPP1_6Client`] with the connector topology its flat `connectorId` addressing needs,
/// plus the [`Clock`] the handlers need - the same pairing
/// [`crate::reservation::Ocpp1_6ReserveNowHandler`] uses for the topology half.
pub struct Ocpp1_6SmartChargingHandler<C> {
    client: OCPP1_6Client,
    connector_counts: Vec<usize>,
    clock: C,
}

impl<C: Clock + Clone + Send + Sync + 'static> Ocpp1_6SmartChargingHandler<C> {
    /// Wraps `client`, resolving 1.6J's flat `connectorId`s against `connector_counts`
    /// (`connector_counts[evse_id]` is that EVSE's connector count, exactly as this crate's
    /// topology helpers take it) and sourcing timestamps from `clock`.
    pub fn with_clock(
        client: OCPP1_6Client,
        connector_counts: impl IntoIterator<Item = usize>,
        clock: C,
    ) -> Self {
        Self {
            client,
            connector_counts: connector_counts.into_iter().collect(),
            clock,
        }
    }
}

#[cfg(feature = "std")]
impl Ocpp1_6SmartChargingHandler<crate::clock::SystemClock> {
    /// [`Self::with_clock`] with [`crate::clock::SystemClock`].
    pub fn new(client: OCPP1_6Client, connector_counts: impl IntoIterator<Item = usize>) -> Self {
        Self::with_clock(client, connector_counts, crate::clock::SystemClock)
    }
}

/// 1.6J's flat `connectorId` onto a profile scope: `0` is the whole charge point, anything else is
/// the EVSE owning that connector.
///
/// The specific connector within the EVSE is deliberately dropped - see this module's docs.
fn map_scope(connector_counts: &[usize], connector_id: i64) -> Option<ChargingProfileScope> {
    if connector_id == 0 {
        return Some(ChargingProfileScope::ChargePoint);
    }
    let (evse_id, _) = unflatten_ocpp_1_6_connector_id(connector_counts, connector_id)?;
    Some(ChargingProfileScope::Evse(evse_id))
}

fn set_response(status: SetChargingProfileResponseStatus) -> SetChargingProfileResponse {
    SetChargingProfileResponse { status }
}

fn clear_response(status: ClearChargingProfileResponseStatus) -> ClearChargingProfileResponse {
    ClearChargingProfileResponse { status }
}

#[async_trait::async_trait]
impl<C: Clock + Clone + Send + Sync + 'static> SetChargingProfileHandler
    for Ocpp1_6SmartChargingHandler<C>
{
    async fn register_set_charging_profile_handler(&self, actor: ChargePointActor) {
        let clock = self.clock.clone();
        let connector_counts = self.connector_counts.clone();
        self.client
            .on_set_charging_profile(move |request: SetChargingProfileRequest, _client| {
                let actor = actor.clone();
                let clock = clock.clone();
                let connector_counts = connector_counts.clone();
                async move {
                    let Some(scope) = map_scope(&connector_counts, request.connector_id) else {
                        return Ok(set_response(SetChargingProfileResponseStatus::Rejected));
                    };
                    let outcome = handle_set_charging_profile(
                        &actor,
                        scope,
                        map_profile(&request.cs_charging_profiles),
                        clock.now(),
                    )
                    .await;
                    Ok(set_response(match outcome {
                        SetChargingProfileOutcome::Accepted => {
                            SetChargingProfileResponseStatus::Accepted
                        }
                        SetChargingProfileOutcome::Rejected(_) => {
                            SetChargingProfileResponseStatus::Rejected
                        }
                    }))
                }
            })
            .await;
    }
}

#[async_trait::async_trait]
impl<C: Clock + Clone + Send + Sync + 'static> ClearChargingProfileHandler
    for Ocpp1_6SmartChargingHandler<C>
{
    async fn register_clear_charging_profile_handler(&self, actor: ChargePointActor) {
        let connector_counts = self.connector_counts.clone();
        self.client
            .on_clear_charging_profile(move |request: ClearChargingProfileRequest, _client| {
                let actor = actor.clone();
                let connector_counts = connector_counts.clone();
                async move {
                    let criteria = ChargingProfileCriteria {
                        id: request.id.map(|id| ChargingProfileId(id as i32)),
                        scope: request
                            .connector_id
                            .and_then(|id| map_scope(&connector_counts, id)),
                        purpose: request.charging_profile_purpose.as_ref().map(map_purpose),
                        stack_level: request
                            .stack_level
                            .and_then(|level| u32::try_from(level).ok()),
                    };
                    let outcome = handle_clear_charging_profile(&actor, criteria).await;
                    Ok(clear_response(match outcome {
                        ClearChargingProfileOutcome::Accepted => {
                            ClearChargingProfileResponseStatus::Accepted
                        }
                        ClearChargingProfileOutcome::Unknown => {
                            ClearChargingProfileResponseStatus::Unknown
                        }
                    }))
                }
            })
            .await;
    }
}

#[async_trait::async_trait]
impl<C: Clock + Clone + Send + Sync + 'static> GetCompositeScheduleHandler
    for Ocpp1_6SmartChargingHandler<C>
{
    async fn register_get_composite_schedule_handler(
        &self,
        actor: ChargePointActor,
        projection: Arc<ChargingLimitProjection>,
    ) {
        let clock = self.clock.clone();
        let connector_counts = self.connector_counts.clone();
        self.client
            .on_get_composite_schedule(move |request: GetCompositeScheduleRequest, _client| {
                let actor = actor.clone();
                let clock = clock.clone();
                let projection = projection.clone();
                let connector_counts = connector_counts.clone();
                async move {
                    let rejected = GetCompositeScheduleResponse {
                        charging_schedule: None,
                        connector_id: None,
                        schedule_start: None,
                        status: GetCompositeScheduleResponseStatus::Rejected,
                    };
                    // `connectorId` 0 asks for the whole charge point, which a per-EVSE composite
                    // cannot answer for - refused rather than answered for one EVSE as though it
                    // spoke for all of them, exactly as the 2.x adapters refuse `evseId` 0.
                    if request.connector_id <= 0 {
                        return Ok(rejected);
                    }
                    let Some((evse_id, _)) =
                        unflatten_ocpp_1_6_connector_id(&connector_counts, request.connector_id)
                    else {
                        return Ok(rejected);
                    };
                    let duration_secs = u32::try_from(request.duration).unwrap_or(0);
                    let rate_unit = request
                        .charging_rate_unit
                        .as_ref()
                        .map_or(ChargingRateUnit::Amps, map_rate_unit);
                    let outcome = handle_get_composite_schedule(
                        &actor,
                        &projection,
                        &clock,
                        evse_id,
                        duration_secs,
                        rate_unit,
                    )
                    .await;
                    Ok(match outcome {
                        GetCompositeScheduleOutcome::Accepted(composed) => {
                            GetCompositeScheduleResponse {
                                charging_schedule: composed.as_ref().map(wire_composite_schedule),
                                connector_id: flatten_ocpp_1_6_connector_id(
                                    &connector_counts,
                                    evse_id,
                                    0,
                                ),
                                schedule_start: composed
                                    .as_ref()
                                    .map(|composed| composed.start.to_rfc3339()),
                                status: GetCompositeScheduleResponseStatus::Accepted,
                            }
                        }
                        GetCompositeScheduleOutcome::Rejected => rejected,
                    })
                }
            })
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire_schedule() -> WireSchedule {
        WireSchedule {
            charging_rate_unit: WireRateUnit::A,
            charging_schedule_period: alloc::vec![
                WirePeriod {
                    limit: 16.0,
                    number_phases: Some(3),
                    start_period: 0,
                },
                WirePeriod {
                    limit: 32.0,
                    number_phases: None,
                    start_period: 1_800,
                },
            ],
            duration: Some(3_600),
            min_charging_rate: Some(6.0),
            start_schedule: Some("2026-03-04T05:06:07Z".into()),
        }
    }

    fn wire_profile() -> CsChargingProfiles {
        CsChargingProfiles {
            charging_profile_id: 42,
            charging_profile_kind: WireKind::Absolute,
            charging_profile_purpose: WirePurpose::TxDefaultProfile,
            charging_schedule: wire_schedule(),
            recurrency_kind: None,
            stack_level: 3,
            transaction_id: None,
            valid_from: None,
            valid_to: None,
        }
    }

    #[test]
    fn a_wire_profile_maps_onto_the_internal_model_with_its_single_schedule() {
        let mapped = map_profile(&wire_profile());

        assert_eq!(mapped.id, ChargingProfileId(42));
        assert_eq!(mapped.stack_level, 3);
        assert_eq!(mapped.purpose, ChargingProfilePurpose::TxDefault);
        assert_eq!(mapped.schedules.len(), 1);
        assert_eq!(mapped.schedules[0].id, 0);
        assert_eq!(mapped.schedules[0].periods.len(), 2);
        assert_eq!(mapped.schedules[0].periods[0].limit, 16.0);
        assert_eq!(mapped.schedules[0].min_charging_rate, Some(6.0));
    }

    #[test]
    fn a_transaction_id_maps_exactly_because_1_6j_already_uses_an_integer() {
        let mut profile = wire_profile();
        profile.charging_profile_purpose = WirePurpose::TxProfile;
        profile.transaction_id = Some(7);

        let mapped = map_profile(&profile);

        assert_eq!(mapped.purpose, ChargingProfilePurpose::Tx);
        assert_eq!(mapped.transaction_id, Some(TransactionId(7)));
    }

    #[test]
    fn connector_zero_is_the_charge_point_and_every_other_connector_resolves_to_its_evse() {
        // Two EVSEs, two connectors each: 1.6J connectors 1-2 are EVSE 0, 3-4 are EVSE 1.
        let counts = [2, 2];

        assert_eq!(
            map_scope(&counts, 0),
            Some(ChargingProfileScope::ChargePoint)
        );
        assert_eq!(map_scope(&counts, 1), Some(ChargingProfileScope::Evse(0)));
        assert_eq!(map_scope(&counts, 2), Some(ChargingProfileScope::Evse(0)));
        assert_eq!(map_scope(&counts, 3), Some(ChargingProfileScope::Evse(1)));
        assert_eq!(map_scope(&counts, 4), Some(ChargingProfileScope::Evse(1)));
        assert_eq!(map_scope(&counts, 5), None);
        assert_eq!(map_scope(&counts, -1), None);
    }

    #[test]
    fn the_two_purposes_1_6j_lacks_degrade_to_their_nearest_true_statement() {
        assert_eq!(
            wire_purpose(ChargingProfilePurpose::ExternalConstraints),
            WirePurpose::ChargePointMaxProfile
        );
        assert_eq!(
            wire_purpose(ChargingProfilePurpose::PriorityCharging),
            WirePurpose::TxProfile
        );
        // The three it does have round-trip exactly.
        for purpose in [
            ChargingProfilePurpose::ChargePointMax,
            ChargingProfilePurpose::TxDefault,
            ChargingProfilePurpose::Tx,
        ] {
            assert_eq!(map_purpose(&wire_purpose(purpose)), purpose);
        }
    }

    #[test]
    fn a_composed_schedule_maps_onto_the_wire_shape() {
        let composed = CompositeSchedule {
            start: DateTime::parse_from_rfc3339("2026-03-04T05:06:07Z")
                .unwrap()
                .with_timezone(&Utc),
            duration_secs: 3_600,
            rate_unit: ChargingRateUnit::Amps,
            periods: alloc::vec![ChargingSchedulePeriod {
                start_period_secs: 0,
                limit: 16.0,
                number_phases: Some(1),
            }],
            ends_limit: true,
            min_charging_rate: Some(6.0),
        };

        let wire = wire_composite_schedule(&composed);

        assert_eq!(wire.duration, Some(3_600));
        assert_eq!(wire.min_charging_rate, Some(6.0));
        assert_eq!(wire.start_schedule, Some(composed.start.to_rfc3339()));
        assert_eq!(wire.charging_schedule_period[0].limit, 16.0);
        assert_eq!(wire.charging_schedule_period[0].number_phases, Some(1));
    }
}
