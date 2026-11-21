//! Outbound delivery. A [`Dispatcher`] owns one client per sink kind and can
//! broadcast a payload to many [`Target`]s concurrently.

pub mod http;
pub mod nats;
pub mod rabbitmq;

use crate::config::{DeliveryPolicy, Target, TargetKind};
use futures_util::future::join_all;
use serde::Serialize;
use serde_json::Value;
use std::time::{Duration, Instant};

/// One unit of work in a broadcast: a target plus the payload already
/// rendered for it (or the reason rendering failed).
pub struct Delivery<'a> {
    pub label: String,
    pub target: &'a Target,
    pub payload: Result<Value, String>,
}

/// Per-target result, serialised straight into the HTTP response.
#[derive(Debug, Clone, Serialize)]
pub struct TargetOutcome {
    pub target: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub status: &'static str, // "ok" | "error"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub elapsed_ms: u64,
}

impl TargetOutcome {
    pub fn is_ok(&self) -> bool {
        self.status == "ok"
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BroadcastReport {
    pub satisfied: bool,
    pub ok: usize,
    pub total: usize,
    pub results: Vec<TargetOutcome>,
}

impl BroadcastReport {
    pub fn new(policy: DeliveryPolicy, results: Vec<TargetOutcome>) -> Self {
        let ok = results.iter().filter(|r| r.is_ok()).count();
        let total = results.len();
        Self { satisfied: policy.is_satisfied(ok, total), ok, total, results }
    }
}

pub struct Dispatcher {
    http: http::HttpSink,
    rabbit: rabbitmq::RabbitSink,
    nats: nats::NatsSink,
    default_timeout: Duration,
}

impl Dispatcher {
    pub fn new(default_timeout_secs: u64) -> Self {
        Self {
            http: http::HttpSink::new(default_timeout_secs),
            rabbit: Default::default(),
            nats: Default::default(),
            default_timeout: Duration::from_secs(default_timeout_secs),
        }
    }

    /// Delivers to a single target, bounded by that target's timeout.
    pub async fn send(&self, target: &Target, payload: &Value) -> Result<Value, String> {
        let limit = match &target.kind {
            TargetKind::Http(h) => h.timeout_seconds.map(Duration::from_secs),
            _ => None,
        }
        .unwrap_or(self.default_timeout);

        let fut = async {
            match &target.kind {
                TargetKind::Http(t) => self.http.send(t, payload).await,
                TargetKind::Rabbitmq(t) => self.rabbit.send(t, payload).await,
                TargetKind::Nats(t) => self.nats.send(t, payload).await,
            }
        };

        tokio::time::timeout(limit, fut)
            .await
            .unwrap_or_else(|_| Err(format!("timed out after {}s", limit.as_secs())))
    }

    /// Sends every delivery concurrently and returns outcomes in input order.
    /// A slow or failing target never delays or cancels the others.
    pub async fn broadcast(&self, deliveries: Vec<Delivery<'_>>) -> Vec<TargetOutcome> {
        join_all(deliveries.into_iter().map(|d| async move {
            let started = Instant::now();
            let result = match d.payload {
                Ok(payload) => self.send(d.target, &payload).await,
                Err(e) => Err(e),
            };
            let elapsed_ms = started.elapsed().as_millis() as u64;
            match result {
                Ok(response) => TargetOutcome {
                    target: d.label,
                    kind: d.target.type_name(),
                    status: "ok",
                    response: Some(response),
                    error: None,
                    elapsed_ms,
                },
                Err(error) => TargetOutcome {
                    target: d.label,
                    kind: d.target.type_name(),
                    status: "error",
                    response: None,
                    error: Some(error),
                    elapsed_ms,
                },
            }
        }))
        .await
    }
}
