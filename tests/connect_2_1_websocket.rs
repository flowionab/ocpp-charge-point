//! Real WebSocket round-trip proving `connect_and_setup` dials a CSMS and completes the boot
//! sequence, closing the docs/ROADMAP.md §0/§2 gap ("setup() doesn't itself open a live
//! connection"). Shape mirrors ocpp-client's own tests/ocpp_2_1_websocket.rs.

use futures::{SinkExt, StreamExt};
use ocpp_charge_point::connect_and_setup;
use ocpp_charge_point::executor::TokioExecutor;
use ocpp_charge_point::hardware::{ChargePoint, Connector, Evse};
use ocpp_charge_point::provisioning::TokioBackoff;
use ocpp_charge_point::state::RegistrationStatus;
use serde_json::{Value, json};
use tokio::net::TcpListener;
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

#[tokio::test]
async fn connect_and_setup_completes_boot_notification_over_a_real_websocket() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_hdr_async(
            tcp,
            // The closure's `Result` type is dictated by tungstenite's `Callback` trait, which
            // we don't control - boxing the `Err` variant would require wrapping every call
            // site instead of just this test helper, for no behavioural benefit.
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
        .unwrap();

        let frame = match ws.next().await.unwrap().unwrap() {
            Message::Text(text) => text.to_string(),
            other => panic!("expected a text frame, got {other:?}"),
        };
        let call: Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(call[2], "BootNotification");
        assert_eq!(call[3]["chargingStation"]["vendorName"], "Acme");
        assert_eq!(call[3]["chargingStation"]["model"], "Charger 9000");
        let message_id = call[1].as_str().unwrap().to_string();

        let response = json!([
            3,
            message_id,
            {
                "currentTime": "2024-01-01T00:00:00Z",
                "interval": 300,
                "status": "Accepted"
            }
        ]);
        ws.send(Message::text(serde_json::to_string(&response).unwrap()))
            .await
            .unwrap();
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
        TokioExecutor,
        TokioBackoff,
    )
    .await
    .unwrap();

    assert_eq!(
        runtime.state().registration,
        Some(RegistrationStatus::Accepted)
    );

    server.await.unwrap();
}

/// A9/A1's payoff, and the test that used to assert the opposite: negotiating **1.6J** now runs a
/// real 1.6J session rather than being refused with `UnsupportedNegotiatedVersion`.
///
/// The mock CSMS answers 1.6J's own `BootNotification` shape - `chargePointVendor`/
/// `chargePointModel` flattened at the top level, where 2.x nests them under `chargingStation` -
/// which is exactly the projection `provisioning::ocpp_1_6` performs, proving the right adapter
/// set was selected rather than merely that *something* connected.
#[tokio::test]
async fn a_negotiated_1_6j_connection_runs_a_1_6j_session() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_hdr_async(
            tcp,
            #[allow(clippy::result_large_err)]
            |_req: &tokio_tungstenite::tungstenite::handshake::server::Request,
             mut response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                response
                    .headers_mut()
                    .insert("Sec-WebSocket-Protocol", "ocpp1.6".parse().unwrap());
                Ok(response)
            },
        )
        .await
        .unwrap();

        let frame = match ws.next().await.unwrap().unwrap() {
            Message::Text(text) => text.to_string(),
            other => panic!("expected a text frame, got {other:?}"),
        };
        let call: Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(call[2], "BootNotification");
        // 1.6J's flat shape, not 2.x's nested `chargingStation` - the version-specific
        // projection, observed on the wire.
        assert_eq!(call[3]["chargePointVendor"], "Acme");
        assert_eq!(call[3]["chargePointModel"], "Charger 9000");
        assert!(call[3].get("chargingStation").is_none());
        let message_id = call[1].as_str().unwrap().to_string();

        let response = json!([
            3,
            message_id,
            {
                "currentTime": "2024-01-01T00:00:00Z",
                "interval": 300,
                "status": "Accepted"
            }
        ]);
        ws.send(Message::text(serde_json::to_string(&response).unwrap()))
            .await
            .unwrap();
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
        TokioExecutor,
        TokioBackoff,
    )
    .await
    .unwrap();

    assert_eq!(
        runtime.state().registration,
        Some(RegistrationStatus::Accepted)
    );

    server.await.unwrap();
}

/// A3: `versions` is the offer list, so naming one version forces it. Here the CSMS would happily
/// speak 1.6J, but is never offered it.
#[tokio::test]
async fn restricting_the_offered_versions_forces_the_one_named() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let offered = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let seen = offered.clone();
        let mut ws = tokio_tungstenite::accept_hdr_async(
            tcp,
            #[allow(clippy::result_large_err)]
            move |req: &tokio_tungstenite::tungstenite::handshake::server::Request,
                  mut response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                *seen.lock().unwrap() = req
                    .headers()
                    .get("Sec-WebSocket-Protocol")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                response
                    .headers_mut()
                    .insert("Sec-WebSocket-Protocol", "ocpp2.1".parse().unwrap());
                Ok(response)
            },
        )
        .await
        .unwrap();

        let subprotocols = offered.lock().unwrap().clone();
        assert!(
            subprotocols.contains("ocpp2.1") && !subprotocols.contains("ocpp1.6"),
            "only the named version should be offered, got `{subprotocols}`"
        );

        let frame = match ws.next().await.unwrap().unwrap() {
            Message::Text(text) => text.to_string(),
            other => panic!("expected a text frame, got {other:?}"),
        };
        let call: Value = serde_json::from_str(&frame).unwrap();
        let message_id = call[1].as_str().unwrap().to_string();
        let response = json!([
            3,
            message_id,
            {
                "currentTime": "2024-01-01T00:00:00Z",
                "interval": 300,
                "status": "Accepted"
            }
        ]);
        ws.send(Message::text(serde_json::to_string(&response).unwrap()))
            .await
            .unwrap();
    });

    let runtime = connect_and_setup(
        TestChargePoint {
            evses: [TestEvse {
                connectors: [TestConnector],
            }],
        },
        &format!("ws://{addr}"),
        Some(&[ocpp_client::OcppVersion::V2_1]),
        None,
        TokioExecutor,
        TokioBackoff,
    )
    .await
    .unwrap();

    assert_eq!(
        runtime.state().registration,
        Some(RegistrationStatus::Accepted)
    );

    server.await.unwrap();
}
