use alloc::string::String;

/// An identifier presented to authorize charging (OCPP `IdTokenType`) - the hidden id of an
/// RFID tag, an app-generated UUID, a vehicle's PnC identity, etc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdToken {
    /// Case-insensitive identifier value.
    pub value: String,
    pub kind: IdTokenKind,
}

/// How an [`IdToken`] was obtained, matching OCPP's `IdTokenType.type` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdTokenKind {
    Central,
    DirectPayment,
    EMAID,
    EVCCID,
    ISO14443,
    ISO15693,
    KeyCode,
    Local,
    MacAddress,
    NoAuthorization,
    Vin,
}
