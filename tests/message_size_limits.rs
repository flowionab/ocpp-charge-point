//! CV2.8 end-to-end: a request larger than the ceiling this charge point publishes in its own
//! device model comes back as the CALLERROR OCPP names for it, and the connection carries on.
//!
//! The per-limit arithmetic is unit-tested in `ocpp_charge_point::message_limits`. What only a
//! real socket can show is the part that actually matters to a CSMS: that the guard is wired into
//! the registered handler ahead of the work, that the refusal reaches the wire as a CALLERROR
//! rather than a CALLRESULT with some status, and that refusing did not cost the session -
//! `CLAUDE.md`'s containment rule applies to a request this charge point declines just as much as
//! to one it fails.
//!
//! `GetVariables` is the subject because it is the one B06.FR.16 names explicitly, it is a pure
//! read with no capability gating to race, and its ceiling (`DeviceDataCtrlr.ItemsPerMessage`
//! instance `GetVariables`, 50) is a built-in default rather than something the test has to
//! install first.

use futures::{SinkExt, StreamExt};
use ocpp_charge_point::connect_and_setup;
use ocpp_charge_point::executor::TokioExecutor;
use ocpp_charge_point::hardware::{ChargePoint, Connector, Evse};
use ocpp_charge_point::provisioning::TokioBackoff;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

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

    fn capabilities(&self) -> ocpp_charge_point::hardware::Capabilities {
        ocpp_charge_point::hardware::Capabilities::default()
    }

    async fn start(
        self: Arc<Self>,
        _events: ocpp_charge_point::hardware::HardwareEventSender,
        _commands: ocpp_charge_point::hardware::HardwareCommandReceiver,
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

async fn accept_boot(socket: &mut Socket, call: &Value) {
    let message_id = call[1].as_str().unwrap().to_string();
    let response = json!([
        3,
        message_id,
        { "currentTime": "2024-01-01T00:00:00Z", "interval": 300, "status": "Accepted" }
    ]);
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

fn charge_point() -> TestChargePoint {
    TestChargePoint {
        evses: [TestEvse {
            connectors: [TestConnector],
        }],
    }
}

/// One `getVariableData` element the charge point could otherwise answer perfectly well - so a
/// refusal can only be about how many of them there are.
fn get_variable_datum() -> Value {
    json!({
        "component": { "name": "OCPPCommCtrlr" },
        "variable": { "name": "HeartbeatInterval" }
    })
}

#[tokio::test]
async fn a_get_variables_over_items_per_message_is_refused_and_the_session_survives() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let csms = tokio::spawn(async move {
        let mut socket = accept(&listener).await;
        let boot = next_call(&mut socket, "BootNotification").await;
        accept_boot(&mut socket, &boot).await;

        // BootNotification is answered from inside `setup()`, before the blocks registered after
        // it - including `GetVariables`'s handler - exist. Same grace period, same reason, as
        // `malformed_payload.rs`.
        tokio::time::sleep(core::time::Duration::from_millis(500)).await;

        // `DeviceDataCtrlr.ItemsPerMessage[GetVariables]` is registered at 50.
        let oversized = json!([
            2,
            "too-many-items",
            "GetVariables",
            { "getVariableData": vec![get_variable_datum(); 51] }
        ]);
        socket
            .send(Message::text(serde_json::to_string(&oversized).unwrap()))
            .await
            .unwrap();

        let reply = next_reply_to(&mut socket, "too-many-items").await;
        assert_eq!(
            reply[0], 4,
            "B06.FR.16: an oversized GetVariables must be a CALLERROR, not a CALLRESULT: {reply}"
        );
        assert_eq!(
            reply[2], "OccurrenceConstraintViolation",
            "B06.FR.16 names this code specifically: {reply}"
        );
        // The description exists so an operator does not have to reproduce the failure to learn
        // which variable they were over and by how much.
        let description = reply[3].as_str().unwrap_or_default();
        assert!(
            description.contains("ItemsPerMessage") && description.contains("51"),
            "the refusal should name the variable and the count: {reply}"
        );

        // A request inside the ceiling is answered normally straight afterwards - refusing one
        // message must not cost the session.
        let allowed = json!([
            2,
            "within-the-limit",
            "GetVariables",
            { "getVariableData": vec![get_variable_datum(); 2] }
        ]);
        socket
            .send(Message::text(serde_json::to_string(&allowed).unwrap()))
            .await
            .unwrap();
        let reply = next_reply_to(&mut socket, "within-the-limit").await;
        assert_eq!(reply[0], 3, "unexpected reply: {reply}");
        assert_eq!(reply[2]["getVariableResult"].as_array().unwrap().len(), 2);
    });

    let _runtime = connect_and_setup(
        charge_point(),
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
