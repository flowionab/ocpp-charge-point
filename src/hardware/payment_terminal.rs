//! The payment-terminal hardware hook (OCPP 2.1 Payment functional block -
//! `docs/PRODUCTION-ROADMAP.md` B7.2): the identity facts of a physically attached payment
//! terminal, used to fill in `PaymentCtrlr`'s device-model variables (see [`crate::payment`]).
//!
//! A payment terminal is niche hardware - most charge points have none - so this trait, like the
//! rest of the block, sits behind the `payment` Cargo feature and the `payment` runtime
//! capability (see [`crate::hardware::Capabilities::payment`]). A normal charge point leaves both
//! off and compiles none of it in; one with an attached terminal supplies a real implementation.
//!
//! Unlike [`crate::hardware::Display`] or [`crate::hardware::BatterySwapStation`], nothing in the
//! Payment block is CSMS-initiated - `NotifySettlement`, `NotifyWebPaymentStarted` and
//! `VatNumberValidation` are all sent *by* this charge point (see [`crate::payment`]'s module
//! docs), so there is no inbound command for this trait to answer. Its one job is reporting the
//! terminal's own static identity, so `PaymentCtrlr`'s required `VendorName`/`Model`/
//! `SerialNumber`/`FirmwareVersion`/`TerminalID`/`PaymentServiceProvider` variables can be
//! genuine instead of the empty placeholder [`crate::device_model::capability_gate_events`]
//! registers for every capability-gated component it knows nothing else about.

use alloc::boxed::Box;
use alloc::string::String;

/// The static identity of a payment terminal, as reported by [`PaymentTerminal::info`] - fills in
/// `PaymentCtrlr`'s identity variables (OCPP 2.1 Appendix `dm_components_vars.csv`).
///
/// Deliberately just data, not a live status query: unlike a settlement or a web-payment session,
/// none of these fields are expected to change while the terminal stays plugged in, so there is no
/// need for the crate to re-poll it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaymentTerminalInfo {
    /// Manufacturer of the payment terminal (`PaymentCtrlr.VendorName`).
    pub vendor_name: String,
    /// Model of the payment terminal (`PaymentCtrlr.Model`).
    pub model: String,
    /// Payment terminal serial number (`PaymentCtrlr.SerialNumber`).
    pub serial_number: String,
    /// Payment terminal firmware version (`PaymentCtrlr.FirmwareVersion`).
    pub firmware_version: String,
    /// Terminal ID of the payment terminal (`PaymentCtrlr.TerminalID`).
    pub terminal_id: String,
    /// The payment service provider the terminal is using (`PaymentCtrlr.PaymentServiceProvider`).
    pub payment_service_provider: String,
}

/// Reports the identity of a physically attached payment terminal - implemented by the integrator
/// against their actual hardware/SDK. See the module docs for why this is the whole trait: every
/// message this block actually sends (settlement outcomes, web-payment starts, VAT validation
/// requests) carries its own data from whoever triggers it, rather than needing this crate to pull
/// anything further from the terminal.
#[async_trait::async_trait]
pub trait PaymentTerminal {
    /// The error type returned by a failed identity read.
    type Error: core::error::Error + Send + Sync + 'static;

    /// Reads the terminal's static identity - called once, at
    /// [`crate::builder::ChargePointBuilder::payment`] registration time.
    async fn info(&self) -> Result<PaymentTerminalInfo, Self::Error>;
}

#[async_trait::async_trait]
impl<T: PaymentTerminal + Send + Sync + ?Sized> PaymentTerminal for alloc::sync::Arc<T> {
    type Error = T::Error;

    async fn info(&self) -> Result<PaymentTerminalInfo, Self::Error> {
        (**self).info().await
    }
}

/// A [`PaymentTerminal`] for charge points with no payment terminal attached, mirroring
/// [`crate::hardware::NoDisplay`]/[`crate::hardware::NoBatterySwapStation`].
#[derive(Debug, Clone, Copy, Default)]
pub struct NoPaymentTerminal;

/// The error [`NoPaymentTerminal::info`] always returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoPaymentTerminalError;

impl core::fmt::Display for NoPaymentTerminalError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "this charge point has no payment terminal, so it cannot report one's identity"
        )
    }
}

impl core::error::Error for NoPaymentTerminalError {}

#[async_trait::async_trait]
impl PaymentTerminal for NoPaymentTerminal {
    type Error = NoPaymentTerminalError;

    async fn info(&self) -> Result<PaymentTerminalInfo, Self::Error> {
        Err(NoPaymentTerminalError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_charge_point_with_no_payment_terminal_fails_rather_than_pretending() {
        let terminal = NoPaymentTerminal;

        let result = terminal.info().await;

        assert!(result.is_err());
    }
}
