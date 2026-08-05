use crate::hardware::Connector;
use alloc::boxed::Box;

/// A single EVSE (Electric Vehicle Supply Equipment) on the charge point: one or more physical
/// connectors sharing the same power source, addressed as `evse_id` throughout this crate's
/// internal state and OCPP messages.
#[async_trait::async_trait]
pub trait Evse<C: Connector> {
    /// This EVSE's connectors, in a fixed order matching `connector_id` addressing throughout
    /// this crate (index 0 is `connector_id` 0, and so on). The set of connectors is assumed
    /// fixed for the lifetime of the charge point - there is no hook for adding or removing one
    /// at runtime.
    async fn connectors(&self) -> &[C];
}
