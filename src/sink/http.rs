//! HTTP sink: POST/PUT/… the rendered JSON to a URL.

use super::error::SinkError;
use crate::config::HttpTarget;
use crate::idempotency::OUTGOING_HTTP_HEADER;
use reqwest::{header, Client, RequestBuilder, StatusCode};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;

pub struct HttpSink {
    client: Client,
}

impl HttpSink {
    pub fn new(default_timeout_secs: u64) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(default_timeout_secs))
            .build()
            .expect("Failed to create HTTP client");
        Self { client }
    }

    pub async fn send(&self, t: &HttpTarget, payload: &Value, message_id: &str) -> Result<Value, SinkError> {
        let mut builder = request_builder(&self.client, &t.method, &t.url)?
            .headers(header_map(&t.headers, message_id)?)
            .json(payload);

        // Per-target timeout overrides the client-wide default.
        if let Some(secs) = t.timeout_seconds {
            builder = builder.timeout(Duration::from_secs(secs));
        }

        let response = builder.send().await.map_err(|e| {
            let msg = format!("Failed to send request to target: {e}");
            // Connect errors, resets and timeouts are transient; a request we
            // couldn't even build or a redirect loop will never succeed.
            if e.is_builder() || e.is_redirect() {
                SinkError::permanent(msg)
            } else {
                SinkError::retryable(msg)
            }
        })?;

        let status = response.status();
        let retry_after = parse_retry_after(response.headers());
        let text = response
            .text()
            .await
            .map_err(|e| SinkError::retryable(format!("Failed to read target response: {e}")))?;

        // Return the body (JSON when possible) for 2xx; anything else is a
        // failed delivery so broadcast policies and retries can act on it.
        let body = serde_json::from_str::<Value>(&text).unwrap_or_else(|_| Value::String(text));
        if status.is_success() {
            Ok(body)
        } else {
            let msg = format!("Target responded with HTTP {status}: {body}");
            Err(if is_retryable_status(status) {
                SinkError::retryable(msg).with_retry_after(retry_after)
            } else {
                SinkError::permanent(msg)
            })
        }
    }
}

/// 5xx (except 501 Not Implemented), 408 Request Timeout, 425 Too Early and
/// 429 Too Many Requests can succeed later; other statuses are final.
pub fn is_retryable_status(status: StatusCode) -> bool {
    match status.as_u16() {
        408 | 425 | 429 => true,
        501 => false,
        500..=599 => true,
        _ => false,
    }
}

/// `Retry-After: <seconds>` (the HTTP-date form is ignored).
fn parse_retry_after(headers: &header::HeaderMap) -> Option<Duration> {
    headers
        .get(header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

fn request_builder(client: &Client, method: &str, url: &str) -> Result<RequestBuilder, SinkError> {
    Ok(match method.to_uppercase().as_str() {
        "GET" => client.get(url),
        "POST" => client.post(url),
        "PUT" => client.put(url),
        "DELETE" => client.delete(url),
        "PATCH" => client.patch(url),
        other => return Err(SinkError::permanent(format!("Unsupported HTTP method: {other}"))),
    })
}

fn header_map(raw: &HashMap<String, String>, message_id: &str) -> Result<header::HeaderMap, SinkError> {
    let mut map = header::HeaderMap::new();
    for (key, value) in raw {
        let name = header::HeaderName::from_bytes(key.as_bytes())
            .map_err(|e| SinkError::permanent(format!("Invalid header name '{key}': {e}")))?;
        let val = header::HeaderValue::from_str(value)
            .map_err(|e| SinkError::permanent(format!("Invalid header value for '{key}': {e}")))?;
        map.insert(name, val);
    }
    if !map.contains_key(header::CONTENT_TYPE) {
        map.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    }
    // Same id on every retry and every target. A header configured on the
    // target wins, so existing setups can keep their own scheme.
    if let Ok(v) = header::HeaderValue::from_str(message_id) {
        if !map.contains_key(OUTGOING_HTTP_HEADER) {
            map.insert(header::HeaderName::from_static("idempotency-key"), v);
        }
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_classification() {
        for (code, retry) in [(500, true), (502, true), (503, true), (504, true), (408, true), (429, true),
                              (501, false), (400, false), (401, false), (404, false), (422, false), (301, false)] {
            assert_eq!(is_retryable_status(StatusCode::from_u16(code).unwrap()), retry, "{code}");
        }
    }

    #[test]
    fn retry_after_parsing() {
        let mut h = header::HeaderMap::new();
        h.insert(header::RETRY_AFTER, "7".parse().unwrap());
        assert_eq!(parse_retry_after(&h), Some(Duration::from_secs(7)));
        h.insert(header::RETRY_AFTER, "Wed, 21 Oct 2015 07:28:00 GMT".parse().unwrap());
        assert_eq!(parse_retry_after(&h), None);
    }

    #[test]
    fn message_id_header_added_but_user_header_wins() {
        let m = header_map(&HashMap::new(), "abc").unwrap();
        assert_eq!(m.get("idempotency-key").unwrap(), "abc");
        let mut custom = HashMap::new();
        custom.insert("Idempotency-Key".to_string(), "mine".to_string());
        assert_eq!(header_map(&custom, "abc").unwrap().get("idempotency-key").unwrap(), "mine");
    }
}
