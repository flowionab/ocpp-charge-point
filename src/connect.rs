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
use crate::provisioning::Backoff;
use crate::setup::setup;
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
    X: Executor,
    B: Backoff + Clone + Send + Sync + 'static,
{
    let negotiated = ocpp_client::connect(address, versions, options)
        .await
        .map_err(ConnectAndSetupError::Connect)?;

    // `SystemMonotonicClock`/`SystemClock` (std-backed) rather than caller-supplied parameters -
    // this function is already std/tokio-only "batteries included" (it dials a real WebSocket),
    // so there is no embedded-target caller for whom a different clock would matter, the same
    // reasoning `executor.spawn`'s tokio dependency here already follows.
    match negotiated {
        NegotiatedClient::V2_1(client) => setup(
            charge_point,
            client,
            executor,
            backoff,
            SystemMonotonicClock,
            SystemClock,
        )
        .await
        .map_err(ConnectAndSetupError::Start),
        #[cfg(feature = "ocpp_2_0_1")]
        NegotiatedClient::V2_0_1(client) => {
            setup_ocpp_2_0_1(charge_point, client, executor, backoff).await
        }
        #[cfg(feature = "ocpp_1_6")]
        NegotiatedClient::V1_6(client) => {
            setup_ocpp_1_6(charge_point, client, executor, backoff).await
        }
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
