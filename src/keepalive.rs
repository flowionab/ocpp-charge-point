//! WebSocket keepalive: driving `OCPPCommCtrlr.WebSocketPingInterval` onto the live connection
//! (A4, `docs/PRODUCTION-ROADMAP.md` §3).
//!
//! # Why a charge point wants this
//!
//! A charge point that only ever *answers* pings cannot tell a working connection from a
//! half-open one - a dropped NAT entry, a mobile link that went away without a FIN. The socket
//! stays readable-looking, nothing arrives, and the reconnect logic never fires because from the
//! read loop's point of view the connection is simply quiet. The station is offline and does not
//! know it, until the OS TCP timeout eventually notices, which can be many minutes.
//!
//! Pinging on a schedule and redialling when the peer stops answering is what closes that gap.
//! `ocpp-client` 0.3.0 owns the mechanism (`KeepalivePolicy`, and `Client::force_reconnect` as
//! the escalation); this module owns the *policy input*, which OCPP models as a device-model
//! variable the CSMS can read and write.
//!
//! # Why the value is applied here rather than read at connect time
//!
//! `crate::connect` seeds the connection's keepalive from a freshly-built
//! [`crate::state::DeviceModel`], because the connection has to exist before the actor that owns
//! the live model does. That covers a CSMS *reading* the variable, but not one *writing* it: a
//! `SetVariables`/`ChangeConfiguration` that only took effect on the next connection would be a
//! setting the CSMS cannot actually use.
//!
//! So this module watches the live model and pushes changes onto the running client, the same
//! shape [`crate::network_switch`] uses for `RetryBackOffRandomRange` and
//! `NetworkProfileConnectionAttempts`. `Client::set_ping_interval` is deliberately non-`async`
//! and takes effect immediately - shortening a long interval does not wait out the interval
//! already being slept - so what the CSMS wrote is what the connection does, from the moment it
//! writes it.
//!
//! # `0` means disabled, in both directions
//!
//! OCPP spells "do not ping" as `WebSocketPingInterval = 0`, and this crate registers `0` as the
//! default. That is deliberately *not* `ocpp-client`'s own default (60 s): what a CSMS reads from
//! this charge point has to be what the connection actually does, and a station that pings on a
//! cadence its own device model reports as `0` would be lying about its behaviour. A CSMS that
//! wants keepalive writes an interval and gets it immediately.

use crate::actor::ChargePointActor;
use crate::state::{ChargePointState, Component, Variable, VariableAttributeType};
use core::time::Duration;

/// The `OCPPCommCtrlr` variable this module drives.
const PING_INTERVAL_VARIABLE: &str = "WebSocketPingInterval";

/// Sets the keepalive ping interval on a live CSMS connection.
///
/// Kept as this crate's own trait, like every other CSMS-facing capability, so the update loop is
/// testable without a live socket and works for whichever OCPP version was negotiated. Implemented
/// for all three `ocpp-client` clients in terms of `Client::set_ping_interval`.
pub trait PingIntervalControl {
    /// Applies `secs` as the ping interval, where `0` disables pinging - OCPP's own spelling, and
    /// the one `Client::set_ping_interval` accepts directly.
    ///
    /// Infallible and non-blocking: this is called from a state-watching loop and from request
    /// handlers, neither of which has anywhere useful to report a failure to.
    fn set_ping_interval_secs(&self, secs: u32);

    /// The interval currently in force, `0` when pinging is off. This is the value to report for
    /// `OCPPCommCtrlr.WebSocketPingInterval` (2.x) or the `WebSocketPingInterval` configuration
    /// key (1.6J).
    fn ping_interval_secs(&self) -> u32;
}

impl<T: PingIntervalControl + ?Sized> PingIntervalControl for alloc::sync::Arc<T> {
    fn set_ping_interval_secs(&self, secs: u32) {
        (**self).set_ping_interval_secs(secs);
    }

    fn ping_interval_secs(&self) -> u32 {
        (**self).ping_interval_secs()
    }
}

/// Applies `OCPPCommCtrlr`/`WebSocketPingInterval` to `control` whenever the CSMS changes it.
///
/// Runs until the task holding it is dropped. The value is pushed only when it actually changes,
/// so an unrelated state update - a connector status, a meter sample - does not disturb a
/// keepalive schedule already in flight.
///
/// The first pass applies the value as it stands, which makes this correct regardless of whether
/// `crate::connect` already seeded the same number into `ConnectOptions`: pushing a value the
/// connection is already using is a no-op.
pub async fn run_ping_interval_updates<P: PingIntervalControl>(
    actor: &ChargePointActor,
    control: &P,
) {
    let mut states = actor.subscribe();
    let mut applied: Option<u32> = None;
    loop {
        let wanted = {
            let state = states.borrow();
            ping_interval_secs(&state)
        };
        if applied != Some(wanted) {
            tracing::info!(
                seconds = wanted,
                "applying WebSocketPingInterval to the live connection"
            );
            control.set_ping_interval_secs(wanted);
            applied = Some(wanted);
        }

        states.changed().await;
    }
}

/// `OCPPCommCtrlr`/`WebSocketPingInterval` from the live device model, or `0` (pinging disabled)
/// when it is absent or unparseable - the same value this crate registers, so what a CSMS reads
/// is what it gets.
pub(crate) fn ping_interval_secs(state: &ChargePointState) -> u32 {
    let component = Component {
        name: "OCPPCommCtrlr".into(),
        instance: None,
        evse: None,
    };
    let variable = Variable {
        name: PING_INTERVAL_VARIABLE.into(),
        instance: None,
    };
    state
        .device_model
        .get(&component, &variable)
        .and_then(|definition| definition.attribute(VariableAttributeType::Actual))
        .and_then(|attribute| attribute.value.parse::<u32>().ok())
        .unwrap_or(0)
}

/// `secs` as the `Option<Duration>` `Client::set_ping_interval` takes, mapping OCPP's `0` onto
/// `None`.
// Only called from the per-version [`PingIntervalControl`] impls below, each gated on its own
// feature - unused (and so a `dead_code` warning without this) when none of them are compiled in,
// e.g. `--no-default-features`. Same shape as `crate::reservation`'s equivalent.
#[cfg_attr(
    not(any(feature = "ocpp_1_6", feature = "ocpp_2_0_1", feature = "ocpp_2_1")),
    allow(dead_code)
)]
fn interval_from_secs(secs: u32) -> Option<Duration> {
    (secs != 0).then(|| Duration::from_secs(u64::from(secs)))
}

macro_rules! ping_interval_control {
    ($feature:literal, $module:ident, $client:ident) => {
        #[cfg(feature = $feature)]
        impl PingIntervalControl for ocpp_client::$module::$client {
            fn set_ping_interval_secs(&self, secs: u32) {
                self.set_ping_interval(interval_from_secs(secs));
            }

            fn ping_interval_secs(&self) -> u32 {
                self.ping_interval()
                    .map(|interval| u32::try_from(interval.as_secs()).unwrap_or(u32::MAX))
                    .unwrap_or(0)
            }
        }
    };
}

ping_interval_control!("ocpp_2_1", ocpp_2_1, OCPP2_1Client);
ping_interval_control!("ocpp_2_0_1", ocpp_2_0_1, OCPP2_0_1Client);
ping_interval_control!("ocpp_1_6", ocpp_1_6, OCPP1_6Client);

#[cfg(all(test, feature = "tokio-runtime"))]
mod tests {
    use super::*;
    use crate::executor::TokioExecutor;
    use crate::state::ChargePointEvent;
    use alloc::string::ToString;
    use alloc::sync::Arc;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingControl {
        applied: Mutex<alloc::vec::Vec<u32>>,
    }

    impl PingIntervalControl for RecordingControl {
        fn set_ping_interval_secs(&self, secs: u32) {
            self.applied.lock().unwrap().push(secs);
        }

        fn ping_interval_secs(&self) -> u32 {
            self.applied.lock().unwrap().last().copied().unwrap_or(0)
        }
    }

    /// Writes `WebSocketPingInterval` the way a CSMS `SetVariables` would - through the same
    /// event `crate::device_model::handle_set_variables` sends.
    async fn write_interval(actor: &ChargePointActor, secs: u32) {
        let _ = actor
            .send(ChargePointEvent::DeviceModel(
                crate::state::DeviceModelEvent::AttributeValueSet {
                    component: Component {
                        name: "OCPPCommCtrlr".into(),
                        instance: None,
                        evse: None,
                    },
                    variable: Variable {
                        name: PING_INTERVAL_VARIABLE.into(),
                        instance: None,
                    },
                    attribute_type: VariableAttributeType::Actual,
                    value: secs.to_string(),
                },
            ))
            .await;
    }

    async fn settle<F: Fn() -> bool>(done: F) {
        for _ in 0..200 {
            if done() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// The whole point of A4: a CSMS write reaches the *running* connection, not merely the next
    /// one. Before `ocpp-client` 0.3.0 there was no interval to set, which is why this crate
    /// registered a hardcoded `0`.
    #[tokio::test]
    async fn a_csms_write_reaches_the_running_connection() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let control = Arc::new(RecordingControl::default());

        let updates = {
            let actor = actor.clone();
            let control = control.clone();
            tokio::spawn(async move {
                run_ping_interval_updates(&actor, &control).await;
            })
        };

        write_interval(&actor, 45).await;
        settle(|| control.applied.lock().unwrap().contains(&45)).await;

        assert_eq!(control.ping_interval_secs(), 45);
        updates.abort();
    }

    /// `0` is OCPP's spelling of "do not ping", so turning keepalive back off has to reach the
    /// connection just as a non-zero value does.
    #[tokio::test]
    async fn writing_zero_turns_pinging_back_off() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let control = Arc::new(RecordingControl::default());

        let updates = {
            let actor = actor.clone();
            let control = control.clone();
            tokio::spawn(async move {
                run_ping_interval_updates(&actor, &control).await;
            })
        };

        write_interval(&actor, 45).await;
        settle(|| control.applied.lock().unwrap().contains(&45)).await;
        write_interval(&actor, 0).await;
        settle(|| control.ping_interval_secs() == 0).await;

        assert_eq!(control.ping_interval_secs(), 0);
        updates.abort();
    }

    /// State churn is constant on a charge point - connector statuses, meter samples - and each
    /// update would otherwise re-apply the same interval, which `set_ping_interval` treats as a
    /// change and uses to wake the keepalive task. Pinging would then restart its interval every
    /// time anything happened, which on a busy station means never actually pinging.
    #[tokio::test]
    async fn an_unrelated_state_change_does_not_disturb_the_schedule() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let control = Arc::new(RecordingControl::default());

        let updates = {
            let actor = actor.clone();
            let control = control.clone();
            tokio::spawn(async move {
                run_ping_interval_updates(&actor, &control).await;
            })
        };

        write_interval(&actor, 45).await;
        settle(|| control.applied.lock().unwrap().contains(&45)).await;

        // Anything that changes state without touching the variable.
        let _ = actor
            .send(ChargePointEvent::RegistrationStatusReceived(
                crate::state::RegistrationStatus::Accepted,
            ))
            .await;
        tokio::time::sleep(Duration::from_millis(100)).await;

        let applied = control.applied.lock().unwrap().clone();
        assert_eq!(
            applied.iter().filter(|secs| **secs == 45).count(),
            1,
            "the same interval was pushed more than once: {applied:?}"
        );
        updates.abort();
    }

    #[test]
    fn ocpps_zero_means_no_pinging_rather_than_a_zero_length_interval() {
        assert_eq!(interval_from_secs(0), None);
        assert_eq!(interval_from_secs(30), Some(Duration::from_secs(30)));
    }
}
