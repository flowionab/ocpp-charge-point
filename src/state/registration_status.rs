/// The CSMS's decision on a BootNotification, per the OCPP Provisioning functional block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationStatus {
    /// The charge point may proceed with normal operations.
    Accepted,
    /// The CSMS needs more time; the charge point must wait before retrying BootNotification.
    Pending,
    /// The CSMS refuses to register the charge point.
    Rejected,
}
