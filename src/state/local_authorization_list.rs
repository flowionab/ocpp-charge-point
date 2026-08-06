use alloc::vec::Vec;

use crate::state::{AuthorizationStatus, IdToken};

/// One entry in the local authorization list (OCPP `AuthorizationData`), collapsed to what this
/// crate's Authorization functional block already supports - a binary accept/reject decision,
/// not the richer `IdTokenInfo` (cache expiry, `groupIdToken`, `evseId` scoping - see
/// `docs/ROADMAP.md` §3/§4).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LocalListEntry {
    /// The identifier this entry decides.
    pub id_token: IdToken,
    /// Whether `id_token` is authorized.
    pub status: AuthorizationStatus,
}

/// The charge point's local authorization list (OCPP `SendLocalList`/`GetLocalListVersion`) - an
/// offline cache of authorization decisions. Storing and versioning the list is implemented; it
/// isn't yet consulted by the Authorization functional block itself (every presented id token
/// still round-trips through Authorize, online or not - see `docs/ROADMAP.md` §4, which needs
/// connection-state tracking from §0 first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalAuthorizationList {
    /// The list's version number, as set by the CSMS's most recent `SendLocalList` (or `0` for a
    /// never-populated list).
    pub version: i64,
    /// The list's entries.
    pub entries: Vec<LocalListEntry>,
}

impl LocalAuthorizationList {
    /// An empty list at version `0`.
    pub fn new() -> Self {
        Self {
            version: 0,
            entries: Vec::new(),
        }
    }
}

impl Default for LocalAuthorizationList {
    fn default() -> Self {
        Self::new()
    }
}
