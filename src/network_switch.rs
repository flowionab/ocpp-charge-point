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

/// Closes the current CSMS connection, so the transport redials through the
/// [`ConnectionTarget`].
///
/// Kept as this crate's own trait, like every other CSMS-facing capability, so the switching loop
/// is testable without a live socket and works for whichever OCPP version was negotiated.
#[async_trait::async_trait]
pub trait ConnectionCloser {
    /// Closes the connection. A failure is not an error worth propagating: the connection is
    /// being deliberately discarded, and one that is already broken needs no closing.
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
        let selected = selected_profile(&state).map(|(_, profile)| profile.csms_url.clone());
        drop(state);

        if let Some(address) = selected
            && target.switch_to(&address)
        {
            tracing::info!(%address, "switching the CSMS connection to a network profile");
            backoff.wait(SWITCH_GRACE_SECS).await;
            closer.close_connection().await;
        }

        states.changed().await;
    }
}

/// `OCPPCommCtrlr`/`NetworkProfileConnectionAttempts`, or this crate's registered default when it
/// is absent or unparseable.
fn connection_attempts(state: &crate::state::ChargePointState) -> u32 {
    let component = crate::state::Component {
        name: "OCPPCommCtrlr".into(),
        instance: None,
        evse: None,
    };
    let variable = crate::state::Variable {
        name: "NetworkProfileConnectionAttempts".into(),
        instance: None,
    };
    state
        .device_model
        .get(&component, &variable)
        .and_then(|definition| definition.attribute(crate::state::VariableAttributeType::Actual))
        .and_then(|attribute| attribute.value.parse().ok())
        .unwrap_or(DEFAULT_CONNECTION_ATTEMPTS)
}

#[cfg(feature = "ocpp_2_1")]
#[async_trait::async_trait]
impl ConnectionCloser for ocpp_client::ocpp_2_1::OCPP2_1Client {
    async fn close_connection(&self) {
        let _ = self.disconnect().await;
    }
}

#[cfg(feature = "ocpp_2_0_1")]
#[async_trait::async_trait]
impl ConnectionCloser for ocpp_client::ocpp_2_0_1::OCPP2_0_1Client {
    async fn close_connection(&self) {
        let _ = self.disconnect().await;
    }
}

#[cfg(feature = "ocpp_1_6")]
#[async_trait::async_trait]
impl ConnectionCloser for ocpp_client::ocpp_1_6::OCPP1_6Client {
    async fn close_connection(&self) {
        let _ = self.disconnect().await;
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
}
