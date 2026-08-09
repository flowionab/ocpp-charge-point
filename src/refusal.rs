//! Runtime refusal discipline for capability-gated handlers - see
//! `docs/PRODUCTION-ROADMAP.md` §5.5 (C5).
//!
//! [`crate::hardware::capabilities::CAPABILITY_GATES`] (C3) already makes *advertisement*
//! consistent: when a capability's Cargo feature is compiled out, `setup()`/
//! [`crate::builder::ChargePointBuilder`] skip registering that block's handler entirely, so
//! `ocpp-client` answers a generic `NotImplemented` CALLERROR for the whole message. This module
//! covers the complementary case: the handler *is* registered (its Cargo feature is on) but the
//! runtime [`Capabilities`] flag for it is off. In that case the handler must not let a generic
//! error leak out - it must refuse in the protocol-correct shape, because a `NotImplemented`
//! CALLERROR where a `Rejected`/`NotSupported` status was required is a certification failure
//! per OCPP's conformance rules.
//!
//! # Decision table
//!
//! Only capabilities with a real registered handler today
//! ([`crate::hardware::capabilities::CapabilityGate::has_handler`] - `reservation`,
//! `local_auth_list`, `tariff_and_cost`) can actually be runtime-absent while still being
//! registered, so those are the only rows with a real capability check wired up
//! ([`REFUSAL_GATES`]). The other messages [`docs/PRODUCTION-ROADMAP.md`] §5.5 asks this table to
//! cover aren't gated by any [`Capabilities`] field in this crate today (`UnlockConnector`,
//! `RequestStartTransaction`, `RequestStopTransaction`, `ChangeAvailability`, `GetVariables`,
//! `SetVariables`, `GetBaseReport`, `GetReport`, `Reset`, `DataTransfer`, `Authorize`) - they're
//! either core (always present) or gated by hardware facts (e.g. `can_unlock_under_load`) that
//! change *how* the handler answers, not *whether* it exists - so they're recorded here as N/A
//! with the shape the response schema would require *if* a capability ever did gate them, so a
//! future capability addition has a documented target to implement against.
//!
//! Verified against the generated Rust response types in `ocpp-types` 0.1.3
//! (`~/.cargo/registry/src/*/ocpp-types-0.1.3/src/{v16,v201,v21}/*_response.rs` and each
//! version's `common.rs` status enums) and cross-checked against `docs/OCPP-2.1` /
//! `docs/OCPP-2.0.1`. Rule of thumb: if the response schema has a status field that can say no,
//! refuse through it; if the response schema has no status field at all, no CALLRESULT can say
//! no, so refusal must be a CALLERROR (`RpcErrorCode::NotSupported`).
//!
//! | Message                    | 1.6J                                   | 2.0.1                                    | 2.1                                      |
//! |-----------------------------|-----------------------------------------|-------------------------------------------|--------------------------------------------|
//! | `UnlockConnector`           | CALLRESULT `UnlockConnectorResponseStatus::NotSupported` (N/A today - not capability-gated) | CALLRESULT `UnlockStatusEnum::UnlockFailed` (no `NotSupported`; N/A today) | CALLRESULT `UnlockStatusEnum::UnlockFailed` (no `NotSupported`; N/A today) |
//! | `RequestStartTransaction`   | n/a (1.6 uses `RemoteStartTransaction`, `RemoteStartTransactionResponseStatus`: `Accepted`/`Rejected` - CALLRESULT `Rejected`; N/A today | CALLRESULT `RequestStartStopStatusEnum::Rejected` (N/A today) | CALLRESULT `RequestStartStopStatusEnum::Rejected` (N/A today) |
//! | `RequestStopTransaction`    | n/a (`RemoteStopTransaction`, same 2-value enum) - CALLRESULT `Rejected`; N/A today | CALLRESULT `RequestStartStopStatusEnum::Rejected` (N/A today) | CALLRESULT `RequestStartStopStatusEnum::Rejected` (N/A today) |
//! | `ChangeAvailability`        | CALLRESULT `ChangeAvailabilityResponseStatus::Rejected` (N/A today) | CALLRESULT `ChangeAvailabilityStatusEnum::Rejected` (N/A today) | CALLRESULT `ChangeAvailabilityStatusEnum::Rejected` (N/A today) |
//! | `ReserveNow`                | CALLRESULT `ReserveNowResponseStatus::Rejected` | CALLRESULT `ReserveNowStatusEnum::Rejected` | CALLRESULT `ReserveNowStatusEnum::Rejected` |
//! | `CancelReservation`         | CALLRESULT `CancelReservationResponseStatus::Rejected` | CALLRESULT `CancelReservationStatusEnum::Rejected` | CALLRESULT `CancelReservationStatusEnum::Rejected` |
//! | `SendLocalList`              | CALLRESULT `SendLocalListResponseStatus::NotSupported` | CALLRESULT `SendLocalListStatusEnum::Failed` (no `NotSupported` in 2.x) | CALLRESULT `SendLocalListStatusEnum::Failed` (no `NotSupported` in 2.x) |
//! | `GetLocalListVersion`        | CALLERROR (`GetLocalListVersionResponse` is `{ listVersion }`, no status field in any version) | CALLERROR (no status field) | CALLERROR (no status field) |
//! | `CostUpdated`                | n/a (no `CostUpdated` in 1.6 - `tariff_and_cost` has no 1.6 feature profile) | CALLERROR (`CostUpdatedResponse` is `{}`, no status field) | CALLERROR (no status field) |
//! | `SetDefaultTariff`           | n/a (2.1-only - no tariff messages before 2.1) | n/a | CALLRESULT `TariffSetStatusEnum::Rejected` |
//! | `ChangeTransactionTariff`    | n/a | n/a | CALLRESULT `TariffChangeStatusEnum::Rejected` |
//! | `ClearTariffs`               | n/a | n/a | CALLERROR (`ClearTariffsResponse` has only a per-item `clearTariffsResult`, no top-level status) |
//! | `GetTariffs`                 | n/a | n/a | CALLRESULT `TariffGetStatusEnum::Rejected` |
//! | `GetVariables`/`SetVariables`| n/a (device model is 2.x-only) | CALLRESULT `GetVariableStatusEnum::Rejected` / `SetVariableStatusEnum::Rejected` (N/A today - not gated by any single `Capabilities` field) | same as 2.0.1 |
//! | `GetBaseReport`/`GetReport`  | n/a (2.x-only) | CALLRESULT `GenericDeviceModelStatusEnum::NotSupported` (N/A today) | CALLRESULT `GenericDeviceModelStatusEnum::NotSupported` (N/A today) |
//! | `Reset`                      | CALLRESULT `ResetResponseStatus::Rejected` (N/A today - `Reset` is core, always registered) | CALLRESULT `ResetStatusEnum::Rejected` (N/A today) | CALLRESULT `ResetStatusEnum::Rejected` (N/A today) |
//! | `DataTransfer`               | CALLRESULT `DataTransferResponseStatus::UnknownVendorId`/`Rejected` (N/A today - vendor-id routing, not a `Capabilities` field) | CALLRESULT `DataTransferStatusEnum::UnknownVendorId`/`Rejected` (N/A today) | same as 2.0.1 |
//! | `Authorize`                  | CALLRESULT (`AuthorizeResponse.idTagInfo.status`; not "absent capability" - authorization always answers) - N/A, not capability-gated | CALLRESULT `AuthorizationStatusEnum` (N/A, not capability-gated) | same as 2.0.1 |
//! | `SetDisplayMessage`          | n/a (2.x-only) | CALLRESULT `DisplayMessageStatusEnum::Rejected` | CALLRESULT `DisplayMessageStatusEnum::Rejected` |
//! | `ClearDisplayMessage`        | n/a (2.x-only) | CALLRESULT `ClearMessageStatusEnum::Unknown` (no `Rejected` in 2.0.1) | CALLRESULT `ClearMessageStatusEnum::Rejected` |
//! | `GetDisplayMessages`         | n/a (2.x-only) | CALLRESULT `GetDisplayMessagesStatusEnum::Unknown` (no `Rejected`/`NotSupported` in either version - an absent capability answers through the same "nothing matched" status an empty store would, not a separate refusal) | same as 2.0.1 |
//! | `PublishFirmware`            | n/a (2.x-only - no local-controller concept in 1.6J) | CALLRESULT `GenericStatusEnum::Rejected` | CALLRESULT `GenericStatusEnum::Rejected` |
//! | `UnpublishFirmware`          | n/a (2.x-only) | CALLRESULT `UnpublishFirmwareStatusEnum::NoFirmware` (no dedicated "unsupported" value; a local controller that can never publish anything genuinely has no firmware to unpublish) | same as 2.0.1 |
//! | `OpenPeriodicEventStream`    | n/a | n/a | CALLRESULT `GenericStatusEnum::Rejected` |
//! | `AdjustPeriodicEventStream`  | n/a | n/a | CALLRESULT `GenericStatusEnum::Rejected` |
//! | `ClosePeriodicEventStream`   | n/a | n/a | CALLERROR (`ClosePeriodicEventStreamResponse` is `{}`, no status field) |
//! | `GetPeriodicEventStream`     | n/a | n/a | CALLERROR (`GetPeriodicEventStreamResponse` is `{ constantStreamData? }`, no status field) |
//! | `RequestBatterySwap`         | n/a (2.1-only) | n/a | CALLRESULT `GenericStatusEnum::Rejected` |
//! | `GetDERControl`              | n/a (2.1-only) | n/a | CALLRESULT `DERControlStatusEnum::NotSupported` |
//! | `SetDERControl`              | n/a (2.1-only) | n/a | CALLRESULT `DERControlStatusEnum::NotSupported` |
//! | `ClearDERControl`            | n/a (2.1-only) | n/a | CALLRESULT `DERControlStatusEnum::NotSupported` |
//! | `AFRRSignal`                 | n/a (2.1-only) | n/a | CALLRESULT `GenericStatusEnum::Rejected` (no `NotSupported` in this enum) |
//! | `NotifyAllowedEnergyTransfer`| n/a (2.1-only) | n/a | CALLRESULT `NotifyAllowedEnergyTransferStatusEnum::Rejected` (no `NotSupported` in this enum) |
//! | `CertificateSigned`          | n/a (2.x-only) | CALLRESULT `CertificateSignedStatusEnum::Rejected` | same as 2.0.1 |
//!
//! `SignCertificate` (B4.3) has no row here: it is charge-point-initiated (this crate sends it,
//! rather than answering a CSMS call), so there is no inbound message for a runtime-absent
//! capability to refuse - an absent `certificate_management`/`key_storage` capability instead
//! means this crate simply does not attempt to send one.
//!
//! Nothing here needed to fall back on assumption where the vendored spec/generated types didn't
//! settle it - every response type above either has a documented status enum or documented-empty
//! body in the generated `ocpp-types` source.

use crate::hardware::Capabilities;

/// How a message's response schema can express "no" when the [`Capabilities`] flag its handler
/// depends on is runtime-absent. See the module docs for the full decision table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalShape {
    /// The response schema has a status field that can say no - refuse by returning that status
    /// in an otherwise-normal CALLRESULT.
    CallResultStatus,
    /// The response schema has no status field at all - no CALLRESULT can say no, so refusal
    /// must be a CALLERROR (`RpcErrorCode::NotSupported`).
    CallError,
}

/// One row of the single source of truth mapping an OCPP message this crate handles to the
/// [`Capabilities`] flag that gates it and the [`RefusalShape`] refusing it must take. Handlers
/// call [`capability_present`] (for [`RefusalShape::CallResultStatus`] messages, which already
/// have an outcome enum with a spec-appropriate "no" variant to fall back to) or construct a
/// CALLERROR directly via [`ocpp_2_1_not_supported`]/[`ocpp_2_0_1_not_supported`]/
/// [`ocpp_1_6_not_supported`] (for [`RefusalShape::CallError`] messages) - either way, adding a
/// new capability-gated message means adding a row here, not new branching logic per handler.
pub struct RefusalGate {
    /// The OCPP action name, matching `Action::NAME` in `ocpp-client`.
    pub message: &'static str,
    /// Reads the gating capability's current value out of a [`Capabilities`] instance.
    pub capability: fn(&Capabilities) -> bool,
    /// How refusal must be shaped on the wire when `capability` reads `false`.
    pub shape: RefusalShape,
}

/// The messages this crate registers a handler for today whose underlying functional block can
/// genuinely be runtime-absent while still compiled in - see [`RefusalGate`] and the module docs'
/// decision table. Mirrors [`crate::hardware::capabilities::CAPABILITY_GATES`]' `has_handler:
/// true` rows (`reservation`, `local_auth_list`, `tariff_and_cost`, `smart_charging`).
pub const REFUSAL_GATES: &[RefusalGate] = &[
    RefusalGate {
        message: "ReserveNow",
        capability: |c| c.reservation,
        shape: RefusalShape::CallResultStatus,
    },
    RefusalGate {
        message: "CancelReservation",
        capability: |c| c.reservation,
        shape: RefusalShape::CallResultStatus,
    },
    RefusalGate {
        message: "SendLocalList",
        capability: |c| c.local_auth_list,
        shape: RefusalShape::CallResultStatus,
    },
    RefusalGate {
        message: "GetLocalListVersion",
        capability: |c| c.local_auth_list,
        shape: RefusalShape::CallError,
    },
    RefusalGate {
        message: "CostUpdated",
        capability: |c| c.tariff_and_cost,
        shape: RefusalShape::CallError,
    },
    RefusalGate {
        message: "SetChargingProfile",
        capability: |c| c.smart_charging,
        shape: RefusalShape::CallResultStatus,
    },
    RefusalGate {
        message: "ClearChargingProfile",
        capability: |c| c.smart_charging,
        shape: RefusalShape::CallResultStatus,
    },
    RefusalGate {
        message: "GetCompositeSchedule",
        capability: |c| c.smart_charging,
        shape: RefusalShape::CallResultStatus,
    },
    RefusalGate {
        message: "GetChargingProfiles",
        capability: |c| c.smart_charging,
        shape: RefusalShape::CallResultStatus,
    },
    RefusalGate {
        message: "InstallCertificate",
        capability: |c| c.certificate_management,
        shape: RefusalShape::CallResultStatus,
    },
    RefusalGate {
        message: "DeleteCertificate",
        capability: |c| c.certificate_management,
        shape: RefusalShape::CallResultStatus,
    },
    RefusalGate {
        message: "GetInstalledCertificateIds",
        capability: |c| c.certificate_management,
        shape: RefusalShape::CallResultStatus,
    },
    RefusalGate {
        message: "UpdateFirmware",
        capability: |c| c.firmware_management,
        shape: RefusalShape::CallResultStatus,
    },
    RefusalGate {
        message: "GetLog",
        capability: |c| c.diagnostics,
        shape: RefusalShape::CallResultStatus,
    },
    RefusalGate {
        message: "UpdateDynamicSchedule",
        capability: |c| c.smart_charging,
        shape: RefusalShape::CallResultStatus,
    },
    RefusalGate {
        message: "UsePriorityCharging",
        capability: |c| c.smart_charging,
        shape: RefusalShape::CallResultStatus,
    },
    RefusalGate {
        message: "SetDefaultTariff",
        capability: |c| c.tariff_and_cost,
        shape: RefusalShape::CallResultStatus,
    },
    RefusalGate {
        message: "ChangeTransactionTariff",
        capability: |c| c.tariff_and_cost,
        shape: RefusalShape::CallResultStatus,
    },
    RefusalGate {
        message: "ClearTariffs",
        capability: |c| c.tariff_and_cost,
        shape: RefusalShape::CallError,
    },
    RefusalGate {
        message: "GetTariffs",
        capability: |c| c.tariff_and_cost,
        shape: RefusalShape::CallResultStatus,
    },
    RefusalGate {
        message: "SetDisplayMessage",
        capability: |c| c.has_display,
        shape: RefusalShape::CallResultStatus,
    },
    RefusalGate {
        message: "ClearDisplayMessage",
        capability: |c| c.has_display,
        shape: RefusalShape::CallResultStatus,
    },
    RefusalGate {
        message: "PublishFirmware",
        capability: |c| c.firmware_publishing,
        shape: RefusalShape::CallResultStatus,
    },
    RefusalGate {
        message: "UnpublishFirmware",
        capability: |c| c.firmware_publishing,
        shape: RefusalShape::CallResultStatus,
    },
    RefusalGate {
        message: "OpenPeriodicEventStream",
        capability: |c| c.periodic_event_stream,
        shape: RefusalShape::CallResultStatus,
    },
    RefusalGate {
        message: "AdjustPeriodicEventStream",
        capability: |c| c.periodic_event_stream,
        shape: RefusalShape::CallResultStatus,
    },
    RefusalGate {
        message: "ClosePeriodicEventStream",
        capability: |c| c.periodic_event_stream,
        shape: RefusalShape::CallError,
    },
    RefusalGate {
        message: "GetPeriodicEventStream",
        capability: |c| c.periodic_event_stream,
        shape: RefusalShape::CallError,
    },
    RefusalGate {
        message: "RequestBatterySwap",
        capability: |c| c.battery_swap,
        shape: RefusalShape::CallResultStatus,
    },
    RefusalGate {
        message: "GetDERControl",
        capability: |c| c.der_control,
        shape: RefusalShape::CallResultStatus,
    },
    RefusalGate {
        message: "SetDERControl",
        capability: |c| c.der_control,
        shape: RefusalShape::CallResultStatus,
    },
    RefusalGate {
        message: "ClearDERControl",
        capability: |c| c.der_control,
        shape: RefusalShape::CallResultStatus,
    },
    RefusalGate {
        message: "AFRRSignal",
        capability: |c| c.der_control,
        shape: RefusalShape::CallResultStatus,
    },
    RefusalGate {
        message: "NotifyAllowedEnergyTransfer",
        capability: |c| c.der_control,
        shape: RefusalShape::CallResultStatus,
    },
    RefusalGate {
        message: "CertificateSigned",
        capability: |c| c.certificate_management,
        shape: RefusalShape::CallResultStatus,
    },
];

/// Looks up the [`RefusalGate`] for `message` (by `Action::NAME`), if this crate has one.
pub fn gate(message: &str) -> Option<&'static RefusalGate> {
    REFUSAL_GATES.iter().find(|g| g.message == message)
}

/// Whether `message`'s gating capability is present in `capabilities`. `true` (permissive) for
/// any message with no [`RefusalGate`] row - only messages this crate actually knows to be
/// capability-gated can be refused.
pub fn capability_present(capabilities: &Capabilities, message: &str) -> bool {
    gate(message).is_none_or(|g| (g.capability)(capabilities))
}

/// Builds the OCPP 2.1 CALLERROR a [`RefusalShape::CallError`] message must answer with when its
/// gating capability is runtime-absent: `RpcErrorCode::NotSupported`, per the module's decision
/// table.
#[cfg(feature = "ocpp_2_1")]
pub fn ocpp_2_1_not_supported(action: &str) -> ocpp_client::ocpp_2_1::OCPP2_1Error {
    use crate::wire::v21::RpcErrorCode;
    ocpp_client::ocpp_2_1::OCPP2_1Error {
        code: RpcErrorCode::NotSupported,
        description: alloc::format!(
            "{action} is not supported: the underlying capability is not present on this charge point"
        ),
        details: Default::default(),
    }
}

/// Builds the OCPP 2.0.1 CALLERROR a [`RefusalShape::CallError`] message must answer with when
/// its gating capability is runtime-absent. Mirrors [`ocpp_2_1_not_supported`].
#[cfg(feature = "ocpp_2_0_1")]
pub fn ocpp_2_0_1_not_supported(action: &str) -> ocpp_client::ocpp_2_0_1::OCPP2_0_1Error {
    use crate::wire::v201::RpcErrorCode;
    ocpp_client::ocpp_2_0_1::OCPP2_0_1Error {
        code: RpcErrorCode::NotSupported,
        description: alloc::format!(
            "{action} is not supported: the underlying capability is not present on this charge point"
        ),
        details: Default::default(),
    }
}

/// Builds the OCPP 1.6J CALLERROR a [`RefusalShape::CallError`] message must answer with when its
/// gating capability is runtime-absent. Mirrors [`ocpp_2_1_not_supported`].
#[cfg(feature = "ocpp_1_6")]
pub fn ocpp_1_6_not_supported(action: &str) -> ocpp_client::ocpp_1_6::OCPP1_6Error {
    use crate::wire::v16::RpcErrorCode;
    ocpp_client::ocpp_1_6::OCPP1_6Error {
        code: RpcErrorCode::NotSupported,
        description: alloc::format!(
            "{action} is not supported: the underlying capability is not present on this charge point"
        ),
        details: Default::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capabilities_with(f: impl FnOnce(&mut Capabilities)) -> Capabilities {
        let mut c = Capabilities::default();
        f(&mut c);
        c
    }

    #[test]
    fn every_refusal_gate_reflects_its_capability() {
        for gate in REFUSAL_GATES {
            let absent = Capabilities::default();
            assert!(
                !(gate.capability)(&absent),
                "{} should default to absent",
                gate.message
            );
        }
    }

    #[test]
    fn capability_present_is_permissive_for_unknown_messages() {
        let capabilities = Capabilities::default();
        assert!(capability_present(&capabilities, "SomeUnrelatedMessage"));
    }

    #[test]
    fn capability_present_reflects_reservation() {
        let absent = Capabilities::default();
        let present = capabilities_with(|c| c.reservation = true);

        assert!(!capability_present(&absent, "ReserveNow"));
        assert!(!capability_present(&absent, "CancelReservation"));
        assert!(capability_present(&present, "ReserveNow"));
        assert!(capability_present(&present, "CancelReservation"));
    }

    #[test]
    fn capability_present_reflects_local_auth_list() {
        let absent = Capabilities::default();
        let present = capabilities_with(|c| c.local_auth_list = true);

        assert!(!capability_present(&absent, "SendLocalList"));
        assert!(!capability_present(&absent, "GetLocalListVersion"));
        assert!(capability_present(&present, "SendLocalList"));
        assert!(capability_present(&present, "GetLocalListVersion"));
    }

    #[test]
    fn capability_present_reflects_firmware_publishing() {
        let absent = Capabilities::default();
        let present = capabilities_with(|c| c.firmware_publishing = true);

        assert!(!capability_present(&absent, "PublishFirmware"));
        assert!(!capability_present(&absent, "UnpublishFirmware"));
        assert!(capability_present(&present, "PublishFirmware"));
        assert!(capability_present(&present, "UnpublishFirmware"));
    }

    #[test]
    fn capability_present_reflects_der_control() {
        let absent = Capabilities::default();
        let present = capabilities_with(|c| c.der_control = true);

        assert!(!capability_present(&absent, "GetDERControl"));
        assert!(!capability_present(&absent, "SetDERControl"));
        assert!(!capability_present(&absent, "ClearDERControl"));
        assert!(!capability_present(&absent, "AFRRSignal"));
        assert!(!capability_present(&absent, "NotifyAllowedEnergyTransfer"));
        assert!(capability_present(&present, "GetDERControl"));
        assert!(capability_present(&present, "SetDERControl"));
        assert!(capability_present(&present, "ClearDERControl"));
        assert!(capability_present(&present, "AFRRSignal"));
        assert!(capability_present(&present, "NotifyAllowedEnergyTransfer"));
    }

    #[test]
    fn capability_present_reflects_tariff_and_cost() {
        let absent = Capabilities::default();
        let present = capabilities_with(|c| c.tariff_and_cost = true);

        assert!(!capability_present(&absent, "CostUpdated"));
        assert!(capability_present(&present, "CostUpdated"));
    }

    #[test]
    fn capability_present_reflects_periodic_event_stream() {
        let absent = Capabilities::default();
        let present = capabilities_with(|c| c.periodic_event_stream = true);

        assert!(!capability_present(&absent, "OpenPeriodicEventStream"));
        assert!(!capability_present(&absent, "AdjustPeriodicEventStream"));
        assert!(!capability_present(&absent, "ClosePeriodicEventStream"));
        assert!(!capability_present(&absent, "GetPeriodicEventStream"));
        assert!(capability_present(&present, "OpenPeriodicEventStream"));
        assert!(capability_present(&present, "AdjustPeriodicEventStream"));
        assert!(capability_present(&present, "ClosePeriodicEventStream"));
        assert!(capability_present(&present, "GetPeriodicEventStream"));
    }

    #[cfg(feature = "ocpp_2_1")]
    #[test]
    fn ocpp_2_1_not_supported_uses_the_not_supported_rpc_error_code() {
        use crate::wire::v21::RpcErrorCode;
        let error = ocpp_2_1_not_supported("GetLocalListVersion");
        assert_eq!(error.code, RpcErrorCode::NotSupported);
    }

    #[cfg(feature = "ocpp_2_0_1")]
    #[test]
    fn ocpp_2_0_1_not_supported_uses_the_not_supported_rpc_error_code() {
        use crate::wire::v201::RpcErrorCode;
        let error = ocpp_2_0_1_not_supported("GetLocalListVersion");
        assert_eq!(error.code, RpcErrorCode::NotSupported);
    }

    #[cfg(feature = "ocpp_1_6")]
    #[test]
    fn ocpp_1_6_not_supported_uses_the_not_supported_rpc_error_code() {
        use crate::wire::v16::RpcErrorCode;
        let error = ocpp_1_6_not_supported("GetLocalListVersion");
        assert_eq!(error.code, RpcErrorCode::NotSupported);
    }
}
