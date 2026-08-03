/// Whether an [`crate::state::IdToken`] is allowed to start charging (a simplified view of
/// OCPP's 10-value `AuthorizationStatusEnumType` - see `docs/ROADMAP.md` §3 for what's
/// collapsed into `Rejected` and why).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorizationStatus {
    Accepted,
    Rejected,
}
