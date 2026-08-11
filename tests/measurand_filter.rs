//! CV2.6 end-to-end, on the path an integrator actually uses: a CSMS that narrows
//! `SampledDataCtrlr.TxUpdatedMeasurands` over the wire stops receiving the measurands it dropped.
//!
//! The filter itself is unit-tested against the pure encoders in `crate::transactions`. What only
//! a real session can show is the part that was wrong before this test existed: that
//! [`connect_and_setup`] wires the *actor-aware* notifier into the TransactionEvent block, rather
//! than the bare client that has no device model to consult. A `SetVariables` returning `Accepted`
//! on a variable the send path then ignores is exactly the silent lie B05.FR.09 forbids, so this
//! asserts both halves: the write is accepted, **and** the next event obeys it.

use futures::{SinkExt, StreamExt};
use ocpp_charge_point::connect_and_setup;
use ocpp_charge_point::executor::TokioExecutor;
use ocpp_charge_point::hardware::{ChargePoint, Connector, Evse, HardwareEventSender};
use ocpp_charge_point::provisioning::TokioBackoff;
use ocpp_charge_point::state::{
    ChargePointEvent, ConnectorEvent, EvseEvent, IdToken, IdTokenKind, MeterSample,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

/// Hands the test the hardware event sender, so the test can drive a transaction and push a meter
/// sample carrying more measurands than the CSMS will ask for.
struct TestChargePoint {
    evses: [TestEvse; 1],
    events: Arc<Mutex<Option<HardwareEventSender>>>,
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

    fn capabilities(&self) -> ocpp_charge_point::hardware::Capabilities {
        ocpp_charge_point::hardware::Capabilities::default()
    }

    async fn start(
        self: Arc<Self>,
        events: HardwareEventSender,
        _commands: ocpp_charge_point::hardware::HardwareCommandReceiver,
    ) -> Result<(), Self::StartError> {
        *self.events.lock().unwrap() = Some(events);
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

type Socket = WebSocketStream<tokio::net::TcpStream>;

async fn accept(listener: &TcpListener) -> Socket {
    let (tcp, _) = listener.accept().await.unwrap();
    tokio_tungstenite::accept_hdr_async(
        tcp,
        #[allow(clippy::result_large_err)]
        |_req: &tokio_tungstenite::tungstenite::handshake::server::Request,
         mut response: tokio_tungstenite::tungstenite::handshake::server::Response| {
            response
                .headers_mut()
                .insert("Sec-WebSocket-Protocol", "ocpp2.1".parse().unwrap());
            Ok(response)
        },
    )
    .await
    .unwrap()
}

async fn next_call(socket: &mut Socket, action: &str) -> Value {
    loop {
        let frame = match socket.next().await {
            Some(Ok(Message::Text(text))) => text.to_string(),
            Some(Ok(_)) => continue,
            other => panic!("connection ended while waiting for {action}: {other:?}"),
        };
        let call: Value = serde_json::from_str(&frame).unwrap();
        if call[0] == 2 && call[2] == action {
            return call;
        }
    }
}

async fn answer(socket: &mut Socket, call: &Value, payload: Value) {
    let message_id = call[1].as_str().unwrap().to_string();
    let response = json!([3, message_id, payload]);
    socket
        .send(Message::text(serde_json::to_string(&response).unwrap()))
        .await
        .unwrap();
}

async fn next_reply_to(socket: &mut Socket, message_id: &str) -> Value {
    loop {
        let frame = match socket.next().await {
            Some(Ok(Message::Text(text))) => text.to_string(),
            Some(Ok(_)) => continue,
            other => {
                panic!("connection ended while waiting for a reply to {message_id}: {other:?}")
            }
        };
        let message: Value = serde_json::from_str(&frame).unwrap();
        if (message[0] == 3 || message[0] == 4) && message[1] == message_id {
            return message;
        }
    }
}

/// Reads `TransactionEvent`s until one actually carries meter data, answering each on the way.
///
/// A session produces several events with no `meterValue` at all - `Started`, and the
/// `ChargingStateChanged` the contactor raises - and those say nothing about the measurand filter
/// either way. Skipping to the first event that reports a reading is what makes this test about
/// filtering rather than about event ordering.
async fn next_transaction_event_with_meter_value(socket: &mut Socket) -> Value {
    loop {
        let event = next_call(socket, "TransactionEvent").await;
        answer(socket, &event, json!({})).await;
        if event[3]["meterValue"].is_array() {
            return event;
        }
    }
}

/// The measurands one `TransactionEvent` reported, as OCPP spells them.
fn measurands_of(event: &Value) -> Vec<String> {
    event[3]["meterValue"]
        .as_array()
        .map(|values| {
            values
                .iter()
                .flat_map(|value| {
                    value["sampledValue"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                })
                .map(|sampled| {
                    sampled["measurand"]
                        .as_str()
                        .unwrap_or("Energy.Active.Import.Register")
                        .to_string()
                })
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn narrowing_the_measurand_list_over_the_wire_narrows_the_next_transaction_event() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let events = Arc::new(Mutex::new(None));
    let hardware_events = events.clone();

    let csms = tokio::spawn(async move {
        let mut socket = accept(&listener).await;
        let boot = next_call(&mut socket, "BootNotification").await;
        answer(
            &mut socket,
            &boot,
            json!({ "currentTime": "2024-01-01T00:00:00Z", "interval": 300, "status": "Accepted" }),
        )
        .await;

        // The blocks are registered after BootNotification is answered - same grace period, same
        // reason, as `malformed_payload.rs`.
        tokio::time::sleep(core::time::Duration::from_millis(500)).await;

        // The default list is energy + power, so an unconfigured station reports both.
        let sender = hardware_events.lock().unwrap().clone().unwrap();
        start_charging(&sender).await;

        let updated = next_transaction_event_with_meter_value(&mut socket).await;
        let before = measurands_of(&updated);
        assert!(
            before.contains(&"Power.Active.Import".to_string()),
            "the registered default (energy + power) reports power: {updated}"
        );

        // Now the CSMS narrows the list to energy only. B05.FR.09: this must be accepted only
        // because the station really will act on it.
        let set = json!([
            2,
            "narrow-measurands",
            "SetVariables",
            { "setVariableData": [{
                "component": { "name": "SampledDataCtrlr" },
                "variable": { "name": "TxUpdatedMeasurands" },
                "attributeValue": "Energy.Active.Import.Register"
            }] }
        ]);
        socket
            .send(Message::text(serde_json::to_string(&set).unwrap()))
            .await
            .unwrap();
        let reply = next_reply_to(&mut socket, "narrow-measurands").await;
        assert_eq!(
            reply[2]["setVariableResult"][0]["attributeStatus"], "Accepted",
            "a measurand list this build honours must be settable: {reply}"
        );

        // ...and the very next event must obey it, with no reconnect and no reboot.
        sender
            .send(ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event: ConnectorEvent::MeterValueSampled(MeterSample {
                        energy_wh: 9_000,
                        power_w: Some(7_400),
                        ..Default::default()
                    }),
                },
            })
            .await
            .unwrap();

        let after = next_transaction_event_with_meter_value(&mut socket).await;
        let reported = measurands_of(&after);
        assert!(
            reported.contains(&"Energy.Active.Import.Register".to_string()),
            "energy was still selected: {after}"
        );
        assert!(
            !reported.contains(&"Power.Active.Import".to_string()),
            "power was sampled but no longer selected - the CSMS asked to stop receiving it and \
             this is the assertion that says connect_and_setup actually honours that: {after}"
        );
    });

    let _runtime = connect_and_setup(
        TestChargePoint {
            evses: [TestEvse {
                connectors: [TestConnector],
            }],
            events,
        },
        &format!("ws://{addr}"),
        None,
        None,
        None,
        TokioExecutor,
        TokioBackoff,
    )
    .await
    .unwrap();

    tokio::time::timeout(core::time::Duration::from_secs(10), csms)
        .await
        .expect("the CSMS side of the test timed out")
        .unwrap();
}

/// Drives a connector far enough to have a running transaction reporting meter data.
async fn start_charging(sender: &HardwareEventSender) {
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
        ConnectorEvent::MeterValueSampled(MeterSample {
            energy_wh: 4_200,
            power_w: Some(7_400),
            ..Default::default()
        }),
    ] {
        sender
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
}
