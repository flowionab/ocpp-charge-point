//! Data transfer functional block: a vendor-specific pass-through channel via `DataTransfer`,
//! usable in either direction. See `docs/ROADMAP.md` §16.
//!
//! Unlike every other CSMS-initiated action this crate handles, `DataTransfer` is explicitly
//! vendor-defined - this crate has no way to interpret `data` itself, so [`DataTransferHandler`]
//! is supplied by the integrator, not by this crate. Consequently this module is **not** wired
//! into [`crate::setup`] like `ReserveNow`/`ChangeAvailability`/etc. are: only integrators that
//! actually use a vendor extension need [`DataTransferRegistrar::register_data_transfer_handler`],
//! called directly against their own (cloned) CSMS client handle, before or after `setup()`.
//! Sending one outbound is likewise just a direct call to [`DataTransferSender::transfer_data`]
//! on that same handle - it needs no `ChargePointActor`/state involvement, since `DataTransfer`
//! carries no state this crate understands.
//!
//! **Known limitation**: `ocpp-types`' `DataTransferRequest`/`DataTransferResponse.data` field is
//! typed `Option<()>` - OCPP's schema allows `data` to be any JSON value, which the upstream
//! codegen couldn't represent, so it collapsed to Rust's unit type. Until that's fixed upstream,
//! no actual payload can cross the wire through this block: [`DataTransferMessage::data`]/
//! [`DataTransferResult::data`] exist as real `Option<String>` fields (raw JSON) so the rest of
//! this crate's API is ready for it, but the OCPP 2.1 adapter can only ever send/receive `None`.

use alloc::boxed::Box;
use alloc::string::String;

/// The outcome of a `DataTransfer` exchange, matching OCPP's `DataTransferStatusEnum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataTransferOutcome {
    Accepted,
    Rejected,
    UnknownMessageId,
    UnknownVendorId,
}

/// A vendor-specific `DataTransfer` message. This crate doesn't interpret `vendor_id`/
/// `message_id`/`data` itself - only an integrator-supplied [`DataTransferHandler`] (inbound) or
/// caller (outbound) gives them meaning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataTransferMessage {
    pub vendor_id: String,
    pub message_id: Option<String>,
    /// Raw JSON payload. Always `None` on the wire today - see this module's docs.
    pub data: Option<String>,
}

/// The response to a [`DataTransferMessage`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataTransferResult {
    pub outcome: DataTransferOutcome,
    /// Raw JSON payload. Always `None` on the wire today - see this module's docs.
    pub data: Option<String>,
}

/// Sends a `DataTransfer` to the CSMS. Implemented per protocol version (see the `ocpp_2_1`
/// module), mirroring [`crate::authorization::Authorizer`] - called directly by integrator code
/// that wants to use a vendor extension, not spawned as a background loop by `setup()`.
#[async_trait::async_trait]
pub trait DataTransferSender {
    type Error: core::error::Error + Send + Sync + 'static;

    async fn transfer_data(
        &self,
        message: &DataTransferMessage,
    ) -> Result<DataTransferResult, Self::Error>;
}

/// Vendor-specific logic for answering a CSMS-initiated `DataTransfer`, supplied by the
/// integrator - this crate has no way to interpret `data` itself.
#[async_trait::async_trait]
pub trait DataTransferHandler {
    async fn handle_data_transfer(&self, message: DataTransferMessage) -> DataTransferResult;
}

/// Registers an integrator-supplied [`DataTransferHandler`] to answer every CSMS-initiated
/// `DataTransfer`. Implemented per protocol version (see the `ocpp_2_1` module). Not wired into
/// [`crate::setup`] - see this module's docs for why.
#[async_trait::async_trait]
pub trait DataTransferRegistrar {
    async fn register_data_transfer_handler<H>(&self, handler: H)
    where
        H: DataTransferHandler + Clone + Send + Sync + 'static;
}

#[cfg(test)]
mod tests {
    use super::{
        DataTransferHandler, DataTransferMessage, DataTransferOutcome, DataTransferResult,
    };

    #[derive(Clone)]
    struct EchoingVendorIdHandler;

    #[async_trait::async_trait]
    impl DataTransferHandler for EchoingVendorIdHandler {
        async fn handle_data_transfer(&self, message: DataTransferMessage) -> DataTransferResult {
            if message.vendor_id == "com.example.known" {
                DataTransferResult {
                    outcome: DataTransferOutcome::Accepted,
                    data: None,
                }
            } else {
                DataTransferResult {
                    outcome: DataTransferOutcome::UnknownVendorId,
                    data: None,
                }
            }
        }
    }

    #[tokio::test]
    async fn a_handler_can_accept_or_reject_based_on_the_message() {
        let handler = EchoingVendorIdHandler;

        let accepted = handler
            .handle_data_transfer(DataTransferMessage {
                vendor_id: "com.example.known".into(),
                message_id: None,
                data: None,
            })
            .await;
        assert_eq!(accepted.outcome, DataTransferOutcome::Accepted);

        let rejected = handler
            .handle_data_transfer(DataTransferMessage {
                vendor_id: "com.example.unknown".into(),
                message_id: None,
                data: None,
            })
            .await;
        assert_eq!(rejected.outcome, DataTransferOutcome::UnknownVendorId);
    }
}

#[cfg(feature = "ocpp_2_1")]
mod ocpp_2_1 {
    use super::{
        DataTransferHandler, DataTransferMessage, DataTransferOutcome, DataTransferRegistrar,
        DataTransferResult, DataTransferSender,
    };
    use alloc::boxed::Box;
    use alloc::string::ToString;
    use ocpp_client::ocpp_2_1::{OCPP2_1Client, OCPP2_1Error};
    use ocpp_client::ocpp_types::v21::common::DataTransferStatusEnum;
    use ocpp_client::ocpp_types::v21::{DataTransferRequest, DataTransferResponse};
    use ocpp_client::ClientError;

    fn map_status_to_outcome(status: DataTransferStatusEnum) -> DataTransferOutcome {
        match status {
            DataTransferStatusEnum::Accepted => DataTransferOutcome::Accepted,
            DataTransferStatusEnum::Rejected => DataTransferOutcome::Rejected,
            DataTransferStatusEnum::UnknownMessageId => DataTransferOutcome::UnknownMessageId,
            DataTransferStatusEnum::UnknownVendorId => DataTransferOutcome::UnknownVendorId,
        }
    }

    fn map_outcome_to_status(outcome: DataTransferOutcome) -> DataTransferStatusEnum {
        match outcome {
            DataTransferOutcome::Accepted => DataTransferStatusEnum::Accepted,
            DataTransferOutcome::Rejected => DataTransferStatusEnum::Rejected,
            DataTransferOutcome::UnknownMessageId => DataTransferStatusEnum::UnknownMessageId,
            DataTransferOutcome::UnknownVendorId => DataTransferStatusEnum::UnknownVendorId,
        }
    }

    /// `request.data` is always dropped - `ocpp-types`' `DataTransferRequest.data: Option<()>`
    /// can't carry a payload at all (see this module's top-level docs).
    fn map_request(request: &DataTransferRequest) -> DataTransferMessage {
        DataTransferMessage {
            vendor_id: request.vendor_id.to_string(),
            message_id: request.message_id.as_ref().map(|id| id.to_string()),
            data: None,
        }
    }

    #[async_trait::async_trait]
    impl DataTransferSender for OCPP2_1Client {
        type Error = ClientError<OCPP2_1Error>;

        async fn transfer_data(
            &self,
            message: &DataTransferMessage,
        ) -> Result<DataTransferResult, Self::Error> {
            let response = self
                .send_data_transfer(DataTransferRequest {
                    custom_data: None,
                    // Can't carry `message.data` - see this module's top-level docs.
                    data: None,
                    // Silently dropped if it doesn't fit OCPP's 50-byte bound - degrades to no
                    // sub-selector rather than failing the whole transfer.
                    message_id: message
                        .message_id
                        .as_deref()
                        .and_then(|id| heapless::String::try_from(id).ok()),
                    // Vendor ids are short, reverse-DNS-style strings (e.g.
                    // "com.example.charger") that always fit OCPP's 255-byte bound in practice.
                    vendor_id: heapless::String::try_from(message.vendor_id.as_str())
                        .expect("vendor id must fit OCPP's 255-byte bound"),
                })
                .await?;
            Ok(DataTransferResult {
                outcome: map_status_to_outcome(response.status),
                data: None,
            })
        }
    }

    #[async_trait::async_trait]
    impl DataTransferRegistrar for OCPP2_1Client {
        async fn register_data_transfer_handler<H>(&self, handler: H)
        where
            H: DataTransferHandler + Clone + Send + Sync + 'static,
        {
            self.on_data_transfer(move |request, _client| {
                let handler = handler.clone();
                async move {
                    let message = map_request(&request);
                    let result = handler.handle_data_transfer(message).await;
                    Ok(DataTransferResponse {
                        custom_data: None,
                        // Can't carry `result.data` - see this module's top-level docs.
                        data: None,
                        status: map_outcome_to_status(result.outcome),
                        status_info: None,
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
        fn every_wire_status_maps_to_the_matching_outcome() {
            assert_eq!(
                map_status_to_outcome(DataTransferStatusEnum::Accepted),
                DataTransferOutcome::Accepted
            );
            assert_eq!(
                map_status_to_outcome(DataTransferStatusEnum::Rejected),
                DataTransferOutcome::Rejected
            );
            assert_eq!(
                map_status_to_outcome(DataTransferStatusEnum::UnknownMessageId),
                DataTransferOutcome::UnknownMessageId
            );
            assert_eq!(
                map_status_to_outcome(DataTransferStatusEnum::UnknownVendorId),
                DataTransferOutcome::UnknownVendorId
            );
        }

        #[test]
        fn every_outcome_maps_to_the_matching_wire_status() {
            assert_eq!(
                map_outcome_to_status(DataTransferOutcome::Accepted),
                DataTransferStatusEnum::Accepted
            );
            assert_eq!(
                map_outcome_to_status(DataTransferOutcome::Rejected),
                DataTransferStatusEnum::Rejected
            );
            assert_eq!(
                map_outcome_to_status(DataTransferOutcome::UnknownMessageId),
                DataTransferStatusEnum::UnknownMessageId
            );
            assert_eq!(
                map_outcome_to_status(DataTransferOutcome::UnknownVendorId),
                DataTransferStatusEnum::UnknownVendorId
            );
        }

        fn request(vendor_id: &str, message_id: Option<&str>) -> DataTransferRequest {
            DataTransferRequest {
                custom_data: None,
                data: None,
                message_id: message_id.map(|id| heapless::String::try_from(id).unwrap()),
                vendor_id: heapless::String::try_from(vendor_id).unwrap(),
            }
        }

        #[test]
        fn a_request_maps_to_a_message_with_no_data() {
            let mapped = map_request(&request("com.example.charger", Some("Ping")));

            assert_eq!(mapped.vendor_id, "com.example.charger");
            assert_eq!(mapped.message_id.as_deref(), Some("Ping"));
            assert_eq!(mapped.data, None);
        }

        #[test]
        fn a_request_with_no_message_id_maps_to_none() {
            let mapped = map_request(&request("com.example.charger", None));

            assert_eq!(mapped.message_id, None);
        }
    }
}
