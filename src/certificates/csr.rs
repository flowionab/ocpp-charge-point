//! PKCS#10 CSR assembly for `SignCertificate` (`docs/PRODUCTION-ROADMAP.md` B4.3).
//!
//! # Where the crypto-dependency line is drawn
//!
//! This crate carries no crypto dependency, deliberately and consistently - see
//! [`crate::hardware::CertificateStore`]'s module docs for why
//! [`crate::hardware::StoredCertificates`] does no X.509 parsing, and
//! [`crate::hardware::SoftwareCrypto`]'s docs for why asymmetric key generation
//! and signing are delegated to the integrator rather than pulled in as a dependency here. This
//! module draws the same line, but a CSR needs three things and only two of them are asymmetric
//! cryptography:
//!
//! 1. **DER-encoding the PKCS#10 structure itself** (RFC 2986's `CertificationRequestInfo` and
//!    `CertificationRequest`). This is plain, deterministic ASN.1 tag/length/value assembly - no
//!    secrets, no randomness, nothing an integrator's hardware could do that this crate cannot.
//!    It happens here, by hand, the same way `crate::wire` hand-assembles OCPP's own wire
//!    shapes rather than reaching for a schema-driven crate.
//! 2. **Hashing the encoded `CertificationRequestInfo`** into the digest
//!    [`KeyStore::sign`] expects. SHA-256 (the only digest this
//!    module currently produces - see [`build_signed_csr`]'s docs) is a fixed, public, keyless
//!    algorithm: computing it does not need a secure element, does not need randomness, and does
//!    not carry the export-control/audit weight asymmetric primitives do. It is small enough
//!    (`sha256`, below) to implement directly and pin down with test vectors, which is the same
//!    trade-off this crate already makes for its own [`crate::sync`] primitives rather than
//!    pulling in `embassy-sync`'s upstream equivalents wholesale. **This is the one exception to
//!    "no crypto in this crate"**, and it is exactly that narrow: no asymmetric math, no key
//!    material, ever touches this module.
//! 3. **Signing that digest** - the one step that genuinely needs the private key, and therefore
//!    the one step this module refuses to do itself. [`build_signed_csr`] calls
//!    [`KeyStore::sign`] with the digest it computed and nothing
//!    else; the private key never appears here, matching that trait's whole reason for existing.
//!
//! # The public-key encoding contract
//!
//! [`crate::hardware::PublicKey::bytes`] is documented as opaque, in whatever
//! encoding the `KeyStore` implementor chose - "common choices are SubjectPublicKeyInfo DER or a
//! raw EC point". A CSR's `subjectPKInfo` field is specifically the former: a complete
//! `SubjectPublicKeyInfo` DER `SEQUENCE` (algorithm identifier plus a `BIT STRING` public key).
//! **[`build_signed_csr`] requires [`crate::hardware::PublicKey::bytes`] to
//! already be that encoding** and embeds it verbatim, unparsed - the same "carried opaquely,
//! never parsed" stance [`crate::hardware::CertificateHashData`] documents.
//! A `KeyStore` whose [`GeneratedKeyPair`] instead produces a
//! raw EC point cannot be used with this function as-is; that is a real, documented limitation
//! rather than a silent miscompilation of the CSR, since this crate has no X.509/ASN.1 parser to
//! detect or convert the mismatch.
//!
//! # What this module deliberately does not remember
//!
//! [`build_signed_csr`] takes the [`GeneratedKeyPair`] it
//! signs with as an argument and does not persist it anywhere - once the CSR is built and sent,
//! nothing in this crate still knows which [`crate::hardware::KeyHandle`] produced it. That is
//! fine for B4.3's own scope (a fresh keypair for an initial CSR), but
//! [F3.2](crate)'s renewal has to sign a *replacement* certificate with the *same* key as the one
//! it is replacing, which means F3.2 (not this module) is responsible for recording the
//! association between an installed certificate's
//! [`crate::hardware::CertificateHashData`] and the
//! [`crate::hardware::KeyHandle`] used to request it, and for handing that handle back
//! to a future [`build_signed_csr`] call rather than generating a new key pair.

use alloc::string::String;
use alloc::vec::Vec;

use crate::hardware::{GeneratedKeyPair, KeyStore, SignatureAlgorithm};

// --- Minimal DER (ASN.1 Distinguished Encoding Rules) assembly -------------------------------
//
// Just enough to build a PKCS#10 CertificationRequest: tag+length+value framing, INTEGER, OID,
// UTF8String, BIT STRING, NULL, SEQUENCE, SET, and an empty context-constructed tag for the
// CSR's mandatory-but-empty `attributes` field. Not a general ASN.1 encoder - extend deliberately
// if a future task needs more of it (e.g. `CHOICE` support for other attribute types).

fn der_length(len: usize) -> Vec<u8> {
    if len < 0x80 {
        return alloc::vec![len as u8];
    }
    let bytes = len.to_be_bytes();
    let first_nonzero = bytes
        .iter()
        .position(|b| *b != 0)
        .unwrap_or(bytes.len() - 1);
    let significant = &bytes[first_nonzero..];
    let mut out = alloc::vec![0x80 | significant.len() as u8];
    out.extend_from_slice(significant);
    out
}

fn der_tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = alloc::vec![tag];
    out.extend(der_length(content.len()));
    out.extend_from_slice(content);
    out
}

fn der_sequence(content: &[u8]) -> Vec<u8> {
    der_tlv(0x30, content)
}

fn der_set(content: &[u8]) -> Vec<u8> {
    der_tlv(0x31, content)
}

fn der_integer_zero() -> Vec<u8> {
    der_tlv(0x02, &[0x00])
}

fn der_oid(content: &[u8]) -> Vec<u8> {
    der_tlv(0x06, content)
}

fn der_utf8_string(value: &str) -> Vec<u8> {
    der_tlv(0x0C, value.as_bytes())
}

fn der_null() -> Vec<u8> {
    der_tlv(0x05, &[])
}

fn der_bit_string(bytes: &[u8]) -> Vec<u8> {
    let mut content = Vec::with_capacity(bytes.len() + 1);
    content.push(0); // no unused trailing bits
    content.extend_from_slice(bytes);
    der_tlv(0x03, &content)
}

/// The CSR's mandatory `[0] IMPLICIT SET OF Attribute` field, always empty here: this module
/// asks for nothing beyond the subject and public key, so there is nothing to put in it.
fn der_empty_attributes() -> Vec<u8> {
    alloc::vec![0xA0, 0x00]
}

// Well-known DER content bytes (the OID value, without the universal tag/length that
// [`der_oid`] adds) for the X.520 attribute types a CSR subject uses, and the two signature
// algorithms [`build_signed_csr`] supports.
const OID_COMMON_NAME: &[u8] = &[0x55, 0x04, 0x03]; // 2.5.4.3
const OID_COUNTRY_NAME: &[u8] = &[0x55, 0x04, 0x06]; // 2.5.4.6
const OID_ORGANIZATION_NAME: &[u8] = &[0x55, 0x04, 0x0A]; // 2.5.4.10
const OID_ORGANIZATIONAL_UNIT_NAME: &[u8] = &[0x55, 0x04, 0x0B]; // 2.5.4.11
const OID_ECDSA_WITH_SHA256: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x02]; // 1.2.840.10045.4.3.2
const OID_SHA256_WITH_RSA_ENCRYPTION: &[u8] =
    &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x0B]; // 1.2.840.113549.1.1.11

fn attribute_type_and_value(oid: &[u8], value: &str) -> Vec<u8> {
    let mut content = der_oid(oid);
    content.extend(der_utf8_string(value));
    der_sequence(&content)
}

/// A single-valued `RelativeDistinguishedName` (`SET` of one `AttributeTypeAndValue`) - every
/// RDN a CSR subject built by this module ever needs, since [`CsrSubject`] has no multi-valued
/// attributes.
fn relative_distinguished_name(oid: &[u8], value: &str) -> Vec<u8> {
    der_set(&attribute_type_and_value(oid, value))
}

/// The CSR subject: a minimal Distinguished Name, since this crate has no policy opinion about
/// what a CSMS' CA requires beyond a common name - callers add the attributes their CA wants.
///
/// Built in the conventional broad-to-narrow RDN order (`C`, `O`, `OU`, `CN`) when present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsrSubject {
    /// `commonName` (2.5.4.3) - the only attribute every CSR here carries. OCPP's Security
    /// Whitepaper convention is the charge point's serial number or identity string; this crate
    /// takes whatever the caller supplies rather than assuming a source for it.
    pub common_name: String,
    /// `organizationName` (2.5.4.10), if the caller's CA policy wants one.
    pub organization_name: Option<String>,
    /// `organizationalUnitName` (2.5.4.11), if the caller's CA policy wants one.
    pub organizational_unit_name: Option<String>,
    /// `countryName` (2.5.4.6), if the caller's CA policy wants one.
    pub country_name: Option<String>,
}

impl CsrSubject {
    /// A subject with only a `commonName` set.
    pub fn new(common_name: impl Into<String>) -> Self {
        Self {
            common_name: common_name.into(),
            organization_name: None,
            organizational_unit_name: None,
            country_name: None,
        }
    }

    /// Sets `organizationName`.
    pub fn with_organization_name(mut self, organization_name: impl Into<String>) -> Self {
        self.organization_name = Some(organization_name.into());
        self
    }

    /// Sets `organizationalUnitName`.
    pub fn with_organizational_unit_name(
        mut self,
        organizational_unit_name: impl Into<String>,
    ) -> Self {
        self.organizational_unit_name = Some(organizational_unit_name.into());
        self
    }

    /// Sets `countryName`.
    pub fn with_country_name(mut self, country_name: impl Into<String>) -> Self {
        self.country_name = Some(country_name.into());
        self
    }

    fn to_der(&self) -> Vec<u8> {
        let mut rdns = Vec::new();
        if let Some(country) = &self.country_name {
            rdns.extend(relative_distinguished_name(OID_COUNTRY_NAME, country));
        }
        if let Some(organization) = &self.organization_name {
            rdns.extend(relative_distinguished_name(
                OID_ORGANIZATION_NAME,
                organization,
            ));
        }
        if let Some(unit) = &self.organizational_unit_name {
            rdns.extend(relative_distinguished_name(
                OID_ORGANIZATIONAL_UNIT_NAME,
                unit,
            ));
        }
        rdns.extend(relative_distinguished_name(
            OID_COMMON_NAME,
            &self.common_name,
        ));
        der_sequence(&rdns)
    }
}

/// The `AlgorithmIdentifier` a CSR's outer `signatureAlgorithm` field carries for `algorithm`, or
/// `None` if this module cannot produce the matching digest - see [`build_signed_csr`]'s docs.
fn signature_algorithm_identifier(algorithm: SignatureAlgorithm) -> Option<Vec<u8>> {
    match algorithm {
        // RFC 5758: ecdsa-with-SHA256's parameters are absent, not NULL.
        SignatureAlgorithm::EcdsaP256Sha256 => Some(der_sequence(&der_oid(OID_ECDSA_WITH_SHA256))),
        // RFC 4055/8017: RSA signature algorithm identifiers carry an explicit NULL parameter.
        SignatureAlgorithm::Rsa2048Sha256 | SignatureAlgorithm::Rsa3072Sha256 => {
            let mut content = der_oid(OID_SHA256_WITH_RSA_ENCRYPTION);
            content.extend(der_null());
            Some(der_sequence(&content))
        }
        // No in-crate SHA-384 implementation yet - see the module docs and `build_signed_csr`.
        SignatureAlgorithm::EcdsaP384Sha384 => None,
    }
}

/// Failure building or signing a CSR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CsrBuildError<E> {
    /// [`build_signed_csr`] does not (yet) have an in-crate digest implementation matching this
    /// key's [`SignatureAlgorithm`] - currently only the SHA-256 family
    /// ([`SignatureAlgorithm::EcdsaP256Sha256`], [`SignatureAlgorithm::Rsa2048Sha256`],
    /// [`SignatureAlgorithm::Rsa3072Sha256`]) is supported. See the module docs.
    UnsupportedAlgorithm(SignatureAlgorithm),
    /// The [`KeyStore`] failed to sign the CSR's digest.
    Signing(E),
}

impl<E: core::fmt::Display> core::fmt::Display for CsrBuildError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnsupportedAlgorithm(algorithm) => {
                write!(f, "no in-crate CSR digest implementation for {algorithm:?}")
            }
            Self::Signing(error) => write!(f, "the key store failed to sign the CSR: {error}"),
        }
    }
}

impl<E: core::fmt::Debug + core::fmt::Display> core::error::Error for CsrBuildError<E> {}

/// Builds a PEM-encoded PKCS#10 CSR (RFC 2986) for `key`, signed through `key_store` - never
/// touching the private key itself, only [`KeyStore::sign`].
///
/// `subject` becomes the CSR's `subject` field; `key.public_key.bytes` is embedded verbatim as
/// the CSR's `subjectPKInfo` (see the module docs for why it must already be a
/// `SubjectPublicKeyInfo` DER document). The digest signed is SHA-256 of the DER-encoded
/// `CertificationRequestInfo`, per RFC 2986 - the one hash this module implements; see the
/// module docs for why hashing (unlike signing) is done in-crate.
///
/// Returns [`CsrBuildError::UnsupportedAlgorithm`] for [`SignatureAlgorithm::EcdsaP384Sha384`],
/// which would need a SHA-384 implementation this module does not yet have.
pub async fn build_signed_csr<K: KeyStore>(
    key_store: &K,
    key: &GeneratedKeyPair,
    subject: &CsrSubject,
) -> Result<String, CsrBuildError<K::Error>> {
    let signature_algorithm_der = signature_algorithm_identifier(key.public_key.algorithm).ok_or(
        CsrBuildError::UnsupportedAlgorithm(key.public_key.algorithm),
    )?;

    let mut info_content = der_integer_zero();
    info_content.extend(subject.to_der());
    info_content.extend_from_slice(&key.public_key.bytes);
    info_content.extend(der_empty_attributes());
    let certification_request_info = der_sequence(&info_content);

    let digest = sha256(&certification_request_info);
    let signature = key_store
        .sign(&key.handle, &digest)
        .await
        .map_err(CsrBuildError::Signing)?;

    let mut request_content = certification_request_info;
    request_content.extend(signature_algorithm_der);
    request_content.extend(der_bit_string(&signature));
    let certification_request = der_sequence(&request_content);

    Ok(pem_encode("CERTIFICATE REQUEST", &certification_request))
}

// --- Base64/PEM -------------------------------------------------------------------------------
//
// No crypto involved - PEM is just base64 with a header/footer, and this crate has no base64
// dependency any more than it has a crypto one. Standard alphabet, `=` padding, 64-column wrap
// (RFC 7468 §2's recommendation and what every CSR-consuming tool expects).

fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied();
        let b2 = chunk.get(2).copied();
        let n =
            (u32::from(b0) << 16) | (u32::from(b1.unwrap_or(0)) << 8) | u32::from(b2.unwrap_or(0));
        out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        out.push(if b1.is_some() {
            ALPHABET[((n >> 6) & 0x3F) as usize] as char
        } else {
            '='
        });
        out.push(if b2.is_some() {
            ALPHABET[(n & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn pem_encode(label: &str, der: &[u8]) -> String {
    let body = base64_encode(der);
    let mut out = String::with_capacity(body.len() + body.len() / 64 + 64);
    out.push_str("-----BEGIN ");
    out.push_str(label);
    out.push_str("-----\n");
    for line in body.as_bytes().chunks(64) {
        // Safe: `body` is base64, which is ASCII.
        out.push_str(core::str::from_utf8(line).unwrap_or(""));
        out.push('\n');
    }
    out.push_str("-----END ");
    out.push_str(label);
    out.push_str("-----\n");
    out
}

// --- SHA-256 ------------------------------------------------------------------------------
//
// FIPS 180-4. See the module docs for why this one hash lives here rather than behind a trait
// or a dependency.

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

const SHA256_H0: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

fn sha256(data: &[u8]) -> Vec<u8> {
    let mut h = SHA256_H0;

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    for block in message.chunks(64) {
        let mut w = [0u32; 64];
        for (i, word) in w.iter_mut().enumerate().take(16) {
            let base = i * 4;
            *word = u32::from_be_bytes([
                block[base],
                block[base + 1],
                block[base + 2],
                block[base + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = Vec::with_capacity(32);
    for word in h {
        out.extend_from_slice(&word.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::{KeyHandle, PublicKey};
    use alloc::sync::Arc;
    use alloc::vec;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| alloc::format!("{b:02x}")).collect()
    }

    #[test]
    fn sha256_matches_the_empty_string_test_vector() {
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sha256_matches_the_abc_test_vector() {
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn sha256_matches_a_two_block_test_vector() {
        // FIPS 180-4's own multi-block example.
        assert_eq!(
            hex(&sha256(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn base64_pads_correctly_for_every_remainder() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct FakeError;
    impl core::fmt::Display for FakeError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str("fake key store error")
        }
    }
    impl core::error::Error for FakeError {}

    /// A [`KeyStore`] that only implements `sign`, returning a fixed, deterministic "signature"
    /// - enough to exercise [`build_signed_csr`]'s assembly without any real cryptography.
    struct FakeKeyStore {
        signature: Vec<u8>,
    }

    #[async_trait::async_trait]
    impl KeyStore for FakeKeyStore {
        type Error = FakeError;

        async fn generate_key_pair(
            &self,
            _algorithm: SignatureAlgorithm,
        ) -> Result<GeneratedKeyPair, Self::Error> {
            Err(FakeError)
        }

        async fn sign(&self, _handle: &KeyHandle, _digest: &[u8]) -> Result<Vec<u8>, Self::Error> {
            Ok(self.signature.clone())
        }

        async fn delete_key_pair(&self, _handle: &KeyHandle) -> Result<(), Self::Error> {
            Ok(())
        }

        async fn supported_algorithms(&self) -> Result<Vec<SignatureAlgorithm>, Self::Error> {
            Ok(vec![SignatureAlgorithm::EcdsaP256Sha256])
        }

        async fn backing(&self) -> Result<crate::hardware::KeyStoreBacking, Self::Error> {
            Ok(crate::hardware::KeyStoreBacking::Software)
        }
    }

    fn fake_spki() -> Vec<u8> {
        // Not a real SubjectPublicKeyInfo - just some bytes to prove they are carried through
        // verbatim (see the module docs on the encoding contract).
        vec![0x30, 0x03, 0x02, 0x01, 0x2A]
    }

    fn fake_key() -> GeneratedKeyPair {
        GeneratedKeyPair {
            handle: KeyHandle::new(vec![1, 2, 3]),
            public_key: PublicKey {
                algorithm: SignatureAlgorithm::EcdsaP256Sha256,
                bytes: fake_spki(),
            },
        }
    }

    #[tokio::test]
    async fn builds_a_pem_csr_with_the_right_framing() {
        let key_store = FakeKeyStore {
            signature: vec![0xAA; 8],
        };
        let key = fake_key();
        let subject = CsrSubject::new("CP-0001").with_organization_name("Acme");

        let pem = build_signed_csr(&key_store, &key, &subject).await.unwrap();

        assert!(pem.starts_with("-----BEGIN CERTIFICATE REQUEST-----\n"));
        assert!(
            pem.trim_end()
                .ends_with("-----END CERTIFICATE REQUEST-----")
        );
        // Every line except the header/footer is base64, at most 64 columns.
        for line in pem.lines() {
            if !line.starts_with("-----") {
                assert!(line.len() <= 64);
            }
        }
    }

    #[tokio::test]
    async fn the_public_key_bytes_are_embedded_verbatim_in_the_csr() {
        let key_store = FakeKeyStore {
            signature: vec![0xBB; 8],
        };
        let key = fake_key();
        let subject = CsrSubject::new("CP-0002");

        let pem = build_signed_csr(&key_store, &key, &subject).await.unwrap();
        let der = decode_pem_body(&pem);

        // The embedded SPKI bytes appear byte-for-byte somewhere in the encoded CSR.
        assert!(
            der.windows(fake_spki().len())
                .any(|window| window == fake_spki().as_slice())
        );
    }

    #[tokio::test]
    async fn unsupported_algorithm_is_rejected_before_any_signing_happens() {
        let key_store = Arc::new(FakeKeyStore {
            signature: vec![0xCC; 8],
        });
        let mut key = fake_key();
        key.public_key.algorithm = SignatureAlgorithm::EcdsaP384Sha384;

        let result = build_signed_csr(&*key_store, &key, &CsrSubject::new("CP-0003")).await;

        assert!(matches!(
            result,
            Err(CsrBuildError::UnsupportedAlgorithm(
                SignatureAlgorithm::EcdsaP384Sha384
            ))
        ));
    }

    fn decode_pem_body(pem: &str) -> Vec<u8> {
        let body: String = pem
            .lines()
            .filter(|line| !line.starts_with("-----"))
            .collect();
        decode_base64(&body)
    }

    fn decode_base64(input: &str) -> Vec<u8> {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = Vec::new();
        let mut buffer = 0u32;
        let mut bits = 0u32;
        for byte in input.bytes() {
            if byte == b'=' {
                break;
            }
            let value = ALPHABET.iter().position(|c| *c == byte).unwrap() as u32;
            buffer = (buffer << 6) | value;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push(((buffer >> bits) & 0xFF) as u8);
            }
        }
        out
    }
}
