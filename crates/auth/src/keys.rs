//! Org keys: P-256 public keys as SPKI PEM (what macrofiche's config
//! holds, as the service stores only public keys), and the public half of
//! a PKCS#8 private key (`macrofiche pubkey`, for an operator who holds
//! fragment's `CODESTORAGE_PRIVATE_KEY`).

use base64::Engine as _;
use thiserror::Error;

/// The DER prefix of a P-256 `SubjectPublicKeyInfo`: the algorithm
/// (`id-ecPublicKey`, `prime256v1`) and the bit string's header, before
/// the 65-byte uncompressed point.
const SPKI_P256_PREFIX: [u8; 26] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];
/// An uncompressed P-256 point: `04 ‖ x ‖ y`.
pub const POINT_BYTES: usize = 65;
/// A PEM's bytes, as read.
pub const PEM_BYTES_MAX: usize = 16 * 1024;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum KeyError {
    #[error("not a PEM block of type {0}")]
    NotPem(&'static str),
    #[error("not a P-256 public key (SPKI)")]
    NotP256,
    #[error("not a PKCS#8 P-256 private key with its public key")]
    NotPkcs8,
}

/// The DER inside a PEM block of `label`, accepting the one-line form
/// with literal `\n` (how fragment's variables carry one).
fn pem_der(pem: &str, label: &'static str) -> Result<Vec<u8>, KeyError> {
    if pem.len() > PEM_BYTES_MAX {
        return Err(KeyError::NotPem(label));
    }
    let pem = pem.replace("\\n", "\n");
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let start = pem.find(&begin).ok_or(KeyError::NotPem(label))? + begin.len();
    let stop = pem[start..].find(&end).ok_or(KeyError::NotPem(label))? + start;
    let body: String = pem[start..stop].chars().filter(|c| !c.is_ascii_whitespace()).collect();
    base64::engine::general_purpose::STANDARD.decode(body).map_err(|_| KeyError::NotPem(label))
}

/// A P-256 public key's point from its SPKI PEM.
pub fn spki_point(pem: &str) -> Result<Vec<u8>, KeyError> {
    let der = pem_der(pem, "PUBLIC KEY")?;
    if der.len() != SPKI_P256_PREFIX.len() + POINT_BYTES || der[..SPKI_P256_PREFIX.len()] != SPKI_P256_PREFIX || der[SPKI_P256_PREFIX.len()] != 0x04 {
        return Err(KeyError::NotP256);
    }
    Ok(der[SPKI_P256_PREFIX.len()..].to_vec())
}

/// A point as SPKI PEM.
pub fn spki_pem(point: &[u8]) -> String {
    assert_eq!(point.len(), POINT_BYTES, "an uncompressed P-256 point");
    let mut der = SPKI_P256_PREFIX.to_vec();
    der.extend_from_slice(point);
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let lines: Vec<&str> = b64.as_bytes().chunks(64).map(|c| std::str::from_utf8(c).expect("base64 is ASCII")).collect();
    format!("-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n", lines.join("\n"))
}

/// The SPKI PEM of a PKCS#8 P-256 private key's public half.
pub fn public_of_pkcs8(pem: &str) -> Result<String, KeyError> {
    let der = pem_der(pem, "PRIVATE KEY")?;
    let rng = ring::rand::SystemRandom::new();
    let pair = ring::signature::EcdsaKeyPair::from_pkcs8(&ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING, &der, &rng).map_err(|_| KeyError::NotPkcs8)?;
    Ok(spki_pem(ring::signature::KeyPair::public_key(&pair).as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A P-256 key made by `openssl genpkey -algorithm EC -pkeyopt
    /// ec_paramgen_curve:P-256` for this test alone, and its public half
    /// by `openssl pkey -pubout`: the shapes an operator holds.
    const PRIVATE: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgb2R/GuKyKyBrWTyr
qE9qrhx2wn5x+V1SUBw1G+ltya6hRANCAARS/3y3kW+oKQdkarE+dgu3vSNg+j2m
umbgsgCl9NJi5ilcb2N0XyTuVvLgd/7BHvP74bG82wjTpDN2+nZklEcp
-----END PRIVATE KEY-----
";
    const PUBLIC: &str = "-----BEGIN PUBLIC KEY-----
MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEUv98t5FvqCkHZGqxPnYLt70jYPo9
prpm4LIApfTSYuYpXG9jdF8k7lby4Hf+wR7z++GxvNsI06Qzdvp2ZJRHKQ==
-----END PUBLIC KEY-----
";

    // Goal: a PKCS#8 key's public half round-trips as SPKI PEM, from
    // either PEM form fragment carries. Method: derive, parse back, and
    // the one-line form.
    #[test]
    fn public_halves() {
        let public = public_of_pkcs8(PRIVATE).unwrap();
        assert_eq!(public, PUBLIC, "openssl pkey -pubout's answer, byte for byte");
        let point = spki_point(&public).unwrap();
        assert_eq!(point.len(), POINT_BYTES);
        assert_eq!(spki_pem(&point), public);
        assert_eq!(public_of_pkcs8(&PRIVATE.replace('\n', "\\n")).unwrap(), public);
    }

    // Goal: anything but a P-256 key in the expected PEM is refused.
    #[test]
    fn refusals() {
        assert_eq!(spki_point("not a key"), Err(KeyError::NotPem("PUBLIC KEY")));
        assert_eq!(spki_point("-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----"), Err(KeyError::NotP256));
        assert_eq!(spki_point(PRIVATE), Err(KeyError::NotPem("PUBLIC KEY")));
        assert_eq!(public_of_pkcs8("-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----"), Err(KeyError::NotPkcs8));
        assert!(spki_point(&"x".repeat(PEM_BYTES_MAX + 1)).is_err());
    }
}
