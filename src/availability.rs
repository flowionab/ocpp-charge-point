//! Availability functional block: reporting connector status to the CSMS via
//! StatusNotification. See `docs/ROADMAP.md` §7.

use crate::state::{ConnectorStatus, ConnectorStatusChanged};
use alloc::boxed::Box;
use tokio::sync::broadcast;

/// Reports a connector's status to the CSMS via StatusNotification. Implemented per protocol
/// version (see the `ocpp_2_1` module), mirroring [`crate::provisioning::BootNotifier`].
#[async_trait::async_trait]
pub trait StatusNotifier {
    type Error: core::error::Error + Send + Sync + 'static;

    async fn notify_status(
        &self,
        evse_id: usize,
        connector_id: usize,
        status: ConnectorStatus,
    ) -> Result<(), Self::Error>;
}

/// Forwards every connector status change received on `changes` to the CSMS via `notifier`,
/// forever. Errors are logged and do not stop the loop or drop the change - the actor already
/// applied it to state; only the CSMS-facing report failed.
pub async fn run_status_notifications<N: StatusNotifier>(
    mut changes: broadcast::Receiver<ConnectorStatusChanged>,
    notifier: &N,
) {
    loop {
        match changes.recv().await {
            Ok(changed) => {
                if let Err(err) = notifier
                    .notify_status(changed.evse_id, changed.connector_id, changed.status)
                    .await
                {
                    tracing::warn!(error = %err, "status notification failed");
                }
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(
                    skipped,
                    "status notification receiver lagged, some updates were dropped"
                );
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{StatusNotifier, run_status_notifications};
    use crate::state::{ConnectorStatus, ConnectorStatusChanged};
    use alloc::boxed::Box;
    use alloc::vec::Vec;
    use tokio::sync::{broadcast, watch};

    struct RecordingStatusNotifier {
        seen: watch::Sender<Vec<(usize, usize, ConnectorStatus)>>,
    }

    #[async_trait::async_trait]
    impl StatusNotifier for RecordingStatusNotifier {
        type Error = core::convert::Infallible;

        async fn notify_status(
            &self,
            evse_id: usize,
            connector_id: usize,
            status: ConnectorStatus,
        ) -> Result<(), Self::Error> {
            self.seen
                .send_modify(|seen| seen.push((evse_id, connector_id, status)));
            Ok(())
        }
    }

    #[tokio::test]
    async fn forwards_every_status_change_to_the_notifier_in_order() {
        let (sender, receiver) = broadcast::channel(8);
        let (seen_tx, mut seen_rx) = watch::channel(Vec::new());
        let notifier = RecordingStatusNotifier { seen: seen_tx };

        let forwarder = tokio::spawn(async move {
            run_status_notifications(receiver, &notifier).await;
        });

        sender
            .send(ConnectorStatusChanged {
                evse_id: 0,
                connector_id: 0,
                status: ConnectorStatus::Occupied,
            })
            .unwrap();
        sender
            .send(ConnectorStatusChanged {
                evse_id: 0,
                connector_id: 1,
                status: ConnectorStatus::Faulted,
            })
            .unwrap();

        seen_rx
            .wait_for(|seen| seen.len() == 2)
            .await
            .expect("notifier task is still running");

        // Dropping the sender closes the channel, which ends `run_status_notifications`'s loop.
        drop(sender);
        forwarder.await.unwrap();

        assert_eq!(
            *seen_rx.borrow(),
            alloc::vec![
                (0, 0, ConnectorStatus::Occupied),
                (0, 1, ConnectorStatus::Faulted),
            ]
        );
    }
}

#[cfg(feature = "ocpp_2_1")]
mod ocpp_2_1 {
    use crate::state::ConnectorStatus;
    use ocpp_client::rust_ocpp::v2_1::messages::status_notification::ConnectorStatusEnumType;

    // Only consumed by `with_system_clock` below (`std`-gated) and by this module's own tests;
    // without either, it's legitimately unused.
    #[cfg_attr(not(feature = "std"), allow(dead_code))]
    pub(super) fn map_status(status: ConnectorStatus) -> ConnectorStatusEnumType {
        match status {
            ConnectorStatus::Available => ConnectorStatusEnumType::Available,
            ConnectorStatus::Occupied => ConnectorStatusEnumType::Occupied,
            ConnectorStatus::Reserved => ConnectorStatusEnumType::Reserved,
            ConnectorStatus::Unavailable => ConnectorStatusEnumType::Unavailable,
            ConnectorStatus::Faulted => ConnectorStatusEnumType::Faulted,
        }
    }

    // `StatusNotificationRequest` needs a timestamp; producing one without a caller-supplied
    // `Clock` requires the `std`-only `SystemClock` (see `crate::clock`), so this impl - unlike
    // the rest of this file - needs both `ocpp_2_1` and `std`.
    #[cfg(feature = "std")]
    mod with_system_clock {
        use super::map_status;
        use crate::availability::StatusNotifier;
        use crate::clock::{Clock, SystemClock};
        use crate::state::ConnectorStatus;
        use alloc::boxed::Box;
        use ocpp_client::ClientError;
        use ocpp_client::ocpp_2_1::{OCPP2_1Client, OCPP2_1Error};
        use ocpp_client::rust_ocpp::v2_1::messages::status_notification::StatusNotificationRequest;

        #[async_trait::async_trait]
        impl StatusNotifier for OCPP2_1Client {
            type Error = ClientError<OCPP2_1Error>;

            async fn notify_status(
                &self,
                evse_id: usize,
                connector_id: usize,
                status: ConnectorStatus,
            ) -> Result<(), Self::Error> {
                self.send_status_notification(StatusNotificationRequest {
                    custom_data: None,
                    timestamp: SystemClock.now(),
                    connector_status: map_status(status),
                    evse_id: evse_id as i32,
                    connector_id: connector_id as i32,
                })
                .await?;
                Ok(())
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn every_internal_status_maps_to_the_matching_wire_status() {
            assert_eq!(
                map_status(ConnectorStatus::Available),
                ConnectorStatusEnumType::Available
            );
            assert_eq!(
                map_status(ConnectorStatus::Occupied),
                ConnectorStatusEnumType::Occupied
            );
            assert_eq!(
                map_status(ConnectorStatus::Reserved),
                ConnectorStatusEnumType::Reserved
            );
            assert_eq!(
                map_status(ConnectorStatus::Unavailable),
                ConnectorStatusEnumType::Unavailable
            );
            assert_eq!(
                map_status(ConnectorStatus::Faulted),
                ConnectorStatusEnumType::Faulted
            );
        }
    }
}
