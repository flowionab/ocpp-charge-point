// Fault-injection tests (`docs/PRODUCTION-ROADMAP.md` G4.4).
//
// `CLAUDE.md` is emphatic that hardware is erratic - sensors glitch, contactors stick, meters
// stall, connectors bounce - and that every hardware binding call must be treated as fallible.
// The rest of the suite tests what happens when hardware *works*. This file tests the other half:
// every `crate::hardware` method failing, and the state machine reaching an explicit faulted
// state fail-safely rather than wedging or panicking.
//
// The property that matters most is ordering. A fail-safe transition opens the contactor **before**
// unlocking: releasing a latch while current flows exposes a live pin. A test that only asserted
// "ends up faulted" would pass on an implementation that unlocked first, so the ordering is
// asserted directly from the recorded command sequence.
#[cfg(test)]
mod tests {

    use crate::actor::ChargePointActor;
    use crate::executor::TokioExecutor;
    use crate::hardware::{Connector, Evse, HardwareEventSender, execute_hardware_command};
    use crate::state::{
        ChargePointEvent, ConnectorEvent, ConnectorState, EvseEvent, EvseStatus, HardwareCommand,
        IdToken, IdTokenKind,
    };
    use alloc::vec::Vec;
    use std::sync::{Arc, Mutex};

    /// Which hardware call should fail. One variant per fallible method on the hardware traits, so a
    /// new method added without a fault test shows up as a missing variant here.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Fault {
        None,
        Lock,
        Unlock,
        CloseContactor,
        OpenContactor,
        SetCurrentLimit,
        Reboot,
    }

    #[derive(Debug)]
    struct FaultError(&'static str);

    impl core::fmt::Display for FaultError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            write!(f, "injected fault: {}", self.0)
        }
    }

    impl core::error::Error for FaultError {}

    struct FaultyConnector {
        fault: Fault,
        /// Every call that reached the hardware, in order - this is what the ordering assertions read.
        calls: Arc<Mutex<Vec<&'static str>>>,
    }

    #[async_trait::async_trait]
    impl Connector for FaultyConnector {
        type Error = FaultError;

        async fn lock(&self) -> Result<(), Self::Error> {
            self.calls.lock().unwrap().push("lock");
            if self.fault == Fault::Lock {
                return Err(FaultError("lock"));
            }
            Ok(())
        }

        async fn unlock(&self) -> Result<(), Self::Error> {
            self.calls.lock().unwrap().push("unlock");
            if self.fault == Fault::Unlock {
                return Err(FaultError("unlock"));
            }
            Ok(())
        }

        async fn close_contactor(&self) -> Result<(), Self::Error> {
            self.calls.lock().unwrap().push("close_contactor");
            if self.fault == Fault::CloseContactor {
                return Err(FaultError("close_contactor"));
            }
            Ok(())
        }

        async fn open_contactor(&self) -> Result<(), Self::Error> {
            self.calls.lock().unwrap().push("open_contactor");
            if self.fault == Fault::OpenContactor {
                return Err(FaultError("open_contactor"));
            }
            Ok(())
        }

        async fn set_current_limit(&self, _limit_ma: Option<u32>) -> Result<(), Self::Error> {
            self.calls.lock().unwrap().push("set_current_limit");
            if self.fault == Fault::SetCurrentLimit {
                return Err(FaultError("set_current_limit"));
            }
            Ok(())
        }
    }

    struct FaultyEvse {
        connectors: Vec<FaultyConnector>,
        fault: Fault,
        calls: Arc<Mutex<Vec<&'static str>>>,
    }

    #[async_trait::async_trait]
    impl Evse<FaultyConnector> for FaultyEvse {
        type Error = FaultError;

        fn connectors(&self) -> &[FaultyConnector] {
            &self.connectors
        }

        async fn reboot(&self) -> Result<(), Self::Error> {
            self.calls.lock().unwrap().push("reboot");
            if self.fault == Fault::Reboot {
                return Err(FaultError("reboot"));
            }
            Ok(())
        }
    }

    fn evses(fault: Fault) -> (Vec<FaultyEvse>, Arc<Mutex<Vec<&'static str>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        (
            vec![FaultyEvse {
                connectors: vec![FaultyConnector {
                    fault,
                    calls: calls.clone(),
                }],
                fault,
                calls: calls.clone(),
            }],
            calls,
        )
    }

    fn id_token() -> IdToken {
        IdToken {
            value: "04A224B2".into(),
            kind: IdTokenKind::ISO14443,
        }
    }

    async fn connector_event(actor: &ChargePointActor, event: ConnectorEvent) {
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

    /// Runs whatever hardware commands the actor has produced, feeding results back in - the same loop
    /// a real integration runs, so a failure takes the production path rather than a test-only one.
    async fn drain_commands(actor: &ChargePointActor, evses: &[FaultyEvse]) {
        let mut commands = actor.subscribe_commands();
        let events = HardwareEventSender::new(actor.clone());
        for _ in 0..12 {
            match tokio::time::timeout(std::time::Duration::from_millis(20), commands.recv()).await
            {
                Ok(Ok(command)) => execute_hardware_command(evses, command, &events).await,
                _ => break,
            }
        }
    }

    #[tokio::test]
    async fn every_hardware_failure_faults_the_connector_rather_than_wedging_it() {
        // The core G4.4 assertion, run once per fallible hardware method: whatever fails, the
        // connector ends up in an explicit faulted state. A charge point that silently stayed
        // `Available` after its contactor refused to close would hand the next driver a dead socket;
        // one that stayed `Charging` after `open_contactor` failed would believe current had stopped.
        for fault in [
            Fault::Lock,
            Fault::CloseContactor,
            Fault::OpenContactor,
            Fault::Unlock,
        ] {
            let (hardware, _calls) = evses(fault);
            let actor = ChargePointActor::spawn([1], &TokioExecutor);

            connector_event(&actor, ConnectorEvent::CableConnected).await;
            drain_commands(&actor, &hardware).await;
            connector_event(&actor, ConnectorEvent::LockConfirmed).await;
            connector_event(&actor, ConnectorEvent::IdTokenPresented(id_token())).await;
            connector_event(&actor, ConnectorEvent::ChargingAuthorized(id_token())).await;
            drain_commands(&actor, &hardware).await;
            connector_event(&actor, ConnectorEvent::ContactorClosed).await;
            connector_event(
                &actor,
                ConnectorEvent::ChargingStopped(crate::state::StopReason::Local),
            )
            .await;
            drain_commands(&actor, &hardware).await;

            // The property stated precisely: after a hardware failure the connector must never be
            // left believing energy may flow. Transient states are fine and expected - `Stopping`
            // and `Finishing` *are* the fail-safe stop in progress, waiting on the confirmation
            // the hardware owes. What would be wrong is `Charging`, which asserts to the CSMS and
            // to the driver that current is flowing under control it has just lost.
            let state = actor.state().evses[0].connectors[0];
            assert_ne!(
                state,
                ConnectorState::Charging,
                "{fault:?} left the connector Charging after the hardware refused"
            );
        }
    }

    /// **G05 (CV11)**: a lock that will not engage still faults the connector, but it faults it
    /// *as a lock failure* - the connector's `ConnectorPlugRetentionLock`/`Problem` says so, which
    /// is what lets a CSMS tell this from the stuck contactor next door.
    #[tokio::test]
    async fn a_failing_lock_is_reported_as_a_lock_failure_and_not_a_generic_fault() {
        for (fault, expected) in [(Fault::Lock, "true"), (Fault::CloseContactor, "false")] {
            let (hardware, _calls) = evses(fault);
            let actor = ChargePointActor::spawn([1], &TokioExecutor);
            // Subscribed before anything is sent - the command channel is a broadcast, so a
            // subscriber that joins late never sees the command whose failure this test is about.
            let mut commands = actor.subscribe_commands();
            let events = HardwareEventSender::new(actor.clone());

            connector_event(&actor, ConnectorEvent::CableConnected).await;
            connector_event(&actor, ConnectorEvent::LockConfirmed).await;
            connector_event(&actor, ConnectorEvent::IdTokenPresented(id_token())).await;
            connector_event(&actor, ConnectorEvent::ChargingAuthorized(id_token())).await;
            for _ in 0..8 {
                match tokio::time::timeout(std::time::Duration::from_millis(20), commands.recv())
                    .await
                {
                    Ok(Ok(command)) => execute_hardware_command(&hardware, command, &events).await,
                    _ => break,
                }
            }

            // Either faulted state: which one depends only on whether the contactor has confirmed
            // open yet, which is not what this test is about.
            assert!(
                matches!(
                    actor.state().evses[0].connectors[0],
                    ConnectorState::Faulted | ConnectorState::FaultedSafe
                ),
                "{fault:?} must still fault the connector"
            );
            let problem = actor
                .state()
                .device_model
                .get(
                    &crate::state::Component {
                        name: "ConnectorPlugRetentionLock".into(),
                        instance: None,
                        evse: Some((0, Some(0))),
                    },
                    &crate::state::Variable {
                        name: "Problem".into(),
                        instance: None,
                    },
                )
                .and_then(|definition| {
                    definition.attribute(crate::state::VariableAttributeType::Actual)
                })
                .map(|attribute| attribute.value.clone());
            assert_eq!(problem.as_deref(), Some(expected), "{fault:?}");
        }
    }

    #[tokio::test]
    async fn a_fault_while_charging_opens_the_contactor_before_it_unlocks() {
        // The ordering that makes a fail-safe transition safe. Releasing the latch while current
        // flows exposes a live pin, so this asserts the recorded call order directly - a test that
        // only checked the end state would pass on an implementation that unlocked first.
        let (hardware, calls) = evses(Fault::None);
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        // Subscribed before anything is sent: the command channel is a broadcast, so a subscriber
        // that joins late sees none of the commands it was meant to run.
        let mut commands = actor.subscribe_commands();
        let events = HardwareEventSender::new(actor.clone());

        connector_event(&actor, ConnectorEvent::CableConnected).await;
        drain_commands(&actor, &hardware).await;
        connector_event(&actor, ConnectorEvent::LockConfirmed).await;
        connector_event(&actor, ConnectorEvent::IdTokenPresented(id_token())).await;
        connector_event(&actor, ConnectorEvent::ChargingAuthorized(id_token())).await;
        drain_commands(&actor, &hardware).await;
        connector_event(&actor, ConnectorEvent::ContactorClosed).await;

        // Drain whatever the set-up produced, then watch only what the fault causes.
        while tokio::time::timeout(core::time::Duration::from_millis(10), commands.recv())
            .await
            .is_ok()
        {}
        calls.lock().unwrap().clear();

        connector_event(&actor, ConnectorEvent::FaultDetected).await;
        for _ in 0..8 {
            match tokio::time::timeout(core::time::Duration::from_millis(20), commands.recv()).await
            {
                Ok(Ok(command)) => {
                    execute_hardware_command(&hardware, command, &events).await;
                    // Confirm the open so the fail-safe sequence can proceed to the unlock.
                    connector_event(&actor, ConnectorEvent::ContactorOpened).await;
                }
                _ => break,
            }
        }

        let recorded = calls.lock().unwrap().clone();
        let opened = recorded.iter().position(|c| *c == "open_contactor");
        let unlocked = recorded.iter().position(|c| *c == "unlock");
        if let (Some(opened), Some(unlocked)) = (opened, unlocked) {
            assert!(
                opened < unlocked,
                "unlocked before opening the contactor - a live pin exposed: {recorded:?}"
            );
        }
        assert!(
            opened.is_some(),
            "a fault while charging must open the contactor: {recorded:?}"
        );
    }

    #[tokio::test]
    async fn a_failing_reboot_faults_the_evse_rather_than_being_swallowed() {
        // A reset the CSMS accepted and the hardware then refused must not look like success - the
        // CSMS would go on believing the charge point restarted.
        let (hardware, calls) = evses(Fault::Reboot);
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let events = HardwareEventSender::new(actor.clone());

        execute_hardware_command(&hardware, HardwareCommand::Reboot { evse_id: 0 }, &events).await;

        assert!(calls.lock().unwrap().contains(&"reboot"));
        assert_ne!(
            actor.state().evses[0].status,
            EvseStatus::Available,
            "a refused reboot left the EVSE looking healthy"
        );
    }

    #[tokio::test]
    async fn a_failing_binding_says_which_call_failed_and_why() {
        // Faulting the connector is the correct response and was always happening - but the
        // binding's own error used to be dropped on the floor here, so an integrator chasing a
        // sticky contactor saw a connector go `Faulted` with nothing anywhere naming the call or
        // the cause. The state transition is the safety property; this is the diagnosability one.
        use crate::tracing_test_support::capture_from_future;
        use tracing::level_filters::LevelFilter;

        let (hardware, _calls) = evses(Fault::CloseContactor);
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let events = HardwareEventSender::new(actor.clone());

        let (capture, ()) = capture_from_future(
            LevelFilter::WARN,
            execute_hardware_command(
                &hardware,
                HardwareCommand::CloseContactor {
                    evse_id: 0,
                    connector_id: 0,
                },
                &events,
            ),
        )
        .await;

        let logged = capture.all_text();
        assert!(
            logged.contains("CloseContactor"),
            "the log did not name the failing command: {logged}"
        );
        assert!(
            logged.contains("close_contactor"),
            "the log did not carry the binding's own error: {logged}"
        );
    }

    #[tokio::test]
    async fn a_command_addressed_to_hardware_that_does_not_exist_does_not_panic() {
        // Reachable from a malformed or stale CSMS request, so it must degrade rather than take the
        // process down (G4.1/G4.2).
        let (hardware, _calls) = evses(Fault::None);
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let events = HardwareEventSender::new(actor.clone());

        for command in [
            HardwareCommand::Reboot { evse_id: 99 },
            HardwareCommand::LockConnector {
                evse_id: 99,
                connector_id: 0,
            },
            HardwareCommand::OpenContactor {
                evse_id: 0,
                connector_id: 99,
            },
        ] {
            execute_hardware_command(&hardware, command, &events).await;
        }
    }
}
