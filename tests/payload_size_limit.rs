//! F5.2 end to end, over a real socket: a CSMS (or something impersonating one) sends an
//! oversized inbound frame, and the charge point refuses it, raises `MemoryExhaustion`, and keeps
//! running - rather than allocating whatever the frame decodes into, or taking the connection
//! down.
//!
//! The guard only covers *redials* (`crate::network_switch::ConnectionTarget::dial`) - the very
//! first connection is dialled by `ocpp_client::connect` itself, which exposes no hook to guard
//! (see `crate::payload_limit`'s module docs for exactly why). So this test boots the charge
//! point once, forces a reconnect to the same address, and sends the oversized frame on the
//! *second* connection - the one this crate's own transport wrapper actually sees.

use futures::{SinkExt, StreamExt};
use ocpp_charge_point::connect_and_setup;
use ocpp_charge_point::executor::TokioExecutor;
use ocpp_charge_point::hardware::{ChargePoint, Connector, Evse};
use ocpp_charge_point::payload_limit::PayloadLimits;
use ocpp_charge_point::provisioning::TokioBackoff;
use ocpp_charge_point::state::SecurityEventType;
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

/// Reads frames until a CALL for `action` arrives, answering nothing else.
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
async fn an_oversized_inbound_frame_on_a_reconnect_is_refused_and_raises_memory_exhaustion() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let csms = tokio::spawn(async move {
        // First connection: boot normally.
        let mut socket = accept(&listener).await;
        let boot = next_call(&mut socket, "BootNotification").await;
        accept_boot(&mut socket, &boot).await;

        // Force a reconnect to the *same* address by dropping this connection - no network
        // profile switch involved, just an ordinary redial through
        // `ConnectionTarget::dial`, which is what this crate's own payload guard wraps.
        socket.close(None).await.ok();
        drop(socket);

        // Second connection: this one goes through the guarded transport. Send a frame far
        // larger than the default 32 KiB ceiling *before* anything else - raw junk, not even
        // valid JSON, since the guard must refuse it purely on length, before any OCPP-J
        // decoding is attempted.
        let mut socket = accept(&listener).await;
        let oversized = "x".repeat(200 * 1024);
        socket.send(Message::text(oversized)).await.unwrap();

        // The connection must still be alive and functioning: the reconnect handling resends
        // BootNotification on arrival, and it must still get here and get answered, proving the
        // read loop was never disrupted by the oversized frame.
        let boot = next_call(&mut socket, "BootNotification").await;
        accept_boot(&mut socket, &boot).await;
    });

    let runtime = connect_and_setup(
        TestChargePoint {
            evses: [TestEvse {
                connectors: [TestConnector],
            }],
        },
        &format!("ws://{addr}"),
        None,
        None,
        // Exercise the configurable ceiling explicitly, rather than relying on the default,
        // so this test also proves `payload_limits` actually reaches the guard.
        Some(PayloadLimits {
            max_inbound_frame_bytes: 32 * 1024,
        }),
        TokioExecutor,
        TokioBackoff,
    )
    .await
    .unwrap();

    let mut security_events = runtime.subscribe_security_events();

    tokio::time::timeout(core::time::Duration::from_secs(10), csms)
        .await
        .expect("the CSMS side of the test timed out")
        .unwrap();

    // The oversized frame must have raised `MemoryExhaustion` - the same treatment every other
    // bounded-memory overflow in this crate gets (G2.1).
    let mut seen_memory_exhaustion = false;
    while let Ok(Ok(event)) = tokio::time::timeout(
        core::time::Duration::from_millis(200),
        security_events.recv(),
    )
    .await
    {
        if event.event_type == SecurityEventType::MemoryExhaustion {
            seen_memory_exhaustion = true;
            assert!(
                event
                    .tech_info
                    .as_deref()
                    .is_some_and(|info| info.contains("32768")),
                "tech_info should name the configured ceiling: {:?}",
                event.tech_info
            );
            break;
        }
    }
    assert!(
        seen_memory_exhaustion,
        "an oversized inbound frame on a reconnect must raise a MemoryExhaustion security event"
    );

    assert_eq!(
        runtime.state().registration,
        Some(ocpp_charge_point::state::RegistrationStatus::Accepted),
        "the charge point must still be a fully working, registered session after the oversized \
         frame - it was refused, not left to take the connection or the actor down"
    );
}
