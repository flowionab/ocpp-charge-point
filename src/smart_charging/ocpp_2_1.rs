//! OCPP 2.1 wire adapters for the Smart Charging block (B2.5).
//!
//! Translates 2.1's `SetChargingProfile`/`ClearChargingProfile`/`GetCompositeSchedule` onto this
//! crate's version-independent model and back, per `CLAUDE.md`'s adapter direction. 2.1's own
//! extensions that this crate's model has no concept of yet - dynamic-schedule fields
//! (`dynUpdateInterval`, `dynUpdateTime`), price schedules, per-phase limits, DER operation modes
//! - are read past rather than half-interpreted; see B2.6 for the ones that are next.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use chrono::{DateTime, Utc};

use ocpp_client::ocpp_2_1::OCPP2_1Client;
use ocpp_client::ocpp_types::v21::common::{
    ChargingProfile as WireChargingProfile, ChargingProfileKindEnum, ChargingProfilePurposeEnum,
    ChargingProfileStatusEnum, ChargingRateUnitEnum, ChargingSchedule as WireChargingSchedule,
    ClearChargingProfileStatusEnum, CompositeSchedule as WireCompositeSchedule, GenericStatusEnum,
    GetChargingProfileStatusEnum, RecurrencyKindEnum,
};
use ocpp_client::ocpp_types::v21::{
    ClearChargingProfileRequest, ClearChargingProfileResponse, GetChargingProfilesRequest,
    GetChargingProfilesResponse, GetCompositeScheduleRequest, GetCompositeScheduleResponse,
    ReportChargingProfilesRequest, SetChargingProfileRequest, SetChargingProfileResponse,
};

use crate::actor::ChargePointActor;
use crate::clock::Clock;
use crate::smart_charging::{
    ChargingLimitProjection, ClearChargingProfileHandler, ClearChargingProfileOutcome,
    CompositeSchedule, GetChargingProfilesHandler, GetCompositeScheduleHandler,
    GetCompositeScheduleOutcome, SetChargingProfileHandler, SetChargingProfileOutcome,
    chunk_charging_profile_report, handle_clear_charging_profile, handle_get_charging_profiles,
    handle_get_composite_schedule, handle_set_charging_profile,
};
use crate::state::{
    ChargingLimitSource, ChargingProfile, ChargingProfileCriteria, ChargingProfileId,
    ChargingProfileKind, ChargingProfilePurpose, ChargingProfileQuery, ChargingProfileScope,
    ChargingRateUnit, ChargingSchedule, ChargingSchedulePeriod, InstalledChargingProfile,
    RecurrencyKind, TransactionId,
};

/// 2.1's purpose enum onto this crate's. Every 2.1 value has an internal counterpart, so nothing is
/// lost in this direction.
fn map_purpose(purpose: &ChargingProfilePurposeEnum) -> ChargingProfilePurpose {
    match purpose {
        ChargingProfilePurposeEnum::ChargingStationExternalConstraints => {
            ChargingProfilePurpose::ExternalConstraints
        }
        ChargingProfilePurposeEnum::ChargingStationMaxProfile => {
            ChargingProfilePurpose::ChargePointMax
        }
        ChargingProfilePurposeEnum::TxDefaultProfile => ChargingProfilePurpose::TxDefault,
        ChargingProfilePurposeEnum::TxProfile => ChargingProfilePurpose::Tx,
        ChargingProfilePurposeEnum::PriorityCharging => ChargingProfilePurpose::PriorityCharging,
        // 2.1 adds `LocalGeneration` (on-site generation feeding the station). Nothing in this
        // crate distinguishes where a cap came from, and it behaves like an external constraint
        // from the charge point's point of view, so it maps there rather than being refused.
        _ => ChargingProfilePurpose::ExternalConstraints,
    }
}

/// This crate's purpose enum back onto 2.1's, for `ReportChargingProfiles`.
pub(super) fn wire_purpose(purpose: ChargingProfilePurpose) -> ChargingProfilePurposeEnum {
    match purpose {
        ChargingProfilePurpose::ChargePointMax => {
            ChargingProfilePurposeEnum::ChargingStationMaxProfile
        }
        ChargingProfilePurpose::TxDefault => ChargingProfilePurposeEnum::TxDefaultProfile,
        ChargingProfilePurpose::Tx => ChargingProfilePurposeEnum::TxProfile,
        ChargingProfilePurpose::ExternalConstraints => {
            ChargingProfilePurposeEnum::ChargingStationExternalConstraints
        }
        ChargingProfilePurpose::PriorityCharging => ChargingProfilePurposeEnum::PriorityCharging,
    }
}

fn map_kind(kind: &ChargingProfileKindEnum) -> ChargingProfileKind {
    match kind {
        ChargingProfileKindEnum::Absolute => ChargingProfileKind::Absolute,
        ChargingProfileKindEnum::Recurring => ChargingProfileKind::Recurring,
        _ => ChargingProfileKind::Relative,
    }
}

fn map_recurrency(kind: &RecurrencyKindEnum) -> RecurrencyKind {
    match kind {
        RecurrencyKindEnum::Weekly => RecurrencyKind::Weekly,
        _ => RecurrencyKind::Daily,
    }
}

pub(super) fn map_rate_unit(unit: &ChargingRateUnitEnum) -> ChargingRateUnit {
    match unit {
        ChargingRateUnitEnum::W => ChargingRateUnit::Watts,
        _ => ChargingRateUnit::Amps,
    }
}

pub(super) fn wire_rate_unit(unit: ChargingRateUnit) -> ChargingRateUnitEnum {
    match unit {
        ChargingRateUnit::Amps => ChargingRateUnitEnum::A,
        ChargingRateUnit::Watts => ChargingRateUnitEnum::W,
    }
}

/// Parses a wire timestamp, treating an unparseable one as absent rather than failing the whole
/// request - the same stance [`crate::reservation`] takes for `expiryDateTime`.
fn parse_time(raw: &Option<alloc::string::String>) -> Option<DateTime<Utc>> {
    raw.as_ref()
        .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        .map(|parsed| parsed.with_timezone(&Utc))
}

/// One wire schedule onto this crate's.
///
/// A period with no `limit` at all (2.1 permits it for the DER/discharge cases this crate doesn't
/// model) is dropped: a period that says nothing about how much current may flow cannot
/// contribute to a current limit, and inventing one would be worse than leaving the neighbouring
/// periods to cover the time.
fn map_schedule(
    schedule: &ocpp_client::ocpp_types::v21::common::ChargingSchedule,
) -> ChargingSchedule {
    ChargingSchedule {
        id: schedule.id as i32,
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
                        limit: period.limit?,
                        number_phases: period.number_phases.and_then(|n| u8::try_from(n).ok()),
                    })
                })
                .collect();
            // Composition assumes periods are ordered; a CSMS is required to send them that way
            // but sorting here makes that an invariant of this crate's model rather than a
            // requirement on every peer.
            periods.sort_by_key(|period| period.start_period_secs);
            periods
        },
    }
}

/// A wire profile onto this crate's.
fn map_profile(profile: &ocpp_client::ocpp_types::v21::common::ChargingProfile) -> ChargingProfile {
    ChargingProfile {
        id: ChargingProfileId(profile.id as i32),
        stack_level: u32::try_from(profile.stack_level).unwrap_or(0),
        purpose: map_purpose(&profile.charging_profile_purpose),
        kind: map_kind(&profile.charging_profile_kind),
        recurrency: profile.recurrency_kind.as_ref().map(map_recurrency),
        valid_from: parse_time(&profile.valid_from),
        valid_to: parse_time(&profile.valid_to),
        // 2.x transaction ids are free-form strings on the wire while this crate mints `u64`s;
        // one that doesn't parse can't match any transaction here, and is treated as "applies to
        // whatever is running on the addressed connector" rather than rejecting the profile.
        transaction_id: profile
            .transaction_id
            .as_ref()
            .and_then(|id| id.parse().ok())
            .map(TransactionId),
        schedules: profile.charging_schedule.iter().map(map_schedule).collect(),
    }
}

/// 2.x addresses profile scope with an `evseId` where `0` means the whole charge point.
fn map_scope(evse_id: i64) -> Option<ChargingProfileScope> {
    match evse_id {
        0 => Some(ChargingProfileScope::ChargePoint),
        id if id > 0 => Some(ChargingProfileScope::Evse(usize::try_from(id).ok()? - 1)),
        _ => None,
    }
}

/// The inverse of [`map_scope`], for reporting a stored profile back.
pub(super) fn wire_evse_id(scope: ChargingProfileScope) -> i64 {
    match scope {
        ChargingProfileScope::ChargePoint => 0,
        ChargingProfileScope::Evse(evse_id) => evse_id as i64 + 1,
    }
}

fn map_criteria(request: &ClearChargingProfileRequest) -> ChargingProfileCriteria {
    let criteria = request.charging_profile_criteria.as_ref();
    ChargingProfileCriteria {
        id: request
            .charging_profile_id
            .map(|id| ChargingProfileId(id as i32)),
        scope: criteria
            .and_then(|criteria| criteria.evse_id)
            .and_then(map_scope),
        purpose: criteria
            .and_then(|criteria| criteria.charging_profile_purpose.as_ref())
            .map(map_purpose),
        stack_level: criteria
            .and_then(|criteria| criteria.stack_level)
            .and_then(|level| u32::try_from(level).ok()),
    }
}

/// This crate's composite schedule onto 2.1's wire shape.
pub(super) fn wire_composite_schedule(
    composed: &CompositeSchedule,
    evse_id: usize,
) -> WireCompositeSchedule {
    WireCompositeSchedule {
        charging_rate_unit: wire_rate_unit(composed.rate_unit),
        charging_schedule_period: composed
            .periods
            .iter()
            .map(
                |period| ocpp_client::ocpp_types::v21::common::ChargingSchedulePeriod {
                    custom_data: None,
                    discharge_limit: None,
                    discharge_limit_l2: None,
                    discharge_limit_l3: None,
                    evse_sleep: None,
                    limit: Some(period.limit),
                    limit_l2: None,
                    limit_l3: None,
                    number_phases: period.number_phases.map(i64::from),
                    operation_mode: None,
                    phase_to_use: None,
                    preconditioning_request: None,
                    setpoint: None,
                    setpoint_l2: None,
                    setpoint_l3: None,
                    setpoint_reactive: None,
                    setpoint_reactive_l2: None,
                    setpoint_reactive_l3: None,
                    start_period: i64::from(period.start_period_secs),
                    v2x_baseline: None,
                    v2x_freq_watt_curve: None,
                    v2x_signal_watt_curve: None,
                },
            )
            .collect(),
        custom_data: None,
        duration: i64::from(composed.duration_secs),
        evse_id: evse_id as i64 + 1,
        schedule_start: composed.start.to_rfc3339(),
    }
}

fn wire_kind(kind: ChargingProfileKind) -> ChargingProfileKindEnum {
    match kind {
        ChargingProfileKind::Absolute => ChargingProfileKindEnum::Absolute,
        ChargingProfileKind::Recurring => ChargingProfileKindEnum::Recurring,
        ChargingProfileKind::Relative => ChargingProfileKindEnum::Relative,
    }
}

fn wire_recurrency(kind: RecurrencyKind) -> RecurrencyKindEnum {
    match kind {
        RecurrencyKind::Daily => RecurrencyKindEnum::Daily,
        RecurrencyKind::Weekly => RecurrencyKindEnum::Weekly,
    }
}

/// One stored schedule back onto 2.1's wire shape.
///
/// Every 2.1 field this crate's model has no concept of goes out as `None` rather than a guess -
/// the same fields [`map_schedule`] reads past on the way in, so a profile that arrived here and
/// is reported back describes exactly what this charge point is acting on.
fn wire_schedule(schedule: &ChargingSchedule) -> WireChargingSchedule {
    WireChargingSchedule {
        absolute_price_schedule: None,
        charging_rate_unit: wire_rate_unit(schedule.rate_unit),
        charging_schedule_period: schedule
            .periods
            .iter()
            .map(
                |period| ocpp_client::ocpp_types::v21::common::ChargingSchedulePeriod {
                    custom_data: None,
                    discharge_limit: None,
                    discharge_limit_l2: None,
                    discharge_limit_l3: None,
                    evse_sleep: None,
                    limit: Some(period.limit),
                    limit_l2: None,
                    limit_l3: None,
                    number_phases: period.number_phases.map(i64::from),
                    operation_mode: None,
                    phase_to_use: None,
                    preconditioning_request: None,
                    setpoint: None,
                    setpoint_l2: None,
                    setpoint_l3: None,
                    setpoint_reactive: None,
                    setpoint_reactive_l2: None,
                    setpoint_reactive_l3: None,
                    start_period: i64::from(period.start_period_secs),
                    v2x_baseline: None,
                    v2x_freq_watt_curve: None,
                    v2x_signal_watt_curve: None,
                },
            )
            .collect(),
        custom_data: None,
        digest_value: None,
        duration: schedule.duration_secs.map(i64::from),
        id: i64::from(schedule.id),
        limit_at_so_c: None,
        min_charging_rate: schedule.min_charging_rate,
        power_tolerance: None,
        price_level_schedule: None,
        randomized_delay: None,
        sales_tariff: None,
        signature_id: None,
        start_schedule: schedule.start_schedule.map(|start| start.to_rfc3339()),
        use_local_time: None,
    }
}

/// One stored profile back onto 2.1's wire shape - the inverse of [`map_profile`].
///
/// 2.1 caps `chargingSchedule` at three entries (one per rate unit, which is all a profile can
/// meaningfully carry). A profile holding more is truncated with a warning rather than dropped
/// from the report: telling the CSMS about most of a profile beats telling it the profile does not
/// exist.
fn wire_profile(profile: &ChargingProfile) -> WireChargingProfile {
    let mut schedules = heapless::Vec::new();
    for schedule in &profile.schedules {
        if schedules.push(wire_schedule(schedule)).is_err() {
            tracing::warn!(
                profile_id = profile.id.0,
                schedules = profile.schedules.len(),
                "truncating a reported charging profile to the three schedules 2.1 can carry"
            );
            break;
        }
    }
    WireChargingProfile {
        charging_profile_kind: wire_kind(profile.kind),
        charging_profile_purpose: wire_purpose(profile.purpose),
        charging_schedule: schedules,
        custom_data: None,
        dyn_update_interval: None,
        dyn_update_time: None,
        id: i64::from(profile.id.0),
        invalid_after_offline_duration: None,
        max_offline_duration: None,
        price_schedule_signature: None,
        recurrency_kind: profile.recurrency.map(wire_recurrency),
        stack_level: i64::from(profile.stack_level),
        transaction_id: profile
            .transaction_id
            .and_then(|id| heapless::String::try_from(alloc::format!("{}", id.0).as_str()).ok()),
        valid_from: profile.valid_from.map(|valid| valid.to_rfc3339()),
        valid_to: profile.valid_to.map(|valid| valid.to_rfc3339()),
    }
}

/// 2.1 carries `chargingLimitSource` as a free-form 20-character string rather than 2.0.1's enum,
/// so this produces the Appendix's `ChargingLimitSourceEnumStringType` values by name.
fn wire_limit_source(source: ChargingLimitSource) -> heapless::String<20> {
    let name = match source {
        ChargingLimitSource::Ems => "EMS",
        ChargingLimitSource::Cso => "CSO",
        ChargingLimitSource::So => "SO",
        ChargingLimitSource::Other => "Other",
    };
    heapless::String::try_from(name).unwrap_or_default()
}

/// 2.1's `chargingLimitSource` strings back onto this crate's enum, for filtering a
/// `GetChargingProfiles`. An unrecognised value maps to [`ChargingLimitSource::Other`], which is
/// what it is from this charge point's point of view - and since nothing here is `Other`-sourced,
/// filtering on it correctly matches nothing rather than everything.
fn map_limit_source(source: &str) -> ChargingLimitSource {
    match source {
        "EMS" => ChargingLimitSource::Ems,
        "CSO" => ChargingLimitSource::Cso,
        "SO" => ChargingLimitSource::So,
        _ => ChargingLimitSource::Other,
    }
}

/// A `GetChargingProfiles` request onto this crate's query.
fn map_query(request: &GetChargingProfilesRequest) -> ChargingProfileQuery {
    let criterion = &request.charging_profile;
    ChargingProfileQuery {
        ids: criterion
            .charging_profile_id
            .as_ref()
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| i32::try_from(*id).ok())
                    .map(ChargingProfileId)
                    .collect()
            })
            .unwrap_or_default(),
        // An `evseId` the request omits means "every scope", which is a `None` scope here. One
        // this charge point cannot address (a negative id) is left as `None` too rather than
        // reported as charge-point-wide.
        scope: request.evse_id.and_then(map_scope),
        purpose: criterion.charging_profile_purpose.as_ref().map(map_purpose),
        stack_level: criterion
            .stack_level
            .and_then(|level| u32::try_from(level).ok()),
        sources: criterion
            .charging_limit_source
            .as_ref()
            .map(|sources| {
                sources
                    .iter()
                    .map(|source| map_limit_source(source))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// Sends the profiles a `GetChargingProfiles` matched as one or more `ReportChargingProfiles`.
///
/// A message that fails to send is logged and the rest still go - the same "log and continue"
/// treatment every other fire-and-forget report send in this crate gets. Nothing is sent at all
/// for an empty match; the `NoProfiles` response has already said so.
async fn send_charging_profile_report(
    client: &OCPP2_1Client,
    request_id: i64,
    profiles: &[InstalledChargingProfile],
) {
    for chunk in chunk_charging_profile_report(profiles) {
        let request = ReportChargingProfilesRequest {
            charging_limit_source: wire_limit_source(chunk.source),
            charging_profile: chunk
                .profiles
                .iter()
                .map(|installed| wire_profile(&installed.profile))
                .collect(),
            custom_data: None,
            evse_id: wire_evse_id(chunk.scope),
            request_id,
            tbc: Some(chunk.tbc),
        };
        if let Err(err) = client.send_report_charging_profiles(request).await {
            tracing::warn!(error = %err, "failed to send a ReportChargingProfiles message");
        }
    }
}

/// The generated response types carry no `Default`, and every one of these responses is "a status
/// and nothing else" - so each gets one constructor here rather than the same three `None`s
/// repeated at every return site.
fn set_response(status: ChargingProfileStatusEnum) -> SetChargingProfileResponse {
    SetChargingProfileResponse {
        custom_data: None,
        status,
        status_info: None,
    }
}

fn clear_response(status: ClearChargingProfileStatusEnum) -> ClearChargingProfileResponse {
    ClearChargingProfileResponse {
        custom_data: None,
        status,
        status_info: None,
    }
}

fn composite_response(
    status: GenericStatusEnum,
    schedule: Option<WireCompositeSchedule>,
) -> GetCompositeScheduleResponse {
    GetCompositeScheduleResponse {
        custom_data: None,
        schedule,
        status,
        status_info: None,
    }
}

fn wire_set_status(outcome: &SetChargingProfileOutcome) -> ChargingProfileStatusEnum {
    match outcome {
        SetChargingProfileOutcome::Accepted => ChargingProfileStatusEnum::Accepted,
        SetChargingProfileOutcome::Rejected(_) => ChargingProfileStatusEnum::Rejected,
    }
}

fn wire_clear_status(outcome: ClearChargingProfileOutcome) -> ClearChargingProfileStatusEnum {
    match outcome {
        ClearChargingProfileOutcome::Accepted => ClearChargingProfileStatusEnum::Accepted,
        ClearChargingProfileOutcome::Unknown => ClearChargingProfileStatusEnum::Unknown,
    }
}

/// Wraps an [`OCPP2_1Client`] with the [`Clock`] the handlers need - `SetChargingProfile` has to
/// stamp the installation instant onto a schedule the CSMS left unanchored, and
/// `GetCompositeSchedule` composes from "now". Mirrors the `with_clock` wrappers
/// `crate::availability`/`crate::transactions` already use, and is reachable on `no_std` for the
/// same reason (see G3.1).
pub struct Ocpp2_1SmartChargingHandler<C> {
    client: OCPP2_1Client,
    clock: C,
}

impl<C: Clock + Clone + Send + Sync + 'static> Ocpp2_1SmartChargingHandler<C> {
    /// Wraps `client`, sourcing every timestamp from `clock`.
    pub fn with_clock(client: OCPP2_1Client, clock: C) -> Self {
        Self { client, clock }
    }
}

#[cfg(feature = "std")]
impl Ocpp2_1SmartChargingHandler<crate::clock::SystemClock> {
    /// Wraps `client`, sourcing every timestamp from [`crate::clock::SystemClock`].
    pub fn new(client: OCPP2_1Client) -> Self {
        Self::with_clock(client, crate::clock::SystemClock)
    }
}

#[async_trait::async_trait]
impl<C: Clock + Clone + Send + Sync + 'static> SetChargingProfileHandler
    for Ocpp2_1SmartChargingHandler<C>
{
    async fn register_set_charging_profile_handler(&self, actor: ChargePointActor) {
        let clock = self.clock.clone();
        self.client
            .on_set_charging_profile(move |request: SetChargingProfileRequest, _client| {
                let actor = actor.clone();
                let clock = clock.clone();
                async move {
                    let Some(scope) = map_scope(request.evse_id) else {
                        return Ok(set_response(ChargingProfileStatusEnum::Rejected));
                    };
                    let outcome = handle_set_charging_profile(
                        &actor,
                        scope,
                        map_profile(&request.charging_profile),
                        clock.now(),
                    )
                    .await;
                    Ok(set_response(wire_set_status(&outcome)))
                }
            })
            .await;
    }
}

#[async_trait::async_trait]
impl<C: Clock + Clone + Send + Sync + 'static> ClearChargingProfileHandler
    for Ocpp2_1SmartChargingHandler<C>
{
    async fn register_clear_charging_profile_handler(&self, actor: ChargePointActor) {
        self.client
            .on_clear_charging_profile(move |request: ClearChargingProfileRequest, _client| {
                let actor = actor.clone();
                async move {
                    let outcome =
                        handle_clear_charging_profile(&actor, map_criteria(&request)).await;
                    Ok(clear_response(wire_clear_status(outcome)))
                }
            })
            .await;
    }
}

#[async_trait::async_trait]
impl<C: Clock + Clone + Send + Sync + 'static> GetChargingProfilesHandler
    for Ocpp2_1SmartChargingHandler<C>
{
    async fn register_get_charging_profiles_handler(&self, actor: ChargePointActor) {
        self.client
            .on_get_charging_profiles(move |request: GetChargingProfilesRequest, client| {
                let actor = actor.clone();
                async move {
                    let matched = handle_get_charging_profiles(&actor, &map_query(&request));
                    let status = if matched.is_empty() {
                        GetChargingProfileStatusEnum::NoProfiles
                    } else {
                        GetChargingProfileStatusEnum::Accepted
                    };
                    // The reports go out *before* this response, because the transport only
                    // writes the response once this closure returns and there is no executor at
                    // this boundary to defer them onto. OCPP would rather the response came
                    // first, but `requestId` is what correlates the two, so a CSMS matching on it
                    // is unaffected - and `crate::reporting`'s `NotifyReport` has the same
                    // ordering for the same reason. Recorded in `docs/PRODUCTION-ROADMAP.md` B2.
                    send_charging_profile_report(&client, request.request_id, &matched).await;
                    Ok(GetChargingProfilesResponse {
                        custom_data: None,
                        status,
                        status_info: None,
                    })
                }
            })
            .await;
    }
}

#[async_trait::async_trait]
impl<C: Clock + Clone + Send + Sync + 'static> GetCompositeScheduleHandler
    for Ocpp2_1SmartChargingHandler<C>
{
    async fn register_get_composite_schedule_handler(
        &self,
        actor: ChargePointActor,
        projection: Arc<ChargingLimitProjection>,
    ) {
        let clock = self.clock.clone();
        self.client
            .on_get_composite_schedule(move |request: GetCompositeScheduleRequest, _client| {
                let actor = actor.clone();
                let clock = clock.clone();
                let projection = projection.clone();
                async move {
                    // `evseId` 0 addresses the whole charge point, which a *composite schedule*
                    // cannot answer for: the composite is per-EVSE, and this crate will not
                    // silently answer for EVSE 1 as though it spoke for all of them.
                    let Some(ChargingProfileScope::Evse(evse_id)) = map_scope(request.evse_id)
                    else {
                        return Ok(composite_response(GenericStatusEnum::Rejected, None));
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
                        GetCompositeScheduleOutcome::Accepted(composed) => composite_response(
                            GenericStatusEnum::Accepted,
                            composed
                                .as_ref()
                                .map(|composed| wire_composite_schedule(composed, evse_id)),
                        ),
                        GetCompositeScheduleOutcome::Rejected => {
                            composite_response(GenericStatusEnum::Rejected, None)
                        }
                    })
                }
            })
            .await;
    }
}

/// The `std` convenience: a bare [`OCPP2_1Client`] handles these messages directly, sourcing its
/// timestamps from [`crate::clock::SystemClock`], so existing callers that pass a client need no
/// source change - the same shape `crate::availability`/`crate::transactions` use.
#[cfg(feature = "std")]
mod std_impls {
    use super::*;

    #[async_trait::async_trait]
    impl SetChargingProfileHandler for OCPP2_1Client {
        async fn register_set_charging_profile_handler(&self, actor: ChargePointActor) {
            Ocpp2_1SmartChargingHandler::new(self.clone())
                .register_set_charging_profile_handler(actor)
                .await;
        }
    }

    #[async_trait::async_trait]
    impl ClearChargingProfileHandler for OCPP2_1Client {
        async fn register_clear_charging_profile_handler(&self, actor: ChargePointActor) {
            Ocpp2_1SmartChargingHandler::new(self.clone())
                .register_clear_charging_profile_handler(actor)
                .await;
        }
    }

    #[async_trait::async_trait]
    impl GetChargingProfilesHandler for OCPP2_1Client {
        async fn register_get_charging_profiles_handler(&self, actor: ChargePointActor) {
            Ocpp2_1SmartChargingHandler::new(self.clone())
                .register_get_charging_profiles_handler(actor)
                .await;
        }
    }

    #[async_trait::async_trait]
    impl GetCompositeScheduleHandler for OCPP2_1Client {
        async fn register_get_composite_schedule_handler(
            &self,
            actor: ChargePointActor,
            projection: Arc<ChargingLimitProjection>,
        ) {
            Ocpp2_1SmartChargingHandler::new(self.clone())
                .register_get_composite_schedule_handler(actor, projection)
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocpp_client::ocpp_types::v21::common::{
        ChargingProfile as WireChargingProfile, ChargingSchedule as WireChargingSchedule,
        ChargingSchedulePeriod as WirePeriod,
    };

    /// The generated wire types carry no `Default`, so each fixture names every field. Verbose,
    /// but it is also the only place in this crate that states, in one view, exactly which of
    /// 2.1's many optional period fields this adapter reads and which it deliberately ignores.
    fn wire_period(start_period: i64, limit: f64, number_phases: Option<i64>) -> WirePeriod {
        WirePeriod {
            custom_data: None,
            discharge_limit: None,
            discharge_limit_l2: None,
            discharge_limit_l3: None,
            evse_sleep: None,
            limit: Some(limit),
            limit_l2: None,
            limit_l3: None,
            number_phases,
            operation_mode: None,
            phase_to_use: None,
            preconditioning_request: None,
            setpoint: None,
            setpoint_reactive: None,
            setpoint_reactive_l2: None,
            setpoint_reactive_l3: None,
            setpoint_l2: None,
            setpoint_l3: None,
            start_period,
            v2x_baseline: None,
            v2x_freq_watt_curve: None,
            v2x_signal_watt_curve: None,
        }
    }

    fn wire_schedule_fixture() -> WireChargingSchedule {
        WireChargingSchedule {
            absolute_price_schedule: None,
            charging_rate_unit: ChargingRateUnitEnum::A,
            charging_schedule_period: alloc::vec![
                wire_period(0, 16.0, Some(3)),
                wire_period(1_800, 32.0, None),
            ],
            custom_data: None,
            digest_value: None,
            duration: Some(3_600),
            id: 7,
            limit_at_so_c: None,
            min_charging_rate: Some(6.0),
            power_tolerance: None,
            price_level_schedule: None,
            randomized_delay: None,
            sales_tariff: None,
            signature_id: None,
            start_schedule: Some("2026-03-04T05:06:07Z".into()),
            use_local_time: None,
        }
    }

    fn wire_profile_fixture() -> WireChargingProfile {
        WireChargingProfile {
            charging_profile_kind: ChargingProfileKindEnum::Absolute,
            charging_profile_purpose: ChargingProfilePurposeEnum::TxDefaultProfile,
            charging_schedule: {
                let mut schedules = heapless::Vec::new();
                schedules.push(wire_schedule_fixture()).ok();
                schedules
            },
            custom_data: None,
            dyn_update_interval: None,
            dyn_update_time: None,
            id: 42,
            invalid_after_offline_duration: None,
            max_offline_duration: None,
            price_schedule_signature: None,
            recurrency_kind: None,
            stack_level: 3,
            transaction_id: None,
            valid_from: None,
            valid_to: None,
        }
    }

    #[test]
    fn a_wire_profile_maps_onto_the_internal_model_field_for_field() {
        let mapped = map_profile(&wire_profile_fixture());

        assert_eq!(mapped.id, ChargingProfileId(42));
        assert_eq!(mapped.stack_level, 3);
        assert_eq!(mapped.purpose, ChargingProfilePurpose::TxDefault);
        assert_eq!(mapped.kind, ChargingProfileKind::Absolute);
        assert_eq!(mapped.schedules.len(), 1);

        let schedule = &mapped.schedules[0];
        assert_eq!(schedule.id, 7);
        assert_eq!(schedule.duration_secs, Some(3_600));
        assert_eq!(schedule.rate_unit, ChargingRateUnit::Amps);
        assert_eq!(schedule.min_charging_rate, Some(6.0));
        assert_eq!(
            schedule.start_schedule,
            Some(
                DateTime::parse_from_rfc3339("2026-03-04T05:06:07Z")
                    .unwrap()
                    .with_timezone(&Utc)
            )
        );
        assert_eq!(schedule.periods.len(), 2);
        assert_eq!(schedule.periods[0].limit, 16.0);
        assert_eq!(schedule.periods[0].number_phases, Some(3));
        assert_eq!(schedule.periods[1].start_period_secs, 1_800);
    }

    #[test]
    fn every_purpose_round_trips_through_the_wire_enum() {
        for purpose in [
            ChargingProfilePurpose::ChargePointMax,
            ChargingProfilePurpose::TxDefault,
            ChargingProfilePurpose::Tx,
            ChargingProfilePurpose::ExternalConstraints,
            ChargingProfilePurpose::PriorityCharging,
        ] {
            assert_eq!(map_purpose(&wire_purpose(purpose)), purpose);
        }
    }

    #[test]
    fn a_period_with_no_limit_at_all_is_dropped_rather_than_invented() {
        let mut schedule = wire_schedule_fixture();
        schedule.charging_schedule_period[1].limit = None;

        let mapped = map_schedule(&schedule);

        assert_eq!(mapped.periods.len(), 1);
        assert_eq!(mapped.periods[0].limit, 16.0);
    }

    #[test]
    fn periods_are_sorted_so_composition_can_rely_on_their_order() {
        let mut schedule = wire_schedule_fixture();
        schedule.charging_schedule_period.reverse();

        let mapped = map_schedule(&schedule);

        assert_eq!(mapped.periods[0].start_period_secs, 0);
        assert_eq!(mapped.periods[1].start_period_secs, 1_800);
    }

    #[test]
    fn evse_zero_is_the_charge_point_scope_and_evse_ids_are_one_based_on_the_wire() {
        assert_eq!(map_scope(0), Some(ChargingProfileScope::ChargePoint));
        assert_eq!(map_scope(1), Some(ChargingProfileScope::Evse(0)));
        assert_eq!(map_scope(3), Some(ChargingProfileScope::Evse(2)));
        assert_eq!(map_scope(-1), None);

        assert_eq!(wire_evse_id(ChargingProfileScope::ChargePoint), 0);
        assert_eq!(wire_evse_id(ChargingProfileScope::Evse(0)), 1);
    }

    #[test]
    fn clear_criteria_map_every_field_the_wire_can_carry() {
        let request = ClearChargingProfileRequest {
            charging_profile_id: Some(9),
            charging_profile_criteria: Some(
                ocpp_client::ocpp_types::v21::common::ClearChargingProfile {
                    charging_profile_purpose: Some(ChargingProfilePurposeEnum::TxProfile),
                    evse_id: Some(2),
                    stack_level: Some(4),
                    custom_data: None,
                },
            ),
            custom_data: None,
        };

        let criteria = map_criteria(&request);

        assert_eq!(criteria.id, Some(ChargingProfileId(9)));
        assert_eq!(criteria.purpose, Some(ChargingProfilePurpose::Tx));
        assert_eq!(criteria.scope, Some(ChargingProfileScope::Evse(1)));
        assert_eq!(criteria.stack_level, Some(4));
    }

    #[test]
    fn an_empty_clear_request_clears_everything_rather_than_nothing() {
        let request = ClearChargingProfileRequest {
            charging_profile_criteria: None,
            charging_profile_id: None,
            custom_data: None,
        };

        assert_eq!(map_criteria(&request), ChargingProfileCriteria::default());
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
            min_charging_rate: None,
        };

        let wire = wire_composite_schedule(&composed, 0);

        assert_eq!(wire.evse_id, 1);
        assert_eq!(wire.duration, 3_600);
        assert_eq!(wire.schedule_start, composed.start.to_rfc3339());
        assert_eq!(wire.charging_schedule_period[0].limit, Some(16.0));
        assert_eq!(wire.charging_schedule_period[0].number_phases, Some(1));
    }

    #[test]
    fn a_stored_profile_round_trips_back_onto_the_wire_it_arrived_on() {
        // The strongest statement this adapter can make about reporting: whatever the CSMS set,
        // it gets back. Anything the model drops on the way in (a period with no limit) is
        // already covered by its own test; this asserts nothing is lost on the way out.
        let mut original = wire_profile_fixture();
        // The fixture leaves the optional fields empty; a round-trip test that did the same
        // would pass just as happily against a mapping that dropped every one of them.
        original.recurrency_kind = Some(RecurrencyKindEnum::Daily);
        original.valid_from = Some("2024-01-01T00:00:00+00:00".into());
        original.valid_to = Some("2024-02-01T00:00:00+00:00".into());
        let stored = map_profile(&original);

        let reported = wire_profile(&stored);

        assert_eq!(reported.id, original.id);
        assert_eq!(reported.stack_level, original.stack_level);
        assert_eq!(reported.recurrency_kind, original.recurrency_kind);
        assert_eq!(reported.valid_from, original.valid_from);
        assert_eq!(reported.valid_to, original.valid_to);
        assert_eq!(
            reported.charging_profile_purpose,
            original.charging_profile_purpose
        );
        assert_eq!(
            reported.charging_profile_kind,
            original.charging_profile_kind
        );
        assert_eq!(reported.valid_from, original.valid_from);
        assert_eq!(reported.valid_to, original.valid_to);
        assert_eq!(reported.charging_schedule.len(), 1);
        let (reported_schedule, original_schedule) = (
            &reported.charging_schedule[0],
            &original.charging_schedule[0],
        );
        assert_eq!(reported_schedule.id, original_schedule.id);
        assert_eq!(
            reported_schedule.charging_rate_unit,
            original_schedule.charging_rate_unit
        );
        assert_eq!(reported_schedule.duration, original_schedule.duration);
        assert_eq!(
            reported_schedule.charging_schedule_period.len(),
            original_schedule.charging_schedule_period.len()
        );
        assert_eq!(
            reported_schedule.charging_schedule_period[0].limit,
            original_schedule.charging_schedule_period[0].limit
        );
    }

    #[test]
    fn a_profile_with_more_schedules_than_the_wire_holds_is_truncated_not_dropped() {
        let mut stored = map_profile(&wire_profile_fixture());
        stored.schedules = alloc::vec![stored.schedules[0].clone(); 5];

        let reported = wire_profile(&stored);

        // Three is 2.1's cap. Reporting most of a profile beats reporting none of it.
        assert_eq!(reported.charging_schedule.len(), 3);
    }

    #[test]
    fn a_get_request_maps_every_criterion_the_wire_can_carry() {
        let query = map_query(&GetChargingProfilesRequest {
            charging_profile: ocpp_client::ocpp_types::v21::common::ChargingProfileCriterion {
                charging_limit_source: Some(
                    [heapless::String::try_from("EMS").unwrap()]
                        .into_iter()
                        .collect(),
                ),
                charging_profile_id: Some(alloc::vec![7, 9]),
                charging_profile_purpose: Some(ChargingProfilePurposeEnum::TxDefaultProfile),
                custom_data: None,
                stack_level: Some(2),
            },
            custom_data: None,
            evse_id: Some(1),
            request_id: 42,
        });

        assert_eq!(
            query.ids,
            alloc::vec![ChargingProfileId(7), ChargingProfileId(9)]
        );
        assert_eq!(query.scope, Some(ChargingProfileScope::Evse(0)));
        assert_eq!(query.purpose, Some(ChargingProfilePurpose::TxDefault));
        assert_eq!(query.stack_level, Some(2));
        assert_eq!(query.sources, alloc::vec![ChargingLimitSource::Ems]);
    }

    #[test]
    fn an_omitted_evse_id_asks_about_every_scope_rather_than_the_charge_point() {
        let query = map_query(&GetChargingProfilesRequest {
            charging_profile: ocpp_client::ocpp_types::v21::common::ChargingProfileCriterion {
                charging_limit_source: None,
                charging_profile_id: None,
                charging_profile_purpose: None,
                custom_data: None,
                stack_level: None,
            },
            custom_data: None,
            evse_id: None,
            request_id: 1,
        });

        // `evseId: 0` would mean the charge point itself; absent means all of them, and
        // conflating the two would hide every EVSE-scoped profile from the CSMS.
        assert_eq!(query.scope, None);
        assert!(query.ids.is_empty());
        assert!(query.sources.is_empty());
    }

    #[test]
    fn limit_sources_round_trip_through_the_wire_strings() {
        for source in [
            ChargingLimitSource::Ems,
            ChargingLimitSource::Cso,
            ChargingLimitSource::So,
            ChargingLimitSource::Other,
        ] {
            assert_eq!(map_limit_source(&wire_limit_source(source)), source);
        }
        // An unknown source is `Other` - so a CSMS filtering on it matches nothing here rather
        // than everything.
        assert_eq!(map_limit_source("Martian"), ChargingLimitSource::Other);
    }
}
