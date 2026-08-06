use crate::ChargePointRuntime;
use crate::availability::{ChangeAvailabilityHandler, StatusNotifier};
use crate::builder::ChargePointBuilder;
use crate::connection::ReconnectHandler;
use crate::cost::CostUpdatedHandler;
use crate::device_model::{GetVariablesHandler, SetVariablesHandler};
use crate::executor::Executor;
use crate::hardware::ChargePoint;
use crate::hardware::Connector;
use crate::hardware::Evse;
use crate::local_authorization_list::{GetLocalListVersionHandler, SendLocalListHandler};
use crate::provisioning::{Backoff, BootNotifier, HeartbeatSender};
use crate::remote_control::{
    RequestStartTransactionHandler, RequestStopTransactionHandler, UnlockConnectorHandler,
};
use crate::reporting::{GetBaseReportHandler, GetReportHandler};
use crate::reservation::{CancelReservationHandler, ReserveNowHandler};
use crate::reset::ResetHandler;
use crate::security::SecurityEventNotifier;
use crate::transactions::TransactionNotifier;

/// Starts the hardware, then runs the Provisioning functional block's BootNotification
/// exchange (retrying with `backoff` on `Pending`/`Rejected` or a transport failure - see
/// [`ChargePointRuntime::register_until_accepted`]). Once accepted, uses `executor` to spawn
/// background tasks that send a Heartbeat at the interval the CSMS returned, forward every
/// connector status change to the CSMS via StatusNotification, forward every transaction
/// lifecycle event via TransactionEvent, answer every presented-id-token authorization
/// request via Authorize, and forward every reported security event via
/// SecurityEventNotification, for as long as the process runs.
///
/// `executor`/`backoff` are caller-supplied (rather than defaulting to tokio) so this function
/// doesn't hard-depend on an async runtime - std/tokio users can pass
/// [`crate::executor::TokioExecutor`]/[`crate::provisioning::TokioBackoff`]; embedded targets
/// supply their own.
///
/// This is a thin "everything on" wrapper around [`ChargePointBuilder`], registering every
/// functional block it exposes in the same order this function has always used. Callers whose
/// CSMS client only implements a subset of blocks - or who want to skip a block outright - should
/// use [`ChargePointBuilder`] directly instead; `N`'s single 21-trait bound below is exactly the
/// limitation the builder exists to remove.
pub async fn setup<T, E, C, N, X, B>(
    charge_point: T,
    csms: N,
    executor: X,
    backoff: B,
) -> Result<ChargePointRuntime<T>, T::StartError>
where
    T: ChargePoint<E, C>,
    E: Evse<C>,
    C: Connector,
    N: BootNotifier
        + HeartbeatSender
        + StatusNotifier
        + TransactionNotifier
        + crate::authorization::Authorizer
        + UnlockConnectorHandler
        + ChangeAvailabilityHandler
        + RequestStartTransactionHandler
        + RequestStopTransactionHandler
        + ReserveNowHandler
        + CancelReservationHandler
        + ResetHandler
        + SendLocalListHandler
        + GetLocalListVersionHandler
        + GetVariablesHandler
        + SetVariablesHandler
        + GetBaseReportHandler
        + GetReportHandler
        + SecurityEventNotifier
        + CostUpdatedHandler
        + ReconnectHandler
        + Clone
        + Send
        + Sync
        + 'static,
    X: Executor,
    B: Backoff + Clone + Send + Sync + 'static,
{
    let runtime = ChargePointBuilder::start(charge_point, executor)
        .await?
        .provisioning(&csms, backoff)
        .await
        .status_notifications(&csms)
        .await
        .transaction_events(&csms)
        .await
        .authorization(&csms)
        .await
        .security_events(&csms)
        .await
        .remote_control(&csms)
        .await
        .availability_control(&csms)
        .await
        .reservation(&csms)
        .await
        .reset(&csms)
        .await
        .local_authorization_list(&csms)
        .await
        .device_model(&csms)
        .await
        .cost(&csms)
        .await
        .build();

    Ok(runtime)
}

#[cfg(test)]
mod tests {
    use super::setup;
    use crate::builder::test_support::{TestChargePoint, TestConnector, TestEvse};
    use crate::executor::TokioExecutor;
    use crate::provisioning::BootNotificationOutcome;
    use crate::provisioning::TokioBackoff;
    use crate::provisioning::test_support::FixedBootNotifier;
    use crate::state::{ConnectorState, RegistrationStatus};
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicBool, Ordering};

    fn accepted_boot_notifier() -> FixedBootNotifier {
        FixedBootNotifier(BootNotificationOutcome {
            status: RegistrationStatus::Accepted,
            interval_secs: 60,
        })
    }

    #[tokio::test]
    async fn setup_routes_startup_hardware_events_into_runtime_state() {
        let locked = Arc::new(AtomicBool::new(false));
        let runtime = setup(
            TestChargePoint {
                evses: [TestEvse {
                    connectors: [TestConnector {
                        locked: locked.clone(),
                        lock_succeeds: true,
                    }],
                }],
            },
            accepted_boot_notifier(),
            TokioExecutor,
            TokioBackoff,
        )
        .await
        .unwrap();

        assert_eq!(
            runtime.state().evses[0].connectors[0],
            ConnectorState::Locked
        );
        assert!(locked.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_failed_hardware_command_reports_a_connector_fault() {
        let locked = Arc::new(AtomicBool::new(false));
        let runtime = setup(
            TestChargePoint {
                evses: [TestEvse {
                    connectors: [TestConnector {
                        locked: locked.clone(),
                        lock_succeeds: false,
                    }],
                }],
            },
            accepted_boot_notifier(),
            TokioExecutor,
            TokioBackoff,
        )
        .await
        .unwrap();

        assert_eq!(
            runtime.state().evses[0].connectors[0],
            ConnectorState::Faulted
        );
        assert!(!locked.load(Ordering::SeqCst));
    }
}
