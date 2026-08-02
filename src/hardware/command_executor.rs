use crate::hardware::{Connector, Evse, HardwareEventSender};
use crate::state::{ChargePointEvent, ConnectorEvent, HardwareCommand};

pub async fn execute_hardware_command<E: Evse<C>, C: Connector>(
    evses: &[E],
    command: HardwareCommand,
    events: &HardwareEventSender,
) {
    let (evse_id, connector_id) = command_address(command);
    let event = match evses.get(evse_id) {
        Some(evse) => match evse.connectors().await.get(connector_id) {
            Some(connector) => match command {
                HardwareCommand::LockConnector { .. } => connector
                    .lock()
                    .await
                    .map(|()| ConnectorEvent::LockConfirmed),
                HardwareCommand::UnlockConnector { .. } => connector
                    .unlock()
                    .await
                    .map(|()| ConnectorEvent::UnlockConfirmed),
                HardwareCommand::CloseContactor { .. } => connector
                    .close_contactor()
                    .await
                    .map(|()| ConnectorEvent::ContactorClosed),
                HardwareCommand::OpenContactor { .. } => connector
                    .open_contactor()
                    .await
                    .map(|()| ConnectorEvent::ContactorOpened),
            }
            .unwrap_or(ConnectorEvent::FaultDetected),
            None => ConnectorEvent::FaultDetected,
        },
        None => ConnectorEvent::FaultDetected,
    };

    let _ = events
        .send(ChargePointEvent::Evse {
            evse_id,
            event: crate::state::EvseEvent::Connector {
                connector_id,
                event,
            },
        })
        .await;
}

fn command_address(command: HardwareCommand) -> (usize, usize) {
    match command {
        HardwareCommand::LockConnector {
            evse_id,
            connector_id,
        }
        | HardwareCommand::UnlockConnector {
            evse_id,
            connector_id,
        }
        | HardwareCommand::CloseContactor {
            evse_id,
            connector_id,
        }
        | HardwareCommand::OpenContactor {
            evse_id,
            connector_id,
        } => (evse_id, connector_id),
    }
}
