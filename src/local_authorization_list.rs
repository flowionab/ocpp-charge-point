//! Local authorization list functional block: CSMS-initiated `SendLocalList`/
//! `GetLocalListVersion`. See `docs/ROADMAP.md` §4.

use crate::actor::ChargePointActor;
use crate::state::{ChargePointEvent, IdToken, LocalListEntry};
use alloc::boxed::Box;
use alloc::vec::Vec;

/// One requested change in a differential `SendLocalList` update: add/replace an entry, or
/// remove an id token from the list (OCPP `AuthorizationData` with no `idTokenInfo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalListChange {
    Upsert(LocalListEntry),
    Remove(IdToken),
}

/// A `SendLocalList` update, already resolved from the wire's `updateType`/
/// `localAuthorizationList` pairing into one of these two shapes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalListUpdate {
    /// Replaces the entire list outright.
    Full(Vec<LocalListEntry>),
    /// Applies each change to the existing list, in order.
    Differential(Vec<LocalListChange>),
}

/// The outcome of a CSMS-initiated `SendLocalList` request, matching (a subset of) OCPP's
/// `SendLocalListStatusEnum`. `Failed` isn't reachable - the list is a plain in-memory `Vec`, so
/// nothing about applying an update (once the version check passes) can fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendLocalListOutcome {
    Accepted,
    /// A differential update whose `version` doesn't immediately follow the charge point's
    /// current version - only a full update may (re)set an arbitrary version.
    VersionMismatch,
}

/// Handles a CSMS-initiated `SendLocalList` request against `actor`. A `Full` update always
/// succeeds, replacing the list outright and adopting `version` regardless of the current one. A
/// `Differential` update only succeeds if `version` is exactly the charge point's current
/// version + 1 - anything else means an earlier update was missed (or the CSMS is out of sync),
/// and the differential can't be safely applied on top of an unknown base, so it's rejected with
/// `VersionMismatch` rather than silently corrupting the list.
pub async fn handle_send_local_list(
    actor: &ChargePointActor,
    version: i64,
    update: LocalListUpdate,
) -> SendLocalListOutcome {
    let current = actor.state().local_authorization_list;

    let entries = match update {
        LocalListUpdate::Full(entries) => entries,
        LocalListUpdate::Differential(changes) => {
            if version != current.version + 1 {
                return SendLocalListOutcome::VersionMismatch;
            }
            apply_differential(current.entries, changes)
        }
    };

    let _ = actor
        .send(ChargePointEvent::LocalListUpdated { version, entries })
        .await;

    SendLocalListOutcome::Accepted
}

/// Applies each [`LocalListChange`] to `entries` in order: `Upsert` replaces an existing entry
/// for the same id token, or appends a new one; `Remove` drops the matching entry, if any.
fn apply_differential(
    mut entries: Vec<LocalListEntry>,
    changes: Vec<LocalListChange>,
) -> Vec<LocalListEntry> {
    for change in changes {
        match change {
            LocalListChange::Upsert(entry) => {
                match entries
                    .iter_mut()
                    .find(|existing| existing.id_token == entry.id_token)
                {
                    Some(existing) => *existing = entry,
                    None => entries.push(entry),
                }
            }
            LocalListChange::Remove(id_token) => {
                entries.retain(|existing| existing.id_token != id_token);
            }
        }
    }
    entries
}

/// Registers this charge point's inbound `SendLocalList` handling with the CSMS connection.
/// Implemented per protocol version (see the `ocpp_2_1` module), mirroring
/// [`crate::reservation::ReserveNowHandler`].
#[async_trait::async_trait]
pub trait SendLocalListHandler {
    async fn register_send_local_list_handler(&self, actor: ChargePointActor);
}

/// Reads the charge point's current local authorization list version (OCPP
/// `GetLocalListVersion`) - purely a state read, unlike every other handler in this crate it
/// needs no actor round trip.
pub fn handle_get_local_list_version(actor: &ChargePointActor) -> i64 {
    actor.state().local_authorization_list.version
}

/// Registers this charge point's inbound `GetLocalListVersion` handling with the CSMS
/// connection. Implemented per protocol version (see the `ocpp_2_1` module), mirroring
/// [`SendLocalListHandler`].
#[async_trait::async_trait]
pub trait GetLocalListVersionHandler {
    async fn register_get_local_list_version_handler(&self, actor: ChargePointActor);
}

#[cfg(test)]
mod tests {
    use super::{
        handle_get_local_list_version, handle_send_local_list, LocalListChange, LocalListUpdate,
        SendLocalListOutcome,
    };
    use crate::actor::ChargePointActor;
    use crate::executor::TokioExecutor;
    use crate::state::{AuthorizationStatus, IdToken, IdTokenKind, LocalListEntry};
    use alloc::vec;

    fn id_token(value: &str) -> IdToken {
        IdToken {
            value: value.into(),
            kind: IdTokenKind::ISO14443,
        }
    }

    fn entry(value: &str, status: AuthorizationStatus) -> LocalListEntry {
        LocalListEntry {
            id_token: id_token(value),
            status,
        }
    }

    #[tokio::test]
    async fn a_full_update_replaces_the_list_and_reports_the_new_version() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let outcome = handle_send_local_list(
            &actor,
            5,
            LocalListUpdate::Full(vec![entry("A", AuthorizationStatus::Accepted)]),
        )
        .await;

        assert_eq!(outcome, SendLocalListOutcome::Accepted);
        assert_eq!(handle_get_local_list_version(&actor), 5);
        assert_eq!(
            actor.state().local_authorization_list.entries,
            vec![entry("A", AuthorizationStatus::Accepted)]
        );
    }

    #[tokio::test]
    async fn a_full_update_may_jump_to_any_version() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let outcome = handle_send_local_list(&actor, 42, LocalListUpdate::Full(vec![])).await;

        assert_eq!(outcome, SendLocalListOutcome::Accepted);
        assert_eq!(handle_get_local_list_version(&actor), 42);
    }

    #[tokio::test]
    async fn a_sequential_differential_update_upserts_and_removes_entries() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        handle_send_local_list(
            &actor,
            1,
            LocalListUpdate::Full(vec![
                entry("A", AuthorizationStatus::Accepted),
                entry("B", AuthorizationStatus::Accepted),
            ]),
        )
        .await;

        let outcome = handle_send_local_list(
            &actor,
            2,
            LocalListUpdate::Differential(vec![
                LocalListChange::Upsert(entry("A", AuthorizationStatus::Rejected)),
                LocalListChange::Remove(id_token("B")),
                LocalListChange::Upsert(entry("C", AuthorizationStatus::Accepted)),
            ]),
        )
        .await;

        assert_eq!(outcome, SendLocalListOutcome::Accepted);
        assert_eq!(handle_get_local_list_version(&actor), 2);
        assert_eq!(
            actor.state().local_authorization_list.entries,
            vec![
                entry("A", AuthorizationStatus::Rejected),
                entry("C", AuthorizationStatus::Accepted),
            ]
        );
    }

    #[tokio::test]
    async fn a_non_sequential_differential_update_is_a_version_mismatch() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        handle_send_local_list(&actor, 1, LocalListUpdate::Full(vec![])).await;

        let outcome = handle_send_local_list(
            &actor,
            5,
            LocalListUpdate::Differential(vec![LocalListChange::Upsert(entry(
                "A",
                AuthorizationStatus::Accepted,
            ))]),
        )
        .await;

        assert_eq!(outcome, SendLocalListOutcome::VersionMismatch);
        // The list is untouched on a rejected update.
        assert_eq!(handle_get_local_list_version(&actor), 1);
    }

    #[tokio::test]
    async fn the_initial_version_is_zero() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        assert_eq!(handle_get_local_list_version(&actor), 0);
    }
}

#[cfg(feature = "ocpp_2_1")]
mod ocpp_2_1 {
    use super::{
        handle_get_local_list_version, handle_send_local_list, GetLocalListVersionHandler,
        LocalListChange, LocalListUpdate, SendLocalListHandler, SendLocalListOutcome,
    };
    use crate::actor::ChargePointActor;
    use crate::state::{AuthorizationStatus, IdToken, IdTokenKind, LocalListEntry};
    use alloc::boxed::Box;
    use alloc::string::ToString;
    use ocpp_client::ocpp_2_1::OCPP2_1Client;
    use ocpp_client::ocpp_types::v21::common::{
        AuthorizationData, AuthorizationStatusEnum, SendLocalListStatusEnum, UpdateEnum,
    };
    use ocpp_client::ocpp_types::v21::{
        GetLocalListVersionResponse, SendLocalListRequest, SendLocalListResponse,
    };

    fn map_id_token_kind(kind: &str) -> IdTokenKind {
        match kind {
            "Central" => IdTokenKind::Central,
            "DirectPayment" => IdTokenKind::DirectPayment,
            "eMAID" => IdTokenKind::EMAID,
            "EVCCID" => IdTokenKind::EVCCID,
            "ISO14443" => IdTokenKind::ISO14443,
            "ISO15693" => IdTokenKind::ISO15693,
            "KeyCode" => IdTokenKind::KeyCode,
            "Local" => IdTokenKind::Local,
            "MacAddress" => IdTokenKind::MacAddress,
            "NoAuthorization" => IdTokenKind::NoAuthorization,
            _ => IdTokenKind::Vin,
        }
    }

    fn map_id_token(id_token: &ocpp_client::ocpp_types::v21::common::IdToken) -> IdToken {
        IdToken {
            value: id_token.id_token.to_string(),
            kind: map_id_token_kind(id_token.r#type.as_str()),
        }
    }

    /// Only `Accepted` maps to our `Accepted` - see [`crate::authorization::ocpp_2_1::map_status`]
    /// for why the wire enum's other values all collapse to `Rejected`.
    fn map_authorization_status(status: AuthorizationStatusEnum) -> AuthorizationStatus {
        match status {
            AuthorizationStatusEnum::Accepted => AuthorizationStatus::Accepted,
            _ => AuthorizationStatus::Rejected,
        }
    }

    /// `None` if `data` has no `idTokenInfo` - meaningful for a differential update (remove this
    /// id token), meaningless for a full update (nothing to authorize with, so the entry is
    /// dropped from the resulting list either way).
    fn map_entry(data: &AuthorizationData) -> Option<LocalListEntry> {
        data.id_token_info.as_ref().map(|info| LocalListEntry {
            id_token: map_id_token(&data.id_token),
            status: map_authorization_status(info.status.clone()),
        })
    }

    fn parse_update(request: &SendLocalListRequest) -> LocalListUpdate {
        let list = request.local_authorization_list.as_deref().unwrap_or(&[]);
        match request.update_type {
            UpdateEnum::Full => LocalListUpdate::Full(list.iter().filter_map(map_entry).collect()),
            UpdateEnum::Differential => LocalListUpdate::Differential(
                list.iter()
                    .map(|data| match map_entry(data) {
                        Some(entry) => LocalListChange::Upsert(entry),
                        None => LocalListChange::Remove(map_id_token(&data.id_token)),
                    })
                    .collect(),
            ),
        }
    }

    pub(super) fn map_outcome(outcome: SendLocalListOutcome) -> SendLocalListStatusEnum {
        match outcome {
            SendLocalListOutcome::Accepted => SendLocalListStatusEnum::Accepted,
            SendLocalListOutcome::VersionMismatch => SendLocalListStatusEnum::VersionMismatch,
        }
    }

    #[async_trait::async_trait]
    impl SendLocalListHandler for OCPP2_1Client {
        async fn register_send_local_list_handler(&self, actor: ChargePointActor) {
            self.on_send_local_list(move |request, _client| {
                let actor = actor.clone();
                async move {
                    let update = parse_update(&request);
                    let outcome =
                        handle_send_local_list(&actor, request.version_number, update).await;
                    Ok(SendLocalListResponse {
                        custom_data: None,
                        status: map_outcome(outcome),
                        status_info: None,
                    })
                }
            })
            .await;
        }
    }

    #[async_trait::async_trait]
    impl GetLocalListVersionHandler for OCPP2_1Client {
        async fn register_get_local_list_version_handler(&self, actor: ChargePointActor) {
            self.on_get_local_list_version(move |_request, _client| {
                let actor = actor.clone();
                async move {
                    Ok(GetLocalListVersionResponse {
                        custom_data: None,
                        version_number: handle_get_local_list_version(&actor),
                    })
                }
            })
            .await;
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn every_outcome_maps_to_the_matching_wire_status() {
            assert_eq!(
                map_outcome(SendLocalListOutcome::Accepted),
                SendLocalListStatusEnum::Accepted
            );
            assert_eq!(
                map_outcome(SendLocalListOutcome::VersionMismatch),
                SendLocalListStatusEnum::VersionMismatch
            );
        }

        fn wire_id_token() -> ocpp_client::ocpp_types::v21::common::IdToken {
            ocpp_client::ocpp_types::v21::common::IdToken {
                additional_info: None,
                id_token: heapless::String::try_from("04A224B2").unwrap(),
                r#type: heapless::String::try_from("ISO14443").unwrap(),
                custom_data: None,
            }
        }

        fn wire_id_token_info() -> ocpp_client::ocpp_types::v21::common::IdTokenInfo {
            ocpp_client::ocpp_types::v21::common::IdTokenInfo {
                cache_expiry_date_time: None,
                charging_priority: None,
                custom_data: None,
                evse_id: None,
                group_id_token: None,
                language1: None,
                language2: None,
                personal_message: None,
                status: AuthorizationStatusEnum::Accepted,
            }
        }

        #[test]
        fn an_entry_with_id_token_info_maps_to_a_local_list_entry() {
            let data = AuthorizationData {
                custom_data: None,
                id_token: wire_id_token(),
                id_token_info: Some(wire_id_token_info()),
            };

            let mapped = map_entry(&data).unwrap();

            assert_eq!(mapped.id_token.value, "04A224B2");
            assert_eq!(mapped.status, AuthorizationStatus::Accepted);
        }

        #[test]
        fn an_entry_without_id_token_info_has_no_local_list_entry() {
            let data = AuthorizationData {
                custom_data: None,
                id_token: wire_id_token(),
                id_token_info: None,
            };

            assert_eq!(map_entry(&data), None);
        }

        fn request(update_type: UpdateEnum, id_token_info: Option<()>) -> SendLocalListRequest {
            SendLocalListRequest {
                custom_data: None,
                local_authorization_list: Some(alloc::vec![AuthorizationData {
                    custom_data: None,
                    id_token: wire_id_token(),
                    id_token_info: id_token_info.map(|_| wire_id_token_info()),
                }]),
                update_type,
                version_number: 1,
            }
        }

        #[test]
        fn a_full_update_with_id_token_info_parses_to_a_full_update() {
            let update = parse_update(&request(UpdateEnum::Full, Some(())));

            assert!(matches!(update, LocalListUpdate::Full(entries) if entries.len() == 1));
        }

        #[test]
        fn a_full_update_without_id_token_info_drops_the_entry() {
            let update = parse_update(&request(UpdateEnum::Full, None));

            assert!(matches!(update, LocalListUpdate::Full(entries) if entries.is_empty()));
        }

        #[test]
        fn a_differential_update_without_id_token_info_is_a_removal() {
            let update = parse_update(&request(UpdateEnum::Differential, None));

            assert!(matches!(
                update,
                LocalListUpdate::Differential(changes)
                    if matches!(changes.as_slice(), [LocalListChange::Remove(_)])
            ));
        }

        #[test]
        fn a_differential_update_with_id_token_info_is_an_upsert() {
            let update = parse_update(&request(UpdateEnum::Differential, Some(())));

            assert!(matches!(
                update,
                LocalListUpdate::Differential(changes)
                    if matches!(changes.as_slice(), [LocalListChange::Upsert(_)])
            ));
        }
    }
}
