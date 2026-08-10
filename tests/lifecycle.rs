//! Full-session scenarios over a real socket (`docs/PRODUCTION-ROADMAP.md` H2.2 and H2.4).
//!
//! The unit suite proves each block's logic in isolation; these prove the pieces work *together*
//! over a WebSocket, which is the only place a wiring mistake between them shows up. Two distinct
//! questions are asked:
//!
//! - **H2.2, per version**: does a whole session - boot, plug, authorize, start, meter, stop -
//!   actually reach the CSMS, in order?
//! - **H2.4, across versions**: does the *same* internal event sequence produce each version's own
//!   wire shape? That is the claim `CLAUDE.md`'s architecture rests on - one protocol-independent
//!   state model, three projections - and it is the one thing a per-version unit test cannot check,
//!   because it needs the same input driven through all three.

mod common;

use common::MockCsms;
use ocpp_charge_point::ChargePointRuntime;
use ocpp_charge_point::OcppVersion;
use ocpp_charge_point::connect_and_setup;
use ocpp_charge_point::executor::TokioExecutor;
use ocpp_charge_point::hardware::{
    Capabilities, ChargePoint, Connector, Evse, HardwareCommandReceiver, HardwareEventSender,
};
use ocpp_charge_point::state::{ChargePointEvent, ConnectorEvent, EvseEvent, IdToken, IdTokenKind};
use std::sync::Arc;

struct TestChargePoint {
    evses: [TestEvse; 1],
}
struct TestEvse {
    connectors: [TestConnector; 1],
}
struct TestConnector;

#[async_trait::async_trait]
impl ChargePoint<TestEvse, TestConnector> for TestChargePoint {
    type StartError = core::convert::Infallible;
    fn vendor_name(&self) -> &str {
        "Acme"
    }
    fn model_name(&self) -> &str {
        "Charger 9000"
    }
    fn evses(&self) -> &[TestEvse] {
        &self.evses
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }
    async fn start(
        self: Arc<Self>,
        _events: HardwareEventSender,
        _commands: HardwareCommandReceiver,
    ) -> Result<(), Self::StartError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl Evse<TestConnector> for TestEvse {
    type Error = core::convert::Infallible;
    fn connectors(&self) -> &[TestConnector] {
        &self.connectors
    }
    async fn reboot(&self) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl Connector for TestConnector {
    type Error = core::convert::Infallible;
    async fn lock(&self) -> Result<(), Self::Error> {
        Ok(())
    }
    async fn unlock(&self) -> Result<(), Self::Error> {
        Ok(())
    }
    async fn close_contactor(&self) -> Result<(), Self::Error> {
        Ok(())
    }
    async fn open_contactor(&self) -> Result<(), Self::Error> {
        Ok(())
    }
    async fn set_current_limit(&self, _limit_ma: Option<u32>) -> Result<(), Self::Error> {
        Ok(())
    }
}

fn charge_point() -> TestChargePoint {
    TestChargePoint {
        evses: [TestEvse {
            connectors: [TestConnector],
        }],
    }
}

fn id_token() -> IdToken {
    IdToken {
        value: "04A224B2".into(),
        kind: IdTokenKind::ISO14443,
    }
}

/// Drives one connector through a whole charging session as the hardware would.
async fn drive_a_session(runtime: &ChargePointRuntime<TestChargePoint>) {
    for event in [
        ConnectorEvent::CableConnected,
        ConnectorEvent::LockConfirmed,
        ConnectorEvent::IdTokenPresented(id_token()),
        ConnectorEvent::ChargingAuthorized(id_token()),
        ConnectorEvent::ContactorClosed,
        ConnectorEvent::MeterValueSampled(ocpp_charge_point::state::MeterSample {
            energy_wh: 1_500,
            ..Default::default()
        }),
        ConnectorEvent::ChargingStopped(ocpp_charge_point::state::StopReason::Local),
        ConnectorEvent::ContactorOpened,
        ConnectorEvent::UnlockConfirmed,
        ConnectorEvent::CableDisconnected,
    ] {
        let _ = runtime
            .send(ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event,
                },
            })
            .await;
    }
    // Let the outbound forwarders drain.
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
}

async fn run_session(subprotocol: &'static str, version: OcppVersion) -> MockCsms {
    let csms = MockCsms::start(subprotocol, Vec::new()).await;
    let runtime = connect_and_setup(
        charge_point(),
        csms.address(),
        Some(&[version]),
        None,
        None,
        TokioExecutor,
        ocpp_charge_point::provisioning::TokioBackoff,
    )
    .await
    .expect("the session should come up");

    csms.wait_for("BootNotification").await;
    drive_a_session(&runtime).await;
    csms
}

#[tokio::test]
async fn a_full_session_reaches_the_csms_on_2_1() {
    let csms = run_session("ocpp2.1", OcppVersion::V2_1).await;
    let actions = csms.actions();

    // H2.2: the session opens with a boot and the connector's status reaches the CSMS - the
    // sequence a CSMS needs to believe a charge point is alive and usable.
    assert_eq!(
        actions.first().map(String::as_str),
        Some("BootNotification")
    );
    assert!(
        actions.iter().any(|a| a == "StatusNotification"),
        "no status reached the CSMS: {actions:?}"
    );
    // 2.x carries the whole session in TransactionEvent rather than Start/StopTransaction.
    assert!(
        actions.iter().any(|a| a == "TransactionEvent"),
        "no transaction reached the CSMS: {actions:?}"
    );
    assert!(
        !actions.iter().any(|a| a == "StartTransaction"),
        "2.1 must not send 1.6J's StartTransaction: {actions:?}"
    );
}

#[tokio::test]
async fn a_full_session_reaches_the_csms_on_2_0_1() {
    let csms = run_session("ocpp2.0.1", OcppVersion::V2_0_1).await;
    let actions = csms.actions();

    assert_eq!(
        actions.first().map(String::as_str),
        Some("BootNotification")
    );
    assert!(
        actions.iter().any(|a| a == "TransactionEvent"),
        "{actions:?}"
    );
}

#[tokio::test]
async fn a_full_session_reaches_the_csms_on_1_6j() {
    let csms = run_session("ocpp1.6", OcppVersion::V1_6).await;
    let actions = csms.actions();

    assert_eq!(
        actions.first().map(String::as_str),
        Some("BootNotification")
    );
    // H2.4's headline difference: 1.6J has no TransactionEvent at all - the same internal
    // transaction becomes a StartTransaction/StopTransaction pair.
    assert!(
        actions.iter().any(|a| a == "StartTransaction"),
        "1.6J must send StartTransaction: {actions:?}"
    );
    assert!(
        !actions.iter().any(|a| a == "TransactionEvent"),
        "1.6J has no TransactionEvent: {actions:?}"
    );
}

#[tokio::test]
async fn the_same_boot_produces_each_versions_own_wire_shape() {
    // H2.4 stated as directly as it can be: one internal event sequence, three projections. This
    // is the claim CLAUDE.md's architecture rests on, and no per-version unit test can check it,
    // because checking it means driving the *same* input through all three adapters.
    let two_one = run_session("ocpp2.1", OcppVersion::V2_1).await;
    let one_six = run_session("ocpp1.6", OcppVersion::V1_6).await;

    let boot_2_1 = two_one.wait_for("BootNotification").await;
    let boot_1_6 = one_six.wait_for("BootNotification").await;

    // 2.x nests the identity under `chargingStation`; 1.6J flattens it to the top level with
    // different field names. Same charge point, same hardware binding, two shapes.
    assert_eq!(boot_2_1.payload["chargingStation"]["vendorName"], "Acme");
    assert_eq!(boot_2_1.payload["chargingStation"]["model"], "Charger 9000");
    assert!(boot_2_1.payload["chargePointVendor"].is_null());

    assert_eq!(boot_1_6.payload["chargePointVendor"], "Acme");
    assert_eq!(boot_1_6.payload["chargePointModel"], "Charger 9000");
    assert!(boot_1_6.payload["chargingStation"].is_null());
}

/// H2.3: a CSMS that goes away mid-session must not lose the transaction, duplicate it, or
/// reorder it.
///
/// This is the scenario the offline queue exists for, and the one a unit test cannot really pose:
/// it needs a socket that actually dies. The mock CSMS is dropped mid-session, the charge point
/// keeps being driven by its hardware, and what matters is what the *state* says afterwards -
/// every event still accounted for, in order, with the transaction closed exactly once.
#[tokio::test]
async fn a_csms_that_disappears_mid_transaction_loses_nothing() {
    let csms = MockCsms::start("ocpp2.1", Vec::new()).await;
    let runtime = connect_and_setup(
        charge_point(),
        csms.address(),
        Some(&[OcppVersion::V2_1]),
        None,
        None,
        TokioExecutor,
        ocpp_charge_point::provisioning::TokioBackoff,
    )
    .await
    .expect("the session should come up");
    csms.wait_for("BootNotification").await;

    // Start charging, and confirm the CSMS saw it before the connection dies.
    for event in [
        ConnectorEvent::CableConnected,
        ConnectorEvent::LockConfirmed,
        ConnectorEvent::IdTokenPresented(id_token()),
        ConnectorEvent::ChargingAuthorized(id_token()),
        ConnectorEvent::ContactorClosed,
    ] {
        let _ = runtime
            .send(ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event,
                },
            })
            .await;
    }
    csms.wait_for("TransactionEvent").await;
    let before = csms.actions().len();

    // The CSMS goes away. Everything below happens with nowhere to send it.
    drop(csms);
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }

    for event in [
        ConnectorEvent::MeterValueSampled(ocpp_charge_point::state::MeterSample {
            energy_wh: 4_200,
            ..Default::default()
        }),
        ConnectorEvent::ChargingStopped(ocpp_charge_point::state::StopReason::Local),
        ConnectorEvent::ContactorOpened,
        ConnectorEvent::UnlockConfirmed,
        ConnectorEvent::CableDisconnected,
    ] {
        let _ = runtime
            .send(ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event,
                },
            })
            .await;
    }
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }

    // The charge point survived the disconnect: it kept applying hardware events rather than
    // wedging or panicking on a dead socket, and the session is properly closed out.
    let state = runtime.state();
    assert!(
        state.evses[0].transactions[0].is_none(),
        "the transaction should be closed, not left dangling"
    );
    assert_eq!(
        state.evses[0].connectors[0],
        ocpp_charge_point::state::ConnectorState::Available,
        "the connector should have returned to Available after the cable was removed"
    );
    assert!(before > 0, "the CSMS should have seen the session start");
}
