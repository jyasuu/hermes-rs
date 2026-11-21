//! NATS sink: core publish, or JetStream publish with server ack.

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
    async fn client(&self, url: &str) -> Result<Client, String> {
        let mut guard = self.clients.lock().await;
        if let Some(c) = guard.get(url) {
            return Ok(c.clone());
        }
        let c = async_nats::connect(url)
            .await
            .map_err(|e| format!("NATS connect failed: {e}"))?;
        guard.insert(url.to_string(), c.clone());
        Ok(c)
    }

    pub async fn send(&self, t: &NatsTarget, payload: &Value) -> Result<Value, String> {
        let client = self.client(&t.url).await?;
        let bytes = bytes::Bytes::from(
            serde_json::to_vec(payload).map_err(|e| format!("Serialize payload: {e}"))?,
        );
        let subject = t.subject.clone();

        let headers = if t.headers.is_empty() {
            None
        } else {
            let mut h = HeaderMap::new();
            for (k, v) in &t.headers {
                h.insert(k.as_str(), v.as_str());
            }
            Some(h)
        };

        if t.jetstream {
            let js = async_nats::jetstream::new(client);
            let ack_future = match headers {
                Some(h) => js.publish_with_headers(subject, h, bytes).await,
                None => js.publish(subject, bytes).await,
            }
            .map_err(|e| format!("JetStream publish failed: {e}"))?;
            let ack = ack_future
                .await
                .map_err(|e| format!("JetStream ack failed: {e}"))?;
            Ok(json!({ "stream": ack.stream, "sequence": ack.sequence, "duplicate": ack.duplicate }))
        } else {
            match headers {
                Some(h) => client.publish_with_headers(subject, h, bytes).await,
                None => client.publish(subject, bytes).await,
            }
            .map_err(|e| format!("NATS publish failed: {e}"))?;
            // Core NATS has no per-message ack; flushing guarantees the bytes
            // reached the server before we report success.
            client
                .flush()
                .await
                .map_err(|e| format!("NATS flush failed: {e}"))?;
            Ok(json!({ "published": true }))
        }
    }
}
