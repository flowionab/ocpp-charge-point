//! Payment functional block (`docs/PRODUCTION-ROADMAP.md` B7.2): `NotifySettlement`,
//! `NotifyWebPaymentStarted` and `VatNumberValidation`, all sent *by* this charge point.
//!
//! **2.1 only** - none of these three messages exist before 2.1, so there is no 1.6J/2.0.1
//! projection to write. See [`crate::hardware::PaymentTerminal`] for the one hardware hook this
//! block needs (just terminal identity - see that trait's docs for why), and
//! `crate::device_model`'s `CAPABILITY_GATED_VARIABLES` for `PaymentCtrlr`'s 22 required
//! device-model variables.
//!
//! # All three messages are outbound
//!
//! Unlike every other capability-gated block in this crate (`ReserveNow`, `SetDefaultTariff`,
//! `RequestBatterySwap`, ...), nothing here is CSMS-initiated: the charge point sends
//! `NotifySettlement` when a payment terminal settles (or fails to settle) a payment,
//! `NotifyWebPaymentStarted` when a driver begins a web/QR payment flow at a connector, and
//! `VatNumberValidation` to ask the CSMS to check a VAT number for invoicing. All three still get
//! a real CALLRESULT back (`NotifySettlement`/`VatNumberValidation` carry receipt/validation data
//! the caller may need), so [`crate::payment::PaymentNotifier`]'s methods are request/response,
//! not fire-and-forget.
//!
//! Because there is no inbound handler, this block has no [`crate::refusal`] row: that module
//! covers a handler that is *registered* but must refuse in the wire-correct shape when its
//! capability is runtime-absent, which does not apply to a message this crate itself chooses
//! whether to originate. The gate here is simpler and applied directly, in
//! [`crate::payment::report_settlement`]/[`crate::payment::report_web_payment_started`]/
//! [`crate::payment::validate_vat_number`]: a runtime-absent `payment` capability fails the call
//! before it reaches a [`crate::payment::PaymentNotifier`] at all.
//!
//! # Money, and what does *not* get logged
//!
//! `settlement_amount` is stored as an already-formatted decimal string rather than a raw `f64`,
//! mirroring [`crate::state::BatteryData`]'s `state_of_charge`/`state_of_health` convention (see
//! its docs) - the same `f64`-can't-derive-`Eq` reason B2.8/B8.3 hit.
//!
//! This module never logs a settlement identifier (`psp_ref`, `receipt_id`, `transaction_id`), a
//! VAT number, or merchant/company address details: payment data belongs in the OCPP messages
//! that carry it to the CSMS, not in this crate's `tracing` output. Failure logging here names
//! only non-sensitive correlators (e.g. an EVSE id).

use crate::actor::ChargePointActor;
use alloc::boxed::Box;
use alloc::string::String;

/// The outcome of a payment settlement (OCPP `PaymentStatusEnum`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettlementStatus {
    /// The payment was successfully settled.
    Settled,
    /// The payment attempt was canceled.
    Canceled,
    /// The payment was rejected.
    Rejected,
    /// The settlement attempt failed.
    Failed,
}

/// A merchant/company postal address (OCPP `Address`), attached to a settlement's receipt or a
/// VAT validation's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MerchantAddress {
    /// Name of the person/company.
    pub name: String,
    /// Address line 1.
    pub address1: String,
    /// Address line 2, if any.
    pub address2: Option<String>,
    /// City.
    pub city: String,
    /// Postal code, if known.
    pub postal_code: Option<String>,
    /// Country name.
    pub country: String,
}

/// A settlement outcome reported by payment-terminal hardware via [`report_settlement`] (OCPP
/// `NotifySettlement`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettlementEvent {
    /// The payment reference from the payment terminal, used as the `idToken` value.
    pub psp_ref: String,
    /// A receipt id, if the payment terminal (rather than the CSMS) generated one.
    pub receipt_id: Option<String>,
    /// A receipt URL, if the payment terminal (rather than the CSMS) generated one.
    pub receipt_url: Option<String>,
    /// The amount that was settled, or attempted to be settled, formatted to two decimal places -
    /// see the module docs for why this is a `String` rather than an `f64`.
    pub settlement_amount: String,
    /// When the settlement was done.
    pub settlement_time: chrono::DateTime<chrono::Utc>,
    /// The outcome.
    pub status: SettlementStatus,
    /// Additional information from the payment terminal/process, if any.
    pub status_info: Option<String>,
    /// The transaction this settlement belongs to, if the payment was not canceled before an
    /// OCPP transaction started.
    pub transaction_id: Option<String>,
    /// The company to put on a company receipt, if requested.
    pub vat_company: Option<MerchantAddress>,
    /// The VAT number for a company receipt, if requested.
    pub vat_number: Option<String>,
}

impl SettlementEvent {
    /// Builds a [`SettlementEvent`], formatting `settlement_amount` to two decimal places - see
    /// the module docs for why this crate stores it as a `String` rather than a raw `f64`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        psp_ref: impl Into<String>,
        receipt_id: Option<String>,
        receipt_url: Option<String>,
        settlement_amount: f64,
        settlement_time: chrono::DateTime<chrono::Utc>,
        status: SettlementStatus,
        status_info: Option<String>,
        transaction_id: Option<String>,
        vat_company: Option<MerchantAddress>,
        vat_number: Option<String>,
    ) -> Self {
        Self {
            psp_ref: psp_ref.into(),
            receipt_id,
            receipt_url,
            settlement_amount: alloc::format!("{settlement_amount:.2}"),
            settlement_time,
            status,
            status_info,
            transaction_id,
            vat_company,
            vat_number,
        }
    }
}

/// The receipt information a CSMS may return from `NotifySettlement`, when the CSMS (rather than
/// the payment terminal) generates the receipt.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SettlementReceipt {
    /// The receipt id, if the CSMS generated one.
    pub receipt_id: Option<String>,
    /// The receipt URL, if the CSMS generated one - a charge point with a display can QR-encode
    /// this for the driver.
    pub receipt_url: Option<String>,
}

/// The outcome of a `VatNumberValidation` round trip (OCPP `GenericStatusEnum`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VatValidationStatus {
    /// The CSMS accepted the VAT number as valid.
    Accepted,
    /// The CSMS rejected the VAT number, or could not validate it.
    Rejected,
}

/// The full result of a `VatNumberValidation` round trip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VatValidationOutcome {
    /// Whether the CSMS considers the VAT number valid.
    pub status: VatValidationStatus,
    /// The company the VAT number resolves to, if the CSMS could determine one.
    pub company: Option<MerchantAddress>,
    /// Additional information from the CSMS/validation service, if any.
    pub status_info: Option<String>,
}

/// Why a [`report_settlement`]/[`report_web_payment_started`]/[`validate_vat_number`] call did
/// not reach the CSMS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaymentError<E> {
    /// The `payment` capability is runtime-absent, so this charge point has nothing to report to,
    /// or ask, a CSMS about.
    CapabilityAbsent,
    /// The [`PaymentNotifier`] call itself failed (e.g. a transport error).
    Notifier(E),
}

impl<E: core::fmt::Display> core::fmt::Display for PaymentError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            PaymentError::CapabilityAbsent => {
                write!(f, "this charge point has no payment capability")
            }
            PaymentError::Notifier(err) => write!(f, "payment notification failed: {err}"),
        }
    }
}

/// Sends this charge point's payment messages to the CSMS and returns whatever data each call's
/// CALLRESULT carries back. Implemented per protocol version (see the `ocpp_2_1` module) -
/// mirrors [`crate::authorization::Authorizer`]'s shape (a CS-initiated call awaiting a real
/// answer), not [`crate::battery_swap::BatterySwapNotifier`]'s (a fire-and-forget report), because
/// all three of these calls return data a caller may act on.
#[async_trait::async_trait]
pub trait PaymentNotifier {
    /// The error type returned if a call itself fails (e.g. a transport error) - distinct from a
    /// CSMS-side rejection, which [`validate_vat_number`] surfaces as
    /// [`VatValidationStatus::Rejected`] rather than an `Err`.
    type Error: core::fmt::Display;

    /// Reports a settlement outcome via `NotifySettlement`.
    async fn notify_settlement(
        &self,
        event: &SettlementEvent,
    ) -> Result<SettlementReceipt, Self::Error>;

    /// Reports that a web/QR payment flow has begun for `evse_id`, timing out after
    /// `timeout_secs` if no result of the flow is seen.
    async fn notify_web_payment_started(
        &self,
        evse_id: usize,
        timeout_secs: u32,
    ) -> Result<(), Self::Error>;

    /// Asks the CSMS to validate `vat_number`, optionally scoped to `evse_id`.
    async fn validate_vat_number(
        &self,
        vat_number: &str,
        evse_id: Option<usize>,
    ) -> Result<VatValidationOutcome, Self::Error>;
}

/// Reports a settlement outcome to the CSMS via `NotifySettlement` - the entry point payment
/// terminal hardware should call as soon as it knows the outcome of a settlement attempt.
///
/// Fails with [`PaymentError::CapabilityAbsent`] before `notifier` is ever called when the
/// `payment` capability is runtime-absent - see the module docs for why this is a direct check
/// rather than a [`crate::refusal`] row.
pub async fn report_settlement<N: PaymentNotifier>(
    actor: &ChargePointActor,
    notifier: &N,
    event: SettlementEvent,
) -> Result<SettlementReceipt, PaymentError<N::Error>> {
    if !actor.state().capabilities.payment {
        return Err(PaymentError::CapabilityAbsent);
    }
    notifier
        .notify_settlement(&event)
        .await
        .map_err(PaymentError::Notifier)
}

/// Reports to the CSMS via `NotifyWebPaymentStarted` that a web/QR payment flow has begun for
/// `evse_id`, timing out after `timeout_secs`.
///
/// Fails with [`PaymentError::CapabilityAbsent`] before `notifier` is ever called when the
/// `payment` capability is runtime-absent.
pub async fn report_web_payment_started<N: PaymentNotifier>(
    actor: &ChargePointActor,
    notifier: &N,
    evse_id: usize,
    timeout_secs: u32,
) -> Result<(), PaymentError<N::Error>> {
    if !actor.state().capabilities.payment {
        return Err(PaymentError::CapabilityAbsent);
    }
    notifier
        .notify_web_payment_started(evse_id, timeout_secs)
        .await
        .map_err(PaymentError::Notifier)
}

/// Asks the CSMS to validate `vat_number` via `VatNumberValidation`, optionally scoped to
/// `evse_id`, for invoicing.
///
/// Fails with [`PaymentError::CapabilityAbsent`] before `notifier` is ever called when the
/// `payment` capability is runtime-absent - a charge point with no payment integration has no
/// business asking a CSMS to validate a VAT number for it.
pub async fn validate_vat_number<N: PaymentNotifier>(
    actor: &ChargePointActor,
    notifier: &N,
    vat_number: &str,
    evse_id: Option<usize>,
) -> Result<VatValidationOutcome, PaymentError<N::Error>> {
    if !actor.state().capabilities.payment {
        return Err(PaymentError::CapabilityAbsent);
    }
    notifier
        .validate_vat_number(vat_number, evse_id)
        .await
        .map_err(PaymentError::Notifier)
}

#[cfg(feature = "ocpp_2_1")]
mod ocpp_2_1;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::TokioExecutor;
    use crate::hardware::Capabilities;
    use crate::state::ChargePointEvent;
    use core::convert::Infallible;

    fn settlement_event() -> SettlementEvent {
        SettlementEvent::new(
            "psp-ref-1",
            None,
            None,
            42.50,
            chrono::Utc::now(),
            SettlementStatus::Settled,
            None,
            Some("txn-1".into()),
            None,
            None,
        )
    }

    async fn spawn_with_payment<const N: usize>(evses: [usize; N]) -> ChargePointActor {
        let actor = ChargePointActor::spawn(evses, &TokioExecutor);
        actor
            .send(ChargePointEvent::CapabilitiesDeclared(Capabilities {
                payment: true,
                ..Default::default()
            }))
            .await
            .unwrap();
        actor
    }

    /// A [`PaymentNotifier`] that always succeeds, recording what it was called with.
    #[derive(Default)]
    struct RecordingNotifier {
        settlements: alloc::sync::Arc<std::sync::Mutex<alloc::vec::Vec<SettlementEvent>>>,
    }

    #[async_trait::async_trait]
    impl PaymentNotifier for RecordingNotifier {
        type Error = Infallible;

        async fn notify_settlement(
            &self,
            event: &SettlementEvent,
        ) -> Result<SettlementReceipt, Self::Error> {
            self.settlements.lock().unwrap().push(event.clone());
            Ok(SettlementReceipt::default())
        }

        async fn notify_web_payment_started(
            &self,
            _evse_id: usize,
            _timeout_secs: u32,
        ) -> Result<(), Self::Error> {
            Ok(())
        }

        async fn validate_vat_number(
            &self,
            _vat_number: &str,
            _evse_id: Option<usize>,
        ) -> Result<VatValidationOutcome, Self::Error> {
            Ok(VatValidationOutcome {
                status: VatValidationStatus::Accepted,
                company: None,
                status_info: None,
            })
        }
    }

    #[tokio::test]
    async fn an_absent_capability_refuses_settlement_reporting_without_touching_the_notifier() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let notifier = RecordingNotifier::default();

        let result = report_settlement(&actor, &notifier, settlement_event()).await;

        assert_eq!(result, Err(PaymentError::CapabilityAbsent));
        assert!(notifier.settlements.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_present_capability_reports_the_settlement_and_returns_its_receipt() {
        let actor = spawn_with_payment([1]).await;
        let notifier = RecordingNotifier::default();

        let result = report_settlement(&actor, &notifier, settlement_event()).await;

        assert_eq!(result, Ok(SettlementReceipt::default()));
        assert_eq!(notifier.settlements.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_absent_capability_refuses_web_payment_started_reporting() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let notifier = RecordingNotifier::default();

        let result = report_web_payment_started(&actor, &notifier, 0, 120).await;

        assert_eq!(result, Err(PaymentError::CapabilityAbsent));
    }

    #[tokio::test]
    async fn a_present_capability_reports_web_payment_started() {
        let actor = spawn_with_payment([1]).await;
        let notifier = RecordingNotifier::default();

        let result = report_web_payment_started(&actor, &notifier, 0, 120).await;

        assert_eq!(result, Ok(()));
    }

    #[tokio::test]
    async fn an_absent_capability_refuses_vat_number_validation() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let notifier = RecordingNotifier::default();

        let result = validate_vat_number(&actor, &notifier, "SE556677889901", None).await;

        assert_eq!(result, Err(PaymentError::CapabilityAbsent));
    }

    #[tokio::test]
    async fn a_present_capability_validates_the_vat_number() {
        let actor = spawn_with_payment([1]).await;
        let notifier = RecordingNotifier::default();

        let result = validate_vat_number(&actor, &notifier, "SE556677889901", Some(0)).await;

        assert_eq!(
            result,
            Ok(VatValidationOutcome {
                status: VatValidationStatus::Accepted,
                company: None,
                status_info: None,
            })
        );
    }
}
