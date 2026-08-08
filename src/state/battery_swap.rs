//! State for the OCPP 2.1 Battery Swap functional block (`docs/PRODUCTION-ROADMAP.md` B8.3):
//! `RequestBatterySwap` (CSMS-initiated) and `BatterySwap` (charge-point-reported), for
//! battery-swap station hardware. See [`crate::battery_swap`] for the handlers built on this.
//!
//! # What the wire actually carries
//!
//! `BatterySwapRequest.requestId` correlates a swap's `BatteryIn`/`BatteryOut`/
//! `BatteryOutTimeout` events with each other and, when the CSMS proactively asked for the swap
//! first, with that `RequestBatterySwapRequest.requestId`. There is no dedicated "swap in
//! progress" status on the wire at all - a swap's lifecycle *is* the sequence of `BatterySwap`
//! events reported under one `requestId`. What this crate tracks as state, then, is only the half
//! that genuinely needs to persist between two separate CALLs: a `RequestBatterySwap` this charge
//! point has accepted but not yet seen a correlated `BatterySwap` event for
//! ([`PendingBatterySwap`], held in [`BatterySwapStore`]). A swap a driver starts without the
//! CSMS ever asking first - equally valid per spec, since `requestId` on `BatterySwap` is
//! required regardless - never enters this store; it is reported and done.

use alloc::string::String;
use alloc::vec::Vec;
use chrono::{DateTime, Utc};

use crate::state::IdToken;

/// Correlates a `RequestBatterySwapRequest`/`BatterySwapRequest` exchange (OCPP `requestId`) -
/// assigned by whichever side initiates: the CSMS for a proactive `RequestBatterySwap`, or this
/// charge point itself when hardware reports a swap the CSMS never asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BatterySwapRequestId(pub i64);

/// Which battery-swap lifecycle event occurred (OCPP `BatterySwapEventEnum`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatterySwapEventKind {
    /// A battery was inserted into a slot.
    BatteryIn,
    /// A battery was removed from a slot.
    BatteryOut,
    /// A battery that was removed was not reinserted/replaced within the station's own timeout -
    /// the swap did not complete normally.
    BatteryOutTimeout,
}

/// One battery involved in a swap event (OCPP `BatteryData`).
///
/// `state_of_charge`/`state_of_health` are OCPP `f64` percentages (0-100), stored here as
/// already-formatted decimal strings rather than raw floats - mirrors
/// [`crate::state::TriggeredMonitor::actual_value`] - so this type (and the
/// [`crate::state::ChargePointEffect::BatterySwapEventOccurred`] variant that carries it) can
/// derive `Eq`, which `f64` cannot. [`crate::battery_swap::report_battery_swap_event`] is the one
/// place that formats them from the caller's `f64`; the wire adapter parses the string back only
/// to place it in the (also `f64`) wire field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatteryData {
    /// The slot (this charge point's EVSE index) the battery was inserted into or removed from.
    pub evse_id: usize,
    /// The battery's serial number.
    pub serial_number: String,
    /// State of charge, 0-100%, formatted to two decimal places - see the struct docs.
    pub state_of_charge: String,
    /// State of health, 0-100%, formatted to two decimal places - see the struct docs.
    pub state_of_health: String,
    /// The battery's production date, if known.
    pub production_date: Option<DateTime<Utc>>,
    /// Vendor-specific info in an undefined format, if the hardware supplies any.
    pub vendor_info: Option<String>,
}

impl BatteryData {
    /// Builds a [`BatteryData`] from the caller's raw values, formatting `state_of_charge`/
    /// `state_of_health` to two decimal places - see the struct docs for why they're stored as
    /// strings rather than `f64`. Values are otherwise passed through unvalidated: OCPP defines
    /// `soC`/`soH` as plain percentages, and this crate trusts the hardware binding's own
    /// reading rather than second-guessing it.
    pub fn new(
        evse_id: usize,
        serial_number: impl Into<String>,
        state_of_charge: f64,
        state_of_health: f64,
        production_date: Option<DateTime<Utc>>,
        vendor_info: Option<String>,
    ) -> Self {
        Self {
            evse_id,
            serial_number: serial_number.into(),
            state_of_charge: alloc::format!("{state_of_charge:.2}"),
            state_of_health: alloc::format!("{state_of_health:.2}"),
            production_date,
            vendor_info,
        }
    }
}

/// One reported battery-swap event (OCPP `BatterySwap`), as raised by hardware via
/// [`crate::battery_swap::report_battery_swap_event`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatterySwapEvent {
    /// Correlates this event with any sibling `BatteryIn`/`BatteryOut`/`BatteryOutTimeout`
    /// events, and with the `RequestBatterySwap` that triggered this swap, if any.
    pub request_id: BatterySwapRequestId,
    /// Which lifecycle event this is.
    pub event_type: BatterySwapEventKind,
    /// The driver/operator undergoing the swap.
    pub id_token: IdToken,
    /// Every battery this event concerns. OCPP requires at least one.
    pub battery_data: Vec<BatteryData>,
}

/// A `RequestBatterySwap` this charge point has accepted but not yet correlated with a reported
/// [`BatterySwapEvent`] - see this module's docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingBatterySwap {
    /// Correlates this pending request with the [`BatterySwapEvent`] that will resolve it.
    pub request_id: BatterySwapRequestId,
    /// The driver/operator the CSMS asked to prepare a swap for.
    pub id_token: IdToken,
}

/// Default maximum number of [`PendingBatterySwap`]s [`BatterySwapStore`] holds (see
/// [`crate::state::StateLimits::max_pending_battery_swaps`]).
///
/// A battery-swap station has a small, physical number of bays a driver can be mid-request for at
/// once; 8 covers even a large station's simultaneous CSMS-initiated requests while keeping the
/// store to a few hundred bytes. A site that legitimately wants more should raise it via
/// [`crate::state::StateLimits::with_max_pending_battery_swaps`] rather than have
/// `RequestBatterySwap` refused.
pub const DEFAULT_MAX_PENDING_BATTERY_SWAPS: usize = 8;

/// The charge point's outstanding `RequestBatterySwap` store, bounded at construction by
/// `max_pending` - mirrors [`crate::state::DisplayMessageStore`]'s shape and bounding rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatterySwapStore {
    pending: Vec<PendingBatterySwap>,
    max_pending: usize,
}

impl BatterySwapStore {
    /// An empty store holding at most [`DEFAULT_MAX_PENDING_BATTERY_SWAPS`] requests.
    pub fn new() -> Self {
        Self::with_max_pending(DEFAULT_MAX_PENDING_BATTERY_SWAPS)
    }

    /// An empty store holding at most `max_pending` requests (clamped to at least 1 - a store
    /// that can hold nothing would refuse every `RequestBatterySwap`, indistinguishable from not
    /// supporting the block at all; integrators who want that should leave
    /// [`crate::hardware::Capabilities::battery_swap`] unset instead).
    pub fn with_max_pending(max_pending: usize) -> Self {
        Self {
            pending: Vec::new(),
            max_pending: max_pending.max(1),
        }
    }

    /// The most requests this store may hold at once.
    pub fn max_pending(&self) -> usize {
        self.max_pending
    }

    /// How many requests are currently pending.
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// Whether the store currently holds no pending requests.
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Every pending request, in no particular order.
    pub fn iter(&self) -> impl Iterator<Item = &PendingBatterySwap> {
        self.pending.iter()
    }

    /// The pending request correlated by `request_id`, if any.
    pub fn get(&self, request_id: BatterySwapRequestId) -> Option<&PendingBatterySwap> {
        self.pending.iter().find(|p| p.request_id == request_id)
    }

    /// Records `pending`, replacing any existing entry with the same `request_id`. Returns
    /// `false` (leaving the store unchanged) only when `request_id` is genuinely new and the
    /// store is already at [`Self::max_pending`] - mirrors
    /// [`crate::state::DisplayMessageStore::set`]'s replace-always-succeeds rule.
    pub fn insert(&mut self, pending: PendingBatterySwap) -> bool {
        if let Some(existing) = self
            .pending
            .iter_mut()
            .find(|p| p.request_id == pending.request_id)
        {
            *existing = pending;
            return true;
        }
        if self.pending.len() >= self.max_pending {
            return false;
        }
        self.pending.push(pending);
        true
    }

    /// Removes and returns the pending request correlated by `request_id`, if any - called once a
    /// [`BatterySwapEvent`] with a matching `request_id` is reported.
    pub fn remove(&mut self, request_id: BatterySwapRequestId) -> Option<PendingBatterySwap> {
        let index = self
            .pending
            .iter()
            .position(|p| p.request_id == request_id)?;
        Some(self.pending.remove(index))
    }
}

impl Default for BatterySwapStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::IdTokenKind;

    fn token() -> IdToken {
        IdToken {
            value: "04A224B2".into(),
            kind: IdTokenKind::ISO14443,
        }
    }

    fn pending(id: i64) -> PendingBatterySwap {
        PendingBatterySwap {
            request_id: BatterySwapRequestId(id),
            id_token: token(),
        }
    }

    #[test]
    fn a_fresh_store_uses_the_default_maximum() {
        let store = BatterySwapStore::new();

        assert_eq!(store.max_pending(), DEFAULT_MAX_PENDING_BATTERY_SWAPS);
        assert!(store.is_empty());
    }

    #[test]
    fn a_maximum_of_zero_is_clamped_to_one() {
        assert_eq!(BatterySwapStore::with_max_pending(0).max_pending(), 1);
    }

    #[test]
    fn inserting_within_the_maximum_succeeds() {
        let mut store = BatterySwapStore::with_max_pending(2);

        assert!(store.insert(pending(1)));
        assert_eq!(store.len(), 1);
        assert!(store.get(BatterySwapRequestId(1)).is_some());
    }

    #[test]
    fn inserting_beyond_the_maximum_is_refused() {
        let mut store = BatterySwapStore::with_max_pending(1);
        store.insert(pending(1));

        assert!(!store.insert(pending(2)));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn replacing_an_existing_id_succeeds_even_at_the_maximum() {
        let mut store = BatterySwapStore::with_max_pending(1);
        store.insert(pending(1));

        assert!(store.insert(pending(1)));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn removing_a_known_request_returns_it() {
        let mut store = BatterySwapStore::with_max_pending(2);
        store.insert(pending(7));

        let removed = store.remove(BatterySwapRequestId(7));

        assert!(removed.is_some());
        assert!(store.is_empty());
    }

    #[test]
    fn removing_an_unknown_request_reports_nothing_was_found() {
        let mut store = BatterySwapStore::with_max_pending(2);

        assert!(store.remove(BatterySwapRequestId(1)).is_none());
    }

    #[test]
    fn battery_data_new_formats_the_percentages_to_two_decimal_places() {
        let data = BatteryData::new(0, "SN123", 87.5, 99.0, None, None);

        assert_eq!(data.state_of_charge, "87.50");
        assert_eq!(data.state_of_health, "99.00");
    }
}
