//! `GetChargingProfiles` end to end, over a real socket: the CSMS installs a charging profile,
//! asks which profiles are installed, and gets it back in a `ReportChargingProfiles`.
//!
//! The unit tests cover the mapping and the chunking; what only a live session can show is that
//! the answer is assembled from the *store* rather than echoed from the request, and that the
//! report actually reaches the wire with the `requestId` that correlates it.

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
        // Smart charging is capability-gated (C3): without this the handlers are never
        // registered and the CSMS gets `NotImplemented`.
        let mut capabilities = ocpp_charge_point::hardware::Capabilities::default();
        capabilities.smart_charging = true;
        capabilities
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

/// Reads frames until the CALLRESULT for `message_id` arrives, returning its payload. Fails the
/// test on a CALLERROR rather than waiting out the timeout, so an unregistered handler says so.
async fn next_result(socket: &mut Socket, message_id: &str) -> Value {
    loop {
        let frame = match socket.next().await {
            Some(Ok(Message::Text(text))) => text.to_string(),
            Some(Ok(_)) => continue,
            other => panic!("connection ended while waiting for {message_id}'s result: {other:?}"),
        };
        let message: Value = serde_json::from_str(&frame).unwrap();
        assert_ne!(
            message[0], 4,
            "{message_id} was answered with a CALLERROR: {message}"
        );
        if message[0] == 3 && message[1] == message_id {
            return message[2].clone();
        }
    }
}

/// Built by hand rather than with `#[tokio::test]` so the worker stack can be raised.
///
/// 2.1's generated `ChargingProfile` is **56 KB by value** - its `ChargingSchedule` inlines
/// `AbsolutePriceSchedule`, `PriceLevelSchedule` and `SalesTariff` at their `heapless` capacities,
/// three schedules deep. Building one to send blows an unoptimised worker's 2 MB stack, though a
/// release build (where the temporaries are elided) is fine. Recorded in
/// `docs/PRODUCTION-ROADMAP.md` D2 - it is an upstream type-shape problem, and it matters well
/// beyond this test on an MCU whose whole stack is smaller than one of these values.
#[test]
fn a_get_charging_profiles_reports_what_set_charging_profile_installed() {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_stack_size(16 * 1024 * 1024)
        .enable_all()
        .build()
        .unwrap()
        .block_on(reports_what_set_charging_profile_installed());
}

async fn reports_what_set_charging_profile_installed() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let csms = tokio::spawn(async move {
        let mut socket = accept(&listener).await;
        let boot = next_call(&mut socket, "BootNotification").await;
        accept_boot(&mut socket, &boot).await;

        // BootNotification is sent from inside `setup()`, before the blocks registered after it
        // exist - see the network-profile switch test for the same race.
        tokio::time::sleep(core::time::Duration::from_millis(500)).await;

        let set = json!([
            2,
            "set-1",
            "SetChargingProfile",
            {
                "evseId": 1,
                "chargingProfile": {
                    "id": 77,
                    "stackLevel": 2,
                    "chargingProfilePurpose": "TxDefaultProfile",
                    "chargingProfileKind": "Absolute",
                    "chargingSchedule": [{
                        "id": 5,
                        "chargingRateUnit": "A",
                        "chargingSchedulePeriod": [
                            { "startPeriod": 0, "limit": 16.0 },
                            { "startPeriod": 1800, "limit": 32.0 }
                        ]
                    }]
                }
            }
        ]);
        socket
            .send(Message::text(serde_json::to_string(&set).unwrap()))
            .await
            .unwrap();
        let accepted = next_result(&mut socket, "set-1").await;
        assert_eq!(accepted["status"], "Accepted");

        let get = json!([
            2,
            "get-1",
            "GetChargingProfiles",
            { "requestId": 4242, "chargingProfile": {} }
        ]);
        socket
            .send(Message::text(serde_json::to_string(&get).unwrap()))
            .await
            .unwrap();

        // The report arrives before the response - see the adapter's own note on why, and why
        // `requestId` is what actually correlates the two.
        let report = next_call(&mut socket, "ReportChargingProfiles").await;
        let payload = &report[3];
        assert_eq!(payload["requestId"], 4242);
        assert_eq!(
            payload["evseId"], 1,
            "reported at the scope it was installed at"
        );
        assert_eq!(payload["chargingLimitSource"], "CSO");
        assert_eq!(
            payload["tbc"], false,
            "one profile is one message, and the last"
        );

        let profiles = payload["chargingProfile"].as_array().unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0]["id"], 77);
        assert_eq!(profiles[0]["stackLevel"], 2);
        assert_eq!(profiles[0]["chargingProfilePurpose"], "TxDefaultProfile");
        let periods = profiles[0]["chargingSchedule"][0]["chargingSchedulePeriod"]
            .as_array()
            .unwrap();
        assert_eq!(periods.len(), 2);
        assert_eq!(periods[1]["startPeriod"], 1800);
        assert_eq!(periods[1]["limit"], 32.0);

        // Answer the report so the client's call completes rather than timing out.
        let report_id = report[1].as_str().unwrap().to_string();
        socket
            .send(Message::text(
                serde_json::to_string(&json!([3, report_id, {}])).unwrap(),
            ))
            .await
            .unwrap();

        assert_eq!(
            next_result(&mut socket, "get-1").await["status"],
            "Accepted"
        );
    });

    let _runtime = connect_and_setup(
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

    tokio::time::timeout(core::time::Duration::from_secs(10), csms)
        .await
        .expect("the charge point never completed the GetChargingProfiles exchange")
        .unwrap();
}
