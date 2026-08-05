//! Live `ocpp-client` WebSocket wiring, this crate's std/tokio "batteries included" entry point.
//! [`setup`](crate::setup) itself only takes an already-connected CSMS client -
//! [`connect_and_setup`] closes that gap by dialing `address` with `ocpp_client::connect` first
//! (offering whichever OCPP versions `versions` names, or every version compiled into this
//! build if `None` - the CSMS picks one via the WebSocket subprotocol handshake, per RFC 6455),
//! then handing the negotiated client straight to `setup`. Embedded targets, or std users who
//! need a non-WebSocket transport, still construct their own client and call
//! [`setup`](crate::setup) directly.
//!
//! Negotiating down to 1.6J or 2.0.1 succeeds at the WebSocket layer - the CSMS really did agree
//! to speak that version - but this crate can't yet drive a full session in either: `setup()`
//! requires a client implementing every functional block's trait, and today only `OCPP2_1Client`
//! does (see `docs/ROADMAP.md` §0's "protocol-version-independent core → version adapters" item
//! for which pieces exist per version so far). [`connect_and_setup`] surfaces that as
//! [`ConnectAndSetupError::UnsupportedNegotiatedVersion`] rather than pretending it can proceed -
//! offering 1.6J/2.0.1 in `versions` only makes sense today if you're prepared to handle that
//! error (e.g. to fail loudly when pointed at an old CSMS, rather than silently only ever trying
//! 2.1). Restrict `versions` to `&[OcppVersion::V2_1]` to negotiate 2.1 only, matching this
//! function's behavior before version negotiation existed.

use crate::ChargePointRuntime;
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
    /// The CSMS negotiated a real OCPP version over the WebSocket handshake, but this crate has
    /// no [`setup`](crate::setup)-compatible client for it yet - see this module's docs.
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

    let client = match negotiated {
        NegotiatedClient::V2_1(client) => client,
        #[cfg(feature = "ocpp_1_6")]
        NegotiatedClient::V1_6(_) => {
            return Err(ConnectAndSetupError::UnsupportedNegotiatedVersion(
                OcppVersion::V1_6,
            ));
        }
        #[cfg(feature = "ocpp_2_0_1")]
        NegotiatedClient::V2_0_1(_) => {
            return Err(ConnectAndSetupError::UnsupportedNegotiatedVersion(
                OcppVersion::V2_0_1,
            ));
        }
    };

    setup(charge_point, client, executor, backoff)
        .await
        .map_err(ConnectAndSetupError::Start)
}
