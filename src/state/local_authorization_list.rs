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
///
/// Bounded at construction by [`max_entries`](Self::max_entries) (see
/// [`crate::state::StateLimits`]) so a CSMS can't grow a charge point's memory use without limit by
/// sending an arbitrarily long list - `docs/PRODUCTION-ROADMAP.md` §9.2 (G2.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalAuthorizationList {
    /// The list's version number, as set by the CSMS's most recent `SendLocalList` (or `0` for a
    /// never-populated list).
    pub version: i64,
    /// The list's entries. Never longer than [`max_entries`](Self::max_entries) - the only way to
    /// change it is [`Self::replace`], which enforces that.
    pub entries: Vec<LocalListEntry>,
    /// The most entries this list may hold, fixed for the life of the charge point. See
    /// [`crate::state::StateLimits::max_local_authorization_list_entries`], which is where callers
    /// configure it.
    pub max_entries: usize,
}

impl LocalAuthorizationList {
    /// An empty list at version `0`, holding at most
    /// [`DEFAULT_MAX_LOCAL_AUTHORIZATION_LIST_ENTRIES`](crate::state::DEFAULT_MAX_LOCAL_AUTHORIZATION_LIST_ENTRIES)
    /// entries.
    pub fn new() -> Self {
        Self::with_max_entries(crate::state::DEFAULT_MAX_LOCAL_AUTHORIZATION_LIST_ENTRIES)
    }

    /// An empty list at version `0` holding at most `max_entries` entries (clamped to at least 1 -
    /// a list that can hold nothing would drop every entry sent to it, which is
    /// indistinguishable from not supporting the local authorization list at all; integrators who
    /// want that should leave [`crate::hardware::Capabilities::local_auth_list`] unset instead).
    pub fn with_max_entries(max_entries: usize) -> Self {
        Self {
            version: 0,
            entries: Vec::new(),
            max_entries: max_entries.max(1),
        }
    }

    /// Replaces the list's contents with `entries` and adopts `version`, truncating to
    /// [`max_entries`](Self::max_entries) and returning how many entries that dropped (`0` in the
    /// normal case).
    ///
    /// Callers are expected to reject an over-long update *before* it gets here (see
    /// [`crate::local_authorization_list::handle_send_local_list`], which refuses one with
    /// [`SendLocalListOutcome::TooManyEntries`](crate::local_authorization_list::SendLocalListOutcome::TooManyEntries)),
    /// so a non-zero return means information was lost: the caller should surface it rather than
    /// ignore it. [`crate::state::ChargePointState::apply`] raises a `MemoryExhaustion` security
    /// event when it happens.
    ///
    /// `version` is adopted even when entries were dropped: the CSMS believes the list it sent is
    /// at that version, and recording an older one would make the next differential update apply
    /// on top of a list this charge point doesn't hold either.
    pub fn replace(&mut self, version: i64, mut entries: Vec<LocalListEntry>) -> usize {
        let dropped = entries.len().saturating_sub(self.max_entries);
        entries.truncate(self.max_entries);
        self.version = version;
        self.entries = entries;
        dropped
    }

    /// Forgets the entry for `id_token`, if this list holds one - the erasure half of GDPR
    /// customer information handling (`crate::customer_information`,
    /// `docs/PRODUCTION-ROADMAP.md` B5.5).
    ///
    /// Bumps [`Self::version`] when an entry was actually removed: the list now on this charge
    /// point no longer matches what the CSMS believes it last sent, the same reasoning
    /// [`Self::replace`]'s docs give for always adopting the version it's told, just in the other
    /// direction here - a CSMS whose next differential `SendLocalList` assumes the old version
    /// must be made to notice (via `GetLocalListVersion`) rather than silently apply on top of a
    /// list that has moved out from under it.
    ///
    /// Returns whether an entry was actually held (and so removed).
    pub fn forget(&mut self, id_token: &IdToken) -> bool {
        let before = self.entries.len();
        self.entries
            .retain(|entry| entry.id_token.value != id_token.value);
        let changed = self.entries.len() != before;
        if changed {
            self.version += 1;
        }
        changed
    }
}

impl Default for LocalAuthorizationList {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::IdTokenKind;
    use alloc::string::ToString;

    fn entry(value: &str) -> LocalListEntry {
        LocalListEntry {
            id_token: IdToken {
                value: value.to_string(),
                kind: IdTokenKind::ISO14443,
            },
            status: AuthorizationStatus::Accepted,
        }
    }

    fn entries(count: usize) -> Vec<LocalListEntry> {
        (0..count)
            .map(|index| entry(&alloc::format!("token-{index}")))
            .collect()
    }

    #[test]
    fn a_fresh_list_uses_the_default_maximum() {
        let list = LocalAuthorizationList::new();

        assert_eq!(
            list.max_entries,
            crate::state::DEFAULT_MAX_LOCAL_AUTHORIZATION_LIST_ENTRIES
        );
        assert_eq!(list.version, 0);
        assert!(list.entries.is_empty());
    }

    #[test]
    fn replacing_within_the_maximum_keeps_every_entry() {
        let mut list = LocalAuthorizationList::with_max_entries(4);

        let dropped = list.replace(7, entries(4));

        assert_eq!(dropped, 0);
        assert_eq!(list.version, 7);
        assert_eq!(list.entries.len(), 4);
    }

    #[test]
    fn replacing_beyond_the_maximum_truncates_and_reports_what_it_dropped() {
        let mut list = LocalAuthorizationList::with_max_entries(2);

        let dropped = list.replace(1, entries(5));

        assert_eq!(dropped, 3);
        assert_eq!(list.entries, entries(2));
        // The version is still adopted - the list the CSMS believes is at this version is the one
        // that was sent, and claiming an older version would make the next differential update
        // apply on top of a list this charge point doesn't actually hold either.
        assert_eq!(list.version, 1);
    }

    #[test]
    fn forgetting_an_entry_removes_it_and_bumps_the_version() {
        let mut list = LocalAuthorizationList::with_max_entries(4);
        list.replace(1, entries(2));

        let removed_token = entries(2)[0].id_token.clone();
        assert!(list.forget(&removed_token));
        assert_eq!(list.entries.len(), 1);
        assert_eq!(list.version, 2);

        // Forgetting a token never held is a no-op that reports honestly and leaves the version
        // alone.
        assert!(!list.forget(&removed_token));
        assert_eq!(list.version, 2);
    }

    #[test]
    fn a_maximum_of_zero_is_clamped_to_one() {
        let mut list = LocalAuthorizationList::with_max_entries(0);

        assert_eq!(list.max_entries, 1);
        assert_eq!(list.replace(1, entries(2)), 1);
        assert_eq!(list.entries.len(), 1);
    }
}
