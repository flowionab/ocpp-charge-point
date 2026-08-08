//! A configurable ceiling on inbound OCPP-J WebSocket frame size, and the transport-level guard
//! that enforces it (`docs/PRODUCTION-ROADMAP.md` §8.5, F5.2).
//!
//! # What "before allocation" actually means here
//!
//! F5.2's brief is to refuse an oversized inbound payload *before* this crate allocates memory to
//! find out how big it is - a charge point is an MCU with kilobytes to spare, and a CSMS (or
//! something impersonating one) sending a 200 KB `SetChargingProfile` must not first succeed at
//! that allocation. That phrase needs an honest boundary, because this crate does not own the
//! transport - `ocpp-client` does (`CLAUDE.md`: "delegate OCPP networking and transport concerns
//! to the `ocpp-client` crate").
//!
//! Investigating `ocpp-client` 0.5.0's actual surface (not assumed from the roadmap) turned up two
//! facts that decide where the earliest *reachable* point is:
//!
//! 1. **No frame-size configuration is exposed.** `ocpp_client::connect`/`connect_1_6`/
//!    `connect_2_0_1`/`connect_2_1` all call `tokio_tungstenite::client_async_tls_with_config`
//!    with `None` for its `WebSocketConfig` argument, hard-coding `tungstenite`'s own defaults
//!    (currently 64 MiB max message size). Nothing in `ConnectOptions` reaches that parameter.
//! 2. **No pre-parse hook exists.** `Client`'s internal read loop (`handle_frame`) calls
//!    `serde_json::from_str::<Value>(frame)` - allocating a full JSON DOM - before it even knows
//!    which action a CALL names, let alone before handing anything to a handler this crate
//!    registered via `Client::on::<A>`. That handler then runs a *second* deserialization,
//!    `serde_json::from_value::<A::Request>(payload)`, which is where a large concrete type (2.1's
//!    `ChargingProfile` is 56 KB by value - see D2.3) actually gets built. Both allocations happen
//!    entirely inside `ocpp-client`, before any code in this crate executes at all.
//!
//! So for the two connection paths built by `ocpp_client::connect()` itself (the *initial* dial,
//! and every version-specific `connect_1_6`/`connect_2_0_1`/`connect_2_1` convenience function),
//! this crate has **no hook to intervene before either allocation**, and says so rather than
//! pretending otherwise. Building one would mean re-implementing the WebSocket handshake and
//! subprotocol negotiation this crate deliberately does not duplicate.
//!
//! What this crate *does* control is [`crate::network_switch::ConnectionTarget::dial`]: every
//! redial (after the very first connection - unavoidably dialled through `ocpp_client::connect`)
//! goes through the public `ocpp_client::websocket_transport`, which hands back the raw
//! [`ocpp_client::TransportSink`]/[`ocpp_client::TransportStream`] halves before they are wired
//! into a `Client`. [`SizeLimitedStream`] wraps that stream: a text frame over the configured
//! ceiling is refused and **never handed to `Client`**, so neither of `ocpp-client`'s
//! deserialization allocations happens for it. That is a real, if partial, improvement over doing
//! nothing - and it is honestly short of "before allocation" in the strictest sense, since
//! `tokio-tungstenite` has already assembled the frame into one `String` by the time
//! [`TransportStream::recv`] returns it (bounded only by its own 64 MiB default, which this crate
//! cannot lower - point 1 above). What this guard prevents is the *second*, much more expensive
//! allocation: the sized Rust structures `ocpp-client` builds from the frame, which is where
//! D2.3's cost actually lives.
//!
//! `SizeLimitedStream` is deliberately transport-agnostic (it only needs
//! [`ocpp_client::TransportStream`], which is not gated behind any Cargo feature): an embedded
//! integrator supplying their own [`ocpp_client::TransportStream`] to
//! [`crate::setup`](crate::setup) directly can wrap it the same way, without pulling in this
//! crate's WebSocket wiring at all.

use crate::actor::ChargePointActor;
use crate::security::report_security_event;
use crate::state::{SecurityEvent, SecurityEventType};
use alloc::boxed::Box;
use core::future::Future;
use core::pin::Pin;
use ocpp_client::{TransportError, TransportEvent, TransportStream};

/// Default ceiling for [`PayloadLimits::max_inbound_frame_bytes`]: 32 KiB.
///
/// Sized from what this crate's own measurements say a legitimate inbound message actually costs
/// (`docs/MEMORY.md`, G2.3): a full `SendLocalList` at this crate's own 100-entry default is
/// ~11 KB of JSON-worthy content, and the largest routine inbound CALLs (`SetChargingProfile`,
/// `SetVariables`, `UpdateFirmware`) are, in real deployments, low single-digit KB of JSON even
/// though the *decoded* 2.1 `ChargingProfile` type is 56 KB by value (D2.3 - a fixed
/// `heapless`-capacity cost independent of the wire payload's actual size, and not something a
/// byte-length ceiling can fix; that is D2.3's job, upstream). 32 KiB leaves generous headroom
/// (roughly 3x) over the largest legitimate message this crate expects while staying small next
/// to the crate's own retained-state budget (`docs/MEMORY.md` puts the tightest configuration's
/// total retained state at ~63 KB) - a single oversized frame should not itself become the
/// dominant cost. It is far below `tungstenite`'s own unconfigurable 64 MiB default, which is
/// exactly the gap this ceiling exists to close.
///
/// Override it with [`crate::network_switch::ConnectionTarget::set_max_inbound_frame_bytes`] (or
/// by constructing [`PayloadLimits`] directly for a custom transport) if a deployment's messages
/// are known to run larger or smaller.
pub const DEFAULT_MAX_INBOUND_FRAME_BYTES: usize = 32 * 1024;

/// A configurable ceiling on one inbound OCPP-J WebSocket text frame, enforced by
/// [`SizeLimitedStream`]. See the module docs for exactly what "enforced" covers and does not
/// cover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayloadLimits {
    /// The largest inbound frame, in bytes, this charge point accepts before handing it to
    /// `ocpp-client` for decoding. A frame longer than this is dropped unparsed and raises a
    /// `MemoryExhaustion` security event; see [`SizeLimitedStream`].
    pub max_inbound_frame_bytes: usize,
}

impl Default for PayloadLimits {
    /// [`DEFAULT_MAX_INBOUND_FRAME_BYTES`].
    fn default() -> Self {
        Self {
            max_inbound_frame_bytes: DEFAULT_MAX_INBOUND_FRAME_BYTES,
        }
    }
}

/// Wraps an [`ocpp_client::TransportStream`], refusing to forward any [`TransportEvent::Frame`]
/// longer than `limits.max_inbound_frame_bytes` - see the module docs for why this is the
/// earliest point this crate can enforce that, and what it does not cover.
///
/// An oversized frame is **dropped, not forwarded**: `recv` keeps polling the inner stream rather
/// than returning the event, so the caller (`ocpp-client`'s `Client`) never sees it and never runs
/// either of its own deserialization steps on it. If `actor` is set, a `MemoryExhaustion` security
/// event is raised through [`report_security_event`] first - the same treatment a saturated
/// [`crate::offline_queue::OfflineQueue`] gets (G2.1), for the same reason: the CSMS is the only
/// party that can act on a persistent oversized-message pattern.
///
/// `Ping`/`Pong` events, and frames at or under the ceiling, pass through untouched.
pub struct SizeLimitedStream {
    inner: Box<dyn TransportStream>,
    limits: PayloadLimits,
    actor: Option<ChargePointActor>,
}

impl SizeLimitedStream {
    /// Wraps `inner`, refusing any frame over `limits.max_inbound_frame_bytes`. `actor`, if
    /// present, receives a `MemoryExhaustion` security event each time a frame is refused - pass
    /// `None` for a transport with no charge-point actor yet (e.g. before the first connection
    /// exists), though [`crate::network_switch::ConnectionTarget`] only wraps redials, which
    /// always have one.
    pub fn new(
        inner: Box<dyn TransportStream>,
        limits: PayloadLimits,
        actor: Option<ChargePointActor>,
    ) -> Self {
        Self {
            inner,
            limits,
            actor,
        }
    }
}

impl TransportStream for SizeLimitedStream {
    fn recv<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<TransportEvent>, TransportError>> + Send + 'a>>
    {
        Box::pin(async move {
            loop {
                let event = self.inner.recv().await?;
                match event {
                    Some(TransportEvent::Frame(text))
                        if text.len() > self.limits.max_inbound_frame_bytes =>
                    {
                        let len = text.len();
                        let ceiling = self.limits.max_inbound_frame_bytes;
                        tracing::warn!(
                            frame_bytes = len,
                            ceiling_bytes = ceiling,
                            "ocpp-charge-point: refusing an inbound frame that exceeds the \
                             configured payload ceiling, before OCPP-J decoding"
                        );
                        if let Some(actor) = &self.actor {
                            report_security_event(
                                actor,
                                SecurityEvent {
                                    event_type: SecurityEventType::MemoryExhaustion,
                                    tech_info: Some(alloc::format!(
                                        "inbound frame of {len} bytes exceeded the \
                                         {ceiling}-byte payload ceiling and was refused before \
                                         OCPP-J decoding"
                                    )),
                                },
                            )
                            .await;
                        }
                        // Drop it and keep reading - `ocpp-client` never sees this event, so it
                        // never runs either of its own deserialization steps on it.
                        continue;
                    }
                    other => return Ok(other),
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec::Vec;

    /// A [`TransportStream`] that yields a fixed, pre-recorded sequence of events, one per
    /// `recv` call, then `None` forever - enough to drive [`SizeLimitedStream`] without a real
    /// transport.
    struct ScriptedStream {
        events: Vec<Option<TransportEvent>>,
        next: usize,
    }

    impl ScriptedStream {
        fn new(events: Vec<Option<TransportEvent>>) -> Self {
            Self { events, next: 0 }
        }
    }

    impl TransportStream for ScriptedStream {
        fn recv<'a>(
            &'a mut self,
        ) -> Pin<Box<dyn Future<Output = Result<Option<TransportEvent>, TransportError>> + Send + 'a>>
        {
            Box::pin(async move {
                // `TransportEvent` doesn't derive `Clone` upstream, so this test double clones by
                // hand for the one shape it needs (`Frame`/`Ping`); `Pong` is never scripted here.
                let event = match self.events.get(self.next) {
                    Some(Some(TransportEvent::Frame(text))) => {
                        Some(TransportEvent::Frame(text.clone()))
                    }
                    Some(Some(TransportEvent::Ping(payload))) => {
                        Some(TransportEvent::Ping(payload.clone()))
                    }
                    Some(Some(TransportEvent::Pong(payload))) => {
                        Some(TransportEvent::Pong(payload.clone()))
                    }
                    _ => None,
                };
                self.next += 1;
                Ok(event)
            })
        }
    }

    fn tiny_limits() -> PayloadLimits {
        PayloadLimits {
            max_inbound_frame_bytes: 16,
        }
    }

    #[test]
    fn default_ceiling_is_the_documented_constant() {
        assert_eq!(
            PayloadLimits::default().max_inbound_frame_bytes,
            DEFAULT_MAX_INBOUND_FRAME_BYTES
        );
    }

    #[tokio::test]
    async fn a_frame_at_or_under_the_ceiling_passes_through_unchanged() {
        let frame = "x".repeat(16);
        let inner = ScriptedStream::new(vec![Some(TransportEvent::Frame(frame.clone()))]);
        let mut guarded = SizeLimitedStream::new(Box::new(inner), tiny_limits(), None);

        let event = guarded.recv().await.unwrap();
        assert!(matches!(event, Some(TransportEvent::Frame(text)) if text == frame));
    }

    #[tokio::test]
    async fn an_oversized_frame_is_dropped_and_the_next_event_is_returned_instead() {
        let oversized = "x".repeat(17);
        let normal = "ok".to_string();
        let inner = ScriptedStream::new(vec![
            Some(TransportEvent::Frame(oversized)),
            Some(TransportEvent::Frame(normal.clone())),
        ]);
        let mut guarded = SizeLimitedStream::new(Box::new(inner), tiny_limits(), None);

        // One `recv` call transparently skips the oversized frame and surfaces the next one -
        // `ocpp-client`'s read loop never sees the oversized frame as an event at all.
        let event = guarded.recv().await.unwrap();
        assert!(matches!(event, Some(TransportEvent::Frame(text)) if text == normal));
    }

    #[tokio::test]
    async fn the_stream_ending_after_an_oversized_frame_still_ends_cleanly() {
        let inner = ScriptedStream::new(vec![Some(TransportEvent::Frame("x".repeat(17))), None]);
        let mut guarded = SizeLimitedStream::new(Box::new(inner), tiny_limits(), None);

        let event = guarded.recv().await.unwrap();
        assert!(event.is_none());
    }

    #[tokio::test]
    async fn ping_and_pong_pass_through_regardless_of_the_ceiling() {
        let inner = ScriptedStream::new(vec![Some(TransportEvent::Ping(alloc::vec![1, 2, 3]))]);
        let mut guarded = SizeLimitedStream::new(Box::new(inner), tiny_limits(), None);

        let event = guarded.recv().await.unwrap();
        assert!(matches!(event, Some(TransportEvent::Ping(payload)) if payload == [1, 2, 3]));
    }

    #[tokio::test]
    async fn an_oversized_frame_raises_memory_exhaustion_when_an_actor_is_attached() {
        use crate::executor::TokioExecutor;
        use crate::state::SecurityEventType;

        // One EVSE with one connector - the actor's own hardware wiring is irrelevant to this
        // test, only that it exists and can receive events.
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let mut observer = actor.subscribe_security_events();

        let inner = ScriptedStream::new(vec![Some(TransportEvent::Frame("x".repeat(17)))]);
        let mut guarded = SizeLimitedStream::new(Box::new(inner), tiny_limits(), Some(actor));
        let event = guarded.recv().await.unwrap();
        assert!(event.is_none(), "the oversized frame must not be forwarded");

        let raised = tokio::time::timeout(core::time::Duration::from_millis(200), observer.recv())
            .await
            .expect("a security event should have been raised")
            .expect("the broadcast channel should not have closed");
        assert_eq!(raised.event_type, SecurityEventType::MemoryExhaustion);
        assert!(
            raised
                .tech_info
                .as_deref()
                .is_some_and(|info| info.contains("17 bytes"))
        );
    }

    #[tokio::test]
    async fn no_event_is_raised_when_no_actor_is_attached() {
        let inner = ScriptedStream::new(vec![Some(TransportEvent::Frame("x".repeat(17))), None]);
        let mut guarded = SizeLimitedStream::new(Box::new(inner), tiny_limits(), None);
        // Simply must not panic reaching for an absent actor.
        let event = guarded.recv().await.unwrap();
        assert!(event.is_none());
    }
}
