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
    /// Add a new entry, or replace the existing one for the same id token.
    Upsert(LocalListEntry),
    /// Remove the entry for this id token, if any.
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
/// `SendLocalListStatusEnum`. Wire `Failed` is reached two ways: directly by
/// [`TooManyEntries`](Self::TooManyEntries), and - because 2.x's `SendLocalListStatusEnum` has no
/// `NotSupported` variant - by [`NotSupported`](Self::NotSupported) under 2.0.1/2.1 (1.6J has a real
/// `NotSupported` wire value and uses it). See `crate::refusal`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendLocalListOutcome {
    /// The update was applied.
    Accepted,
    /// A differential update whose `version` doesn't immediately follow the charge point's
    /// current version - only a full update may (re)set an arbitrary version.
    VersionMismatch,
    /// The resulting list would hold more entries than
    /// [`crate::state::StateLimits::max_local_authorization_list_entries`] allows, so the update
    /// was refused outright and the current list left untouched - see
    /// [`handle_send_local_list`] and `docs/PRODUCTION-ROADMAP.md` §9.2 (G2.2). Maps to wire
    /// `Failed` in every protocol version.
    TooManyEntries,
    /// The local authorization list capability is registered (the `local-auth-list` Cargo
    /// feature is on) but runtime-absent (`Capabilities::local_auth_list` is `false`) - see
    /// `crate::refusal` (docs/PRODUCTION-ROADMAP.md §5.5, C5).
    NotSupported,
}

/// Handles a CSMS-initiated `SendLocalList` request against `actor`. A `Full` update replaces the
/// list outright and adopts `version` regardless of the current one. A
/// `Differential` update only succeeds if `version` is exactly the charge point's current
/// version + 1 - anything else means an earlier update was missed (or the CSMS is out of sync),
/// and the differential can't be safely applied on top of an unknown base, so it's rejected with
/// `VersionMismatch` rather than silently corrupting the list.
///
/// Either kind is refused with [`SendLocalListOutcome::TooManyEntries`] if the resulting list would
/// exceed [`crate::state::StateLimits::max_local_authorization_list_entries`] (G2.2,
/// `docs/PRODUCTION-ROADMAP.md` §9.2) - refused *outright*, leaving the current list exactly as it
/// was, rather than applied up to the bound: a partially applied list would reject id tokens the
/// CSMS believes are cached, which is worse for a driver at an offline charge point than a `Failed`
/// the CSMS can see and act on. A differential update that only replaces or removes entries stays
/// within the bound and is accepted even on a full list.
pub async fn handle_send_local_list(
    actor: &ChargePointActor,
    version: i64,
    update: LocalListUpdate,
) -> SendLocalListOutcome {
    let state = actor.state();
    // C5 (docs/PRODUCTION-ROADMAP.md §5.5): the handler is registered whenever the
    // `local-auth-list` Cargo feature is on, but the hardware may still declare the capability
    // runtime-absent.
    if !crate::refusal::capability_present(&state.capabilities, "SendLocalList") {
        return SendLocalListOutcome::NotSupported;
    }
    let current = state.local_authorization_list;

    let max_entries = current.max_entries;
    let entries = match update {
        LocalListUpdate::Full(entries) => entries,
        LocalListUpdate::Differential(changes) => {
            if version != current.version + 1 {
                return SendLocalListOutcome::VersionMismatch;
            }
            apply_differential(current.entries, changes)
        }
    };

    // G2.2 (docs/PRODUCTION-ROADMAP.md §9.2): the list is bounded, so refuse an update that
    // wouldn't fit rather than letting the state machine truncate it - see this function's docs
    // for why a partially applied list is the worse failure.
    if entries.len() > max_entries {
        tracing::warn!(
            requested = entries.len(),
            max_entries,
            "refusing a SendLocalList update larger than the configured maximum"
        );
        return SendLocalListOutcome::TooManyEntries;
    }

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
    /// Registers a `SendLocalList` handler with the CSMS connection that dispatches incoming
    /// requests to [`handle_send_local_list`] against `actor`.
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
    /// Registers a `GetLocalListVersion` handler with the CSMS connection that dispatches
    /// incoming requests to [`handle_get_local_list_version`] against `actor`.
    async fn register_get_local_list_version_handler(&self, actor: ChargePointActor);
}

#[cfg(test)]
mod tests {
    use super::{
        LocalListChange, LocalListUpdate, SendLocalListOutcome, handle_get_local_list_version,
        handle_send_local_list,
    };
    use crate::actor::ChargePointActor;
    use crate::executor::TokioExecutor;
    use crate::hardware::Capabilities;
    use crate::state::{
        AuthorizationStatus, ChargePointEvent, IdToken, IdTokenKind, LocalListEntry,
    };
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

    /// Spawns an actor with the `local_auth_list` capability declared present - mirrors
    /// `crate::reservation::tests::spawn_with_reservation`.
    async fn spawn_with_local_auth_list<const N: usize>(evses: [usize; N]) -> ChargePointActor {
        spawn_with_local_auth_list_limited(evses, crate::state::StateLimits::default()).await
    }

    /// [`spawn_with_local_auth_list`] with caller-chosen [`crate::state::StateLimits`], for the
    /// bounded-list tests (G2.2, docs/PRODUCTION-ROADMAP.md §9.2).
    async fn spawn_with_local_auth_list_limited<const N: usize>(
        evses: [usize; N],
        limits: crate::state::StateLimits,
    ) -> ChargePointActor {
        let actor = ChargePointActor::spawn_with_limits(evses, &TokioExecutor, limits);
        actor
            .send(ChargePointEvent::CapabilitiesDeclared(Capabilities {
                local_auth_list: true,
                ..Default::default()
            }))
            .await
            .unwrap();
        actor
    }

    #[tokio::test]
    async fn a_full_update_replaces_the_list_and_reports_the_new_version() {
        let actor = spawn_with_local_auth_list([1]).await;

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
        let actor = spawn_with_local_auth_list([1]).await;

        let outcome = handle_send_local_list(&actor, 42, LocalListUpdate::Full(vec![])).await;

        assert_eq!(outcome, SendLocalListOutcome::Accepted);
        assert_eq!(handle_get_local_list_version(&actor), 42);
    }

    #[tokio::test]
    async fn a_sequential_differential_update_upserts_and_removes_entries() {
        let actor = spawn_with_local_auth_list([1]).await;
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
        let actor = spawn_with_local_auth_list([1]).await;
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

    // C5 (docs/PRODUCTION-ROADMAP.md §5.5): with `local_auth_list` runtime-absent (the default),
    // `SendLocalList` must refuse via `NotSupported` rather than silently applying the update.
    #[tokio::test]
    async fn send_local_list_is_not_supported_when_the_capability_is_absent() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let outcome = handle_send_local_list(
            &actor,
            5,
            LocalListUpdate::Full(vec![entry("A", AuthorizationStatus::Accepted)]),
        )
        .await;

        assert_eq!(outcome, SendLocalListOutcome::NotSupported);
        assert_eq!(
            actor.state().local_authorization_list.entries,
            vec![],
            "a refused update must not mutate the list"
        );
    }

    // G2.2 (docs/PRODUCTION-ROADMAP.md §9.2): the list is bounded, and an update that would
    // exceed the bound is refused outright rather than applied in part - a silently truncated
    // authorization list would reject id tokens the CSMS believes are cached.
    #[tokio::test]
    async fn a_full_update_beyond_the_maximum_is_refused_and_leaves_the_list_untouched() {
        let actor = spawn_with_local_auth_list_limited(
            [1],
            crate::state::StateLimits::default().with_max_local_authorization_list_entries(2),
        )
        .await;
        handle_send_local_list(
            &actor,
            1,
            LocalListUpdate::Full(vec![entry("A", AuthorizationStatus::Accepted)]),
        )
        .await;

        let outcome = handle_send_local_list(
            &actor,
            2,
            LocalListUpdate::Full(vec![
                entry("B", AuthorizationStatus::Accepted),
                entry("C", AuthorizationStatus::Accepted),
                entry("D", AuthorizationStatus::Accepted),
            ]),
        )
        .await;

        assert_eq!(outcome, SendLocalListOutcome::TooManyEntries);
        assert_eq!(handle_get_local_list_version(&actor), 1);
        assert_eq!(
            actor.state().local_authorization_list.entries,
            vec![entry("A", AuthorizationStatus::Accepted)],
            "a refused update must not mutate the list"
        );
    }

    #[tokio::test]
    async fn a_differential_update_that_would_overflow_the_maximum_is_refused() {
        let actor = spawn_with_local_auth_list_limited(
            [1],
            crate::state::StateLimits::default().with_max_local_authorization_list_entries(2),
        )
        .await;
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
            LocalListUpdate::Differential(vec![LocalListChange::Upsert(entry(
                "C",
                AuthorizationStatus::Accepted,
            ))]),
        )
        .await;

        assert_eq!(outcome, SendLocalListOutcome::TooManyEntries);
        assert_eq!(handle_get_local_list_version(&actor), 1);
        assert_eq!(actor.state().local_authorization_list.entries.len(), 2);
    }

    /// A differential update that stays within the bound is still accepted once the list is full -
    /// replacing an existing entry, or removing one, doesn't grow anything.
    #[tokio::test]
    async fn a_differential_update_on_a_full_list_that_does_not_grow_it_is_accepted() {
        let actor = spawn_with_local_auth_list_limited(
            [1],
            crate::state::StateLimits::default().with_max_local_authorization_list_entries(2),
        )
        .await;
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
        assert_eq!(
            actor.state().local_authorization_list.entries,
            vec![
                entry("A", AuthorizationStatus::Rejected),
                entry("C", AuthorizationStatus::Accepted),
            ]
        );
    }
}

#[cfg(feature = "ocpp_2_1")]
mod ocpp_2_1 {
    use super::{
        GetLocalListVersionHandler, LocalListChange, LocalListUpdate, SendLocalListHandler,
        SendLocalListOutcome, handle_get_local_list_version, handle_send_local_list,
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
            // G2.2 (docs/PRODUCTION-ROADMAP.md §9.2): the list is bounded and this update didn't
            // fit. `Failed` is the only wire value that says so - there is no "too big" status in
            // any protocol version.
            SendLocalListOutcome::TooManyEntries => SendLocalListStatusEnum::Failed,
            // 2.x's `SendLocalListStatusEnum` has no `NotSupported` variant - `Failed` is the
            // closest available "no", per crate::refusal's decision table (docs/PRODUCTION-ROADMAP.md
            // §5.5).
            SendLocalListOutcome::NotSupported => SendLocalListStatusEnum::Failed,
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

    /// The logic behind [`OCPP2_1Client`]'s registered `GetLocalListVersion` handler, factored
    /// out so C5's capability-off CALLERROR can be asserted directly. See
    /// [`super::CostUpdatedHandler`]... - mirrors `crate::cost::ocpp_2_1::handle`.
    fn handle_get_local_list_version_request(
        actor: &ChargePointActor,
    ) -> Result<GetLocalListVersionResponse, ocpp_client::ocpp_2_1::OCPP2_1Error> {
        // C5 (docs/PRODUCTION-ROADMAP.md §5.5): `GetLocalListVersionResponse` has no status
        // field in any protocol version, so a runtime-absent capability can only be refused as
        // a CALLERROR - see `crate::refusal`'s decision table.
        if !crate::refusal::capability_present(&actor.state().capabilities, "GetLocalListVersion") {
            return Err(crate::refusal::ocpp_2_1_not_supported(
                "GetLocalListVersion",
            ));
        }
        Ok(GetLocalListVersionResponse {
            custom_data: None,
            version_number: handle_get_local_list_version(actor),
        })
    }

    #[async_trait::async_trait]
    impl GetLocalListVersionHandler for OCPP2_1Client {
        async fn register_get_local_list_version_handler(&self, actor: ChargePointActor) {
            self.on_get_local_list_version(move |_request, _client| {
                let actor = actor.clone();
                async move { handle_get_local_list_version_request(&actor) }
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
            assert_eq!(
                map_outcome(SendLocalListOutcome::TooManyEntries),
                SendLocalListStatusEnum::Failed
            );
            assert_eq!(
                map_outcome(SendLocalListOutcome::NotSupported),
                SendLocalListStatusEnum::Failed
            );
        }

        // C5 (docs/PRODUCTION-ROADMAP.md §5.5): `GetLocalListVersionResponse` has no status
        // field, so a runtime-absent `local_auth_list` capability must refuse with a
        // `NotSupported` CALLERROR.
        #[tokio::test]
        async fn get_local_list_version_is_a_not_supported_call_error_when_the_capability_is_absent()
         {
            use crate::executor::TokioExecutor;
            use ocpp_client::ocpp_types::v21::RpcErrorCode;

            let actor = crate::actor::ChargePointActor::spawn([1], &TokioExecutor);

            let result = handle_get_local_list_version_request(&actor);

            assert_eq!(result.unwrap_err().code, RpcErrorCode::NotSupported);
        }

        #[tokio::test]
        async fn get_local_list_version_succeeds_when_the_capability_is_present() {
            use crate::executor::TokioExecutor;
            use crate::hardware::Capabilities;

            let actor = crate::actor::ChargePointActor::spawn([1], &TokioExecutor);
            actor
                .send(crate::state::ChargePointEvent::CapabilitiesDeclared(
                    Capabilities {
                        local_auth_list: true,
                        ..Default::default()
                    },
                ))
                .await
                .unwrap();

            let result = handle_get_local_list_version_request(&actor);

            assert_eq!(result.unwrap().version_number, 0);
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

/// The OCPP 2.0.1 projection - identical `SendLocalListRequest`/`GetLocalListVersionResponse`/
/// `AuthorizationData`/`SendLocalListStatusEnum`/`UpdateEnum`/`AuthorizationStatusEnum` wire
/// shapes to 2.1's, so this is close to a copy of the 2.1 module - **except** `id_token` mapping,
/// which for 2.0.1 goes through the same closed `IdTokenEnum`
/// [`crate::remote_control::ocpp_2_0_1::map_id_token_kind`] already had to handle - reused
/// directly here instead of a third copy.
#[cfg(feature = "ocpp_2_0_1")]
mod ocpp_2_0_1 {
    use super::{
        GetLocalListVersionHandler, LocalListChange, LocalListUpdate, SendLocalListHandler,
        SendLocalListOutcome, handle_get_local_list_version, handle_send_local_list,
    };
    use crate::actor::ChargePointActor;
    use crate::remote_control::ocpp_2_0_1::map_id_token_kind;
    use crate::state::{AuthorizationStatus, IdToken, LocalListEntry};
    use alloc::boxed::Box;
    use alloc::string::ToString;
    use ocpp_client::ocpp_2_0_1::OCPP2_0_1Client;
    use ocpp_client::ocpp_types::v201::common::{
        AuthorizationData, AuthorizationStatusEnum, SendLocalListStatusEnum, UpdateEnum,
    };
    use ocpp_client::ocpp_types::v201::{
        GetLocalListVersionResponse, SendLocalListRequest, SendLocalListResponse,
    };

    fn map_id_token(id_token: &ocpp_client::ocpp_types::v201::common::IdToken) -> IdToken {
        IdToken {
            value: id_token.id_token.to_string(),
            kind: map_id_token_kind(id_token.r#type.clone()),
        }
    }

    /// Mirrors [`super::ocpp_2_1::map_authorization_status`].
    fn map_authorization_status(status: AuthorizationStatusEnum) -> AuthorizationStatus {
        match status {
            AuthorizationStatusEnum::Accepted => AuthorizationStatus::Accepted,
            _ => AuthorizationStatus::Rejected,
        }
    }

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
            // G2.2 (docs/PRODUCTION-ROADMAP.md §9.2): the list is bounded and this update didn't
            // fit. `Failed` is the only wire value that says so - there is no "too big" status in
            // any protocol version.
            SendLocalListOutcome::TooManyEntries => SendLocalListStatusEnum::Failed,
            // 2.x's `SendLocalListStatusEnum` has no `NotSupported` variant - `Failed` is the
            // closest available "no", per crate::refusal's decision table (docs/PRODUCTION-ROADMAP.md
            // §5.5).
            SendLocalListOutcome::NotSupported => SendLocalListStatusEnum::Failed,
        }
    }

    #[async_trait::async_trait]
    impl SendLocalListHandler for OCPP2_0_1Client {
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

    /// Mirrors `super::ocpp_2_1::handle_get_local_list_version_request`.
    fn handle_get_local_list_version_request(
        actor: &ChargePointActor,
    ) -> Result<GetLocalListVersionResponse, ocpp_client::ocpp_2_0_1::OCPP2_0_1Error> {
        if !crate::refusal::capability_present(&actor.state().capabilities, "GetLocalListVersion") {
            return Err(crate::refusal::ocpp_2_0_1_not_supported(
                "GetLocalListVersion",
            ));
        }
        Ok(GetLocalListVersionResponse {
            custom_data: None,
            version_number: handle_get_local_list_version(actor),
        })
    }

    #[async_trait::async_trait]
    impl GetLocalListVersionHandler for OCPP2_0_1Client {
        async fn register_get_local_list_version_handler(&self, actor: ChargePointActor) {
            self.on_get_local_list_version(move |_request, _client| {
                let actor = actor.clone();
                async move { handle_get_local_list_version_request(&actor) }
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
            assert_eq!(
                map_outcome(SendLocalListOutcome::TooManyEntries),
                SendLocalListStatusEnum::Failed
            );
            assert_eq!(
                map_outcome(SendLocalListOutcome::NotSupported),
                SendLocalListStatusEnum::Failed
            );
        }

        #[tokio::test]
        async fn get_local_list_version_is_a_not_supported_call_error_when_the_capability_is_absent()
         {
            use crate::executor::TokioExecutor;
            use ocpp_client::ocpp_types::v201::RpcErrorCode;

            let actor = crate::actor::ChargePointActor::spawn([1], &TokioExecutor);

            let result = handle_get_local_list_version_request(&actor);

            assert_eq!(result.unwrap_err().code, RpcErrorCode::NotSupported);
        }

        #[tokio::test]
        async fn get_local_list_version_succeeds_when_the_capability_is_present() {
            use crate::executor::TokioExecutor;
            use crate::hardware::Capabilities;

            let actor = crate::actor::ChargePointActor::spawn([1], &TokioExecutor);
            actor
                .send(crate::state::ChargePointEvent::CapabilitiesDeclared(
                    Capabilities {
                        local_auth_list: true,
                        ..Default::default()
                    },
                ))
                .await
                .unwrap();

            let result = handle_get_local_list_version_request(&actor);

            assert_eq!(result.unwrap().version_number, 0);
        }

        fn wire_id_token() -> ocpp_client::ocpp_types::v201::common::IdToken {
            ocpp_client::ocpp_types::v201::common::IdToken {
                additional_info: None,
                id_token: heapless::String::try_from("04A224B2").unwrap(),
                r#type: ocpp_client::ocpp_types::v201::common::IdTokenEnum::ISO14443,
                custom_data: None,
            }
        }

        fn wire_id_token_info() -> ocpp_client::ocpp_types::v201::common::IdTokenInfo {
            ocpp_client::ocpp_types::v201::common::IdTokenInfo {
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

/// The OCPP 1.6J projection - no topology needed at all (neither `SendLocalList` nor
/// `GetLocalListVersion` addresses a connector), so both traits are implemented directly on
/// `OCPP1_6Client`, unlike the topology-aware wrappers `crate::remote_control`/`crate::reservation`
/// need for their own 1.6J adapters.
///
/// `SendLocalListRequest.localAuthorizationList` items pair a bare `idTag` with an optional
/// `idTagInfo`, the same "present means Upsert, absent means Remove" shape 2.x's `AuthorizationData`
/// has - just with `idTagInfo.status: IdTagInfoStatus` (5 values) instead of `AuthorizationStatusEnum`
/// (10 values), so [`map_status`] narrows the same way [`crate::authorization::ocpp_1_6::map_status`]
/// does (only `Accepted` maps to `Accepted`). `SendLocalListResponseStatus`'s `Failed` value stays
/// unreachable (unlike 2.x, 1.6J's wire enum has a real `NotSupported` value, so
/// [`SendLocalListOutcome::NotSupported`] maps to it directly - see [`map_outcome`] and
/// `crate::refusal`).
#[cfg(feature = "ocpp_1_6")]
mod ocpp_1_6 {
    use super::{
        GetLocalListVersionHandler, LocalListChange, LocalListUpdate, SendLocalListHandler,
        SendLocalListOutcome, handle_get_local_list_version, handle_send_local_list,
    };
    use crate::actor::ChargePointActor;
    use crate::id_tag::map_id_token;
    use crate::state::{AuthorizationStatus, LocalListEntry};
    use alloc::boxed::Box;
    use ocpp_client::ocpp_1_6::OCPP1_6Client;
    use ocpp_client::ocpp_types::v16::common::{
        IdTagInfoStatus, LocalAuthorizationListItem, SendLocalListResponseStatus, UpdateType,
    };
    use ocpp_client::ocpp_types::v16::{
        GetLocalListVersionResponse, SendLocalListRequest, SendLocalListResponse,
    };

    /// Only `Accepted` maps to our `Accepted` - see [`crate::authorization::ocpp_1_6::map_status`].
    fn map_status(status: IdTagInfoStatus) -> AuthorizationStatus {
        match status {
            IdTagInfoStatus::Accepted => AuthorizationStatus::Accepted,
            _ => AuthorizationStatus::Rejected,
        }
    }

    /// `None` if `item` has no `idTagInfo` - meaningful for a differential update (remove this id
    /// token), meaningless for a full update (nothing to authorize with, so the entry is dropped
    /// from the resulting list either way).
    fn map_entry(item: &LocalAuthorizationListItem) -> Option<LocalListEntry> {
        item.id_tag_info.as_ref().map(|info| LocalListEntry {
            id_token: map_id_token(&item.id_tag),
            status: map_status(info.status.clone()),
        })
    }

    fn parse_update(request: &SendLocalListRequest) -> LocalListUpdate {
        let list = request.local_authorization_list.as_deref().unwrap_or(&[]);
        match request.update_type {
            UpdateType::Full => LocalListUpdate::Full(list.iter().filter_map(map_entry).collect()),
            UpdateType::Differential => LocalListUpdate::Differential(
                list.iter()
                    .map(|item| match map_entry(item) {
                        Some(entry) => LocalListChange::Upsert(entry),
                        None => LocalListChange::Remove(map_id_token(&item.id_tag)),
                    })
                    .collect(),
            ),
        }
    }

    pub(super) fn map_outcome(outcome: SendLocalListOutcome) -> SendLocalListResponseStatus {
        match outcome {
            SendLocalListOutcome::Accepted => SendLocalListResponseStatus::Accepted,
            SendLocalListOutcome::VersionMismatch => SendLocalListResponseStatus::VersionMismatch,
            // See the 2.x `map_outcome`: `Failed` is the only wire value for "the list is bounded
            // and this didn't fit" (G2.2, docs/PRODUCTION-ROADMAP.md §9.2).
            SendLocalListOutcome::TooManyEntries => SendLocalListResponseStatus::Failed,
            SendLocalListOutcome::NotSupported => SendLocalListResponseStatus::NotSupported,
        }
    }

    #[async_trait::async_trait]
    impl SendLocalListHandler for OCPP1_6Client {
        async fn register_send_local_list_handler(&self, actor: ChargePointActor) {
            self.on_send_local_list(move |request, _client| {
                let actor = actor.clone();
                async move {
                    let update = parse_update(&request);
                    let outcome =
                        handle_send_local_list(&actor, request.list_version, update).await;
                    Ok(SendLocalListResponse {
                        status: map_outcome(outcome),
                    })
                }
            })
            .await;
        }
    }

    /// Mirrors `super::ocpp_2_1::handle_get_local_list_version_request` - 1.6J's
    /// `GetLocalListVersionResponse` (`{ listVersion }`) has no status field either, so this too
    /// can only refuse via a CALLERROR.
    fn handle_get_local_list_version_request(
        actor: &ChargePointActor,
    ) -> Result<GetLocalListVersionResponse, ocpp_client::ocpp_1_6::OCPP1_6Error> {
        if !crate::refusal::capability_present(&actor.state().capabilities, "GetLocalListVersion") {
            return Err(crate::refusal::ocpp_1_6_not_supported(
                "GetLocalListVersion",
            ));
        }
        Ok(GetLocalListVersionResponse {
            list_version: handle_get_local_list_version(actor),
        })
    }

    #[async_trait::async_trait]
    impl GetLocalListVersionHandler for OCPP1_6Client {
        async fn register_get_local_list_version_handler(&self, actor: ChargePointActor) {
            self.on_get_local_list_version(move |_request, _client| {
                let actor = actor.clone();
                async move { handle_get_local_list_version_request(&actor) }
            })
            .await;
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn every_reachable_outcome_maps_to_a_wire_status() {
            assert_eq!(
                map_outcome(SendLocalListOutcome::Accepted),
                SendLocalListResponseStatus::Accepted
            );
            assert_eq!(
                map_outcome(SendLocalListOutcome::VersionMismatch),
                SendLocalListResponseStatus::VersionMismatch
            );
            assert_eq!(
                map_outcome(SendLocalListOutcome::TooManyEntries),
                SendLocalListResponseStatus::Failed
            );
            assert_eq!(
                map_outcome(SendLocalListOutcome::NotSupported),
                SendLocalListResponseStatus::NotSupported
            );
        }

        #[tokio::test]
        async fn get_local_list_version_is_a_not_supported_call_error_when_the_capability_is_absent()
         {
            use crate::executor::TokioExecutor;
            use ocpp_client::ocpp_types::v16::RpcErrorCode;

            let actor = crate::actor::ChargePointActor::spawn([1], &TokioExecutor);

            let result = handle_get_local_list_version_request(&actor);

            assert_eq!(result.unwrap_err().code, RpcErrorCode::NotSupported);
        }

        #[tokio::test]
        async fn get_local_list_version_succeeds_when_the_capability_is_present() {
            use crate::executor::TokioExecutor;
            use crate::hardware::Capabilities;

            let actor = crate::actor::ChargePointActor::spawn([1], &TokioExecutor);
            actor
                .send(crate::state::ChargePointEvent::CapabilitiesDeclared(
                    Capabilities {
                        local_auth_list: true,
                        ..Default::default()
                    },
                ))
                .await
                .unwrap();

            let result = handle_get_local_list_version_request(&actor);

            assert_eq!(result.unwrap().list_version, 0);
        }

        fn id_tag_info(status: IdTagInfoStatus) -> ocpp_client::ocpp_types::v16::common::IdTagInfo {
            ocpp_client::ocpp_types::v16::common::IdTagInfo {
                expiry_date: None,
                parent_id_tag: None,
                status,
            }
        }

        fn item(id_tag_info: Option<IdTagInfoStatus>) -> LocalAuthorizationListItem {
            LocalAuthorizationListItem {
                id_tag: ocpp_client::ocpp_types::v16::IdTag::try_from("04A224B2").unwrap(),
                id_tag_info: id_tag_info.map(self::id_tag_info),
            }
        }

        #[test]
        fn an_item_with_id_tag_info_maps_to_a_local_list_entry() {
            let mapped = map_entry(&item(Some(IdTagInfoStatus::Accepted))).unwrap();

            assert_eq!(mapped.id_token.value, "04A224B2");
            assert_eq!(mapped.status, AuthorizationStatus::Accepted);
        }

        #[test]
        fn an_item_without_id_tag_info_has_no_local_list_entry() {
            assert_eq!(map_entry(&item(None)), None);
        }

        fn request(
            update_type: UpdateType,
            id_tag_info: Option<IdTagInfoStatus>,
        ) -> SendLocalListRequest {
            SendLocalListRequest {
                list_version: 1,
                local_authorization_list: Some(alloc::vec![item(id_tag_info)]),
                update_type,
            }
        }

        #[test]
        fn a_full_update_with_id_tag_info_parses_to_a_full_update() {
            let update = parse_update(&request(UpdateType::Full, Some(IdTagInfoStatus::Accepted)));

            assert!(matches!(update, LocalListUpdate::Full(entries) if entries.len() == 1));
        }

        #[test]
        fn a_full_update_without_id_tag_info_drops_the_entry() {
            let update = parse_update(&request(UpdateType::Full, None));

            assert!(matches!(update, LocalListUpdate::Full(entries) if entries.is_empty()));
        }

        #[test]
        fn a_differential_update_without_id_tag_info_is_a_removal() {
            let update = parse_update(&request(UpdateType::Differential, None));

            assert!(matches!(
                update,
                LocalListUpdate::Differential(changes)
                    if matches!(changes.as_slice(), [LocalListChange::Remove(_)])
            ));
        }

        #[test]
        fn a_differential_update_with_id_tag_info_is_an_upsert() {
            let update = parse_update(&request(
                UpdateType::Differential,
                Some(IdTagInfoStatus::Accepted),
            ));

            assert!(matches!(
                update,
                LocalListUpdate::Differential(changes)
                    if matches!(changes.as_slice(), [LocalListChange::Upsert(_)])
            ));
        }

        #[test]
        fn ocpp1_6_client_implements_the_handler_traits() {
            fn assert_impl<T: SendLocalListHandler + GetLocalListVersionHandler>() {}
            assert_impl::<OCPP1_6Client>();
        }
    }
}
