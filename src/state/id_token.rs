use alloc::string::String;

/// An identifier presented to authorize charging (OCPP `IdTokenType`) - the hidden id of an
/// RFID tag, an app-generated UUID, a vehicle's PnC identity, etc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdToken {
    /// Case-insensitive identifier value.
    pub value: String,
    /// How this identifier was obtained.
    pub kind: IdTokenKind,
}

/// How an [`IdToken`] was obtained, matching OCPP's `IdTokenType.type` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdTokenKind {
    /// Assigned centrally by the CSMS rather than read from a physical medium.
    Central,
    /// A direct-payment identifier (e.g. a bank card used for ad hoc payment).
    DirectPayment,
    /// An ISO 15118 eMobility Account Identifier (contract identity for Plug & Charge).
    EMAID,
    /// An ISO 15118 EV Communication Controller identifier.
    EVCCID,
    /// An RFID identifier read via ISO 14443 (typically MIFARE-family cards).
    ISO14443,
    /// An RFID identifier read via ISO 15693 (vicinity cards).
    ISO15693,
    /// A manually entered key code.
    KeyCode,
    /// An identifier local to this charge point, not centrally registered.
    Local,
    /// A MAC address used as an identifier.
    MacAddress,
    /// No authorization required (e.g. a free-charging connector).
    NoAuthorization,
    /// A vehicle identification number.
    Vin,
}
