use ocpp_charge_point::authorization::{Authorizer, ClearCacheHandler};
use ocpp_charge_point::availability::{ChangeAvailabilityHandler, StatusNotifier};
use ocpp_charge_point::connection::ReconnectHandler;
use ocpp_charge_point::cost::CostUpdatedHandler;
use ocpp_charge_point::device_model::{GetVariablesHandler, SetVariablesHandler};
use ocpp_charge_point::executor::TokioExecutor;
use ocpp_charge_point::hardware::{ChargePoint, Connector, Evse};
use ocpp_charge_point::local_authorization_list::{
    GetLocalListVersionHandler, SendLocalListHandler,
};
use ocpp_charge_point::meter_values::MeterValuesNotifier;
use ocpp_charge_point::periodic_event_stream::{
    AdjustPeriodicEventStreamHandler, ClosePeriodicEventStreamHandler,
    GetPeriodicEventStreamHandler, OpenPeriodicEventStreamHandler, PeriodicEventStreamNotifier,
};
use ocpp_charge_point::provisioning::{
    BootNotificationOutcome, BootNotifier, HeartbeatSender, TokioBackoff,
};
use ocpp_charge_point::remote_control::{
    RequestStartTransactionHandler, RequestStopTransactionHandler, TriggerMessageHandler,
    UnlockConnectorHandler,
};
use ocpp_charge_point::reporting::{GetBaseReportHandler, GetReportHandler};
use ocpp_charge_point::reservation::{
    CancelReservationHandler, ReservationStatusNotifier, ReserveNowHandler,
};
use ocpp_charge_point::reset::ResetHandler;
use ocpp_charge_point::security::SecurityEventNotifier;
use ocpp_charge_point::setup;
use ocpp_charge_point::smart_charging::{
    ChargingLimitProjection, ClearChargingProfileHandler, GetChargingProfilesHandler,
    GetCompositeScheduleHandler, SetChargingProfileHandler,
};
use ocpp_charge_point::state::{
    AuthorizationStatus, BootReasonCause, ChargePointEvent, ConnectorEvent, ConnectorStatus,
    EvseEvent, IdToken, RegistrationStatus, Transaction, TransactionEventKind,
};
use ocpp_charge_point::tariff::{
    ChangeTransactionTariffHandler, ClearTariffsHandler, GetTariffsHandler, SetDefaultTariffHandler,
};
use ocpp_charge_point::transactions::{TransactionEventOutcome, TransactionNotifier};
use std::sync::Arc;

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
        _offline: bool,
    ) -> Result<TransactionEventOutcome, Self::Error> {
        Ok(TransactionEventOutcome::acknowledged())
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
impl ocpp_charge_point::variable_monitoring::SetVariableMonitoringHandler
    for AlwaysAcceptBootNotifier
{
    async fn register_set_variable_monitoring_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl ocpp_charge_point::variable_monitoring::ClearVariableMonitoringHandler
    for AlwaysAcceptBootNotifier
{
    async fn register_clear_variable_monitoring_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl ocpp_charge_point::variable_monitoring::VariableMonitorEventNotifier
    for AlwaysAcceptBootNotifier
{
    type Error = core::convert::Infallible;

    async fn notify_variable_monitor_event(
        &self,
        _event: &ocpp_charge_point::state::TriggeredMonitor,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl ocpp_charge_point::variable_monitoring::SetMonitoringBaseHandler for AlwaysAcceptBootNotifier {
    async fn register_set_monitoring_base_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl ocpp_charge_point::variable_monitoring::SetMonitoringLevelHandler
    for AlwaysAcceptBootNotifier
{
    async fn register_set_monitoring_level_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl ocpp_charge_point::variable_monitoring::GetMonitoringReportHandler
    for AlwaysAcceptBootNotifier
{
    async fn register_get_monitoring_report_handler(
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

// The tariff store and per-transaction tariff assignment (docs/PRODUCTION-ROADMAP.md B7.1):
// SetDefaultTariff/ChangeTransactionTariff/ClearTariffs/GetTariffs. This sample never receives
// any of them - see `ocpp_charge_point::tariff`.
#[async_trait::async_trait]
impl SetDefaultTariffHandler for AlwaysAcceptBootNotifier {
    async fn register_set_default_tariff_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl ChangeTransactionTariffHandler for AlwaysAcceptBootNotifier {
    async fn register_change_transaction_tariff_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl ClearTariffsHandler for AlwaysAcceptBootNotifier {
    async fn register_clear_tariffs_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl GetTariffsHandler for AlwaysAcceptBootNotifier {
    async fn register_get_tariffs_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

// Periodic event streams (docs/PRODUCTION-ROADMAP.md B5.6): this sample has no monitored
// variable a CSMS would stream, so every handler is a no-op and outbound notification always
// succeeds.
#[async_trait::async_trait]
impl OpenPeriodicEventStreamHandler for AlwaysAcceptBootNotifier {
    async fn register_open_periodic_event_stream_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl ClosePeriodicEventStreamHandler for AlwaysAcceptBootNotifier {
    async fn register_close_periodic_event_stream_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl AdjustPeriodicEventStreamHandler for AlwaysAcceptBootNotifier {
    async fn register_adjust_periodic_event_stream_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl GetPeriodicEventStreamHandler for AlwaysAcceptBootNotifier {
    async fn register_get_periodic_event_stream_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl PeriodicEventStreamNotifier for AlwaysAcceptBootNotifier {
    type Error = std::convert::Infallible;

    async fn notify_periodic_event_stream(
        &self,
        _sample: ocpp_charge_point::periodic_event_stream::PeriodicStreamSample,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

// SetNetworkProfile (docs/ROADMAP.md §2): storing a connection profile in a slot. This sample
// never receives one, and storing one would not change where it connects anyway - see
// `ocpp_charge_point::network_profile`.
#[async_trait::async_trait]
impl ocpp_charge_point::network_profile::SetNetworkProfileHandler for AlwaysAcceptBootNotifier {
    async fn register_set_network_profile_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

// ClearCache (docs/ROADMAP.md §3): emptying the authorization cache.
#[async_trait::async_trait]
impl ClearCacheHandler for AlwaysAcceptBootNotifier {
    async fn register_clear_cache_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

// TriggerMessage (docs/ROADMAP.md §6): a CSMS asking for a Heartbeat or StatusNotification
// re-send. This fake registers nothing, so nothing is ever triggered.
#[async_trait::async_trait]
impl TriggerMessageHandler for AlwaysAcceptBootNotifier {
    async fn register_trigger_message_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

// Standalone MeterValues (docs/ROADMAP.md §10). This sample never configures
// `AlignedDataCtrlr.Interval`, so the loop `setup` spawns stays parked and this is never called -
// it exists because `setup`'s bound list requires the CSMS type to be able to send one.
#[async_trait::async_trait]
impl MeterValuesNotifier for AlwaysAcceptBootNotifier {
    type Error = std::convert::Infallible;

    async fn send_meter_values(
        &self,
        _evse_id: usize,
        _connector_id: usize,
        _sample: ocpp_charge_point::state::MeterSample,
    ) -> Result<(), Self::Error> {
        Ok(())
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
impl ReservationStatusNotifier for AlwaysAcceptBootNotifier {
    type Error = core::convert::Infallible;

    async fn notify_reservation_status(
        &self,
        _update: ocpp_charge_point::state::ReservationUpdate,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl GetChargingProfilesHandler for AlwaysAcceptBootNotifier {
    async fn register_get_charging_profiles_handler(
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
    evses: [SampleEvse; 2],
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
            evses: [SampleEvse::new(), SampleEvse::new()],
        }
    }
}

#[async_trait::async_trait]
impl ChargePoint<SampleEvse, SampleConnector> for SampleChargePoint {
    type StartError = core::convert::Infallible;

    fn vendor_name(&self) -> &str {
        "Test Vendor"
    }
    fn model_name(&self) -> &str {
        "Test Model"
    }

    fn evses(&self) -> &[SampleEvse] {
        &self.evses[..]
    }

    fn capabilities(&self) -> ocpp_charge_point::hardware::Capabilities {
        ocpp_charge_point::hardware::Capabilities::default()
    }

    async fn start(
        self: Arc<Self>,
        events: ocpp_charge_point::hardware::HardwareEventSender,
        mut commands: ocpp_charge_point::hardware::HardwareCommandReceiver,
    ) -> Result<(), Self::StartError> {
        // Real hardware bindings should service `commands` for as long as the process runs -
        // `execute_hardware_command` dispatches each one to the right `Connector` and reports
        // the outcome back via `events`, including turning a hardware failure into the right
        // fault event. `start()` itself must return promptly (setup() awaits it before
        // registering with the CSMS), so the loop runs in a spawned task instead - which is why
        // the receiver is an `Arc<Self>`: the task moves it, and nothing here needs an `Arc` of
        // its own.
        tokio::spawn(async move {
            while let Ok(command) = commands.recv().await {
                ocpp_charge_point::hardware::execute_hardware_command(
                    self.evses(),
                    command,
                    &events,
                )
                .await;
            }
        });
        Ok(())
    }
}

#[async_trait::async_trait]
impl Evse<SampleConnector> for SampleEvse {
    type Error = core::convert::Infallible;

    fn connectors(&self) -> &[SampleConnector] {
        &self.connectors
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
