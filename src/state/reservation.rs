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
/// `expires_at` carries OCPP `ReserveNowRequest.expiryDateTime` (2.x)/`expiryDate` (1.6J), parsed
/// from the wire request by `crate::reservation::handle_reserve_now`'s callers - `None` if the
/// CSMS's value didn't parse as RFC 3339, or if the `Reservation` was constructed directly (e.g.
/// most existing tests) rather than through the OCPP wire handler; either way it is treated as
/// "never expires" throughout.
///
/// This crate doesn't yet *act* on `expires_at` live (see `docs/ROADMAP.md` §8): a reservation
/// still only ends via an explicit `CancelReservation` or being superseded by a cable connection
/// while the process keeps running, since the state machine is deliberately clock-free and there
/// is no background timer that expires one on the clock. The field is consulted today
/// specifically so `persistence::restore_reservations` can refuse to resurrect a reservation
/// whose window has already closed while the charge point was powered off - see that function's
/// docs. A reservation created and expiring within a single boot therefore does not expire on its
/// own yet; a periodic sweep task (in the shape of `crate::provisioning::run_heartbeat`, driving
/// a new `ChargePointEvent` through the actor on an `Executor`/`Backoff`-driven interval) is the
/// natural next step, but needs a new event variant on `ChargePointEvent`/`ChargePointState` to
/// land first.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Reservation {
    /// This reservation's identifier, assigned by the CSMS.
    pub id: ReservationId,
    /// The identifier this reservation is held for.
    pub id_token: IdToken,
    /// The *group* this reservation is held for, if the CSMS named one - OCPP
    /// `ReserveNowRequest.groupIdToken` (2.x), `parentIdTag` (1.6J).
    ///
    /// A reservation may be honoured by any driver in the group, which is what makes a fleet
    /// reservation useful: the vehicle that turns up is rarely the one the booking named.
    /// **F01.FR.22** is the rule - a start is refused only when *neither* the identifier nor the
    /// group matches - and [`crate::remote_control::handle_request_start_transaction`] is where it
    /// is applied.
    ///
    /// `None` means the reservation names no group, and a request naming one does not match it.
    /// Absence is not a wildcard in either direction: two reservations with no group are not
    /// thereby in the same group.
    #[serde(default)]
    pub group_id_token: Option<IdToken>,
    /// When this reservation expires, if known. See the struct docs.
    pub expires_at: Option<DateTime<Utc>>,
}
