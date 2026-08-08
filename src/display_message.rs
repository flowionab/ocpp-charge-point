//! Display Message functional block: CSMS-initiated `SetDisplayMessage`/`GetDisplayMessages`/
//! `ClearDisplayMessage`, answered/reported via `NotifyDisplayMessages`. See `docs/ROADMAP.md`
//! §15 and `docs/PRODUCTION-ROADMAP.md` B6.
//!
//! **2.x only** - 1.6J has no display-message concept at all (no functional block, no feature
//! profile - see [`crate::hardware::CAPABILITY_GATES`]'s `has_display` row, whose
//! `feature_profile_1_6` is `None`).
//!
//! # The interesting half: deriving *which* message shows
//!
//! OCPP lets several messages be installed at once, each optionally scoped to a
//! [`crate::state::MessageState`] and always carrying a [`crate::state::MessagePriority`]. Which
//! one a driver actually sees at any moment is therefore not a CSMS decision made once at
//! `SetDisplayMessage` time - it is *derived*, continuously, from the charge point's own state.
//! [`current_message`] is that derivation: pure, hardware-free, and unit-tested without any
//! `Display` implementor at all. A connector faulting or a transaction starting can change what's
//! shown with no CSMS round-trip - [`run_display_updates`] is what turns a change in that
//! derivation into an actual [`crate::hardware::Display::show`] call.

use crate::actor::ChargePointActor;
use crate::state::{
    ChargePointEvent, ChargePointState, ConnectorState, DisplayMessageId, DisplayedMessage,
    EvseStatus, LifecycleState, MessageFormat, MessagePriority, MessageState, TransactionId,
};
use alloc::boxed::Box;
use alloc::vec::Vec;

/// The outcome of a CSMS-initiated `SetDisplayMessage` request, matching OCPP's
/// `DisplayMessageStatusEnum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetDisplayMessageOutcome {
    /// The message was installed.
    Accepted,
    /// `message.message.format` (or, for 2.1, `message_extra`) uses a format
    /// [`crate::hardware::Display::supported_formats`] does not list.
    NotSupportedMessageFormat,
    /// The `has_display` capability is runtime-absent, or the store is already at its configured
    /// maximum and `message.id` names a message that isn't already stored (see
    /// [`crate::state::DisplayMessageStore::set`]).
    Rejected,
    /// `message.priority` isn't one this charge point supports. Never produced today - this
    /// crate's [`MessagePriority`] already covers every value OCPP defines - modeled for wire
    /// completeness, mirroring [`crate::reporting::ReportOutcome::NotSupported`].
    NotSupportedPriority,
    /// `message.state` names a [`MessageState`] this charge point does not support - reachable
    /// today for 2.1's `Suspended`/`Discharging` (see [`MessageState`]'s docs).
    NotSupportedState,
    /// `message.transaction_id` doesn't address a transaction currently in progress.
    UnknownTransaction,
    /// `message.message.language` isn't one this hardware can render. Never produced today - this
    /// crate does not yet ask hardware which languages it supports (a further-narrowing companion
    /// to [`crate::hardware::Display::supported_formats`] this block doesn't model yet) - modeled
    /// for wire completeness.
    LanguageNotSupported,
}

/// Whether `format` is one `supported_formats` lists.
fn format_supported(format: MessageFormat, supported_formats: &[MessageFormat]) -> bool {
    supported_formats.contains(&format)
}

/// Whether any currently in-progress transaction has this id.
fn transaction_in_progress(state: &ChargePointState, id: TransactionId) -> bool {
    state
        .evses
        .iter()
        .flat_map(|evse| evse.transactions.iter())
        .any(|transaction| transaction.as_ref().is_some_and(|t| t.id == id))
}

/// Handles a CSMS-initiated `SetDisplayMessage` request against `actor`: installs `message` if
/// this charge point can honour it - see [`SetDisplayMessageOutcome`] for every way it can't.
/// `supported_formats` comes from the registered [`crate::hardware::Display::supported_formats`],
/// reported honestly per that trait's docs, so a format the hardware truly cannot render is
/// refused here rather than accepted and silently failing on the driver's own screen.
pub async fn handle_set_display_message(
    actor: &ChargePointActor,
    message: DisplayedMessage,
    supported_formats: &[MessageFormat],
) -> SetDisplayMessageOutcome {
    let state = actor.state();
    // C5 (docs/PRODUCTION-ROADMAP.md §5.5): mirrors `crate::reservation::handle_reserve_now`'s
    // capability check - a charge point with no display must refuse rather than pretend to
    // remember a message it can never show.
    if !crate::refusal::capability_present(&state.capabilities, "SetDisplayMessage") {
        return SetDisplayMessageOutcome::Rejected;
    }
    if !format_supported(message.message.format, supported_formats) {
        return SetDisplayMessageOutcome::NotSupportedMessageFormat;
    }
    if let Some(transaction_id) = message.transaction_id
        && !transaction_in_progress(&state, transaction_id)
    {
        return SetDisplayMessageOutcome::UnknownTransaction;
    }

    let id = message.id;
    let _ = actor
        .send(ChargePointEvent::DisplayMessageSet(Box::new(message)))
        .await;
    if actor.state().display_messages.get(id).is_none() {
        // The store refused it - the only remaining reason is the bound
        // (`crate::state::DisplayMessageStore::set`'s docs).
        return SetDisplayMessageOutcome::Rejected;
    }
    SetDisplayMessageOutcome::Accepted
}

/// Registers this charge point's inbound `SetDisplayMessage` handling with the CSMS connection.
/// Implemented per protocol version (see the `ocpp_2_1` module).
#[async_trait::async_trait]
pub trait SetDisplayMessageHandler {
    /// Registers a `SetDisplayMessage` handler with the CSMS connection that dispatches incoming
    /// requests against `actor`, checking formats against `supported_formats`.
    async fn register_set_display_message_handler(
        &self,
        actor: ChargePointActor,
        supported_formats: Vec<MessageFormat>,
    );
}

/// The outcome of a CSMS-initiated `ClearDisplayMessage` request, matching OCPP's
/// `ClearMessageStatusEnum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClearDisplayMessageOutcome {
    /// The message was removed.
    Accepted,
    /// `id` doesn't address a currently stored message.
    Unknown,
    /// The `has_display` capability is runtime-absent.
    Rejected,
}

/// Handles a CSMS-initiated `ClearDisplayMessage` request against `actor`: removes the message
/// named by `id`, if this charge point can honour it.
pub async fn handle_clear_display_message(
    actor: &ChargePointActor,
    id: DisplayMessageId,
) -> ClearDisplayMessageOutcome {
    let state = actor.state();
    if !crate::refusal::capability_present(&state.capabilities, "ClearDisplayMessage") {
        return ClearDisplayMessageOutcome::Rejected;
    }
    if state.display_messages.get(id).is_none() {
        return ClearDisplayMessageOutcome::Unknown;
    }
    let _ = actor
        .send(ChargePointEvent::DisplayMessageCleared(id))
        .await;
    ClearDisplayMessageOutcome::Accepted
}

/// Registers this charge point's inbound `ClearDisplayMessage` handling with the CSMS connection.
/// Implemented per protocol version (see the `ocpp_2_1` module).
#[async_trait::async_trait]
pub trait ClearDisplayMessageHandler {
    /// Registers a `ClearDisplayMessage` handler with the CSMS connection that dispatches
    /// incoming requests against `actor`.
    async fn register_clear_display_message_handler(&self, actor: ChargePointActor);
}

/// A `GetDisplayMessages` filter (OCPP: `id`/`priority`/`state`, combined with AND semantics -
/// a stored message must match every filter given to be included).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DisplayMessageFilter {
    /// If non-empty, only messages whose id is in this list.
    pub ids: Vec<DisplayMessageId>,
    /// If given, only messages with this priority.
    pub priority: Option<MessagePriority>,
    /// If given, only messages scoped to this state.
    pub state: Option<MessageState>,
}

/// Whether `message` matches every criterion `filter` gives (an empty/default filter matches
/// everything).
fn matches_filter(message: &DisplayedMessage, filter: &DisplayMessageFilter) -> bool {
    (filter.ids.is_empty() || filter.ids.contains(&message.id))
        && filter.priority.is_none_or(|p| p == message.priority)
        && filter.state.is_none_or(|s| Some(s) == message.state)
}

/// The outcome of a CSMS-initiated `GetDisplayMessages` request, matching OCPP's
/// `GetDisplayMessagesStatusEnum` - the status carried on the *immediate* response, sent before
/// any `NotifyDisplayMessages` follow (see [`chunk_display_messages`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GetDisplayMessagesOutcome {
    /// The filter matched at least one message, reported via one or more
    /// `NotifyDisplayMessages`.
    Accepted(Vec<DisplayedMessage>),
    /// The filter matched nothing - mirrors [`crate::reporting::ReportOutcome::EmptyResultSet`]'s
    /// role for `GetReport`. Also the answer when `has_display` is runtime-absent: with no
    /// display, the store is always empty, so this is the honest status regardless (no separate
    /// capability check needed - unlike `SetDisplayMessage`/`ClearDisplayMessage`,
    /// `GetDisplayMessagesStatusEnum` has no `Rejected` value to refuse through).
    Unknown,
}

/// Handles a CSMS-initiated `GetDisplayMessages` request against `actor`'s current display
/// message store, applying `filter`.
pub fn handle_get_display_messages(
    actor: &ChargePointActor,
    filter: &DisplayMessageFilter,
) -> GetDisplayMessagesOutcome {
    let state = actor.state();
    let matched: Vec<DisplayedMessage> = state
        .display_messages
        .iter()
        .filter(|message| matches_filter(message, filter))
        .cloned()
        .collect();
    if matched.is_empty() {
        GetDisplayMessagesOutcome::Unknown
    } else {
        GetDisplayMessagesOutcome::Accepted(matched)
    }
}

/// Registers this charge point's inbound `GetDisplayMessages` handling with the CSMS connection.
/// Implemented per protocol version (see the `ocpp_2_1` module).
#[async_trait::async_trait]
pub trait GetDisplayMessagesHandler {
    /// Registers a `GetDisplayMessages` handler with the CSMS connection that answers with
    /// [`handle_get_display_messages`]'s outcome, then sends the resulting messages (if any) as
    /// one or more `NotifyDisplayMessages`.
    async fn register_get_display_messages_handler(&self, actor: ChargePointActor);
}

/// The most [`DisplayedMessage`]s carried in a single `NotifyDisplayMessages` chunk - mirrors
/// [`crate::reporting::REPORT_CHUNK_SIZE`]'s reasoning exactly (matches `ocpp-types`' non-`alloc`
/// `NotifyDisplayMessagesRequest`'s default `heapless::Vec<MessageInfo, 16>` capacity).
pub const DISPLAY_MESSAGE_CHUNK_SIZE: usize = 16;

/// One `NotifyDisplayMessages` message's worth of a chunked answer - see
/// [`crate::reporting::ReportChunk`], which this mirrors exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayMessageChunk {
    /// This chunk's sequence number - `0` for the first chunk. Unlike `NotifyReport`'s `seqNo`,
    /// `NotifyDisplayMessages` carries no sequence number field at all; this exists purely to
    /// drive `tbc` correctly and is not itself sent on the wire.
    pub seq_no: i64,
    /// Whether another chunk follows this one.
    pub tbc: bool,
    /// This chunk's messages (at most [`DISPLAY_MESSAGE_CHUNK_SIZE`]).
    pub messages: Vec<DisplayedMessage>,
}

/// Splits `messages` into an ordered sequence of [`DisplayMessageChunk`]s - see
/// [`crate::reporting::chunk_report`], which this mirrors. Unlike that function, an empty
/// `messages` produces zero chunks rather than one: [`handle_get_display_messages`] never accepts
/// an empty result (it answers [`GetDisplayMessagesOutcome::Unknown`] instead), so there is never
/// a reason to send a `NotifyDisplayMessages` with nothing in it - `messageInfo` is optional on
/// the wire specifically for that "search still in progress" case, not for one that already knew
/// the answer was empty.
pub fn chunk_display_messages(messages: &[DisplayedMessage]) -> Vec<DisplayMessageChunk> {
    let total = messages.chunks(DISPLAY_MESSAGE_CHUNK_SIZE).count();
    messages
        .chunks(DISPLAY_MESSAGE_CHUNK_SIZE)
        .enumerate()
        .map(|(index, chunk)| DisplayMessageChunk {
            seq_no: index as i64,
            tbc: index + 1 < total,
            messages: chunk.to_vec(),
        })
        .collect()
}

/// Derives the [`MessageState`] that best describes `state` right now, used by [`current_message`]
/// to decide which stored messages currently apply.
///
/// Priority order - most informative wins, mirroring `crate::reservation`'s own
/// `unavailable_outcome` "which rejection reason to report" ordering:
/// [`MessageState::Faulted`] (any fault, charge-point-wide/EVSE-wide/connector-wide) beats
/// [`MessageState::Charging`] (any connector actively charging) beats
/// [`MessageState::Unavailable`] (made unavailable, charge-point-wide/EVSE-wide/connector-wide)
/// beats the default, [`MessageState::Idle`].
pub fn current_message_state(state: &ChargePointState) -> MessageState {
    let any_faulted = state.lifecycle == LifecycleState::Faulted
        || state
            .evses
            .iter()
            .any(|evse| evse.status == EvseStatus::Faulted);
    let any_connector_faulted = state.evses.iter().any(|evse| {
        evse.connectors
            .iter()
            .any(|c| matches!(c, ConnectorState::Faulted | ConnectorState::FaultedSafe))
    });
    if any_faulted || any_connector_faulted {
        return MessageState::Faulted;
    }
    let any_charging = state
        .evses
        .iter()
        .any(|evse| evse.connectors.contains(&ConnectorState::Charging));
    if any_charging {
        return MessageState::Charging;
    }
    let any_unavailable = state.lifecycle == LifecycleState::Unavailable
        || state
            .evses
            .iter()
            .any(|evse| evse.status == EvseStatus::Unavailable)
        || state
            .evses
            .iter()
            .any(|evse| evse.connectors.contains(&ConnectorState::Unavailable));
    if any_unavailable {
        return MessageState::Unavailable;
    }
    MessageState::Idle
}

/// Picks the message that should currently be shown on the physical display, given `state` - the
/// highest-[`MessagePriority`] message whose `state` is `None` (shown unconditionally) or matches
/// [`current_message_state`], breaking ties by lowest [`DisplayMessageId`] for a deterministic
/// answer. `None` when nothing applies (no messages stored, or none match the current state).
///
/// Pure and hardware-free by design - see this module's docs - so a state transition changes
/// what's shown without any CSMS involvement, and this is unit-testable without a
/// [`crate::hardware::Display`] implementor at all. [`run_display_updates`] is what turns a change
/// here into an actual render.
pub fn current_message(state: &ChargePointState) -> Option<DisplayedMessage> {
    let now_state = current_message_state(state);
    state
        .display_messages
        .iter()
        .filter(|message| message.state.is_none_or(|s| s == now_state))
        .max_by(|a, b| a.priority.cmp(&b.priority).then(b.id.0.cmp(&a.id.0)))
        .cloned()
}

/// Watches `actor` for state changes and calls [`crate::hardware::Display::show`] on `display`
/// whenever [`current_message`]'s derivation actually changes - never on a state change that
/// leaves the shown message the same, so a hardware binding is not asked to re-render its screen
/// on every unrelated transition (a meter sample, say).
///
/// Runs until the actor stops. Register via
/// [`crate::builder::ChargePointBuilder::display_messages`], which also registers the inbound
/// `SetDisplayMessage`/`GetDisplayMessages`/`ClearDisplayMessage` handlers.
pub async fn run_display_updates<D: crate::hardware::Display>(
    actor: &ChargePointActor,
    display: &D,
) {
    let mut updates = actor.subscribe();
    let mut shown = current_message(&updates.borrow());
    if let Err(err) = display.show(shown.as_ref()).await {
        tracing::warn!(error = %err, "failed to render the initial display message");
    }
    loop {
        updates.changed().await;
        let next = current_message(&updates.borrow());
        if next != shown {
            if let Err(err) = display.show(next.as_ref()).await {
                tracing::warn!(error = %err, "failed to render a display message change");
            }
            shown = next;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::ChargePointActor;
    use crate::executor::TokioExecutor;
    use crate::hardware::Capabilities;
    use crate::state::{ConnectorEvent, EvseEvent, MessageContent};

    fn content(text: &str) -> MessageContent {
        MessageContent {
            content: text.into(),
            format: MessageFormat::Ascii,
            language: None,
        }
    }

    fn message(
        id: i64,
        priority: MessagePriority,
        state: Option<MessageState>,
    ) -> DisplayedMessage {
        DisplayedMessage {
            id: DisplayMessageId(id),
            priority,
            state,
            message: content("hi"),
            transaction_id: None,
        }
    }

    async fn spawn_with_display<const N: usize>(evses: [usize; N]) -> ChargePointActor {
        let actor = ChargePointActor::spawn(evses, &TokioExecutor);
        actor
            .send(ChargePointEvent::CapabilitiesDeclared(Capabilities {
                has_display: true,
                ..Default::default()
            }))
            .await
            .unwrap();
        actor
    }

    // --- SetDisplayMessage ---

    #[tokio::test]
    async fn a_supported_format_is_accepted() {
        let actor = spawn_with_display([1]).await;

        let outcome = handle_set_display_message(
            &actor,
            message(1, MessagePriority::NormalCycle, None),
            &[MessageFormat::Ascii],
        )
        .await;

        assert_eq!(outcome, SetDisplayMessageOutcome::Accepted);
        assert!(
            actor
                .state()
                .display_messages
                .get(DisplayMessageId(1))
                .is_some()
        );
    }

    #[tokio::test]
    async fn an_unsupported_format_is_refused_without_storing_it() {
        let actor = spawn_with_display([1]).await;

        let outcome = handle_set_display_message(
            &actor,
            message(1, MessagePriority::NormalCycle, None),
            &[MessageFormat::Html],
        )
        .await;

        assert_eq!(outcome, SetDisplayMessageOutcome::NotSupportedMessageFormat);
        assert!(
            actor
                .state()
                .display_messages
                .get(DisplayMessageId(1))
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_message_scoped_to_a_transaction_that_is_not_running_is_unknown() {
        let actor = spawn_with_display([1]).await;
        let mut with_tx = message(1, MessagePriority::NormalCycle, None);
        with_tx.transaction_id = Some(TransactionId(42));

        let outcome = handle_set_display_message(&actor, with_tx, &[MessageFormat::Ascii]).await;

        assert_eq!(outcome, SetDisplayMessageOutcome::UnknownTransaction);
    }

    #[tokio::test]
    async fn set_display_message_is_rejected_when_the_display_capability_is_absent() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let outcome = handle_set_display_message(
            &actor,
            message(1, MessagePriority::NormalCycle, None),
            &[MessageFormat::Ascii],
        )
        .await;

        assert_eq!(outcome, SetDisplayMessageOutcome::Rejected);
    }

    #[tokio::test]
    async fn setting_beyond_the_configured_maximum_is_rejected() {
        let actor = ChargePointActor::spawn_with_limits(
            [1],
            &TokioExecutor,
            crate::state::StateLimits::default().with_max_display_messages(1),
        );
        actor
            .send(ChargePointEvent::CapabilitiesDeclared(Capabilities {
                has_display: true,
                ..Default::default()
            }))
            .await
            .unwrap();
        handle_set_display_message(
            &actor,
            message(1, MessagePriority::NormalCycle, None),
            &[MessageFormat::Ascii],
        )
        .await;

        let outcome = handle_set_display_message(
            &actor,
            message(2, MessagePriority::NormalCycle, None),
            &[MessageFormat::Ascii],
        )
        .await;

        assert_eq!(outcome, SetDisplayMessageOutcome::Rejected);
    }

    // --- ClearDisplayMessage ---

    #[tokio::test]
    async fn clearing_a_known_message_succeeds() {
        let actor = spawn_with_display([1]).await;
        handle_set_display_message(
            &actor,
            message(1, MessagePriority::NormalCycle, None),
            &[MessageFormat::Ascii],
        )
        .await;

        let outcome = handle_clear_display_message(&actor, DisplayMessageId(1)).await;

        assert_eq!(outcome, ClearDisplayMessageOutcome::Accepted);
        assert!(
            actor
                .state()
                .display_messages
                .get(DisplayMessageId(1))
                .is_none()
        );
    }

    #[tokio::test]
    async fn clearing_an_unknown_message_is_unknown() {
        let actor = spawn_with_display([1]).await;

        let outcome = handle_clear_display_message(&actor, DisplayMessageId(1)).await;

        assert_eq!(outcome, ClearDisplayMessageOutcome::Unknown);
    }

    #[tokio::test]
    async fn clear_display_message_is_rejected_when_the_display_capability_is_absent() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let outcome = handle_clear_display_message(&actor, DisplayMessageId(1)).await;

        assert_eq!(outcome, ClearDisplayMessageOutcome::Rejected);
    }

    // --- GetDisplayMessages ---

    #[tokio::test]
    async fn a_filter_matching_nothing_is_unknown() {
        let actor = spawn_with_display([1]).await;

        let outcome = handle_get_display_messages(&actor, &DisplayMessageFilter::default());

        assert_eq!(outcome, GetDisplayMessagesOutcome::Unknown);
    }

    #[tokio::test]
    async fn no_filter_returns_every_stored_message() {
        let actor = spawn_with_display([1]).await;
        handle_set_display_message(
            &actor,
            message(1, MessagePriority::NormalCycle, None),
            &[MessageFormat::Ascii],
        )
        .await;
        handle_set_display_message(
            &actor,
            message(2, MessagePriority::AlwaysFront, None),
            &[MessageFormat::Ascii],
        )
        .await;

        let outcome = handle_get_display_messages(&actor, &DisplayMessageFilter::default());

        match outcome {
            GetDisplayMessagesOutcome::Accepted(messages) => assert_eq!(messages.len(), 2),
            other => panic!("expected Accepted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_priority_filter_only_matches_that_priority() {
        let actor = spawn_with_display([1]).await;
        handle_set_display_message(
            &actor,
            message(1, MessagePriority::NormalCycle, None),
            &[MessageFormat::Ascii],
        )
        .await;
        handle_set_display_message(
            &actor,
            message(2, MessagePriority::AlwaysFront, None),
            &[MessageFormat::Ascii],
        )
        .await;

        let outcome = handle_get_display_messages(
            &actor,
            &DisplayMessageFilter {
                priority: Some(MessagePriority::AlwaysFront),
                ..Default::default()
            },
        );

        match outcome {
            GetDisplayMessagesOutcome::Accepted(messages) => {
                assert_eq!(
                    messages,
                    alloc::vec![message(2, MessagePriority::AlwaysFront, None)]
                );
            }
            other => panic!("expected Accepted, got {other:?}"),
        }
    }

    // --- chunking ---

    #[test]
    fn chunking_an_empty_list_produces_no_chunks() {
        assert!(chunk_display_messages(&[]).is_empty());
    }

    #[test]
    fn chunking_splits_at_the_configured_size() {
        let messages: Vec<DisplayedMessage> = (0..(DISPLAY_MESSAGE_CHUNK_SIZE + 1))
            .map(|i| message(i as i64, MessagePriority::NormalCycle, None))
            .collect();

        let chunks = chunk_display_messages(&messages);

        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].tbc);
        assert!(!chunks[1].tbc);
        assert_eq!(chunks[0].messages.len(), DISPLAY_MESSAGE_CHUNK_SIZE);
        assert_eq!(chunks[1].messages.len(), 1);
    }

    // --- current_message / current_message_state ---

    #[test]
    fn an_idle_charge_point_with_no_messages_shows_nothing() {
        let state = ChargePointState::new([1]);

        assert_eq!(current_message_state(&state), MessageState::Idle);
        assert_eq!(current_message(&state), None);
    }

    #[tokio::test]
    async fn the_highest_priority_matching_message_is_shown() {
        let actor = spawn_with_display([1]).await;
        handle_set_display_message(
            &actor,
            message(1, MessagePriority::NormalCycle, None),
            &[MessageFormat::Ascii],
        )
        .await;
        handle_set_display_message(
            &actor,
            message(2, MessagePriority::AlwaysFront, None),
            &[MessageFormat::Ascii],
        )
        .await;

        let shown = current_message(&actor.state());

        assert_eq!(shown.unwrap().id, DisplayMessageId(2));
    }

    #[tokio::test]
    async fn a_message_scoped_to_charging_only_shows_while_charging() {
        let actor = spawn_with_display([1]).await;
        handle_set_display_message(
            &actor,
            message(
                1,
                MessagePriority::NormalCycle,
                Some(MessageState::Charging),
            ),
            &[MessageFormat::Ascii],
        )
        .await;

        assert_eq!(current_message(&actor.state()), None, "idle: not shown yet");

        actor
            .send(ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event: ConnectorEvent::CableConnected,
                },
            })
            .await
            .unwrap();
        actor
            .send(ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event: ConnectorEvent::LockConfirmed,
                },
            })
            .await
            .unwrap();
        actor
            .send(ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event: ConnectorEvent::IdTokenPresented(crate::state::IdToken {
                        value: "abc".into(),
                        kind: crate::state::IdTokenKind::ISO14443,
                    }),
                },
            })
            .await
            .unwrap();
        actor
            .send(ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event: ConnectorEvent::ChargingAuthorized(crate::state::IdToken {
                        value: "abc".into(),
                        kind: crate::state::IdTokenKind::ISO14443,
                    }),
                },
            })
            .await
            .unwrap();
        actor
            .send(ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event: ConnectorEvent::ContactorClosed,
                },
            })
            .await
            .unwrap();

        assert_eq!(
            actor.state().evses[0].connectors[0],
            ConnectorState::Charging
        );
        assert_eq!(
            current_message(&actor.state()).map(|m| m.id),
            Some(DisplayMessageId(1)),
            "charging: the scoped message now applies"
        );
    }

    #[tokio::test]
    async fn a_fault_takes_priority_over_charging_in_the_derived_state() {
        let actor = spawn_with_display([1]).await;

        actor
            .send(ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event: ConnectorEvent::FaultDetected,
                },
            })
            .await
            .unwrap();

        assert_eq!(current_message_state(&actor.state()), MessageState::Faulted);
    }

    #[test]
    fn run_display_updates_compiles_against_a_generic_display() {
        // Compile-level check only - a live run loop is exercised end-to-end by the hardware
        // module's own tests via `crate::hardware::NoDisplay`.
        fn assert_bound<D: crate::hardware::Display>() {}
        assert_bound::<crate::hardware::NoDisplay>();
    }
}

#[cfg(feature = "ocpp_2_1")]
mod ocpp_2_1 {
    use super::{
        ClearDisplayMessageHandler, ClearDisplayMessageOutcome, DisplayMessageFilter,
        GetDisplayMessagesHandler, GetDisplayMessagesOutcome, SetDisplayMessageHandler,
        SetDisplayMessageOutcome, chunk_display_messages, handle_clear_display_message,
        handle_get_display_messages, handle_set_display_message,
    };
    use crate::actor::ChargePointActor;
    use crate::state::{
        DisplayMessageId, DisplayedMessage, MessageContent, MessageFormat, MessagePriority,
        MessageState, TransactionId,
    };
    use crate::wire::v21::common::{
        ClearMessageStatusEnum, DisplayMessageStatusEnum, GetDisplayMessagesStatusEnum,
        MessageContent as WireMessageContent, MessageFormatEnum, MessageInfo, MessagePriorityEnum,
        MessageStateEnum,
    };
    use crate::wire::v21::{
        ClearDisplayMessageResponse, GetDisplayMessagesRequest, GetDisplayMessagesResponse,
        NotifyDisplayMessagesRequest, SetDisplayMessageResponse,
    };
    use alloc::boxed::Box;
    use alloc::string::ToString;
    use alloc::vec::Vec;
    use ocpp_client::ocpp_2_1::OCPP2_1Client;

    /// Truncates/bounds `value` to fit a `heapless::String<N>`, mirroring
    /// `crate::reporting::ocpp_2_1::bounded_string` - duplicated per this crate's small-helper
    /// convention rather than shared across modules.
    fn bounded_string<const N: usize>(value: &str) -> heapless::String<N> {
        let mut end = value.len().min(N);
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        heapless::String::try_from(&value[..end]).expect("truncated to fit the wire bound")
    }

    /// Truncates `value` to at most `max_bytes` bytes, on a UTF-8 boundary, and hands back an
    /// owned `String`.
    ///
    /// The sibling of [`bounded_string`] for the wire fields `ocpp-types` 0.2.0 retyped from
    /// `heapless::String<N>` to an unbounded `String` - the specification leaves their length to
    /// a device-model variable rather than fixing it, so the bound became this crate's to apply
    /// instead of the type's. The byte bounds are kept exactly where the `heapless` capacities
    /// had them, so what goes on the wire is unchanged.
    fn bounded_owned(value: &str, max_bytes: usize) -> alloc::string::String {
        let mut end = value.len().min(max_bytes);
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        value[..end].into()
    }

    fn map_message_format(format: &MessageFormatEnum) -> MessageFormat {
        match format {
            MessageFormatEnum::ASCII => MessageFormat::Ascii,
            MessageFormatEnum::HTML => MessageFormat::Html,
            MessageFormatEnum::URI => MessageFormat::Uri,
            MessageFormatEnum::UTF8 => MessageFormat::Utf8,
            MessageFormatEnum::QRCODE => MessageFormat::QrCode,
        }
    }

    fn build_message_format(format: MessageFormat) -> MessageFormatEnum {
        match format {
            MessageFormat::Ascii => MessageFormatEnum::ASCII,
            MessageFormat::Html => MessageFormatEnum::HTML,
            MessageFormat::Uri => MessageFormatEnum::URI,
            MessageFormat::Utf8 => MessageFormatEnum::UTF8,
            MessageFormat::QrCode => MessageFormatEnum::QRCODE,
        }
    }

    fn map_message_priority(priority: &MessagePriorityEnum) -> MessagePriority {
        match priority {
            MessagePriorityEnum::AlwaysFront => MessagePriority::AlwaysFront,
            MessagePriorityEnum::InFront => MessagePriority::InFront,
            MessagePriorityEnum::NormalCycle => MessagePriority::NormalCycle,
        }
    }

    fn build_message_priority(priority: MessagePriority) -> MessagePriorityEnum {
        match priority {
            MessagePriority::AlwaysFront => MessagePriorityEnum::AlwaysFront,
            MessagePriority::InFront => MessagePriorityEnum::InFront,
            MessagePriority::NormalCycle => MessagePriorityEnum::NormalCycle,
        }
    }

    /// `None` for 2.1's `Suspended`/`Discharging`, which this crate's [`MessageState`] does not
    /// model yet - see that type's docs.
    fn map_message_state(state: &MessageStateEnum) -> Option<MessageState> {
        match state {
            MessageStateEnum::Charging => Some(MessageState::Charging),
            MessageStateEnum::Faulted => Some(MessageState::Faulted),
            MessageStateEnum::Idle => Some(MessageState::Idle),
            MessageStateEnum::Unavailable => Some(MessageState::Unavailable),
            MessageStateEnum::Suspended | MessageStateEnum::Discharging => None,
        }
    }

    fn build_message_state(state: MessageState) -> MessageStateEnum {
        match state {
            MessageState::Charging => MessageStateEnum::Charging,
            MessageState::Faulted => MessageStateEnum::Faulted,
            MessageState::Idle => MessageStateEnum::Idle,
            MessageState::Unavailable => MessageStateEnum::Unavailable,
        }
    }

    /// Parses a wire `transactionId` string into a [`TransactionId`]. A value that doesn't parse
    /// as `u64` (malformed, or simply not a transaction id this crate ever issues) becomes a
    /// sentinel no real transaction can ever have, so [`crate::display_message::handle_set_display_message`]'s
    /// existence check naturally answers `UnknownTransaction` rather than needing a second error
    /// path - mirroring `crate::reservation::parse_expiry_date_time`'s "forgive, don't reject
    /// outright" stance for a malformed-but-not-fatal field.
    fn parse_transaction_id(raw: &str) -> TransactionId {
        TransactionId(raw.parse().unwrap_or(u64::MAX))
    }

    fn map_message_content(content: &WireMessageContent) -> MessageContent {
        MessageContent {
            content: content.content.to_string(),
            format: map_message_format(&content.format),
            language: content.language.as_ref().map(|l| l.to_string()),
        }
    }

    fn build_message_content(content: &MessageContent) -> WireMessageContent {
        WireMessageContent {
            content: bounded_owned(&content.content, 1024),
            custom_data: None,
            format: build_message_format(content.format),
            language: content.language.as_deref().map(bounded_string::<8>),
        }
    }

    fn map_message_info(info: &MessageInfo) -> DisplayedMessage {
        DisplayedMessage {
            id: DisplayMessageId(info.id),
            priority: map_message_priority(&info.priority),
            state: info.state.as_ref().and_then(map_message_state),
            message: map_message_content(&info.message),
            transaction_id: info
                .transaction_id
                .as_ref()
                .map(|id| parse_transaction_id(id)),
        }
    }

    fn build_message_info(message: &DisplayedMessage) -> MessageInfo {
        MessageInfo {
            custom_data: None,
            display: None,
            end_date_time: None,
            id: message.id.0,
            message: build_message_content(&message.message),
            message_extra: None,
            priority: build_message_priority(message.priority),
            start_date_time: None,
            state: message.state.map(build_message_state),
            transaction_id: message
                .transaction_id
                .map(|id| bounded_string::<36>(&id.0.to_string())),
        }
    }

    fn map_set_outcome(outcome: SetDisplayMessageOutcome) -> DisplayMessageStatusEnum {
        match outcome {
            SetDisplayMessageOutcome::Accepted => DisplayMessageStatusEnum::Accepted,
            SetDisplayMessageOutcome::NotSupportedMessageFormat => {
                DisplayMessageStatusEnum::NotSupportedMessageFormat
            }
            SetDisplayMessageOutcome::Rejected => DisplayMessageStatusEnum::Rejected,
            SetDisplayMessageOutcome::NotSupportedPriority => {
                DisplayMessageStatusEnum::NotSupportedPriority
            }
            SetDisplayMessageOutcome::NotSupportedState => {
                DisplayMessageStatusEnum::NotSupportedState
            }
            SetDisplayMessageOutcome::UnknownTransaction => {
                DisplayMessageStatusEnum::UnknownTransaction
            }
            SetDisplayMessageOutcome::LanguageNotSupported => {
                DisplayMessageStatusEnum::LanguageNotSupported
            }
        }
    }

    fn map_clear_outcome(outcome: ClearDisplayMessageOutcome) -> ClearMessageStatusEnum {
        match outcome {
            ClearDisplayMessageOutcome::Accepted => ClearMessageStatusEnum::Accepted,
            ClearDisplayMessageOutcome::Unknown => ClearMessageStatusEnum::Unknown,
            ClearDisplayMessageOutcome::Rejected => ClearMessageStatusEnum::Rejected,
        }
    }

    fn map_get_outcome(outcome: &GetDisplayMessagesOutcome) -> GetDisplayMessagesStatusEnum {
        match outcome {
            GetDisplayMessagesOutcome::Accepted(_) => GetDisplayMessagesStatusEnum::Accepted,
            GetDisplayMessagesOutcome::Unknown => GetDisplayMessagesStatusEnum::Unknown,
        }
    }

    fn build_filter(request: &GetDisplayMessagesRequest) -> DisplayMessageFilter {
        DisplayMessageFilter {
            ids: request
                .id
                .as_ref()
                .map(|ids| ids.iter().map(|id| DisplayMessageId(*id)).collect())
                .unwrap_or_default(),
            priority: request.priority.as_ref().map(map_message_priority),
            state: request.state.as_ref().and_then(map_message_state),
        }
    }

    #[async_trait::async_trait]
    impl SetDisplayMessageHandler for OCPP2_1Client {
        async fn register_set_display_message_handler(
            &self,
            actor: ChargePointActor,
            supported_formats: Vec<MessageFormat>,
        ) {
            self.on_set_display_message(move |request, _client| {
                let actor = actor.clone();
                let supported_formats = supported_formats.clone();
                async move {
                    let outcome = handle_set_display_message(
                        &actor,
                        map_message_info(&request.message),
                        &supported_formats,
                    )
                    .await;
                    Ok(SetDisplayMessageResponse {
                        custom_data: None,
                        status: map_set_outcome(outcome),
                        status_info: None,
                    })
                }
            })
            .await;
        }
    }

    #[async_trait::async_trait]
    impl ClearDisplayMessageHandler for OCPP2_1Client {
        async fn register_clear_display_message_handler(&self, actor: ChargePointActor) {
            self.on_clear_display_message(move |request, _client| {
                let actor = actor.clone();
                async move {
                    let outcome =
                        handle_clear_display_message(&actor, DisplayMessageId(request.id)).await;
                    Ok(ClearDisplayMessageResponse {
                        custom_data: None,
                        status: map_clear_outcome(outcome),
                        status_info: None,
                    })
                }
            })
            .await;
        }
    }

    #[async_trait::async_trait]
    impl GetDisplayMessagesHandler for OCPP2_1Client {
        async fn register_get_display_messages_handler(&self, actor: ChargePointActor) {
            self.on_get_display_messages(move |request, client| {
                let actor = actor.clone();
                async move {
                    let filter = build_filter(&request);
                    let outcome = handle_get_display_messages(&actor, &filter);
                    let response = GetDisplayMessagesResponse {
                        custom_data: None,
                        status: map_get_outcome(&outcome),
                        status_info: None,
                    };
                    if let GetDisplayMessagesOutcome::Accepted(messages) = outcome {
                        for chunk in chunk_display_messages(&messages) {
                            let message_info: Vec<_> =
                                chunk.messages.iter().map(build_message_info).collect();
                            let notification = NotifyDisplayMessagesRequest {
                                custom_data: None,
                                message_info: Some(message_info),
                                request_id: request.request_id,
                                tbc: Some(chunk.tbc),
                            };
                            if let Err(err) =
                                client.send_notify_display_messages(notification).await
                            {
                                tracing::warn!(
                                    error = %err,
                                    "failed to send a NotifyDisplayMessages chunk"
                                );
                            }
                        }
                    }
                    Ok(response)
                }
            })
            .await;
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn every_set_outcome_maps_to_the_matching_wire_status() {
            assert_eq!(
                map_set_outcome(SetDisplayMessageOutcome::Accepted),
                DisplayMessageStatusEnum::Accepted
            );
            assert_eq!(
                map_set_outcome(SetDisplayMessageOutcome::NotSupportedMessageFormat),
                DisplayMessageStatusEnum::NotSupportedMessageFormat
            );
            assert_eq!(
                map_set_outcome(SetDisplayMessageOutcome::Rejected),
                DisplayMessageStatusEnum::Rejected
            );
            assert_eq!(
                map_set_outcome(SetDisplayMessageOutcome::NotSupportedPriority),
                DisplayMessageStatusEnum::NotSupportedPriority
            );
            assert_eq!(
                map_set_outcome(SetDisplayMessageOutcome::NotSupportedState),
                DisplayMessageStatusEnum::NotSupportedState
            );
            assert_eq!(
                map_set_outcome(SetDisplayMessageOutcome::UnknownTransaction),
                DisplayMessageStatusEnum::UnknownTransaction
            );
            assert_eq!(
                map_set_outcome(SetDisplayMessageOutcome::LanguageNotSupported),
                DisplayMessageStatusEnum::LanguageNotSupported
            );
        }

        #[test]
        fn every_clear_outcome_maps_to_the_matching_wire_status() {
            assert_eq!(
                map_clear_outcome(ClearDisplayMessageOutcome::Accepted),
                ClearMessageStatusEnum::Accepted
            );
            assert_eq!(
                map_clear_outcome(ClearDisplayMessageOutcome::Unknown),
                ClearMessageStatusEnum::Unknown
            );
            assert_eq!(
                map_clear_outcome(ClearDisplayMessageOutcome::Rejected),
                ClearMessageStatusEnum::Rejected
            );
        }

        #[test]
        fn every_get_outcome_maps_to_the_matching_wire_status() {
            assert_eq!(
                map_get_outcome(&GetDisplayMessagesOutcome::Accepted(Vec::new())),
                GetDisplayMessagesStatusEnum::Accepted
            );
            assert_eq!(
                map_get_outcome(&GetDisplayMessagesOutcome::Unknown),
                GetDisplayMessagesStatusEnum::Unknown
            );
        }

        #[test]
        fn v2x_only_message_states_map_to_none() {
            assert_eq!(map_message_state(&MessageStateEnum::Suspended), None);
            assert_eq!(map_message_state(&MessageStateEnum::Discharging), None);
            assert_eq!(
                map_message_state(&MessageStateEnum::Charging),
                Some(MessageState::Charging)
            );
        }

        #[test]
        fn a_malformed_transaction_id_becomes_a_never_matching_sentinel() {
            assert_eq!(
                parse_transaction_id("not-a-number"),
                TransactionId(u64::MAX)
            );
            assert_eq!(parse_transaction_id("42"), TransactionId(42));
        }

        #[test]
        fn round_tripping_message_info_preserves_every_field() {
            let message = DisplayedMessage {
                id: DisplayMessageId(7),
                priority: MessagePriority::InFront,
                state: Some(MessageState::Idle),
                message: MessageContent {
                    content: "hello".into(),
                    format: MessageFormat::Utf8,
                    language: Some("en".into()),
                },
                transaction_id: Some(TransactionId(9)),
            };

            let wire = build_message_info(&message);
            let mapped_back = map_message_info(&wire);

            assert_eq!(mapped_back, message);
        }

        #[test]
        fn ocpp2_1_client_implements_every_display_message_handler_trait() {
            fn assert_impl<
                T: SetDisplayMessageHandler + ClearDisplayMessageHandler + GetDisplayMessagesHandler,
            >() {
            }
            assert_impl::<OCPP2_1Client>();
        }
    }
}

/// The OCPP 2.0.1 projection - identical wire shapes to 2.1's, except `MessageStateEnum` has no
/// `Suspended`/`Discharging` (2.1 added those for V2X), so every state 2.0.1 can send maps
/// directly onto [`MessageState`] with no `None` case to handle.
#[cfg(feature = "ocpp_2_0_1")]
mod ocpp_2_0_1 {
    use super::{
        ClearDisplayMessageHandler, ClearDisplayMessageOutcome, DisplayMessageFilter,
        GetDisplayMessagesHandler, GetDisplayMessagesOutcome, SetDisplayMessageHandler,
        SetDisplayMessageOutcome, chunk_display_messages, handle_clear_display_message,
        handle_get_display_messages, handle_set_display_message,
    };
    use crate::actor::ChargePointActor;
    use crate::state::{
        DisplayMessageId, DisplayedMessage, MessageContent, MessageFormat, MessagePriority,
        MessageState, TransactionId,
    };
    use crate::wire::v201::common::{
        ClearMessageStatusEnum, DisplayMessageStatusEnum, GetDisplayMessagesStatusEnum,
        MessageContent as WireMessageContent, MessageFormatEnum, MessageInfo, MessagePriorityEnum,
        MessageStateEnum,
    };
    use crate::wire::v201::{
        ClearDisplayMessageResponse, GetDisplayMessagesRequest, GetDisplayMessagesResponse,
        NotifyDisplayMessagesRequest, SetDisplayMessageResponse,
    };
    use alloc::boxed::Box;
    use alloc::string::ToString;
    use alloc::vec::Vec;
    use ocpp_client::ocpp_2_0_1::OCPP2_0_1Client;

    fn bounded_string<const N: usize>(value: &str) -> heapless::String<N> {
        let mut end = value.len().min(N);
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        heapless::String::try_from(&value[..end]).expect("truncated to fit the wire bound")
    }

    fn map_message_format(format: &MessageFormatEnum) -> MessageFormat {
        match format {
            MessageFormatEnum::ASCII => MessageFormat::Ascii,
            MessageFormatEnum::HTML => MessageFormat::Html,
            MessageFormatEnum::URI => MessageFormat::Uri,
            MessageFormatEnum::UTF8 => MessageFormat::Utf8,
        }
    }

    fn build_message_format(format: MessageFormat) -> MessageFormatEnum {
        match format {
            MessageFormat::Ascii => MessageFormatEnum::ASCII,
            MessageFormat::Html => MessageFormatEnum::HTML,
            MessageFormat::Uri => MessageFormatEnum::URI,
            MessageFormat::Utf8 => MessageFormatEnum::UTF8,
            // 2.0.1 has no QR code format; the closest honest projection is URI (a QR payload is
            // typically a URI anyway) rather than silently mislabeling it ASCII.
            MessageFormat::QrCode => MessageFormatEnum::URI,
        }
    }

    fn map_message_priority(priority: &MessagePriorityEnum) -> MessagePriority {
        match priority {
            MessagePriorityEnum::AlwaysFront => MessagePriority::AlwaysFront,
            MessagePriorityEnum::InFront => MessagePriority::InFront,
            MessagePriorityEnum::NormalCycle => MessagePriority::NormalCycle,
        }
    }

    fn build_message_priority(priority: MessagePriority) -> MessagePriorityEnum {
        match priority {
            MessagePriority::AlwaysFront => MessagePriorityEnum::AlwaysFront,
            MessagePriority::InFront => MessagePriorityEnum::InFront,
            MessagePriority::NormalCycle => MessagePriorityEnum::NormalCycle,
        }
    }

    fn map_message_state(state: &MessageStateEnum) -> MessageState {
        match state {
            MessageStateEnum::Charging => MessageState::Charging,
            MessageStateEnum::Faulted => MessageState::Faulted,
            MessageStateEnum::Idle => MessageState::Idle,
            MessageStateEnum::Unavailable => MessageState::Unavailable,
        }
    }

    fn build_message_state(state: MessageState) -> MessageStateEnum {
        match state {
            MessageState::Charging => MessageStateEnum::Charging,
            MessageState::Faulted => MessageStateEnum::Faulted,
            MessageState::Idle => MessageStateEnum::Idle,
            MessageState::Unavailable => MessageStateEnum::Unavailable,
        }
    }

    fn parse_transaction_id(raw: &str) -> TransactionId {
        TransactionId(raw.parse().unwrap_or(u64::MAX))
    }

    fn map_message_content(content: &WireMessageContent) -> MessageContent {
        MessageContent {
            content: content.content.to_string(),
            format: map_message_format(&content.format),
            language: content.language.as_ref().map(|l| l.to_string()),
        }
    }

    fn build_message_content(content: &MessageContent) -> WireMessageContent {
        WireMessageContent {
            content: bounded_string::<512>(&content.content),
            custom_data: None,
            format: build_message_format(content.format),
            language: content.language.as_deref().map(bounded_string::<8>),
        }
    }

    fn map_message_info(info: &MessageInfo) -> DisplayedMessage {
        DisplayedMessage {
            id: DisplayMessageId(info.id),
            priority: map_message_priority(&info.priority),
            state: info.state.as_ref().map(map_message_state),
            message: map_message_content(&info.message),
            transaction_id: info
                .transaction_id
                .as_ref()
                .map(|id| parse_transaction_id(id)),
        }
    }

    fn build_message_info(message: &DisplayedMessage) -> MessageInfo {
        MessageInfo {
            custom_data: None,
            display: None,
            end_date_time: None,
            id: message.id.0,
            message: build_message_content(&message.message),
            priority: build_message_priority(message.priority),
            start_date_time: None,
            state: message.state.map(build_message_state),
            transaction_id: message
                .transaction_id
                .map(|id| bounded_string::<36>(&id.0.to_string())),
        }
    }

    /// 2.0.1's `DisplayMessageStatusEnum` has no `LanguageNotSupported` (2.1 added it) - never
    /// produced by [`handle_set_display_message`] today (see that outcome's docs), but projected
    /// onto `NotSupportedMessageFormat` rather than the more generic `Rejected` if it ever is:
    /// both describe "this hardware cannot render what was asked for", and that is a closer match
    /// than a bare "no".
    fn map_set_outcome(outcome: SetDisplayMessageOutcome) -> DisplayMessageStatusEnum {
        match outcome {
            SetDisplayMessageOutcome::Accepted => DisplayMessageStatusEnum::Accepted,
            SetDisplayMessageOutcome::NotSupportedMessageFormat
            | SetDisplayMessageOutcome::LanguageNotSupported => {
                DisplayMessageStatusEnum::NotSupportedMessageFormat
            }
            SetDisplayMessageOutcome::Rejected => DisplayMessageStatusEnum::Rejected,
            SetDisplayMessageOutcome::NotSupportedPriority => {
                DisplayMessageStatusEnum::NotSupportedPriority
            }
            SetDisplayMessageOutcome::NotSupportedState => {
                DisplayMessageStatusEnum::NotSupportedState
            }
            SetDisplayMessageOutcome::UnknownTransaction => {
                DisplayMessageStatusEnum::UnknownTransaction
            }
        }
    }

    /// 2.0.1's `ClearMessageStatusEnum` has only `Accepted`/`Unknown` - no `Rejected` (2.1 added
    /// it). A capability-absent refusal (`ClearDisplayMessageOutcome::Rejected`) therefore
    /// projects onto `Unknown`: from the CSMS's perspective, a charge point with no display holds
    /// no messages either way, so "unknown message id" is the honest reading available on this
    /// wire.
    fn map_clear_outcome(outcome: ClearDisplayMessageOutcome) -> ClearMessageStatusEnum {
        match outcome {
            ClearDisplayMessageOutcome::Accepted => ClearMessageStatusEnum::Accepted,
            ClearDisplayMessageOutcome::Unknown | ClearDisplayMessageOutcome::Rejected => {
                ClearMessageStatusEnum::Unknown
            }
        }
    }

    fn map_get_outcome(outcome: &GetDisplayMessagesOutcome) -> GetDisplayMessagesStatusEnum {
        match outcome {
            GetDisplayMessagesOutcome::Accepted(_) => GetDisplayMessagesStatusEnum::Accepted,
            GetDisplayMessagesOutcome::Unknown => GetDisplayMessagesStatusEnum::Unknown,
        }
    }

    fn build_filter(request: &GetDisplayMessagesRequest) -> DisplayMessageFilter {
        DisplayMessageFilter {
            ids: request
                .id
                .as_ref()
                .map(|ids| ids.iter().map(|id| DisplayMessageId(*id)).collect())
                .unwrap_or_default(),
            priority: request.priority.as_ref().map(map_message_priority),
            state: request.state.as_ref().map(map_message_state),
        }
    }

    #[async_trait::async_trait]
    impl SetDisplayMessageHandler for OCPP2_0_1Client {
        async fn register_set_display_message_handler(
            &self,
            actor: ChargePointActor,
            supported_formats: Vec<MessageFormat>,
        ) {
            self.on_set_display_message(move |request, _client| {
                let actor = actor.clone();
                let supported_formats = supported_formats.clone();
                async move {
                    let outcome = handle_set_display_message(
                        &actor,
                        map_message_info(&request.message),
                        &supported_formats,
                    )
                    .await;
                    Ok(SetDisplayMessageResponse {
                        custom_data: None,
                        status: map_set_outcome(outcome),
                        status_info: None,
                    })
                }
            })
            .await;
        }
    }

    #[async_trait::async_trait]
    impl ClearDisplayMessageHandler for OCPP2_0_1Client {
        async fn register_clear_display_message_handler(&self, actor: ChargePointActor) {
            self.on_clear_display_message(move |request, _client| {
                let actor = actor.clone();
                async move {
                    let outcome =
                        handle_clear_display_message(&actor, DisplayMessageId(request.id)).await;
                    Ok(ClearDisplayMessageResponse {
                        custom_data: None,
                        status: map_clear_outcome(outcome),
                        status_info: None,
                    })
                }
            })
            .await;
        }
    }

    #[async_trait::async_trait]
    impl GetDisplayMessagesHandler for OCPP2_0_1Client {
        async fn register_get_display_messages_handler(&self, actor: ChargePointActor) {
            self.on_get_display_messages(move |request, client| {
                let actor = actor.clone();
                async move {
                    let filter = build_filter(&request);
                    let outcome = handle_get_display_messages(&actor, &filter);
                    let response = GetDisplayMessagesResponse {
                        custom_data: None,
                        status: map_get_outcome(&outcome),
                        status_info: None,
                    };
                    if let GetDisplayMessagesOutcome::Accepted(messages) = outcome {
                        for chunk in chunk_display_messages(&messages) {
                            let message_info: Vec<_> =
                                chunk.messages.iter().map(build_message_info).collect();
                            let notification = NotifyDisplayMessagesRequest {
                                custom_data: None,
                                message_info: Some(message_info),
                                request_id: request.request_id,
                                tbc: Some(chunk.tbc),
                            };
                            if let Err(err) =
                                client.send_notify_display_messages(notification).await
                            {
                                tracing::warn!(
                                    error = %err,
                                    "failed to send a NotifyDisplayMessages chunk"
                                );
                            }
                        }
                    }
                    Ok(response)
                }
            })
            .await;
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn every_set_outcome_maps_to_the_matching_wire_status() {
            assert_eq!(
                map_set_outcome(SetDisplayMessageOutcome::Accepted),
                DisplayMessageStatusEnum::Accepted
            );
            assert_eq!(
                map_set_outcome(SetDisplayMessageOutcome::UnknownTransaction),
                DisplayMessageStatusEnum::UnknownTransaction
            );
            assert_eq!(
                map_set_outcome(SetDisplayMessageOutcome::LanguageNotSupported),
                DisplayMessageStatusEnum::NotSupportedMessageFormat,
                "2.0.1 has no LanguageNotSupported status - the closest honest projection"
            );
        }

        #[test]
        fn a_capability_absent_clear_refusal_projects_to_unknown() {
            assert_eq!(
                map_clear_outcome(ClearDisplayMessageOutcome::Rejected),
                ClearMessageStatusEnum::Unknown,
                "2.0.1 has no Rejected status for ClearDisplayMessage"
            );
        }

        #[test]
        fn a_qr_code_format_projects_to_uri_rather_than_a_wrong_format() {
            assert_eq!(
                build_message_format(MessageFormat::QrCode),
                MessageFormatEnum::URI
            );
        }

        #[test]
        fn round_tripping_message_info_preserves_every_field() {
            let message = DisplayedMessage {
                id: DisplayMessageId(3),
                priority: MessagePriority::AlwaysFront,
                state: Some(MessageState::Faulted),
                message: MessageContent {
                    content: "fault".into(),
                    format: MessageFormat::Ascii,
                    language: Some("en".into()),
                },
                transaction_id: None,
            };

            let wire = build_message_info(&message);
            let mapped_back = map_message_info(&wire);

            assert_eq!(mapped_back, message);
        }

        #[test]
        fn ocpp2_0_1_client_implements_every_display_message_handler_trait() {
            fn assert_impl<
                T: SetDisplayMessageHandler + ClearDisplayMessageHandler + GetDisplayMessagesHandler,
            >() {
            }
            assert_impl::<OCPP2_0_1Client>();
        }
    }
}
