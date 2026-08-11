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
//! docs), so there is no inbound command for this trait to answer. Its job is reporting what the
//! terminal knows about itself, so `PaymentCtrlr`'s required variables can be genuine instead of
//! the empty placeholder [`crate::device_model::capability_gate_events`] registers for every
//! capability-gated component it knows nothing else about.
//!
//! That splits in two, along the line of what can change while the station runs:
//!
//! - [`PaymentTerminalInfo`], read once. A terminal's model and serial number do not change.
//! - [`PaymentTerminalStatus`], re-read on a schedule (CV2.11). Whether the terminal is reachable
//!   at all, whether it is reporting a fault, and which SIM is in it are exactly the facts a
//!   support engineer needs and cannot get any other way once the station is on a wall.

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

/// The merchant a terminal settles on behalf of - `PaymentCtrlr.Merchant`'s five instances
/// (`Id`, `TaxId`, `Name`, `Address`, `City`).
///
/// A deployment fact rather than a hardware one: it is configured into the terminal when the site
/// is commissioned, and changes when the site changes hands. That is why it lives on
/// [`PaymentTerminalStatus`] with the things that can move, not on [`PaymentTerminalInfo`] with
/// the things stamped on the case.
///
/// Every field defaults to empty, and empty is reported as empty. A merchant identifier appears on
/// a driver's card statement, so a plausible-looking invention here would be a receipt naming
/// somebody who was never paid.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MerchantIdentity {
    /// `PaymentCtrlr.Merchant[Id]` - the merchant's identifier with the payment provider.
    pub id: String,
    /// `PaymentCtrlr.Merchant[TaxId]` - the merchant's tax/VAT registration number.
    pub tax_id: String,
    /// `PaymentCtrlr.Merchant[Name]` - the trading name printed on a receipt.
    pub name: String,
    /// `PaymentCtrlr.Merchant[Address]` - the merchant's street address.
    pub address: String,
    /// `PaymentCtrlr.Merchant[City]` - the merchant's city.
    pub city: String,
}

/// What a payment terminal is doing *right now*, as reported by [`PaymentTerminal::status`]
/// (`docs/OCPP-2.1-COMPLIANCE-ROADMAP.md` CV2.11, OCPP use cases C18-C24).
///
/// # The default is a real answer, not a placeholder
///
/// [`Default`] is "not connected, no fault reported, nothing known" - which is precisely true of a
/// charge point whose terminal binding does not implement [`PaymentTerminal::status`], and true of
/// one whose terminal has been unplugged. A CSMS reading `Connected = false` on a station that
/// simply never told this crate otherwise is being told something correct: this firmware has no
/// evidence a terminal is there. Defaulting `Connected` to `true` would be the opposite - an
/// assertion about hardware nobody made.
///
/// `problem: false` alongside it is not a contradiction. `Problem` means "the terminal is
/// reporting a fault", and a terminal that has not been heard from is not reporting anything;
/// OCPP's `Connected` is the variable that carries "it is not there", and duplicating that into
/// `Problem` would raise an alarm on every station that never had a terminal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PaymentTerminalStatus {
    /// `PaymentCtrlr.Connected` - whether the charge point can currently reach the terminal.
    pub connected: bool,
    /// `PaymentCtrlr.Problem` - whether the terminal is reporting a fault of its own.
    pub problem: bool,
    /// `PaymentCtrlr.ICCID` - the identifier of the SIM in the terminal's modem, where it has one.
    pub iccid: String,
    /// `PaymentCtrlr.IMSI` - the subscriber identity of that SIM.
    pub imsi: String,
    /// `PaymentCtrlr.Merchant[*]` - who the terminal settles on behalf of.
    pub merchant: MerchantIdentity,
}

/// Reports what a physically attached payment terminal is and what it is doing - implemented by
/// the integrator against their actual hardware/SDK.
///
/// Every message this block *sends* (settlement outcomes, web-payment starts, VAT validation
/// requests) carries its own data from whoever triggers it, so this trait exists only to keep
/// `PaymentCtrlr`'s device-model variables true.
#[async_trait::async_trait]
pub trait PaymentTerminal {
    /// The error type returned by a failed read.
    type Error: core::error::Error + Send + Sync + 'static;

    /// Reads the terminal's static identity - called once, at
    /// [`crate::builder::ChargePointBuilder::payment`] registration time.
    async fn info(&self) -> Result<PaymentTerminalInfo, Self::Error>;

    /// Reads what the terminal is doing right now - its reachability, any fault it is reporting,
    /// its modem's SIM identifiers and the merchant it settles for (CV2.11).
    ///
    /// **Default-implemented**, returning [`PaymentTerminalStatus::default`], so adding it broke
    /// no existing binding - the same stance
    /// [`ChargePoint::electrical`](crate::hardware::ChargePoint::electrical) took (CV1.5). A
    /// binding that does not override it leaves `PaymentCtrlr` saying `Connected = false` and the
    /// rest empty, which is what this firmware actually knows.
    ///
    /// Called once at registration and then on whatever schedule
    /// [`ChargePointBuilder::payment_status_updates`](crate::builder::ChargePointBuilder::payment_status_updates)
    /// was given, so it must be cheap and must not block: answer from whatever the terminal SDK
    /// last reported rather than forcing a round trip to the terminal per call. Returning `Err`
    /// leaves the previously reported values in place - see that method's docs for why a
    /// momentarily unreadable terminal is not the same as an absent one.
    async fn status(&self) -> Result<PaymentTerminalStatus, Self::Error> {
        Ok(PaymentTerminalStatus::default())
    }
}

#[async_trait::async_trait]
impl<T: PaymentTerminal + Send + Sync + ?Sized> PaymentTerminal for alloc::sync::Arc<T> {
    type Error = T::Error;

    async fn info(&self) -> Result<PaymentTerminalInfo, Self::Error> {
        (**self).info().await
    }

    async fn status(&self) -> Result<PaymentTerminalStatus, Self::Error> {
        (**self).status().await
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

    /// A binding written before CV2.11 existed still compiles, and its station reports the honest
    /// "nothing is connected and this firmware was told nothing" - not an invented terminal.
    #[tokio::test]
    async fn a_binding_that_does_not_implement_status_reports_no_terminal() {
        struct IdentityOnlyTerminal;

        #[async_trait::async_trait]
        impl PaymentTerminal for IdentityOnlyTerminal {
            type Error = NoPaymentTerminalError;

            async fn info(&self) -> Result<PaymentTerminalInfo, Self::Error> {
                Err(NoPaymentTerminalError)
            }
        }

        let status = IdentityOnlyTerminal.status().await.expect("the default");

        assert_eq!(status, PaymentTerminalStatus::default());
        assert!(!status.connected);
        assert!(
            !status.problem,
            "an unheard-from terminal is not a faulty one - Connected is what says it is absent"
        );
        assert_eq!(status.merchant, MerchantIdentity::default());
    }
}
