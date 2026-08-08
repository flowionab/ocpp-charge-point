//! The authorization cache: decisions the CSMS has already made, kept so a charge point that
//! cannot reach it can still answer (`docs/ROADMAP.md` §3, `docs/PRODUCTION-ROADMAP.md` B1.2).
//!
//! # How this differs from the local authorization list
//!
//! [`LocalAuthorizationList`](crate::state::LocalAuthorizationList) is *pushed* by the CSMS
//! (`SendLocalList`) and is authoritative: the operator decided in advance who may charge. The
//! cache is *learned* - every `Authorize` answer is remembered, so a token that worked five
//! minutes ago still works when the link drops. The list therefore wins when both have an opinion,
//! and only the cache expires.
//!
//! # Bounds and expiry
//!
//! Bounded by [`StateLimits::max_authorization_cache_entries`](crate::state::StateLimits::max_authorization_cache_entries)
//! (G2.2), evicting the least recently used entry when full: a cache is a convenience, and the
//! token used least recently is the one least likely to be needed offline. Entries also expire
//! after `AuthCacheCtrlr`/`LifeTime` seconds - see [`AuthorizationCache::lookup`], which takes the
//! current time from its caller rather than a clock of its own, because the state machine holding
//! this is deliberately clock-free (see [`crate::clock`]).

use alloc::string::String;
use alloc::vec::Vec;
use chrono::{DateTime, Utc};

use crate::state::{AuthorizationStatus, IdToken};

/// Default maximum number of cached authorization decisions (see
/// [`StateLimits::max_authorization_cache_entries`](crate::state::StateLimits::max_authorization_cache_entries)).
///
/// 50 covers the population that actually turns up repeatedly at one charge point - the regulars
/// a cache exists to serve - in a couple of KB. A site that wants every card it has ever seen
/// should raise it, or push a real [`LocalAuthorizationList`](crate::state::LocalAuthorizationList)
/// instead, which is the mechanism designed for "the operator decided in advance".
pub const DEFAULT_MAX_AUTHORIZATION_CACHE_ENTRIES: usize = 50;

/// One remembered decision.
///
/// `serde`-derived directly rather than through a mirror type in [`crate::persistence`], like
/// [`LocalListEntry`](crate::state::LocalListEntry) beside it: every field is either a scalar or
/// another already-derived state type, so there is no closed wire enum here for a mirror to
/// protect against drifting.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AuthorizationCacheEntry {
    /// The identifier the decision was about.
    pub id_token: IdToken,
    /// What the CSMS decided.
    pub status: AuthorizationStatus,
    /// When it was decided, per the [`Clock`](crate::clock::Clock) of whoever cached it, or `None`
    /// if that clock wasn't synchronized at the time (see [`crate::clock::is_synchronized`]).
    ///
    /// An entry with no timestamp **never expires**, and that is the deliberate reading: on
    /// hardware with no RTC the alternative is either expiring everything immediately (a cache
    /// that caches nothing) or inventing an age. Keeping it is also the safer error - an operator
    /// who needs stale decisions gone can clear the cache, which is exactly what `ClearCache` is
    /// for.
    pub cached_at: Option<DateTime<Utc>>,
}

/// Every remembered authorization decision, most recently used last.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizationCache {
    entries: Vec<AuthorizationCacheEntry>,
    max_entries: usize,
}

impl AuthorizationCache {
    /// An empty cache holding at most `max_entries` decisions (clamped to at least 1).
    pub fn with_max_entries(max_entries: usize) -> Self {
        Self {
            entries: Vec::new(),
            max_entries: max_entries.max(1),
        }
    }

    /// The configured maximum.
    pub fn max_entries(&self) -> usize {
        self.max_entries
    }

    /// Every entry, least recently used first.
    pub fn entries(&self) -> &[AuthorizationCacheEntry] {
        &self.entries
    }

    /// How many decisions are cached.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is cached.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Remembers `status` for `id_token`, replacing any previous decision for it and marking it
    /// most recently used. Evicts the least recently used entry if that would exceed the bound.
    ///
    /// Returns whether anything changed - re-caching an identical decision for the same token
    /// still counts as a change, since it refreshes both the entry's age and its position.
    pub fn remember(
        &mut self,
        id_token: IdToken,
        status: AuthorizationStatus,
        cached_at: Option<DateTime<Utc>>,
    ) -> bool {
        self.entries
            .retain(|entry| !same_token(&entry.id_token, &id_token));
        while self.entries.len() >= self.max_entries {
            self.entries.remove(0);
        }
        self.entries.push(AuthorizationCacheEntry {
            id_token,
            status,
            cached_at,
        });
        true
    }

    /// The cached decision for `id_token`, if one is cached and hasn't expired.
    ///
    /// `now` and `life_time_secs` come from the caller: the clock because this type lives inside
    /// the clock-free state machine, and the lifetime because it is a device-model variable
    /// (`AuthCacheCtrlr`/`LifeTime`) a CSMS can change at any time. A `life_time_secs` of `None`
    /// or `0` means entries don't expire, which is OCPP's own reading of an unset lifetime.
    ///
    /// This does **not** mark the entry as recently used: `lookup` takes `&self` so it can be
    /// called from a read-only view of the state, and the "used" ordering is refreshed by
    /// [`Self::remember`] when a fresh decision arrives. The consequence is honest and worth
    /// knowing: eviction order tracks how recently a token was *authorized by the CSMS*, not how
    /// recently it was served from cache.
    pub fn lookup(
        &self,
        id_token: &IdToken,
        now: Option<DateTime<Utc>>,
        life_time_secs: Option<u32>,
    ) -> Option<&AuthorizationCacheEntry> {
        let entry = self
            .entries
            .iter()
            .find(|entry| same_token(&entry.id_token, id_token))?;
        if entry.is_expired(now, life_time_secs) {
            return None;
        }
        Some(entry)
    }

    /// Forgets every cached decision, returning how many were discarded - what `ClearCache`
    /// reports on.
    pub fn clear(&mut self) -> usize {
        let discarded = self.entries.len();
        self.entries.clear();
        discarded
    }

    /// Forgets the cached decision for `id_token`, if one is held - the erasure half of GDPR
    /// customer information handling (`crate::customer_information`,
    /// `docs/PRODUCTION-ROADMAP.md` B5.5). Unlike [`Self::lookup`], this removes the entry
    /// outright regardless of whether it has expired: an expired entry is still data this charge
    /// point holds about the customer until something actually deletes it.
    ///
    /// Returns whether an entry was actually held (and so removed).
    pub fn forget(&mut self, id_token: &IdToken) -> bool {
        let before = self.entries.len();
        self.entries
            .retain(|entry| !same_token(&entry.id_token, id_token));
        self.entries.len() != before
    }

    /// Replaces the cache's contents (used by recovery from durable storage), respecting the
    /// bound. Returns how many entries the bound dropped.
    pub fn replace(&mut self, entries: Vec<AuthorizationCacheEntry>) -> usize {
        self.entries = entries;
        let dropped = self.entries.len().saturating_sub(self.max_entries);
        if dropped > 0 {
            // Keep the most recently authorized tail, which is the half a cache exists to serve.
            self.entries.drain(0..dropped);
        }
        dropped
    }
}

impl AuthorizationCacheEntry {
    /// Whether this entry is past `life_time_secs` as of `now` - see
    /// [`AuthorizationCache::lookup`] for where those two arguments come from and why.
    ///
    /// An entry with no `cached_at`, a caller with no usable `now`, or a lifetime of `None`/`0`
    /// all mean "never expires".
    pub fn is_expired(&self, now: Option<DateTime<Utc>>, life_time_secs: Option<u32>) -> bool {
        let (Some(cached_at), Some(now), Some(life_time_secs)) =
            (self.cached_at, now, life_time_secs)
        else {
            return false;
        };
        if life_time_secs == 0 {
            return false;
        }
        (now - cached_at).num_seconds() >= i64::from(life_time_secs)
    }
}

/// Two identifiers are the same cache key when their values match. The *kind* is deliberately not
/// part of the key: a CSMS that reports the same RFID uid as `ISO14443` on one occasion and
/// `Local` on another has still authorized the same physical card, and treating those as two
/// entries would double the cache's footprint for no benefit.
fn same_token(cached: &IdToken, presented: &IdToken) -> bool {
    let cached_value: &String = &cached.value;
    cached_value == &presented.value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::IdTokenKind;

    fn token(value: &str) -> IdToken {
        IdToken {
            value: value.into(),
            kind: IdTokenKind::ISO14443,
        }
    }

    fn at(secs: i64) -> Option<DateTime<Utc>> {
        DateTime::from_timestamp(1_800_000_000 + secs, 0)
    }

    #[test]
    fn a_remembered_decision_is_found_again() {
        let mut cache = AuthorizationCache::with_max_entries(10);
        cache.remember(token("A"), AuthorizationStatus::Accepted, at(0));

        let entry = cache.lookup(&token("A"), at(1), None).expect("cached");
        assert_eq!(entry.status, AuthorizationStatus::Accepted);
        assert!(cache.lookup(&token("B"), at(1), None).is_none());
    }

    #[test]
    fn a_rejection_is_cached_too_not_just_an_acceptance() {
        // A cache that only remembered "yes" would let a revoked card in every time the link
        // drops, which is the opposite of what caching is for.
        let mut cache = AuthorizationCache::with_max_entries(10);
        cache.remember(token("A"), AuthorizationStatus::Rejected, at(0));

        assert_eq!(
            cache.lookup(&token("A"), at(1), None).map(|e| e.status),
            Some(AuthorizationStatus::Rejected)
        );
    }

    #[test]
    fn a_fresh_decision_replaces_the_previous_one_for_that_token() {
        let mut cache = AuthorizationCache::with_max_entries(10);
        cache.remember(token("A"), AuthorizationStatus::Accepted, at(0));
        cache.remember(token("A"), AuthorizationStatus::Rejected, at(10));

        assert_eq!(cache.len(), 1);
        assert_eq!(
            cache.lookup(&token("A"), at(11), None).map(|e| e.status),
            Some(AuthorizationStatus::Rejected)
        );
    }

    #[test]
    fn the_same_card_reported_with_a_different_kind_is_the_same_entry() {
        let mut cache = AuthorizationCache::with_max_entries(10);
        cache.remember(token("A"), AuthorizationStatus::Accepted, at(0));
        let mut other_kind = token("A");
        other_kind.kind = IdTokenKind::Local;
        cache.remember(other_kind.clone(), AuthorizationStatus::Rejected, at(1));

        assert_eq!(cache.len(), 1);
        assert_eq!(
            cache.lookup(&other_kind, at(2), None).map(|e| e.status),
            Some(AuthorizationStatus::Rejected)
        );
    }

    #[test]
    fn the_bound_evicts_the_least_recently_authorized_entry() {
        let mut cache = AuthorizationCache::with_max_entries(2);
        cache.remember(token("A"), AuthorizationStatus::Accepted, at(0));
        cache.remember(token("B"), AuthorizationStatus::Accepted, at(1));
        cache.remember(token("C"), AuthorizationStatus::Accepted, at(2));

        assert_eq!(cache.len(), 2);
        assert!(cache.lookup(&token("A"), at(3), None).is_none());
        assert!(cache.lookup(&token("B"), at(3), None).is_some());
        assert!(cache.lookup(&token("C"), at(3), None).is_some());
    }

    #[test]
    fn re_authorizing_a_token_moves_it_out_of_the_eviction_firing_line() {
        let mut cache = AuthorizationCache::with_max_entries(2);
        cache.remember(token("A"), AuthorizationStatus::Accepted, at(0));
        cache.remember(token("B"), AuthorizationStatus::Accepted, at(1));
        // A is authorized again, so B is now the least recently authorized.
        cache.remember(token("A"), AuthorizationStatus::Accepted, at(2));
        cache.remember(token("C"), AuthorizationStatus::Accepted, at(3));

        assert!(cache.lookup(&token("A"), at(4), None).is_some());
        assert!(cache.lookup(&token("B"), at(4), None).is_none());
    }

    #[test]
    fn an_entry_older_than_the_lifetime_is_not_returned() {
        let mut cache = AuthorizationCache::with_max_entries(10);
        cache.remember(token("A"), AuthorizationStatus::Accepted, at(0));

        assert!(cache.lookup(&token("A"), at(59), Some(60)).is_some());
        assert!(cache.lookup(&token("A"), at(60), Some(60)).is_none());
    }

    #[test]
    fn a_zero_or_absent_lifetime_means_entries_never_expire() {
        let mut cache = AuthorizationCache::with_max_entries(10);
        cache.remember(token("A"), AuthorizationStatus::Accepted, at(0));

        assert!(cache.lookup(&token("A"), at(1_000_000), Some(0)).is_some());
        assert!(cache.lookup(&token("A"), at(1_000_000), None).is_some());
    }

    #[test]
    fn an_entry_cached_without_a_usable_clock_never_expires() {
        // Hardware with no RTC: expiring everything immediately would be a cache that caches
        // nothing, and inventing an age would be worse.
        let mut cache = AuthorizationCache::with_max_entries(10);
        cache.remember(token("A"), AuthorizationStatus::Accepted, None);

        assert!(cache.lookup(&token("A"), at(1_000_000), Some(60)).is_some());
    }

    #[test]
    fn a_reader_with_no_usable_clock_cannot_expire_anything_either() {
        let mut cache = AuthorizationCache::with_max_entries(10);
        cache.remember(token("A"), AuthorizationStatus::Accepted, at(0));

        assert!(cache.lookup(&token("A"), None, Some(60)).is_some());
    }

    #[test]
    fn forgetting_removes_only_the_named_token_regardless_of_expiry() {
        let mut cache = AuthorizationCache::with_max_entries(10);
        cache.remember(token("A"), AuthorizationStatus::Accepted, at(0));
        cache.remember(token("B"), AuthorizationStatus::Accepted, at(0));

        // Expired, but still held - and `forget` must still find and remove it.
        assert!(cache.lookup(&token("A"), at(1_000_000), Some(60)).is_none());
        assert!(cache.forget(&token("A")));
        assert_eq!(cache.len(), 1);
        assert!(cache.lookup(&token("B"), at(1), None).is_some());

        // Forgetting a token never held is a no-op that reports honestly.
        assert!(!cache.forget(&token("A")));
    }

    #[test]
    fn clearing_discards_everything_and_reports_how_much() {
        let mut cache = AuthorizationCache::with_max_entries(10);
        cache.remember(token("A"), AuthorizationStatus::Accepted, at(0));
        cache.remember(token("B"), AuthorizationStatus::Accepted, at(1));

        assert_eq!(cache.clear(), 2);
        assert!(cache.is_empty());
        assert_eq!(cache.clear(), 0);
    }

    #[test]
    fn restoring_more_entries_than_the_bound_keeps_the_most_recent() {
        let mut cache = AuthorizationCache::with_max_entries(2);
        let dropped = cache.replace(alloc::vec![
            AuthorizationCacheEntry {
                id_token: token("A"),
                status: AuthorizationStatus::Accepted,
                cached_at: at(0),
            },
            AuthorizationCacheEntry {
                id_token: token("B"),
                status: AuthorizationStatus::Accepted,
                cached_at: at(1),
            },
            AuthorizationCacheEntry {
                id_token: token("C"),
                status: AuthorizationStatus::Accepted,
                cached_at: at(2),
            },
        ]);

        assert_eq!(dropped, 1);
        assert_eq!(cache.len(), 2);
        assert!(cache.lookup(&token("A"), at(3), None).is_none());
        assert!(cache.lookup(&token("C"), at(3), None).is_some());
    }
}
