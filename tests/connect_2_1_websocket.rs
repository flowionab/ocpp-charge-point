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

    async fn set_current_limit(&self, _limit_ma: u32) -> Result<(), Self::Error> {
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

#[tokio::test]
async fn connect_and_setup_reports_an_unsupported_negotiated_version_instead_of_hanging() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        // The CSMS only speaks 1.6J - a real, valid WebSocket-layer negotiation outcome this
        // crate can't yet drive a full session in (see `connect.rs`'s module docs).
        tokio_tungstenite::accept_hdr_async(
            tcp,
            // The closure's `Result` type is dictated by tungstenite's `Callback` trait, which
            // we don't control - boxing the `Err` variant would require wrapping every call
            // site instead of just this test helper, for no behavioural benefit.
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
    });

    let result = connect_and_setup(
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
    .await;
    let error = result
        .err()
        .expect("expected connect_and_setup to reject the negotiated version");

    assert!(matches!(
        error,
        ocpp_charge_point::ConnectAndSetupError::UnsupportedNegotiatedVersion(
            ocpp_client::OcppVersion::V1_6
        )
    ));

    server.await.unwrap();
}
