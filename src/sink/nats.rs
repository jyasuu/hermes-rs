//! NATS sink: core publish, or JetStream publish with server ack.

use super::error::SinkError;
use crate::config::NatsTarget;
use async_nats::{Client, HeaderMap};
use serde_json::{json, Value};
use std::collections::HashMap;
use tokio::sync::Mutex;

#[derive(Default)]
pub struct NatsSink {
    /// One client per server URL. `async_nats::Client` is cheap to clone and
    /// reconnects on its own once the first connection has succeeded.
    clients: Mutex<HashMap<String, Client>>,
}

impl NatsSink {
    async fn client(&self, url: &str) -> Result<Client, SinkError> {
        let mut guard = self.clients.lock().await;
        if let Some(c) = guard.get(url) {
            return Ok(c.clone());
        }
        let c = async_nats::connect(url)
            .await
            .map_err(|e| SinkError::retryable(format!("NATS connect failed: {e}")))?;
        guard.insert(url.to_string(), c.clone());
        Ok(c)
    }

    pub async fn send(&self, t: &NatsTarget, payload: &Value, message_id: &str) -> Result<Value, SinkError> {
        let client = self.client(&t.url).await?;
        let bytes = bytes::Bytes::from(
            serde_json::to_vec(payload)
                .map_err(|e| SinkError::permanent(format!("Serialize payload: {e}")))?,
        );
        let subject = t.subject.clone();

        // `Nats-Msg-Id` makes JetStream drop a re-delivery of the same message
        // inside the stream's duplicate window. Needs NATS server >= 2.2.
        let mut headers = HeaderMap::new();
        for (k, v) in &t.headers {
            headers.insert(k.as_str(), v.as_str());
        }
        if !t.headers.keys().any(|k| k.eq_ignore_ascii_case("nats-msg-id")) {
            headers.insert("Nats-Msg-Id", message_id);
        }

        if t.jetstream {
            let js = async_nats::jetstream::new(client);
            let ack = js
                .publish_with_headers(subject, headers, bytes)
                .await
                .map_err(|e| SinkError::retryable(format!("JetStream publish failed: {e}")))?
                .await
                .map_err(|e| SinkError::retryable(format!("JetStream ack failed: {e}")))?;
            Ok(json!({ "stream": ack.stream, "sequence": ack.sequence, "duplicate": ack.duplicate }))
        } else {
            client
                .publish_with_headers(subject, headers, bytes)
                .await
                .map_err(|e| SinkError::retryable(format!("NATS publish failed: {e}")))?;
            // Core NATS has no per-message ack; flushing guarantees the bytes
            // reached the server before we report success.
            client
                .flush()
                .await
                .map_err(|e| SinkError::retryable(format!("NATS flush failed: {e}")))?;
            Ok(json!({ "published": true }))
        }
    }
}
