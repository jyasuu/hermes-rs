//! Inbound authentication: is this request really from the registered sender?
//!
//! Runs on the **raw body bytes** before JSON parsing, so an unauthenticated
//! caller learns nothing about parse errors, templates or targets.
//! All secret comparisons are constant-time.

use crate::config::{
    AuthConfig, BearerAuth, HmacAlgorithm, HmacAuth, SignatureEncoding, SlackAuth, StripeAuth,
};
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
    /// Signature is genuine but the timestamp is outside the tolerance:
    /// a replay, or badly skewed clocks.
    Expired,
}

impl AuthError {
    pub fn reason(self) -> &'static str {
        match self {
            AuthError::MissingCredential => "credential header missing",
            AuthError::MalformedCredential => "credential malformed",
            AuthError::Mismatch => "credential does not match",
            AuthError::Expired => "signature valid but timestamp outside tolerance (replay or clock skew)",
        }
    }
}

pub fn verify(auth: &AuthConfig, headers: &HeaderMap, body: &[u8]) -> Result<(), AuthError> {
    verify_at(auth, headers, body, unix_now())
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Like [`verify`] with an explicit clock (unix seconds), for tests.
pub fn verify_at(auth: &AuthConfig, headers: &HeaderMap, body: &[u8], now: i64) -> Result<(), AuthError> {
    match auth {
        AuthConfig::Hmac(a) => verify_hmac(a, headers, body),
        AuthConfig::Bearer(a) => verify_bearer(a, headers),
        AuthConfig::Stripe(a) => verify_stripe(a, headers, body, now),
        AuthConfig::Slack(a) => verify_slack(a, headers, body, now),
    }
}

/// Signature first, freshness second: a forged request is `Mismatch` whatever
/// its timestamp, while `Expired` is reserved for genuinely signed requests
/// (which is what an operator debugging clock skew needs to see in the log).
fn check_fresh(timestamp: i64, now: i64, tolerance_secs: u64) -> Result<(), AuthError> {
    if (now - timestamp).unsigned_abs() <= tolerance_secs {
        Ok(())
    } else {
        Err(AuthError::Expired)
    }
}

fn verify_stripe(a: &StripeAuth, headers: &HeaderMap, body: &[u8], now: i64) -> Result<(), AuthError> {
    let raw = header_str(headers, &a.header)?;
    let mut timestamp: Option<i64> = None;
    let mut candidates: Vec<Vec<u8>> = Vec::new();
    for part in raw.split(',') {
        let (k, v) = part.trim().split_once('=').ok_or(AuthError::MalformedCredential)?;
        match k {
            "t" => timestamp = Some(v.parse().map_err(|_| AuthError::MalformedCredential)?),
            // Several v1 entries appear while a secret is being rolled; any may match.
            "v1" => {
                if let Ok(bytes) = hex::decode(v) {
                    candidates.push(bytes);
                }
            }
            _ => {} // v0 (test-mode scheme) and future versions are ignored
        }
    }
    let timestamp = timestamp.ok_or(AuthError::MalformedCredential)?;
    if candidates.is_empty() {
        return Err(AuthError::MalformedCredential);
    }

    let mut mac = Hmac::<Sha256>::new_from_slice(a.secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    if !candidates.iter().any(|sig| mac.clone().verify_slice(sig).is_ok()) {
        return Err(AuthError::Mismatch);
    }
    check_fresh(timestamp, now, a.tolerance_secs)
}

fn verify_slack(a: &SlackAuth, headers: &HeaderMap, body: &[u8], now: i64) -> Result<(), AuthError> {
    let sig = header_str(headers, "x-slack-signature")?;
    let ts_raw = header_str(headers, "x-slack-request-timestamp")?;
    let timestamp: i64 = ts_raw.parse().map_err(|_| AuthError::MalformedCredential)?;
    let signature = hex::decode(sig.strip_prefix("v0=").ok_or(AuthError::MalformedCredential)?)
        .map_err(|_| AuthError::MalformedCredential)?;

    let mut mac = Hmac::<Sha256>::new_from_slice(a.secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(b"v0:");
    mac.update(ts_raw.as_bytes());
    mac.update(b":");
    mac.update(body);
    mac.verify_slice(&signature).map_err(|_| AuthError::Mismatch)?;
    check_fresh(timestamp, now, a.tolerance_secs)
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

    // ---- timestamped schemes -------------------------------------------------

    const NOW: i64 = 1_700_000_000;

    fn stripe(tol: u64) -> AuthConfig {
        AuthConfig::Stripe(StripeAuth { secret: "whsec_test".into(), header: "Stripe-Signature".into(), tolerance_secs: tol })
    }
    fn slack(tol: u64) -> AuthConfig {
        AuthConfig::Slack(SlackAuth { secret: "slack_secret".into(), tolerance_secs: tol })
    }
    fn stripe_sig(secret: &str, t: i64, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(format!("{t}.").as_bytes());
        mac.update(body);
        hex::encode(mac.finalize().into_bytes())
    }

    #[test]
    fn stripe_valid_request_passes_and_window_edges() {
        let body = b"{\"id\":\"evt_1\"}";
        let hdr = |t: i64| h(&[("stripe-signature", &format!("t={t},v1={}", stripe_sig("whsec_test", t, body)))]);
        assert_eq!(verify_at(&stripe(300), &hdr(NOW), body, NOW), Ok(()));
        assert_eq!(verify_at(&stripe(300), &hdr(NOW - 300), body, NOW), Ok(()), "edge of window is accepted");
        assert_eq!(verify_at(&stripe(300), &hdr(NOW - 301), body, NOW), Err(AuthError::Expired));
        assert_eq!(verify_at(&stripe(300), &hdr(NOW + 301), body, NOW), Err(AuthError::Expired), "future too");
        assert_eq!(verify_at(&stripe(300), &hdr(NOW + 300), body, NOW), Ok(()), "small clock skew tolerated");
    }

    #[test]
    fn stripe_replay_of_a_captured_request_is_rejected_later() {
        let body = b"{}";
        let t = NOW;
        let captured = h(&[("stripe-signature", &format!("t={t},v1={}", stripe_sig("whsec_test", t, body)))]);
        assert_eq!(verify_at(&stripe(300), &captured, body, NOW + 10), Ok(()), "fresh");
        assert_eq!(verify_at(&stripe(300), &captured, body, NOW + 3600), Err(AuthError::Expired), "replayed an hour later");
    }

    #[test]
    fn stripe_forgeries_are_mismatch_not_expired() {
        let body = b"{}";
        let old = NOW - 100_000;
        // attacker moves the timestamp to look fresh but cannot re-sign
        let forged = h(&[("stripe-signature", &format!("t={NOW},v1={}", stripe_sig("whsec_test", old, body)))]);
        assert_eq!(verify_at(&stripe(300), &forged, body, NOW), Err(AuthError::Mismatch));
        // wrong secret
        let wrong = h(&[("stripe-signature", &format!("t={NOW},v1={}", stripe_sig("other", NOW, body)))]);
        assert_eq!(verify_at(&stripe(300), &wrong, body, NOW), Err(AuthError::Mismatch));
        // tampered body
        let ok = h(&[("stripe-signature", &format!("t={NOW},v1={}", stripe_sig("whsec_test", NOW, body)))]);
        assert_eq!(verify_at(&stripe(300), &ok, b"{\"x\":1}", NOW), Err(AuthError::Mismatch));
    }

    #[test]
    fn stripe_accepts_any_v1_during_secret_rotation_and_ignores_v0() {
        let body = b"{}";
        let good = stripe_sig("whsec_test", NOW, body);
        let bad = stripe_sig("old_secret", NOW, body);
        let hdr = h(&[("stripe-signature", &format!("t={NOW}, v1={bad}, v1={good}, v0=deadbeef"))]);
        assert_eq!(verify_at(&stripe(300), &hdr, body, NOW), Ok(()));
        let only_v0 = h(&[("stripe-signature", &format!("t={NOW},v0={good}"))]);
        assert_eq!(verify_at(&stripe(300), &only_v0, body, NOW), Err(AuthError::MalformedCredential), "v0 alone is not accepted");
    }

    #[test]
    fn stripe_malformed_headers_never_panic() {
        for bad in ["", "t=abc,v1=00", "v1=00", "t=1", "garbage", "t=1,v1", "t=,v1=zz", "=,=", "t=99999999999999999999,v1=00"] {
            assert!(verify_at(&stripe(300), &h(&[("stripe-signature", bad)]), b"{}", NOW).is_err(), "{bad:?}");
        }
        assert_eq!(verify_at(&stripe(300), &HeaderMap::new(), b"{}", NOW), Err(AuthError::MissingCredential));
    }

    fn slack_sig(secret: &str, ts: &str, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(format!("v0:{ts}:").as_bytes());
        mac.update(body);
        format!("v0={}", hex::encode(mac.finalize().into_bytes()))
    }

    #[test]
    fn slack_valid_stale_and_tampered() {
        let body = b"token=abc&team_id=T1&text=hi";
        let ts = NOW.to_string();
        let hdr = h(&[("x-slack-signature", &slack_sig("slack_secret", &ts, body)), ("x-slack-request-timestamp", &ts)]);
        assert_eq!(verify_at(&slack(300), &hdr, body, NOW), Ok(()));
        assert_eq!(verify_at(&slack(300), &hdr, body, NOW + 301), Err(AuthError::Expired));
        assert_eq!(verify_at(&slack(300), &hdr, b"token=abc&team_id=T1&text=HI", NOW), Err(AuthError::Mismatch));
        assert_eq!(verify_at(&slack(300), &h(&[("x-slack-request-timestamp", &ts)]), body, NOW), Err(AuthError::MissingCredential));
        assert_eq!(verify_at(&slack(300), &h(&[("x-slack-signature", "v0=zz"), ("x-slack-request-timestamp", &ts)]), body, NOW), Err(AuthError::MalformedCredential));
        assert_eq!(verify_at(&slack(300), &h(&[("x-slack-signature", "v1=00"), ("x-slack-request-timestamp", &ts)]), body, NOW), Err(AuthError::MalformedCredential));
        assert_eq!(verify_at(&slack(300), &h(&[("x-slack-signature", &slack_sig("slack_secret", &ts, body)), ("x-slack-request-timestamp", "soon")]), body, NOW), Err(AuthError::MalformedCredential));
    }

    #[test]
    fn slack_timestamp_header_is_part_of_the_signed_string() {
        // Changing only the timestamp (to dodge the window) invalidates the signature.
        let body = b"x=1";
        let old = (NOW - 99_999).to_string();
        let sig = slack_sig("slack_secret", &old, body);
        let forged = h(&[("x-slack-signature", &sig), ("x-slack-request-timestamp", &NOW.to_string())]);
        assert_eq!(verify_at(&slack(300), &forged, body, NOW), Err(AuthError::Mismatch));
    }

    // Slack's published example (api.slack.com "Verifying requests from Slack").
    #[test]
    fn slack_documented_example() {
        let secret = "8f742231b10e8888abcd99yyyzzz85a5";
        let ts = "1531420618";
        let body = b"token=xyzz0WbapA4vBCDEFasx0q6G&team_id=T1DC2JH3J&team_domain=testteamnow&channel_id=G8PSS9T3V&channel_name=foobar&user_id=U2CERLKJA&user_name=roadrunner&command=%2Fwebhook-collect&text=&response_url=https%3A%2F%2Fhooks.slack.com%2Fcommands%2FT1DC2JH3J%2F397700885554%2F96rGlfmibIGlgcZRskXaIFfN&trigger_id=398738663015.47445629121.803a0bc887a14d10d2c447fce8b6703c";
        let a = AuthConfig::Slack(SlackAuth { secret: secret.into(), tolerance_secs: 300 });
        let hdr = h(&[
            ("x-slack-signature", "v0=a2114d57b48eac39b9ad189dd8316235a7b4a8d21a10bd27519666489c69b503"),
            ("x-slack-request-timestamp", ts),
        ]);
        assert_eq!(verify_at(&a, &hdr, body, 1531420618 + 5), Ok(()));
    }
}
