//! HTTP sink: POST/PUT/… the rendered JSON to a URL.

use crate::config::HttpTarget;
use reqwest::{header, Client, RequestBuilder};
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

    pub async fn send(&self, t: &HttpTarget, payload: &Value) -> Result<Value, String> {
        let mut builder = request_builder(&self.client, &t.method, &t.url)?
            .headers(header_map(&t.headers)?)
            .json(payload);

        // Per-target timeout overrides the client-wide default.
        if let Some(secs) = t.timeout_seconds {
            builder = builder.timeout(Duration::from_secs(secs));
        }

        let response = builder
            .send()
            .await
            .map_err(|e| format!("Failed to send request to target: {e}"))?;

        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| format!("Failed to read target response: {e}"))?;

        // Preserve the original behaviour (return the body, JSON when
        // possible) for 2xx; surface non-2xx as a delivery failure so that
        // broadcast policies can reason about it.
        let body = serde_json::from_str::<Value>(&text).unwrap_or_else(|_| Value::String(text));
        if status.is_success() {
            Ok(body)
        } else {
            Err(format!("Target responded with HTTP {status}: {body}"))
        }
    }
}

fn request_builder(client: &Client, method: &str, url: &str) -> Result<RequestBuilder, String> {
    Ok(match method.to_uppercase().as_str() {
        "GET" => client.get(url),
        "POST" => client.post(url),
        "PUT" => client.put(url),
        "DELETE" => client.delete(url),
        "PATCH" => client.patch(url),
        other => return Err(format!("Unsupported HTTP method: {other}")),
    })
}

fn header_map(raw: &HashMap<String, String>) -> Result<header::HeaderMap, String> {
    let mut map = header::HeaderMap::new();
    for (key, value) in raw {
        let name = header::HeaderName::from_bytes(key.as_bytes())
            .map_err(|e| format!("Invalid header name '{key}': {e}"))?;
        let val = header::HeaderValue::from_str(value)
            .map_err(|e| format!("Invalid header value for '{key}': {e}"))?;
        map.insert(name, val);
    }
    if !map.contains_key(header::CONTENT_TYPE) {
        map.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    }
    Ok(map)
}
