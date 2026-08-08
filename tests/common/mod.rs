//! A scripted mock CSMS over a real WebSocket (`docs/PRODUCTION-ROADMAP.md` H2.1).
//!
//! Every integration test here used to stand up its own `TcpListener`, negotiate its own
//! subprotocol and hand-decode its own frames. That is fine for one test and unworkable for the
//! lifecycle, offline and version-projection scenarios H2.2-H2.4 ask for, which need to assert on
//! a *sequence* of messages rather than a single round trip.
//!
//! # What this harness is for, and what it deliberately is not
//!
//! It answers charge-point-initiated calls from a script and records everything it receives, so a
//! test can assert the wire shape the charge point actually produced. It is **not** a CSMS
//! simulator: it does not validate payloads against the OCPP schema, and it does not model CSMS
//! behaviour. Its job is to be a truthful mirror - what went out on the socket, in order - because
//! that is the thing unit tests cannot see.
//!
//! Default replies exist for the messages every session sends (`BootNotification`, `Heartbeat`,
//! `StatusNotification`, `TransactionEvent`, …) so a test scripts only what it cares about. An
//! unscripted action still gets a valid empty CALLRESULT rather than silence: a charge point left
//! waiting on a response it will never get would fail the test slowly and for the wrong reason.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

/// One CALL the charge point sent, as it appeared on the wire.
#[derive(Debug, Clone)]
pub struct ReceivedCall {
    /// The OCPP action name, e.g. `"BootNotification"`.
    pub action: String,
    /// The request payload.
    pub payload: Value,
}

/// A running mock CSMS.
pub struct MockCsms {
    address: String,
    received: Arc<Mutex<Vec<ReceivedCall>>>,
    /// CSMS-initiated calls to push at the charge point.
    outbound: mpsc::UnboundedSender<Value>,
}

impl MockCsms {
    /// Binds a mock CSMS that negotiates `subprotocol` (`"ocpp2.1"`, `"ocpp2.0.1"`, `"ocpp1.6"`)
    /// and answers with `replies` - a list of `(action, response payload)` pairs consulted in
    /// order, falling back to a valid empty result.
    pub async fn start(subprotocol: &'static str, replies: Vec<(&'static str, Value)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("ws://{}", listener.local_addr().unwrap());
        let received = Arc::new(Mutex::new(Vec::new()));
        let (outbound, mut outbound_rx) = mpsc::unbounded_channel::<Value>();

        let recorded = received.clone();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_hdr_async(
                tcp,
                #[allow(clippy::result_large_err)]
                move |_req: &tokio_tungstenite::tungstenite::handshake::server::Request,
                      mut response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                    response
                        .headers_mut()
                        .insert("Sec-WebSocket-Protocol", subprotocol.parse().unwrap());
                    Ok(response)
                },
            )
            .await
            .unwrap();

            loop {
                tokio::select! {
                    // A CSMS-initiated call a test wants to push at the charge point.
                    Some(call) = outbound_rx.recv() => {
                        if ws.send(Message::text(call.to_string())).await.is_err() {
                            break;
                        }
                    }
                    frame = ws.next() => {
                        let Some(Ok(Message::Text(text))) = frame else { break };
                        let Ok(message) = serde_json::from_str::<Value>(&text) else { continue };
                        // OCPP-J: [2, id, action, payload] is a CALL; [3, id, payload] is the
                        // CALLRESULT to something this mock sent, which is recorded but needs no
                        // reply.
                        if message[0] == 2 {
                            let action = message[2].as_str().unwrap_or_default().to_string();
                            let id = message[1].as_str().unwrap_or_default().to_string();
                            recorded.lock().unwrap().push(ReceivedCall {
                                action: action.clone(),
                                payload: message[3].clone(),
                            });
                            let payload = replies
                                .iter()
                                .find(|(scripted, _)| *scripted == action)
                                .map(|(_, reply)| reply.clone())
                                .unwrap_or_else(|| default_reply(&action));
                            let result = json!([3, id, payload]);
                            if ws.send(Message::text(result.to_string())).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            }
        });

        Self {
            address,
            received,
            outbound,
        }
    }

    /// The `ws://` address to point a charge point at.
    pub fn address(&self) -> &str {
        &self.address
    }

    /// Every call received so far, in order.
    pub fn received(&self) -> Vec<ReceivedCall> {
        self.received.lock().unwrap().clone()
    }

    /// Just the action names, in order - what a sequence assertion usually wants.
    pub fn actions(&self) -> Vec<String> {
        self.received()
            .into_iter()
            .map(|call| call.action)
            .collect()
    }

    /// Waits up to two seconds for `action` to arrive and returns the first one.
    ///
    /// Bounded rather than unbounded for the reason [`crate`]'s A8 test gives: the failure this
    /// really guards is *silence*, and an unbounded wait would hang the suite instead of failing
    /// it with a usable message.
    pub async fn wait_for(&self, action: &str) -> ReceivedCall {
        for _ in 0..200 {
            if let Some(call) = self
                .received()
                .into_iter()
                .find(|call| call.action == action)
            {
                return call;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "timed out waiting for {action}; received: {:?}",
            self.actions()
        );
    }

    /// Pushes a CSMS-initiated CALL at the charge point.
    pub fn send_call(&self, id: &str, action: &str, payload: Value) {
        let _ = self.outbound.send(json!([2, id, action, payload]));
    }
}

/// A valid, empty-ish CALLRESULT for an action the test did not script.
///
/// Each of these is the minimum the corresponding response schema requires. Answering *something*
/// matters more than answering richly: a charge point blocked on a response it never receives
/// fails the test by timing out, which points at the harness rather than at the behaviour under
/// test.
fn default_reply(action: &str) -> Value {
    match action {
        "BootNotification" => json!({
            "currentTime": "2026-01-01T00:00:00Z",
            "interval": 300,
            "status": "Accepted"
        }),
        "Heartbeat" => json!({ "currentTime": "2026-01-01T00:00:00Z" }),
        "Authorize" => json!({ "idTokenInfo": { "status": "Accepted" } }),
        "TransactionEvent" => json!({}),
        "StatusNotification" => json!({}),
        "MeterValues" => json!({}),
        "SecurityEventNotification" => json!({}),
        "NotifyReport" => json!({}),
        "FirmwareStatusNotification" => json!({}),
        "LogStatusNotification" => json!({}),
        _ => json!({}),
    }
}
