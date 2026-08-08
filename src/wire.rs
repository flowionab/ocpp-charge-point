//! `CustomData`-bound aliases for the `ocpp-types` wire types.
//!
//! `ocpp-types` 0.2.0 made every OCPP 2.x message and common struct generic over the
//! `customData` payload type, defaulting to [`ocpp_client::ocpp_types::NoCustomData`].
//! `ocpp-client`'s generated `send_*`/`on_*`/`wait_for_*` methods, however, are concrete at
//! the specification's `CustomData` - so a bare `AuthorizeRequest` written anywhere in this
//! crate means `AuthorizeRequest<NoCustomData>` and does not match what the client expects.
//!
//! Rather than annotate every construction, handler closure and helper signature, this module
//! re-exports each version's namespace with the generic types shadowed by aliases already bound
//! to `CustomData`. Adapters import from `crate::wire::v21` instead of
//! `ocpp_client::ocpp_types::v21` and are otherwise unchanged; the alias also restores type
//! inference for `let request = AuthorizeRequest { .. }`, which a defaulted type parameter does
//! not provide.
//!
//! Deliberately `pub(crate)`: re-exporting it would put `ocpp-types` back into this crate's
//! public API and hand integrators a protocol concern, which `CLAUDE.md` assigns to this crate
//! rather than to them.
//!
//! Generated against `ocpp-types` 0.2.0 - regenerate when that dependency moves.
//!
//! OCPP 1.6 has no `customData` concept, so [`v16`] is a plain re-export kept for symmetry, so
//! that all three version adapters import their wire types the same way.

#![allow(unused_imports)]
#![allow(dead_code)]
// The alias for a generated type has to be spelled exactly as `ocpp-types` spells it, or importing
// it by name from here stops working. `APN`, `EVSE` and `VPN` are how the OCPP schemas name those
// structures, so renaming them to `Apn`/`Evse`/`Vpn` to satisfy the lint would make this module
// stop being a drop-in replacement for the upstream namespace.
#![allow(clippy::upper_case_acronyms)]

/// The validated `dateTime`/`date`/`time` scalars, which are shared across all three versions
/// rather than living under one of them. Re-exported here so an adapter needs a single import
/// path for its wire types.
///
/// Convert to and from [`chrono::DateTime<Utc>`] with `From`/`Into` (this crate enables
/// `ocpp-client`'s `chrono` feature); the round trip preserves nanoseconds.
pub(crate) use ocpp_client::ocpp_types::{OcppDate, OcppTimeOfDay, OcppTimestamp};

/// OCPP 2.1 wire types, with `customData` bound to
/// [`ocpp_client::ocpp_types::v21::common::CustomData`].
pub(crate) mod v21 {
    pub(crate) use ocpp_client::ocpp_types::v21::*;
    pub(crate) type AFRRSignalRequest = ocpp_client::ocpp_types::v21::AFRRSignalRequest<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type AFRRSignalResponse = ocpp_client::ocpp_types::v21::AFRRSignalResponse<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type AdjustPeriodicEventStreamRequest =
        ocpp_client::ocpp_types::v21::AdjustPeriodicEventStreamRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type AdjustPeriodicEventStreamResponse =
        ocpp_client::ocpp_types::v21::AdjustPeriodicEventStreamResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type AuthorizeRequest = ocpp_client::ocpp_types::v21::AuthorizeRequest<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type AuthorizeResponse = ocpp_client::ocpp_types::v21::AuthorizeResponse<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type BatterySwapRequest = ocpp_client::ocpp_types::v21::BatterySwapRequest<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type BatterySwapResponse = ocpp_client::ocpp_types::v21::BatterySwapResponse<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type BootNotificationRequest = ocpp_client::ocpp_types::v21::BootNotificationRequest<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type BootNotificationResponse =
        ocpp_client::ocpp_types::v21::BootNotificationResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type CancelReservationRequest =
        ocpp_client::ocpp_types::v21::CancelReservationRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type CancelReservationResponse =
        ocpp_client::ocpp_types::v21::CancelReservationResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type CertificateSignedRequest =
        ocpp_client::ocpp_types::v21::CertificateSignedRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type CertificateSignedResponse =
        ocpp_client::ocpp_types::v21::CertificateSignedResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type ChangeAvailabilityRequest =
        ocpp_client::ocpp_types::v21::ChangeAvailabilityRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type ChangeAvailabilityResponse =
        ocpp_client::ocpp_types::v21::ChangeAvailabilityResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type ChangeTransactionTariffRequest =
        ocpp_client::ocpp_types::v21::ChangeTransactionTariffRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type ChangeTransactionTariffResponse =
        ocpp_client::ocpp_types::v21::ChangeTransactionTariffResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type ClearCacheRequest = ocpp_client::ocpp_types::v21::ClearCacheRequest<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type ClearCacheResponse = ocpp_client::ocpp_types::v21::ClearCacheResponse<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type ClearChargingProfileRequest =
        ocpp_client::ocpp_types::v21::ClearChargingProfileRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type ClearChargingProfileResponse =
        ocpp_client::ocpp_types::v21::ClearChargingProfileResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type ClearDERControlRequest = ocpp_client::ocpp_types::v21::ClearDERControlRequest<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type ClearDERControlResponse = ocpp_client::ocpp_types::v21::ClearDERControlResponse<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type ClearDisplayMessageRequest =
        ocpp_client::ocpp_types::v21::ClearDisplayMessageRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type ClearDisplayMessageResponse =
        ocpp_client::ocpp_types::v21::ClearDisplayMessageResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type ClearTariffsRequest = ocpp_client::ocpp_types::v21::ClearTariffsRequest<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type ClearTariffsResponse = ocpp_client::ocpp_types::v21::ClearTariffsResponse<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type ClearVariableMonitoringRequest =
        ocpp_client::ocpp_types::v21::ClearVariableMonitoringRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type ClearVariableMonitoringResponse =
        ocpp_client::ocpp_types::v21::ClearVariableMonitoringResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type ClearedChargingLimitRequest =
        ocpp_client::ocpp_types::v21::ClearedChargingLimitRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type ClearedChargingLimitResponse =
        ocpp_client::ocpp_types::v21::ClearedChargingLimitResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type ClosePeriodicEventStreamRequest =
        ocpp_client::ocpp_types::v21::ClosePeriodicEventStreamRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type ClosePeriodicEventStreamResponse =
        ocpp_client::ocpp_types::v21::ClosePeriodicEventStreamResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type CostUpdatedRequest = ocpp_client::ocpp_types::v21::CostUpdatedRequest<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type CostUpdatedResponse = ocpp_client::ocpp_types::v21::CostUpdatedResponse<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type CustomerInformationRequest =
        ocpp_client::ocpp_types::v21::CustomerInformationRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type CustomerInformationResponse =
        ocpp_client::ocpp_types::v21::CustomerInformationResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    // `DataTransfer` is the one action whose *payload* is also a type parameter, and it
    // is declared before `CustomDataType`:
    //
    //     pub struct DataTransferRequest<DataTransferRequestData = (), CustomDataType = NoCustomData>
    //
    // `ocpp-client` 0.4.0's action macro appends the customData type positionally
    // (`$req<C>`, src/ocpp_2_1/actions.rs), so for this one action it lands on the wrong
    // parameter: `send_data_transfer` takes `DataTransferRequest<CustomData, NoCustomData>`,
    // i.e. `data: Option<CustomData>` and `custom_data: Option<NoCustomData>` - the
    // free-form vendor payload typed as the `{vendorId}` object, and the real `customData`
    // discarded. That is an upstream defect (see `docs/UPSTREAM-GAPS.md`), not a choice
    // this crate can make differently: these aliases have to name what the client's
    // signatures actually are or nothing type-checks.
    //
    // Harmless here today - `crate::data_transfer` sends `data: None`/`custom_data: None`
    // on both, and both skip-serialize when `None`, so the wire bytes are unchanged.
    pub(crate) type DataTransferRequest = ocpp_client::ocpp_types::v21::DataTransferRequest<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type DataTransferResponse = ocpp_client::ocpp_types::v21::DataTransferResponse<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type DeleteCertificateRequest =
        ocpp_client::ocpp_types::v21::DeleteCertificateRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type DeleteCertificateResponse =
        ocpp_client::ocpp_types::v21::DeleteCertificateResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type FirmwareStatusNotificationRequest =
        ocpp_client::ocpp_types::v21::FirmwareStatusNotificationRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type FirmwareStatusNotificationResponse =
        ocpp_client::ocpp_types::v21::FirmwareStatusNotificationResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type Get15118EVCertificateRequest =
        ocpp_client::ocpp_types::v21::Get15118EVCertificateRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type Get15118EVCertificateResponse =
        ocpp_client::ocpp_types::v21::Get15118EVCertificateResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type GetBaseReportRequest = ocpp_client::ocpp_types::v21::GetBaseReportRequest<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type GetBaseReportResponse = ocpp_client::ocpp_types::v21::GetBaseReportResponse<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type GetCertificateChainStatusRequest =
        ocpp_client::ocpp_types::v21::GetCertificateChainStatusRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type GetCertificateChainStatusResponse =
        ocpp_client::ocpp_types::v21::GetCertificateChainStatusResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type GetCertificateStatusRequest =
        ocpp_client::ocpp_types::v21::GetCertificateStatusRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type GetCertificateStatusResponse =
        ocpp_client::ocpp_types::v21::GetCertificateStatusResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type GetChargingProfilesRequest =
        ocpp_client::ocpp_types::v21::GetChargingProfilesRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type GetChargingProfilesResponse =
        ocpp_client::ocpp_types::v21::GetChargingProfilesResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type GetCompositeScheduleRequest =
        ocpp_client::ocpp_types::v21::GetCompositeScheduleRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type GetCompositeScheduleResponse =
        ocpp_client::ocpp_types::v21::GetCompositeScheduleResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type GetDERControlRequest = ocpp_client::ocpp_types::v21::GetDERControlRequest<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type GetDERControlResponse = ocpp_client::ocpp_types::v21::GetDERControlResponse<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type GetDisplayMessagesRequest =
        ocpp_client::ocpp_types::v21::GetDisplayMessagesRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type GetDisplayMessagesResponse =
        ocpp_client::ocpp_types::v21::GetDisplayMessagesResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type GetInstalledCertificateIdsRequest =
        ocpp_client::ocpp_types::v21::GetInstalledCertificateIdsRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type GetInstalledCertificateIdsResponse =
        ocpp_client::ocpp_types::v21::GetInstalledCertificateIdsResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type GetLocalListVersionRequest =
        ocpp_client::ocpp_types::v21::GetLocalListVersionRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type GetLocalListVersionResponse =
        ocpp_client::ocpp_types::v21::GetLocalListVersionResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type GetLogRequest = ocpp_client::ocpp_types::v21::GetLogRequest<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type GetLogResponse = ocpp_client::ocpp_types::v21::GetLogResponse<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type GetMonitoringReportRequest =
        ocpp_client::ocpp_types::v21::GetMonitoringReportRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type GetMonitoringReportResponse =
        ocpp_client::ocpp_types::v21::GetMonitoringReportResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type GetPeriodicEventStreamRequest =
        ocpp_client::ocpp_types::v21::GetPeriodicEventStreamRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type GetPeriodicEventStreamResponse =
        ocpp_client::ocpp_types::v21::GetPeriodicEventStreamResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type GetReportRequest = ocpp_client::ocpp_types::v21::GetReportRequest<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type GetReportResponse = ocpp_client::ocpp_types::v21::GetReportResponse<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type GetTariffsRequest = ocpp_client::ocpp_types::v21::GetTariffsRequest<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type GetTariffsResponse = ocpp_client::ocpp_types::v21::GetTariffsResponse<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type GetTransactionStatusRequest =
        ocpp_client::ocpp_types::v21::GetTransactionStatusRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type GetTransactionStatusResponse =
        ocpp_client::ocpp_types::v21::GetTransactionStatusResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type GetVariablesRequest = ocpp_client::ocpp_types::v21::GetVariablesRequest<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type GetVariablesResponse = ocpp_client::ocpp_types::v21::GetVariablesResponse<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type HeartbeatRequest = ocpp_client::ocpp_types::v21::HeartbeatRequest<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type HeartbeatResponse = ocpp_client::ocpp_types::v21::HeartbeatResponse<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type InstallCertificateRequest =
        ocpp_client::ocpp_types::v21::InstallCertificateRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type InstallCertificateResponse =
        ocpp_client::ocpp_types::v21::InstallCertificateResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type LogStatusNotificationRequest =
        ocpp_client::ocpp_types::v21::LogStatusNotificationRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type LogStatusNotificationResponse =
        ocpp_client::ocpp_types::v21::LogStatusNotificationResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type MeterValuesRequest = ocpp_client::ocpp_types::v21::MeterValuesRequest<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type MeterValuesResponse = ocpp_client::ocpp_types::v21::MeterValuesResponse<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type NotifyAllowedEnergyTransferRequest =
        ocpp_client::ocpp_types::v21::NotifyAllowedEnergyTransferRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type NotifyAllowedEnergyTransferResponse =
        ocpp_client::ocpp_types::v21::NotifyAllowedEnergyTransferResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type NotifyChargingLimitRequest =
        ocpp_client::ocpp_types::v21::NotifyChargingLimitRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type NotifyChargingLimitResponse =
        ocpp_client::ocpp_types::v21::NotifyChargingLimitResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type NotifyCustomerInformationRequest =
        ocpp_client::ocpp_types::v21::NotifyCustomerInformationRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type NotifyCustomerInformationResponse =
        ocpp_client::ocpp_types::v21::NotifyCustomerInformationResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type NotifyDERAlarmRequest = ocpp_client::ocpp_types::v21::NotifyDERAlarmRequest<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type NotifyDERAlarmResponse = ocpp_client::ocpp_types::v21::NotifyDERAlarmResponse<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type NotifyDERStartStopRequest =
        ocpp_client::ocpp_types::v21::NotifyDERStartStopRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type NotifyDERStartStopResponse =
        ocpp_client::ocpp_types::v21::NotifyDERStartStopResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type NotifyDisplayMessagesRequest =
        ocpp_client::ocpp_types::v21::NotifyDisplayMessagesRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type NotifyDisplayMessagesResponse =
        ocpp_client::ocpp_types::v21::NotifyDisplayMessagesResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type NotifyEVChargingNeedsRequest =
        ocpp_client::ocpp_types::v21::NotifyEVChargingNeedsRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type NotifyEVChargingNeedsResponse =
        ocpp_client::ocpp_types::v21::NotifyEVChargingNeedsResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type NotifyEVChargingScheduleRequest =
        ocpp_client::ocpp_types::v21::NotifyEVChargingScheduleRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type NotifyEVChargingScheduleResponse =
        ocpp_client::ocpp_types::v21::NotifyEVChargingScheduleResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type NotifyEventRequest = ocpp_client::ocpp_types::v21::NotifyEventRequest<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type NotifyEventResponse = ocpp_client::ocpp_types::v21::NotifyEventResponse<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type NotifyMonitoringReportRequest =
        ocpp_client::ocpp_types::v21::NotifyMonitoringReportRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type NotifyMonitoringReportResponse =
        ocpp_client::ocpp_types::v21::NotifyMonitoringReportResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type NotifyPeriodicEventStream =
        ocpp_client::ocpp_types::v21::NotifyPeriodicEventStream<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type NotifyPriorityChargingRequest =
        ocpp_client::ocpp_types::v21::NotifyPriorityChargingRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type NotifyPriorityChargingResponse =
        ocpp_client::ocpp_types::v21::NotifyPriorityChargingResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type NotifyReportRequest = ocpp_client::ocpp_types::v21::NotifyReportRequest<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type NotifyReportResponse = ocpp_client::ocpp_types::v21::NotifyReportResponse<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type NotifySettlementRequest = ocpp_client::ocpp_types::v21::NotifySettlementRequest<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type NotifySettlementResponse =
        ocpp_client::ocpp_types::v21::NotifySettlementResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type NotifyWebPaymentStartedRequest =
        ocpp_client::ocpp_types::v21::NotifyWebPaymentStartedRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type NotifyWebPaymentStartedResponse =
        ocpp_client::ocpp_types::v21::NotifyWebPaymentStartedResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type OpenPeriodicEventStreamRequest =
        ocpp_client::ocpp_types::v21::OpenPeriodicEventStreamRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type OpenPeriodicEventStreamResponse =
        ocpp_client::ocpp_types::v21::OpenPeriodicEventStreamResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type PublishFirmwareRequest = ocpp_client::ocpp_types::v21::PublishFirmwareRequest<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type PublishFirmwareResponse = ocpp_client::ocpp_types::v21::PublishFirmwareResponse<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type PublishFirmwareStatusNotificationRequest =
        ocpp_client::ocpp_types::v21::PublishFirmwareStatusNotificationRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type PublishFirmwareStatusNotificationResponse =
        ocpp_client::ocpp_types::v21::PublishFirmwareStatusNotificationResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type PullDynamicScheduleUpdateRequest =
        ocpp_client::ocpp_types::v21::PullDynamicScheduleUpdateRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type PullDynamicScheduleUpdateResponse =
        ocpp_client::ocpp_types::v21::PullDynamicScheduleUpdateResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type ReportChargingProfilesRequest =
        ocpp_client::ocpp_types::v21::ReportChargingProfilesRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type ReportChargingProfilesResponse =
        ocpp_client::ocpp_types::v21::ReportChargingProfilesResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type ReportDERControlRequest = ocpp_client::ocpp_types::v21::ReportDERControlRequest<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type ReportDERControlResponse =
        ocpp_client::ocpp_types::v21::ReportDERControlResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type RequestBatterySwapRequest =
        ocpp_client::ocpp_types::v21::RequestBatterySwapRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type RequestBatterySwapResponse =
        ocpp_client::ocpp_types::v21::RequestBatterySwapResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type RequestStartTransactionRequest =
        ocpp_client::ocpp_types::v21::RequestStartTransactionRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type RequestStartTransactionResponse =
        ocpp_client::ocpp_types::v21::RequestStartTransactionResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type RequestStopTransactionRequest =
        ocpp_client::ocpp_types::v21::RequestStopTransactionRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type RequestStopTransactionResponse =
        ocpp_client::ocpp_types::v21::RequestStopTransactionResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type ReservationStatusUpdateRequest =
        ocpp_client::ocpp_types::v21::ReservationStatusUpdateRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type ReservationStatusUpdateResponse =
        ocpp_client::ocpp_types::v21::ReservationStatusUpdateResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type ReserveNowRequest = ocpp_client::ocpp_types::v21::ReserveNowRequest<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type ReserveNowResponse = ocpp_client::ocpp_types::v21::ReserveNowResponse<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type ResetRequest = ocpp_client::ocpp_types::v21::ResetRequest<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type ResetResponse = ocpp_client::ocpp_types::v21::ResetResponse<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type SecurityEventNotificationRequest =
        ocpp_client::ocpp_types::v21::SecurityEventNotificationRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type SecurityEventNotificationResponse =
        ocpp_client::ocpp_types::v21::SecurityEventNotificationResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type SendLocalListRequest = ocpp_client::ocpp_types::v21::SendLocalListRequest<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type SendLocalListResponse = ocpp_client::ocpp_types::v21::SendLocalListResponse<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type SetChargingProfileRequest =
        ocpp_client::ocpp_types::v21::SetChargingProfileRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type SetChargingProfileResponse =
        ocpp_client::ocpp_types::v21::SetChargingProfileResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type SetDERControlRequest = ocpp_client::ocpp_types::v21::SetDERControlRequest<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type SetDERControlResponse = ocpp_client::ocpp_types::v21::SetDERControlResponse<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type SetDefaultTariffRequest = ocpp_client::ocpp_types::v21::SetDefaultTariffRequest<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type SetDefaultTariffResponse =
        ocpp_client::ocpp_types::v21::SetDefaultTariffResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type SetDisplayMessageRequest =
        ocpp_client::ocpp_types::v21::SetDisplayMessageRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type SetDisplayMessageResponse =
        ocpp_client::ocpp_types::v21::SetDisplayMessageResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type SetMonitoringBaseRequest =
        ocpp_client::ocpp_types::v21::SetMonitoringBaseRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type SetMonitoringBaseResponse =
        ocpp_client::ocpp_types::v21::SetMonitoringBaseResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type SetMonitoringLevelRequest =
        ocpp_client::ocpp_types::v21::SetMonitoringLevelRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type SetMonitoringLevelResponse =
        ocpp_client::ocpp_types::v21::SetMonitoringLevelResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type SetNetworkProfileRequest =
        ocpp_client::ocpp_types::v21::SetNetworkProfileRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type SetNetworkProfileResponse =
        ocpp_client::ocpp_types::v21::SetNetworkProfileResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type SetVariableMonitoringRequest =
        ocpp_client::ocpp_types::v21::SetVariableMonitoringRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type SetVariableMonitoringResponse =
        ocpp_client::ocpp_types::v21::SetVariableMonitoringResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type SetVariablesRequest = ocpp_client::ocpp_types::v21::SetVariablesRequest<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type SetVariablesResponse = ocpp_client::ocpp_types::v21::SetVariablesResponse<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type SignCertificateRequest = ocpp_client::ocpp_types::v21::SignCertificateRequest<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type SignCertificateResponse = ocpp_client::ocpp_types::v21::SignCertificateResponse<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type StatusNotificationRequest =
        ocpp_client::ocpp_types::v21::StatusNotificationRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type StatusNotificationResponse =
        ocpp_client::ocpp_types::v21::StatusNotificationResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type TransactionEventRequest = ocpp_client::ocpp_types::v21::TransactionEventRequest<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type TransactionEventResponse =
        ocpp_client::ocpp_types::v21::TransactionEventResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type TriggerMessageRequest = ocpp_client::ocpp_types::v21::TriggerMessageRequest<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type TriggerMessageResponse = ocpp_client::ocpp_types::v21::TriggerMessageResponse<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type UnlockConnectorRequest = ocpp_client::ocpp_types::v21::UnlockConnectorRequest<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type UnlockConnectorResponse = ocpp_client::ocpp_types::v21::UnlockConnectorResponse<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type UnpublishFirmwareRequest =
        ocpp_client::ocpp_types::v21::UnpublishFirmwareRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type UnpublishFirmwareResponse =
        ocpp_client::ocpp_types::v21::UnpublishFirmwareResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type UpdateDynamicScheduleRequest =
        ocpp_client::ocpp_types::v21::UpdateDynamicScheduleRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type UpdateDynamicScheduleResponse =
        ocpp_client::ocpp_types::v21::UpdateDynamicScheduleResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type UpdateFirmwareRequest = ocpp_client::ocpp_types::v21::UpdateFirmwareRequest<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type UpdateFirmwareResponse = ocpp_client::ocpp_types::v21::UpdateFirmwareResponse<
        ocpp_client::ocpp_types::v21::common::CustomData,
    >;
    pub(crate) type UsePriorityChargingRequest =
        ocpp_client::ocpp_types::v21::UsePriorityChargingRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type UsePriorityChargingResponse =
        ocpp_client::ocpp_types::v21::UsePriorityChargingResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type VatNumberValidationRequest =
        ocpp_client::ocpp_types::v21::VatNumberValidationRequest<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    pub(crate) type VatNumberValidationResponse =
        ocpp_client::ocpp_types::v21::VatNumberValidationResponse<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;

    /// The shared sub-structures, bound the same way.
    pub(crate) mod common {
        pub(crate) use ocpp_client::ocpp_types::v21::common::*;
        pub(crate) type ACChargingParameters =
            ocpp_client::ocpp_types::v21::common::ACChargingParameters<
                ocpp_client::ocpp_types::v21::common::CustomData,
            >;
        pub(crate) type APN = ocpp_client::ocpp_types::v21::common::APN<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type AbsolutePriceSchedule =
            ocpp_client::ocpp_types::v21::common::AbsolutePriceSchedule<
                ocpp_client::ocpp_types::v21::common::CustomData,
            >;
        pub(crate) type AdditionalInfo = ocpp_client::ocpp_types::v21::common::AdditionalInfo<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type AdditionalSelectedServices =
            ocpp_client::ocpp_types::v21::common::AdditionalSelectedServices<
                ocpp_client::ocpp_types::v21::common::CustomData,
            >;
        pub(crate) type Address = ocpp_client::ocpp_types::v21::common::Address<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type AuthorizationData = ocpp_client::ocpp_types::v21::common::AuthorizationData<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type BatteryData = ocpp_client::ocpp_types::v21::common::BatteryData<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type CertificateHashData =
            ocpp_client::ocpp_types::v21::common::CertificateHashData<
                ocpp_client::ocpp_types::v21::common::CustomData,
            >;
        pub(crate) type CertificateHashDataChain =
            ocpp_client::ocpp_types::v21::common::CertificateHashDataChain<
                ocpp_client::ocpp_types::v21::common::CustomData,
            >;
        pub(crate) type CertificateStatus = ocpp_client::ocpp_types::v21::common::CertificateStatus<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type CertificateStatusRequestInfo =
            ocpp_client::ocpp_types::v21::common::CertificateStatusRequestInfo<
                ocpp_client::ocpp_types::v21::common::CustomData,
            >;
        pub(crate) type ChargingLimit = ocpp_client::ocpp_types::v21::common::ChargingLimit<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type ChargingNeeds = ocpp_client::ocpp_types::v21::common::ChargingNeeds<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type ChargingPeriod = ocpp_client::ocpp_types::v21::common::ChargingPeriod<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type ChargingProfile = ocpp_client::ocpp_types::v21::common::ChargingProfile<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type ChargingProfileCriterion =
            ocpp_client::ocpp_types::v21::common::ChargingProfileCriterion<
                ocpp_client::ocpp_types::v21::common::CustomData,
            >;
        pub(crate) type ChargingSchedule = ocpp_client::ocpp_types::v21::common::ChargingSchedule<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type ChargingSchedulePeriod =
            ocpp_client::ocpp_types::v21::common::ChargingSchedulePeriod<
                ocpp_client::ocpp_types::v21::common::CustomData,
            >;
        pub(crate) type ChargingScheduleUpdate =
            ocpp_client::ocpp_types::v21::common::ChargingScheduleUpdate<
                ocpp_client::ocpp_types::v21::common::CustomData,
            >;
        pub(crate) type ChargingStation = ocpp_client::ocpp_types::v21::common::ChargingStation<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type ClearChargingProfile =
            ocpp_client::ocpp_types::v21::common::ClearChargingProfile<
                ocpp_client::ocpp_types::v21::common::CustomData,
            >;
        pub(crate) type ClearMonitoringResult =
            ocpp_client::ocpp_types::v21::common::ClearMonitoringResult<
                ocpp_client::ocpp_types::v21::common::CustomData,
            >;
        pub(crate) type ClearTariffsResult =
            ocpp_client::ocpp_types::v21::common::ClearTariffsResult<
                ocpp_client::ocpp_types::v21::common::CustomData,
            >;
        pub(crate) type Component = ocpp_client::ocpp_types::v21::common::Component<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type ComponentVariable = ocpp_client::ocpp_types::v21::common::ComponentVariable<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type CompositeSchedule = ocpp_client::ocpp_types::v21::common::CompositeSchedule<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type ConstantStreamData =
            ocpp_client::ocpp_types::v21::common::ConstantStreamData<
                ocpp_client::ocpp_types::v21::common::CustomData,
            >;
        pub(crate) type ConsumptionCost = ocpp_client::ocpp_types::v21::common::ConsumptionCost<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type Cost = ocpp_client::ocpp_types::v21::common::Cost<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type CostDetails = ocpp_client::ocpp_types::v21::common::CostDetails<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type CostDimension = ocpp_client::ocpp_types::v21::common::CostDimension<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type DCChargingParameters =
            ocpp_client::ocpp_types::v21::common::DCChargingParameters<
                ocpp_client::ocpp_types::v21::common::CustomData,
            >;
        pub(crate) type DERChargingParameters =
            ocpp_client::ocpp_types::v21::common::DERChargingParameters<
                ocpp_client::ocpp_types::v21::common::CustomData,
            >;
        pub(crate) type DERCurve = ocpp_client::ocpp_types::v21::common::DERCurve<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type DERCurveGet = ocpp_client::ocpp_types::v21::common::DERCurveGet<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type DERCurvePoints = ocpp_client::ocpp_types::v21::common::DERCurvePoints<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type EVAbsolutePriceSchedule =
            ocpp_client::ocpp_types::v21::common::EVAbsolutePriceSchedule<
                ocpp_client::ocpp_types::v21::common::CustomData,
            >;
        pub(crate) type EVAbsolutePriceScheduleEntry =
            ocpp_client::ocpp_types::v21::common::EVAbsolutePriceScheduleEntry<
                ocpp_client::ocpp_types::v21::common::CustomData,
            >;
        pub(crate) type EVEnergyOffer = ocpp_client::ocpp_types::v21::common::EVEnergyOffer<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type EVPowerSchedule = ocpp_client::ocpp_types::v21::common::EVPowerSchedule<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type EVPowerScheduleEntry =
            ocpp_client::ocpp_types::v21::common::EVPowerScheduleEntry<
                ocpp_client::ocpp_types::v21::common::CustomData,
            >;
        pub(crate) type EVPriceRule = ocpp_client::ocpp_types::v21::common::EVPriceRule<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type EVSE = ocpp_client::ocpp_types::v21::common::EVSE<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type EnterService = ocpp_client::ocpp_types::v21::common::EnterService<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type EnterServiceGet = ocpp_client::ocpp_types::v21::common::EnterServiceGet<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type EventData = ocpp_client::ocpp_types::v21::common::EventData<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type Firmware = ocpp_client::ocpp_types::v21::common::Firmware<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type FixedPF = ocpp_client::ocpp_types::v21::common::FixedPF<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type FixedPFGet = ocpp_client::ocpp_types::v21::common::FixedPFGet<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type FixedVar = ocpp_client::ocpp_types::v21::common::FixedVar<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type FixedVarGet = ocpp_client::ocpp_types::v21::common::FixedVarGet<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type FreqDroop = ocpp_client::ocpp_types::v21::common::FreqDroop<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type FreqDroopGet = ocpp_client::ocpp_types::v21::common::FreqDroopGet<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type GetVariableData = ocpp_client::ocpp_types::v21::common::GetVariableData<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type GetVariableResult = ocpp_client::ocpp_types::v21::common::GetVariableResult<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type Gradient = ocpp_client::ocpp_types::v21::common::Gradient<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type GradientGet = ocpp_client::ocpp_types::v21::common::GradientGet<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type Hysteresis = ocpp_client::ocpp_types::v21::common::Hysteresis<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type IdToken = ocpp_client::ocpp_types::v21::common::IdToken<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type IdTokenInfo = ocpp_client::ocpp_types::v21::common::IdTokenInfo<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type LimitAtSoC = ocpp_client::ocpp_types::v21::common::LimitAtSoC<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type LimitMaxDischarge = ocpp_client::ocpp_types::v21::common::LimitMaxDischarge<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type LimitMaxDischargeGet =
            ocpp_client::ocpp_types::v21::common::LimitMaxDischargeGet<
                ocpp_client::ocpp_types::v21::common::CustomData,
            >;
        pub(crate) type LogParameters = ocpp_client::ocpp_types::v21::common::LogParameters<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type MessageContent = ocpp_client::ocpp_types::v21::common::MessageContent<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type MessageInfo = ocpp_client::ocpp_types::v21::common::MessageInfo<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type MeterValue = ocpp_client::ocpp_types::v21::common::MeterValue<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type Modem = ocpp_client::ocpp_types::v21::common::Modem<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type MonitoringData = ocpp_client::ocpp_types::v21::common::MonitoringData<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type NetworkConnectionProfile =
            ocpp_client::ocpp_types::v21::common::NetworkConnectionProfile<
                ocpp_client::ocpp_types::v21::common::CustomData,
            >;
        pub(crate) type OCSPRequestData = ocpp_client::ocpp_types::v21::common::OCSPRequestData<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type OverstayRule = ocpp_client::ocpp_types::v21::common::OverstayRule<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type OverstayRuleList = ocpp_client::ocpp_types::v21::common::OverstayRuleList<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type PeriodicEventStreamParams =
            ocpp_client::ocpp_types::v21::common::PeriodicEventStreamParams<
                ocpp_client::ocpp_types::v21::common::CustomData,
            >;
        pub(crate) type Price = ocpp_client::ocpp_types::v21::common::Price<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type PriceLevelSchedule =
            ocpp_client::ocpp_types::v21::common::PriceLevelSchedule<
                ocpp_client::ocpp_types::v21::common::CustomData,
            >;
        pub(crate) type PriceLevelScheduleEntry =
            ocpp_client::ocpp_types::v21::common::PriceLevelScheduleEntry<
                ocpp_client::ocpp_types::v21::common::CustomData,
            >;
        pub(crate) type PriceRule = ocpp_client::ocpp_types::v21::common::PriceRule<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type PriceRuleStack = ocpp_client::ocpp_types::v21::common::PriceRuleStack<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type RationalNumber = ocpp_client::ocpp_types::v21::common::RationalNumber<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type ReactivePowerParams =
            ocpp_client::ocpp_types::v21::common::ReactivePowerParams<
                ocpp_client::ocpp_types::v21::common::CustomData,
            >;
        pub(crate) type RelativeTimeInterval =
            ocpp_client::ocpp_types::v21::common::RelativeTimeInterval<
                ocpp_client::ocpp_types::v21::common::CustomData,
            >;
        pub(crate) type ReportData = ocpp_client::ocpp_types::v21::common::ReportData<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type SalesTariff = ocpp_client::ocpp_types::v21::common::SalesTariff<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type SalesTariffEntry = ocpp_client::ocpp_types::v21::common::SalesTariffEntry<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type SampledValue = ocpp_client::ocpp_types::v21::common::SampledValue<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type SetMonitoringData = ocpp_client::ocpp_types::v21::common::SetMonitoringData<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type SetMonitoringResult =
            ocpp_client::ocpp_types::v21::common::SetMonitoringResult<
                ocpp_client::ocpp_types::v21::common::CustomData,
            >;
        pub(crate) type SetVariableData = ocpp_client::ocpp_types::v21::common::SetVariableData<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type SetVariableResult = ocpp_client::ocpp_types::v21::common::SetVariableResult<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type SignedMeterValue = ocpp_client::ocpp_types::v21::common::SignedMeterValue<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type StatusInfo = ocpp_client::ocpp_types::v21::common::StatusInfo<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type StreamDataElement = ocpp_client::ocpp_types::v21::common::StreamDataElement<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type Tariff = ocpp_client::ocpp_types::v21::common::Tariff<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type TariffAssignment = ocpp_client::ocpp_types::v21::common::TariffAssignment<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type TariffConditions = ocpp_client::ocpp_types::v21::common::TariffConditions<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type TariffConditionsFixed =
            ocpp_client::ocpp_types::v21::common::TariffConditionsFixed<
                ocpp_client::ocpp_types::v21::common::CustomData,
            >;
        pub(crate) type TariffEnergy = ocpp_client::ocpp_types::v21::common::TariffEnergy<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type TariffEnergyPrice = ocpp_client::ocpp_types::v21::common::TariffEnergyPrice<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type TariffFixed = ocpp_client::ocpp_types::v21::common::TariffFixed<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type TariffFixedPrice = ocpp_client::ocpp_types::v21::common::TariffFixedPrice<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type TariffTime = ocpp_client::ocpp_types::v21::common::TariffTime<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type TariffTimePrice = ocpp_client::ocpp_types::v21::common::TariffTimePrice<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type TaxRate = ocpp_client::ocpp_types::v21::common::TaxRate<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type TaxRule = ocpp_client::ocpp_types::v21::common::TaxRule<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type TotalCost = ocpp_client::ocpp_types::v21::common::TotalCost<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type TotalPrice = ocpp_client::ocpp_types::v21::common::TotalPrice<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type TotalUsage = ocpp_client::ocpp_types::v21::common::TotalUsage<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type Transaction = ocpp_client::ocpp_types::v21::common::Transaction<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type TransactionLimit = ocpp_client::ocpp_types::v21::common::TransactionLimit<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type UnitOfMeasure = ocpp_client::ocpp_types::v21::common::UnitOfMeasure<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type V2XChargingParameters =
            ocpp_client::ocpp_types::v21::common::V2XChargingParameters<
                ocpp_client::ocpp_types::v21::common::CustomData,
            >;
        pub(crate) type V2XFreqWattPoint = ocpp_client::ocpp_types::v21::common::V2XFreqWattPoint<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type V2XSignalWattPoint =
            ocpp_client::ocpp_types::v21::common::V2XSignalWattPoint<
                ocpp_client::ocpp_types::v21::common::CustomData,
            >;
        pub(crate) type VPN = ocpp_client::ocpp_types::v21::common::VPN<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type Variable = ocpp_client::ocpp_types::v21::common::Variable<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type VariableAttribute = ocpp_client::ocpp_types::v21::common::VariableAttribute<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
        pub(crate) type VariableCharacteristics =
            ocpp_client::ocpp_types::v21::common::VariableCharacteristics<
                ocpp_client::ocpp_types::v21::common::CustomData,
            >;
        pub(crate) type VariableMonitoring =
            ocpp_client::ocpp_types::v21::common::VariableMonitoring<
                ocpp_client::ocpp_types::v21::common::CustomData,
            >;
        pub(crate) type VoltageParams = ocpp_client::ocpp_types::v21::common::VoltageParams<
            ocpp_client::ocpp_types::v21::common::CustomData,
        >;
    }
}

/// OCPP 2.0.1 wire types, with `customData` bound to
/// [`ocpp_client::ocpp_types::v201::common::CustomData`].
pub(crate) mod v201 {
    pub(crate) use ocpp_client::ocpp_types::v201::*;
    pub(crate) type AuthorizeRequest = ocpp_client::ocpp_types::v201::AuthorizeRequest<
        ocpp_client::ocpp_types::v201::common::CustomData,
    >;
    pub(crate) type AuthorizeResponse = ocpp_client::ocpp_types::v201::AuthorizeResponse<
        ocpp_client::ocpp_types::v201::common::CustomData,
    >;
    pub(crate) type BootNotificationRequest =
        ocpp_client::ocpp_types::v201::BootNotificationRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type BootNotificationResponse =
        ocpp_client::ocpp_types::v201::BootNotificationResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type CancelReservationRequest =
        ocpp_client::ocpp_types::v201::CancelReservationRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type CancelReservationResponse =
        ocpp_client::ocpp_types::v201::CancelReservationResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type CertificateSignedRequest =
        ocpp_client::ocpp_types::v201::CertificateSignedRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type CertificateSignedResponse =
        ocpp_client::ocpp_types::v201::CertificateSignedResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type ChangeAvailabilityRequest =
        ocpp_client::ocpp_types::v201::ChangeAvailabilityRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type ChangeAvailabilityResponse =
        ocpp_client::ocpp_types::v201::ChangeAvailabilityResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type ClearCacheRequest = ocpp_client::ocpp_types::v201::ClearCacheRequest<
        ocpp_client::ocpp_types::v201::common::CustomData,
    >;
    pub(crate) type ClearCacheResponse = ocpp_client::ocpp_types::v201::ClearCacheResponse<
        ocpp_client::ocpp_types::v201::common::CustomData,
    >;
    pub(crate) type ClearChargingProfileRequest =
        ocpp_client::ocpp_types::v201::ClearChargingProfileRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type ClearChargingProfileResponse =
        ocpp_client::ocpp_types::v201::ClearChargingProfileResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type ClearDisplayMessageRequest =
        ocpp_client::ocpp_types::v201::ClearDisplayMessageRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type ClearDisplayMessageResponse =
        ocpp_client::ocpp_types::v201::ClearDisplayMessageResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type ClearVariableMonitoringRequest =
        ocpp_client::ocpp_types::v201::ClearVariableMonitoringRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type ClearVariableMonitoringResponse =
        ocpp_client::ocpp_types::v201::ClearVariableMonitoringResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type ClearedChargingLimitRequest =
        ocpp_client::ocpp_types::v201::ClearedChargingLimitRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type ClearedChargingLimitResponse =
        ocpp_client::ocpp_types::v201::ClearedChargingLimitResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type CostUpdatedRequest = ocpp_client::ocpp_types::v201::CostUpdatedRequest<
        ocpp_client::ocpp_types::v201::common::CustomData,
    >;
    pub(crate) type CostUpdatedResponse = ocpp_client::ocpp_types::v201::CostUpdatedResponse<
        ocpp_client::ocpp_types::v201::common::CustomData,
    >;
    pub(crate) type CustomerInformationRequest =
        ocpp_client::ocpp_types::v201::CustomerInformationRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type CustomerInformationResponse =
        ocpp_client::ocpp_types::v201::CustomerInformationResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    // `DataTransfer` is the one action whose *payload* is also a type parameter, and it
    // is declared before `CustomDataType`:
    //
    //     pub struct DataTransferRequest<DataTransferRequestData = (), CustomDataType = NoCustomData>
    //
    // `ocpp-client` 0.4.0's action macro appends the customData type positionally
    // (`$req<C>`, src/ocpp_2_1/actions.rs), so for this one action it lands on the wrong
    // parameter: `send_data_transfer` takes `DataTransferRequest<CustomData, NoCustomData>`,
    // i.e. `data: Option<CustomData>` and `custom_data: Option<NoCustomData>` - the
    // free-form vendor payload typed as the `{vendorId}` object, and the real `customData`
    // discarded. That is an upstream defect (see `docs/UPSTREAM-GAPS.md`), not a choice
    // this crate can make differently: these aliases have to name what the client's
    // signatures actually are or nothing type-checks.
    //
    // Harmless here today - `crate::data_transfer` sends `data: None`/`custom_data: None`
    // on both, and both skip-serialize when `None`, so the wire bytes are unchanged.
    pub(crate) type DataTransferRequest = ocpp_client::ocpp_types::v201::DataTransferRequest<
        ocpp_client::ocpp_types::v201::common::CustomData,
    >;
    pub(crate) type DataTransferResponse = ocpp_client::ocpp_types::v201::DataTransferResponse<
        ocpp_client::ocpp_types::v201::common::CustomData,
    >;
    pub(crate) type DeleteCertificateRequest =
        ocpp_client::ocpp_types::v201::DeleteCertificateRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type DeleteCertificateResponse =
        ocpp_client::ocpp_types::v201::DeleteCertificateResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type FirmwareStatusNotificationRequest =
        ocpp_client::ocpp_types::v201::FirmwareStatusNotificationRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type FirmwareStatusNotificationResponse =
        ocpp_client::ocpp_types::v201::FirmwareStatusNotificationResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type Get15118EVCertificateRequest =
        ocpp_client::ocpp_types::v201::Get15118EVCertificateRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type Get15118EVCertificateResponse =
        ocpp_client::ocpp_types::v201::Get15118EVCertificateResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type GetBaseReportRequest = ocpp_client::ocpp_types::v201::GetBaseReportRequest<
        ocpp_client::ocpp_types::v201::common::CustomData,
    >;
    pub(crate) type GetBaseReportResponse = ocpp_client::ocpp_types::v201::GetBaseReportResponse<
        ocpp_client::ocpp_types::v201::common::CustomData,
    >;
    pub(crate) type GetCertificateStatusRequest =
        ocpp_client::ocpp_types::v201::GetCertificateStatusRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type GetCertificateStatusResponse =
        ocpp_client::ocpp_types::v201::GetCertificateStatusResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type GetChargingProfilesRequest =
        ocpp_client::ocpp_types::v201::GetChargingProfilesRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type GetChargingProfilesResponse =
        ocpp_client::ocpp_types::v201::GetChargingProfilesResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type GetCompositeScheduleRequest =
        ocpp_client::ocpp_types::v201::GetCompositeScheduleRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type GetCompositeScheduleResponse =
        ocpp_client::ocpp_types::v201::GetCompositeScheduleResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type GetDisplayMessagesRequest =
        ocpp_client::ocpp_types::v201::GetDisplayMessagesRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type GetDisplayMessagesResponse =
        ocpp_client::ocpp_types::v201::GetDisplayMessagesResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type GetInstalledCertificateIdsRequest =
        ocpp_client::ocpp_types::v201::GetInstalledCertificateIdsRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type GetInstalledCertificateIdsResponse =
        ocpp_client::ocpp_types::v201::GetInstalledCertificateIdsResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type GetLocalListVersionRequest =
        ocpp_client::ocpp_types::v201::GetLocalListVersionRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type GetLocalListVersionResponse =
        ocpp_client::ocpp_types::v201::GetLocalListVersionResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type GetLogRequest = ocpp_client::ocpp_types::v201::GetLogRequest<
        ocpp_client::ocpp_types::v201::common::CustomData,
    >;
    pub(crate) type GetLogResponse = ocpp_client::ocpp_types::v201::GetLogResponse<
        ocpp_client::ocpp_types::v201::common::CustomData,
    >;
    pub(crate) type GetMonitoringReportRequest =
        ocpp_client::ocpp_types::v201::GetMonitoringReportRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type GetMonitoringReportResponse =
        ocpp_client::ocpp_types::v201::GetMonitoringReportResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type GetReportRequest = ocpp_client::ocpp_types::v201::GetReportRequest<
        ocpp_client::ocpp_types::v201::common::CustomData,
    >;
    pub(crate) type GetReportResponse = ocpp_client::ocpp_types::v201::GetReportResponse<
        ocpp_client::ocpp_types::v201::common::CustomData,
    >;
    pub(crate) type GetTransactionStatusRequest =
        ocpp_client::ocpp_types::v201::GetTransactionStatusRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type GetTransactionStatusResponse =
        ocpp_client::ocpp_types::v201::GetTransactionStatusResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type GetVariablesRequest = ocpp_client::ocpp_types::v201::GetVariablesRequest<
        ocpp_client::ocpp_types::v201::common::CustomData,
    >;
    pub(crate) type GetVariablesResponse = ocpp_client::ocpp_types::v201::GetVariablesResponse<
        ocpp_client::ocpp_types::v201::common::CustomData,
    >;
    pub(crate) type HeartbeatRequest = ocpp_client::ocpp_types::v201::HeartbeatRequest<
        ocpp_client::ocpp_types::v201::common::CustomData,
    >;
    pub(crate) type HeartbeatResponse = ocpp_client::ocpp_types::v201::HeartbeatResponse<
        ocpp_client::ocpp_types::v201::common::CustomData,
    >;
    pub(crate) type InstallCertificateRequest =
        ocpp_client::ocpp_types::v201::InstallCertificateRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type InstallCertificateResponse =
        ocpp_client::ocpp_types::v201::InstallCertificateResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type LogStatusNotificationRequest =
        ocpp_client::ocpp_types::v201::LogStatusNotificationRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type LogStatusNotificationResponse =
        ocpp_client::ocpp_types::v201::LogStatusNotificationResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type MeterValuesRequest = ocpp_client::ocpp_types::v201::MeterValuesRequest<
        ocpp_client::ocpp_types::v201::common::CustomData,
    >;
    pub(crate) type MeterValuesResponse = ocpp_client::ocpp_types::v201::MeterValuesResponse<
        ocpp_client::ocpp_types::v201::common::CustomData,
    >;
    pub(crate) type NotifyChargingLimitRequest =
        ocpp_client::ocpp_types::v201::NotifyChargingLimitRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type NotifyChargingLimitResponse =
        ocpp_client::ocpp_types::v201::NotifyChargingLimitResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type NotifyCustomerInformationRequest =
        ocpp_client::ocpp_types::v201::NotifyCustomerInformationRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type NotifyCustomerInformationResponse =
        ocpp_client::ocpp_types::v201::NotifyCustomerInformationResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type NotifyDisplayMessagesRequest =
        ocpp_client::ocpp_types::v201::NotifyDisplayMessagesRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type NotifyDisplayMessagesResponse =
        ocpp_client::ocpp_types::v201::NotifyDisplayMessagesResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type NotifyEVChargingNeedsRequest =
        ocpp_client::ocpp_types::v201::NotifyEVChargingNeedsRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type NotifyEVChargingNeedsResponse =
        ocpp_client::ocpp_types::v201::NotifyEVChargingNeedsResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type NotifyEVChargingScheduleRequest =
        ocpp_client::ocpp_types::v201::NotifyEVChargingScheduleRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type NotifyEVChargingScheduleResponse =
        ocpp_client::ocpp_types::v201::NotifyEVChargingScheduleResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type NotifyEventRequest = ocpp_client::ocpp_types::v201::NotifyEventRequest<
        ocpp_client::ocpp_types::v201::common::CustomData,
    >;
    pub(crate) type NotifyEventResponse = ocpp_client::ocpp_types::v201::NotifyEventResponse<
        ocpp_client::ocpp_types::v201::common::CustomData,
    >;
    pub(crate) type NotifyMonitoringReportRequest =
        ocpp_client::ocpp_types::v201::NotifyMonitoringReportRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type NotifyMonitoringReportResponse =
        ocpp_client::ocpp_types::v201::NotifyMonitoringReportResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type NotifyReportRequest = ocpp_client::ocpp_types::v201::NotifyReportRequest<
        ocpp_client::ocpp_types::v201::common::CustomData,
    >;
    pub(crate) type NotifyReportResponse = ocpp_client::ocpp_types::v201::NotifyReportResponse<
        ocpp_client::ocpp_types::v201::common::CustomData,
    >;
    pub(crate) type PublishFirmwareRequest = ocpp_client::ocpp_types::v201::PublishFirmwareRequest<
        ocpp_client::ocpp_types::v201::common::CustomData,
    >;
    pub(crate) type PublishFirmwareResponse =
        ocpp_client::ocpp_types::v201::PublishFirmwareResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type PublishFirmwareStatusNotificationRequest =
        ocpp_client::ocpp_types::v201::PublishFirmwareStatusNotificationRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type PublishFirmwareStatusNotificationResponse =
        ocpp_client::ocpp_types::v201::PublishFirmwareStatusNotificationResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type ReportChargingProfilesRequest =
        ocpp_client::ocpp_types::v201::ReportChargingProfilesRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type ReportChargingProfilesResponse =
        ocpp_client::ocpp_types::v201::ReportChargingProfilesResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type RequestStartTransactionRequest =
        ocpp_client::ocpp_types::v201::RequestStartTransactionRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type RequestStartTransactionResponse =
        ocpp_client::ocpp_types::v201::RequestStartTransactionResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type RequestStopTransactionRequest =
        ocpp_client::ocpp_types::v201::RequestStopTransactionRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type RequestStopTransactionResponse =
        ocpp_client::ocpp_types::v201::RequestStopTransactionResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type ReservationStatusUpdateRequest =
        ocpp_client::ocpp_types::v201::ReservationStatusUpdateRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type ReservationStatusUpdateResponse =
        ocpp_client::ocpp_types::v201::ReservationStatusUpdateResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type ReserveNowRequest = ocpp_client::ocpp_types::v201::ReserveNowRequest<
        ocpp_client::ocpp_types::v201::common::CustomData,
    >;
    pub(crate) type ReserveNowResponse = ocpp_client::ocpp_types::v201::ReserveNowResponse<
        ocpp_client::ocpp_types::v201::common::CustomData,
    >;
    pub(crate) type ResetRequest = ocpp_client::ocpp_types::v201::ResetRequest<
        ocpp_client::ocpp_types::v201::common::CustomData,
    >;
    pub(crate) type ResetResponse = ocpp_client::ocpp_types::v201::ResetResponse<
        ocpp_client::ocpp_types::v201::common::CustomData,
    >;
    pub(crate) type SecurityEventNotificationRequest =
        ocpp_client::ocpp_types::v201::SecurityEventNotificationRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type SecurityEventNotificationResponse =
        ocpp_client::ocpp_types::v201::SecurityEventNotificationResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type SendLocalListRequest = ocpp_client::ocpp_types::v201::SendLocalListRequest<
        ocpp_client::ocpp_types::v201::common::CustomData,
    >;
    pub(crate) type SendLocalListResponse = ocpp_client::ocpp_types::v201::SendLocalListResponse<
        ocpp_client::ocpp_types::v201::common::CustomData,
    >;
    pub(crate) type SetChargingProfileRequest =
        ocpp_client::ocpp_types::v201::SetChargingProfileRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type SetChargingProfileResponse =
        ocpp_client::ocpp_types::v201::SetChargingProfileResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type SetDisplayMessageRequest =
        ocpp_client::ocpp_types::v201::SetDisplayMessageRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type SetDisplayMessageResponse =
        ocpp_client::ocpp_types::v201::SetDisplayMessageResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type SetMonitoringBaseRequest =
        ocpp_client::ocpp_types::v201::SetMonitoringBaseRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type SetMonitoringBaseResponse =
        ocpp_client::ocpp_types::v201::SetMonitoringBaseResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type SetMonitoringLevelRequest =
        ocpp_client::ocpp_types::v201::SetMonitoringLevelRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type SetMonitoringLevelResponse =
        ocpp_client::ocpp_types::v201::SetMonitoringLevelResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type SetNetworkProfileRequest =
        ocpp_client::ocpp_types::v201::SetNetworkProfileRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type SetNetworkProfileResponse =
        ocpp_client::ocpp_types::v201::SetNetworkProfileResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type SetVariableMonitoringRequest =
        ocpp_client::ocpp_types::v201::SetVariableMonitoringRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type SetVariableMonitoringResponse =
        ocpp_client::ocpp_types::v201::SetVariableMonitoringResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type SetVariablesRequest = ocpp_client::ocpp_types::v201::SetVariablesRequest<
        ocpp_client::ocpp_types::v201::common::CustomData,
    >;
    pub(crate) type SetVariablesResponse = ocpp_client::ocpp_types::v201::SetVariablesResponse<
        ocpp_client::ocpp_types::v201::common::CustomData,
    >;
    pub(crate) type SignCertificateRequest = ocpp_client::ocpp_types::v201::SignCertificateRequest<
        ocpp_client::ocpp_types::v201::common::CustomData,
    >;
    pub(crate) type SignCertificateResponse =
        ocpp_client::ocpp_types::v201::SignCertificateResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type StatusNotificationRequest =
        ocpp_client::ocpp_types::v201::StatusNotificationRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type StatusNotificationResponse =
        ocpp_client::ocpp_types::v201::StatusNotificationResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type TransactionEventRequest =
        ocpp_client::ocpp_types::v201::TransactionEventRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type TransactionEventResponse =
        ocpp_client::ocpp_types::v201::TransactionEventResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type TriggerMessageRequest = ocpp_client::ocpp_types::v201::TriggerMessageRequest<
        ocpp_client::ocpp_types::v201::common::CustomData,
    >;
    pub(crate) type TriggerMessageResponse = ocpp_client::ocpp_types::v201::TriggerMessageResponse<
        ocpp_client::ocpp_types::v201::common::CustomData,
    >;
    pub(crate) type UnlockConnectorRequest = ocpp_client::ocpp_types::v201::UnlockConnectorRequest<
        ocpp_client::ocpp_types::v201::common::CustomData,
    >;
    pub(crate) type UnlockConnectorResponse =
        ocpp_client::ocpp_types::v201::UnlockConnectorResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type UnpublishFirmwareRequest =
        ocpp_client::ocpp_types::v201::UnpublishFirmwareRequest<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type UnpublishFirmwareResponse =
        ocpp_client::ocpp_types::v201::UnpublishFirmwareResponse<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
    pub(crate) type UpdateFirmwareRequest = ocpp_client::ocpp_types::v201::UpdateFirmwareRequest<
        ocpp_client::ocpp_types::v201::common::CustomData,
    >;
    pub(crate) type UpdateFirmwareResponse = ocpp_client::ocpp_types::v201::UpdateFirmwareResponse<
        ocpp_client::ocpp_types::v201::common::CustomData,
    >;

    /// The shared sub-structures, bound the same way.
    pub(crate) mod common {
        pub(crate) use ocpp_client::ocpp_types::v201::common::*;
        pub(crate) type ACChargingParameters =
            ocpp_client::ocpp_types::v201::common::ACChargingParameters<
                ocpp_client::ocpp_types::v201::common::CustomData,
            >;
        pub(crate) type APN = ocpp_client::ocpp_types::v201::common::APN<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
        pub(crate) type AdditionalInfo = ocpp_client::ocpp_types::v201::common::AdditionalInfo<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
        pub(crate) type AuthorizationData =
            ocpp_client::ocpp_types::v201::common::AuthorizationData<
                ocpp_client::ocpp_types::v201::common::CustomData,
            >;
        pub(crate) type CertificateHashData =
            ocpp_client::ocpp_types::v201::common::CertificateHashData<
                ocpp_client::ocpp_types::v201::common::CustomData,
            >;
        pub(crate) type CertificateHashDataChain =
            ocpp_client::ocpp_types::v201::common::CertificateHashDataChain<
                ocpp_client::ocpp_types::v201::common::CustomData,
            >;
        pub(crate) type ChargingLimit = ocpp_client::ocpp_types::v201::common::ChargingLimit<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
        pub(crate) type ChargingNeeds = ocpp_client::ocpp_types::v201::common::ChargingNeeds<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
        pub(crate) type ChargingProfile = ocpp_client::ocpp_types::v201::common::ChargingProfile<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
        pub(crate) type ChargingProfileCriterion =
            ocpp_client::ocpp_types::v201::common::ChargingProfileCriterion<
                ocpp_client::ocpp_types::v201::common::CustomData,
            >;
        pub(crate) type ChargingSchedule = ocpp_client::ocpp_types::v201::common::ChargingSchedule<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
        pub(crate) type ChargingSchedulePeriod =
            ocpp_client::ocpp_types::v201::common::ChargingSchedulePeriod<
                ocpp_client::ocpp_types::v201::common::CustomData,
            >;
        pub(crate) type ChargingStation = ocpp_client::ocpp_types::v201::common::ChargingStation<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
        pub(crate) type ClearChargingProfile =
            ocpp_client::ocpp_types::v201::common::ClearChargingProfile<
                ocpp_client::ocpp_types::v201::common::CustomData,
            >;
        pub(crate) type ClearMonitoringResult =
            ocpp_client::ocpp_types::v201::common::ClearMonitoringResult<
                ocpp_client::ocpp_types::v201::common::CustomData,
            >;
        pub(crate) type Component = ocpp_client::ocpp_types::v201::common::Component<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
        pub(crate) type ComponentVariable =
            ocpp_client::ocpp_types::v201::common::ComponentVariable<
                ocpp_client::ocpp_types::v201::common::CustomData,
            >;
        pub(crate) type CompositeSchedule =
            ocpp_client::ocpp_types::v201::common::CompositeSchedule<
                ocpp_client::ocpp_types::v201::common::CustomData,
            >;
        pub(crate) type ConsumptionCost = ocpp_client::ocpp_types::v201::common::ConsumptionCost<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
        pub(crate) type Cost = ocpp_client::ocpp_types::v201::common::Cost<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
        pub(crate) type DCChargingParameters =
            ocpp_client::ocpp_types::v201::common::DCChargingParameters<
                ocpp_client::ocpp_types::v201::common::CustomData,
            >;
        pub(crate) type EVSE = ocpp_client::ocpp_types::v201::common::EVSE<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
        pub(crate) type EventData = ocpp_client::ocpp_types::v201::common::EventData<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
        pub(crate) type Firmware = ocpp_client::ocpp_types::v201::common::Firmware<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
        pub(crate) type GetVariableData = ocpp_client::ocpp_types::v201::common::GetVariableData<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
        pub(crate) type GetVariableResult =
            ocpp_client::ocpp_types::v201::common::GetVariableResult<
                ocpp_client::ocpp_types::v201::common::CustomData,
            >;
        pub(crate) type IdToken = ocpp_client::ocpp_types::v201::common::IdToken<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
        pub(crate) type IdTokenInfo = ocpp_client::ocpp_types::v201::common::IdTokenInfo<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
        pub(crate) type LogParameters = ocpp_client::ocpp_types::v201::common::LogParameters<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
        pub(crate) type MessageContent = ocpp_client::ocpp_types::v201::common::MessageContent<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
        pub(crate) type MessageInfo = ocpp_client::ocpp_types::v201::common::MessageInfo<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
        pub(crate) type MeterValue = ocpp_client::ocpp_types::v201::common::MeterValue<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
        pub(crate) type Modem = ocpp_client::ocpp_types::v201::common::Modem<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
        pub(crate) type MonitoringData = ocpp_client::ocpp_types::v201::common::MonitoringData<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
        pub(crate) type NetworkConnectionProfile =
            ocpp_client::ocpp_types::v201::common::NetworkConnectionProfile<
                ocpp_client::ocpp_types::v201::common::CustomData,
            >;
        pub(crate) type OCSPRequestData = ocpp_client::ocpp_types::v201::common::OCSPRequestData<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
        pub(crate) type RelativeTimeInterval =
            ocpp_client::ocpp_types::v201::common::RelativeTimeInterval<
                ocpp_client::ocpp_types::v201::common::CustomData,
            >;
        pub(crate) type ReportData = ocpp_client::ocpp_types::v201::common::ReportData<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
        pub(crate) type SalesTariff = ocpp_client::ocpp_types::v201::common::SalesTariff<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
        pub(crate) type SalesTariffEntry = ocpp_client::ocpp_types::v201::common::SalesTariffEntry<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
        pub(crate) type SampledValue = ocpp_client::ocpp_types::v201::common::SampledValue<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
        pub(crate) type SetMonitoringData =
            ocpp_client::ocpp_types::v201::common::SetMonitoringData<
                ocpp_client::ocpp_types::v201::common::CustomData,
            >;
        pub(crate) type SetMonitoringResult =
            ocpp_client::ocpp_types::v201::common::SetMonitoringResult<
                ocpp_client::ocpp_types::v201::common::CustomData,
            >;
        pub(crate) type SetVariableData = ocpp_client::ocpp_types::v201::common::SetVariableData<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
        pub(crate) type SetVariableResult =
            ocpp_client::ocpp_types::v201::common::SetVariableResult<
                ocpp_client::ocpp_types::v201::common::CustomData,
            >;
        pub(crate) type SignedMeterValue = ocpp_client::ocpp_types::v201::common::SignedMeterValue<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
        pub(crate) type StatusInfo = ocpp_client::ocpp_types::v201::common::StatusInfo<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
        pub(crate) type Transaction = ocpp_client::ocpp_types::v201::common::Transaction<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
        pub(crate) type UnitOfMeasure = ocpp_client::ocpp_types::v201::common::UnitOfMeasure<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
        pub(crate) type VPN = ocpp_client::ocpp_types::v201::common::VPN<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
        pub(crate) type Variable = ocpp_client::ocpp_types::v201::common::Variable<
            ocpp_client::ocpp_types::v201::common::CustomData,
        >;
        pub(crate) type VariableAttribute =
            ocpp_client::ocpp_types::v201::common::VariableAttribute<
                ocpp_client::ocpp_types::v201::common::CustomData,
            >;
        pub(crate) type VariableCharacteristics =
            ocpp_client::ocpp_types::v201::common::VariableCharacteristics<
                ocpp_client::ocpp_types::v201::common::CustomData,
            >;
        pub(crate) type VariableMonitoring =
            ocpp_client::ocpp_types::v201::common::VariableMonitoring<
                ocpp_client::ocpp_types::v201::common::CustomData,
            >;
    }
}

/// OCPP 1.6 wire types. A plain re-export: 1.6J has no `customData`, so nothing here is generic.
pub(crate) mod v16 {
    pub(crate) use ocpp_client::ocpp_types::v16::*;

    pub(crate) mod common {
        pub(crate) use ocpp_client::ocpp_types::v16::common::*;
    }
}
