use chrono::{DateTime, Utc};

use crate::state::IdToken;

/// Identifies a reservation (OCPP `ReserveNow.id`). Unlike [`crate::state::TransactionId`], this
/// is assigned by the CSMS, not minted by the charge point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReservationId(pub i64);

/// A connector reservation (OCPP `ReserveNow`), distinct from `ConnectorState::Reserved` the
/// same way [`crate::state::Transaction`] is distinct from `ConnectorState::Charging` - tracked
/// per connector on `EvseState`.
///
/// `expires_at` carries OCPP `ReserveNowRequest.expiryDateTime`, but this crate doesn't yet
/// *act* on it live (see `docs/ROADMAP.md` §8): a reservation still only ends via an explicit
/// `CancelReservation` or being superseded by a cable connection while the process keeps
/// running, since there is no background timer that expires one on the clock. The field exists
/// today specifically so `persistence::restore_reservations` can refuse to resurrect a
/// reservation whose window has already closed while the charge point was powered off - see that
/// function's docs. `None` for a reservation created without ever supplying an expiry (e.g. every
/// existing test/call site that constructs one directly rather than through the OCPP wire
/// handler) is treated as "never expires" throughout, matching the pre-existing behaviour before
/// this field was added.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Reservation {
    /// This reservation's identifier, assigned by the CSMS.
    pub id: ReservationId,
    /// The identifier this reservation is held for.
    pub id_token: IdToken,
    /// When this reservation expires, if known. See the struct docs.
    pub expires_at: Option<DateTime<Utc>>,
}
