//! RabbitMQ sink: publisher-confirmed publishes over a cached connection.

use super::error::SinkError;
use crate::config::RabbitMqTarget;
use lapin::{
    options::{
        BasicPublishOptions, ConfirmSelectOptions, ExchangeDeclareOptions, QueueBindOptions,
        QueueDeclareOptions,
    },
    types::{AMQPValue, FieldTable},
    BasicProperties, Channel, Confirmation, Connection, ConnectionProperties, ExchangeKind,
};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use tokio::sync::Mutex;

struct Session {
    /// Kept so the connection lives as long as the channel is cached.
    conn: Connection,
    channel: Channel,
    /// Exchanges/queues already declared on this session.
    declared: HashSet<String>,
}

impl Session {
    fn healthy(&self) -> bool {
        self.conn.status().connected() && self.channel.status().connected()
    }
}

#[derive(Default)]
pub struct RabbitSink {
    sessions: Mutex<HashMap<String, Session>>,
}

fn exchange_kind(s: &str) -> ExchangeKind {
    match s.to_ascii_lowercase().as_str() {
        "direct" => ExchangeKind::Direct,
        "fanout" => ExchangeKind::Fanout,
        "topic" => ExchangeKind::Topic,
        "headers" => ExchangeKind::Headers,
        other => ExchangeKind::Custom(other.to_string()),
    }
}

impl RabbitSink {
    /// Returns a confirm-enabled channel, (re)connecting and declaring the
    /// topology lazily. The lock is held while connecting so two targets that
    /// share a URL never open two connections.
    async fn channel(&self, t: &RabbitMqTarget) -> Result<Channel, SinkError> {
        let mut sessions = self.sessions.lock().await;

        if !sessions.get(&t.url).map(Session::healthy).unwrap_or(false) {
            sessions.remove(&t.url);
            let conn = Connection::connect(&t.url, ConnectionProperties::default())
                .await
                .map_err(|e| SinkError::retryable(format!("RabbitMQ connect failed: {e}")))?;
            let channel = conn
                .create_channel()
                .await
                .map_err(|e| SinkError::retryable(format!("RabbitMQ create_channel failed: {e}")))?;
            channel
                .confirm_select(ConfirmSelectOptions::default())
                .await
                .map_err(|e| SinkError::retryable(format!("RabbitMQ confirm_select failed: {e}")))?;
            sessions.insert(
                t.url.clone(),
                Session { conn, channel, declared: HashSet::new() },
            );
        }

        let session = sessions.get_mut(&t.url).expect("session just inserted");

        if let (false, Some(kind)) = (t.exchange.is_empty(), t.exchange_type.as_deref()) {
            let key = format!("x:{}:{kind}", t.exchange);
            if !session.declared.contains(&key) {
                session
                    .channel
                    .exchange_declare(
                        t.exchange.as_str().into(),
                        exchange_kind(kind),
                        ExchangeDeclareOptions { durable: true, ..Default::default() },
                        FieldTable::default(),
                    )
                    .await
                    .map_err(|e| SinkError::permanent(format!("exchange_declare '{}' failed: {e}", t.exchange)))?;
                session.declared.insert(key);
            }
        }

        if let Some(queue) = &t.queue {
            let key = format!("q:{queue}:{}:{}", t.exchange, t.routing_key);
            if !session.declared.contains(&key) {
                session
                    .channel
                    .queue_declare(
                        queue.as_str().into(),
                        QueueDeclareOptions { durable: true, ..Default::default() },
                        FieldTable::default(),
                    )
                    .await
                    .map_err(|e| SinkError::permanent(format!("queue_declare '{queue}' failed: {e}")))?;
                if !t.exchange.is_empty() {
                    session
                        .channel
                        .queue_bind(
                            queue.as_str().into(),
                            t.exchange.as_str().into(),
                            t.routing_key.as_str().into(),
                            QueueBindOptions::default(),
                            FieldTable::default(),
                        )
                        .await
                        .map_err(|e| SinkError::permanent(format!("queue_bind '{queue}' failed: {e}")))?;
                }
                session.declared.insert(key);
            }
        }

        Ok(session.channel.clone())
    }

    async fn evict(&self, url: &str) {
        self.sessions.lock().await.remove(url);
    }

    pub async fn send(&self, t: &RabbitMqTarget, payload: &Value, message_id: &str) -> Result<Value, SinkError> {
        let channel = self.channel(t).await?;
        let body = serde_json::to_vec(payload)
            .map_err(|e| SinkError::permanent(format!("Serialize payload: {e}")))?;

        // `message_id` is the standard AMQP property consumers dedupe on.
        let mut props = BasicProperties::default()
            .with_message_id(message_id.into())
            .with_content_type("application/json".into())
            .with_delivery_mode(if t.persistent { 2 } else { 1 });
        if !t.headers.is_empty() {
            let mut table = FieldTable::default();
            for (k, v) in &t.headers {
                table.insert(k.as_str().into(), AMQPValue::LongString(v.as_str().into()));
            }
            props = props.with_headers(table);
        }

        let published = channel
            .basic_publish(
                t.exchange.as_str().into(),
                t.routing_key.as_str().into(),
                BasicPublishOptions { mandatory: t.mandatory, ..Default::default() },
                &body,
                props,
            )
            .await;

        let confirm = match published {
            Ok(pending) => pending.await,
            Err(e) => Err(e),
        };

        match confirm {
            Ok(Confirmation::Ack(None)) | Ok(Confirmation::NotRequested) => {
                Ok(json!({ "confirmed": true }))
            }
            // The broker is alive and answered; retrying cannot fix a missing binding.
            Ok(Confirmation::Ack(Some(_))) => Err(SinkError::permanent(format!(
                "message unroutable: no queue bound to exchange '{}' with key '{}'",
                t.exchange, t.routing_key
            ))),
            Ok(Confirmation::Nack(_)) => Err(SinkError::retryable("broker nacked the message")),
            Err(e) => {
                // The channel/connection is probably dead; drop it so the next
                // delivery reconnects instead of failing forever.
                self.evict(&t.url).await;
                Err(SinkError::retryable(format!("RabbitMQ publish failed: {e}")))
            }
        }
    }
}
