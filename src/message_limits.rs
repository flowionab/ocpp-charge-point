//! Per-message size limits: refusing a request that exceeds what this charge point declared it
//! can take (`docs/OCPP-2.1-COMPLIANCE-ROADMAP.md` CV2.8).
//!
//! # What OCPP asks for
//!
//! Several blocks let a charge point publish a ceiling on the requests it will accept, as a pair
//! of device-model variables: `ItemsPerMessage` (how many elements the request's list may hold)
//! and `BytesPerMessage` (how large the whole payload may be). The CSMS is required not to exceed
//! them — B05.FR.11, B08.FR.06, D01.FR.11, N04.FR.09 — and the charge point *may* refuse one that
//! does, with a CALLERROR: `OccurrenceConstraintViolation` for too many items (B06.FR.16,
//! B08.FR.17) and `FormatViolation` for too many bytes (B06.FR.17, B08.FR.18).
//!
//! This crate takes the MAY. The alternative is to attempt the request anyway, which on an MCU
//! with tens of kilobytes of RAM means the failure mode is an allocation that does not come back —
//! and an unexplained reset is a far worse answer to the CSMS than an error code naming the
//! variable it should have read. Declaring a bound and then not enforcing it also invites a CSMS
//! to discover the real limit empirically, which is the opposite of what publishing it was for.
//!
//! # What each of the two limits can actually promise
//!
//! The item count is exact: it is the length of the list the request carried.
//!
//! **The byte count is a measurement of the re-encoded payload, not of the frame that arrived.**
//! By the time a handler runs, `ocpp-client` has already decoded the JSON into a typed request;
//! the original text is gone, and this crate is not permitted to duplicate transport handling to
//! keep it (see `CLAUDE.md`). Re-serializing measures the same content without the CSMS's
//! whitespace, key ordering or the OCPP-J call envelope, so it reads **at or below** what was
//! actually received. That is the safe direction for a limit: the error is always "we accepted
//! something marginally over", never "we refused something that fitted". A frame large enough to
//! be a real threat is caught long before this, at the transport ceiling
//! ([`crate::payload_limit`]) — this layer exists to give the CSMS a *reason*, which that one
//! structurally cannot.
//!
//! # Zero means unlimited
//!
//! A limit of `0` — or an absent, unregistered or unparseable variable — disables that half of the
//! check, the same reading every other numeric device-model variable in this crate gives `0`. A
//! literal zero-item ceiling would refuse every request the block exists to serve.

use crate::actor::ChargePointActor;
use crate::state::{Component, Variable, VariableAttributeType};
use alloc::string::String;

/// The size ceiling a component declares for one message kind — see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MessageSizeLimits {
    /// `ItemsPerMessage`: the most list elements one request may carry. `None` is unlimited.
    pub items: Option<usize>,
    /// `BytesPerMessage`: the largest payload one request may be. `None` is unlimited.
    pub bytes: Option<usize>,
}

/// Reads `component`'s `ItemsPerMessage`/`BytesPerMessage` for the message kind named by
/// `instance` (`None` for the components that declare one ceiling for the whole block, e.g.
/// `LocalAuthListCtrlr`).
///
/// Read fresh per request rather than cached, so the ceiling a CSMS sees via `GetVariables` and the
/// ceiling this charge point enforces are always the same number.
pub fn message_size_limits(
    actor: &ChargePointActor,
    component: &str,
    instance: Option<&str>,
) -> MessageSizeLimits {
    let state = actor.state();
    let read = |variable: &str| {
        state
            .device_model
            .get(
                &Component {
                    name: component.into(),
                    instance: None,
                    evse: None,
                },
                &Variable {
                    name: variable.into(),
                    instance: instance.map(Into::into),
                },
            )
            .and_then(|definition| definition.attribute(VariableAttributeType::Actual))
            .and_then(|attribute| attribute.value.parse::<usize>().ok())
            .filter(|limit| *limit != 0)
    };
    MessageSizeLimits {
        items: read("ItemsPerMessage"),
        bytes: read("BytesPerMessage"),
    }
}

/// Which ceiling a request broke, and by how much.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitViolation {
    /// More list elements than `ItemsPerMessage` allows → `OccurrenceConstraintViolation`.
    TooManyItems {
        /// How many elements the request carried.
        count: usize,
        /// The declared ceiling.
        limit: usize,
    },
    /// A larger payload than `BytesPerMessage` allows → `FormatViolation`.
    TooManyBytes {
        /// How many bytes the re-encoded payload measured — see the module docs on what this
        /// does and does not count.
        bytes: usize,
        /// The declared ceiling.
        limit: usize,
    },
}

impl LimitViolation {
    /// The CALLERROR description this violation is reported with: names the variable the CSMS
    /// should have read, and both numbers, so the operator does not have to reproduce it to find
    /// out how far over the request was.
    pub fn description(self, action: &str) -> String {
        match self {
            Self::TooManyItems { count, limit } => alloc::format!(
                "{action} carried {count} items, more than the {limit} this charge point declares \
                 in ItemsPerMessage"
            ),
            Self::TooManyBytes { bytes, limit } => alloc::format!(
                "{action} measured {bytes} bytes, more than the {limit} this charge point declares \
                 in BytesPerMessage"
            ),
        }
    }
}

/// Checks `item_count` and `request`'s encoded size against what `component`/`instance` declares.
///
/// The item ceiling is checked first: it is the exact one, so where a request breaks both, the
/// CSMS is told the thing it can act on without guessing. A payload that cannot be re-encoded at
/// all is treated as within the byte limit rather than refused — it decoded successfully, so the
/// only thing a serializer failure here proves is that this crate could not measure it, which is
/// not the CSMS's fault.
pub fn check_message_size<T: serde::Serialize>(
    actor: &ChargePointActor,
    component: &str,
    instance: Option<&str>,
    item_count: usize,
    request: &T,
) -> Result<(), LimitViolation> {
    let limits = message_size_limits(actor, component, instance);
    if let Some(limit) = limits.items
        && item_count > limit
    {
        return Err(LimitViolation::TooManyItems {
            count: item_count,
            limit,
        });
    }
    if let Some(limit) = limits.bytes
        && let Ok(encoded) = serde_json::to_vec(request)
        && encoded.len() > limit
    {
        return Err(LimitViolation::TooManyBytes {
            bytes: encoded.len(),
            limit,
        });
    }
    Ok(())
}

/// Logs a refusal at `warn!` — degraded but handled: the charge point kept working and the
/// operator needs to know why a CSMS request is bouncing.
///
/// Gated on the versions that have a CALLERROR to carry it, so a `--no-default-features` build
/// with no protocol version compiled in does not carry a function nothing can reach.
#[cfg(any(feature = "ocpp_2_1", feature = "ocpp_2_0_1"))]
fn warn_refused(action: &str, violation: LimitViolation) {
    match violation {
        LimitViolation::TooManyItems { count, limit } => tracing::warn!(
            action,
            count,
            limit,
            "refusing a request with more items than ItemsPerMessage allows"
        ),
        LimitViolation::TooManyBytes { bytes, limit } => tracing::warn!(
            action,
            bytes,
            limit,
            "refusing a request larger than BytesPerMessage allows"
        ),
    }
}

/// The OCPP 2.1 CALLERROR for `violation`: `OccurrenceConstraintViolation` for items,
/// `FormatViolation` for bytes, exactly as B06.FR.16/.17 and B08.FR.17/.18 name them.
#[cfg(feature = "ocpp_2_1")]
pub fn ocpp_2_1_too_large(
    action: &str,
    violation: LimitViolation,
) -> ocpp_client::ocpp_2_1::OCPP2_1Error {
    use crate::wire::v21::RpcErrorCode;
    warn_refused(action, violation);
    ocpp_client::ocpp_2_1::OCPP2_1Error {
        code: match violation {
            LimitViolation::TooManyItems { .. } => RpcErrorCode::OccurrenceConstraintViolation,
            LimitViolation::TooManyBytes { .. } => RpcErrorCode::FormatViolation,
        },
        description: violation.description(action),
        details: Default::default(),
    }
}

/// The OCPP 2.0.1 CALLERROR for `violation`. Mirrors [`ocpp_2_1_too_large`].
#[cfg(feature = "ocpp_2_0_1")]
pub fn ocpp_2_0_1_too_large(
    action: &str,
    violation: LimitViolation,
) -> ocpp_client::ocpp_2_0_1::OCPP2_0_1Error {
    use crate::wire::v201::RpcErrorCode;
    warn_refused(action, violation);
    ocpp_client::ocpp_2_0_1::OCPP2_0_1Error {
        code: match violation {
            LimitViolation::TooManyItems { .. } => RpcErrorCode::OccurrenceConstraintViolation,
            LimitViolation::TooManyBytes { .. } => RpcErrorCode::FormatViolation,
        },
        description: violation.description(action),
        details: Default::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::TokioExecutor;
    use crate::state::{ChargePointEvent, DeviceModelEvent};

    /// Writes an `ItemsPerMessage`/`BytesPerMessage` variable the way a hardware binding or a
    /// restore would - these are `ReadOnly` to a CSMS by construction.
    async fn set_limit(
        actor: &ChargePointActor,
        component: &str,
        instance: Option<&str>,
        variable: &str,
        value: &str,
    ) {
        actor
            .send(ChargePointEvent::DeviceModel(
                DeviceModelEvent::AttributeValueSet {
                    component: Component {
                        name: component.into(),
                        instance: None,
                        evse: None,
                    },
                    variable: Variable {
                        name: variable.into(),
                        instance: instance.map(Into::into),
                    },
                    attribute_type: VariableAttributeType::Actual,
                    value: value.into(),
                },
            ))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn the_registered_device_data_limits_are_what_gets_enforced() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        // The figures `DEFAULT_VARIABLES` publishes, which is the whole point of publishing them.
        assert_eq!(
            message_size_limits(&actor, "DeviceDataCtrlr", Some("GetVariables")),
            MessageSizeLimits {
                items: Some(50),
                bytes: Some(8_192),
            }
        );
        assert_eq!(
            message_size_limits(&actor, "DeviceDataCtrlr", Some("GetReport")).items,
            Some(16),
            "GetReport chunks at 16, and says so"
        );
    }

    #[tokio::test]
    async fn an_unregistered_component_declares_no_limit_at_all() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        assert_eq!(
            message_size_limits(&actor, "NoSuchCtrlr", None),
            MessageSizeLimits::default()
        );
    }

    #[tokio::test]
    async fn zero_means_unlimited_rather_than_refuse_everything() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        set_limit(
            &actor,
            "DeviceDataCtrlr",
            Some("GetVariables"),
            "ItemsPerMessage",
            "0",
        )
        .await;

        assert_eq!(
            message_size_limits(&actor, "DeviceDataCtrlr", Some("GetVariables")).items,
            None
        );
        assert!(
            check_message_size(
                &actor,
                "DeviceDataCtrlr",
                Some("GetVariables"),
                10_000,
                &"payload"
            )
            .is_ok()
        );
    }

    #[tokio::test]
    async fn too_many_items_is_refused_before_the_byte_count_is_even_measured() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        set_limit(
            &actor,
            "DeviceDataCtrlr",
            Some("SetVariables"),
            "ItemsPerMessage",
            "2",
        )
        .await;
        set_limit(
            &actor,
            "DeviceDataCtrlr",
            Some("SetVariables"),
            "BytesPerMessage",
            "1",
        )
        .await;

        // Over both ceilings; the exact one is the one reported.
        assert_eq!(
            check_message_size(
                &actor,
                "DeviceDataCtrlr",
                Some("SetVariables"),
                3,
                &"a long payload"
            ),
            Err(LimitViolation::TooManyItems { count: 3, limit: 2 })
        );
    }

    #[tokio::test]
    async fn an_oversized_payload_within_the_item_count_is_a_byte_violation() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        set_limit(
            &actor,
            "DeviceDataCtrlr",
            Some("SetVariables"),
            "BytesPerMessage",
            "8",
        )
        .await;

        let violation = check_message_size(
            &actor,
            "DeviceDataCtrlr",
            Some("SetVariables"),
            1,
            &"more than eight bytes once encoded",
        )
        .expect_err("the payload is over the byte ceiling");
        assert!(matches!(
            violation,
            LimitViolation::TooManyBytes { limit: 8, .. }
        ));
    }

    /// The description is what an operator reads in a CSMS log, so it has to carry both numbers
    /// and name the variable that produced the refusal.
    #[test]
    fn a_refusal_names_the_variable_and_both_numbers() {
        let description = LimitViolation::TooManyItems {
            count: 99,
            limit: 50,
        }
        .description("GetVariables");
        assert!(description.contains("GetVariables"));
        assert!(description.contains("99"));
        assert!(description.contains("50"));
        assert!(description.contains("ItemsPerMessage"));
    }

    #[cfg(feature = "ocpp_2_1")]
    #[test]
    fn each_violation_maps_to_the_error_code_ocpp_names_for_it() {
        use crate::wire::v21::RpcErrorCode;

        assert_eq!(
            ocpp_2_1_too_large(
                "GetVariables",
                LimitViolation::TooManyItems {
                    count: 99,
                    limit: 50
                }
            )
            .code,
            RpcErrorCode::OccurrenceConstraintViolation,
            "B06.FR.16"
        );
        assert_eq!(
            ocpp_2_1_too_large(
                "GetVariables",
                LimitViolation::TooManyBytes {
                    bytes: 9_000,
                    limit: 8_192
                }
            )
            .code,
            RpcErrorCode::FormatViolation,
            "B06.FR.17"
        );
    }
}
