//! Live `ocpp-client` WebSocket wiring, this crate's std/tokio "batteries included" entry point.
//! [`setup`](crate::setup) itself only takes an already-connected CSMS client -
//! [`connect_and_setup`] closes that gap by dialing `address` with `ocpp_client::connect` first
//! (offering whichever OCPP versions `versions` names, or every version compiled into this
//! build if `None` - the CSMS picks one via the WebSocket subprotocol handshake, per RFC 6455),
//! then handing the negotiated client straight to `setup`. Embedded targets, or std users who
//! need a non-WebSocket transport, still construct their own client and call
//! [`setup`](crate::setup) directly.
//!
//! # All three versions run a session (A1/A2)
//!
//! Whichever version the CSMS picks, [`connect_and_setup`] runs it. That took more than a
//! `match`: [`setup`](crate::setup) is a single monomorphised function requiring *every*
//! functional block's trait on one client type, and no 1.6J or 2.0.1 client satisfies all of
//! them - 1.6J addresses connectors with a flat id and needs a topology-aware wrapper for half
//! its adapters, and 2.0.1 has no `SecurityEventNotification` upstream at all. So the 1.6J and
//! 2.0.1 paths register blocks through [`ChargePointBuilder`](crate::builder::ChargePointBuilder)
//! one at a time, which is exactly the limitation C4's builder was built to remove.
//!
//! What each version gets is therefore *not* identical, and the differences are real rather than
//! oversights - see [`connect_and_setup`]'s own docs for the per-version list. `versions` orders
//! what this charge point offers (A3): the CSMS picks from that list, so putting one version in
//! it forces that version, and the default offers every version compiled into the build,
//! newest first.

use crate::ChargePointRuntime;
use crate::builder::ChargePointBuilder;
use crate::clock::{SystemClock, SystemMonotonicClock};
use crate::executor::Executor;
use crate::hardware::{ChargePoint, Connector, Evse};
use crate::network_switch::ConnectionTarget;
use crate::provisioning::Backoff;
use crate::setup::setup;
use crate::state::{Component, DeviceModel, Variable, VariableAttributeType};
use core::fmt;
use ocpp_client::{ConnectOptions, NegotiatedClient, OcppVersion};

/// Failure from [`connect_and_setup`]: the initial WebSocket dial/handshake to the CSMS failed,
/// the CSMS negotiated a version this crate can't yet drive a full session in, or the connection
/// succeeded and [`setup`](crate::setup) itself then failed starting the hardware.
#[derive(Debug)]
pub enum ConnectAndSetupError<S> {
    /// The WebSocket connection to the CSMS could not be established.
    Connect(Box<dyn std::error::Error + Send + Sync>),
    /// The CSMS negotiated a real OCPP version over the WebSocket handshake that this build was
    /// not compiled with support for. Only reachable if `versions` names a version whose Cargo
    /// feature is off - which `ocpp_client::connect` would not offer in the first place - so this
    /// is a defensive case rather than one a correctly-configured charge point meets.
    ///
    /// Before A1/A2 this was the *normal* outcome of negotiating 1.6J or 2.0.1, because only 2.1
    /// could run a session. All three now can.
    UnsupportedNegotiatedVersion(OcppVersion),
    /// The CSMS connection succeeded, but starting the hardware (see
    /// [`ChargePoint::start`](crate::hardware::ChargePoint::start)) failed.
    Start(S),
}

impl<S: fmt::Display> fmt::Display for ConnectAndSetupError<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(err) => write!(f, "failed to connect to CSMS: {err}"),
            Self::UnsupportedNegotiatedVersion(version) => write!(
                f,
                "CSMS negotiated {version:?}, which this crate can't yet run a full session in"
            ),
            Self::Start(err) => write!(f, "failed to start hardware: {err}"),
        }
    }
}

impl<S: fmt::Debug + fmt::Display> std::error::Error for ConnectAndSetupError<S> {}

/// Dials `address` over WebSocket, offering `versions` (or every version compiled into this
/// build, if `None`) and letting the CSMS pick one (via `ocpp_client::connect`), then runs
/// [`setup`](crate::setup) against the resulting client if it negotiated OCPP 2.1 - the only
/// version `setup()` can currently drive end-to-end (see this module's docs). The "batteries
/// included" entry point for std/tokio users who don't need a custom transport.
pub async fn connect_and_setup<T, E, C, X, B>(
    charge_point: T,
    address: &str,
    versions: Option<&[OcppVersion]>,
    options: Option<ConnectOptions<'_>>,
    executor: X,
    backoff: B,
) -> Result<ChargePointRuntime<T>, ConnectAndSetupError<T::StartError>>
where
    T: ChargePoint<E, C>,
    E: Evse<C>,
    C: Connector,
    // `Clone` because the profile-switching loop (A9) is spawned alongside the session rather
    // than inside it: `setup()` takes the executor by value and knows nothing about redial
    // targets. `TokioExecutor` - the only executor this std/tokio entry point is built for - is
    // `Copy`.
    X: Executor + Clone,
    B: Backoff + Clone + Send + Sync + 'static,
{
    // A5/A6: a caller who supplied `ConnectOptions` is configuring the transport deliberately and
    // is left alone; one who didn't gets the reconnect backoff and per-call timeout OCPP models as
    // device-model variables, so what a CSMS reads there matches what the connection does.
    let options = options.unwrap_or_else(connection_options_from_device_model);
    // A9: the redial target has to exist before the connection does, because it *is* the
    // connection's reconnector. Which version it redials is filled in below, once the CSMS has
    // picked one.
    let target = ConnectionTarget::new(address, &options);
    let negotiated = ocpp_client::connect(address, versions, Some(target.install(options)))
        .await
        .map_err(ConnectAndSetupError::Connect)?;

    // `SystemMonotonicClock`/`SystemClock` (std-backed) rather than caller-supplied parameters -
    // this function is already std/tokio-only "batteries included" (it dials a real WebSocket),
    // so there is no embedded-target caller for whom a different clock would matter, the same
    // reasoning `executor.spawn`'s tokio dependency here already follows.
    match negotiated {
        NegotiatedClient::V2_1(client) => {
            target.set_version(OcppVersion::V2_1);
            setup_ocpp_2_1(charge_point, client, executor, backoff, target).await
        }
        #[cfg(feature = "ocpp_2_0_1")]
        NegotiatedClient::V2_0_1(client) => {
            target.set_version(OcppVersion::V2_0_1);
            setup_ocpp_2_0_1(charge_point, client, executor, backoff, Some(target)).await
        }
        #[cfg(feature = "ocpp_1_6")]
        NegotiatedClient::V1_6(client) => {
            target.set_version(OcppVersion::V1_6);
            setup_ocpp_1_6(charge_point, client, executor, backoff).await
        }
    }
}

/// Runs a full OCPP 2.1 session against `client`, adding the network-profile switching
/// [`setup`](crate::setup) itself cannot register - it takes no redial target, because a caller
/// who built their own client owns their own transport.
#[cfg(feature = "ocpp_2_1")]
async fn setup_ocpp_2_1<T, E, C, X, B>(
    charge_point: T,
    client: ocpp_client::ocpp_2_1::OCPP2_1Client,
    executor: X,
    backoff: B,
    target: alloc::sync::Arc<ConnectionTarget>,
) -> Result<ChargePointRuntime<T>, ConnectAndSetupError<T::StartError>>
where
    T: ChargePoint<E, C>,
    E: Evse<C>,
    C: Connector,
    X: Executor + Clone,
    B: Backoff + Clone + Send + Sync + 'static,
{
    let runtime = setup(
        charge_point,
        client.clone(),
        executor.clone(),
        backoff.clone(),
        SystemMonotonicClock,
        SystemClock,
    )
    .await
    .map_err(ConnectAndSetupError::Start)?;

    let actor = runtime.actor();
    executor.spawn(alloc::boxed::Box::pin(async move {
        crate::network_switch::run_network_profile_switching(&actor, &target, &client, &backoff)
            .await;
    }));
    Ok(runtime)
}

/// Reads an integer `OCPPCommCtrlr` variable out of a fresh [`DeviceModel`], falling back to
/// `default` when it is absent, unparseable or `0`.
///
/// A *fresh* model on purpose: these values are needed to build the connection, which happens
/// before there is an actor to ask. See [`connection_options_from_device_model`] for what that
/// means and what it costs.
fn ocpp_comm_ctrlr_secs(model: &DeviceModel, variable: &str, default: u64) -> u64 {
    let component = Component {
        name: "OCPPCommCtrlr".into(),
        instance: None,
        evse: None,
    };
    let variable = Variable {
        name: variable.into(),
        instance: None,
    };
    model
        .get(&component, &variable)
        .and_then(|definition| definition.attribute(VariableAttributeType::Actual))
        .and_then(|attribute| attribute.value.parse::<u64>().ok())
        .filter(|value| *value != 0)
        .unwrap_or(default)
}

/// Fills in the transport settings OCPP models as device-model variables - the reconnect backoff
/// (`RetryBackOffWaitMinimum`/`RetryBackOffRepeatTimes`, A5) and the per-call timeout
/// (`MessageTimeout[Default]`, A6) - for a caller who did not supply its own
/// [`ConnectOptions`].
///
/// **What this does and does not give you.** The values are read from a freshly-built
/// [`DeviceModel`], i.e. this crate's registered defaults, because the connection has to exist
/// before the actor that owns the live model does. So a CSMS *reading* these variables sees the
/// values genuinely in force, and a CSMS *writing* them changes what the next connection uses -
/// not the current one. Applying a change to the *live* connection is A5's remaining half and is
/// not implemented: [`crate::network_switch`] can re-point where a connection goes, but the
/// backoff and timeout are fixed in the transport when it is built.
///
/// Two honest gaps rather than approximations. `RetryBackOffRandomRange` (jitter) has **no**
/// counterpart in the transport's reconnect policy, so it is registered as `0` and applied
/// nowhere; reporting a jitter this charge point does not add would be worse than reporting none.
/// And `RetryBackOffRepeatTimes` is a *doubling count* where the transport takes a maximum delay,
/// so it is converted (`minimum × 2^repeats`) rather than stored twice - the two express the same
/// curve, and keeping both would let them disagree.
///
/// An explicit `options` is left exactly as the caller wrote it: someone who reached for
/// `ConnectOptions` is configuring the transport deliberately, and silently overriding their
/// timeout with a device-model default would be the opposite of helpful.
fn connection_options_from_device_model<'a>() -> ConnectOptions<'a> {
    use core::time::Duration;

    let model = DeviceModel::new();
    let minimum = ocpp_comm_ctrlr_secs(&model, "RetryBackOffWaitMinimum", 1);
    let repeats = ocpp_comm_ctrlr_secs(&model, "RetryBackOffRepeatTimes", 5).min(16);
    let message_timeout = ocpp_comm_ctrlr_secs(&model, "MessageTimeout", 30);

    ConnectOptions {
        timeout: Some(Duration::from_secs(message_timeout)),
        reconnect: ocpp_client::ReconnectBehavior::Enabled(ocpp_client::ReconnectPolicy {
            initial_delay: Duration::from_secs(minimum),
            max_delay: Duration::from_secs(minimum.saturating_mul(1u64 << repeats)),
            multiplier: 2,
        }),
        ..Default::default()
    }
}

/// Runs a full OCPP 2.0.1 session against `client` (A1).
///
/// Registers every functional block 2.0.1 has an adapter for. **`SecurityEventNotification` is
/// absent**, and not because this crate skipped it: `ocpp-client` 0.2.1 generates no 2.0.1 action
/// for it at all (verified against its `ocpp_2_0_1::actions` list - see
/// `docs/PRODUCTION-ROADMAP.md` D1). Security events raised on a 2.0.1 connection are recorded in
/// the durable log and reach no CSMS until that lands upstream.
#[cfg(feature = "ocpp_2_0_1")]
async fn setup_ocpp_2_0_1<T, E, C, X, B>(
    charge_point: T,
    client: ocpp_client::ocpp_2_0_1::OCPP2_0_1Client,
    executor: X,
    backoff: B,
    target: Option<alloc::sync::Arc<ConnectionTarget>>,
) -> Result<ChargePointRuntime<T>, ConnectAndSetupError<T::StartError>>
where
    T: ChargePoint<E, C>,
    E: Evse<C>,
    C: Connector,
    X: Executor,
    B: Backoff + Clone + Send + Sync + 'static,
{
    let mut builder = ChargePointBuilder::start(charge_point, executor)
        .await
        .map_err(ConnectAndSetupError::Start)?
        .provisioning(&client, backoff.clone(), SystemMonotonicClock)
        .await
        .status_notifications(&client)
        .await
        .transaction_events(&client)
        .await
        .authorization(&client, SystemClock)
        .await
        .clear_cache(&client)
        .await
        .network_profiles(&client)
        .await
        .remote_control(&client)
        .await
        .trigger_message(&client)
        .await
        .availability_control(&client)
        .await
        .reset(&client)
        .await
        .device_model(&client)
        .await
        .meter_values(&client, backoff.clone(), SystemClock)
        .await;

    // C3.1: the same capability gating `setup()` applies - an absent capability means no handler,
    // so the CSMS gets `NotImplemented` rather than a handler backed by hardware that can't.
    let capabilities = builder.capabilities();
    if capabilities.reservation {
        builder = builder.reservation(&client).await;
    }
    if capabilities.local_auth_list {
        builder = builder.local_authorization_list(&client).await;
    }
    if capabilities.tariff_and_cost {
        builder = builder.cost(&client).await;
    }
    if capabilities.smart_charging {
        builder = builder
            .smart_charging(
                &client,
                alloc::sync::Arc::new(crate::smart_charging::ChargingLimitProjection::new()),
                SystemClock,
                backoff.clone(),
            )
            .await;
    }

    // A9: move the connection when the CSMS's selected profile changes.
    if let Some(target) = target {
        builder = builder.network_profile_switching(&target, client.clone(), backoff.clone());
    }

    // A7: sweep the queues registered above on OCPP's own MessageAttemptInterval - see
    // `ChargePointBuilder::offline_queue_retries`.
    Ok(builder.offline_queue_retries(backoff, 60).build())
}

/// Runs a full OCPP 1.6J session against `client` (A1).
///
/// 1.6J needs more than a different client type. It has no EVSE concept - connectors are
/// addressed by a single flat id - so every block whose request or report names a connector goes
/// through a topology-aware wrapper built from [`ChargePointBuilder::connector_counts`], and the
/// transaction notifier additionally caches the CSMS-assigned transaction ids that 1.6J, alone
/// among the three versions, hands back rather than letting the charge point mint.
///
/// Two blocks are absent because 1.6J has no such messages, not because they were skipped:
/// `SecurityEventNotification` and `CostUpdated`. Reporting (`GetBaseReport`/`GetReport`) is
/// likewise 2.x-only; 1.6J's flat `GetConfiguration` covers the same ground and *is* registered.
#[cfg(feature = "ocpp_1_6")]
async fn setup_ocpp_1_6<T, E, C, X, B>(
    charge_point: T,
    client: ocpp_client::ocpp_1_6::OCPP1_6Client,
    executor: X,
    backoff: B,
) -> Result<ChargePointRuntime<T>, ConnectAndSetupError<T::StartError>>
where
    T: ChargePoint<E, C>,
    E: Evse<C>,
    C: Connector,
    X: Executor,
    B: Backoff + Clone + Send + Sync + 'static,
{
    let builder = ChargePointBuilder::start(charge_point, executor)
        .await
        .map_err(ConnectAndSetupError::Start)?;
    let counts = builder.connector_counts();

    let status = crate::availability::Ocpp1_6StatusNotifier::new(client.clone(), counts.clone());
    let transactions = alloc::sync::Arc::new(crate::transactions::Ocpp1_6TransactionNotifier::new(
        client.clone(),
        counts.clone(),
    ));
    let remote =
        crate::remote_control::Ocpp1_6RemoteControlHandler::new(client.clone(), counts.clone());
    let trigger =
        crate::remote_control::Ocpp1_6TriggerMessageHandler::new(client.clone(), counts.clone());
    let availability =
        crate::availability::Ocpp1_6ChangeAvailabilityHandler::new(client.clone(), counts.clone());
    let meter =
        crate::meter_values::Ocpp1_6MeterValuesNotifier::new(client.clone(), counts.clone());

    let mut builder = builder
        .provisioning(&client, backoff.clone(), SystemMonotonicClock)
        .await
        .status_notifications(&status)
        .await
        .transaction_events(&transactions)
        .await
        .authorization(&client, SystemClock)
        .await
        .clear_cache(&client)
        .await
        .remote_control(&remote)
        .await
        .trigger_message(&trigger)
        .await
        .availability_control(&availability)
        .await
        .reset(&client)
        .await
        .configuration(&client)
        .await
        .meter_values(&meter, backoff.clone(), SystemClock)
        .await;

    let capabilities = builder.capabilities();
    if capabilities.reservation {
        builder = builder
            .reservation(&crate::reservation::Ocpp1_6ReserveNowHandler::new(
                client.clone(),
                counts.clone(),
            ))
            .await;
    }
    if capabilities.local_auth_list {
        builder = builder.local_authorization_list(&client).await;
    }
    if capabilities.smart_charging {
        builder = builder
            .smart_charging(
                &crate::smart_charging::Ocpp1_6SmartChargingHandler::new(client.clone(), counts),
                alloc::sync::Arc::new(crate::smart_charging::ChargingLimitProjection::new()),
                SystemClock,
                backoff.clone(),
            )
            .await;
    }

    // A7: sweep the queues registered above on OCPP's own MessageAttemptInterval - see
    // `ChargePointBuilder::offline_queue_retries`.
    Ok(builder.offline_queue_retries(backoff, 60).build())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_connection_settings_come_from_the_device_models_own_values() {
        use core::time::Duration;

        let options = connection_options_from_device_model();

        // `MessageTimeout[Default]`'s registered value, so a CSMS reading it sees what the
        // connection actually waits.
        assert_eq!(options.timeout, Some(Duration::from_secs(30)));

        let ocpp_client::ReconnectBehavior::Enabled(policy) = options.reconnect else {
            panic!("reconnect should stay enabled");
        };
        // `RetryBackOffWaitMinimum` = 1 s, doubling `RetryBackOffRepeatTimes` = 5 times.
        assert_eq!(policy.initial_delay, Duration::from_secs(1));
        assert_eq!(policy.max_delay, Duration::from_secs(32));
        assert_eq!(policy.multiplier, 2);
    }

    #[test]
    fn a_repeat_count_a_csms_could_write_cannot_overflow_the_maximum_delay() {
        // `1 << repeats` with an unbounded `repeats` is a shift overflow, and a CSMS can write any
        // integer into this variable - so the conversion clamps rather than trusting it.
        let model = crate::state::DeviceModel::new();
        let repeats = ocpp_comm_ctrlr_secs(&model, "RetryBackOffRepeatTimes", 5).min(16);
        assert!(repeats <= 16);
        assert!(1u64.checked_shl(repeats as u32).is_some());
    }

    #[test]
    fn an_absent_or_zero_variable_falls_back_rather_than_producing_a_zero_delay() {
        let model = crate::state::DeviceModel::new();

        // A variable this crate does not register at all.
        assert_eq!(ocpp_comm_ctrlr_secs(&model, "NotAVariable", 7), 7);
        // `WebSocketPingInterval` is registered as `0` (A4 has no transport support), so it
        // exercises the zero path against a real registered variable.
        assert_eq!(ocpp_comm_ctrlr_secs(&model, "WebSocketPingInterval", 7), 7);
    }
}
