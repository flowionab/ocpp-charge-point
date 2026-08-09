use alloc::string::String;
use core::fmt;

/// An identifier presented to authorize charging (OCPP `IdTokenType`) - the hidden id of an
/// RFID tag, an app-generated UUID, a vehicle's PnC identity, etc.
///
/// # Privacy
///
/// [`value`](Self::value) identifies a *person* - it is the number on the card in a driver's
/// wallet - so this type's [`Debug`] implementation is **deliberately not derived**: it renders
/// at most the last four characters (see [`redacted_value`](Self::redacted_value)). Every
/// `tracing` event that formats an `IdToken`, and every `{:?}` of a type that contains one
/// (notably [`ChargePointEvent`](crate::state::ChargePointEvent), which the charge point actor
/// logs on every applied event), is redacted by construction rather than by remembering to
/// redact at each call site.
///
/// This affects `Debug` only. [`Serialize`](serde::Serialize) still round-trips the full value,
/// because the authorization cache and the local authorization list persist these tokens through
/// `hardware::Storage` and must compare them exactly on the next presentation. Protocol paths are
/// likewise unaffected: an `Authorize` request carries the real value, and
/// `CustomerInformation` deliberately reports it in full - that is a data-subject access
/// response, which is the one place the value is the point.
///
/// Enabling the crate's off-by-default `unredacted-logs` feature restores the full value in
/// `Debug` output for local bring-up. Never ship an image with it on.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IdToken {
    /// Case-insensitive identifier value.
    pub value: String,
    /// How this identifier was obtained.
    pub kind: IdTokenKind,
}

impl IdToken {
    /// The identifier value as it should appear in a log: enough to tell two cards apart while
    /// troubleshooting, never enough to reconstruct the card.
    ///
    /// A value of more than four characters renders as `…` followed by its last four; a value of
    /// four or fewer renders as `****`, because a four-character suffix of a four-character token
    /// is the token. An empty value renders as an empty string.
    ///
    /// Counts [`char`]s, not bytes, so a multi-byte value is never split mid-character.
    ///
    /// With the crate's `unredacted-logs` feature on this returns the value verbatim.
    pub fn redacted_value(&self) -> String {
        #[cfg(feature = "unredacted-logs")]
        {
            self.value.clone()
        }
        #[cfg(not(feature = "unredacted-logs"))]
        {
            use alloc::string::ToString;

            let len = self.value.chars().count();
            if len == 0 {
                return String::new();
            }
            if len <= 4 {
                return "****".to_string();
            }
            let mut out = String::from("…");
            out.extend(self.value.chars().skip(len - 4));
            out
        }
    }
}

impl fmt::Debug for IdToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IdToken")
            .field("value", &self.redacted_value())
            .field("kind", &self.kind)
            .finish()
    }
}

/// How an [`IdToken`] was obtained, matching OCPP's `IdTokenType.type` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

#[cfg(test)]
mod redaction_tests {
    use super::*;
    use alloc::format;
    use alloc::string::ToString;

    fn token(value: &str) -> IdToken {
        IdToken {
            value: value.to_string(),
            kind: IdTokenKind::ISO14443,
        }
    }

    #[test]
    #[cfg(not(feature = "unredacted-logs"))]
    fn debug_output_never_contains_the_whole_card_number() {
        let rendered = format!("{:?}", token("04A1B2C3D4E5F6A7"));

        assert!(
            !rendered.contains("04A1B2C3D4E5F6A7"),
            "the full id token leaked into Debug output: {rendered}"
        );
    }

    #[test]
    #[cfg(not(feature = "unredacted-logs"))]
    fn debug_output_keeps_the_last_four_characters_and_the_kind() {
        let rendered = format!("{:?}", token("04A1B2C3D4E5F6A7"));

        // Enough to tell two cards apart in a log while troubleshooting a site, and enough to
        // know which reader produced it - which is the whole reason not to mask it outright.
        assert!(rendered.contains("F6A7"), "{rendered}");
        assert!(rendered.contains("ISO14443"), "{rendered}");
    }

    #[test]
    #[cfg(not(feature = "unredacted-logs"))]
    fn a_short_id_token_is_masked_completely() {
        // A four-character suffix of a four-character token is the token, so there is no useful
        // "last four" to show here - showing one would leak the whole value.
        let rendered = format!("{:?}", token("1234"));

        assert!(!rendered.contains("1234"), "{rendered}");
        assert!(rendered.contains("****"), "{rendered}");
    }

    #[test]
    #[cfg(not(feature = "unredacted-logs"))]
    fn a_multi_byte_value_is_never_split_mid_character() {
        // Byte slicing the last four bytes of this would panic; char counting must not.
        let rendered = format!("{:?}", token("naïvé-tökèn-ÅÄÖ"));

        assert!(rendered.contains("-ÅÄÖ"), "{rendered}");
        assert!(!rendered.contains("naïvé"), "{rendered}");
    }

    #[test]
    fn an_empty_value_renders_as_empty_rather_than_as_a_mask() {
        // `NoAuthorization` connectors present an empty token; there is nothing to hide, and
        // rendering `****` would imply a value was withheld.
        assert_eq!(
            IdToken {
                value: String::new(),
                kind: IdTokenKind::NoAuthorization,
            }
            .redacted_value(),
            ""
        );
    }

    #[test]
    fn redaction_is_a_debug_concern_only_and_never_touches_serialization() {
        // The authorization cache and local authorization list persist these through
        // `hardware::Storage` and must compare them exactly on the next presentation, so a
        // redacted round-trip would silently stop matching the card it was written for.
        let original = token("04A1B2C3D4E5F6A7");

        let encoded = serde_json::to_string(&original).expect("id tokens serialize");
        assert!(encoded.contains("04A1B2C3D4E5F6A7"), "{encoded}");

        let decoded: IdToken = serde_json::from_str(&encoded).expect("id tokens round-trip");
        assert_eq!(decoded, original);
    }

    #[test]
    #[cfg(feature = "unredacted-logs")]
    fn the_escape_hatch_restores_the_full_value_for_local_bring_up() {
        assert!(
            format!("{:?}", token("04A1B2C3D4E5F6A7")).contains("04A1B2C3D4E5F6A7"),
            "the `unredacted-logs` feature did not restore the full value"
        );
    }
}
