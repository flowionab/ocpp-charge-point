//! The tariff store - default tariffs installed per EVSE (or charge-point-wide), the state behind
//! OCPP 2.1's `SetDefaultTariff`/`ClearTariffs`/`GetTariffs`. See `docs/ROADMAP.md` §9 and
//! `docs/PRODUCTION-ROADMAP.md` B7.1.
//!
//! **2.1-only.** 1.6J and 2.0.1 have no tariff messages at all - a spec boundary, not a gap - so
//! this model exists purely for the 2.1 adapter (`crate::tariff::ocpp_2_1`) to project onto; there
//! is nothing to downgrade.
//!
//! A tariff assigned to one running transaction (OCPP `ChangeTransactionTariff`) is deliberately
//! **not** modeled here. It lives on [`crate::state::EvseState::transaction_tariffs`] instead,
//! indexed the same way [`crate::state::EvseState::running_costs`] already is for `CostUpdated` -
//! cleared the moment the transaction it was assigned to starts or ends, so a new session on the
//! same connector never inherits a previous driver's tariff. See `crate::tariff`.

use alloc::string::String;
use alloc::vec::Vec;
use chrono::{DateTime, Utc};

/// A tariff's CSMS-assigned identifier (OCPP `tariffId`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct TariffId(pub String);

/// A tariff as installed on the charge point.
///
/// Deliberately shallow. OCPP's wire `Tariff` also carries the priced structure - energy/
/// charging-time/idle-time/fixed-fee price components, each with its own conditions (time of day,
/// day of week, min/max kWh...) - that a charge point would need to *compute* a running cost from
/// a tariff. This crate has no consumer for a computed price yet (`docs/ROADMAP.md` §9: outbound
/// cost reporting needs a real pricing model this crate does not have a reason to build without
/// one), and neither `GetTariffs`' response nor anything else this crate sends echoes that priced
/// structure back - so modeling it here would be dead weight kept faithful to the spec for nothing
/// to read. What's kept is everything `SetDefaultTariff`/`ClearTariffs`/`GetTariffs` actually
/// need to identify, scope and report a tariff.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Tariff {
    /// This tariff's identifier - see [`TariffId`].
    pub id: TariffId,
    /// The tariff's currency, ISO 4217.
    pub currency: String,
    /// When this tariff becomes active. `None` means immediately.
    pub valid_from: Option<DateTime<Utc>>,
}

/// Which connector scope a default tariff was installed against. OCPP addresses this as an
/// `evseId` where `0` means "every EVSE"; this type makes that sentinel explicit rather than
/// leaving a magic zero to be remembered at every use site - mirrors
/// [`crate::state::ChargingProfileScope`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TariffScope {
    /// Installed charge-point-wide (OCPP `evseId` `0`). The default for every EVSE that has no
    /// tariff of its own.
    ChargePoint,
    /// Installed against one EVSE, overriding the charge-point-wide default for it.
    Evse(usize),
}

/// A tariff together with the scope it was installed at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledTariff {
    /// Where it was installed - see [`TariffScope`].
    pub scope: TariffScope,
    /// The tariff itself.
    pub tariff: Tariff,
}

/// Why [`TariffStore::set_default`] refused a tariff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TariffSetRejection {
    /// The store is already holding [`crate::state::StateLimits::max_tariffs`] tariffs and this
    /// one is not replacing any of them (mirrors
    /// [`ChargingProfileRejection::TooManyProfiles`](crate::state::ChargingProfileRejection::TooManyProfiles)'s
    /// reasoning - a bound a remote peer can push past is not a bound).
    TooManyTariffs,
    /// A tariff with this id is already installed at a *different* scope. A tariff id identifies
    /// one tariff on the whole charge point, so installing the same id at a second scope would
    /// leave the CSMS unable to say which scope's copy is authoritative - refused rather than
    /// silently duplicated.
    DuplicateTariffId,
}

/// Which tariffs a `ClearTariffs` request selects. Every field that is `Some` must match; a
/// request with neither selects every installed tariff, matching OCPP's absent-means-all rule.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TariffClearCriteria {
    /// Match only tariffs installed at this scope.
    pub scope: Option<TariffScope>,
    /// Match one specific tariff id.
    pub id: Option<TariffId>,
}

impl TariffClearCriteria {
    /// Whether `installed` is selected by these criteria.
    pub fn matches(&self, installed: &InstalledTariff) -> bool {
        self.scope.is_none_or(|scope| installed.scope == scope)
            && self.id.as_ref().is_none_or(|id| &installed.tariff.id == id)
    }
}

/// Every default tariff installed on the charge point, across every scope - the state behind
/// OCPP 2.1's `SetDefaultTariff`/`ClearTariffs`/`GetTariffs`.
///
/// Bounded by [`crate::state::StateLimits::max_tariffs`]. Owned by
/// [`crate::state::ChargePointState`] and mutated only through
/// [`crate::state::ChargePointEvent::DefaultTariffSet`]/
/// [`TariffsCleared`](crate::state::ChargePointEvent::TariffsCleared), like every other piece of
/// state in this crate.
#[derive(Debug, Clone, PartialEq)]
pub struct TariffStore {
    tariffs: Vec<InstalledTariff>,
    max_tariffs: usize,
}

impl TariffStore {
    /// An empty store holding at most `max_tariffs` tariffs (clamped to at least 1).
    pub fn with_limit(max_tariffs: usize) -> Self {
        Self {
            tariffs: Vec::new(),
            max_tariffs: max_tariffs.max(1),
        }
    }

    /// The configured maximum - see [`crate::state::StateLimits::max_tariffs`].
    pub fn max_tariffs(&self) -> usize {
        self.max_tariffs
    }

    /// Every installed default tariff, in installation order.
    pub fn installed(&self) -> &[InstalledTariff] {
        &self.tariffs
    }

    /// How many tariffs are installed.
    pub fn len(&self) -> usize {
        self.tariffs.len()
    }

    /// Whether nothing is installed.
    pub fn is_empty(&self) -> bool {
        self.tariffs.is_empty()
    }

    /// Installs `tariff` as the default for `scope`, replacing whatever tariff currently occupies
    /// that scope - each scope holds at most one default tariff at a time, exactly like
    /// [`crate::state::ChargingProfileScope`]'s `ChargePointMax` slot.
    pub fn set_default(
        &mut self,
        scope: TariffScope,
        tariff: Tariff,
    ) -> Result<(), TariffSetRejection> {
        let duplicate_id = self
            .tariffs
            .iter()
            .any(|installed| installed.scope != scope && installed.tariff.id == tariff.id);
        if duplicate_id {
            return Err(TariffSetRejection::DuplicateTariffId);
        }
        let replacing = self.tariffs.iter().any(|installed| installed.scope == scope);
        if !replacing && self.tariffs.len() >= self.max_tariffs {
            return Err(TariffSetRejection::TooManyTariffs);
        }
        self.tariffs.retain(|installed| installed.scope != scope);
        self.tariffs.push(InstalledTariff { scope, tariff });
        Ok(())
    }

    /// Removes every tariff matching `criteria`, returning how many were removed (`0` means the
    /// CSMS's `ClearTariffs` matched nothing).
    pub fn clear(&mut self, criteria: &TariffClearCriteria) -> usize {
        let before = self.tariffs.len();
        self.tariffs.retain(|installed| !criteria.matches(installed));
        before - self.tariffs.len()
    }

    /// Every tariff matching `criteria`, in installation order - what [`Self::clear`] would
    /// remove.
    pub fn matching(&self, criteria: &TariffClearCriteria) -> Vec<&InstalledTariff> {
        self.tariffs
            .iter()
            .filter(|installed| criteria.matches(installed))
            .collect()
    }

    /// The tariffs `GetTariffs` reports for `evse_id`: every installed tariff when `evse_id` is
    /// `None` (OCPP's `evseId = 0`, "from all EVSEs"), or the charge-point-wide default plus
    /// `Evse(evse_id)`'s own tariff (if either is installed) otherwise.
    pub fn selected_by_evse(&self, evse_id: Option<usize>) -> Vec<&InstalledTariff> {
        let Some(evse_id) = evse_id else {
            return self.tariffs.iter().collect();
        };
        self.tariffs
            .iter()
            .filter(|installed| match installed.scope {
                TariffScope::ChargePoint => true,
                TariffScope::Evse(id) => id == evse_id,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tariff(id: &str) -> Tariff {
        Tariff {
            id: TariffId(id.into()),
            currency: "EUR".into(),
            valid_from: None,
        }
    }

    #[test]
    fn installing_at_a_new_scope_stacks_beside_an_existing_one() {
        let mut store = TariffStore::with_limit(10);
        store
            .set_default(TariffScope::ChargePoint, tariff("t1"))
            .unwrap();
        store.set_default(TariffScope::Evse(0), tariff("t2")).unwrap();

        assert_eq!(store.len(), 2);
    }

    #[test]
    fn installing_at_an_occupied_scope_replaces_it() {
        let mut store = TariffStore::with_limit(10);
        store.set_default(TariffScope::Evse(0), tariff("t1")).unwrap();
        store.set_default(TariffScope::Evse(0), tariff("t2")).unwrap();

        assert_eq!(store.len(), 1);
        assert_eq!(store.installed()[0].tariff.id, TariffId("t2".into()));
    }

    #[test]
    fn the_same_id_at_a_different_scope_is_refused_as_a_duplicate() {
        let mut store = TariffStore::with_limit(10);
        store
            .set_default(TariffScope::ChargePoint, tariff("t1"))
            .unwrap();

        assert_eq!(
            store.set_default(TariffScope::Evse(0), tariff("t1")),
            Err(TariffSetRejection::DuplicateTariffId)
        );
    }

    #[test]
    fn the_tariff_bound_refuses_a_new_tariff_but_never_a_replacement() {
        let mut store = TariffStore::with_limit(1);
        store.set_default(TariffScope::Evse(0), tariff("t1")).unwrap();

        assert_eq!(
            store.set_default(TariffScope::Evse(1), tariff("t2")),
            Err(TariffSetRejection::TooManyTariffs)
        );
        assert_eq!(
            store.set_default(TariffScope::Evse(0), tariff("t1-again")),
            Ok(())
        );
    }

    #[test]
    fn clearing_by_id_removes_only_the_matching_tariff() {
        let mut store = TariffStore::with_limit(10);
        store.set_default(TariffScope::Evse(0), tariff("t1")).unwrap();
        store.set_default(TariffScope::Evse(1), tariff("t2")).unwrap();

        assert_eq!(
            store.clear(&TariffClearCriteria {
                id: Some(TariffId("t1".into())),
                ..Default::default()
            }),
            1
        );
        assert_eq!(store.len(), 1);
        assert_eq!(store.installed()[0].tariff.id, TariffId("t2".into()));
    }

    #[test]
    fn clearing_with_no_criteria_at_all_clears_everything() {
        let mut store = TariffStore::with_limit(10);
        store.set_default(TariffScope::Evse(0), tariff("t1")).unwrap();
        store
            .set_default(TariffScope::ChargePoint, tariff("t2"))
            .unwrap();

        assert_eq!(store.clear(&TariffClearCriteria::default()), 2);
        assert!(store.is_empty());
    }

    #[test]
    fn clearing_something_absent_removes_nothing() {
        let mut store = TariffStore::with_limit(10);
        assert_eq!(
            store.clear(&TariffClearCriteria {
                id: Some(TariffId("nope".into())),
                ..Default::default()
            }),
            0
        );
    }

    #[test]
    fn getting_evse_zero_returns_every_installed_tariff() {
        let mut store = TariffStore::with_limit(10);
        store
            .set_default(TariffScope::ChargePoint, tariff("t1"))
            .unwrap();
        store.set_default(TariffScope::Evse(0), tariff("t2")).unwrap();
        store.set_default(TariffScope::Evse(1), tariff("t3")).unwrap();

        assert_eq!(store.selected_by_evse(None).len(), 3);
    }

    #[test]
    fn getting_one_evse_returns_its_own_tariff_and_the_charge_point_default() {
        let mut store = TariffStore::with_limit(10);
        store
            .set_default(TariffScope::ChargePoint, tariff("t1"))
            .unwrap();
        store.set_default(TariffScope::Evse(0), tariff("t2")).unwrap();
        store.set_default(TariffScope::Evse(1), tariff("t3")).unwrap();

        let ids: Vec<String> = store
            .selected_by_evse(Some(0))
            .iter()
            .map(|installed| installed.tariff.id.0.clone())
            .collect();
        assert_eq!(ids, alloc::vec!["t1".to_owned(), "t2".to_owned()]);
    }
}
