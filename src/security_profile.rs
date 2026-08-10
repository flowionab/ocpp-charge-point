//! OCPP security profiles and the credentials they need (`docs/PRODUCTION-ROADMAP.md` F1/F3).
//!
//! OCPP defines three, in increasing strength, and which one is in force is a property of the
//! *connection* rather than of a message: see [`SecurityProfile`].
//!
//! # The rule this module exists to enforce
//!
//! A charge point's security profile may be **raised** over OCPP but essentially never lowered
//! (spec §A05): dropping to profile 1 is forbidden outright, and 3 → 2 only where the operator has
//! explicitly opted in via `AllowSecurityProfileDowngrade`. The reason is worth stating, because
//! it is the whole point of the check: the CSMS connection is the channel an attacker would use to
//! weaken the charge point, and a `SetNetworkProfile` that could silently move a TLS station onto
//! plaintext would turn a single compromised CSMS credential into a fleet-wide downgrade to
//! cleartext. [`SecurityProfileChange::evaluate`] is that gate, and
//! [`crate::network_profile::handle_set_network_profile`] applies it.
//!
//! # What this crate can and cannot do about credentials
//!
//! [`BasicAuthPassword`] validates and holds the profile 1/2 password to OCPP's own rules, and
//! [`ChargePointIdentity`] the username those profiles pair it with. What is deliberately *not*
//! here is anything needing a certificate - profile 3's client certificate, the CSMS trust store,
//! and the TLS wiring itself live in [`crate::certificates`] and [`crate::mutual_tls`] (F1.3),
//! which this crate can now run end to end. What [`SecurityProfile::is_implemented`] says is
//! narrower than "this station is currently running mutual TLS" - see its own docs and
//! [`SecurityProfile::is_usable`] for the distinction between "this crate can wire profile 3 at
//! all" and "this particular station has a certificate installed to present".

use alloc::string::String;

/// One of OCPP's three security profiles.
///
/// Ordered, and `PartialOrd` is derived deliberately: "may not be downgraded" is a comparison, and
/// deriving it keeps that comparison from drifting away from the numbering the spec assigns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SecurityProfile {
    /// **1 - Unsecured transport with Basic Authentication.** The charge point authenticates with
    /// HTTP Basic; nothing authenticates the CSMS and nothing is encrypted. The spec says this
    /// SHOULD only be used on already-trusted networks (A00.FR.201/206) - a VPN, typically.
    UnsecuredBasicAuth,
    /// **2 - TLS with Basic Authentication.** The CSMS authenticates with a TLS server
    /// certificate; the charge point still authenticates with HTTP Basic, now sent encrypted.
    TlsBasicAuth,
    /// **3 - TLS with client-side certificates.** Both ends authenticate with certificates. This
    /// crate can wire it end to end ([F1.3](crate), [`crate::mutual_tls`]) - but a *particular*
    /// station still needs a client certificate installed and a key to sign with before it can
    /// actually run it. See [`Self::is_implemented`] and [`Self::is_usable`] for that
    /// distinction.
    TlsMutualAuth,
}

impl SecurityProfile {
    /// The profile with OCPP's number, or `None` for a number OCPP does not define.
    pub fn from_number(number: u8) -> Option<Self> {
        match number {
            1 => Some(Self::UnsecuredBasicAuth),
            2 => Some(Self::TlsBasicAuth),
            3 => Some(Self::TlsMutualAuth),
            _ => None,
        }
    }

    /// OCPP's number for this profile.
    pub fn number(&self) -> u8 {
        match self {
            Self::UnsecuredBasicAuth => 1,
            Self::TlsBasicAuth => 2,
            Self::TlsMutualAuth => 3,
        }
    }

    /// Whether this profile needs HTTP Basic credentials.
    pub fn needs_basic_auth(&self) -> bool {
        matches!(self, Self::UnsecuredBasicAuth | Self::TlsBasicAuth)
    }

    /// Whether the connection is encrypted.
    pub fn is_encrypted(&self) -> bool {
        matches!(self, Self::TlsBasicAuth | Self::TlsMutualAuth)
    }

    /// Whether *this crate build* has the machinery to run this profile end to end at all.
    ///
    /// This is a statement about the *code*, not about any particular station: it answers "does
    /// this crate know how to speak this profile" rather than "can this station present a
    /// certificate right now" - see [`Self::is_usable`] for the latter, which is almost always
    /// the question that actually matters before dialing. All three profiles are implemented as
    /// of [F1.3](crate): 1 and 2 through Basic credentials and a TLS trust configuration reaching
    /// the transport, and 3 through the certificate store (B4.1), key storage (F2.4) and
    /// [`crate::mutual_tls`]'s rustls wiring (F1.3) together.
    pub fn is_implemented(&self) -> bool {
        true
    }

    /// Whether *this station* can actually run this profile right now.
    ///
    /// For profiles 1 and 2 this is always `true` - [`Self::is_implemented`] already covers
    /// everything they need. For profile 3, it is exactly `has_charging_station_certificate`:
    /// mutual TLS needs an installed [`CertificateUse::ChargingStation`](
    /// crate::hardware::CertificateUse) chain and a key to sign with
    /// ([`crate::hardware::CertificateStore::has_client_private_key`]), and code existing to wire one up
    /// (`is_implemented`) says nothing about whether *this* station has actually obtained one -
    /// see [`crate::mutual_tls`]'s module docs for why a station with neither is refused a
    /// profile-3 connection rather than silently handed a weaker one. Callers are expected to
    /// check this (with `has_charging_station_certificate` from
    /// `CertificateStore::has_client_private_key`/[`crate::mutual_tls::client_config`]'s success) before
    /// choosing to dial with this profile at all.
    pub fn is_usable(&self, has_charging_station_certificate: bool) -> bool {
        match self {
            Self::UnsecuredBasicAuth | Self::TlsBasicAuth => true,
            Self::TlsMutualAuth => has_charging_station_certificate,
        }
    }
}

/// Whether a proposed change of security profile is allowed, and why not when it isn't.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityProfileChange {
    /// The change may go ahead.
    Allowed,
    /// The change would move to a profile OCPP does not define.
    UnknownProfile,
    /// The change would drop to profile 1. Forbidden unconditionally (§A05): whatever else an
    /// operator has opted into, OCPP never allows a secured station to be returned to plaintext
    /// over OCPP itself.
    DowngradeToUnsecured,
    /// The change would lower the profile (3 → 2) without `AllowSecurityProfileDowngrade` set.
    DowngradeNotAllowed,
}

impl SecurityProfileChange {
    /// Whether this outcome permits the change.
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed)
    }

    /// Evaluates a move from `current` to `proposed` under §A05.
    ///
    /// `downgrade_allowed` is the operator's `AllowSecurityProfileDowngrade`, and it only ever
    /// unlocks 3 → 2. A move to profile 1 stays refused with it set, because the spec's wording is
    /// explicit that even then "it is not allowed to revert from profile 2 or profile 3 to
    /// security profile 1".
    ///
    /// Raising is always allowed, and so is staying put - a CSMS rewriting a profile slot without
    /// touching its security profile must not be refused for it.
    pub fn evaluate(
        current: SecurityProfile,
        proposed: Option<SecurityProfile>,
        downgrade_allowed: bool,
    ) -> Self {
        let Some(proposed) = proposed else {
            return Self::UnknownProfile;
        };
        if proposed >= current {
            return Self::Allowed;
        }
        if proposed == SecurityProfile::UnsecuredBasicAuth {
            return Self::DowngradeToUnsecured;
        }
        if downgrade_allowed {
            Self::Allowed
        } else {
            Self::DowngradeNotAllowed
        }
    }
}

/// The charge point's identity, which doubles as the HTTP Basic username on profiles 1 and 2.
///
/// OCPP requires the username to *be* the identity the charge point uses in its OCPP-J connection
/// URL (A00.FR.204), and forbids a `:` in it - Basic authentication joins username and password
/// with a colon, so an identity containing one would let the CSMS split the pair in the wrong
/// place. That is a parsing ambiguity with an authentication outcome, so it is refused at
/// construction rather than trusted to be absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChargePointIdentity(String);

/// Why a [`ChargePointIdentity`] was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityError {
    /// The identity was empty.
    Empty,
    /// The identity contained `:`, which Basic authentication cannot encode unambiguously
    /// (A00.FR.204).
    ContainsColon,
}

impl ChargePointIdentity {
    /// Validates and wraps `identity`.
    pub fn new(identity: &str) -> Result<Self, IdentityError> {
        if identity.is_empty() {
            return Err(IdentityError::Empty);
        }
        if identity.contains(':') {
            return Err(IdentityError::ContainsColon);
        }
        Ok(Self(identity.into()))
    }

    /// The identity as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The shortest Basic-auth password OCPP permits (A00.FR.205).
pub const MIN_BASIC_AUTH_PASSWORD_LEN: usize = 16;

/// The longest Basic-auth password OCPP permits (A00.FR.205).
pub const MAX_BASIC_AUTH_PASSWORD_LEN: usize = 64;

/// A validated HTTP Basic password for security profile 1 or 2.
///
/// OCPP is specific about this one (A00.FR.205): 16 to 64 characters, randomly chosen with
/// sufficient entropy, sent as UTF-8 rather than base64-of-octets. The length bounds are checked
/// here; entropy is not, because it cannot be - a 40-character string of `a` passes every
/// mechanical test there is. Generating the password is the operator's job, and this type's role
/// is to stop a too-short one reaching the wire, where it would be a credential the CSMS accepts
/// and an attacker enumerates.
///
/// `Debug` is implemented by hand to print nothing but a placeholder. A password that reaches a
/// log or a panic message has leaked, and every other type in this crate derives `Debug` freely -
/// so the protection has to live here rather than in a rule about call sites.
#[derive(Clone, PartialEq, Eq)]
pub struct BasicAuthPassword(String);

/// Why a [`BasicAuthPassword`] was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BasicAuthPasswordError {
    /// Shorter than [`MIN_BASIC_AUTH_PASSWORD_LEN`].
    TooShort,
    /// Longer than [`MAX_BASIC_AUTH_PASSWORD_LEN`].
    TooLong,
}

impl BasicAuthPassword {
    /// Validates and wraps `password` per A00.FR.205.
    pub fn new(password: &str) -> Result<Self, BasicAuthPasswordError> {
        // Counted in characters rather than bytes: the spec's bound is on a UTF-8 *string*, and a
        // password of 20 multi-byte characters is 20 characters however many bytes it occupies.
        let length = password.chars().count();
        if length < MIN_BASIC_AUTH_PASSWORD_LEN {
            return Err(BasicAuthPasswordError::TooShort);
        }
        if length > MAX_BASIC_AUTH_PASSWORD_LEN {
            return Err(BasicAuthPasswordError::TooLong);
        }
        Ok(Self(password.into()))
    }

    /// The password itself, for handing to the transport.
    ///
    /// Named `expose` rather than `as_str` on purpose: every call site is a place a credential
    /// leaves this type, and it should be obvious in review which ones those are.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Debug for BasicAuthPassword {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("BasicAuthPassword(<redacted>)")
    }
}

/// The TLS versions a charge point may negotiate, and the check that it did.
///
/// OCPP requires TLS 1.2 or better and names `InvalidTLSVersion` as the security event for a CSMS
/// that offers less. This crate does not *negotiate* TLS - `ocpp-client`'s `rustls` transport
/// does, and modern rustls will not speak below 1.2 at all - so this exists to turn "the
/// connection came up on something too old" into the event OCPP asks for, for transports that can
/// report what they negotiated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TlsVersion {
    /// TLS 1.0 - below OCPP's floor.
    Tls1_0,
    /// TLS 1.1 - below OCPP's floor.
    Tls1_1,
    /// TLS 1.2 - OCPP's minimum.
    Tls1_2,
    /// TLS 1.3.
    Tls1_3,
}

impl TlsVersion {
    /// Whether this version meets OCPP's floor of 1.2.
    pub fn is_permitted(&self) -> bool {
        *self >= Self::Tls1_2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raising_the_security_profile_is_always_allowed() {
        for (current, proposed) in [
            (
                SecurityProfile::UnsecuredBasicAuth,
                SecurityProfile::TlsBasicAuth,
            ),
            (
                SecurityProfile::UnsecuredBasicAuth,
                SecurityProfile::TlsMutualAuth,
            ),
            (
                SecurityProfile::TlsBasicAuth,
                SecurityProfile::TlsMutualAuth,
            ),
        ] {
            assert_eq!(
                SecurityProfileChange::evaluate(current, Some(proposed), false),
                SecurityProfileChange::Allowed
            );
        }
    }

    #[test]
    fn staying_on_the_same_profile_is_not_a_downgrade() {
        // A CSMS rewriting a slot's URL without touching its security profile must not be refused
        // for it.
        assert_eq!(
            SecurityProfileChange::evaluate(
                SecurityProfile::TlsMutualAuth,
                Some(SecurityProfile::TlsMutualAuth),
                false
            ),
            SecurityProfileChange::Allowed
        );
    }

    #[test]
    fn dropping_to_plaintext_is_refused_even_with_downgrade_allowed() {
        // §A05 is explicit: even with `AllowSecurityProfileDowngrade` set, reverting from 2 or 3
        // to profile 1 is not allowed. This is the case that matters most - it is the one that
        // would turn a compromised CSMS credential into a fleet moved onto cleartext.
        for current in [
            SecurityProfile::TlsBasicAuth,
            SecurityProfile::TlsMutualAuth,
        ] {
            for downgrade_allowed in [false, true] {
                assert_eq!(
                    SecurityProfileChange::evaluate(
                        current,
                        Some(SecurityProfile::UnsecuredBasicAuth),
                        downgrade_allowed
                    ),
                    SecurityProfileChange::DowngradeToUnsecured,
                    "current={current:?} downgrade_allowed={downgrade_allowed}"
                );
            }
        }
    }

    #[test]
    fn three_to_two_needs_the_operator_to_have_opted_in() {
        assert_eq!(
            SecurityProfileChange::evaluate(
                SecurityProfile::TlsMutualAuth,
                Some(SecurityProfile::TlsBasicAuth),
                false
            ),
            SecurityProfileChange::DowngradeNotAllowed
        );
        // The one downgrade §A05 permits, and only on an explicit opt-in.
        assert_eq!(
            SecurityProfileChange::evaluate(
                SecurityProfile::TlsMutualAuth,
                Some(SecurityProfile::TlsBasicAuth),
                true
            ),
            SecurityProfileChange::Allowed
        );
    }

    #[test]
    fn a_profile_number_ocpp_does_not_define_is_refused_rather_than_guessed_at() {
        assert_eq!(SecurityProfile::from_number(0), None);
        assert_eq!(SecurityProfile::from_number(4), None);
        assert_eq!(
            SecurityProfileChange::evaluate(SecurityProfile::TlsBasicAuth, None, true),
            SecurityProfileChange::UnknownProfile
        );
    }

    #[test]
    fn every_profile_is_implemented_now_that_f1_3_has_landed() {
        // "Implemented" is about the crate, not any one station - see `is_usable` for the
        // station-specific question.
        assert!(SecurityProfile::UnsecuredBasicAuth.is_implemented());
        assert!(SecurityProfile::TlsBasicAuth.is_implemented());
        assert!(SecurityProfile::TlsMutualAuth.is_implemented());
    }

    #[test]
    fn profile_3_is_only_usable_with_a_charging_station_certificate() {
        // Profiles 1/2 need nothing beyond what `is_implemented` already covers.
        assert!(SecurityProfile::UnsecuredBasicAuth.is_usable(false));
        assert!(SecurityProfile::TlsBasicAuth.is_usable(false));

        // Profile 3 needs a certificate a *station* actually holds - code existing to wire one up
        // is not the same as this station having done so. Saying otherwise would have a station
        // behave as though it presents a certificate it does not have.
        assert!(!SecurityProfile::TlsMutualAuth.is_usable(false));
        assert!(SecurityProfile::TlsMutualAuth.is_usable(true));
    }

    #[test]
    fn profile_3_needs_no_basic_auth_credentials() {
        // Mutual TLS authenticates both ends with certificates; a caller building
        // `ConnectOptions` from the selected profile and consulting this before setting
        // `username`/`password` never sends a password on a profile-3 connection.
        assert!(!SecurityProfile::TlsMutualAuth.needs_basic_auth());
        assert!(SecurityProfile::UnsecuredBasicAuth.needs_basic_auth());
        assert!(SecurityProfile::TlsBasicAuth.needs_basic_auth());
    }

    #[test]
    fn an_identity_with_a_colon_is_refused_because_basic_auth_cannot_encode_it() {
        // Basic joins username and password with `:`, so the CSMS would split in the wrong place -
        // a parsing ambiguity with an authentication outcome (A00.FR.204).
        assert_eq!(
            ChargePointIdentity::new("CP:001"),
            Err(IdentityError::ContainsColon)
        );
        assert_eq!(ChargePointIdentity::new(""), Err(IdentityError::Empty));
        assert_eq!(
            ChargePointIdentity::new("CP-001").unwrap().as_str(),
            "CP-001"
        );
    }

    #[test]
    fn a_password_below_ocpps_floor_is_refused_before_it_can_reach_the_wire() {
        // 15 characters: one short. A too-short password is one the CSMS accepts and an attacker
        // enumerates, so it is refused here rather than at the far end.
        assert_eq!(
            BasicAuthPassword::new(&"a".repeat(15)),
            Err(BasicAuthPasswordError::TooShort)
        );
        assert!(BasicAuthPassword::new(&"a".repeat(16)).is_ok());
        assert!(BasicAuthPassword::new(&"a".repeat(64)).is_ok());
        assert_eq!(
            BasicAuthPassword::new(&"a".repeat(65)),
            Err(BasicAuthPasswordError::TooLong)
        );
    }

    #[test]
    fn password_length_is_counted_in_characters_not_bytes() {
        // 16 multi-byte characters is 16 characters, whatever it weighs. Counting bytes would
        // accept a 6-character password made of 3-byte glyphs.
        let sixteen_chars = "é".repeat(16);
        assert!(sixteen_chars.len() > 16, "should be multi-byte");
        assert!(BasicAuthPassword::new(&sixteen_chars).is_ok());

        let six_chars = "😀".repeat(6);
        assert!(six_chars.len() >= 16, "should look long enough in bytes");
        assert_eq!(
            BasicAuthPassword::new(&six_chars),
            Err(BasicAuthPasswordError::TooShort)
        );
    }

    #[test]
    fn a_password_never_prints_itself() {
        let password = BasicAuthPassword::new("s3cret-but-long-enough").unwrap();

        // Every other type here derives `Debug` freely, so a credential that printed itself would
        // reach a log the first time anything containing it was traced.
        let printed = alloc::format!("{password:?}");
        assert!(!printed.contains("s3cret"), "{printed}");
        assert_eq!(password.expose(), "s3cret-but-long-enough");
    }

    #[test]
    fn tls_below_1_2_is_not_permitted() {
        assert!(!TlsVersion::Tls1_0.is_permitted());
        assert!(!TlsVersion::Tls1_1.is_permitted());
        assert!(TlsVersion::Tls1_2.is_permitted());
        assert!(TlsVersion::Tls1_3.is_permitted());
    }
}
