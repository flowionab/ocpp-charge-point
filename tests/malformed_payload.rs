//! F5.2, the malformed-payload half: a CALL for a real, registered action whose payload doesn't
//! decode must come back as a protocol-correct CALLERROR, and must not take the actor or the
//! connection down - consistent with `CLAUDE.md`'s failure-containment rules.
//!
//! This behaviour is `ocpp-client`'s own (`Client::on`'s decode-failure path answers with a
//! CALLERROR before this crate's handler ever runs), which this test verifies rather than
//! reimplements - `CLAUDE.md` reserves wire-protocol concerns to `ocpp-client`, and duplicating
//! its decode/error path here would be exactly the kind of transport duplication it forbids. What
//! this crate is responsible for, and what this test actually exercises, is that the connection
//! and the charge point's actor are unaffected: a well-formed request right after the malformed
//! one is still answered correctly.
//!
//! One real gap, documented rather than hidden: if a frame isn't even valid JSON (or isn't a
//! `[messageTypeId, uniqueId, ...]` array), `ocpp-client`'s read loop logs a warning and drops it
//! with **no** CALLERROR at all - there's no `uniqueId` to reply to it with, and no hook exists to
//! change that from this crate. It still doesn't crash anything, which the second test below
//! covers.

use futures::{SinkExt, StreamExt};
use ocpp_charge_point::connect_and_setup;
use ocpp_charge_point::executor::TokioExecutor;
use ocpp_charge_point::hardware::{ChargePoint, Connector, Evse};
use ocpp_charge_point::provisioning::TokioBackoff;
use serde_json::{Value, json};
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

    async fn vendor_name(&self) -> &str {
        "Acme"
    }

    async fn model_name(&self) -> &str {
        "Charger 9000"
    }

    async fn evses(&self) -> &[TestEvse] {
        &self.evses
    }

    async fn capabilities(&self) -> ocpp_charge_point::hardware::Capabilities {
        ocpp_charge_point::hardware::Capabilities::default()
    }

    async fn start(
        &self,
        _events: ocpp_charge_point::hardware::HardwareEventSender,
        _commands: ocpp_charge_point::hardware::HardwareCommandReceiver,
    ) -> Result<(), Self::StartError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl Evse<TestConnector> for TestEvse {
    type Error = core::convert::Infallible;

    async fn connectors(&self) -> &[TestConnector] {
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

fn charge_point() -> TestChargePoint {
    TestChargePoint {
        evses: [TestEvse {
            connectors: [TestConnector],
        }],
    }
}

/// Reads frames until one addressed to `message_id` arrives (a CALLRESULT or CALLERROR), skipping
/// anything else the charge point sends unprompted.
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

#[tokio::test]
async fn a_call_with_a_payload_that_does_not_decode_gets_a_callerror_and_the_connection_survives() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let csms = tokio::spawn(async move {
        let mut socket = accept(&listener).await;
        let boot = next_call(&mut socket, "BootNotification").await;
        accept_boot(&mut socket, &boot).await;

        // BootNotification is answered from inside `setup()`, before the blocks registered
        // after it - including `GetVariables`'s handler - exist. A short grace period avoids
        // this test racing that registration (see `network_profile_switch.rs`'s `handover` task
        // for the same wait, for the same reason).
        tokio::time::sleep(core::time::Duration::from_millis(500)).await;

        // `GetVariables` requires `getVariableData` (a non-empty array) - omitting it fails
        // `serde_json::from_value` inside `ocpp-client`'s own handler-dispatch, before this
        // crate's device-model handler ever runs. Picked over e.g. `Reset` because it is a pure
        // read with no capability gating to race against or be refused by (`crate::refusal`) -
        // this test is about decode failure, not about any particular action's own semantics.
        let malformed = json!([2, "malformed-1", "GetVariables", {}]);
        socket
            .send(Message::text(serde_json::to_string(&malformed).unwrap()))
            .await
            .unwrap();

        let reply = next_reply_to(&mut socket, "malformed-1").await;
        assert_eq!(
            reply[0], 4,
            "a payload that fails to decode must come back as a CALLERROR, not a CALLRESULT: \
             {reply}"
        );
        // The wire error code itself is `ocpp-client`'s to choose (see this file's module docs);
        // what this crate is responsible for is that a *reply* comes back at all, and it does.
        assert!(
            reply[2].is_string(),
            "CALLERROR must carry an error code: {reply}"
        );

        // The connection must still work: a well-formed `GetVariables` right after the malformed
        // one is answered normally, proving neither the actor nor the read loop went down.
        let well_formed = json!([
            2,
            "well-formed-1",
            "GetVariables",
            {
                "getVariableData": [
                    { "component": { "name": "OCPPCommCtrlr" }, "variable": { "name": "HeartbeatInterval" } }
                ]
            }
        ]);
        socket
            .send(Message::text(serde_json::to_string(&well_formed).unwrap()))
            .await
            .unwrap();
        let reply = next_reply_to(&mut socket, "well-formed-1").await;
        assert_eq!(
            reply[0], 3,
            "a well-formed request right after a malformed one must still be answered normally: \
             {reply}"
        );
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

#[tokio::test]
async fn a_frame_that_is_not_even_valid_json_does_not_take_the_connection_down() {
    // The honest upstream gap this file's module docs describe: `ocpp-client` drops a frame that
    // isn't valid JSON at all with no CALLERROR (there's no `uniqueId` to address one to). What
    // this crate is still responsible for - and what this test actually proves - is that nothing
    // crashes and the connection keeps working.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let csms = tokio::spawn(async move {
        let mut socket = accept(&listener).await;
        let boot = next_call(&mut socket, "BootNotification").await;
        accept_boot(&mut socket, &boot).await;

        // BootNotification is answered from inside `setup()`, before the blocks registered
        // after it - including `GetVariables`'s handler - exist. A short grace period avoids
        // this test racing that registration (see `network_profile_switch.rs`'s `handover` task
        // for the same wait, for the same reason).
        tokio::time::sleep(core::time::Duration::from_millis(500)).await;

        socket
            .send(Message::text("not json at all { [ garbage"))
            .await
            .unwrap();

        // A well-formed request right after it must still work.
        let well_formed = json!([
            2,
            "after-garbage",
            "GetVariables",
            {
                "getVariableData": [
                    { "component": { "name": "OCPPCommCtrlr" }, "variable": { "name": "HeartbeatInterval" } }
                ]
            }
        ]);
        socket
            .send(Message::text(serde_json::to_string(&well_formed).unwrap()))
            .await
            .unwrap();
        let reply = next_reply_to(&mut socket, "after-garbage").await;
        assert_eq!(reply[0], 3, "unexpected reply: {reply}");
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
