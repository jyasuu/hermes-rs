//! Inbound authentication: is this request really from the registered sender?
//!
//! Runs on the **raw body bytes** before JSON parsing, so an unauthenticated
//! caller learns nothing about parse errors, templates or targets.
//! All secret comparisons are constant-time.

use crate::config::{AuthConfig, BearerAuth, HmacAlgorithm, HmacAuth, SignatureEncoding};
use axum::http::{header, HeaderMap};
use base64::{engine::general_purpose::STANDARD, Engine};
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Why a request was rejected. Logged server-side only; the caller always
/// receives the same generic 401 so nothing can be probed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    MissingCredential,
    MalformedCredential,
    Mismatch,
}

impl AuthError {
    pub fn reason(self) -> &'static str {
        match self {
            AuthError::MissingCredential => "credential header missing",
            AuthError::MalformedCredential => "credential malformed",
            AuthError::Mismatch => "credential does not match",
        }
    }
}

pub fn verify(auth: &AuthConfig, headers: &HeaderMap, body: &[u8]) -> Result<(), AuthError> {
    match auth {
        AuthConfig::Hmac(a) => verify_hmac(a, headers, body),
        AuthConfig::Bearer(a) => verify_bearer(a, headers),
    }
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str, AuthError> {
    headers
        .get(name)
        .ok_or(AuthError::MissingCredential)?
        .to_str()
        .map(str::trim)
        .map_err(|_| AuthError::MalformedCredential)
}

fn verify_hmac(a: &HmacAuth, headers: &HeaderMap, body: &[u8]) -> Result<(), AuthError> {
    let raw = header_str(headers, &a.header)?;
    let encoded = raw
        .strip_prefix(a.expected_prefix().as_str())
        .ok_or(AuthError::MalformedCredential)?;
    let signature = match a.encoding {
        SignatureEncoding::Hex => hex::decode(encoded).map_err(|_| AuthError::MalformedCredential)?,
        SignatureEncoding::Base64 => STANDARD.decode(encoded).map_err(|_| AuthError::MalformedCredential)?,
    };

    // `verify_slice` compares in constant time.
    let ok = match a.algorithm {
        HmacAlgorithm::Sha256 => {
            let mut mac = Hmac::<Sha256>::new_from_slice(a.secret.as_bytes()).expect("HMAC accepts any key length");
            mac.update(body);
            mac.verify_slice(&signature).is_ok()
        }
        HmacAlgorithm::Sha1 => {
            let mut mac = Hmac::<sha1::Sha1>::new_from_slice(a.secret.as_bytes()).expect("HMAC accepts any key length");
            mac.update(body);
            mac.verify_slice(&signature).is_ok()
        }
    };
    if ok { Ok(()) } else { Err(AuthError::Mismatch) }
}

fn verify_bearer(a: &BearerAuth, headers: &HeaderMap) -> Result<(), AuthError> {
    let provided = match &a.header {
        None => {
            let v = header_str(headers, header::AUTHORIZATION.as_str())?;
            // The scheme name is case-insensitive (RFC 7235).
            match v.split_once(' ') {
                Some((scheme, token)) if scheme.eq_ignore_ascii_case("bearer") => token.trim(),
                _ => return Err(AuthError::MalformedCredential),
            }
        }
        Some(h) => header_str(headers, h)?,
    };
    if constant_time_eq(provided.as_bytes(), a.token.as_bytes()) {
        Ok(())
    } else {
        Err(AuthError::Mismatch)
    }
}

/// Equality that does not leak where the inputs differ *or* their lengths:
/// both sides are hashed to a fixed size first.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let (da, db) = (Sha256::digest(a), Sha256::digest(b));
    da.as_slice().ct_eq(db.as_slice()).into()
}

/// `Authorization: Bearer …` check for the metrics endpoint.
pub fn verify_bearer_token(headers: &HeaderMap, token: &str) -> bool {
    verify_bearer(&BearerAuth { token: token.to_string(), header: None }, headers).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hmac_auth(secret: &str) -> HmacAuth {
        HmacAuth {
            secret: secret.into(),
            header: "X-Hub-Signature-256".into(),
            algorithm: HmacAlgorithm::Sha256,
            encoding: SignatureEncoding::Hex,
            prefix: None,
        }
    }

    fn h(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.insert(*k, v.parse().unwrap());
        }
        m
    }

    // Test vector published in GitHub's webhook documentation.
    const GH_SECRET: &str = "It's a Secret to Everybody";
    const GH_BODY: &[u8] = b"Hello, World!";
    const GH_SIG: &str = "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";

    #[test]
    fn github_documented_vector_verifies() {
        let a = AuthConfig::Hmac(hmac_auth(GH_SECRET));
        assert_eq!(verify(&a, &h(&[("x-hub-signature-256", GH_SIG)]), GH_BODY), Ok(()));
    }

    #[test]
    fn header_name_is_case_insensitive_and_hex_may_be_uppercase() {
        let a = AuthConfig::Hmac(hmac_auth(GH_SECRET));
        let upper = format!("sha256={}", GH_SIG[7..].to_uppercase());
        assert_eq!(verify(&a, &h(&[("X-HUB-SIGNATURE-256", &upper)]), GH_BODY), Ok(()));
    }

    #[test]
    fn tampered_body_wrong_secret_and_missing_header_are_rejected() {
        let a = AuthConfig::Hmac(hmac_auth(GH_SECRET));
        let hdr = h(&[("x-hub-signature-256", GH_SIG)]);
        assert_eq!(verify(&a, &hdr, b"Hello, World?"), Err(AuthError::Mismatch));
        let wrong = AuthConfig::Hmac(hmac_auth("another secret"));
        assert_eq!(verify(&wrong, &hdr, GH_BODY), Err(AuthError::Mismatch));
        assert_eq!(verify(&a, &HeaderMap::new(), GH_BODY), Err(AuthError::MissingCredential));
    }

    #[test]
    fn malformed_signatures_are_rejected_without_panicking() {
        let a = AuthConfig::Hmac(hmac_auth(GH_SECRET));
        for bad in ["", "sha256=", "sha256=zz", "757107ea", "sha1=757107ea", "sha256=7571", "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e1"] {
            assert!(verify(&a, &h(&[("x-hub-signature-256", bad)]), GH_BODY).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn shopify_style_base64_without_prefix() {
        let mut cfg = hmac_auth("hush");
        cfg.header = "X-Shopify-Hmac-Sha256".into();
        cfg.encoding = SignatureEncoding::Base64;
        let mut mac = Hmac::<Sha256>::new_from_slice(b"hush").unwrap();
        mac.update(b"{\"id\":1}");
        let sig = STANDARD.encode(mac.finalize().into_bytes());
        let a = AuthConfig::Hmac(cfg);
        assert_eq!(verify(&a, &h(&[("x-shopify-hmac-sha256", &sig)]), b"{\"id\":1}"), Ok(()));
        assert!(verify(&a, &h(&[("x-shopify-hmac-sha256", &sig)]), b"{\"id\":2}").is_err());
    }

    #[test]
    fn legacy_sha1_and_custom_prefix() {
        let mut cfg = hmac_auth("k");
        cfg.algorithm = HmacAlgorithm::Sha1;
        cfg.header = "X-Hub-Signature".into();
        let mut mac = Hmac::<sha1::Sha1>::new_from_slice(b"k").unwrap();
        mac.update(b"body");
        let hexsig = hex::encode(mac.finalize().into_bytes());
        let a = AuthConfig::Hmac(cfg.clone());
        assert_eq!(verify(&a, &h(&[("x-hub-signature", &format!("sha1={hexsig}"))]), b"body"), Ok(()));
        cfg.prefix = Some("v1:".into());
        let a = AuthConfig::Hmac(cfg);
        assert_eq!(verify(&a, &h(&[("x-hub-signature", &format!("v1:{hexsig}"))]), b"body"), Ok(()));
    }

    #[test]
    fn bearer_authorization_header() {
        let a = AuthConfig::Bearer(BearerAuth { token: "tok-123".into(), header: None });
        assert_eq!(verify(&a, &h(&[("authorization", "Bearer tok-123")]), b""), Ok(()));
        assert_eq!(verify(&a, &h(&[("authorization", "bearer   tok-123")]), b""), Ok(()), "scheme is case-insensitive");
        assert_eq!(verify(&a, &h(&[("authorization", "Bearer nope")]), b""), Err(AuthError::Mismatch));
        assert_eq!(verify(&a, &h(&[("authorization", "tok-123")]), b""), Err(AuthError::MalformedCredential));
        assert_eq!(verify(&a, &h(&[("authorization", "Basic dG9rLTEyMw==")]), b""), Err(AuthError::MalformedCredential));
        assert_eq!(verify(&a, &HeaderMap::new(), b""), Err(AuthError::MissingCredential));
    }

    #[test]
    fn bearer_custom_header_like_gitlab() {
        let a = AuthConfig::Bearer(BearerAuth { token: "glpat".into(), header: Some("X-Gitlab-Token".into()) });
        assert_eq!(verify(&a, &h(&[("x-gitlab-token", "glpat")]), b""), Ok(()));
        assert_eq!(verify(&a, &h(&[("x-gitlab-token", "glpa")]), b""), Err(AuthError::Mismatch));
        assert_eq!(verify(&a, &h(&[("authorization", "Bearer glpat")]), b""), Err(AuthError::MissingCredential));
    }

    #[test]
    fn constant_time_eq_basics() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }
}
