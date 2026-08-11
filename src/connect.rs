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
use crate::payload_limit::PayloadLimits;
use crate::provisioning::Backoff;
use crate::state::{Component, DeviceModel, Variable, VariableAttributeType};
use core::fmt;
use ocpp_client::{ConnectOptions, NegotiatedClient, OcppVersion};

/// A CSMS address with any `user:password@` userinfo stripped, for logging.
///
/// OCPP carries security profile 1/2 credentials in an HTTP Basic header rather than the URL -
/// see [`crate::security_profile::BasicAuthPassword`], whose `Debug` prints a placeholder for the
/// same reason - but nothing stops an integrator passing `wss://cp001:secret@csms.example/` as
/// `address`, and the first thing this crate would otherwise do with it is log it. Strips
/// everything between `//` and the last `@` of the authority.
fn redacted_endpoint(address: &str) -> alloc::string::String {
    use alloc::string::{String, ToString};

    let Some((scheme, rest)) = address.split_once("//") else {
        return address.to_string();
    };
    // Only the authority can hold userinfo; a later `@` (in a path or query) must survive.
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let (authority, path) = rest.split_at(authority_end);
    match authority.rsplit_once('@') {
        Some((_userinfo, host)) => {
            let mut out = String::from(scheme);
            out.push_str("//");
            out.push_str("***@");
            out.push_str(host);
            out.push_str(path);
            out
        }
        None => address.to_string(),
    }
}

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
///
/// `payload_limits` (`None` for [`PayloadLimits::default`]) sets the inbound-frame ceiling every
/// *redial* is guarded with (F5.2) - see [`crate::payload_limit`] for exactly what that covers.
/// It does **not** cover the initial dial this function performs itself: `ocpp_client::connect`
/// builds that connection's transport internally and exposes no hook to guard it, which is a real
/// upstream gap this crate cannot close without duplicating transport/handshake logic
/// `CLAUDE.md` reserves to `ocpp-client`.
pub async fn connect_and_setup<T, E, C, X, B>(
    charge_point: T,
    address: &str,
    versions: Option<&[OcppVersion]>,
    options: Option<ConnectOptions<'_>>,
    payload_limits: Option<PayloadLimits>,
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
    // F5.2: configures the ceiling every redial through `target` is guarded with - see this
    // function's own docs for why the *initial* dial just below is not covered.
    target.set_max_inbound_frame_bytes(payload_limits.unwrap_or_default().max_inbound_frame_bytes);
    tracing::info!(
        endpoint = %redacted_endpoint(address),
        offered_versions = ?versions,
        "dialing the CSMS"
    );
    let negotiated =
        match ocpp_client::connect(address, versions, Some(target.install(options))).await {
            Ok(negotiated) => negotiated,
            Err(error) => {
                // The single most-asked question during commissioning is "why won't it connect", and
                // until now this path answered it only by returning an error to a caller that
                // typically just propagates it.
                tracing::warn!(
                    endpoint = %redacted_endpoint(address),
                    %error,
                    "the CSMS dial failed"
                );
                return Err(ConnectAndSetupError::Connect(error));
            }
        };

    // `SystemMonotonicClock`/`SystemClock` (std-backed) rather than caller-supplied parameters -
    // this function is already std/tokio-only "batteries included" (it dials a real WebSocket),
    // so there is no embedded-target caller for whom a different clock would matter, the same
    // reasoning `executor.spawn`'s tokio dependency here already follows.
    // Which version the CSMS picked decides which functional blocks this session will have (see
    // this module's docs - what each version gets is deliberately not identical), so it is the
    // first thing to establish when a charge point misbehaves against one CSMS but not another.
    match negotiated {
        NegotiatedClient::V2_1(client) => {
            tracing::info!(version = "2.1", "the CSMS negotiated an OCPP version");
            target.set_version(OcppVersion::V2_1);
            setup_ocpp_2_1(charge_point, client, executor, backoff, target).await
        }
        #[cfg(feature = "ocpp_2_0_1")]
        NegotiatedClient::V2_0_1(client) => {
            tracing::info!(version = "2.0.1", "the CSMS negotiated an OCPP version");
            target.set_version(OcppVersion::V2_0_1);
            setup_ocpp_2_0_1(charge_point, client, executor, backoff, Some(target)).await
        }
        #[cfg(feature = "ocpp_1_6")]
        NegotiatedClient::V1_6(client) => {
            tracing::info!(version = "1.6J", "the CSMS negotiated an OCPP version");
            target.set_version(OcppVersion::V1_6);
            setup_ocpp_1_6(charge_point, client, executor, backoff, target).await
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
    // CV2.6 (J01/J02): the two meter-data blocks get the actor-aware 2.1 adapters rather than the
    // bare client, so `SampledDataCtrlr`/`AlignedDataCtrlr`'s measurand lists are read per message
    // and a CSMS narrowing one sees it take effect. They are built from the actor `setup` creates,
    // which is why they arrive as factories - see `setup_with_meter_data_notifiers`.
    let transactions_client = client.clone();
    let meter_values_client = client.clone();
    let runtime = crate::setup::setup_with_meter_data_notifiers(
        charge_point,
        client.clone(),
        move |actor| {
            crate::transactions::Ocpp2_1TransactionNotifier::with_clock(
                transactions_client,
                SystemClock,
                actor.clone(),
            )
        },
        move |actor| {
            crate::meter_values::Ocpp2_1MeterValuesNotifier::with_clock(
                meter_values_client,
                SystemClock,
                actor.clone(),
            )
        },
        executor.clone(),
        backoff.clone(),
        SystemMonotonicClock,
        SystemClock,
    )
    .await
    .map_err(ConnectAndSetupError::Start)?;

    // F5.2: from here on, a redial's `SizeLimitedStream` can raise `MemoryExhaustion` on this
    // charge point's own actor when it refuses an oversized frame.
    target.attach_security_reporting(runtime.actor());

    // B2.6's priority charging is registered here rather than in `setup()` for the same reason
    // network switching is: it is 2.1-only, and `setup()` is generic over a CSMS client that may
    // equally be a 2.0.1 one. Adding a 2.1-only bound there would make the whole "everything on"
    // wrapper unusable on 2.0.1, which is the exact coupling C4's builder exists to avoid.
    if runtime.actor().state().capabilities.smart_charging {
        use crate::smart_charging::{UpdateDynamicScheduleHandler, UsePriorityChargingHandler};
        client
            .register_use_priority_charging_handler(runtime.actor())
            .await;
        let changes = runtime.actor().subscribe_priority_charging_changes();
        let notifier = client.clone();
        executor.spawn(alloc::boxed::Box::pin(async move {
            crate::smart_charging::run_priority_charging_notifications(changes, &notifier).await;
        }));

        client
            .register_update_dynamic_schedule_handler(runtime.actor())
            .await;
        let pull_actor = runtime.actor();
        let puller = client.clone();
        let pull_backoff = backoff.clone();
        executor.spawn(alloc::boxed::Box::pin(async move {
            // 30 s between due-ness checks, not between pulls: each dynamic profile carries its
            // own `dynUpdateInterval`, and a charge point with none installed asks for nothing.
            crate::smart_charging::run_dynamic_schedule_pulls(
                &pull_actor,
                &puller,
                &SystemClock,
                &pull_backoff,
                30,
            )
            .await;
        }));
    }

    // A4: carries a CSMS `SetVariables` on `WebSocketPingInterval` onto the running connection.
    // Spawned rather than folded into the switching loop below because the two watch the same
    // state for unrelated reasons, and a keepalive change must not wait on a switch grace period.
    let ping_actor = runtime.actor();
    let ping_client = client.clone();
    executor.spawn(alloc::boxed::Box::pin(async move {
        crate::keepalive::run_ping_interval_updates(&ping_actor, &ping_client).await;
    }));

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

/// Fills in the transport settings OCPP models as device-model variables - the exponential part of
/// the reconnect backoff (`RetryBackOffWaitMinimum`/`RetryBackOffRepeatTimes`, A5) and the per-call
/// timeout
/// (`MessageTimeout[Default]`, A6) - for a caller who did not supply its own
/// [`ConnectOptions`].
///
/// **What this does and does not give you.** The values are read from a freshly-built
/// [`DeviceModel`], i.e. this crate's registered defaults, because the connection has to exist
/// before the actor that owns the live model does. So a CSMS *reading* these variables sees the
/// values genuinely in force, and a CSMS *writing* them changes what the next connection uses -
/// not the current one. That is not fixable from here for these two: [`crate::network_switch`] can
/// re-point where a connection goes, but the exponential backoff and the timeout are sealed into
/// the transport when it is built. `RetryBackOffRandomRange` is the exception - see below.
///
/// `RetryBackOffRepeatTimes` is a *doubling count* where the transport takes a maximum delay,
/// so it is converted (`minimum × 2^repeats`) rather than stored twice - the two express the same
/// curve, and keeping both would let them disagree.
///
/// `RetryBackOffRandomRange` (jitter) is **not** here, because it is not the transport's to apply:
/// this crate owns the reconnector ([`crate::network_switch::ConnectionTarget`]) and adds the
/// random part itself on every redial, reading the range from the live device model. That makes it
/// the one piece of the reconnect back-off a CSMS can change without waiting for the next
/// connection - and it means a caller who supplies their own reconnector, rather than letting
/// `connect_and_setup` install one, gets no jitter.
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

    // A4: what a CSMS reads from `WebSocketPingInterval` has to be what the connection does.
    // `ConnectOptions::default()` enables keepalive at 60s from `ocpp-client` 0.3.0, which would
    // make this charge point ping on a cadence its own device model reports as `0`. Seeding from
    // the registered value instead keeps the two honest; `crate::keepalive` then carries CSMS
    // *writes* onto the running connection, which is the half a connect-time read cannot cover.
    let ping_interval = ocpp_comm_ctrlr_secs(&model, "WebSocketPingInterval", 0);

    ConnectOptions {
        timeout: Some(Duration::from_secs(message_timeout)),
        keepalive: if ping_interval == 0 {
            ocpp_client::KeepaliveBehavior::Disabled
        } else {
            ocpp_client::KeepaliveBehavior::Enabled(ocpp_client::KeepalivePolicy {
                interval: Duration::from_secs(ping_interval),
                ..Default::default()
            })
        },
        reconnect: ocpp_client::ReconnectBehavior::Enabled(ocpp_client::ReconnectPolicy {
            initial_delay: Duration::from_secs(minimum),
            max_delay: Duration::from_secs(minimum.saturating_mul(1u64 << repeats)),
            multiplier: 2,
            // `false` on purpose, against the upstream default of `true`: as the doc comment
            // above says, jitter is applied by this crate's own reconnector from the live
            // `RetryBackOffRandomRange`. Leaving the transport's own half-jitter on as well
            // would compound two independent random draws into a delay curve neither the
            // device model nor `ocpp-client` describes, and would silently ignore a CSMS that
            // set `RetryBackOffRandomRange` to `0` to ask for exact delays.
            jitter: false,
        }),
        ..Default::default()
    }
}

/// Runs a full OCPP 2.0.1 session against `client` (A1).
///
/// Registers every functional block 2.0.1 has an adapter for - including
/// `SecurityEventNotification`, which used to be the one documented omission here because
/// `ocpp-client` 0.2.0 generated no 2.0.1 action for it. D1 landed that upstream and 0.2.2 carries
/// it, so a 2.0.1 session reports security events like a 2.1 one (F4.4).
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
    let builder = ChargePointBuilder::start(charge_point, executor)
        .await
        .map_err(ConnectAndSetupError::Start)?;
    // CV2.6 (J01/J02), mirroring the 2.1 path: the meter-data blocks get the actor-aware adapters
    // so the measurand lists are honoured rather than merely settable.
    let transactions = crate::transactions::Ocpp2_0_1TransactionNotifier::with_clock(
        client.clone(),
        SystemClock,
        builder.actor(),
    );
    let meter_values = crate::meter_values::Ocpp2_0_1MeterValuesNotifier::with_clock(
        client.clone(),
        SystemClock,
        builder.actor(),
    );
    let mut builder = builder
        .provisioning(&client, backoff.clone(), SystemMonotonicClock)
        .await
        .status_notifications(&client, backoff.clone())
        .await
        .transaction_events(&transactions)
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
        .meter_values(&meter_values, backoff.clone(), SystemClock)
        .await
        // F4.4: 2.0.1 reports security events now. This registration was absent because
        // `ocpp-client` 0.2.0 generated no 2.0.1 action for `SecurityEventNotification` at all;
        // D1 added it upstream and 0.2.2 carries it.
        .security_events(&client)
        .await;

    // C3.1: the same capability gating `setup()` applies - an absent capability means no handler,
    // so the CSMS gets `NotImplemented` rather than a handler backed by hardware that can't.
    let capabilities = builder.capabilities();
    if capabilities.reservation {
        builder = builder
            .reservation(&client)
            .await
            .reservation_status_updates(&client, SystemClock, backoff.clone(), 60);
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
            .await
            // 2.x only - see `ChargePointBuilder::charging_profile_reports`.
            .charging_profile_reports(&client)
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
    target: alloc::sync::Arc<ConnectionTarget>,
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
        .status_notifications(&status, backoff.clone())
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
    let runtime = builder.offline_queue_retries(backoff, 60).build();
    // F5.2: 1.6J gets no `network_profile_switching` registration (no such OCPP message exists
    // for it), but `target` is still installed as the transport's reconnector (see
    // `connect_and_setup`), so its redials still need a security-event destination for an
    // oversized frame `SizeLimitedStream` refuses.
    target.attach_security_reporting(runtime.actor());
    Ok(runtime)
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
        // `WebSocketPingInterval` is registered as `0` - OCPP's spelling of "do not ping", and
        // now a real setting rather than a placeholder (see `crate::keepalive`) - so it exercises
        // the zero path against a variable this crate actually registers.
        assert_eq!(ocpp_comm_ctrlr_secs(&model, "WebSocketPingInterval", 7), 7);
    }

    /// A4. `ConnectOptions::default()` enables keepalive at 60s from `ocpp-client` 0.3.0, so
    /// taking the default would have this charge point pinging on a cadence its own
    /// `WebSocketPingInterval` reports as `0`. The registered value wins instead, in both
    /// directions.
    #[test]
    fn keepalive_follows_the_registered_ping_interval_rather_than_the_transports_default() {
        let options = connection_options_from_device_model();

        assert!(
            matches!(options.keepalive, ocpp_client::KeepaliveBehavior::Disabled),
            "a registered WebSocketPingInterval of 0 must mean no unsolicited traffic"
        );
    }
}

#[cfg(test)]
mod endpoint_redaction_tests {
    use super::redacted_endpoint;

    #[test]
    fn userinfo_in_the_dial_address_never_reaches_the_log() {
        assert_eq!(
            redacted_endpoint("wss://cp001:hunter2@csms.example.com/ocpp"),
            "wss://***@csms.example.com/ocpp"
        );
    }

    #[test]
    fn an_address_without_credentials_is_left_alone() {
        // The endpoint is the single most useful field in a commissioning log; redacting it
        // wholesale to be safe would defeat the point of logging it.
        assert_eq!(
            redacted_endpoint("wss://csms.example.com/ocpp/CP001"),
            "wss://csms.example.com/ocpp/CP001"
        );
    }

    #[test]
    fn an_at_sign_after_the_authority_is_not_mistaken_for_userinfo() {
        // A charge point id is free-form and may well contain one; treating the path's `@` as a
        // credential boundary would eat the host and the id both.
        assert_eq!(
            redacted_endpoint("wss://csms.example.com/ocpp/cp@site"),
            "wss://csms.example.com/ocpp/cp@site"
        );
    }

    #[test]
    fn a_password_containing_an_at_sign_is_still_removed_whole() {
        // Splitting on the *last* `@` of the authority, not the first, is what makes this hold.
        assert_eq!(
            redacted_endpoint("wss://cp001:p@ss@csms.example.com/ocpp"),
            "wss://***@csms.example.com/ocpp"
        );
    }

    #[test]
    fn an_address_with_no_scheme_separator_is_returned_unchanged() {
        assert_eq!(redacted_endpoint("csms.example.com"), "csms.example.com");
    }
}
