//! Outbound smart-charging notifications this crate forwards to the CSMS but never decides to
//! send on its own initiative - `NotifyChargingLimit`/`ClearedChargingLimit` (an external charging
//! limit, typically from a local energy-management system) and `NotifyEVChargingNeeds`/
//! `NotifyEVChargingSchedule` (what an EV reported via ISO 15118, which this crate does not
//! implement - see `docs/PRODUCTION-ROADMAP.md` B4.5). **2.0.1/2.1 only** - 1.6J has none of these
//! four messages. See `docs/PRODUCTION-ROADMAP.md` B2.8.
//!
//! Shaped like [`crate::meter_values`] and [`crate::reservation`]'s `ReservationStatusNotifier`:
//! this is entirely outbound, so there is a notifier trait per pair of related messages, a
//! per-version wire mapping, and [`run_smart_charging_notifications`] forwarding whatever
//! [`crate::actor::ChargePointActor::subscribe_smart_charging_notifications`] emits. Nothing here
//! decides *when* to notify - that is [`crate::state::ChargePointState::apply`], reacting to
//! [`crate::state::ChargePointEvent::ExternalChargingLimitSet`]/
//! [`crate::state::ChargePointEvent::ExternalChargingLimitCleared`]/
//! [`crate::state::EvseEvent::EVChargingNeedsReported`]/
//! [`crate::state::EvseEvent::EVChargingScheduleReported`], all of which an integrator (a local
//! energy-management binding, or an ISO 15118 stack) pushes in the same way
//! [`crate::state::ConnectorEvent::MeterValueSampled`] lets hardware push in a meter reading.

use alloc::boxed::Box;

use crate::state::{
    ChargingLimitSource, EVChargingNeeds, EVChargingScheduleReport, ExternalChargingLimit,
    SmartChargingNotification,
};

/// Reports an external charging limit taking effect or clearing, via `NotifyChargingLimit`/
/// `ClearedChargingLimit`. Implemented per protocol version, mirroring
/// [`crate::reservation::ReservationStatusNotifier`].
#[async_trait::async_trait]
pub trait ChargingLimitNotifier {
    /// What went wrong reporting the limit.
    type Error: core::error::Error + Send + Sync + 'static;

    /// Reports that `limit` is now in force on `evse_id` (`None` for the whole charging station).
    async fn notify_charging_limit(
        &self,
        evse_id: Option<usize>,
        limit: &ExternalChargingLimit,
    ) -> Result<(), Self::Error>;

    /// Reports that a limit from `source` on `evse_id` (`None` for the whole charging station) has
    /// gone away.
    async fn notify_cleared_charging_limit(
        &self,
        evse_id: Option<usize>,
        source: ChargingLimitSource,
    ) -> Result<(), Self::Error>;
}

/// The CSMS's answer to `NotifyEVChargingNeeds` - OCPP's `NotifyEVChargingNeedsStatusEnum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EVChargingNeedsOutcome {
    /// The CSMS accepted the reported needs.
    Accepted,
    /// The CSMS rejected them.
    Rejected,
    /// The CSMS is still working out what to do with them; the charge point should wait for a
    /// `SetChargingProfile` rather than treat this as a final answer.
    Processing,
    /// *(2.1 only)* The charge point has no charging profile prepared yet - retry later. Never
    /// produced by the 2.0.1 adapter, whose wire enum has no such value.
    NoChargingProfile,
}

/// The CSMS's answer to `NotifyEVChargingSchedule` - OCPP's `GenericStatusEnum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenericNotifyOutcome {
    /// The CSMS accepted the reported schedule.
    Accepted,
    /// The CSMS rejected it.
    Rejected,
}

/// Forwards whatever an EV reported via ISO 15118 - `NotifyEVChargingNeeds`/
/// `NotifyEVChargingSchedule`. Implemented per protocol version.
#[async_trait::async_trait]
pub trait EVChargingNotifier {
    /// What went wrong reporting to the CSMS.
    type Error: core::error::Error + Send + Sync + 'static;

    /// Reports `needs`, reported by the EV connected to `evse_id`.
    async fn notify_ev_charging_needs(
        &self,
        evse_id: usize,
        needs: &EVChargingNeeds,
    ) -> Result<EVChargingNeedsOutcome, Self::Error>;

    /// Reports `report`'s schedule, for the EV connected to `evse_id`.
    async fn notify_ev_charging_schedule(
        &self,
        evse_id: usize,
        report: &EVChargingScheduleReport,
    ) -> Result<GenericNotifyOutcome, Self::Error>;
}

/// Forwards every [`SmartChargingNotification`] to the CSMS, until `notifications` closes (the
/// actor stopped). A failed send is logged and dropped rather than queued - mirrors
/// [`crate::reservation::run_reservation_status_updates`]'s reasoning exactly: a notification
/// telling the CSMS "this was true a while ago" after an outage is worth much less than the
/// CSMS's own re-derivation once reconnected (a fresh `SetChargingProfile`/`GetCompositeSchedule`
/// round-trip for a charging limit; a fresh ISO 15118 session for EV needs/schedule), and none of
/// these four messages are the sole record of anything this crate depends on later.
pub async fn run_smart_charging_notifications<N>(
    mut notifications: crate::sync::BroadcastReceiver<SmartChargingNotification>,
    csms: &N,
) where
    N: ChargingLimitNotifier + EVChargingNotifier,
{
    while let Ok(notification) = notifications.recv().await {
        match notification {
            SmartChargingNotification::ExternalChargingLimitSet { evse_id, limit } => {
                if let Err(err) = csms.notify_charging_limit(evse_id, &limit).await {
                    tracing::warn!(
                        error = %err,
                        ?evse_id,
                        "failed to report an external charging limit"
                    );
                }
            }
            SmartChargingNotification::ExternalChargingLimitCleared { evse_id, source } => {
                if let Err(err) = csms.notify_cleared_charging_limit(evse_id, source).await {
                    tracing::warn!(
                        error = %err,
                        ?evse_id,
                        "failed to report a cleared external charging limit"
                    );
                }
            }
            SmartChargingNotification::EVChargingNeedsReported { evse_id, needs } => {
                match csms.notify_ev_charging_needs(evse_id, &needs).await {
                    Ok(EVChargingNeedsOutcome::Accepted) => {}
                    Ok(outcome) => tracing::info!(
                        evse_id,
                        ?outcome,
                        "the CSMS did not accept the reported EV charging needs"
                    ),
                    Err(err) => tracing::warn!(
                        error = %err,
                        evse_id,
                        "failed to report EV charging needs"
                    ),
                }
            }
            SmartChargingNotification::EVChargingScheduleReported { evse_id, report } => {
                match csms.notify_ev_charging_schedule(evse_id, &report).await {
                    Ok(GenericNotifyOutcome::Accepted) => {}
                    Ok(outcome) => tracing::info!(
                        evse_id,
                        ?outcome,
                        "the CSMS did not accept the reported EV charging schedule"
                    ),
                    Err(err) => tracing::warn!(
                        error = %err,
                        evse_id,
                        "failed to report an EV charging schedule"
                    ),
                }
            }
        }
    }
}

#[cfg(all(test, feature = "tokio-runtime"))]
mod tests {
    use super::*;
    use crate::actor::ChargePointActor;
    use crate::executor::TokioExecutor;
    use crate::state::{ChargePointEvent, ChargingLimitSource};
    use alloc::sync::Arc;
    use alloc::vec::Vec;

    /// A notifier recording every call it received, implementing both traits at once - a CSMS
    /// client in this crate always does, since one connection speaks for both message pairs.
    #[derive(Clone, Default)]
    struct RecordingNotifier {
        limits_set: Arc<std::sync::Mutex<Vec<Option<usize>>>>,
        limits_cleared: Arc<std::sync::Mutex<Vec<Option<usize>>>>,
        needs: Arc<std::sync::Mutex<Vec<usize>>>,
        schedules: Arc<std::sync::Mutex<Vec<usize>>>,
    }

    #[async_trait::async_trait]
    impl ChargingLimitNotifier for RecordingNotifier {
        type Error = core::convert::Infallible;

        async fn notify_charging_limit(
            &self,
            evse_id: Option<usize>,
            _limit: &ExternalChargingLimit,
        ) -> Result<(), Self::Error> {
            self.limits_set.lock().unwrap().push(evse_id);
            Ok(())
        }

        async fn notify_cleared_charging_limit(
            &self,
            evse_id: Option<usize>,
            _source: ChargingLimitSource,
        ) -> Result<(), Self::Error> {
            self.limits_cleared.lock().unwrap().push(evse_id);
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl EVChargingNotifier for RecordingNotifier {
        type Error = core::convert::Infallible;

        async fn notify_ev_charging_needs(
            &self,
            evse_id: usize,
            _needs: &EVChargingNeeds,
        ) -> Result<EVChargingNeedsOutcome, Self::Error> {
            self.needs.lock().unwrap().push(evse_id);
            Ok(EVChargingNeedsOutcome::Accepted)
        }

        async fn notify_ev_charging_schedule(
            &self,
            evse_id: usize,
            _report: &EVChargingScheduleReport,
        ) -> Result<GenericNotifyOutcome, Self::Error> {
            self.schedules.lock().unwrap().push(evse_id);
            Ok(GenericNotifyOutcome::Accepted)
        }
    }

    #[tokio::test]
    async fn every_notification_reaches_the_csms_in_order() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let notifications = actor.subscribe_smart_charging_notifications();
        let notifier = RecordingNotifier::default();
        let forwarding = {
            let notifier = notifier.clone();
            tokio::spawn(async move {
                run_smart_charging_notifications(notifications, &notifier).await;
            })
        };

        actor
            .send(ChargePointEvent::ExternalChargingLimitSet {
                evse_id: Some(0),
                limit: ExternalChargingLimit {
                    source: ChargingLimitSource::Ems,
                    is_grid_critical: None,
                    schedule: None,
                },
            })
            .await
            .unwrap();
        actor
            .send(ChargePointEvent::ExternalChargingLimitCleared {
                evse_id: Some(0),
                source: ChargingLimitSource::Ems,
            })
            .await
            .unwrap();
        actor
            .send(ChargePointEvent::Evse {
                evse_id: 0,
                event: crate::state::EvseEvent::EVChargingNeedsReported(EVChargingNeeds {
                    requested_energy_transfer: crate::state::EnergyTransferMode::Dc,
                    departure_time: None,
                    ac: None,
                    dc: None,
                    max_schedule_tuples: None,
                }),
            })
            .await
            .unwrap();

        for _ in 0..50 {
            if !notifier.schedules.lock().unwrap().is_empty()
                || !notifier.needs.lock().unwrap().is_empty()
            {
                break;
            }
            tokio::time::sleep(core::time::Duration::from_millis(10)).await;
        }

        assert_eq!(*notifier.limits_set.lock().unwrap(), alloc::vec![Some(0)]);
        assert_eq!(
            *notifier.limits_cleared.lock().unwrap(),
            alloc::vec![Some(0)]
        );
        assert_eq!(*notifier.needs.lock().unwrap(), alloc::vec![0]);
        forwarding.abort();
    }
}

#[cfg(feature = "ocpp_2_0_1")]
use super::ocpp_2_0_1::wire_schedule as wire_schedule_2_0_1;
#[cfg(feature = "ocpp_2_1")]
use super::ocpp_2_1::wire_schedule as wire_schedule_2_1;

/// OCPP 2.1's wire mapping.
#[cfg(feature = "ocpp_2_1")]
mod ocpp_2_1 {
    use super::{
        ChargingLimitNotifier, ChargingLimitSource, EVChargingNeeds, EVChargingNeedsOutcome,
        EVChargingNotifier, EVChargingScheduleReport, ExternalChargingLimit, GenericNotifyOutcome,
        wire_schedule_2_1,
    };
    use crate::state::{AcChargingNeeds, DcChargingNeeds, EnergyTransferMode};
    use crate::wire::v21::common::{
        ACChargingParameters, ChargingLimit, ChargingNeeds, DCChargingParameters,
        EnergyTransferModeEnum, GenericStatusEnum, NotifyEVChargingNeedsStatusEnum,
    };
    use crate::wire::v21::{
        ClearedChargingLimitRequest, NotifyChargingLimitRequest, NotifyEVChargingNeedsRequest,
        NotifyEVChargingScheduleRequest,
    };
    use alloc::boxed::Box;
    use alloc::vec;
    use ocpp_client::ClientError;
    use ocpp_client::ocpp_2_1::{OCPP2_1Client, OCPP2_1Error};

    fn wire_source(source: ChargingLimitSource) -> heapless::String<20> {
        let text = match source {
            ChargingLimitSource::Ems => "EMS",
            ChargingLimitSource::Cso => "CSO",
            ChargingLimitSource::So => "SO",
            ChargingLimitSource::Other => "Other",
        };
        heapless::String::try_from(text).unwrap_or_default()
    }

    fn wire_charging_limit(limit: &ExternalChargingLimit) -> ChargingLimit {
        ChargingLimit {
            charging_limit_source: wire_source(limit.source),
            custom_data: None,
            is_grid_critical: limit.is_grid_critical,
            is_local_generation: None,
        }
    }

    fn wire_energy_transfer_mode(mode: EnergyTransferMode) -> EnergyTransferModeEnum {
        match mode {
            EnergyTransferMode::Dc => EnergyTransferModeEnum::DC,
            EnergyTransferMode::AcSinglePhase => EnergyTransferModeEnum::ACsinglephase,
            EnergyTransferMode::AcTwoPhase => EnergyTransferModeEnum::ACtwophase,
            EnergyTransferMode::AcThreePhase => EnergyTransferModeEnum::ACthreephase,
        }
    }

    fn wire_ac_parameters(ac: &AcChargingNeeds) -> ACChargingParameters {
        ACChargingParameters {
            custom_data: None,
            energy_amount: ac.energy_amount_wh as f64,
            ev_max_current: ac.ev_max_current_a as f64,
            ev_max_voltage: ac.ev_max_voltage_v as f64,
            ev_min_current: ac.ev_min_current_a as f64,
        }
    }

    fn wire_dc_parameters(dc: &DcChargingNeeds) -> DCChargingParameters {
        DCChargingParameters {
            bulk_so_c: dc.bulk_soc_percent,
            custom_data: None,
            energy_amount: dc.energy_amount_wh.map(|v| v as f64),
            ev_energy_capacity: dc.ev_energy_capacity_wh.map(|v| v as f64),
            ev_max_current: dc.ev_max_current_a as f64,
            ev_max_power: dc.ev_max_power_w.map(|v| v as f64),
            ev_max_voltage: dc.ev_max_voltage_v as f64,
            full_so_c: dc.full_soc_percent,
            state_of_charge: dc.state_of_charge_percent,
        }
    }

    fn wire_charging_needs(needs: &EVChargingNeeds) -> ChargingNeeds {
        ChargingNeeds {
            ac_charging_parameters: needs.ac.as_ref().map(wire_ac_parameters),
            available_energy_transfer: None,
            control_mode: None,
            custom_data: None,
            dc_charging_parameters: needs.dc.as_ref().map(wire_dc_parameters),
            departure_time: needs.departure_time.map(Into::into),
            der_charging_parameters: None,
            ev_energy_offer: None,
            mobility_needs_mode: None,
            requested_energy_transfer: wire_energy_transfer_mode(needs.requested_energy_transfer),
            v2x_charging_parameters: None,
        }
    }

    fn map_needs_outcome(status: NotifyEVChargingNeedsStatusEnum) -> EVChargingNeedsOutcome {
        match status {
            NotifyEVChargingNeedsStatusEnum::Accepted => EVChargingNeedsOutcome::Accepted,
            NotifyEVChargingNeedsStatusEnum::Rejected => EVChargingNeedsOutcome::Rejected,
            NotifyEVChargingNeedsStatusEnum::Processing => EVChargingNeedsOutcome::Processing,
            NotifyEVChargingNeedsStatusEnum::NoChargingProfile => {
                EVChargingNeedsOutcome::NoChargingProfile
            }
        }
    }

    fn map_generic_outcome(status: GenericStatusEnum) -> GenericNotifyOutcome {
        match status {
            GenericStatusEnum::Accepted => GenericNotifyOutcome::Accepted,
            GenericStatusEnum::Rejected => GenericNotifyOutcome::Rejected,
        }
    }

    #[async_trait::async_trait]
    impl ChargingLimitNotifier for OCPP2_1Client {
        type Error = ClientError<OCPP2_1Error>;

        async fn notify_charging_limit(
            &self,
            evse_id: Option<usize>,
            limit: &ExternalChargingLimit,
        ) -> Result<(), Self::Error> {
            self.send_notify_charging_limit(NotifyChargingLimitRequest {
                charging_limit: wire_charging_limit(limit),
                charging_schedule: limit
                    .schedule
                    .as_ref()
                    .map(|schedule| vec![wire_schedule_2_1(schedule)]),
                custom_data: None,
                evse_id: evse_id.map(|id| id as i64 + 1),
            })
            .await?;
            Ok(())
        }

        async fn notify_cleared_charging_limit(
            &self,
            evse_id: Option<usize>,
            source: ChargingLimitSource,
        ) -> Result<(), Self::Error> {
            self.send_cleared_charging_limit(ClearedChargingLimitRequest {
                charging_limit_source: wire_source(source),
                custom_data: None,
                evse_id: evse_id.map(|id| id as i64 + 1),
            })
            .await?;
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl EVChargingNotifier for OCPP2_1Client {
        type Error = ClientError<OCPP2_1Error>;

        async fn notify_ev_charging_needs(
            &self,
            evse_id: usize,
            needs: &EVChargingNeeds,
        ) -> Result<EVChargingNeedsOutcome, Self::Error> {
            let response = self
                .send_notify_ev_charging_needs(NotifyEVChargingNeedsRequest {
                    charging_needs: wire_charging_needs(needs),
                    custom_data: None,
                    evse_id: evse_id as i64 + 1,
                    max_schedule_tuples: needs.max_schedule_tuples.map(i64::from),
                    timestamp: None,
                })
                .await?;
            Ok(map_needs_outcome(response.status))
        }

        async fn notify_ev_charging_schedule(
            &self,
            evse_id: usize,
            report: &EVChargingScheduleReport,
        ) -> Result<GenericNotifyOutcome, Self::Error> {
            let response = self
                .send_notify_ev_charging_schedule(NotifyEVChargingScheduleRequest {
                    charging_schedule: wire_schedule_2_1(&report.schedule),
                    custom_data: None,
                    evse_id: evse_id as i64 + 1,
                    power_tolerance_acceptance: report.power_tolerance_accepted,
                    selected_charging_schedule_id: report
                        .selected_charging_schedule_id
                        .map(i64::from),
                    time_base: report.time_base.into(),
                })
                .await?;
            Ok(map_generic_outcome(response.status))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn every_charging_limit_source_maps_to_a_wire_string() {
            assert_eq!(wire_source(ChargingLimitSource::Ems).as_str(), "EMS");
            assert_eq!(wire_source(ChargingLimitSource::Cso).as_str(), "CSO");
            assert_eq!(wire_source(ChargingLimitSource::So).as_str(), "SO");
            assert_eq!(wire_source(ChargingLimitSource::Other).as_str(), "Other");
        }

        #[test]
        fn every_energy_transfer_mode_maps_to_a_wire_value() {
            assert_eq!(
                wire_energy_transfer_mode(EnergyTransferMode::Dc),
                EnergyTransferModeEnum::DC
            );
            assert_eq!(
                wire_energy_transfer_mode(EnergyTransferMode::AcSinglePhase),
                EnergyTransferModeEnum::ACsinglephase
            );
            assert_eq!(
                wire_energy_transfer_mode(EnergyTransferMode::AcTwoPhase),
                EnergyTransferModeEnum::ACtwophase
            );
            assert_eq!(
                wire_energy_transfer_mode(EnergyTransferMode::AcThreePhase),
                EnergyTransferModeEnum::ACthreephase
            );
        }

        #[test]
        fn every_needs_status_maps_to_an_outcome() {
            assert_eq!(
                map_needs_outcome(NotifyEVChargingNeedsStatusEnum::Accepted),
                EVChargingNeedsOutcome::Accepted
            );
            assert_eq!(
                map_needs_outcome(NotifyEVChargingNeedsStatusEnum::Rejected),
                EVChargingNeedsOutcome::Rejected
            );
            assert_eq!(
                map_needs_outcome(NotifyEVChargingNeedsStatusEnum::Processing),
                EVChargingNeedsOutcome::Processing
            );
            assert_eq!(
                map_needs_outcome(NotifyEVChargingNeedsStatusEnum::NoChargingProfile),
                EVChargingNeedsOutcome::NoChargingProfile
            );
        }

        #[test]
        fn the_evse_id_offsets_by_one_or_reports_the_whole_station() {
            let limit = ExternalChargingLimit {
                source: ChargingLimitSource::Ems,
                is_grid_critical: Some(true),
                schedule: None,
            };
            let request = NotifyChargingLimitRequest {
                charging_limit: wire_charging_limit(&limit),
                charging_schedule: None,
                custom_data: None,
                evse_id: Some(1usize).map(|id| id as i64 + 1),
            };
            assert_eq!(request.evse_id, Some(2));

            let station_wide = NotifyChargingLimitRequest {
                charging_limit: wire_charging_limit(&limit),
                charging_schedule: None,
                custom_data: None,
                evse_id: None::<usize>.map(|id| id as i64 + 1),
            };
            assert_eq!(station_wide.evse_id, None);
        }
    }
}

/// OCPP 2.0.1's wire mapping - the same shape as 2.1's above, except every AC/DC parameter is a
/// bare `i64` rather than `f64` (2.1 widened them for ISO 15118-20's fractional values), and
/// `ChargingLimit`/`ClearedChargingLimit`'s source is a closed `ChargingLimitSourceEnum` rather
/// than a free string.
#[cfg(feature = "ocpp_2_0_1")]
mod ocpp_2_0_1 {
    use super::{
        ChargingLimitNotifier, ChargingLimitSource, EVChargingNeeds, EVChargingNeedsOutcome,
        EVChargingNotifier, EVChargingScheduleReport, ExternalChargingLimit, GenericNotifyOutcome,
        wire_schedule_2_0_1,
    };
    use crate::state::{AcChargingNeeds, DcChargingNeeds, EnergyTransferMode};
    use crate::wire::v201::common::{
        ACChargingParameters, ChargingLimit, ChargingLimitSourceEnum, ChargingNeeds,
        DCChargingParameters, EnergyTransferModeEnum, GenericStatusEnum,
        NotifyEVChargingNeedsStatusEnum,
    };
    use crate::wire::v201::{
        ClearedChargingLimitRequest, NotifyChargingLimitRequest, NotifyEVChargingNeedsRequest,
        NotifyEVChargingScheduleRequest,
    };
    use alloc::boxed::Box;
    use ocpp_client::ClientError;
    use ocpp_client::ocpp_2_0_1::{OCPP2_0_1Client, OCPP2_0_1Error};

    fn wire_source(source: ChargingLimitSource) -> ChargingLimitSourceEnum {
        match source {
            ChargingLimitSource::Ems => ChargingLimitSourceEnum::EMS,
            ChargingLimitSource::Cso => ChargingLimitSourceEnum::CSO,
            ChargingLimitSource::So => ChargingLimitSourceEnum::SO,
            ChargingLimitSource::Other => ChargingLimitSourceEnum::Other,
        }
    }

    fn wire_charging_limit(limit: &ExternalChargingLimit) -> ChargingLimit {
        ChargingLimit {
            charging_limit_source: wire_source(limit.source),
            custom_data: None,
            is_grid_critical: limit.is_grid_critical,
        }
    }

    fn wire_energy_transfer_mode(mode: EnergyTransferMode) -> EnergyTransferModeEnum {
        match mode {
            EnergyTransferMode::Dc => EnergyTransferModeEnum::DC,
            EnergyTransferMode::AcSinglePhase => EnergyTransferModeEnum::ACsinglephase,
            EnergyTransferMode::AcTwoPhase => EnergyTransferModeEnum::ACtwophase,
            EnergyTransferMode::AcThreePhase => EnergyTransferModeEnum::ACthreephase,
        }
    }

    fn wire_ac_parameters(ac: &AcChargingNeeds) -> ACChargingParameters {
        ACChargingParameters {
            custom_data: None,
            energy_amount: ac.energy_amount_wh,
            ev_max_current: ac.ev_max_current_a,
            ev_max_voltage: ac.ev_max_voltage_v,
            ev_min_current: ac.ev_min_current_a,
        }
    }

    fn wire_dc_parameters(dc: &DcChargingNeeds) -> DCChargingParameters {
        DCChargingParameters {
            bulk_so_c: dc.bulk_soc_percent,
            custom_data: None,
            energy_amount: dc.energy_amount_wh,
            ev_energy_capacity: dc.ev_energy_capacity_wh,
            ev_max_current: dc.ev_max_current_a,
            ev_max_power: dc.ev_max_power_w,
            ev_max_voltage: dc.ev_max_voltage_v,
            full_so_c: dc.full_soc_percent,
            state_of_charge: dc.state_of_charge_percent,
        }
    }

    fn wire_charging_needs(needs: &EVChargingNeeds) -> ChargingNeeds {
        ChargingNeeds {
            ac_charging_parameters: needs.ac.as_ref().map(wire_ac_parameters),
            custom_data: None,
            dc_charging_parameters: needs.dc.as_ref().map(wire_dc_parameters),
            departure_time: needs.departure_time.map(Into::into),
            requested_energy_transfer: wire_energy_transfer_mode(needs.requested_energy_transfer),
        }
    }

    fn map_needs_outcome(status: NotifyEVChargingNeedsStatusEnum) -> EVChargingNeedsOutcome {
        match status {
            NotifyEVChargingNeedsStatusEnum::Accepted => EVChargingNeedsOutcome::Accepted,
            NotifyEVChargingNeedsStatusEnum::Rejected => EVChargingNeedsOutcome::Rejected,
            NotifyEVChargingNeedsStatusEnum::Processing => EVChargingNeedsOutcome::Processing,
        }
    }

    fn map_generic_outcome(status: GenericStatusEnum) -> GenericNotifyOutcome {
        match status {
            GenericStatusEnum::Accepted => GenericNotifyOutcome::Accepted,
            GenericStatusEnum::Rejected => GenericNotifyOutcome::Rejected,
        }
    }

    #[async_trait::async_trait]
    impl ChargingLimitNotifier for OCPP2_0_1Client {
        type Error = ClientError<OCPP2_0_1Error>;

        async fn notify_charging_limit(
            &self,
            evse_id: Option<usize>,
            limit: &ExternalChargingLimit,
        ) -> Result<(), Self::Error> {
            self.send_notify_charging_limit(NotifyChargingLimitRequest {
                charging_limit: wire_charging_limit(limit),
                charging_schedule: limit
                    .schedule
                    .as_ref()
                    .map(|schedule| alloc::vec![wire_schedule_2_0_1(schedule)]),
                custom_data: None,
                evse_id: evse_id.map(|id| id as i64 + 1),
            })
            .await?;
            Ok(())
        }

        async fn notify_cleared_charging_limit(
            &self,
            evse_id: Option<usize>,
            source: ChargingLimitSource,
        ) -> Result<(), Self::Error> {
            self.send_cleared_charging_limit(ClearedChargingLimitRequest {
                charging_limit_source: wire_source(source),
                custom_data: None,
                evse_id: evse_id.map(|id| id as i64 + 1),
            })
            .await?;
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl EVChargingNotifier for OCPP2_0_1Client {
        type Error = ClientError<OCPP2_0_1Error>;

        async fn notify_ev_charging_needs(
            &self,
            evse_id: usize,
            needs: &EVChargingNeeds,
        ) -> Result<EVChargingNeedsOutcome, Self::Error> {
            let response = self
                .send_notify_ev_charging_needs(NotifyEVChargingNeedsRequest {
                    charging_needs: wire_charging_needs(needs),
                    custom_data: None,
                    evse_id: evse_id as i64 + 1,
                    max_schedule_tuples: needs.max_schedule_tuples.map(i64::from),
                })
                .await?;
            Ok(map_needs_outcome(response.status))
        }

        async fn notify_ev_charging_schedule(
            &self,
            evse_id: usize,
            report: &EVChargingScheduleReport,
        ) -> Result<GenericNotifyOutcome, Self::Error> {
            let response = self
                .send_notify_ev_charging_schedule(NotifyEVChargingScheduleRequest {
                    charging_schedule: wire_schedule_2_0_1(&report.schedule),
                    custom_data: None,
                    evse_id: evse_id as i64 + 1,
                    time_base: report.time_base.into(),
                })
                .await?;
            Ok(map_generic_outcome(response.status))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn every_charging_limit_source_maps_to_a_wire_value() {
            assert_eq!(
                wire_source(ChargingLimitSource::Ems),
                ChargingLimitSourceEnum::EMS
            );
            assert_eq!(
                wire_source(ChargingLimitSource::Cso),
                ChargingLimitSourceEnum::CSO
            );
            assert_eq!(
                wire_source(ChargingLimitSource::So),
                ChargingLimitSourceEnum::SO
            );
            assert_eq!(
                wire_source(ChargingLimitSource::Other),
                ChargingLimitSourceEnum::Other
            );
        }

        #[test]
        fn every_needs_status_maps_to_an_outcome() {
            assert_eq!(
                map_needs_outcome(NotifyEVChargingNeedsStatusEnum::Accepted),
                EVChargingNeedsOutcome::Accepted
            );
            assert_eq!(
                map_needs_outcome(NotifyEVChargingNeedsStatusEnum::Rejected),
                EVChargingNeedsOutcome::Rejected
            );
            assert_eq!(
                map_needs_outcome(NotifyEVChargingNeedsStatusEnum::Processing),
                EVChargingNeedsOutcome::Processing
            );
        }

        #[test]
        fn ac_parameters_carry_amp_and_watt_values_as_plain_integers() {
            let ac = AcChargingNeeds {
                energy_amount_wh: 40_000,
                ev_max_current_a: 32,
                ev_max_voltage_v: 230,
                ev_min_current_a: 6,
            };
            let wire = wire_ac_parameters(&ac);
            assert_eq!(wire.energy_amount, 40_000);
            assert_eq!(wire.ev_max_current, 32);
            assert_eq!(wire.ev_max_voltage, 230);
            assert_eq!(wire.ev_min_current, 6);
        }
    }
}
