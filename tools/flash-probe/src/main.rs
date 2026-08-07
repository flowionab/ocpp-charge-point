//! A bare-metal firmware image that exercises this crate, built once per Cargo feature set so the
//! flash cost of each feature can be measured rather than guessed - G2.4 in
//! `docs/PRODUCTION-ROADMAP.md` §9.2. Driven by `scripts/flash-cost.sh`; results live in
//! `docs/MEMORY.md`.
//!
//! # Why it looks like this
//!
//! The measurement is of a *linked* image for `thumbv7em-none-eabihf` with LTO and
//! `--gc-sections`, because that is what an integrator actually flashes: code the firmware can't
//! reach is code it doesn't pay for. That has one consequence that shapes this whole file - **a
//! feature whose code the probe never calls would be stripped and would measure as free**. So for
//! every feature it enables, the probe reaches the code that feature gates: it constructs a real
//! `ocpp-client` client per protocol version over a null transport, registers every inbound handler
//! this crate provides for that version, and spawns every outbound forwarding loop.
//!
//! `_start` never runs (there is no hardware here, and no reset vector) - it exists so the linker
//! has an entry point and a reachability root. Futures handed to [`Probe`]'s
//! [`Executor`](ocpp_charge_point::executor::Executor) are **polled once** with a no-op waker rather
//! than dropped, because merely storing a spawned future is not enough to keep it: an earlier
//! version of this probe escaped the boxed future's address through a `static` and measured 60 bytes
//! total, since casting the fat pointer to a thin one discarded the vtable and LTO then proved every
//! future body unreachable. Polling makes `poll` - and therefore the entire async call graph being
//! measured - unavoidably reachable.
//!
//! # What the numbers include, and don't
//!
//! Included: this crate's code for the enabled features, the `ocpp-client` code it calls into, and
//! the serde/`ocpp-types` machinery the wire adapters pull in - i.e. everything a charge point needs
//! above the transport.
//!
//! Not included: a real transport (this one is a stub that never sends), TLS, a real `Executor`
//! (embassy or otherwise), a panic handler that does anything, and the reset vector/startup code
//! `cortex-m-rt` would add. Those are the integrator's, and they are why the absolute figures are a
//! floor for the application layer rather than a whole firmware's size.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::boxed::Box;
use core::alloc::{GlobalAlloc, Layout};
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicUsize, Ordering};

use ocpp_charge_point::actor::ChargePointActor;
use ocpp_charge_point::clock::{Clock, MonotonicClock, MonotonicInstant};
use ocpp_charge_point::executor::Executor;
use ocpp_charge_point::hardware::{
    Capabilities, ChargePoint, Connector, Evse, HardwareCommandReceiver, HardwareEventSender,
};
use ocpp_charge_point::provisioning::Backoff;
use ocpp_charge_point::state::StateLimits;

/// Connectors per EVSE for the measured charge point - one EVSE with two connectors, the shape most
/// AC wallboxes ship.
const CONNECTOR_COUNTS: [usize; 1] = [2];

// ---------------------------------------------------------------------------
// The bare-metal scaffolding a `no_std` binary needs, none of it measured on purpose.
// ---------------------------------------------------------------------------

#[panic_handler]
#[allow(clippy::empty_loop)]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

/// A bump allocator over a small static arena. `alloc` needs *an* allocator to link, and it has to
/// be one the optimizer cannot prove always fails: an earlier version of this probe returned
/// `null` unconditionally, which let LLVM fold every allocation into an abort and every caller into
/// unreachable code - the whole image measured 60 bytes. Its own code is a few dozen bytes and is
/// included in the figures; a real integrator brings a proper heap (`embedded-alloc` or similar),
/// whose cost is not counted here.
struct BumpHeap;

/// Arena the bump allocator hands out. Never actually touched - `_start` never runs - so its size
/// only has to be plausible; it lands in `.bss`, not in the flash image.
static mut ARENA: [u8; 64 * 1024] = [0; 64 * 1024];
static NEXT: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for BumpHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let base = (&raw mut ARENA) as *mut u8;
        let mut offset = NEXT.load(Ordering::Relaxed);
        offset = (offset + layout.align() - 1) & !(layout.align() - 1);
        let end = offset + layout.size();
        if end > 64 * 1024 {
            return core::ptr::null_mut();
        }
        NEXT.store(end, Ordering::Relaxed);
        unsafe { base.add(offset) }
    }

    /// A bump allocator never reclaims. Fine here: nothing runs.
    unsafe fn dealloc(&self, _pointer: *mut u8, _layout: Layout) {}
}

#[global_allocator]
static ALLOCATOR: BumpHeap = BumpHeap;

/// The `getrandom` custom backend the `getrandom_backend="custom"` cfg in `.cargo/config.toml`
/// promises the linker. `getrandom` is reached transitively (`ocpp-client` -> `uuid`'s `v4`
/// feature) and has no entropy source on a bare-metal target, so the firmware must supply one -
/// CI's `embedded` job sets the same cfg for `cargo check`. **This stub returns zeros and is not
/// random**: it exists so the image links and can be measured. A real integrator wires this to an
/// RNG peripheral.
#[unsafe(no_mangle)]
unsafe extern "Rust" fn __getrandom_v03_custom(
    dest: *mut u8,
    len: usize,
) -> Result<(), getrandom::Error> {
    unsafe { core::ptr::write_bytes(dest, 0, len) };
    Ok(())
}

/// A `critical-section` backend, required because this crate's channels are `embassy-sync`-backed
/// (see `src/sync.rs`). Single-core, never-actually-executed stand-in for the real
/// interrupt-disabling implementation an integrator registers.
struct SingleCoreCriticalSection;
critical_section::set_impl!(SingleCoreCriticalSection);

unsafe impl critical_section::Impl for SingleCoreCriticalSection {
    unsafe fn acquire() -> bool {
        false
    }

    unsafe fn release(_was_active: bool) {}
}

// ---------------------------------------------------------------------------
// The integrator-supplied abstractions: executor, backoff, clocks, hardware.
// ---------------------------------------------------------------------------

/// Sink for each polled future's readiness, so the poll call itself can't be optimized away as
/// having no effect - see the module docs.
static mut POLLED: bool = false;

/// A waker that does nothing, for [`poll_once`]. Never actually woken: the probe's futures are
/// polled exactly once, at a point in the program that never executes, purely so their code is
/// emitted and linked.
fn noop_waker() -> core::task::Waker {
    use core::task::{RawWaker, RawWakerVTable, Waker};

    unsafe fn clone(_data: *const ()) -> RawWaker {
        RawWaker::new(core::ptr::null(), &VTABLE)
    }
    unsafe fn noop(_data: *const ()) {}

    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);

    unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) }
}

/// Polls `future` once with [`noop_waker`], recording that it was polled through a `volatile` write
/// so the call has an observable effect. The future is deliberately leaked afterwards: dropping it
/// here would be fine too, but leaking keeps the drop glue for a spawned task out of the
/// measurement, which belongs to whatever real executor the integrator brings.
fn poll_once(future: Pin<Box<dyn Future<Output = ()> + Send>>) {
    let mut future = future;
    let waker = noop_waker();
    let mut context = core::task::Context::from_waker(&waker);
    let ready = future.as_mut().poll(&mut context).is_ready();
    unsafe { core::ptr::write_volatile(&raw mut POLLED, ready) };
    core::mem::forget(future);
}

/// Stands in for everything an integrator supplies: `Executor`, `Backoff`, `Clock`,
/// `MonotonicClock`.
#[derive(Clone, Copy)]
struct Probe;

impl Executor for Probe {
    fn spawn(&self, future: Pin<Box<dyn Future<Output = ()> + Send>>) {
        poll_once(future);
    }
}

#[async_trait::async_trait]
impl Backoff for Probe {
    async fn wait(&self, _seconds: u32) {}
}

impl Clock for Probe {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(0, 0).unwrap_or_default()
    }
}

impl MonotonicClock for Probe {
    fn now(&self) -> MonotonicInstant {
        MonotonicInstant::from_ticks(0)
    }
}

struct ProbeChargePoint {
    evses: [ProbeEvse; 1],
}

struct ProbeEvse {
    connectors: [ProbeConnector; 2],
}

struct ProbeConnector;

#[async_trait::async_trait]
impl ChargePoint<ProbeEvse, ProbeConnector> for ProbeChargePoint {
    type StartError = core::convert::Infallible;

    async fn vendor_name(&self) -> &str {
        "Flash"
    }

    async fn model_name(&self) -> &str {
        "Probe"
    }

    async fn evses(&self) -> &[ProbeEvse] {
        &self.evses
    }

    async fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }

    async fn start(
        &self,
        events: HardwareEventSender,
        mut commands: HardwareCommandReceiver,
    ) -> Result<(), Self::StartError> {
        // The real dispatch path, so `execute_hardware_command` and everything it reaches is
        // counted.
        while let Ok(command) = commands.recv().await {
            ocpp_charge_point::hardware::execute_hardware_command(&self.evses, command, &events)
                .await;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Evse<ProbeConnector> for ProbeEvse {
    type Error = core::convert::Infallible;

    async fn connectors(&self) -> &[ProbeConnector] {
        &self.connectors
    }

    async fn reboot(&self) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl Connector for ProbeConnector {
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

// ---------------------------------------------------------------------------
// A null transport, so a real `ocpp-client` client can be constructed without a network.
// ---------------------------------------------------------------------------

#[cfg(any(feature = "ocpp_1_6", feature = "ocpp_2_0_1", feature = "ocpp_2_1"))]
mod transport {
    use alloc::boxed::Box;
    use alloc::string::String;
    use core::future::Future;
    use core::pin::Pin;
    use core::time::Duration;

    use ocpp_client::{TransportError, TransportEvent, TransportSink, TransportStream};

    /// A transport that accepts every frame and never yields one. Enough to construct a client; not
    /// enough to talk to anything, which is the point - a real transport's code is the
    /// integrator's, not this crate's.
    pub struct NullSink;
    pub struct NullStream;

    impl TransportSink for NullSink {
        fn send<'a>(
            &'a mut self,
            _frame: String,
        ) -> Pin<Box<dyn Future<Output = Result<(), TransportError>> + Send + 'a>> {
            Box::pin(async { Ok(()) })
        }

        fn ping<'a>(
            &'a mut self,
        ) -> Pin<Box<dyn Future<Output = Result<(), TransportError>> + Send + 'a>> {
            Box::pin(async { Ok(()) })
        }

        fn pong<'a>(
            &'a mut self,
        ) -> Pin<Box<dyn Future<Output = Result<(), TransportError>> + Send + 'a>> {
            Box::pin(async { Ok(()) })
        }

        fn close<'a>(
            &'a mut self,
        ) -> Pin<Box<dyn Future<Output = Result<(), TransportError>> + Send + 'a>> {
            Box::pin(async { Ok(()) })
        }
    }

    impl TransportStream for NullStream {
        fn recv<'a>(
            &'a mut self,
        ) -> Pin<Box<dyn Future<Output = Result<Option<TransportEvent>, TransportError>> + Send + 'a>>
        {
            Box::pin(async { Ok(None) })
        }
    }

    /// `ocpp-client`'s own `Executor`/`Timer`, same escaping trick as the charge point's.
    pub struct ClientRuntime;

    impl ocpp_client::runtime::Executor for ClientRuntime {
        fn spawn(&self, future: Pin<Box<dyn Future<Output = ()> + Send>>) {
            super::poll_once(future);
        }
    }

    impl ocpp_client::runtime::Timer for ClientRuntime {
        fn delay<'a>(
            &'a self,
            _duration: Duration,
        ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
            Box::pin(async {})
        }
    }

    /// The timeout a client is built with - irrelevant to code size, but a client needs one.
    pub const TIMEOUT: Duration = Duration::from_secs(30);
}

// ---------------------------------------------------------------------------
// The measured work: one function per protocol version, plus the version-independent core.
// ---------------------------------------------------------------------------

/// Wires everything that exists regardless of protocol version: the actor, the hardware binding, and
/// the state machine driven through a representative event.
fn core_charge_point() -> ChargePointActor {
    let actor =
        ChargePointActor::spawn_with_limits(CONNECTOR_COUNTS, &Probe, StateLimits::default());

    // A second root for the hardware side. `HardwareEventSender`/`HardwareCommandReceiver` are
    // handed to a binding by the crate itself rather than constructed by hand, so the only way to
    // reach `ChargePoint::start` - and through it `execute_hardware_command` and the whole command
    // dispatch path - is the way an integrator does it: `ChargePointBuilder::start`. Same
    // monomorphizations as the actor above, so nothing is counted twice.
    Probe.spawn(Box::pin(async move {
        let hardware = ProbeChargePoint {
            evses: [ProbeEvse {
                connectors: [ProbeConnector, ProbeConnector],
            }],
        };
        let Ok(builder) = ocpp_charge_point::ChargePointBuilder::start_with_limits(
            hardware,
            Probe,
            StateLimits::default(),
        )
        .await;
        let runtime = builder.build();
        let _ = runtime
            .send(ocpp_charge_point::state::ChargePointEvent::BootCompleted)
            .await;
    }));

    let feed = actor.clone();
    Probe.spawn(Box::pin(async move {
        let _ = feed
            .send(ocpp_charge_point::state::ChargePointEvent::Evse {
                evse_id: 0,
                event: ocpp_charge_point::state::EvseEvent::Connector {
                    connector_id: 0,
                    event: ocpp_charge_point::state::ConnectorEvent::CableConnected,
                },
            })
            .await;
    }));

    actor
}

/// Registers every OCPP 2.1 handler and forwarding loop this crate provides, so the 2.1 adapters
/// are reachable and therefore measured.
#[cfg(feature = "ocpp_2_1")]
fn wire_ocpp_2_1(actor: &ChargePointActor) {
    use ocpp_charge_point::availability::{ChangeAvailabilityHandler, Ocpp2_1StatusNotifier};
    use ocpp_charge_point::device_model::{GetVariablesHandler, SetVariablesHandler};
    use ocpp_charge_point::remote_control::{
        RequestStartTransactionHandler, RequestStopTransactionHandler, UnlockConnectorHandler,
    };
    use ocpp_charge_point::reporting::{GetBaseReportHandler, GetReportHandler};
    use ocpp_charge_point::reset::ResetHandler;
    use ocpp_charge_point::security::Ocpp2_1SecurityEventNotifier;
    use ocpp_charge_point::transactions::Ocpp2_1TransactionNotifier;

    let client = ocpp_client::ocpp_2_1::OCPP2_1Client::from_transport(
        Box::new(transport::NullSink),
        Box::new(transport::NullStream),
        transport::TIMEOUT,
        Box::new(transport::ClientRuntime),
        Box::new(transport::ClientRuntime),
    );

    let status = Ocpp2_1StatusNotifier::with_clock(client.clone(), Probe);
    let transactions = Ocpp2_1TransactionNotifier::with_clock(client.clone(), Probe);
    let security = Ocpp2_1SecurityEventNotifier::with_clock(client.clone(), Probe);

    let status_changes = actor.subscribe_status_notifications();
    Probe.spawn(Box::pin(async move {
        ocpp_charge_point::availability::run_status_notifications(status_changes, &status).await;
    }));

    let transaction_events = actor.subscribe_transaction_events();
    Probe.spawn(Box::pin(async move {
        ocpp_charge_point::transactions::run_transaction_events(transaction_events, &transactions)
            .await;
    }));

    let security_events = actor.subscribe_security_events();
    Probe.spawn(Box::pin(async move {
        ocpp_charge_point::security::run_security_events(security_events, &security).await;
    }));

    let authorization_requests = actor.subscribe_authorization_requests();
    let authorizer = client.clone();
    let authorization_actor = actor.clone();
    Probe.spawn(Box::pin(async move {
        ocpp_charge_point::authorization::run_authorization_requests(
            authorization_requests,
            &authorizer,
            authorization_actor,
        )
        .await;
    }));

    let boot = client.clone();
    let boot_actor = actor.clone();
    Probe.spawn(Box::pin(async move {
        ocpp_charge_point::provisioning::register_until_accepted(
            &boot_actor,
            &boot,
            &Probe,
            &Probe,
            "Flash",
            "Probe",
            None,
        )
        .await;
        ocpp_charge_point::provisioning::run_heartbeat(&boot, &Probe, &Probe, &boot_actor, 300)
            .await;
    }));

    let handlers = client.clone();
    let handler_actor = actor.clone();
    Probe.spawn(Box::pin(async move {
        handlers
            .register_change_availability_handler(handler_actor.clone())
            .await;
        handlers.register_reset_handler(handler_actor.clone()).await;
        handlers
            .register_unlock_connector_handler(handler_actor.clone())
            .await;
        handlers
            .register_request_start_transaction_handler(handler_actor.clone())
            .await;
        handlers
            .register_request_stop_transaction_handler(handler_actor.clone())
            .await;
        handlers
            .register_get_variables_handler(handler_actor.clone())
            .await;
        handlers
            .register_set_variables_handler(handler_actor.clone())
            .await;
        ocpp_charge_point::reporting::Ocpp2_1ReportHandler::with_clock(handlers.clone(), Probe)
            .register_get_base_report_handler(handler_actor.clone())
            .await;
        ocpp_charge_point::reporting::Ocpp2_1ReportHandler::with_clock(handlers.clone(), Probe)
            .register_get_report_handler(handler_actor.clone())
            .await;
        #[cfg(feature = "reservation")]
        {
            use ocpp_charge_point::reservation::{CancelReservationHandler, ReserveNowHandler};
            handlers
                .register_reserve_now_handler(handler_actor.clone())
                .await;
            handlers
                .register_cancel_reservation_handler(handler_actor.clone())
                .await;
        }
        #[cfg(feature = "local-auth-list")]
        {
            use ocpp_charge_point::local_authorization_list::{
                GetLocalListVersionHandler, SendLocalListHandler,
            };
            handlers
                .register_send_local_list_handler(handler_actor.clone())
                .await;
            handlers
                .register_get_local_list_version_handler(handler_actor.clone())
                .await;
        }
        #[cfg(feature = "tariff-cost")]
        {
            use ocpp_charge_point::cost::CostUpdatedHandler;
            handlers
                .register_cost_updated_handler(handler_actor.clone())
                .await;
        }
    }));
}

/// The OCPP 2.0.1 counterpart of [`wire_ocpp_2_1`].
#[cfg(feature = "ocpp_2_0_1")]
fn wire_ocpp_2_0_1(actor: &ChargePointActor) {
    use ocpp_charge_point::availability::{ChangeAvailabilityHandler, Ocpp2_0_1StatusNotifier};
    use ocpp_charge_point::device_model::{GetVariablesHandler, SetVariablesHandler};
    use ocpp_charge_point::remote_control::{
        RequestStartTransactionHandler, RequestStopTransactionHandler, UnlockConnectorHandler,
    };
    use ocpp_charge_point::reporting::{GetBaseReportHandler, GetReportHandler};
    use ocpp_charge_point::reset::ResetHandler;
    use ocpp_charge_point::transactions::Ocpp2_0_1TransactionNotifier;

    let client = ocpp_client::ocpp_2_0_1::OCPP2_0_1Client::from_transport(
        Box::new(transport::NullSink),
        Box::new(transport::NullStream),
        transport::TIMEOUT,
        Box::new(transport::ClientRuntime),
        Box::new(transport::ClientRuntime),
    );

    let status = Ocpp2_0_1StatusNotifier::with_clock(client.clone(), Probe);
    let transactions = Ocpp2_0_1TransactionNotifier::with_clock(client.clone(), Probe);

    let status_changes = actor.subscribe_status_notifications();
    Probe.spawn(Box::pin(async move {
        ocpp_charge_point::availability::run_status_notifications(status_changes, &status).await;
    }));

    let transaction_events = actor.subscribe_transaction_events();
    Probe.spawn(Box::pin(async move {
        ocpp_charge_point::transactions::run_transaction_events(transaction_events, &transactions)
            .await;
    }));

    let authorization_requests = actor.subscribe_authorization_requests();
    let authorizer = client.clone();
    let authorization_actor = actor.clone();
    Probe.spawn(Box::pin(async move {
        ocpp_charge_point::authorization::run_authorization_requests(
            authorization_requests,
            &authorizer,
            authorization_actor,
        )
        .await;
    }));

    let boot = client.clone();
    let boot_actor = actor.clone();
    Probe.spawn(Box::pin(async move {
        ocpp_charge_point::provisioning::register_until_accepted(
            &boot_actor,
            &boot,
            &Probe,
            &Probe,
            "Flash",
            "Probe",
            None,
        )
        .await;
        ocpp_charge_point::provisioning::run_heartbeat(&boot, &Probe, &Probe, &boot_actor, 300)
            .await;
    }));

    let handlers = client.clone();
    let handler_actor = actor.clone();
    Probe.spawn(Box::pin(async move {
        handlers
            .register_change_availability_handler(handler_actor.clone())
            .await;
        handlers.register_reset_handler(handler_actor.clone()).await;
        handlers
            .register_unlock_connector_handler(handler_actor.clone())
            .await;
        handlers
            .register_request_start_transaction_handler(handler_actor.clone())
            .await;
        handlers
            .register_request_stop_transaction_handler(handler_actor.clone())
            .await;
        handlers
            .register_get_variables_handler(handler_actor.clone())
            .await;
        handlers
            .register_set_variables_handler(handler_actor.clone())
            .await;
        ocpp_charge_point::reporting::Ocpp2_0_1ReportHandler::with_clock(handlers.clone(), Probe)
            .register_get_base_report_handler(handler_actor.clone())
            .await;
        ocpp_charge_point::reporting::Ocpp2_0_1ReportHandler::with_clock(handlers.clone(), Probe)
            .register_get_report_handler(handler_actor.clone())
            .await;
        #[cfg(feature = "reservation")]
        {
            use ocpp_charge_point::reservation::{CancelReservationHandler, ReserveNowHandler};
            handlers
                .register_reserve_now_handler(handler_actor.clone())
                .await;
            handlers
                .register_cancel_reservation_handler(handler_actor.clone())
                .await;
        }
        #[cfg(feature = "local-auth-list")]
        {
            use ocpp_charge_point::local_authorization_list::{
                GetLocalListVersionHandler, SendLocalListHandler,
            };
            handlers
                .register_send_local_list_handler(handler_actor.clone())
                .await;
            handlers
                .register_get_local_list_version_handler(handler_actor.clone())
                .await;
        }
    }));
}

/// The OCPP 1.6J counterpart of [`wire_ocpp_2_1`]. 1.6J's notifiers additionally need the connector
/// topology, since the wire protocol addresses connectors by a flat id (see `crate::topology`).
#[cfg(feature = "ocpp_1_6")]
fn wire_ocpp_1_6(actor: &ChargePointActor) {
    use ocpp_charge_point::availability::{ChangeAvailabilityHandler, Ocpp1_6StatusNotifier};
    use ocpp_charge_point::device_model::{GetVariablesHandler, SetVariablesHandler};
    use ocpp_charge_point::remote_control::{
        RequestStartTransactionHandler, RequestStopTransactionHandler, UnlockConnectorHandler,
    };
    use ocpp_charge_point::reset::ResetHandler;
    use ocpp_charge_point::transactions::Ocpp1_6TransactionNotifier;

    let client = ocpp_client::ocpp_1_6::OCPP1_6Client::from_transport(
        Box::new(transport::NullSink),
        Box::new(transport::NullStream),
        transport::TIMEOUT,
        Box::new(transport::ClientRuntime),
        Box::new(transport::ClientRuntime),
    );

    let counts: alloc::vec::Vec<usize> = CONNECTOR_COUNTS.to_vec();
    let status = Ocpp1_6StatusNotifier::new(client.clone(), counts.clone());
    let transactions =
        Ocpp1_6TransactionNotifier::with_clock(client.clone(), counts.clone(), Probe);

    let status_changes = actor.subscribe_status_notifications();
    Probe.spawn(Box::pin(async move {
        ocpp_charge_point::availability::run_status_notifications(status_changes, &status).await;
    }));

    let transaction_events = actor.subscribe_transaction_events();
    Probe.spawn(Box::pin(async move {
        ocpp_charge_point::transactions::run_transaction_events(transaction_events, &transactions)
            .await;
    }));

    let authorization_requests = actor.subscribe_authorization_requests();
    let authorizer = client.clone();
    let authorization_actor = actor.clone();
    Probe.spawn(Box::pin(async move {
        ocpp_charge_point::authorization::run_authorization_requests(
            authorization_requests,
            &authorizer,
            authorization_actor,
        )
        .await;
    }));

    let boot = client.clone();
    let boot_actor = actor.clone();
    Probe.spawn(Box::pin(async move {
        ocpp_charge_point::provisioning::register_until_accepted(
            &boot_actor,
            &boot,
            &Probe,
            &Probe,
            "Flash",
            "Probe",
            None,
        )
        .await;
        ocpp_charge_point::provisioning::run_heartbeat(&boot, &Probe, &Probe, &boot_actor, 300)
            .await;
    }));

    let handlers = client.clone();
    let handler_actor = actor.clone();
    let handler_counts = counts.clone();
    Probe.spawn(Box::pin(async move {
        ocpp_charge_point::availability::Ocpp1_6ChangeAvailabilityHandler::new(
            handlers.clone(),
            handler_counts.clone(),
        )
        .register_change_availability_handler(handler_actor.clone())
        .await;
        handlers.register_reset_handler(handler_actor.clone()).await;
        // 1.6J addresses connectors by a flat wire id, so the handlers that need to translate one
        // are wrappers carrying the topology rather than impls on the bare client - see
        // `crate::topology`.
        let remote = ocpp_charge_point::remote_control::Ocpp1_6RemoteControlHandler::new(
            handlers.clone(),
            handler_counts.clone(),
        );
        remote
            .register_unlock_connector_handler(handler_actor.clone())
            .await;
        remote
            .register_request_start_transaction_handler(handler_actor.clone())
            .await;
        handlers
            .register_request_stop_transaction_handler(handler_actor.clone())
            .await;
        handlers
            .register_get_variables_handler(handler_actor.clone())
            .await;
        handlers
            .register_set_variables_handler(handler_actor.clone())
            .await;
        #[cfg(feature = "reservation")]
        {
            use ocpp_charge_point::reservation::{CancelReservationHandler, ReserveNowHandler};
            ocpp_charge_point::reservation::Ocpp1_6ReserveNowHandler::new(
                handlers.clone(),
                handler_counts.clone(),
            )
            .register_reserve_now_handler(handler_actor.clone())
            .await;
            handlers
                .register_cancel_reservation_handler(handler_actor.clone())
                .await;
        }
        #[cfg(feature = "local-auth-list")]
        {
            use ocpp_charge_point::local_authorization_list::{
                GetLocalListVersionHandler, SendLocalListHandler,
            };
            handlers
                .register_send_local_list_handler(handler_actor.clone())
                .await;
            handlers
                .register_get_local_list_version_handler(handler_actor.clone())
                .await;
        }
    }));
}

/// Reaches the version-independent half of each capability-gated functional block, so a feature
/// still shows a cost when measured without any protocol version enabled.
fn wire_capability_blocks(actor: &ChargePointActor) {
    let _ = actor;
    #[cfg(feature = "reservation")]
    {
        let actor = actor.clone();
        Probe.spawn(Box::pin(async move {
            let _ = ocpp_charge_point::reservation::handle_reserve_now(
                &actor,
                Some(0),
                ocpp_charge_point::state::ReservationId(1),
                ocpp_charge_point::state::IdToken {
                    value: alloc::string::String::new(),
                    kind: ocpp_charge_point::state::IdTokenKind::ISO14443,
                },
                None,
            )
            .await;
            let _ = ocpp_charge_point::reservation::handle_cancel_reservation(
                &actor,
                ocpp_charge_point::state::ReservationId(1),
            )
            .await;
        }));
    }
    #[cfg(feature = "local-auth-list")]
    {
        let actor = actor.clone();
        Probe.spawn(Box::pin(async move {
            let _ = ocpp_charge_point::local_authorization_list::handle_send_local_list(
                &actor,
                1,
                ocpp_charge_point::local_authorization_list::LocalListUpdate::Full(
                    alloc::vec::Vec::new(),
                ),
            )
            .await;
            let _ =
                ocpp_charge_point::local_authorization_list::handle_get_local_list_version(&actor);
        }));
    }
    #[cfg(feature = "tariff-cost")]
    {
        let actor = actor.clone();
        Probe.spawn(Box::pin(async move {
            let _ = ocpp_charge_point::cost::handle_cost_updated(
                &actor,
                ocpp_charge_point::state::TransactionId(0),
                1.0,
            )
            .await;
        }));
    }
}

/// The linker's entry point and the reachability root for everything measured. Never executed -
/// there is no reset vector here and no hardware to run on.
#[unsafe(no_mangle)]
// `loop {}` is exactly right for a never-reached bare-metal entry point; there is no thread to
// sleep and nothing to panic about.
#[allow(clippy::empty_loop)]
pub extern "C" fn _start() -> ! {
    let actor = core_charge_point();
    wire_capability_blocks(&actor);
    #[cfg(feature = "ocpp_2_1")]
    wire_ocpp_2_1(&actor);
    #[cfg(feature = "ocpp_2_0_1")]
    wire_ocpp_2_0_1(&actor);
    #[cfg(feature = "ocpp_1_6")]
    wire_ocpp_1_6(&actor);
    loop {}
}
