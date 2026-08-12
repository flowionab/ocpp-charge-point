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
    ChargingLimitProjection, CompositeSchedule, compose, composing_profiles,
    connector_composition_context, external_charging_limits,
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
    /// because the store is full, or because the profile itself is unusable. Every version
    /// collapses refusal to a single `Rejected` status; what varies is whether OCPP names a
    /// reason code to go with it. See [`SetChargingProfileRejection`].
    Rejected(SetChargingProfileRejection),
}

/// Why a `SetChargingProfile` was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetChargingProfileRejection {
    /// A human explanation, for this charge point's own logs.
    pub explanation: &'static str,
    /// OCPP's `statusInfo.reasonCode`, where the spec names one for this refusal - 2.1's K28
    /// does, for the two dynamic-profile shape rules (`"InvalidSchedule"` for K28.FR.03,
    /// `"InvalidProfile"` for K28.FR.04).
    ///
    /// `None` for the refusals OCPP leaves unnamed (an unknown EVSE, a full store). Inventing a
    /// code for those would put a string a CSMS may branch on into a field the spec does not
    /// define, which is worse than the silence the schema allows.
    pub reason_code: Option<&'static str>,
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
#[tracing::instrument(skip_all)]
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
        return SetChargingProfileOutcome::Rejected(SetChargingProfileRejection {
            explanation: "smart charging is not available",
            reason_code: None,
        });
    }
    if let ChargingProfileScope::Evse(evse_id) = scope
        && evse_id >= state.evses.len()
    {
        return SetChargingProfileOutcome::Rejected(SetChargingProfileRejection {
            explanation: "no such EVSE",
            reason_code: None,
        });
    }
    for schedule in &mut profile.schedules {
        schedule.start_schedule.get_or_insert(installed_at);
    }
    if profile.kind == crate::state::ChargingProfileKind::Dynamic {
        // K28.FR.05. A dynamic profile's period is active on receipt, so "now" is both its anchor
        // and the start of the deadline its `duration` sets - and this adapter is the only place
        // that knows when the profile arrived.
        profile.dyn_update_time.get_or_insert(installed_at);
    }

    let mut trial = state.charging_profiles.clone();
    if let Err(rejection) = trial.install(scope, profile.clone()) {
        tracing::warn!(?rejection, "refusing a charging profile");
        return SetChargingProfileOutcome::Rejected(rejection_reason(&rejection));
    }

    let _ = actor
        .send(ChargePointEvent::ChargingProfileSet {
            scope,
            profile: Box::new(profile),
        })
        .await;
    SetChargingProfileOutcome::Accepted
}

/// The store's reason for refusing a profile, as the CSMS should hear it.
///
/// 2.1's K28 is the only place OCPP names a reason code for `SetChargingProfile`, and it names
/// exactly two - so those two are mapped and everything else refuses without one.
fn rejection_reason(
    rejection: &crate::state::ChargingProfileRejection,
) -> SetChargingProfileRejection {
    use crate::state::ChargingProfileRejection as Rejection;
    match rejection {
        // K28.FR.03.
        Rejection::InvalidDynamicSchedule(_) => SetChargingProfileRejection {
            explanation: "a Dynamic profile must carry exactly one single-period schedule",
            reason_code: Some("InvalidSchedule"),
        },
        // K28.FR.04.
        Rejection::DynUpdateIntervalOnNonDynamicProfile => SetChargingProfileRejection {
            explanation: "dynUpdateInterval only applies to a Dynamic profile",
            reason_code: Some("InvalidProfile"),
        },
        Rejection::TooManyProfiles => SetChargingProfileRejection {
            explanation: "the charge point cannot hold another profile",
            reason_code: None,
        },
        Rejection::NoSchedule => SetChargingProfileRejection {
            explanation: "a profile with no schedule could never produce a limit",
            reason_code: None,
        },
        Rejection::ScopeNotAllowedForPurpose(_) => SetChargingProfileRejection {
            explanation: "this profile's purpose cannot be installed at that scope",
            reason_code: None,
        },
    }
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
#[tracing::instrument(skip_all)]
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
#[tracing::instrument(skip_all)]
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
    let external = external_charging_limits(&state, evse_id);
    let profiles = composing_profiles(&state, evse_id, &external);
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
    /// The transaction exists, but no [`crate::state::ChargingProfilePurpose::PriorityCharging`] profile is
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
#[tracing::instrument(skip_all)]
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

/// A dynamic limit update, as it arrives from either direction - `UpdateDynamicSchedule` pushed by
/// the CSMS, or the `scheduleUpdate` of a `PullDynamicScheduleUpdate` response
/// (`docs/PRODUCTION-ROADMAP.md` B2.6).
///
/// **Only `limit` survives the translation from the wire, and that is a decision, not an
/// omission.** 2.1's `ChargingScheduleUpdate` also carries setpoints, discharge limits, reactive
/// setpoints and per-phase (`_L2`/`_L3`) variants of all of them. Every one of those needs a
/// hardware capability [`crate::hardware`] cannot express: `Connector::set_current_limit` takes a
/// single import limit, and there is no hook for discharging, for a setpoint, or for driving
/// phases asymmetrically. Carrying them in this type would mean storing values nothing can act on
/// and reporting a compliance this charge point does not have. They arrive, they are logged, and
/// they are dropped - the same stance the 2.1 adapter takes for DER period fields, and the gap
/// closes when B8.2's bidirectional-power hardware surface lands.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct DynamicScheduleUpdate {
    /// The new charge limit for the profile's single period, in that schedule's rate unit.
    pub limit: Option<f64>,
    /// Whether the update carried values this crate cannot project onto hardware (see the type's
    /// docs). Recorded so the caller can log precisely what was ignored rather than reporting a
    /// silent success.
    pub carried_unprojectable_values: bool,
}

/// The outcome of a dynamic limit update, matching the `ChargingProfileStatusEnum` both
/// `UpdateDynamicSchedule` and `PullDynamicScheduleUpdate` answer with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateDynamicScheduleOutcome {
    /// The update was applied to the named profile.
    Accepted,
    /// No such profile is installed, or the one installed is not
    /// [`ChargingProfileKind::Dynamic`](crate::state::ChargingProfileKind::Dynamic) - OCPP
    /// K28.FR.11, reported with `statusInfo.reasonCode = "InvalidProfile"`.
    Rejected,
}

/// Applies a dynamic limit update to the profile with `profile_id` (OCPP K28.FR.06/K28.FR.08).
///
/// Shared by both directions on purpose: a limit the CSMS pushed and a limit the charge point
/// pulled are the same change to the same profile, and OCPP requires the same immediate
/// application of both. Only the message that carried it differs.
#[tracing::instrument(skip_all)]
pub async fn handle_update_dynamic_schedule(
    actor: &ChargePointActor,
    profile_id: crate::state::ChargingProfileId,
    update: DynamicScheduleUpdate,
    updated_at: DateTime<Utc>,
) -> UpdateDynamicScheduleOutcome {
    let state = actor.state();
    if !crate::refusal::capability_present(&state.capabilities, "UpdateDynamicSchedule") {
        return UpdateDynamicScheduleOutcome::Rejected;
    }
    let addressable = state.charging_profiles.installed().iter().any(|installed| {
        installed.profile.id == profile_id
            && installed.profile.kind == crate::state::ChargingProfileKind::Dynamic
    });
    if !addressable {
        return UpdateDynamicScheduleOutcome::Rejected;
    }
    if update.carried_unprojectable_values {
        tracing::info!(
            profile_id = profile_id.0,
            "a dynamic schedule update carried setpoints, discharge limits or per-phase values \
             this charge point has no hardware hook for; only the charge limit was applied"
        );
    }

    let _ = actor
        .send(ChargePointEvent::DynamicScheduleUpdated {
            profile_id,
            limit: update.limit,
            updated_at,
        })
        .await;
    UpdateDynamicScheduleOutcome::Accepted
}

/// Asks the CSMS for a fresh limit for one dynamic charging profile - OCPP 2.1's
/// `PullDynamicScheduleUpdate`.
///
/// 2.1 only, and the *outbound* half of the same mechanism
/// [`handle_update_dynamic_schedule`] serves inbound.
#[async_trait::async_trait]
pub trait DynamicSchedulePuller {
    /// What went wrong asking.
    type Error: core::fmt::Display;

    /// Requests an update for `profile_id`, returning what the CSMS answered with. `Ok(None)`
    /// means the CSMS refused (its own `Rejected`, K28.FR.12) or sent no `scheduleUpdate` -
    /// either way there is nothing to apply, which is different from the request having failed.
    async fn pull_dynamic_schedule_update(
        &self,
        profile_id: crate::state::ChargingProfileId,
    ) -> Result<Option<DynamicScheduleUpdate>, Self::Error>;
}

#[async_trait::async_trait]
impl<T: DynamicSchedulePuller + Send + Sync + ?Sized> DynamicSchedulePuller
    for alloc::sync::Arc<T>
{
    type Error = T::Error;

    async fn pull_dynamic_schedule_update(
        &self,
        profile_id: crate::state::ChargingProfileId,
    ) -> Result<Option<DynamicScheduleUpdate>, Self::Error> {
        (**self).pull_dynamic_schedule_update(profile_id).await
    }
}

/// Pulls a fresh limit for every dynamic profile that is due one, every `interval_secs` (OCPP
/// K28.FR.10).
///
/// **Skipped entirely while the clock is unsynchronized** ([`crate::clock::is_synchronized`]), the
/// same stance [`crate::reservation::run_reservation_expiry`] takes and for the same reason: due-ness
/// is `dyn_update_time + dyn_update_interval` against now, and a charge point that does not know
/// the time would either pull continuously or never. Continuously is the worse failure - it is a
/// request storm at the CSMS - and neither is worth guessing at.
///
/// The sweep interval is how often *due-ness is checked*, not how often a pull happens: each
/// profile carries its own `dynUpdateInterval`, and the store only reports the ones that have
/// reached it.
pub async fn run_dynamic_schedule_pulls<P, C, B>(
    actor: &ChargePointActor,
    puller: &P,
    clock: &C,
    backoff: &B,
    interval_secs: u32,
) where
    P: DynamicSchedulePuller,
    C: Clock,
    B: crate::provisioning::Backoff,
{
    let interval_secs = interval_secs.max(1);
    loop {
        backoff.wait(interval_secs).await;
        let now = clock.now();
        if !crate::clock::is_synchronized(&now) {
            continue;
        }
        let due = actor.state().charging_profiles.dynamic_pulls_due(now);
        for profile_id in due {
            match puller.pull_dynamic_schedule_update(profile_id).await {
                Ok(Some(update)) => {
                    // K28.FR.08/K28.FR.09: a pulled update applies exactly like a pushed one, and
                    // is stamped with the instant it was *applied* rather than the instant the
                    // sweep started - a slow round trip must not shorten the next interval.
                    handle_update_dynamic_schedule(actor, profile_id, update, clock.now()).await;
                }
                Ok(None) => {
                    // The CSMS refused or sent nothing. Deliberately *not* stamping
                    // `dyn_update_time`: this profile's K28.FR.13 deadline must keep running, or a
                    // CSMS that answers "Rejected" forever would keep a stale limit alive by
                    // replying at all.
                    tracing::debug!(
                        profile_id = profile_id.0,
                        "the CSMS had no dynamic schedule update to give"
                    );
                }
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        profile_id = profile_id.0,
                        "failed to pull a dynamic schedule update"
                    );
                }
            }
        }
    }
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

/// Registers this charge point's inbound `UpdateDynamicSchedule` handling with the CSMS
/// connection.
///
/// 2.1 only - dynamic charging profiles are OCPP K28, which neither 1.6J nor 2.0.1 has.
#[async_trait::async_trait]
pub trait UpdateDynamicScheduleHandler {
    /// Registers an `UpdateDynamicSchedule` handler dispatching against `actor`.
    async fn register_update_dynamic_schedule_handler(&self, actor: ChargePointActor);
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
            dyn_update_interval_secs: None,
            dyn_update_time: None,
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

    /// The composite the CSMS is shown has to be the one the charge point will apply, so an
    /// external limit belongs in it too (CV13). Answering `GetCompositeSchedule` from the installed
    /// profiles alone would tell the CSMS the station is about to draw 16 A while it is in fact
    /// held to 6 A by the site's energy manager.
    #[tokio::test]
    async fn a_composite_schedule_includes_the_external_limit_in_force() {
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
        let _ = actor
            .send(ChargePointEvent::ExternalChargingLimitSet {
                evse_id: Some(0),
                limit: crate::state::ExternalChargingLimit {
                    is_local_generation: false,
                    source: ChargingLimitSource::Ems,
                    is_grid_critical: Some(true),
                    schedule: Some(ChargingSchedule {
                        id: 1,
                        start_schedule: None,
                        duration_secs: None,
                        rate_unit: ChargingRateUnit::Amps,
                        min_charging_rate: None,
                        periods: alloc::vec![ChargingSchedulePeriod {
                            start_period_secs: 0,
                            limit: 6.0,
                            number_phases: None,
                        }],
                    }),
                },
            })
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
        assert_eq!(composed.periods[0].limit, 6.0);
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

    fn dynamic_profile(id: i32) -> ChargingProfile {
        ChargingProfile {
            kind: crate::state::ChargingProfileKind::Dynamic,
            ..profile(id)
        }
    }

    fn limit_update(limit: f64) -> DynamicScheduleUpdate {
        DynamicScheduleUpdate {
            limit: Some(limit),
            carried_unprojectable_values: false,
        }
    }

    #[tokio::test]
    async fn installing_a_dynamic_profile_stamps_the_moment_it_arrived_as_its_anchor() {
        let actor = actor_with_smart_charging().await;

        handle_set_charging_profile(
            &actor,
            ChargingProfileScope::Evse(0),
            dynamic_profile(1),
            now(),
        )
        .await;

        // K28.FR.05. Without it the profile has no anchor to be active from and no start for the
        // deadline its `duration` sets.
        assert_eq!(
            actor.state().charging_profiles.installed()[0]
                .profile
                .dyn_update_time,
            Some(now())
        );
    }

    #[tokio::test]
    async fn a_dynamic_profile_that_is_not_a_single_immediate_period_is_refused_with_a_reason_code()
    {
        let actor = actor_with_smart_charging().await;
        let mut two_periods = dynamic_profile(1);
        two_periods.schedules[0]
            .periods
            .push(ChargingSchedulePeriod {
                start_period_secs: 600,
                limit: 32.0,
                number_phases: None,
            });

        let outcome =
            handle_set_charging_profile(&actor, ChargingProfileScope::Evse(0), two_periods, now())
                .await;

        // K28.FR.03: the CSMS is told which rule it broke, not just that something was wrong.
        assert_eq!(
            outcome,
            SetChargingProfileOutcome::Rejected(SetChargingProfileRejection {
                explanation: "a Dynamic profile must carry exactly one single-period schedule",
                reason_code: Some("InvalidSchedule"),
            })
        );
        assert!(actor.state().charging_profiles.is_empty());
    }

    #[tokio::test]
    async fn a_scheduled_profile_carrying_a_dyn_update_interval_is_refused_with_a_reason_code() {
        let actor = actor_with_smart_charging().await;
        let mut scheduled = profile(1);
        scheduled.dyn_update_interval_secs = Some(60);

        let outcome =
            handle_set_charging_profile(&actor, ChargingProfileScope::Evse(0), scheduled, now())
                .await;

        // K28.FR.04.
        assert_eq!(
            outcome,
            SetChargingProfileOutcome::Rejected(SetChargingProfileRejection {
                explanation: "dynUpdateInterval only applies to a Dynamic profile",
                reason_code: Some("InvalidProfile"),
            })
        );
    }

    #[tokio::test]
    async fn a_pushed_dynamic_update_replaces_the_profiles_limit() {
        let actor = actor_with_smart_charging().await;
        handle_set_charging_profile(
            &actor,
            ChargingProfileScope::Evse(0),
            dynamic_profile(1),
            now(),
        )
        .await;

        let later = now() + chrono::Duration::seconds(300);
        let outcome =
            handle_update_dynamic_schedule(&actor, ChargingProfileId(1), limit_update(24.0), later)
                .await;

        assert_eq!(outcome, UpdateDynamicScheduleOutcome::Accepted);
        let state = actor.state();
        let installed = &state.charging_profiles.installed()[0].profile;
        assert_eq!(installed.schedules[0].periods[0].limit, 24.0);
        assert_eq!(installed.dyn_update_time, Some(later));
    }

    #[tokio::test]
    async fn a_dynamic_update_naming_a_scheduled_profile_is_refused_before_anything_is_dispatched()
    {
        let actor = actor_with_smart_charging().await;
        handle_set_charging_profile(&actor, ChargingProfileScope::Evse(0), profile(1), now()).await;

        let outcome =
            handle_update_dynamic_schedule(&actor, ChargingProfileId(1), limit_update(24.0), now())
                .await;

        assert_eq!(outcome, UpdateDynamicScheduleOutcome::Rejected);
        // The CSMS's curve is untouched - K28.FR.11 refuses rather than rewriting it.
        assert_eq!(
            actor.state().charging_profiles.installed()[0]
                .profile
                .schedules[0]
                .periods[0]
                .limit,
            16.0
        );
    }

    #[tokio::test]
    async fn a_dynamic_update_for_an_uninstalled_profile_is_refused() {
        let actor = actor_with_smart_charging().await;

        let outcome = handle_update_dynamic_schedule(
            &actor,
            ChargingProfileId(99),
            limit_update(24.0),
            now(),
        )
        .await;

        assert_eq!(outcome, UpdateDynamicScheduleOutcome::Rejected);
    }

    /// A backoff that does not actually wait, so a sweep loop can be driven at test speed.
    struct InstantBackoff;

    #[async_trait::async_trait]
    impl crate::provisioning::Backoff for InstantBackoff {
        async fn wait(&self, _seconds: u32) {
            tokio::task::yield_now().await;
        }
    }

    /// A clock frozen at one instant, like the one `crate::transactions`' tests use.
    #[derive(Clone, Copy)]
    struct FixedClock(DateTime<Utc>);

    impl Clock for FixedClock {
        fn now(&self) -> DateTime<Utc> {
            self.0
        }
    }

    /// A puller that records what it was asked for and answers from a script.
    struct ScriptedPuller {
        answers: std::sync::Mutex<Vec<Option<DynamicScheduleUpdate>>>,
        asked: std::sync::Mutex<Vec<ChargingProfileId>>,
    }

    impl ScriptedPuller {
        fn new(answers: Vec<Option<DynamicScheduleUpdate>>) -> Self {
            Self {
                answers: std::sync::Mutex::new(answers),
                asked: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl DynamicSchedulePuller for ScriptedPuller {
        type Error = core::convert::Infallible;

        async fn pull_dynamic_schedule_update(
            &self,
            profile_id: ChargingProfileId,
        ) -> Result<Option<DynamicScheduleUpdate>, Self::Error> {
            self.asked.lock().unwrap().push(profile_id);
            let mut answers = self.answers.lock().unwrap();
            Ok(if answers.is_empty() {
                None
            } else {
                answers.remove(0)
            })
        }
    }

    #[tokio::test]
    async fn a_due_dynamic_profile_is_pulled_and_the_answer_applied() {
        let actor = alloc::sync::Arc::new(actor_with_smart_charging().await);
        let mut pullable = dynamic_profile(1);
        pullable.dyn_update_interval_secs = Some(60);
        handle_set_charging_profile(&actor, ChargingProfileScope::Evse(0), pullable, now()).await;

        let puller =
            alloc::sync::Arc::new(ScriptedPuller::new(alloc::vec![Some(limit_update(20.0))]));
        // A clock well past the profile's interval, so the first sweep finds it due.
        let clock = FixedClock(now() + chrono::Duration::seconds(600));
        let task_actor = actor.clone();
        let task_puller = puller.clone();
        let handle = tokio::spawn(async move {
            run_dynamic_schedule_pulls(&task_actor, &task_puller, &clock, &InstantBackoff, 1).await;
        });

        for _ in 0..400 {
            if actor.state().charging_profiles.installed()[0]
                .profile
                .schedules[0]
                .periods[0]
                .limit
                == 20.0
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        handle.abort();

        assert_eq!(
            puller.asked.lock().unwrap().as_slice(),
            &[ChargingProfileId(1)]
        );
        assert_eq!(
            actor.state().charging_profiles.installed()[0]
                .profile
                .schedules[0]
                .periods[0]
                .limit,
            20.0
        );
    }

    #[tokio::test]
    async fn nothing_is_pulled_while_the_clock_is_unsynchronized() {
        let actor = alloc::sync::Arc::new(actor_with_smart_charging().await);
        let mut pullable = dynamic_profile(1);
        pullable.dyn_update_interval_secs = Some(60);
        handle_set_charging_profile(&actor, ChargingProfileScope::Evse(0), pullable, now()).await;

        let puller = alloc::sync::Arc::new(ScriptedPuller::new(Vec::new()));
        // An unset RTC. Due-ness is a time comparison, so asking here would either storm the CSMS
        // or never fire - neither is worth guessing at.
        let clock = FixedClock(DateTime::from_timestamp(0, 0).unwrap());
        let task_actor = actor.clone();
        let task_puller = puller.clone();
        let handle = tokio::spawn(async move {
            run_dynamic_schedule_pulls(&task_actor, &task_puller, &clock, &InstantBackoff, 1).await;
        });
        for _ in 0..200 {
            tokio::task::yield_now().await;
        }
        handle.abort();

        assert!(puller.asked.lock().unwrap().is_empty());
    }
}
