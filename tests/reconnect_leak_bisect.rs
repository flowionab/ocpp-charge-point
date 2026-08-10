//! Root-causing H4.1's unresolved finding (`docs/PRODUCTION-ROADMAP.md` H4.3, part 2): at 400+
//! reconnect rounds, `tests/network_flapping_soak.rs`'s long mode shows retained memory growing by
//! roughly a few hundred bytes per reconnect.
//!
//! # The isolation this file performs
//!
//! `tests/network_flapping_soak.rs`'s long soak reconnects *and* drives transaction traffic in
//! the same rounds, so a reading taken there cannot tell "grows with reconnect count" from "grows
//! with transaction count" apart by itself - only by comparison with another file. Two other data
//! points already exist:
//!
//! - `tests/memory_growth.rs` (H4.2) drives 3,000 transactions with **zero** reconnects and finds
//!   ~0 net growth (115,015 B -> 115,112 B, 97 B of allocator noise).
//! - `tests/network_flapping_soak.rs`'s long soak drives ~4,800 reconnects *and* 1,250
//!   transactions per half and finds several hundred KB of growth per half.
//!
//! Comparing those two already makes transaction volume an unlikely sole cause (H4.2 ran more
//! transactions than either half of the soak and saw nothing). What was still missing was a
//! reading with reconnects *and no transactions at all*, to confirm the reconnect axis directly
//! rather than by elimination. That is what [`bare_reconnect_churn_grows_without_any_transaction_traffic`]
//! does: [`BootOnlyFlappingCsms`] answers `BootNotification` and then closes the socket
//! immediately - no `StatusNotification`, no `TransactionEvent`, no meter traffic, nothing this
//! crate's offline queues or security log ever see - so every single round is *pure* reconnect
//! churn: dial, register, get dropped, redial.
//!
//! Not run by `cargo test` for the same reason `network_flapping_soak.rs`'s long mode isn't: many
//! thousands of reconnects take real wall-clock seconds even at this file's fast backoff. Run
//! explicitly:
//!
//! ```text
//! cargo test --release --test reconnect_leak_bisect -- --ignored --nocapture
//! OCPP_RECONNECT_ROUNDS=4000 cargo test --release --test reconnect_leak_bisect -- --ignored --nocapture
//! ```
//!
//! # What the result means
//!
//! If retained memory keeps climbing here, at roughly the same per-reconnect rate H4.1 recorded,
//! that confirms the growth is intrinsic to the reconnect path itself (this crate's
//! `ConnectionTarget`/`TargetReconnector`, or `ocpp-client`'s `Client` reconnect loop) and has
//! nothing to do with transaction volume or the offline queue - narrowing the two remaining
//! candidates from H4.1's list of three (this crate's `ConnectionTarget`, `ocpp-client`'s
//! reconnect path, or benign allocator fragmentation from short-lived TCP connections) rather than
//! ruling any of them in specifically. See `docs/MEMORY.md` for the reading this run produced and
//! the conclusion drawn from it.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use ocpp_charge_point::connect_and_setup;
use ocpp_charge_point::executor::TokioExecutor;
use ocpp_charge_point::hardware::{
    Capabilities, ChargePoint, Connector, Evse, HardwareCommandReceiver, HardwareEventSender,
};
use ocpp_charge_point::provisioning::TokioBackoff;
use ocpp_client::{ConnectOptions, KeepaliveBehavior, ReconnectBehavior, ReconnectPolicy};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

static LIVE: AtomicUsize = AtomicUsize::new(0);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            LIVE.fetch_add(layout.size(), Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(pointer, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn live() -> usize {
    LIVE.load(Ordering::Relaxed)
}

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

fn fast_reconnect_options() -> ConnectOptions<'static> {
    ConnectOptions {
        timeout: Some(Duration::from_millis(200)),
        keepalive: KeepaliveBehavior::Disabled,
        reconnect: ReconnectBehavior::Enabled(ReconnectPolicy {
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
            multiplier: 2,
            jitter: false,
        }),
        ..Default::default()
    }
}

fn default_reply(action: &str) -> Value {
    match action {
        "BootNotification" => json!({
            "currentTime": "2026-01-01T00:00:00Z",
            "interval": 300,
            "status": "Accepted"
        }),
        _ => json!({}),
    }
}

/// A mock CSMS that answers exactly one `BootNotification` and then immediately closes the
/// socket - no other traffic is ever exchanged, so every accepted connection is pure reconnect
/// churn with nothing for this crate's offline queues, security log, or transaction machinery to
/// touch.
struct BootOnlyFlappingCsms {
    boots: Arc<AtomicUsize>,
    address: String,
}

impl BootOnlyFlappingCsms {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("ws://{}", listener.local_addr().unwrap());
        let boots = Arc::new(AtomicUsize::new(0));

        let boot_counter = boots.clone();
        tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    break;
                };
                let boot_counter = boot_counter.clone();
                tokio::spawn(async move {
                    let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(
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
                    else {
                        return;
                    };

                    // Answer exactly the first CALL (always BootNotification - nothing else has
                    // had a chance to be sent yet) and then drop the socket.
                    while let Some(Ok(Message::Text(text))) = ws.next().await {
                        let Ok(message) = serde_json::from_str::<Value>(&text) else {
                            continue;
                        };
                        if message[0] != 2 {
                            continue;
                        }
                        let action = message[2].as_str().unwrap_or_default().to_string();
                        let id = message[1].as_str().unwrap_or_default().to_string();
                        if action == "BootNotification" {
                            boot_counter.fetch_add(1, Ordering::SeqCst);
                        }
                        let reply = default_reply(&action);
                        let _ = ws
                            .send(Message::text(json!([3, id, reply]).to_string()))
                            .await;
                        break;
                    }
                    // Force-close: the read loop above sees EOF/error and redials.
                });
            }
        });

        Self { boots, address }
    }

    fn boot_count(&self) -> usize {
        self.boots.load(Ordering::SeqCst)
    }
}

async fn wait_until(max_wait: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let attempts = (max_wait.as_millis() / 2).max(1) as usize;
    for _ in 0..attempts {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    condition()
}

#[tokio::test]
#[ignore = "diagnostic for H4.3 part 2; run explicitly, see module docs"]
async fn bare_reconnect_churn_grows_without_any_transaction_traffic() {
    let rounds: usize = std::env::var("OCPP_RECONNECT_ROUNDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(4_000);

    let csms = BootOnlyFlappingCsms::start().await;
    let _runtime = connect_and_setup(
        charge_point(),
        &csms.address,
        Some(&[ocpp_charge_point::OcppVersion::V2_1]),
        Some(fast_reconnect_options()),
        None,
        TokioExecutor,
        TokioBackoff,
    )
    .await
    .expect("the initial connection should come up");

    assert!(
        wait_until(Duration::from_secs(5), || csms.boot_count() >= 1).await,
        "no initial BootNotification"
    );

    let warm_up = rounds / 2;
    wait_until(Duration::from_secs(300), || csms.boot_count() >= warm_up).await;
    let after_warm_up = live();
    let after_warm_up_boots = csms.boot_count();
    println!(
        "bare reconnect churn: after warm-up, {after_warm_up_boots} reconnects -> {after_warm_up} B"
    );

    // Deliberately NOT `assert!(reached `rounds` within the deadline)`. This is a diagnostic that
    // measures *bytes per reconnect*, so it does not need an exact reconnect count - and demanding
    // one inside a wall-clock deadline is precisely the flake mode H4.1's brief warns about. On a
    // loaded machine this loop reached 2,590 of 4,000 in 300 s and the assertion hard-failed after
    // ten minutes, reporting nothing at all: the measurement was thrown away because the *count*
    // fell short, even though the ratio it exists to compute was perfectly well-defined. Wait for
    // what we can get, then require only enough additional reconnects for the ratio to mean
    // something.
    wait_until(Duration::from_secs(300), || csms.boot_count() >= rounds).await;
    let after_follow_up = live();
    let after_follow_up_boots = csms.boot_count();

    let grown = after_follow_up.saturating_sub(after_warm_up);
    let reconnects = after_follow_up_boots.saturating_sub(after_warm_up_boots);
    assert!(
        reconnects >= warm_up / 4,
        "only {reconnects} further reconnects completed - too few for a meaningful \
         bytes-per-reconnect figure. Re-run on a less loaded machine, or lower OCPP_SOAK_ROUNDS."
    );
    println!(
        "bare reconnect churn: {after_warm_up_boots} reconnects -> {after_warm_up} B, \
         {after_follow_up_boots} reconnects -> {after_follow_up} B ({grown} B grown over \
         {reconnects} further reconnects with zero transaction traffic; {:.1} B/reconnect)",
        grown as f64 / reconnects.max(1) as f64
    );
}

// --- second isolation: bare `ocpp-client`, none of this crate's wiring at all -------------------

/// A [`Reconnector`] that redials through `ocpp_client::websocket_transport` directly - no
/// [`ocpp_charge_point::network_switch::ConnectionTarget`], no [`SizeLimitedStream`]
/// (`crate::payload_limit`), nothing this crate adds on top. If growth still shows up with this,
/// the leak cannot be in this crate's redial wrapping - it has to be in `ocpp-client` itself (its
/// `Client`'s reconnect loop, or the transport plumbing `websocket_transport` shares with it).
struct BareReconnector(String);

impl ocpp_client::Reconnector for BareReconnector {
    fn connect<'a>(
        &'a self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        (
                            Box<dyn ocpp_client::TransportSink>,
                            Box<dyn ocpp_client::TransportStream>,
                        ),
                        ocpp_client::TransportError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            ocpp_client::websocket_transport(&self.0, ocpp_client::OcppVersion::V2_1, None).await
        })
    }
}

/// Same measurement as [`bare_reconnect_churn_grows_without_any_transaction_traffic`], but built
/// directly on `ocpp_client::connect_2_1`/[`BareReconnector`] with no `ocpp_charge_point` code
/// running at all beyond the one `BootNotification` this test sends by hand after each reconnect
/// signal - narrowing "which crate" rather than "which layer of this crate".
#[tokio::test]
#[ignore = "diagnostic for H4.3 part 2; run explicitly, see module docs"]
async fn bare_ocpp_client_reconnect_churn_with_no_charge_point_crate_involved() {
    use ocpp_client::ocpp_2_1::BootNotification;
    use ocpp_client::ocpp_types::v21::BootNotificationRequest;
    use ocpp_client::ocpp_types::v21::common::{BootReasonEnum, ChargingStation};

    let rounds: usize = std::env::var("OCPP_RECONNECT_ROUNDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1_200);

    let csms = BootOnlyFlappingCsms::start().await;
    let options = fast_reconnect_options();
    let reconnector: Box<dyn ocpp_client::Reconnector> =
        Box::new(BareReconnector(csms.address.clone()));
    let client = match ocpp_client::connect_2_1(
        &csms.address,
        Some(ConnectOptions {
            reconnector: Some(std::sync::Arc::from(reconnector)),
            ..options
        }),
    )
    .await
    {
        Ok(client) => client,
        Err(err) => panic!("initial connection should come up: {err}"),
    };

    let boots = Arc::new(AtomicUsize::new(0));
    async fn send_boot(client: &ocpp_client::ocpp_2_1::OCPP2_1Client, boots: &Arc<AtomicUsize>) {
        let _ = client
            .call::<BootNotification>(BootNotificationRequest {
                charging_station: ChargingStation {
                    custom_data: None,
                    firmware_version: None,
                    model: heapless::String::try_from("Charger 9000").unwrap(),
                    modem: None,
                    serial_number: None,
                    vendor_name: heapless::String::try_from("Acme").unwrap(),
                },
                reason: BootReasonEnum::PowerUp,
                custom_data: None,
            })
            .await;
        boots.fetch_add(1, Ordering::SeqCst);
    }
    send_boot(&client, &boots).await;

    {
        let client = client.clone();
        let boots = boots.clone();
        ocpp_client::Client::on_reconnect(&client, move |client| {
            let boots = boots.clone();
            async move {
                send_boot(&client, &boots).await;
            }
        })
        .await;
    }

    let count = || boots.load(Ordering::SeqCst);
    let warm_up = rounds / 2;
    wait_until(Duration::from_secs(300), || count() >= warm_up).await;
    let after_warm_up = live();
    let after_warm_up_boots = count();
    println!(
        "bare ocpp-client reconnect churn: after warm-up, {after_warm_up_boots} reconnects -> \
         {after_warm_up} B"
    );

    assert!(
        wait_until(Duration::from_secs(300), || count() >= rounds).await,
        "follow-up did not reach {rounds} reconnects (stuck at {})",
        count()
    );
    let after_follow_up = live();
    let after_follow_up_boots = count();

    let grown = after_follow_up.saturating_sub(after_warm_up);
    let reconnects = after_follow_up_boots.saturating_sub(after_warm_up_boots);
    println!(
        "bare ocpp-client reconnect churn: {after_warm_up_boots} reconnects -> {after_warm_up} B, \
         {after_follow_up_boots} reconnects -> {after_follow_up} B ({grown} B grown over \
         {reconnects} further reconnects, no ocpp_charge_point code involved; {:.1} B/reconnect)",
        grown as f64 / reconnects.max(1) as f64
    );
}
