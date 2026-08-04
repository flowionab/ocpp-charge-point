//! Live `ocpp-client` WebSocket wiring for OCPP 2.1, this crate's primary protocol target (see
//! `CLAUDE.md`). [`setup`](crate::setup) itself only takes an already-connected CSMS client -
//! [`connect_and_setup`] closes that gap for std/tokio users by dialing `address` with
//! `ocpp_client::connect_2_1` first, then handing the resulting client straight to `setup`.
//! Embedded targets, or std users who need a non-WebSocket transport, still construct their own
//! client and call [`setup`](crate::setup) directly.

use crate::ChargePointRuntime;
use crate::executor::Executor;
use crate::hardware::{ChargePoint, Connector, Evse};
use crate::provisioning::Backoff;
use crate::setup::setup;
use core::fmt;
use ocpp_client::ConnectOptions;

/// Failure from [`connect_and_setup`]: either the initial WebSocket dial/handshake to the CSMS
/// failed, or it succeeded and [`setup`](crate::setup) itself then failed starting the
/// hardware.
#[derive(Debug)]
pub enum ConnectAndSetupError<S> {
    /// The WebSocket connection to the CSMS could not be established.
    Connect(Box<dyn std::error::Error + Send + Sync>),
    /// The CSMS connection succeeded, but starting the hardware (see
    /// [`ChargePoint::start`](crate::hardware::ChargePoint::start)) failed.
    Start(S),
}

impl<S: fmt::Display> fmt::Display for ConnectAndSetupError<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(err) => write!(f, "failed to connect to CSMS: {err}"),
            Self::Start(err) => write!(f, "failed to start hardware: {err}"),
        }
    }
}

impl<S: fmt::Debug + fmt::Display> std::error::Error for ConnectAndSetupError<S> {}

/// Dials `address` as an OCPP 2.1 CSMS over WebSocket (via `ocpp_client::connect_2_1`), then
/// runs [`setup`](crate::setup) against the resulting client - the "batteries included" entry
/// point for std/tokio users who don't need a custom transport. Equivalent to calling
/// `ocpp_client::connect_2_1(address, options)` and passing the result to
/// [`setup`](crate::setup) by hand.
pub async fn connect_and_setup<T, E, C, X, B>(
    charge_point: T,
    address: &str,
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
    let client = ocpp_client::connect_2_1(address, options)
        .await
        .map_err(ConnectAndSetupError::Connect)?;

    setup(charge_point, client, executor, backoff)
        .await
        .map_err(ConnectAndSetupError::Start)
}
