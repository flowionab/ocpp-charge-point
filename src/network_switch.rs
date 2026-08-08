//! Moving a live CSMS connection onto a stored network connection profile (A9).
//!
//! [`crate::network_profile`] stores what the CSMS wrote and says which slot the priority order
//! selects. This module is the other half: it makes the connection *go there*, and brings it back
//! if the new address turns out not to work.
//!
//! # How a switch happens
//!
//! Nothing here dials anything directly. A [`ConnectionTarget`] is handed to `ocpp-client` as
//! `ConnectOptions::reconnector` before the first connection, and from then on it is what decides
//! where a dropped connection is redialled. Switching is therefore two steps:
//!
//! 1. point the target at the new address ([`ConnectionTarget::switch_to`]), and
//! 2. close the current connection ([`ConnectionCloser::close_connection`]).
//!
//! `ocpp-client`'s read loop sees the close, asks the target where to go, and reconnects there -
//! *through the same `Client`*. That matters more than it looks: every registered handler, every
//! offline queue and every in-flight request survives the move, so a profile switch is invisible
//! to the rest of this crate. Tearing the client down and dialling again would strand all of it.
//!
//! [`run_network_profile_switching`] performs those two steps automatically whenever the selected
//! profile changes, after a short grace period - see [`SWITCH_GRACE_SECS`].
//!
//! # Rollback
//!
//! A profile the CSMS wrote may simply not work - wrong host, firewalled port, a VPN that is not
//! up on this unit. A charge point that moved to it and stayed there would be unreachable, and
//! unreachable in a way the CSMS cannot fix, because it can no longer talk to the charge point to
//! correct the profile it just wrote. So the target remembers where it came from and reverts
//! after `OCPPCommCtrlr`/`NetworkProfileConnectionAttempts` consecutive failures - OCPP's own
//! variable for exactly this count, not an invented constant.
//!
//! The revert is to the last address that *worked*, not merely the previous one: two switches in
//! a row without a successful connection in between keep the original fallback rather than
//! falling back to an address that was never proven either.
//!
//! # Credentials are not carried across
//!
//! A switch dials the new address with the TLS trust configuration the connection was built with,
//! but **without** the HTTP Basic credentials. Those belong to the CSMS that issued them; sending
//! them to whatever host a profile names would hand this charge point's password to a different
//! server. The original address keeps using them, so a redial that is not a switch is unaffected.
//!
//! This is a real limitation rather than a stance: OCPP carries `basicAuthPassword` in the profile
//! itself, and this crate deliberately does not store it (see
//! [`crate::state::NetworkConnectionProfile::security_profile`]). A profile whose endpoint demands
//! Basic auth will therefore fail to connect and roll back. Closing that gap is workstream F's
//! (security profiles) job, not A9's.

use crate::actor::ChargePointActor;
use crate::network_profile::selected_profile;
use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use core::future::Future;
use core::pin::Pin;
use core::time::Duration;
use ocpp_client::{
    ConnectOptions, OcppVersion, Reconnector, TransportError, TransportSink, TransportStream,
    websocket_transport,
};
use std::sync::Mutex;

/// How many consecutive failed connection attempts trigger a rollback when
/// `OCPPCommCtrlr`/`NetworkProfileConnectionAttempts` is unreadable. Matches the value this crate
/// registers for that variable, so the fallback and the reported value agree.
const DEFAULT_CONNECTION_ATTEMPTS: u32 = 3;

/// How long to wait between deciding to switch and closing the connection.
///
/// A switch is almost always triggered by the CSMS's own `SetNetworkProfile`, and closing
/// immediately races that request's response out of the socket: the CSMS would be left never
/// knowing whether the profile it just wrote was accepted, which is exactly the thing it needs to
/// know before it stops expecting this charge point on the old address. The grace period lets the
/// reply go out first.
///
/// A fixed delay rather than an acknowledgement because there is nothing to acknowledge - OCPP
/// defines no handshake for "the response has been flushed", and the transport does not expose
/// one either. One second is far longer than a socket write and far shorter than any CSMS's
/// patience.
pub const SWITCH_GRACE_SECS: u32 = 1;

/// Where the connection goes when it is (re)dialled, and what to do when it cannot get there.
///
/// Constructed before the first connection and handed to `ocpp-client` via
/// [`ConnectionTarget::install`]; see the module docs for the switch and rollback rules. Cheap to
/// clone (it is used through an `Arc`) and safe to consult from any task.
pub struct ConnectionTarget {
    /// The address this connection started on - the only one the original credentials are ever
    /// sent to (module docs, "Credentials are not carried across").
    origin: String,
    username: Option<String>,
    password: Option<String>,
    tls_config: Option<Arc<ocpp_client::rustls::ClientConfig>>,
    timeout: Option<Duration>,
    inner: Mutex<Inner>,
}

struct Inner {
    /// Where the next dial goes.
    active: String,
    /// The last address known to work, held while `active` is unproven. `None` means `active` is
    /// itself proven (or is the original address, which the initial dial proved).
    fallback: Option<String>,
    /// Consecutive failed attempts on `active` since it was switched to.
    failures: u32,
    /// How many of those failures trigger a rollback.
    attempts_before_rollback: u32,
    /// The negotiated OCPP version, which a redial has to keep speaking. `None` only before the
    /// first connection completes - see [`ConnectionTarget::set_version`].
    version: Option<OcppVersion>,
    /// `OCPPCommCtrlr`/`RetryBackOffRandomRange`: the largest random delay, in seconds, added
    /// before a redial. `0` disables jitter entirely, which is this crate's registered default.
    jitter_range_secs: u32,
    /// The jitter PRNG's state - see [`ConnectionTarget::next_jitter_secs`].
    jitter_state: u64,
    /// How many redials this target has been asked for, ever. Only the *difference* matters:
    /// [`run_network_profile_switching`] snapshots it across the switch grace period to tell
    /// "the connection has not moved yet" from "it already moved on its own".
    dials: u64,
}

impl ConnectionTarget {
    /// A target that starts on `address`, reusing `options`' credentials, TLS configuration and
    /// timeout for redials of that same address.
    pub fn new(address: &str, options: &ConnectOptions<'_>) -> Arc<Self> {
        Arc::new(Self {
            origin: address.to_string(),
            username: options.username.map(ToString::to_string),
            password: options.password.map(ToString::to_string),
            tls_config: options.tls_config.clone(),
            timeout: options.timeout,
            inner: Mutex::new(Inner {
                active: address.to_string(),
                fallback: None,
                failures: 0,
                attempts_before_rollback: DEFAULT_CONNECTION_ATTEMPTS,
                version: None,
                jitter_range_secs: 0,
                jitter_state: jitter_seed(address, options.username),
                dials: 0,
            }),
        })
    }

    /// Installs this target as `options`' reconnector, so every redial asks it where to go.
    ///
    /// A caller who already set [`ConnectOptions::reconnector`] is left alone: they have taken
    /// over redialling deliberately, and overriding that would be worse than not switching
    /// profiles at all.
    pub fn install<'a>(self: &Arc<Self>, mut options: ConnectOptions<'a>) -> ConnectOptions<'a> {
        if options.reconnector.is_none() {
            options.reconnector = Some(Arc::new(TargetReconnector(self.clone())));
        }
        options
    }

    /// Records the OCPP version the connection negotiated, which every redial must keep speaking -
    /// the client's handlers are bound to it.
    ///
    /// Set after the first connection rather than at construction because the version is the
    /// server's choice (RFC 6455 subprotocol negotiation) and is not known until the handshake
    /// completes - while the reconnector has to exist *before* it, to be passed in as an option.
    /// A redial cannot happen before the connection it would replace, so the window where this is
    /// unset is not reachable in practice; it is treated as an error rather than a guess if it
    /// ever is.
    pub fn set_version(&self, version: OcppVersion) {
        self.inner.lock().expect("target lock").version = Some(version);
    }

    /// Records how many consecutive failures trigger a rollback, from
    /// `OCPPCommCtrlr`/`NetworkProfileConnectionAttempts`. Zero is ignored: it would roll back
    /// before ever attempting the new profile, which would make switching impossible.
    pub fn set_connection_attempts(&self, attempts: u32) {
        if attempts > 0 {
            self.inner
                .lock()
                .expect("target lock")
                .attempts_before_rollback = attempts;
        }
    }

    /// Records the largest random delay a redial may add, from
    /// `OCPPCommCtrlr`/`RetryBackOffRandomRange` (A5). `0` disables jitter.
    ///
    /// Read from the **live** device model by [`run_network_profile_switching`] on every state
    /// change, so unlike the rest of the backoff a CSMS write to this variable takes effect on the
    /// connection that is already running. That asymmetry is not a design choice so much as a fact
    /// about who owns which half: `initial_delay`/`max_delay` are fixed inside the transport when
    /// it is built, while the random part is added here, by this crate, on every redial.
    pub fn set_jitter_range(&self, seconds: u32) {
        self.inner.lock().expect("target lock").jitter_range_secs = seconds;
    }

    /// How long the next redial waits before dialling, in seconds - a fresh draw from
    /// `[0, RetryBackOffRandomRange]`, or `0` when jitter is switched off.
    ///
    /// This is OCPP's "random part of the back-off time", and it composes with rather than
    /// replaces the transport's exponential backoff: `ocpp-client`'s read loop has already waited
    /// out `ReconnectPolicy::delay_for` by the time it asks this target to dial.
    ///
    /// **What it is for.** A CSMS that goes down takes every charge point it serves down with it,
    /// and they all notice within a second of each other. Without jitter they then retry in
    /// lockstep - the same exponential curve, from the same instant - so the CSMS coming back up
    /// meets its entire fleet arriving simultaneously and goes down again. Spreading the retries
    /// costs a few seconds per station and is the difference between a recovery and a thundering
    /// herd.
    ///
    /// It is deliberately applied to a profile switch too, which is also a redial: a CSMS that
    /// rewrites the network profile of a whole fleet would otherwise point all of them at a new
    /// endpoint at the same instant, which is the same stampede pointed somewhere fresh.
    ///
    /// A xorshift64* generator rather than a `rand` dependency: this needs to spread retries
    /// across a fleet, not resist an adversary, and the crate compiles for bare metal where
    /// pulling in an RNG stack for one number would be an odd trade. The *seed* is what matters
    /// for spreading - see [`jitter_seed`].
    fn next_jitter_secs(&self) -> u32 {
        let mut inner = self.inner.lock().expect("target lock");
        if inner.jitter_range_secs == 0 {
            return 0;
        }
        // xorshift64*, whose state must never be zero.
        let mut state = inner.jitter_state | 1;
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        inner.jitter_state = state;
        let draw = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
        // Inclusive of the range itself, which is what "maximum value for the random part" reads
        // as - and one extra second at the top of a spread does no harm either way.
        (draw >> 33) as u32 % (inner.jitter_range_secs + 1)
    }

    /// The address the next dial will use.
    pub fn address(&self) -> String {
        self.inner.lock().expect("target lock").active.clone()
    }

    /// Points the next dial at `address`, remembering where to roll back to.
    ///
    /// Returns `false` if that is already the active address, so a caller can skip closing a
    /// working connection for no reason.
    pub fn switch_to(&self, address: &str) -> bool {
        let mut inner = self.inner.lock().expect("target lock");
        if inner.active == address {
            return false;
        }
        // Only capture a fallback if there isn't one already: if `active` is itself unproven
        // (a switch that hasn't connected yet), rolling back to it later would be rolling back
        // to an address that never worked either.
        if inner.fallback.is_none() {
            inner.fallback = Some(inner.active.clone());
        }
        // Switching back to the fallback is a rollback, not a new gamble.
        if inner.fallback.as_deref() == Some(address) {
            inner.fallback = None;
        }
        inner.active = address.to_string();
        inner.failures = 0;
        true
    }

    /// Marks the active address as working: it becomes the address a future rollback returns to.
    /// Counts a redial through this target, whatever its outcome. See [`Inner::dials`].
    fn record_dial(&self) {
        let mut inner = self.inner.lock().expect("target lock");
        inner.dials = inner.dials.saturating_add(1);
    }

    /// How many redials this target has been asked for. See [`Inner::dials`].
    fn dial_count(&self) -> u64 {
        self.inner.lock().expect("target lock").dials
    }

    fn record_success(&self) {
        let mut inner = self.inner.lock().expect("target lock");
        inner.fallback = None;
        inner.failures = 0;
    }

    /// Counts a failed attempt on the active address and rolls back once there have been enough
    /// of them. Returns the address rolled back to, if it rolled back.
    fn record_failure(&self) -> Option<String> {
        let mut inner = self.inner.lock().expect("target lock");
        inner.failures = inner.failures.saturating_add(1);
        if inner.failures < inner.attempts_before_rollback {
            return None;
        }
        let fallback = inner.fallback.take()?;
        inner.active = fallback.clone();
        inner.failures = 0;
        Some(fallback)
    }

    /// The address and version for one dial attempt.
    fn dial_parameters(&self) -> Result<(String, OcppVersion), TransportError> {
        let inner = self.inner.lock().expect("target lock");
        let version = inner.version.ok_or_else(|| {
            TransportError::from(
                "cannot redial before the initial connection negotiated an OCPP version",
            )
        })?;
        Ok((inner.active.clone(), version))
    }

    /// Dials the active address once, applying the credential rule in the module docs.
    async fn dial(
        &self,
    ) -> Result<(Box<dyn TransportSink>, Box<dyn TransportStream>), TransportError> {
        let (address, version) = self.dial_parameters()?;
        // A5's random back-off, added on top of the exponential wait the transport has already
        // done - see `next_jitter_secs` for why a charge point wants one at all.
        let jitter = self.next_jitter_secs();
        if jitter > 0 {
            tracing::debug!(
                seconds = jitter,
                "waiting out the reconnect back-off jitter"
            );
            tokio::time::sleep(Duration::from_secs(u64::from(jitter))).await;
        }
        let is_origin = address == self.origin;
        if !is_origin && self.username.is_some() {
            tracing::debug!(
                %address,
                "redialling a switched address without the original Basic credentials"
            );
        }
        let options = ConnectOptions {
            username: is_origin.then_some(self.username.as_deref()).flatten(),
            password: is_origin.then_some(self.password.as_deref()).flatten(),
            timeout: self.timeout,
            tls_config: self.tls_config.clone(),
            ..Default::default()
        };

        self.record_dial();
        match websocket_transport(&address, version, Some(options)).await {
            Ok(transport) => {
                self.record_success();
                Ok(transport)
            }
            Err(error) => {
                if let Some(rolled_back_to) = self.record_failure() {
                    tracing::warn!(
                        failed = %address,
                        reverted_to = %rolled_back_to,
                        "a network profile could not be connected; rolling back to the last working address"
                    );
                }
                Err(error)
            }
        }
    }
}

/// Seeds the jitter generator with something that differs between charge points.
///
/// The seed is the whole point: a generator every station in a fleet seeds identically produces
/// identical "random" delays, which is no better than having none. Three ingredients, in
/// increasing order of how much they help:
///
/// - the CSMS address, which distinguishes fleets but not stations within one;
/// - the Basic-auth username, which in OCPP deployments is usually the charge point's own
///   identity, and is therefore the ingredient that saves a fleet that all boots at once (a
///   regional power cut) rather than merely all disconnecting at once;
/// - the wall clock in nanoseconds, which decorrelates stations whose connections dropped at
///   slightly different moments - the common case, and where a few milliseconds of difference is
///   already enough.
///
/// None is sufficient alone, which is why all three are mixed. FNV-1a because it is four lines and
/// this is a seed, not a checksum.
fn jitter_seed(address: &str, username: Option<&str>) -> u64 {
    fn fnv1a(seed: u64, bytes: &[u8]) -> u64 {
        let mut hash = seed;
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }

    let mut seed = fnv1a(0xcbf2_9ce4_8422_2325, address.as_bytes());
    seed = fnv1a(seed, username.unwrap_or_default().as_bytes());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.subsec_nanos());
    fnv1a(seed, &nanos.to_le_bytes())
}

/// Adapts [`ConnectionTarget`] to `ocpp-client`'s `Reconnector`.
struct TargetReconnector(Arc<ConnectionTarget>);

impl Reconnector for TargetReconnector {
    fn connect<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        (Box<dyn TransportSink>, Box<dyn TransportStream>),
                        TransportError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move { self.0.dial().await })
    }
}

/// Abandons the current CSMS connection, so the transport redials through the
/// [`ConnectionTarget`].
///
/// Kept as this crate's own trait, like every other CSMS-facing capability, so the switching loop
/// is testable without a live socket and works for whichever OCPP version was negotiated.
///
/// **This must not close the client.** The implementations below call
/// [`Client::force_reconnect`], not `Client::disconnect`. From `ocpp-client` 0.3.0 `disconnect()`
/// is sticky and outranks every automatic recovery path - the read loop exits instead of
/// redialling, `force_reconnect()` becomes a no-op, and later sends fail with
/// `ClientError::Closed` - so using it here would take the charge point permanently offline on
/// the first network-profile switch, needing a reboot to recover. Before 0.3.0 the two were
/// indistinguishable from the read loop's side, which is why this used to be spelled as a close.
///
/// The method name is retained from that era; it describes the intent (give up this connection)
/// rather than the mechanism.
///
/// [`Client::force_reconnect`]: ocpp_client::Client::force_reconnect
#[async_trait::async_trait]
pub trait ConnectionCloser {
    /// Abandons the connection so it is redialled through the [`ConnectionTarget`]. Infallible:
    /// the connection is being deliberately discarded, and one that is already broken needs no
    /// help being abandoned.
    async fn close_connection(&self);
}

#[async_trait::async_trait]
impl<T: ConnectionCloser + Send + Sync + ?Sized> ConnectionCloser for Arc<T> {
    async fn close_connection(&self) {
        (**self).close_connection().await
    }
}

/// Moves the connection whenever the selected network profile changes.
///
/// Watches the selected profile (see [`crate::network_profile::selected_profile`]) and, when it
/// names an address other than the one currently in force, points the target at it and - after
/// [`SWITCH_GRACE_SECS`] - closes the connection so the transport redials there. Runs until the
/// task holding it is dropped.
///
/// A charge point whose priority order selects nothing - the common case, since most fleets never
/// send `SetNetworkProfile` - stays where it is. "No profile" is not an instruction to move.
pub async fn run_network_profile_switching<D: ConnectionCloser, B: crate::provisioning::Backoff>(
    actor: &ChargePointActor,
    target: &Arc<ConnectionTarget>,
    closer: &D,
    backoff: &B,
) {
    let mut states = actor.subscribe();
    loop {
        let state = states.borrow();
        // `NetworkProfileConnectionAttempts` is read on every pass rather than cached: a CSMS can
        // write it at any time, and the value that matters is the one in force when a profile is
        // actually being tried.
        target.set_connection_attempts(connection_attempts(&state));
        // Read live, like the attempt count above and for the same reason: this is the one part of
        // the reconnect back-off this crate applies itself, so a CSMS write can reach the running
        // connection rather than only the next one.
        target.set_jitter_range(jitter_range(&state));
        let selected = selected_profile(&state).map(|(_, profile)| profile.csms_url.clone());
        drop(state);

        if let Some(address) = selected
            && target.switch_to(&address)
        {
            tracing::info!(%address, "switching the CSMS connection to a network profile");
            let dials_before = target.dial_count();
            backoff.wait(SWITCH_GRACE_SECS).await;
            // Only abandon the connection if it is still the old one. `switch_to` has already
            // re-pointed the target, so a redial that happened during the grace period - the CSMS
            // dropping us, a keepalive giving up, anything - has *already* landed on the new
            // address, and there is nothing left to move.
            //
            // Forcing anyway is actively harmful: `Client::force_reconnect` tears down whatever
            // connection is current when it is observed, so a force racing an in-flight redial
            // kills the freshly-established one. The charge point recovers (it redials again),
            // but it pays an extra round trip and, because the resend of `BootNotification` from
            // `on_reconnect` lands in that torn-down window, a full boot-retry interval offline.
            if target.dial_count() == dials_before {
                closer.close_connection().await;
            } else {
                tracing::debug!(
                    %address,
                    "the connection already redialled through the new target; not forcing another"
                );
            }
        }

        states.changed().await;
    }
}

/// Reads an integer `OCPPCommCtrlr` variable out of the live device model.
fn ocpp_comm_ctrlr_u32(state: &crate::state::ChargePointState, variable: &str) -> Option<u32> {
    let component = crate::state::Component {
        name: "OCPPCommCtrlr".into(),
        instance: None,
        evse: None,
    };
    let variable = crate::state::Variable {
        name: variable.into(),
        instance: None,
    };
    state
        .device_model
        .get(&component, &variable)
        .and_then(|definition| definition.attribute(crate::state::VariableAttributeType::Actual))
        .and_then(|attribute| attribute.value.parse().ok())
}

/// `OCPPCommCtrlr`/`RetryBackOffRandomRange`, or `0` (no jitter) when it is absent or
/// unparseable - the same value this crate registers, so what a CSMS reads is what it gets.
fn jitter_range(state: &crate::state::ChargePointState) -> u32 {
    ocpp_comm_ctrlr_u32(state, "RetryBackOffRandomRange").unwrap_or(0)
}

/// `OCPPCommCtrlr`/`NetworkProfileConnectionAttempts`, or this crate's registered default when it
/// is absent or unparseable.
fn connection_attempts(state: &crate::state::ChargePointState) -> u32 {
    ocpp_comm_ctrlr_u32(state, "NetworkProfileConnectionAttempts")
        .unwrap_or(DEFAULT_CONNECTION_ATTEMPTS)
}

#[cfg(feature = "ocpp_2_1")]
#[async_trait::async_trait]
impl ConnectionCloser for ocpp_client::ocpp_2_1::OCPP2_1Client {
    async fn close_connection(&self) {
        // `force_reconnect`, never `disconnect` - see the trait's docs.
        self.force_reconnect();
    }
}

#[cfg(feature = "ocpp_2_0_1")]
#[async_trait::async_trait]
impl ConnectionCloser for ocpp_client::ocpp_2_0_1::OCPP2_0_1Client {
    async fn close_connection(&self) {
        // `force_reconnect`, never `disconnect` - see the trait's docs.
        self.force_reconnect();
    }
}

#[cfg(feature = "ocpp_1_6")]
#[async_trait::async_trait]
impl ConnectionCloser for ocpp_client::ocpp_1_6::OCPP1_6Client {
    async fn close_connection(&self) {
        // `force_reconnect`, never `disconnect` - see the trait's docs.
        self.force_reconnect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::TokioExecutor;
    use crate::provisioning::TokioBackoff;
    use crate::state::{
        ChargePointEvent, NetworkConnectionProfile, NetworkInterface, NetworkTransport,
    };

    fn target(address: &str) -> Arc<ConnectionTarget> {
        let target = ConnectionTarget::new(address, &ConnectOptions::default());
        target.set_version(OcppVersion::V2_1);
        target
    }

    fn profile(url: &str) -> NetworkConnectionProfile {
        NetworkConnectionProfile {
            csms_url: url.into(),
            interface: NetworkInterface::Any,
            transport: NetworkTransport::Json,
            security_profile: 1,
            message_timeout_secs: 30,
            identity: None,
        }
    }

    #[test]
    fn a_switch_changes_where_the_next_dial_goes() {
        let target = target("wss://origin");

        assert!(target.switch_to("wss://new"));
        assert_eq!(target.address(), "wss://new");
        // Switching to where we already are is not a switch - closing a working connection for
        // that would be a self-inflicted outage.
        assert!(!target.switch_to("wss://new"));
    }

    #[test]
    fn enough_consecutive_failures_roll_back_to_the_last_working_address() {
        let target = target("wss://origin");
        target.set_connection_attempts(3);
        target.switch_to("wss://broken");

        assert_eq!(target.record_failure(), None);
        assert_eq!(target.record_failure(), None);
        assert_eq!(
            target.record_failure().as_deref(),
            Some("wss://origin"),
            "the third failure should hit NetworkProfileConnectionAttempts"
        );
        assert_eq!(target.address(), "wss://origin");
    }

    #[test]
    fn a_success_resets_the_failure_count_so_a_flaky_link_does_not_roll_back() {
        let target = target("wss://origin");
        target.set_connection_attempts(3);
        target.switch_to("wss://new");

        target.record_failure();
        target.record_failure();
        target.record_success();
        target.record_failure();
        target.record_failure();

        // Five failures overall, but never three in a row on an unproven address - and the
        // address proved itself in between, so there is nothing to roll back to.
        assert_eq!(target.address(), "wss://new");
        assert_eq!(target.record_failure(), None);
        assert_eq!(target.address(), "wss://new");
    }

    #[test]
    fn two_switches_without_a_connection_still_roll_back_to_the_address_that_worked() {
        let target = target("wss://origin");
        target.set_connection_attempts(2);
        target.switch_to("wss://first-guess");
        target.switch_to("wss://second-guess");

        target.record_failure();
        assert_eq!(target.record_failure().as_deref(), Some("wss://origin"));
    }

    #[test]
    fn switching_back_to_the_fallback_leaves_nothing_to_roll_back_to() {
        let target = target("wss://origin");
        target.set_connection_attempts(1);
        target.switch_to("wss://new");
        target.switch_to("wss://origin");

        // Back where we started and it is proven, so a failure here is an outage to keep retrying
        // rather than a profile to abandon.
        assert_eq!(target.record_failure(), None);
        assert_eq!(target.address(), "wss://origin");
    }

    #[test]
    fn a_zero_attempt_count_is_ignored_rather_than_making_every_switch_impossible() {
        let target = target("wss://origin");
        target.set_connection_attempts(0);
        target.switch_to("wss://new");

        assert_eq!(target.record_failure(), None);
        assert_eq!(target.address(), "wss://new");
    }

    #[test]
    fn a_caller_who_brought_its_own_reconnector_keeps_it() {
        struct Theirs;
        impl Reconnector for Theirs {
            fn connect<'a>(
                &'a self,
            ) -> Pin<
                Box<
                    dyn Future<
                            Output = Result<
                                (Box<dyn TransportSink>, Box<dyn TransportStream>),
                                TransportError,
                            >,
                        > + Send
                        + 'a,
                >,
            > {
                unimplemented!("never dialled in this test")
            }
        }

        let target = target("wss://origin");
        let theirs: Arc<dyn Reconnector> = Arc::new(Theirs);
        let options = target.install(ConnectOptions {
            reconnector: Some(theirs.clone()),
            ..Default::default()
        });

        assert!(Arc::ptr_eq(&options.reconnector.unwrap(), &theirs));

        // ...and with no reconnector of their own, ours goes in.
        let options = target.install(ConnectOptions::default());
        assert!(options.reconnector.is_some());
    }

    struct RecordingCloser {
        closes: Arc<Mutex<u32>>,
    }

    #[async_trait::async_trait]
    impl ConnectionCloser for RecordingCloser {
        async fn close_connection(&self) {
            *self.closes.lock().unwrap() += 1;
        }
    }

    #[tokio::test]
    async fn a_new_profile_points_the_target_at_it_and_drops_the_connection() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let target = target("wss://origin");
        let closes = Arc::new(Mutex::new(0));
        let closer = RecordingCloser {
            closes: closes.clone(),
        };

        let switching = {
            let actor = actor.clone();
            let target = target.clone();
            tokio::spawn(async move {
                run_network_profile_switching(&actor, &target, &closer, &TokioBackoff).await;
            })
        };

        let _ = actor
            .send(ChargePointEvent::NetworkProfileSet {
                slot: 1,
                profile: Box::new(profile("wss://elsewhere")),
            })
            .await;

        // The loop is spawned, so give it a moment to observe the new state.
        for _ in 0..200 {
            if *closes.lock().unwrap() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert_eq!(target.address(), "wss://elsewhere");
        assert_eq!(*closes.lock().unwrap(), 1);
        switching.abort();
    }

    /// A redial that happens on its own during the switch grace period has already landed on the
    /// new address (`switch_to` re-pointed the target before the wait), so there is nothing left
    /// to abandon.
    ///
    /// Forcing anyway is not merely redundant: `Client::force_reconnect` tears down whichever
    /// connection is current when the read loop observes it, so a force racing an in-flight
    /// redial destroys the connection that just came up. The charge point recovers, but only
    /// after another redial *and* a boot-retry interval, because the `BootNotification` resent
    /// from `on_reconnect` is written into the torn-down window. Seen for real against
    /// `ocpp-client` 0.4.0 in `tests/network_profile_switch.rs`, where the CSMS closes its side
    /// as soon as it has answered `SetNetworkProfile`.
    #[tokio::test]
    async fn a_connection_that_already_redialled_itself_is_not_forced_to_redial_again() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let target = target("wss://origin");
        let closes = Arc::new(Mutex::new(0));
        let closer = RecordingCloser {
            closes: closes.clone(),
        };

        let switching = {
            let actor = actor.clone();
            let target = target.clone();
            tokio::spawn(async move {
                run_network_profile_switching(&actor, &target, &closer, &TokioBackoff).await;
            })
        };

        let _ = actor
            .send(ChargePointEvent::NetworkProfileSet {
                slot: 1,
                profile: Box::new(profile("wss://elsewhere")),
            })
            .await;

        // Stand in for the transport redialling of its own accord during the grace period - what
        // `ConnectionTarget::dial` records on every redial it is asked for.
        target.record_dial();

        // Long enough for the grace period to elapse and the loop to decide.
        tokio::time::sleep(Duration::from_millis(300)).await;

        assert_eq!(target.address(), "wss://elsewhere");
        assert_eq!(
            *closes.lock().unwrap(),
            0,
            "the connection had already moved; forcing another redial would kill it"
        );
        switching.abort();
    }

    #[tokio::test]
    async fn a_charge_point_with_no_profiles_is_left_where_it_is() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let target = target("wss://origin");
        let closes = Arc::new(Mutex::new(0));
        let closer = RecordingCloser {
            closes: closes.clone(),
        };

        let switching = {
            let actor = actor.clone();
            let target = target.clone();
            tokio::spawn(async move {
                run_network_profile_switching(&actor, &target, &closer, &TokioBackoff).await;
            })
        };

        // Any state change at all, with no profile behind it.
        let _ = actor
            .send(ChargePointEvent::TimeSynced {
                csms_time: chrono::Utc::now(),
                recorded_at: crate::clock::MonotonicInstant::from_ticks(0),
            })
            .await;
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert_eq!(target.address(), "wss://origin");
        assert_eq!(*closes.lock().unwrap(), 0);
        switching.abort();
    }

    #[test]
    fn no_jitter_is_added_until_the_csms_asks_for_some() {
        let target = target("ws://csms.example/ocpp");

        // This crate registers `RetryBackOffRandomRange` as 0, and a charge point that spread its
        // retries without being told to would be reporting one thing and doing another.
        assert_eq!(target.next_jitter_secs(), 0);
    }

    #[test]
    fn jitter_stays_within_the_range_the_csms_set_and_can_reach_both_ends() {
        let target = target("ws://csms.example/ocpp");
        target.set_jitter_range(4);

        let draws: Vec<u32> = (0..500).map(|_| target.next_jitter_secs()).collect();

        assert!(
            draws.iter().all(|draw| *draw <= 4),
            "a delay past the range the CSMS set is a delay it did not agree to: {draws:?}"
        );
        // Inclusive of the range itself - "the maximum value for the random part" is a value the
        // random part may take.
        assert!(draws.contains(&0));
        assert!(draws.contains(&4));
    }

    #[test]
    fn a_range_of_one_still_spreads_rather_than_collapsing_to_a_constant() {
        let target = target("ws://csms.example/ocpp");
        target.set_jitter_range(1);

        let draws: Vec<u32> = (0..200).map(|_| target.next_jitter_secs()).collect();

        // The smallest range that means anything at all. An off-by-one in the modulus would make
        // this always 0, which reads as working while spreading nothing.
        assert!(draws.contains(&0) && draws.contains(&1));
    }

    #[test]
    fn two_charge_points_do_not_draw_the_same_delays() {
        // The whole point of jitter: a fleet that all seeded identically retries in lockstep, and
        // is no better off than a fleet with no jitter at all.
        let first = ConnectionTarget::new(
            "ws://csms.example/ocpp",
            &ConnectOptions {
                username: Some("CP-0001"),
                ..Default::default()
            },
        );
        let second = ConnectionTarget::new(
            "ws://csms.example/ocpp",
            &ConnectOptions {
                username: Some("CP-0002"),
                ..Default::default()
            },
        );
        first.set_jitter_range(60);
        second.set_jitter_range(60);

        let firsts: Vec<u32> = (0..20).map(|_| first.next_jitter_secs()).collect();
        let seconds: Vec<u32> = (0..20).map(|_| second.next_jitter_secs()).collect();

        assert_ne!(firsts, seconds);
    }

    #[test]
    fn turning_jitter_off_again_stops_it_immediately() {
        let target = target("ws://csms.example/ocpp");
        target.set_jitter_range(30);
        target.set_jitter_range(0);

        assert_eq!(target.next_jitter_secs(), 0);
    }

    #[tokio::test]
    async fn a_csms_write_to_the_jitter_range_reaches_the_running_connection() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let target = target("ws://csms.example/ocpp");

        let switching_target = target.clone();
        let switching_actor = actor.clone();
        tokio::spawn(async move {
            run_network_profile_switching(
                &switching_actor,
                &switching_target,
                &RecordingCloser {
                    closes: Arc::new(Mutex::new(0)),
                },
                &TokioBackoff,
            )
            .await;
        });

        let _ = actor
            .send(ChargePointEvent::DeviceModel(
                crate::state::DeviceModelEvent::AttributeValueSet {
                    component: crate::state::Component {
                        name: "OCPPCommCtrlr".into(),
                        instance: None,
                        evse: None,
                    },
                    variable: crate::state::Variable {
                        name: "RetryBackOffRandomRange".into(),
                        instance: None,
                    },
                    attribute_type: crate::state::VariableAttributeType::Actual,
                    value: "10".into(),
                },
            ))
            .await;
        for _ in 0..200 {
            if target.inner.lock().expect("target lock").jitter_range_secs == 10 {
                break;
            }
            tokio::task::yield_now().await;
        }

        // Unlike `initial_delay`/`max_delay`, which are sealed into the transport when it is
        // built, the random part is applied by this crate on every redial - so a CSMS write to it
        // does not have to wait for the next connection.
        assert_eq!(
            target.inner.lock().expect("target lock").jitter_range_secs,
            10
        );
    }
}
