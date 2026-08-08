//! The battery-swap hardware hook (OCPP 2.1 Battery Swap functional block -
//! `docs/PRODUCTION-ROADMAP.md` B8.3): reacting, on real swap-station hardware, to a
//! CSMS-initiated `RequestBatterySwap`.
//!
//! This is niche hardware - a battery-swap station rather than a conventional charge point - so
//! this trait, like the rest of the block ([`crate::battery_swap`]), is behind the
//! `battery-swap` Cargo feature and the `battery_swap` runtime capability (see
//! [`crate::hardware::Capabilities::battery_swap`]). A normal charge point leaves both off and
//! compiles none of it in; a battery-swap station supplies a real implementation.

use crate::state::{BatterySwapRequestId, IdToken};
use alloc::boxed::Box;
use alloc::string::String;

/// Prepares physical swap hardware for a CSMS-initiated swap request - implemented by the
/// integrator against their actual battery-swap station (e.g. unlocking the relevant battery
/// slot(s), or lighting an indicator for the driver named by `id_token`).
///
/// Reporting the swap itself back to the CSMS (`BatterySwap`, as batteries actually go in or out)
/// is a separate, narrower concern: hardware calls
/// [`crate::battery_swap::report_battery_swap_event`] directly as those events happen, rather
/// than through this trait, since a driver-initiated swap the CSMS never asked for is equally
/// valid per spec and has no `RequestBatterySwap` to prepare for in the first place.
#[async_trait::async_trait]
pub trait BatterySwapStation {
    /// The error type returned by a failed preparation.
    type Error: core::error::Error + Send + Sync + 'static;

    /// Prepares this station for a swap on behalf of `id_token`, correlated by `request_id` -
    /// called by [`crate::battery_swap::handle_request_battery_swap`] once a `RequestBatterySwap`
    /// has been accepted, before that CALL is answered.
    async fn prepare_swap(
        &self,
        request_id: BatterySwapRequestId,
        id_token: &IdToken,
    ) -> Result<(), Self::Error>;
}

#[async_trait::async_trait]
impl<T: BatterySwapStation + Send + Sync + ?Sized> BatterySwapStation for alloc::sync::Arc<T> {
    type Error = T::Error;

    async fn prepare_swap(
        &self,
        request_id: BatterySwapRequestId,
        id_token: &IdToken,
    ) -> Result<(), Self::Error> {
        (**self).prepare_swap(request_id, id_token).await
    }
}

/// A [`BatterySwapStation`] for charge points with no swap hardware, mirroring
/// [`crate::hardware::NoDisplay`].
#[derive(Debug, Clone, Copy, Default)]
pub struct NoBatterySwapStation;

/// The error [`NoBatterySwapStation::prepare_swap`] always returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoBatterySwapStationError {
    /// What was attempted, for the log.
    pub attempted: String,
}

impl core::fmt::Display for NoBatterySwapStationError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "this charge point has no battery-swap hardware, so it cannot {}",
            self.attempted
        )
    }
}

impl core::error::Error for NoBatterySwapStationError {}

#[async_trait::async_trait]
impl BatterySwapStation for NoBatterySwapStation {
    type Error = NoBatterySwapStationError;

    async fn prepare_swap(
        &self,
        _request_id: BatterySwapRequestId,
        _id_token: &IdToken,
    ) -> Result<(), Self::Error> {
        Err(NoBatterySwapStationError {
            attempted: "prepare a battery swap".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::IdTokenKind;

    #[tokio::test]
    async fn a_charge_point_with_no_battery_swap_hardware_fails_rather_than_pretending() {
        let station = NoBatterySwapStation;
        let id_token = IdToken {
            value: "04A224B2".into(),
            kind: IdTokenKind::ISO14443,
        };

        let result = station
            .prepare_swap(BatterySwapRequestId(1), &id_token)
            .await;

        assert!(result.is_err());
    }
}
