use crate::hardware::{Connector, Evse, HardwareEventSender};
use crate::state::{ChargePointEvent, ConnectorEvent, EvseEvent, HardwareCommand};

/// Dispatches a single [`HardwareCommand`] to the [`Connector`]/[`Evse`] it addresses and
/// reports the outcome back via `events`.
///
/// For a per-connector command (`LockConnector`/`UnlockConnector`/`CloseContactor`/
/// `OpenContactor`), the outcome is reported as the matching `ConnectorEvent`
/// (`LockConfirmed`/`UnlockConfirmed`/`ContactorClosed`/`ContactorOpened`). A failed hardware
/// call, or a command addressing an EVSE/connector index that doesn't exist (which should not
/// happen in practice - `HardwareCommand`s are only ever emitted by this crate's own state
/// machine for connectors it already knows about), is reported as
/// [`ConnectorEvent::FaultDetected`] rather than propagated as an error or a panic - per
/// `CLAUDE.md`'s error-handling guidance, a hardware problem must drive the connector into an
/// explicit faulted state, never take down the process.
///
/// For `Reboot { evse_id }` (OCPP `Reset` - see `docs/ROADMAP.md` §2), the addressed
/// [`Evse::reboot`] is called; a failure (or an out-of-range `evse_id`) is reported as
/// [`EvseEvent::FaultDetected`], the EVSE-level equivalent of the same principle. On success,
/// nothing further is reported: a real reboot ends the process (and this whole actor) before any
/// continuation could run, so there is no "came back up" event to send here - see
/// [`Evse::reboot`]'s docs.
///
/// Most [`ChargePoint::start`](crate::hardware::ChargePoint::start) implementations should loop
/// this over `commands.recv()` for as long as it returns `Ok`, rather than dispatching commands
/// by hand.
pub async fn execute_hardware_command<E: Evse<C>, C: Connector>(
    evses: &[E],
    command: HardwareCommand,
    events: &HardwareEventSender,
) {
    if let HardwareCommand::Reboot { evse_id } = command {
        let failed = match evses.get(evse_id) {
            Some(evse) => match evse.reboot().await {
                Ok(()) => false,
                Err(error) => {
                    tracing::warn!(evse_id, %error, "the EVSE failed to reboot; faulting it");
                    true
                }
            },
            None => {
                tracing::error!(
                    evse_id,
                    "the state machine asked to reboot an EVSE the hardware does not have"
                );
                true
            }
        };
        if failed {
            let _ = events
                .send(ChargePointEvent::Evse {
                    evse_id,
                    event: EvseEvent::FaultDetected,
                })
                .await;
        }
        return;
    }

    let (evse_id, connector_id) = command_address(command);
    let event = match evses.get(evse_id) {
        Some(evse) => match evse.connectors().get(connector_id) {
            Some(connector) => {
                let outcome = match command {
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
                    HardwareCommand::SetCurrentLimit { limit_ma, .. } => connector
                        .set_current_limit(limit_ma)
                        .await
                        .map(|()| ConnectorEvent::CurrentLimitConfirmed(limit_ma)),
                    HardwareCommand::Reboot { .. } => {
                        unreachable!("Reboot is handled and returned above")
                    }
                };
                match outcome {
                    Ok(event) => event,
                    Err(error) => {
                        // Faulting the connector is the right response and was already happening;
                        // what was missing is *why*. The binding's error was dropped on the floor
                        // here, so an integrator debugging a sticky contactor saw a connector go
                        // `Faulted` with nothing anywhere saying which call failed.
                        tracing::warn!(
                            evse_id,
                            connector_id,
                            command = ?command,
                            %error,
                            "a hardware binding call failed; faulting the connector"
                        );
                        ConnectorEvent::FaultDetected
                    }
                }
            }
            // Not a hardware fault but a firmware one: the state machine's topology and the
            // hardware's disagree, and every command addressed here will keep failing.
            None => {
                tracing::error!(
                    evse_id,
                    connector_id,
                    command = ?command,
                    "the state machine addressed a connector this EVSE does not have"
                );
                ConnectorEvent::FaultDetected
            }
        },
        None => {
            tracing::error!(
                evse_id,
                command = ?command,
                "the state machine addressed an EVSE the hardware does not have"
            );
            ConnectorEvent::FaultDetected
        }
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

/// The `(evse_id, connector_id)` a per-connector [`HardwareCommand`] addresses - every such
/// variant carries the same two fields, just for a different physical action. Never called with
/// `Reboot` - [`execute_hardware_command`] handles and returns on that variant before reaching
/// this function.
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
        }
        | HardwareCommand::SetCurrentLimit {
            evse_id,
            connector_id,
            ..
        } => (evse_id, connector_id),
        HardwareCommand::Reboot { .. } => {
            unreachable!("Reboot is handled by execute_hardware_command before this is called")
        }
    }
}
