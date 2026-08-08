//! Network connection profiles: how the CSMS tells a charge point where to connect *next time*
//! (`docs/ROADMAP.md` §2, `docs/PRODUCTION-ROADMAP.md` B1.8).
//!
//! # What storing one does, and what it doesn't
//!
//! `SetNetworkProfile` writes a profile into a numbered configuration slot. It does **not** switch
//! the live connection - OCPP is explicit that the profile applies to a future connection attempt,
//! and the CSMS orders slots separately via `OCPPCommCtrlr`/`NetworkConfigurationPriority`. So
//! this crate stores profiles, reports them, and keeps them across a reboot; *dialling* one, and
//! rolling back when a new profile fails to connect, is
//! [A9](../../docs/PRODUCTION-ROADMAP.md) and is not done.
//!
//! That boundary is worth stating plainly because it is easy to mistake a full slot store for a
//! working profile switch: a charge point built on this crate today connects to whatever address
//! its integrator passed to [`crate::connect::connect_and_setup`], whatever these slots say.
//!
//! # 2.x only
//!
//! 1.6J has no `SetNetworkProfile` at all (network configuration lives in its security whitepaper
//! extensions, which `ocpp-client` does not generate types for - see `docs/PRODUCTION-ROADMAP.md`
//! D2.2), so there is no 1.6J adapter and none is missing.

use alloc::string::String;
use alloc::vec::Vec;

/// Which physical interface a profile connects over (OCPP `OCPPInterfaceEnum`). Recorded for the
/// CSMS's benefit; this crate has no interface selection of its own - see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum NetworkInterface {
    /// A wired interface, by index (`Wired0`..`Wired3`).
    Wired(u8),
    /// A wireless interface, by index (`Wireless0`..`Wireless3`).
    Wireless(u8),
    /// Any interface the charge point has.
    Any,
}

/// The transport a profile uses. OCPP 2.x defines `SOAP` and `JSON`; only `JSON` is meaningful for
/// a 2.x charge point, and a `SOAP` profile is refused rather than stored (see
/// [`crate::network_profile::handle_set_network_profile`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum NetworkTransport {
    /// JSON over WebSocket - what every OCPP 2.x connection uses.
    Json,
    /// SOAP, which OCPP 2.x does not support.
    Soap,
}

/// One network connection profile, as stored in a configuration slot.
///
/// A deliberate subset of OCPP's `NetworkConnectionProfile`: the fields a charge point needs to
/// *make* a connection, minus the ones this crate has no way to use. Notably absent are `apn`
/// (cellular modem configuration - hardware this crate has no binding for) and `vpn`, both of
/// which belong to an integrator's own connectivity stack rather than to the OCPP application
/// layer; and `basicAuthPassword`, which is a **credential** and is dropped on purpose - see
/// [`NetworkConnectionProfile::security_profile`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NetworkConnectionProfile {
    /// The CSMS URL to connect to.
    pub csms_url: String,
    /// The interface to connect over.
    pub interface: NetworkInterface,
    /// The transport to use.
    pub transport: NetworkTransport,
    /// The OCPP security profile (1, 2 or 3) this connection should use. Recorded, not enforced:
    /// this crate implements none of them yet (workstream F), which is also why
    /// `basicAuthPassword` is not stored - keeping a credential this crate cannot use, in state
    /// that is reported by `GetVariables` and written to durable storage, would be a liability
    /// with no upside.
    pub security_profile: u8,
    /// How long to wait for a message on this connection, in seconds.
    pub message_timeout_secs: i64,
    /// The charge point's identity on this connection, if the profile names one.
    pub identity: Option<String>,
}

/// One occupied configuration slot.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NetworkProfileSlot {
    /// The slot number the CSMS addressed.
    pub slot: i32,
    /// The profile in it.
    pub profile: NetworkConnectionProfile,
}

/// Default maximum number of occupied network profile slots (see
/// [`StateLimits::max_network_profile_slots`](crate::state::StateLimits::max_network_profile_slots)).
///
/// Four covers a primary CSMS, a fallback, and room to stage a replacement without evicting
/// either - which is what slots are for. A charge point is not a routing table.
pub const DEFAULT_MAX_NETWORK_PROFILE_SLOTS: usize = 4;

/// Every configuration slot the CSMS has written, ordered by slot number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkProfileStore {
    slots: Vec<NetworkProfileSlot>,
    max_slots: usize,
}

impl NetworkProfileStore {
    /// An empty store holding at most `max_slots` occupied slots (clamped to at least 1).
    pub fn with_max_slots(max_slots: usize) -> Self {
        Self {
            slots: Vec::new(),
            max_slots: max_slots.max(1),
        }
    }

    /// The configured maximum.
    pub fn max_slots(&self) -> usize {
        self.max_slots
    }

    /// Every occupied slot, by ascending slot number.
    pub fn slots(&self) -> &[NetworkProfileSlot] {
        &self.slots
    }

    /// How many slots are occupied.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Whether no slot is occupied.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// The profile in `slot`, if it is occupied.
    pub fn get(&self, slot: i32) -> Option<&NetworkConnectionProfile> {
        self.slots
            .iter()
            .find(|occupied| occupied.slot == slot)
            .map(|occupied| &occupied.profile)
    }

    /// Writes `profile` into `slot`, replacing whatever was there.
    ///
    /// Returns `false` - leaving the store untouched - when the slot is new *and* the store is
    /// already full. Overwriting an occupied slot always succeeds: a CSMS replacing a profile is
    /// not asking for more storage, and refusing it would leave the charge point holding a
    /// connection profile the CSMS believes it has replaced.
    pub fn set(&mut self, slot: i32, profile: NetworkConnectionProfile) -> bool {
        if let Some(occupied) = self.slots.iter_mut().find(|occupied| occupied.slot == slot) {
            occupied.profile = profile;
            return true;
        }
        if self.slots.len() >= self.max_slots {
            return false;
        }
        self.slots.push(NetworkProfileSlot { slot, profile });
        self.slots.sort_by_key(|occupied| occupied.slot);
        true
    }

    /// Replaces the store's contents (recovery from durable storage), respecting the bound.
    /// Returns how many slots the bound dropped, from the highest slot numbers down.
    pub fn replace(&mut self, mut slots: Vec<NetworkProfileSlot>) -> usize {
        slots.sort_by_key(|occupied| occupied.slot);
        let dropped = slots.len().saturating_sub(self.max_slots);
        slots.truncate(self.max_slots);
        self.slots = slots;
        dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(url: &str) -> NetworkConnectionProfile {
        NetworkConnectionProfile {
            csms_url: url.into(),
            interface: NetworkInterface::Any,
            transport: NetworkTransport::Json,
            security_profile: 1,
            message_timeout_secs: 30,
            identity: None,
        }
    }

    #[test]
    fn a_written_slot_is_readable_again() {
        let mut store = NetworkProfileStore::with_max_slots(4);

        assert!(store.set(1, profile("wss://a")));

        assert_eq!(store.get(1).map(|p| p.csms_url.as_str()), Some("wss://a"));
        assert!(store.get(2).is_none());
    }

    #[test]
    fn writing_an_occupied_slot_replaces_it_even_when_the_store_is_full() {
        let mut store = NetworkProfileStore::with_max_slots(2);
        store.set(1, profile("wss://a"));
        store.set(2, profile("wss://b"));

        // Full, but this is a replacement, not a new slot: refusing it would leave the charge
        // point holding a profile the CSMS believes it has replaced.
        assert!(store.set(2, profile("wss://c")));
        assert_eq!(store.len(), 2);
        assert_eq!(store.get(2).map(|p| p.csms_url.as_str()), Some("wss://c"));
    }

    #[test]
    fn a_new_slot_beyond_the_bound_is_refused_and_changes_nothing() {
        let mut store = NetworkProfileStore::with_max_slots(1);
        store.set(1, profile("wss://a"));

        assert!(!store.set(2, profile("wss://b")));
        assert_eq!(store.len(), 1);
        assert_eq!(store.get(1).map(|p| p.csms_url.as_str()), Some("wss://a"));
    }

    #[test]
    fn slots_are_kept_in_ascending_order_however_they_arrive() {
        // The order is the CSMS's own priority vocabulary, and `NetworkConfigurationPriority`
        // reports it - so it must not depend on the order slots happened to be written in.
        let mut store = NetworkProfileStore::with_max_slots(4);
        store.set(3, profile("wss://c"));
        store.set(1, profile("wss://a"));
        store.set(2, profile("wss://b"));

        let numbers: Vec<i32> = store.slots().iter().map(|slot| slot.slot).collect();
        assert_eq!(numbers, alloc::vec![1, 2, 3]);
    }

    #[test]
    fn restoring_more_slots_than_the_bound_keeps_the_lowest_numbered() {
        let mut store = NetworkProfileStore::with_max_slots(2);

        let dropped = store.replace(alloc::vec![
            NetworkProfileSlot {
                slot: 3,
                profile: profile("wss://c"),
            },
            NetworkProfileSlot {
                slot: 1,
                profile: profile("wss://a"),
            },
            NetworkProfileSlot {
                slot: 2,
                profile: profile("wss://b"),
            },
        ]);

        assert_eq!(dropped, 1);
        let numbers: Vec<i32> = store.slots().iter().map(|slot| slot.slot).collect();
        assert_eq!(numbers, alloc::vec![1, 2]);
    }
}
