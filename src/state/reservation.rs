use crate::state::IdToken;

/// Identifies a reservation (OCPP `ReserveNow.id`). Unlike [`crate::state::TransactionId`], this
/// is assigned by the CSMS, not minted by the charge point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReservationId(pub i64);

/// A connector reservation (OCPP `ReserveNow`), distinct from `ConnectorState::Reserved` the
/// same way [`crate::state::Transaction`] is distinct from `ConnectorState::Charging` - tracked
/// per connector on `EvseState`. Expiry isn't modeled yet (see `docs/ROADMAP.md` §8): a
/// reservation only ends via an explicit `CancelReservation`, or being superseded by a cable
/// connection, not by elapsed time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reservation {
    pub id: ReservationId,
    pub id_token: IdToken,
}
