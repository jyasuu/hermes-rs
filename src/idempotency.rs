//! Message identity. One id per inbound request, reused by *every* target and
//! *every* retry, so consumers (and NATS JetStream) can drop duplicates.

use axum::http::HeaderMap;

/// Headers a sender may use to identify a delivery, in priority order.
/// `X-GitHub-Delivery` is the same GUID on every redelivery of an event.
const ID_HEADERS: [&str; 4] = [
    "idempotency-key",
    "x-idempotency-key",
    "x-github-delivery",
    "x-request-id",
];

/// Header Hermes attaches to outgoing HTTP requests.
pub const OUTGOING_HTTP_HEADER: &str = "Idempotency-Key";

/// The sender's own key if it sent a sane one, otherwise a fresh random id.
pub fn resolve_message_id(headers: &HeaderMap) -> String {
    ID_HEADERS
        .iter()
        .filter_map(|name| headers.get(*name))
        .filter_map(|v| v.to_str().ok())
        .map(str::trim)
        .find(|v| is_valid(v))
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
}

/// 1–128 visible ASCII chars: safe as an HTTP header value, an AMQP
/// message-id and a NATS header value alike.
fn is_valid(v: &str) -> bool {
    !v.is_empty() && v.len() <= 128 && v.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.insert(*k, v.parse().unwrap());
        }
        m
    }

    #[test]
    fn uses_senders_key() {
        assert_eq!(resolve_message_id(&h(&[("Idempotency-Key", " abc-123 ")])), "abc-123");
    }

    #[test]
    fn priority_order_is_respected() {
        let m = h(&[("x-request-id", "req"), ("x-github-delivery", "gh"), ("idempotency-key", "idem")]);
        assert_eq!(resolve_message_id(&m), "idem");
        let m = h(&[("x-request-id", "req"), ("x-github-delivery", "gh")]);
        assert_eq!(resolve_message_id(&m), "gh");
    }

    #[test]
    fn generates_unique_ids_when_absent() {
        let a = resolve_message_id(&HeaderMap::new());
        let b = resolve_message_id(&HeaderMap::new());
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
    }

    #[test]
    fn rejects_unsafe_values_and_falls_through() {
        let long = "x".repeat(129);
        let m = h(&[("idempotency-key", long.as_str()), ("x-request-id", "ok-id")]);
        assert_eq!(resolve_message_id(&m), "ok-id");
        let m = h(&[("idempotency-key", "has space")]);
        assert_ne!(resolve_message_id(&m), "has space");
    }
}
