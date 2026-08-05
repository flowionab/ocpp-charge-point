//! Reset functional block: CSMS-initiated `Reset` (Immediate/OnIdle), targeting either the whole
//! charge point or one EVSE. See `docs/ROADMAP.md` §2.
//!
//! The state machine side of this (recording a pending `OnIdle` reset, driving an `Immediate`
//! reset's fail-safe stop, and dispatching the eventual reboot as a
//! [`HardwareCommand::Reboot`](crate::state::HardwareCommand::Reboot)) lives in
//! `crate::state`'s `ChargePointState::apply` - this module is the protocol-agnostic entry point
//! (`handle_reset`) plus the per-version wire adapters, following the same shape as
//! `crate::reservation` (protocol-agnostic handler fn + registration trait + per-version
//! `ocpp_2_1`/`ocpp_2_0_1`/`ocpp_1_6` submodules).

use crate::actor::ChargePointActor;
use crate::state::{ChargePointEvent, ChargePointState, ResetKind, ResetTarget};
use alloc::boxed::Box;

/// The outcome of a CSMS-initiated `Reset` request, matching OCPP's `ResetStatusEnum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetOutcome {
    /// `kind` was `Immediate`: the charge point will interrupt anything in progress and reboot
    /// right away, without waiting for this response to be sent.
    Accepted,
    /// `target` doesn't address an EVSE on this charge point.
    Rejected,
    /// `kind` was `OnIdle`: the charge point will reboot once `target` has no transaction in
    /// progress - immediately, if it already doesn't.
    Scheduled,
}

/// Handles a CSMS-initiated `Reset` request against `actor`: rejects an out-of-range EVSE
/// `target`, otherwise records the request (`ChargePointEvent::ResetRequested`) and reports the
/// outcome without waiting for the reboot itself to happen. Per OCPP, both `Immediate`'s
/// `Accepted` and `OnIdle`'s `Scheduled` are returned *before* the charge point actually
/// reboots - the fail-safe stop and/or the wait for idle, and the reboot that follows, all
/// happen asynchronously in the actor (see [`crate::state::ChargePointState`] for how the
/// reboot is eventually dispatched as a [`HardwareCommand::Reboot`](crate::state::HardwareCommand::Reboot)).
pub async fn handle_reset(
    actor: &ChargePointActor,
    target: ResetTarget,
    kind: ResetKind,
) -> ResetOutcome {
    if !target_exists(&actor.state(), target) {
        return ResetOutcome::Rejected;
    }

    let _ = actor
        .send(ChargePointEvent::ResetRequested { target, kind })
        .await;

    match kind {
        ResetKind::Immediate => ResetOutcome::Accepted,
        ResetKind::OnIdle => ResetOutcome::Scheduled,
    }
}

/// Whether `target` addresses a real EVSE (or the whole charge point, which always exists).
fn target_exists(state: &ChargePointState, target: ResetTarget) -> bool {
    match target {
        ResetTarget::ChargePoint => true,
        ResetTarget::Evse { evse_id } => state.evses.get(evse_id).is_some(),
    }
}

/// Registers this charge point's inbound `Reset` handling with the CSMS connection. Implemented
/// per protocol version (see the `ocpp_2_1`/`ocpp_2_0_1`/`ocpp_1_6` modules), mirroring
/// [`crate::reservation::ReserveNowHandler`].
#[async_trait::async_trait]
pub trait ResetHandler {
    async fn register_reset_handler(&self, actor: ChargePointActor);
}

#[cfg(test)]
mod tests {
    use super::{handle_reset, ResetOutcome};
    use crate::actor::ChargePointActor;
    use crate::executor::TokioExecutor;
    use crate::state::{
        ChargePointEvent, ConnectorEvent, ConnectorState, EvseEvent, HardwareCommand, ResetKind,
        ResetTarget,
    };
    use crate::sync::RecvError;

    #[tokio::test]
    async fn an_immediate_reset_with_nothing_in_progress_reboots_right_away() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let mut commands = actor.subscribe_commands();

        let outcome =
            handle_reset(&actor, ResetTarget::ChargePoint, ResetKind::Immediate).await;

        assert_eq!(outcome, ResetOutcome::Accepted);
        assert_eq!(
            commands.recv().await,
            Ok(HardwareCommand::Reboot { evse_id: 0 })
        );
        assert!(actor.state().pending_reset.is_none());
    }

    #[tokio::test]
    async fn an_onidle_reset_while_idle_reboots_right_away_but_still_reports_scheduled() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let mut commands = actor.subscribe_commands();

        let outcome = handle_reset(&actor, ResetTarget::ChargePoint, ResetKind::OnIdle).await;

        assert_eq!(outcome, ResetOutcome::Scheduled);
        assert_eq!(
            commands.recv().await,
            Ok(HardwareCommand::Reboot { evse_id: 0 })
        );
    }

    #[tokio::test]
    async fn an_onidle_reset_while_charging_waits_for_the_transaction_to_end() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        for event in [
            ConnectorEvent::CableConnected,
            ConnectorEvent::LockConfirmed,
            ConnectorEvent::RemoteStartRequested,
            ConnectorEvent::ContactorClosed,
        ] {
            actor
                .send(ChargePointEvent::Evse {
                    evse_id: 0,
                    event: EvseEvent::Connector {
                        connector_id: 0,
                        event,
                    },
                })
                .await
                .unwrap();
        }
        let mut commands = actor.subscribe_commands();

        let outcome = handle_reset(&actor, ResetTarget::ChargePoint, ResetKind::OnIdle).await;
        assert_eq!(outcome, ResetOutcome::Scheduled);
        assert!(actor.state().pending_reset.is_some());

        actor
            .send(ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event: ConnectorEvent::ChargingStopped(crate::state::StopReason::Local),
                },
            })
            .await
            .unwrap();
        assert_eq!(
            commands.recv().await.unwrap(),
            HardwareCommand::OpenContactor {
                evse_id: 0,
                connector_id: 0
            }
        );
        assert!(actor.state().pending_reset.is_some(), "still Stopping");

        actor
            .send(ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event: ConnectorEvent::ContactorOpened,
                },
            })
            .await
            .unwrap();

        assert!(actor.state().pending_reset.is_none());
        assert_eq!(
            commands.recv().await.unwrap(),
            HardwareCommand::UnlockConnector {
                evse_id: 0,
                connector_id: 0
            }
        );
        assert_eq!(
            commands.recv().await.unwrap(),
            HardwareCommand::Reboot { evse_id: 0 }
        );
    }

    #[tokio::test]
    async fn an_immediate_reset_while_charging_stops_the_transaction_fail_safely_then_reboots() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        for event in [
            ConnectorEvent::CableConnected,
            ConnectorEvent::LockConfirmed,
            ConnectorEvent::RemoteStartRequested,
            ConnectorEvent::ContactorClosed,
        ] {
            actor
                .send(ChargePointEvent::Evse {
                    evse_id: 0,
                    event: EvseEvent::Connector {
                        connector_id: 0,
                        event,
                    },
                })
                .await
                .unwrap();
        }
        let mut commands = actor.subscribe_commands();

        let outcome =
            handle_reset(&actor, ResetTarget::ChargePoint, ResetKind::Immediate).await;
        assert_eq!(outcome, ResetOutcome::Accepted);

        // Fail-safe order: contactor opens before anything is unlocked.
        assert_eq!(
            commands.recv().await.unwrap(),
            HardwareCommand::OpenContactor {
                evse_id: 0,
                connector_id: 0
            }
        );
        assert_eq!(
            actor.state().evses[0].connectors[0],
            ConnectorState::Stopping
        );
        assert!(actor.state().pending_reset.is_some());

        actor
            .send(ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event: ConnectorEvent::ContactorOpened,
                },
            })
            .await
            .unwrap();
        assert_eq!(
            commands.recv().await.unwrap(),
            HardwareCommand::UnlockConnector {
                evse_id: 0,
                connector_id: 0
            }
        );
        assert!(actor.state().pending_reset.is_some(), "still Finishing");

        actor
            .send(ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event: ConnectorEvent::UnlockConfirmed,
                },
            })
            .await
            .unwrap();

        assert_eq!(
            actor.state().evses[0].connectors[0],
            ConnectorState::Connected
        );
        assert!(actor.state().pending_reset.is_none());
        assert_eq!(
            commands.recv().await.unwrap(),
            HardwareCommand::Reboot { evse_id: 0 }
        );
        assert!(actor.state().evses[0].transactions[0].is_none());
    }

    #[tokio::test]
    async fn an_evse_targeted_reset_only_reboots_that_evse() {
        let actor = ChargePointActor::spawn([1, 1], &TokioExecutor);
        let mut commands = actor.subscribe_commands();

        let outcome = handle_reset(
            &actor,
            ResetTarget::Evse { evse_id: 1 },
            ResetKind::Immediate,
        )
        .await;

        assert_eq!(outcome, ResetOutcome::Accepted);
        assert_eq!(
            commands.recv().await,
            Ok(HardwareCommand::Reboot { evse_id: 1 })
        );
    }

    #[tokio::test]
    async fn an_unknown_evse_target_is_rejected() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let outcome = handle_reset(
            &actor,
            ResetTarget::Evse { evse_id: 5 },
            ResetKind::Immediate,
        )
        .await;

        assert_eq!(outcome, ResetOutcome::Rejected);
        assert!(actor.state().pending_reset.is_none());
    }

    #[tokio::test]
    async fn a_second_pending_reset_supersedes_the_first() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        // A transaction in progress keeps either request pending rather than firing right away,
        // so we can observe the second superseding the first.
        for event in [
            ConnectorEvent::CableConnected,
            ConnectorEvent::LockConfirmed,
            ConnectorEvent::RemoteStartRequested,
            ConnectorEvent::ContactorClosed,
        ] {
            actor
                .send(ChargePointEvent::Evse {
                    evse_id: 0,
                    event: EvseEvent::Connector {
                        connector_id: 0,
                        event,
                    },
                })
                .await
                .unwrap();
        }

        handle_reset(&actor, ResetTarget::ChargePoint, ResetKind::OnIdle).await;
        assert_eq!(
            actor.state().pending_reset,
            Some(crate::state::PendingReset {
                target: ResetTarget::ChargePoint,
                kind: ResetKind::OnIdle,
            })
        );

        handle_reset(
            &actor,
            ResetTarget::Evse { evse_id: 0 },
            ResetKind::OnIdle,
        )
        .await;

        assert_eq!(
            actor.state().pending_reset,
            Some(crate::state::PendingReset {
                target: ResetTarget::Evse { evse_id: 0 },
                kind: ResetKind::OnIdle,
            })
        );
    }

    #[tokio::test]
    async fn a_failed_reboot_faults_the_evse() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let mut commands = actor.subscribe_commands();
        let faulting_actor = actor.clone();
        tokio::spawn(async move {
            loop {
                match commands.recv().await {
                    Ok(HardwareCommand::Reboot { evse_id }) => {
                        let _ = faulting_actor
                            .send(ChargePointEvent::Evse {
                                evse_id,
                                event: EvseEvent::FaultDetected,
                            })
                            .await;
                    }
                    Ok(_) => {}
                    Err(RecvError::Closed) => break,
                }
            }
        });

        handle_reset(&actor, ResetTarget::ChargePoint, ResetKind::Immediate).await;

        // Give the spawned confirmer a chance to run.
        tokio::task::yield_now().await;
        assert_eq!(
            actor.state().evses[0].status,
            crate::state::EvseStatus::Faulted
        );
    }
}

#[cfg(feature = "ocpp_2_1")]
mod ocpp_2_1 {
    use super::{handle_reset, ResetHandler, ResetOutcome};
    use crate::actor::ChargePointActor;
    use crate::state::{ResetKind, ResetTarget};
    use alloc::boxed::Box;
    use ocpp_client::ocpp_2_1::OCPP2_1Client;
    use ocpp_client::ocpp_types::v21::common::{ResetEnum, ResetStatusEnum};
    use ocpp_client::ocpp_types::v21::{ResetRequest, ResetResponse};

    /// OCPP 2.1 adds `ImmediateAndResume` (reset immediately, then automatically resume the
    /// transaction that was interrupted) alongside `Immediate`/`OnIdle`. This crate doesn't yet
    /// model resuming a transaction across a reboot (`docs/ROADMAP.md` §2), so
    /// `ImmediateAndResume` projects down to plain `Immediate` - the reset itself still happens
    /// right away; only the "and resume" part isn't honored yet.
    pub(super) fn map_kind(kind: ResetEnum) -> ResetKind {
        match kind {
            ResetEnum::Immediate | ResetEnum::ImmediateAndResume => ResetKind::Immediate,
            ResetEnum::OnIdle => ResetKind::OnIdle,
        }
    }

    pub(super) fn map_outcome(outcome: ResetOutcome) -> ResetStatusEnum {
        match outcome {
            ResetOutcome::Accepted => ResetStatusEnum::Accepted,
            ResetOutcome::Rejected => ResetStatusEnum::Rejected,
            ResetOutcome::Scheduled => ResetStatusEnum::Scheduled,
        }
    }

    /// A negative wire `evseId` can't address an EVSE - treated the same as an out-of-range one,
    /// without needing to consult the actor.
    fn parse_target(request: &ResetRequest) -> Result<ResetTarget, ()> {
        match request.evse_id {
            None => Ok(ResetTarget::ChargePoint),
            Some(evse_id) => usize::try_from(evse_id)
                .map(|evse_id| ResetTarget::Evse { evse_id })
                .map_err(|_| ()),
        }
    }

    #[async_trait::async_trait]
    impl ResetHandler for OCPP2_1Client {
        async fn register_reset_handler(&self, actor: ChargePointActor) {
            self.on_reset(move |request, _client| {
                let actor = actor.clone();
                async move {
                    let target = parse_target(&request);
                    let kind = map_kind(request.r#type);
                    let outcome = match target {
                        Ok(target) => handle_reset(&actor, target, kind).await,
                        Err(()) => ResetOutcome::Rejected,
                    };
                    Ok(ResetResponse {
                        custom_data: None,
                        status: map_outcome(outcome),
                        status_info: None,
                    })
                }
            })
            .await;
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn immediate_and_immediate_and_resume_both_map_to_immediate() {
            assert_eq!(map_kind(ResetEnum::Immediate), ResetKind::Immediate);
            assert_eq!(
                map_kind(ResetEnum::ImmediateAndResume),
                ResetKind::Immediate
            );
        }

        #[test]
        fn on_idle_maps_to_on_idle() {
            assert_eq!(map_kind(ResetEnum::OnIdle), ResetKind::OnIdle);
        }

        #[test]
        fn every_outcome_maps_to_the_matching_wire_status() {
            assert_eq!(
                map_outcome(ResetOutcome::Accepted),
                ResetStatusEnum::Accepted
            );
            assert_eq!(
                map_outcome(ResetOutcome::Rejected),
                ResetStatusEnum::Rejected
            );
            assert_eq!(
                map_outcome(ResetOutcome::Scheduled),
                ResetStatusEnum::Scheduled
            );
        }

        fn request(evse_id: Option<i64>) -> ResetRequest {
            ResetRequest {
                custom_data: None,
                evse_id,
                r#type: ResetEnum::Immediate,
            }
        }

        #[test]
        fn no_evse_id_targets_the_whole_charge_point() {
            assert_eq!(parse_target(&request(None)), Ok(ResetTarget::ChargePoint));
        }

        #[test]
        fn a_valid_evse_id_targets_that_evse() {
            assert_eq!(
                parse_target(&request(Some(1))),
                Ok(ResetTarget::Evse { evse_id: 1 })
            );
        }

        #[test]
        fn a_negative_evse_id_fails_to_parse() {
            assert_eq!(parse_target(&request(Some(-1))), Err(()));
        }
    }
}

#[cfg(feature = "ocpp_2_0_1")]
mod ocpp_2_0_1 {
    use super::{handle_reset, ResetHandler, ResetOutcome};
    use crate::actor::ChargePointActor;
    use crate::state::{ResetKind, ResetTarget};
    use alloc::boxed::Box;
    use ocpp_client::ocpp_2_0_1::OCPP2_0_1Client;
    use ocpp_client::ocpp_types::v201::common::{ResetEnum, ResetStatusEnum};
    use ocpp_client::ocpp_types::v201::{ResetRequest, ResetResponse};

    /// OCPP 2.0.1's `ResetEnum` only has `Immediate`/`OnIdle` - a direct 1:1 mapping, unlike
    /// 2.1's `ImmediateAndResume` addition (see `super::ocpp_2_1::map_kind`).
    pub(super) fn map_kind(kind: ResetEnum) -> ResetKind {
        match kind {
            ResetEnum::Immediate => ResetKind::Immediate,
            ResetEnum::OnIdle => ResetKind::OnIdle,
        }
    }

    pub(super) fn map_outcome(outcome: ResetOutcome) -> ResetStatusEnum {
        match outcome {
            ResetOutcome::Accepted => ResetStatusEnum::Accepted,
            ResetOutcome::Rejected => ResetStatusEnum::Rejected,
            ResetOutcome::Scheduled => ResetStatusEnum::Scheduled,
        }
    }

    /// A negative wire `evseId` can't address an EVSE - treated the same as an out-of-range one,
    /// without needing to consult the actor.
    fn parse_target(request: &ResetRequest) -> Result<ResetTarget, ()> {
        match request.evse_id {
            None => Ok(ResetTarget::ChargePoint),
            Some(evse_id) => usize::try_from(evse_id)
                .map(|evse_id| ResetTarget::Evse { evse_id })
                .map_err(|_| ()),
        }
    }

    #[async_trait::async_trait]
    impl ResetHandler for OCPP2_0_1Client {
        async fn register_reset_handler(&self, actor: ChargePointActor) {
            self.on_reset(move |request, _client| {
                let actor = actor.clone();
                async move {
                    let target = parse_target(&request);
                    let kind = map_kind(request.r#type);
                    let outcome = match target {
                        Ok(target) => handle_reset(&actor, target, kind).await,
                        Err(()) => ResetOutcome::Rejected,
                    };
                    Ok(ResetResponse {
                        custom_data: None,
                        status: map_outcome(outcome),
                        status_info: None,
                    })
                }
            })
            .await;
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn every_kind_maps_1_to_1() {
            assert_eq!(map_kind(ResetEnum::Immediate), ResetKind::Immediate);
            assert_eq!(map_kind(ResetEnum::OnIdle), ResetKind::OnIdle);
        }

        #[test]
        fn every_outcome_maps_to_the_matching_wire_status() {
            assert_eq!(
                map_outcome(ResetOutcome::Accepted),
                ResetStatusEnum::Accepted
            );
            assert_eq!(
                map_outcome(ResetOutcome::Rejected),
                ResetStatusEnum::Rejected
            );
            assert_eq!(
                map_outcome(ResetOutcome::Scheduled),
                ResetStatusEnum::Scheduled
            );
        }

        fn request(evse_id: Option<i64>) -> ResetRequest {
            ResetRequest {
                custom_data: None,
                evse_id,
                r#type: ResetEnum::Immediate,
            }
        }

        #[test]
        fn no_evse_id_targets_the_whole_charge_point() {
            assert_eq!(parse_target(&request(None)), Ok(ResetTarget::ChargePoint));
        }

        #[test]
        fn a_valid_evse_id_targets_that_evse() {
            assert_eq!(
                parse_target(&request(Some(1))),
                Ok(ResetTarget::Evse { evse_id: 1 })
            );
        }

        #[test]
        fn a_negative_evse_id_fails_to_parse() {
            assert_eq!(parse_target(&request(Some(-1))), Err(()));
        }
    }
}

#[cfg(feature = "ocpp_1_6")]
mod ocpp_1_6 {
    use super::{handle_reset, ResetHandler, ResetOutcome};
    use crate::actor::ChargePointActor;
    use crate::state::{ResetKind, ResetTarget};
    use alloc::boxed::Box;
    use ocpp_client::ocpp_1_6::OCPP1_6Client;
    use ocpp_client::ocpp_types::v16::common::{ResetRequestType, ResetResponseStatus};
    use ocpp_client::ocpp_types::v16::ResetResponse;

    /// OCPP 1.6J's `Reset.req` has no `evseId` - a Reset always targets the whole charge point.
    /// Its `type` is `Hard`/`Soft` rather than 2.0.1/2.1's `Immediate`/`OnIdle`, with different
    /// (if related) semantics: per the OCPP 1.6 spec, `Hard` restarts the hardware without being
    /// required to gracefully stop an ongoing transaction first (a last-resort, possibly
    /// data-losing action), while `Soft` gracefully stops any ongoing transaction (sending
    /// `StopTransaction.req` for it) before restarting the application. Neither literally waits
    /// for the charge point to go idle on its own the way 2.0.1/2.1's `OnIdle` does - but the
    /// common, spec-recommended migration mapping (and the one this crate uses) pairs `Hard`
    /// with `Immediate` (interrupt right away, no waiting) and `Soft` with `OnIdle` (let what's
    /// in progress finish before rebooting), since that's the axis (urgent-and-abrupt vs.
    /// graceful-and-deferred) our internal `ResetKind` actually models.
    pub(super) fn map_kind(kind: ResetRequestType) -> ResetKind {
        match kind {
            ResetRequestType::Hard => ResetKind::Immediate,
            ResetRequestType::Soft => ResetKind::OnIdle,
        }
    }

    /// 1.6's `ResetResponseStatus` has no `Scheduled` value - `ResetOutcome::Scheduled` (an
    /// `OnIdle` request that's been recorded and will fire once idle) collapses to `Accepted`,
    /// the closest true statement 1.6 can express: the charge point *did* accept the request and
    /// will act on it, just without a distinct wire status for "later" vs. "now".
    pub(super) fn map_outcome(outcome: ResetOutcome) -> ResetResponseStatus {
        match outcome {
            ResetOutcome::Accepted | ResetOutcome::Scheduled => ResetResponseStatus::Accepted,
            ResetOutcome::Rejected => ResetResponseStatus::Rejected,
        }
    }

    #[async_trait::async_trait]
    impl ResetHandler for OCPP1_6Client {
        async fn register_reset_handler(&self, actor: ChargePointActor) {
            self.on_reset(move |request, _client| {
                let actor = actor.clone();
                async move {
                    let kind = map_kind(request.r#type);
                    let outcome = handle_reset(&actor, ResetTarget::ChargePoint, kind).await;
                    Ok(ResetResponse {
                        status: map_outcome(outcome),
                    })
                }
            })
            .await;
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn hard_maps_to_immediate() {
            assert_eq!(map_kind(ResetRequestType::Hard), ResetKind::Immediate);
        }

        #[test]
        fn soft_maps_to_on_idle() {
            assert_eq!(map_kind(ResetRequestType::Soft), ResetKind::OnIdle);
        }

        #[test]
        fn accepted_and_scheduled_both_map_to_accepted() {
            assert_eq!(
                map_outcome(ResetOutcome::Accepted),
                ResetResponseStatus::Accepted
            );
            assert_eq!(
                map_outcome(ResetOutcome::Scheduled),
                ResetResponseStatus::Accepted
            );
        }

        #[test]
        fn rejected_maps_to_rejected() {
            assert_eq!(
                map_outcome(ResetOutcome::Rejected),
                ResetResponseStatus::Rejected
            );
        }
    }
}
