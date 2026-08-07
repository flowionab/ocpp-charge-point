use ocpp_charge_point::authorization::Authorizer;
use ocpp_charge_point::availability::{ChangeAvailabilityHandler, StatusNotifier};
use ocpp_charge_point::connection::ReconnectHandler;
use ocpp_charge_point::cost::CostUpdatedHandler;
use ocpp_charge_point::device_model::{GetVariablesHandler, SetVariablesHandler};
use ocpp_charge_point::executor::TokioExecutor;
use ocpp_charge_point::hardware::{ChargePoint, Connector, Evse};
use ocpp_charge_point::local_authorization_list::{
    GetLocalListVersionHandler, SendLocalListHandler,
};
use ocpp_charge_point::provisioning::{
    BootNotificationOutcome, BootNotifier, HeartbeatSender, TokioBackoff,
};
use ocpp_charge_point::remote_control::{
    RequestStartTransactionHandler, RequestStopTransactionHandler, UnlockConnectorHandler,
};
use ocpp_charge_point::reporting::{GetBaseReportHandler, GetReportHandler};
use ocpp_charge_point::reservation::{CancelReservationHandler, ReserveNowHandler};
use ocpp_charge_point::reset::ResetHandler;
use ocpp_charge_point::security::SecurityEventNotifier;
use ocpp_charge_point::setup;
use ocpp_charge_point::smart_charging::{
    ChargingLimitProjection, ClearChargingProfileHandler, GetCompositeScheduleHandler,
    SetChargingProfileHandler,
};
use ocpp_charge_point::state::{
    AuthorizationStatus, BootReasonCause, ChargePointEvent, ConnectorEvent, ConnectorStatus,
    EvseEvent, IdToken, RegistrationStatus, Transaction, TransactionEventKind,
};
use ocpp_charge_point::transactions::TransactionNotifier;

/// A stand-in for a real CSMS connection. Real deployments pass an `ocpp-client` version
/// client (e.g. `ocpp_client::connect_2_1`) instead, which already implements `BootNotifier`,
/// `HeartbeatSender`, `StatusNotifier`, `TransactionNotifier`, and `Authorizer`.
#[derive(Clone)]
struct AlwaysAcceptBootNotifier;

#[async_trait::async_trait]
impl BootNotifier for AlwaysAcceptBootNotifier {
    type Error = core::convert::Infallible;

    async fn notify_boot(
        &self,
        _vendor_name: &str,
        _model_name: &str,
        _reason: Option<BootReasonCause>,
    ) -> Result<BootNotificationOutcome, Self::Error> {
        Ok(BootNotificationOutcome {
            status: RegistrationStatus::Accepted,
            interval_secs: 300,
            current_time: None,
        })
    }
}

#[async_trait::async_trait]
impl HeartbeatSender for AlwaysAcceptBootNotifier {
    type Error = core::convert::Infallible;

    async fn send_heartbeat(&self) -> Result<Option<chrono::DateTime<chrono::Utc>>, Self::Error> {
        Ok(None)
    }
}

#[async_trait::async_trait]
impl StatusNotifier for AlwaysAcceptBootNotifier {
    type Error = core::convert::Infallible;

    async fn notify_status(
        &self,
        _evse_id: usize,
        _connector_id: usize,
        _status: ConnectorStatus,
        _connector_state: ocpp_charge_point::state::ConnectorState,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl TransactionNotifier for AlwaysAcceptBootNotifier {
    type Error = core::convert::Infallible;

    async fn notify_transaction_event(
        &self,
        _evse_id: usize,
        _connector_id: usize,
        _kind: TransactionEventKind,
        _transaction: Transaction,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl Authorizer for AlwaysAcceptBootNotifier {
    type Error = core::convert::Infallible;

    async fn authorize(&self, _id_token: &IdToken) -> Result<AuthorizationStatus, Self::Error> {
        Ok(AuthorizationStatus::Accepted)
    }
}

#[async_trait::async_trait]
impl UnlockConnectorHandler for AlwaysAcceptBootNotifier {
    async fn register_unlock_connector_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl ChangeAvailabilityHandler for AlwaysAcceptBootNotifier {
    async fn register_change_availability_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl RequestStartTransactionHandler for AlwaysAcceptBootNotifier {
    async fn register_request_start_transaction_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl RequestStopTransactionHandler for AlwaysAcceptBootNotifier {
    async fn register_request_stop_transaction_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl ReserveNowHandler for AlwaysAcceptBootNotifier {
    async fn register_reserve_now_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl CancelReservationHandler for AlwaysAcceptBootNotifier {
    async fn register_cancel_reservation_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl ResetHandler for AlwaysAcceptBootNotifier {
    async fn register_reset_handler(&self, _actor: ocpp_charge_point::actor::ChargePointActor) {}
}

#[async_trait::async_trait]
impl SendLocalListHandler for AlwaysAcceptBootNotifier {
    async fn register_send_local_list_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl GetLocalListVersionHandler for AlwaysAcceptBootNotifier {
    async fn register_get_local_list_version_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl GetVariablesHandler for AlwaysAcceptBootNotifier {
    async fn register_get_variables_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl SetVariablesHandler for AlwaysAcceptBootNotifier {
    async fn register_set_variables_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl GetBaseReportHandler for AlwaysAcceptBootNotifier {
    async fn register_get_base_report_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl GetReportHandler for AlwaysAcceptBootNotifier {
    async fn register_get_report_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl SecurityEventNotifier for AlwaysAcceptBootNotifier {
    type Error = core::convert::Infallible;

    async fn notify_security_event(
        &self,
        _event_type: &ocpp_charge_point::state::SecurityEventType,
        _tech_info: Option<&str>,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl CostUpdatedHandler for AlwaysAcceptBootNotifier {
    async fn register_cost_updated_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

// Smart Charging (docs/ROADMAP.md §11). This sample charge point declares no `smart_charging`
// capability, so `setup` never registers these - they exist because `setup`'s "everything on"
// bound list still requires the CSMS type to be *able* to handle them.
#[async_trait::async_trait]
impl SetChargingProfileHandler for AlwaysAcceptBootNotifier {
    async fn register_set_charging_profile_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl ClearChargingProfileHandler for AlwaysAcceptBootNotifier {
    async fn register_clear_charging_profile_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl GetCompositeScheduleHandler for AlwaysAcceptBootNotifier {
    async fn register_get_composite_schedule_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
        _projection: std::sync::Arc<ChargingLimitProjection>,
    ) {
    }
}

#[async_trait::async_trait]
impl ReconnectHandler for AlwaysAcceptBootNotifier {
    async fn register_reconnect_handler<F, FF>(&self, _callback: F)
    where
        F: FnMut() -> FF + Send + Sync + 'static,
        FF: core::future::Future<Output = ()> + Send + 'static,
    {
    }
}

struct SampleChargePoint {
    // `Arc` so `start()` can clone a handle into its spawned command-processing loop while
    // `evses()` keeps borrowing from `&self` - see `ChargePoint::start`'s docs on why a real
    // implementation generally can't service `commands` from within `start()` itself.
    evses: std::sync::Arc<[SampleEvse; 2]>,
}

struct SampleEvse {
    connectors: [SampleConnector; 2],
}

struct SampleConnector;

impl SampleEvse {
    pub fn new() -> Self {
        Self {
            connectors: [SampleConnector, SampleConnector],
        }
    }
}

impl SampleChargePoint {
    pub fn new() -> Self {
        Self {
            evses: std::sync::Arc::new([SampleEvse::new(), SampleEvse::new()]),
        }
    }
}

#[async_trait::async_trait]
impl ChargePoint<SampleEvse, SampleConnector> for SampleChargePoint {
    type StartError = core::convert::Infallible;

    async fn vendor_name(&self) -> &str {
        "Test Vendor"
    }
    async fn model_name(&self) -> &str {
        "Test Model"
    }

    async fn evses(&self) -> &[SampleEvse] {
        &self.evses[..]
    }

    async fn capabilities(&self) -> ocpp_charge_point::hardware::Capabilities {
        ocpp_charge_point::hardware::Capabilities::default()
    }

    async fn start(
        &self,
        events: ocpp_charge_point::hardware::HardwareEventSender,
        mut commands: ocpp_charge_point::hardware::HardwareCommandReceiver,
    ) -> Result<(), Self::StartError> {
        // Real hardware bindings should service `commands` for as long as the process runs -
        // `execute_hardware_command` dispatches each one to the right `Connector` and reports
        // the outcome back via `events`, including turning a hardware failure into the right
        // fault event. `start()` itself must return promptly (setup() awaits it before
        // registering with the CSMS), so the loop runs in a spawned task instead.
        let evses = self.evses.clone();
        tokio::spawn(async move {
            while let Ok(command) = commands.recv().await {
                ocpp_charge_point::hardware::execute_hardware_command(&evses[..], command, &events)
                    .await;
            }
        });
        Ok(())
    }
}

#[async_trait::async_trait]
impl Evse<SampleConnector> for SampleEvse {
    type Error = core::convert::Infallible;

    async fn connectors(&self) -> &[SampleConnector] {
        return &self.connectors;
    }

    async fn reboot(&self) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl Connector for SampleConnector {
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let runtime = setup(
        SampleChargePoint::new(),
        AlwaysAcceptBootNotifier,
        TokioExecutor,
        TokioBackoff,
        ocpp_charge_point::clock::SystemMonotonicClock,
        ocpp_charge_point::clock::SystemClock,
    )
    .await?;
    runtime
        .send(ChargePointEvent::Evse {
            evse_id: 1,
            event: EvseEvent::Connector {
                connector_id: 1,
                event: ConnectorEvent::CableConnected,
            },
        })
        .await
        .unwrap();

    Ok(())
}
