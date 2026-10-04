//! code.storage's authentication as macrofiche applies it (docs/contract.md,
//! section 3): a bearer JWT, ES256 (ECDSA P-256 over SHA-256, the 64-byte
//! `r‖s`), verified against the public keys of the org its `iss` names;
//! scopes per route; the `repo` claim against the path; and the service's
//! status and code for each refusal. Pure: the clock is a parameter.

pub mod keys;

use base64::Engine as _;
use macrofiche_contract::limits::{TOKEN_BYTES_MAX, TOKEN_TTL_S_MAX};
use macrofiche_contract::Problem;
use serde::Deserialize;
use thiserror::Error;

/// A token's scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    GitRead,
    GitWrite,
    RepoWrite,
    OrgRead,
}

impl Scope {
    pub fn parse(s: &str) -> Option<Scope> {
        Some(match s {
            "git:read" => Scope::GitRead,
            "git:write" => Scope::GitWrite,
            "repo:write" => Scope::RepoWrite,
            "org:read" => Scope::OrgRead,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Scope::GitRead => "git:read",
            Scope::GitWrite => "git:write",
            Scope::RepoWrite => "repo:write",
            Scope::OrgRead => "org:read",
        }
    }

    /// Scopes that need a `repo` claim.
    fn repo_bound(self) -> bool {
        !matches!(self, Scope::OrgRead)
    }
}

/// One public key of an org.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicKey {
    /// Picks the key when a token's header names it; a token without a
    /// `kid` is tried against every key of its org.
    pub kid: Option<String>,
    /// The uncompressed point.
    pub point: Vec<u8>,
}

/// The orgs a node serves and their keys.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Keyring {
    orgs: Vec<(String, Vec<PublicKey>)>,
}

impl Keyring {
    pub fn add(&mut self, org: &str, keys: Vec<PublicKey>) {
        assert!(!keys.is_empty() && !self.orgs.iter().any(|(o, _)| o == org), "an org once, with a key");
        self.orgs.push((org.to_string(), keys));
    }

    fn keys(&self, org: &str) -> Option<&[PublicKey]> {
        self.orgs.iter().find(|(o, _)| o == org).map(|(_, k)| k.as_slice())
    }
}

/// A verified token's claims.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claims {
    pub org: String,
    pub sub: String,
    pub repo: Option<String>,
    pub scopes: Vec<Scope>,
    pub exp: i64,
}

/// Why a request is refused, with the service's status for each
/// (contract section 3, the table).
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum AuthError {
    #[error("missing bearer token")]
    NoBearer,
    /// A bad signature, an expired token, an unknown org, or a malformed
    /// one: one answer, so a probe learns nothing from which.
    #[error("Invalid or expired token")]
    Invalid(&'static str),
    #[error("the token lacks the {0} scope")]
    InsufficientScope(&'static str),
    #[error("the token has no repo claim")]
    RepoClaimMissing,
    #[error("repository not found")]
    RepoMismatch,
    #[error("repo_name must equal the token's repo claim")]
    RepoNameMismatch,
}

impl AuthError {
    pub fn problem(&self) -> Problem {
        let (status, code) = match self {
            AuthError::NoBearer => (401, "unauthorized"),
            AuthError::Invalid(_) => (403, "forbidden"),
            AuthError::InsufficientScope(_) => (403, "insufficient_scope"),
            AuthError::RepoClaimMissing => (400, "repo_claim_missing"),
            AuthError::RepoMismatch => (404, "repository_not_found"),
            AuthError::RepoNameMismatch => (400, "repo_name_mismatch"),
        };
        Problem::new(status, code, self.to_string())
    }
}

#[derive(Deserialize)]
struct Header {
    alg: String,
    #[serde(default)]
    typ: Option<String>,
    #[serde(default)]
    kid: Option<String>,
}

#[derive(Deserialize)]
struct RawClaims {
    iss: Option<String>,
    sub: Option<String>,
    #[serde(default)]
    repo: Option<String>,
    scopes: Option<Vec<String>>,
    #[serde(default)]
    iat: Option<i64>,
    exp: Option<i64>,
}

/// The token from an `Authorization` header: `Bearer <token>`.
pub fn bearer(header: Option<&str>) -> Result<&str, AuthError> {
    let h = header.ok_or(AuthError::NoBearer)?.trim();
    let (scheme, token) = h.split_once(' ').ok_or(AuthError::NoBearer)?;
    if !scheme.eq_ignore_ascii_case("bearer") || token.trim().is_empty() {
        return Err(AuthError::NoBearer);
    }
    Ok(token.trim())
}

/// Verifies `token` at `now` (unix seconds): its signature under a key of
/// the org its `iss` names, its expiry (in the future, at most
/// `TOKEN_TTL_S_MAX` ahead), and its claims' shape.
pub fn verify(ring: &Keyring, token: &str, now: i64) -> Result<Claims, AuthError> {
    use AuthError::Invalid;
    if token.len() > TOKEN_BYTES_MAX {
        return Err(Invalid("too long"));
    }
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let mut parts = token.split('.');
    let (Some(h), Some(c), Some(s), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else { return Err(Invalid("not three parts")) };
    let header: Header = serde_json::from_slice(&b64.decode(h).map_err(|_| Invalid("header"))?).map_err(|_| Invalid("header"))?;
    if header.alg != "ES256" || header.typ.as_deref().is_some_and(|t| !t.eq_ignore_ascii_case("JWT")) {
        return Err(Invalid("not ES256"));
    }
    let raw: RawClaims = serde_json::from_slice(&b64.decode(c).map_err(|_| Invalid("claims"))?).map_err(|_| Invalid("claims"))?;
    let org = raw.iss.ok_or(Invalid("no iss"))?;
    let keys = ring.keys(&org).ok_or(Invalid("unknown org"))?;
    let sig = b64.decode(s).map_err(|_| Invalid("signature"))?;
    let signed = &token[..h.len() + 1 + c.len()];
    let candidates: Vec<&PublicKey> = match &header.kid {
        Some(kid) => keys.iter().filter(|k| k.kid.as_deref() == Some(kid)).collect(),
        None => keys.iter().collect(),
    };
    let verified = candidates.iter().any(|k| ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_FIXED, &k.point).verify(signed.as_bytes(), &sig).is_ok());
    if !verified {
        return Err(Invalid("signature"));
    }
    let exp = raw.exp.ok_or(Invalid("no exp"))?;
    if exp <= now {
        return Err(Invalid("expired"));
    }
    if exp - now > TOKEN_TTL_S_MAX || raw.iat.is_some_and(|iat| iat > exp) {
        return Err(Invalid("lifetime"));
    }
    let sub = raw.sub.ok_or(Invalid("no sub"))?;
    let mut scopes = Vec::new();
    for name in raw.scopes.ok_or(Invalid("no scopes"))? {
        let scope = Scope::parse(&name).ok_or(Invalid("unknown scope"))?;
        if !scopes.contains(&scope) {
            scopes.push(scope);
        }
    }
    let repo = raw.repo.filter(|r| !r.is_empty());
    if repo.is_none() && scopes.iter().any(|s| s.repo_bound()) {
        return Err(AuthError::RepoClaimMissing);
    }
    Ok(Claims { org, sub, repo, scopes, exp })
}

/// Whether verified `claims` may call a route that needs `scope`, on the
/// repository `path_repo` names (none for an org's routes). A token for
/// another repository reads as that repository not existing.
pub fn authorize(claims: &Claims, scope: Scope, path_repo: Option<&str>) -> Result<(), AuthError> {
    if !claims.scopes.contains(&scope) {
        return Err(AuthError::InsufficientScope(scope.name()));
    }
    if let Some(repo) = path_repo {
        if claims.repo.as_deref() != Some(repo) {
            return Err(AuthError::RepoMismatch);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};

    fn pair() -> EcdsaKeyPair {
        // A fresh key, made by ring.
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap()
    }

    fn sign(k: &EcdsaKeyPair, header: &str, claims: &serde_json::Value) -> String {
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let input = format!("{}.{}", b64.encode(header), b64.encode(claims.to_string()));
        let sig = k.sign(&ring::rand::SystemRandom::new(), input.as_bytes()).unwrap();
        format!("{input}.{}", b64.encode(sig.as_ref()))
    }

    const H: &str = r#"{"alg":"ES256","typ":"JWT"}"#;
    const NOW: i64 = 1_790_000_000;

    fn claims(repo: &str, scopes: &[&str]) -> serde_json::Value {
        serde_json::json!({"iss": "fragment-dev", "sub": "fragment-runtime", "repo": repo, "scopes": scopes, "iat": NOW, "exp": NOW + 300})
    }

    fn ring_of(k: &EcdsaKeyPair) -> Keyring {
        let mut r = Keyring::default();
        r.add("fragment-dev", vec![PublicKey { kid: None, point: k.public_key().as_ref().to_vec() }]);
        r
    }

    // Goal: fragment's tokens pass: the cell's (one scope, 300 s) and a
    // storage token (both git scopes, 900 s), each the shape
    // `fragment_core::codestorage::OrgKey::token` signs. Method: signed
    // here with a fresh key, verified, authorized for their routes.
    #[test]
    fn fragments_tokens_pass() {
        let k = pair();
        let ring = ring_of(&k);
        let t = sign(&k, H, &claims("todo--paul", &["git:read"]));
        let c = verify(&ring, &t, NOW).unwrap();
        assert_eq!((c.org.as_str(), c.repo.as_deref(), c.scopes.as_slice()), ("fragment-dev", Some("todo--paul"), &[Scope::GitRead][..]));
        authorize(&c, Scope::GitRead, Some("todo--paul")).unwrap();
        let mut storage = claims("todo--paul", &["git:read", "git:write"]);
        storage["sub"] = "editor:ab12".into();
        storage["exp"] = (NOW + 900).into();
        let c = verify(&ring, &sign(&k, H, &storage), NOW).unwrap();
        authorize(&c, Scope::GitWrite, Some("todo--paul")).unwrap();
        // an org-level token: the repo claim is the org, or absent
        let c = verify(&ring, &sign(&k, H, &claims("fragment-dev", &["org:read"])), NOW).unwrap();
        authorize(&c, Scope::OrgRead, None).unwrap();
        let mut bare = claims("", &["org:read"]);
        bare.as_object_mut().unwrap().remove("repo");
        assert!(verify(&ring, &sign(&k, H, &bare), NOW).is_ok());
        // a kid picks its key
        let mut kr = Keyring::default();
        kr.add("fragment-dev", vec![PublicKey { kid: Some("old".into()), point: pair().public_key().as_ref().to_vec() }, PublicKey { kid: Some("new".into()), point: k.public_key().as_ref().to_vec() }]);
        assert!(verify(&kr, &sign(&k, r#"{"alg":"ES256","kid":"new"}"#, &claims("r", &["git:read"])), NOW).is_ok());
        assert!(verify(&kr, &sign(&k, r#"{"alg":"ES256","kid":"old"}"#, &claims("r", &["git:read"])), NOW).is_err());
        assert!(verify(&kr, &sign(&k, H, &claims("r", &["git:read"])), NOW).is_ok(), "no kid: every key is tried");
    }

    // Goal: each refusal gets the service's status and code. Method: one
    // token per fault.
    #[test]
    fn refusals_get_the_services_statuses() {
        let k = pair();
        let ring = ring_of(&k);
        let status = |e: AuthError| (e.problem().status, e.problem().code);
        assert_eq!(bearer(None).map_err(status), Err((401, "unauthorized")));
        assert_eq!(bearer(Some("Basic abc")).map_err(status), Err((401, "unauthorized")));
        assert_eq!(bearer(Some("bearer  t ")), Ok("t"));
        let invalid = |t: &str| verify(&ring, t, NOW).map_err(status);
        assert_eq!(invalid("garbage"), Err((403, "forbidden")));
        assert_eq!(invalid(&sign(&pair(), H, &claims("r", &["git:read"]))), Err((403, "forbidden")), "another key");
        let mut c = claims("r", &["git:read"]);
        c["exp"] = NOW.into();
        assert_eq!(invalid(&sign(&k, H, &c)), Err((403, "forbidden")), "expired");
        c["exp"] = (NOW + TOKEN_TTL_S_MAX + 1).into();
        assert_eq!(invalid(&sign(&k, H, &c)), Err((403, "forbidden")), "too long-lived");
        let mut c = claims("r", &["git:read"]);
        c["iss"] = "other-org".into();
        assert_eq!(invalid(&sign(&k, H, &c)), Err((403, "forbidden")), "another org");
        assert_eq!(invalid(&sign(&k, H, &claims("r", &["git:admin"]))), Err((403, "forbidden")), "an unknown scope");
        assert_eq!(invalid(&sign(&k, r#"{"alg":"HS256"}"#, &claims("r", &["git:read"]))), Err((403, "forbidden")), "not ES256");
        let mut c = claims("r", &["git:read"]);
        c.as_object_mut().unwrap().remove("sub");
        assert_eq!(invalid(&sign(&k, H, &c)), Err((403, "forbidden")), "no sub");
        assert_eq!(invalid(&sign(&k, H, &claims("", &["git:read"]))), Err((400, "repo_claim_missing")));
        let c = verify(&ring, &sign(&k, H, &claims("r", &["git:read"])), NOW).unwrap();
        assert_eq!(authorize(&c, Scope::GitWrite, Some("r")).map_err(status), Err((403, "insufficient_scope")), "no scope implies another");
        assert_eq!(authorize(&c, Scope::GitRead, Some("s")).map_err(status), Err((404, "repository_not_found")));
        assert_eq!(status(AuthError::RepoNameMismatch), (400, "repo_name_mismatch"));
        assert_eq!(AuthError::Invalid("x").to_string(), "Invalid or expired token");
        // a tampered payload under a valid signature
        let t = sign(&k, H, &claims("r", &["git:read"]));
        let mut parts: Vec<String> = t.split('.').map(str::to_string).collect();
        parts[1] = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims("s", &["git:read"]).to_string());
        assert_eq!(invalid(&parts.join(".")), Err((403, "forbidden")));
    }
}
