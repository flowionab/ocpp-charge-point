use alloc::string::String;
use alloc::vec::Vec;

use crate::state::TransactionId;

/// A display message's identifier (OCPP `MessageInfo.id`), assigned by the CSMS via
/// `SetDisplayMessage` and referenced again by `ClearDisplayMessage`/`GetDisplayMessages`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DisplayMessageId(pub i64);

/// How a display message competes with others for the driver's attention (OCPP
/// `MessagePriorityEnum`). Ordered least to most attention-grabbing via `Ord` - higher sorts
/// greater - which is exactly the ordering [`crate::display_message::current_message`] needs to
/// pick one message when several apply to the same [`MessageState`] at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MessagePriority {
    /// Shown as part of the normal message cycle, alongside other `NormalCycle` messages.
    NormalCycle,
    /// Shown in front of any `NormalCycle` message, but yields to an `AlwaysFront` one.
    InFront,
    /// Always shown, overriding every other message.
    AlwaysFront,
}

impl MessagePriority {
    /// Every priority this crate models, most attention-grabbing first - the order OCPP's own
    /// `MessagePriorityEnumType` lists them in, and the order
    /// `DisplayMessageCtrlr.SupportedPriorities` advertises (see
    /// `crate::device_model`'s `CAPABILITY_GATED_VARIABLES`).
    ///
    /// All three exist here because the derivation in [`crate::display_message::current_message`]
    /// really does honour all three; a value missing from this list would be one a
    /// `SetDisplayMessage` gets refused for.
    pub const ALL: &'static [Self] = &[Self::AlwaysFront, Self::InFront, Self::NormalCycle];

    /// This priority's OCPP `MessagePriorityEnumType` name, for the device model and for logs.
    ///
    /// Exhaustive with no wildcard arm on purpose (`CLAUDE.md`): a new priority must be a compile
    /// error here rather than a variant silently missing from what this charge point advertises.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::NormalCycle => "NormalCycle",
            Self::InFront => "InFront",
            Self::AlwaysFront => "AlwaysFront",
        }
    }
}

/// Which charge-point condition a display message applies to (OCPP `MessageStateEnum`), collapsed
/// to the four values every OCPP version this crate targets shares. 2.1 additionally defines
/// `Suspended`/`Discharging` for V2X/bidirectional power, which this crate does not yet model
/// (`docs/PRODUCTION-ROADMAP.md` B6, B8) - a `SetDisplayMessage` naming one of those is refused
/// with `NotSupportedState` rather than silently dropped or folded into a nearby value, see
/// `crate::display_message::ocpp_2_1::map_message_state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageState {
    /// A transaction is actively drawing energy.
    Charging,
    /// The charge point (or the connector this message is scoped to) is faulted.
    Faulted,
    /// No transaction is in progress and nothing is faulted or unavailable.
    Idle,
    /// The charge point (or connector) has been made unavailable.
    Unavailable,
}

impl MessageState {
    /// Every state this crate models, in OCPP's own `MessageStateEnumType` order - and exactly
    /// what `DisplayMessageCtrlr.SupportedStates` advertises (see `crate::device_model`'s
    /// `CAPABILITY_GATED_VARIABLES`).
    ///
    /// 2.1's `Suspended`/`Discharging` are deliberately absent, matching this type's own docs: a
    /// `SetDisplayMessage` naming one is refused with `NotSupportedState`, and a CSMS that reads
    /// this variable learns that before composing one.
    pub const ALL: &'static [Self] =
        &[Self::Charging, Self::Faulted, Self::Idle, Self::Unavailable];

    /// This state's OCPP `MessageStateEnumType` name, for the device model and for logs.
    ///
    /// Exhaustive with no wildcard arm on purpose (`CLAUDE.md`), for the same reason as
    /// [`MessagePriority::name`].
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Charging => "Charging",
            Self::Faulted => "Faulted",
            Self::Idle => "Idle",
            Self::Unavailable => "Unavailable",
        }
    }
}

/// A display message's rendering format (OCPP `MessageFormatEnum`) - what a hardware binding must
/// be able to render for [`crate::display_message::handle_set_display_message`] to accept a
/// message using it. See [`crate::hardware::Display::supported_formats`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageFormat {
    /// Plain ASCII text.
    Ascii,
    /// A restricted HTML subset.
    Html,
    /// A URI the display should fetch and render.
    Uri,
    /// UTF-8 text (beyond plain ASCII).
    Utf8,
    /// A QR code encoding the content.
    QrCode,
}

impl MessageFormat {
    /// This format's OCPP `MessageFormatEnumType` name, as
    /// [`crate::builder::ChargePointBuilder::display_messages`] writes it into
    /// `DisplayMessageCtrlr.SupportedFormats`.
    ///
    /// Note the wire spelling is not the Rust one: OCPP writes `ASCII`/`UTF8`/`QRCODE`, so this
    /// cannot be derived from the variant name. Exhaustive with no wildcard arm on purpose
    /// (`CLAUDE.md`) - a new format must be a compile error rather than an unadvertised one.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Ascii => "ASCII",
            Self::Html => "HTML",
            Self::Uri => "URI",
            Self::Utf8 => "UTF8",
            Self::QrCode => "QRCODE",
        }
    }
}

/// One message's renderable content (OCPP `MessageContent`, collapsed to the fields this crate
/// models - see [`DisplayedMessage`]'s docs for what is deliberately left out).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageContent {
    /// The text/URI/QR payload to render.
    pub content: String,
    /// The format `content` is in.
    pub format: MessageFormat,
    /// The message's language, as an RFC 5646 tag (e.g. `"en"`), if the CSMS specified one.
    pub language: Option<String>,
}

/// A message the CSMS has asked this charge point to display (OCPP `MessageInfo`, as installed by
/// `SetDisplayMessage` and reported back by `GetDisplayMessages`/`NotifyDisplayMessages`).
///
/// Deliberately narrower than OCPP's full `MessageInfo`: `messageExtra` (2.1's up-to-4 additional
/// same-message translations) and the `display` component target (routing a message to one of
/// several physical screens) are not modeled - this crate targets a single display surface (see
/// [`crate::hardware::Display`]) and a single rendered language per message, which is what every
/// hardware binding this crate has today actually is. `startDateTime`/`endDateTime` are likewise
/// not enforced - see `crate::display_message::current_message`'s docs for why picking the
/// message to show is driven by [`MessageState`]/[`MessagePriority`] alone today.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayedMessage {
    /// This message's identifier.
    pub id: DisplayMessageId,
    /// How this message competes with others for the driver's attention.
    pub priority: MessagePriority,
    /// The condition under which to show this message. `None` means "unconditionally", matching
    /// OCPP's optional `state` (shown regardless of [`MessageState`]).
    pub state: Option<MessageState>,
    /// This message's content.
    pub message: MessageContent,
    /// The transaction this message is scoped to, if any. OCPP requires such a message to be
    /// removed once that transaction ends - **not yet enforced by this crate** (a known gap; see
    /// `docs/PRODUCTION-ROADMAP.md` B6). [`DisplayMessageStore::clear_for_transaction`] exists for
    /// whichever transaction-ending code path is wired to call it.
    pub transaction_id: Option<TransactionId>,
}

/// Default maximum number of [`DisplayedMessage`]s the store holds (see
/// [`crate::state::StateLimits::max_display_messages`]).
///
/// 50 covers realistic use (a handful of standing messages plus one per simultaneously-active
/// transaction on a small-to-mid-size charge point) while keeping the store to a few KB - a
/// message's content is capped at 1024 bytes on the wire (OCPP `MessageContentType.content`), so
/// 50 of them bounds this comfortably. A site that legitimately wants more should raise it via
/// [`crate::state::StateLimits::with_max_display_messages`] rather than have `SetDisplayMessage`
/// refused - see `docs/PRODUCTION-ROADMAP.md` §9.2 (G2.2) for why the bound exists at all.
pub const DEFAULT_MAX_DISPLAY_MESSAGES: usize = 50;

/// The charge point's display message store (OCPP `SetDisplayMessage`/`ClearDisplayMessage`),
/// bounded at construction by `max_messages` - mirrors [`crate::state::LocalAuthorizationList`]'s
/// shape and bounding rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayMessageStore {
    messages: Vec<DisplayedMessage>,
    max_messages: usize,
}

impl DisplayMessageStore {
    /// An empty store holding at most [`DEFAULT_MAX_DISPLAY_MESSAGES`] messages.
    pub fn new() -> Self {
        Self::with_max_messages(DEFAULT_MAX_DISPLAY_MESSAGES)
    }

    /// An empty store holding at most `max_messages` messages (clamped to at least 1 - a store
    /// that can hold nothing would drop every message sent to it, indistinguishable from not
    /// supporting display messages at all; integrators who want that should leave
    /// [`crate::hardware::Capabilities::has_display`] unset instead).
    pub fn with_max_messages(max_messages: usize) -> Self {
        Self {
            messages: Vec::new(),
            max_messages: max_messages.max(1),
        }
    }

    /// The most messages this store may hold.
    pub fn max_messages(&self) -> usize {
        self.max_messages
    }

    /// How many messages are currently stored.
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Whether the store currently holds no messages.
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Every stored message, in no particular order.
    pub fn iter(&self) -> impl Iterator<Item = &DisplayedMessage> {
        self.messages.iter()
    }

    /// The stored message with `id`, if any.
    pub fn get(&self, id: DisplayMessageId) -> Option<&DisplayedMessage> {
        self.messages.iter().find(|message| message.id == id)
    }

    /// Installs `message`, replacing any existing message with the same id. Returns `false` (and
    /// leaves the store unchanged) only when `message.id` is genuinely new and the store is
    /// already at [`Self::max_messages`] - replacing an already-stored id always succeeds, even at
    /// the bound, the same rule [`crate::state::ChargingProfileStore::install`] follows: a CSMS
    /// updating a message it already sees as installed must not be refused by the very bound that
    /// message counts against.
    pub fn set(&mut self, message: DisplayedMessage) -> bool {
        if let Some(existing) = self.messages.iter_mut().find(|m| m.id == message.id) {
            *existing = message;
            return true;
        }
        if self.messages.len() >= self.max_messages {
            return false;
        }
        self.messages.push(message);
        true
    }

    /// Removes the message with `id`. Returns whether one was found.
    pub fn clear(&mut self, id: DisplayMessageId) -> bool {
        let before = self.messages.len();
        self.messages.retain(|message| message.id != id);
        self.messages.len() != before
    }

    /// Removes every message scoped to `transaction_id` (see [`DisplayedMessage::transaction_id`]).
    /// Returns how many were removed.
    pub fn clear_for_transaction(&mut self, transaction_id: TransactionId) -> usize {
        let before = self.messages.len();
        self.messages
            .retain(|message| message.transaction_id != Some(transaction_id));
        before - self.messages.len()
    }
}

impl Default for DisplayMessageStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: i64, priority: MessagePriority) -> DisplayedMessage {
        DisplayedMessage {
            id: DisplayMessageId(id),
            priority,
            state: None,
            message: MessageContent {
                content: "hello".into(),
                format: MessageFormat::Ascii,
                language: None,
            },
            transaction_id: None,
        }
    }

    #[test]
    fn a_fresh_store_uses_the_default_maximum() {
        let store = DisplayMessageStore::new();

        assert_eq!(store.max_messages(), DEFAULT_MAX_DISPLAY_MESSAGES);
        assert!(store.is_empty());
    }

    #[test]
    fn a_maximum_of_zero_is_clamped_to_one() {
        let store = DisplayMessageStore::with_max_messages(0);

        assert_eq!(store.max_messages(), 1);
    }

    #[test]
    fn setting_a_new_message_within_the_maximum_succeeds() {
        let mut store = DisplayMessageStore::with_max_messages(2);

        assert!(store.set(message(1, MessagePriority::NormalCycle)));
        assert_eq!(store.len(), 1);
        assert!(store.get(DisplayMessageId(1)).is_some());
    }

    #[test]
    fn setting_beyond_the_maximum_is_refused() {
        let mut store = DisplayMessageStore::with_max_messages(1);
        store.set(message(1, MessagePriority::NormalCycle));

        assert!(!store.set(message(2, MessagePriority::NormalCycle)));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn replacing_an_existing_id_succeeds_even_at_the_maximum() {
        let mut store = DisplayMessageStore::with_max_messages(1);
        store.set(message(1, MessagePriority::NormalCycle));

        assert!(store.set(message(1, MessagePriority::AlwaysFront)));
        assert_eq!(store.len(), 1);
        assert_eq!(
            store.get(DisplayMessageId(1)).unwrap().priority,
            MessagePriority::AlwaysFront
        );
    }

    #[test]
    fn clearing_a_known_message_removes_it() {
        let mut store = DisplayMessageStore::with_max_messages(2);
        store.set(message(1, MessagePriority::NormalCycle));

        assert!(store.clear(DisplayMessageId(1)));
        assert!(store.is_empty());
    }

    #[test]
    fn clearing_an_unknown_message_reports_nothing_was_found() {
        let mut store = DisplayMessageStore::with_max_messages(2);

        assert!(!store.clear(DisplayMessageId(1)));
    }

    #[test]
    fn clearing_for_a_transaction_only_removes_scoped_messages() {
        let mut store = DisplayMessageStore::with_max_messages(4);
        store.set(message(1, MessagePriority::NormalCycle));
        let mut scoped = message(2, MessagePriority::NormalCycle);
        scoped.transaction_id = Some(TransactionId(7));
        store.set(scoped);

        let removed = store.clear_for_transaction(TransactionId(7));

        assert_eq!(removed, 1);
        assert_eq!(store.len(), 1);
        assert!(store.get(DisplayMessageId(1)).is_some());
    }

    #[test]
    fn priority_ordering_places_always_front_highest() {
        assert!(MessagePriority::AlwaysFront > MessagePriority::InFront);
        assert!(MessagePriority::InFront > MessagePriority::NormalCycle);
    }
}
