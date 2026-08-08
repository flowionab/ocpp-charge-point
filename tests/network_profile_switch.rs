//! A9 end to end, over real sockets: a CSMS writes a network connection profile pointing at a
//! *different* address, and the charge point moves there.
//!
//! The proof is that the second server sees a `BootNotification`. That means the whole chain ran -
//! the profile was stored, the priority order selected it, the redial target was re-pointed, the
//! connection was closed, `ocpp-client` redialled the new address through the *same* `Client`, and
//! this crate's reconnect handling re-registered on arrival.

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

/// Accepts one OCPP 2.1 WebSocket connection.
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

/// Reads frames until a CALL for `action` arrives, answering nothing else. Returns the call.
///
/// Skipping other traffic rather than asserting on the first frame keeps this test about the
/// switch: a charge point is free to send a StatusNotification or anything else on connect, and
/// this test should not fail when it starts doing so.
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

/// Answers a BootNotification CALL with `Accepted`.
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

#[tokio::test]
async fn a_set_network_profile_moves_the_connection_to_the_new_address() {
    let first = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let second = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let first_addr = first.local_addr().unwrap();
    let second_addr = second.local_addr().unwrap();

    // The CSMS that boots the charge point, then hands it a profile pointing somewhere else.
    let handover = tokio::spawn(async move {
        let mut socket = accept(&first).await;
        let boot = next_call(&mut socket, "BootNotification").await;
        accept_boot(&mut socket, &boot).await;

        // BootNotification is sent from inside `setup()`, before the blocks registered after it -
        // including `SetNetworkProfile`'s handler - exist. A CSMS that answers the boot and
        // immediately sends a request can therefore beat the handler to the socket and get a
        // `NotImplemented` CALLERROR, which is a race in this test rather than in the crate.
        tokio::time::sleep(core::time::Duration::from_millis(500)).await;

        let request = json!([
            2,
            "switch-1",
            "SetNetworkProfile",
            {
                "configurationSlot": 1,
                "connectionData": {
                    "ocppVersion": "OCPP20",
                    "ocppTransport": "JSON",
                    "ocppCsmsUrl": format!("ws://{second_addr}"),
                    "messageTimeout": 30,
                    "securityProfile": 1,
                    "ocppInterface": "Wired0"
                }
            }
        ]);
        socket
            .send(Message::text(serde_json::to_string(&request).unwrap()))
            .await
            .unwrap();

        // The charge point must accept the profile before it can act on it.
        loop {
            let frame = match socket.next().await {
                Some(Ok(Message::Text(text))) => text.to_string(),
                Some(Ok(_)) => continue,
                other => {
                    panic!("connection ended before SetNetworkProfile was answered: {other:?}")
                }
            };
            let message: Value = serde_json::from_str(&frame).unwrap();
            assert_ne!(
                message[0], 4,
                "SetNetworkProfile was answered with a CALLERROR: {message}"
            );
            if message[0] == 3 && message[1] == "switch-1" {
                assert_eq!(message[2]["status"], "Accepted");
                break;
            }
        }
    });

    // The address the profile names. Seeing a BootNotification here is the whole assertion.
    //
    // More than one connection may arrive before it does, which is why this loops rather than
    // reading a single socket. `ConnectionCloser::close_connection` forces a redial (it must not
    // *close* - see `crate::network_switch::ConnectionCloser`), and from `ocpp-client` 0.3.0
    // `Client::force_reconnect` tears down whichever connection is current when the read loop
    // observes the request. When the CSMS closes its own side as this one does - immediately
    // after answering `SetNetworkProfile` - the client's own redial and the forced one race, and
    // the forced one can land on the connection the natural redial just established. The charge
    // point recovers by redialling again, so what is guaranteed is that it ends up connected to
    // the new address and boots there, not that it gets there in exactly one attempt.
    let arrival = tokio::spawn(async move {
        for _ in 0..4 {
            let mut socket = accept(&second).await;
            loop {
                let frame = match socket.next().await {
                    Some(Ok(Message::Text(text))) => text.to_string(),
                    Some(Ok(_)) => continue,
                    // This connection went away before booting; wait for the next one.
                    _ => break,
                };
                let call: Value = serde_json::from_str(&frame).unwrap();
                if call[0] == 2 && call[2] == "BootNotification" {
                    assert_eq!(call[3]["chargingStation"]["vendorName"], "Acme");
                    accept_boot(&mut socket, &call).await;
                    return;
                }
            }
        }
        panic!("the charge point never booted at the address the profile named");
    });

    let _runtime = connect_and_setup(
        TestChargePoint {
            evses: [TestEvse {
                connectors: [TestConnector],
            }],
        },
        &format!("ws://{first_addr}"),
        None,
        None,
        None,
        TokioExecutor,
        TokioBackoff,
    )
    .await
    .unwrap();

    tokio::time::timeout(core::time::Duration::from_secs(10), handover)
        .await
        .expect("the charge point never answered SetNetworkProfile")
        .unwrap();

    // Bounded, so a charge point that never moves fails the test instead of hanging it.
    tokio::time::timeout(core::time::Duration::from_secs(10), arrival)
        .await
        .expect("the charge point never connected to the address the profile named")
        .unwrap();
}
