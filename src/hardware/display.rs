//! The display hardware hook (OCPP Display Message functional block - `docs/ROADMAP.md` §15,
//! `docs/PRODUCTION-ROADMAP.md` B6): rendering whatever [`crate::display_message::current_message`]
//! decides should currently be shown to the driver.
//!
//! A charge point with no screen is a normal, supported configuration - not every charge point
//! has one, and OCPP's Display Message block is entirely optional. Leaving
//! [`crate::hardware::Capabilities::has_display`] unset (or supplying [`NoDisplay`]) is how an
//! integrator says so honestly; see [`NoDisplay`]'s docs for why that means refusing rather than
//! silently accepting messages nothing will ever show.

use crate::state::{DisplayedMessage, MessageFormat};
use alloc::boxed::Box;
use alloc::string::String;

/// Renders whichever display message currently applies, per [`crate::display_message`]'s
/// derivation - implemented by the integrator against their actual screen/LED hardware.
///
/// # Error handling
///
/// Rendering is fallible, like every hardware call this crate makes (`CLAUDE.md`): a screen may
/// be unresponsive, an LED driver may not acknowledge. A failure is logged by
/// [`crate::display_message::run_display_updates`] and the derivation moves on - a display that
/// cannot currently render is not a charge-point-level fault (charging continues regardless), so
/// this does not drive any connector/EVSE state machine into `Faulted`.
#[async_trait::async_trait]
pub trait Display {
    /// The error type returned by a failed render.
    type Error: core::error::Error + Send + Sync + 'static;

    /// Renders `message` on the physical display, or clears whatever is currently shown when
    /// `message` is `None` - called whenever
    /// [`crate::display_message::current_message`]'s derivation actually changes.
    async fn show(&self, message: Option<&DisplayedMessage>) -> Result<(), Self::Error>;

    /// The message formats this hardware can actually render (OCPP `MessageFormatEnum`), reported
    /// honestly: used both to advertise `DisplayMessageCtrlr.SupportedFormats` and to refuse a
    /// `SetDisplayMessage` naming a format outside this list with
    /// `NotSupportedMessageFormat` (see
    /// [`crate::display_message::handle_set_display_message`]) rather than accepting it and
    /// failing silently on the driver's own screen.
    fn supported_formats(&self) -> &[MessageFormat];
}

#[async_trait::async_trait]
impl<T: Display + Send + Sync + ?Sized> Display for alloc::sync::Arc<T> {
    type Error = T::Error;

    async fn show(&self, message: Option<&DisplayedMessage>) -> Result<(), Self::Error> {
        (**self).show(message).await
    }

    fn supported_formats(&self) -> &[MessageFormat] {
        (**self).supported_formats()
    }
}

/// A [`Display`] for charge points with no physical display, mirroring
/// [`crate::hardware::NoFileTransfer`]/[`crate::hardware::NoStorage`].
///
/// Every render fails with [`NoDisplayError`] rather than succeeding silently: reporting success
/// for a message nothing ever shows would let a CSMS believe a driver has been told something
/// they haven't, which is worse than an honest failure the caller logs and moves past (see
/// [`Display`]'s docs on error handling). [`supported_formats`](Display::supported_formats) is
/// empty, which alone is enough to make [`crate::display_message::handle_set_display_message`]
/// refuse every message with `NotSupportedMessageFormat` before `show` is ever called - an
/// integrator with no screen should also leave
/// [`Capabilities::has_display`](crate::hardware::Capabilities::has_display) `false` so the
/// handlers are never registered and the CSMS is told up front.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoDisplay;

/// The error [`NoDisplay::show`] always returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoDisplayError {
    /// What was attempted, for the log.
    pub attempted: String,
}

impl core::fmt::Display for NoDisplayError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "this charge point has no display, so it cannot {}",
            self.attempted
        )
    }
}

impl core::error::Error for NoDisplayError {}

#[async_trait::async_trait]
impl Display for NoDisplay {
    type Error = NoDisplayError;

    async fn show(&self, message: Option<&DisplayedMessage>) -> Result<(), Self::Error> {
        Err(NoDisplayError {
            attempted: if message.is_some() {
                "render a display message".into()
            } else {
                "clear the display".into()
            },
        })
    }

    fn supported_formats(&self) -> &[MessageFormat] {
        &[]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{DisplayMessageId, MessageContent, MessagePriority};

    fn message() -> DisplayedMessage {
        DisplayedMessage {
            id: DisplayMessageId(1),
            priority: MessagePriority::NormalCycle,
            state: None,
            message: MessageContent {
                content: "hi".into(),
                format: MessageFormat::Ascii,
                language: None,
            },
            transaction_id: None,
        }
    }

    #[tokio::test]
    async fn a_charge_point_with_no_display_fails_rather_than_pretending() {
        let display = NoDisplay;

        assert!(display.show(Some(&message())).await.is_err());
        assert!(display.show(None).await.is_err());
        assert!(display.supported_formats().is_empty());
    }
}
