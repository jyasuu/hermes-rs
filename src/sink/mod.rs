//! Outbound delivery. A [`Dispatcher`] owns one client per sink kind and can
//! broadcast a payload to many [`Target`]s concurrently.

pub mod breaker;
pub mod error;
pub mod http;
pub mod nats;
pub mod rabbitmq;
pub mod retry;

pub use error::SinkError;
pub use retry::RetryPolicy;

use crate::config::{AppSettings, DeliveryPolicy, Target, TargetKind};
use breaker::{BreakerConfig, Breakers};
use futures_util::future::join_all;
use serde::Serialize;
use serde_json::Value;
use std::time::{Duration, Instant};
use tracing::warn;

/// One unit of work in a broadcast: a target plus the payload already
/// rendered for it (or the reason rendering failed).
pub struct Delivery<'a> {
    pub label: String,
    pub target: &'a Target,
    pub payload: Result<Value, String>,
    pub retry: RetryPolicy,
}

/// Per-target result, serialised straight into the HTTP response.
#[derive(Debug, Clone, Serialize)]
pub struct TargetOutcome {
    pub target: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub status: &'static str, // "ok" | "error"
    /// How many times delivery was actually tried (0 = skipped, circuit open).
    pub attempts: u32,
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
    breakers: Breakers,
    default_timeout: Duration,
}

/// What a circuit breaker protects: the *destination*, so every target that
/// shares a broker (or URL) shares its health.
fn breaker_key(target: &Target) -> String {
    match &target.kind {
        TargetKind::Http(t) => format!("http {}", t.url),
        TargetKind::Rabbitmq(t) => format!("rabbitmq {}", t.url),
        TargetKind::Nats(t) => format!("nats {}", t.url),
    }
}

impl Dispatcher {
    pub fn new(default_timeout_secs: u64, settings: &AppSettings) -> Self {
        Self {
            http: http::HttpSink::new(default_timeout_secs),
            rabbit: Default::default(),
            nats: Default::default(),
            breakers: Breakers::new(BreakerConfig::from(&settings.circuit_breaker)),
            default_timeout: Duration::from_secs(default_timeout_secs),
        }
    }

    /// A single delivery attempt, bounded by the target's timeout.
    async fn send_once(&self, target: &Target, payload: &Value, message_id: &str) -> Result<Value, SinkError> {
        let limit = match &target.kind {
            TargetKind::Http(h) => h.timeout_seconds.map(Duration::from_secs),
            _ => None,
        }
        .unwrap_or(self.default_timeout);

        let fut = async {
            match &target.kind {
                TargetKind::Http(t) => self.http.send(t, payload, message_id).await,
                TargetKind::Rabbitmq(t) => self.rabbit.send(t, payload, message_id).await,
                TargetKind::Nats(t) => self.nats.send(t, payload, message_id).await,
            }
        };

        tokio::time::timeout(limit, fut)
            .await
            .unwrap_or_else(|_| Err(SinkError::retryable(format!("timed out after {}s", limit.as_secs()))))
    }

    /// Delivers to one target with retries and circuit breaking. Returns the
    /// result and how many attempts were actually made.
    pub async fn deliver(
        &self,
        target: &Target,
        payload: &Value,
        message_id: &str,
        policy: &RetryPolicy,
    ) -> (Result<Value, SinkError>, u32) {
        let key = breaker_key(target);
        let started = Instant::now();
        let mut attempts = 0u32;

        loop {
            // Checked before *every* attempt, so a breaker that opens while
            // we are retrying stops us immediately instead of waiting it out.
            if let Err(wait) = self.breakers.acquire(&key) {
                let msg = format!(
                    "circuit open for {}: skipped, destination looks down (retry in ~{}s)",
                    target.describe(),
                    wait.as_secs().max(1)
                );
                return (Err(SinkError::permanent(msg)), attempts);
            }

            attempts += 1;
            match self.send_once(target, payload, message_id).await {
                Ok(v) => {
                    self.breakers.record(&key, true);
                    return (Ok(v), attempts);
                }
                Err(e) => {
                    self.breakers.record(&key, e.destination_healthy());
                    if !e.retryable || attempts >= policy.attempts {
                        return (Err(e), attempts);
                    }

                    let delay = policy.delay_for(attempts, e.retry_after);
                    if let Some(budget) = policy.max_elapsed {
                        if started.elapsed() + delay > budget {
                            let msg = format!("{} (retry time budget of {}ms exhausted)", e.message, budget.as_millis());
                            return (Err(SinkError { message: msg, ..e }), attempts);
                        }
                    }

                    warn!(
                        target = %target.describe(),
                        attempt = attempts,
                        max_attempts = policy.attempts,
                        retry_in_ms = delay.as_millis() as u64,
                        error = %e,
                        "Delivery failed, will retry"
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    /// Sends every delivery concurrently and returns outcomes in input order.
    /// A slow or failing target never delays or cancels the others. All
    /// targets, and all retries, share one `message_id`.
    pub async fn broadcast(&self, message_id: &str, deliveries: Vec<Delivery<'_>>) -> Vec<TargetOutcome> {
        join_all(deliveries.into_iter().map(|d| async move {
            let started = Instant::now();
            let (result, attempts) = match d.payload {
                Ok(payload) => self.deliver(d.target, &payload, message_id, &d.retry).await,
                Err(e) => (Err(SinkError::permanent(e)), 0),
            };
            let elapsed_ms = started.elapsed().as_millis() as u64;
            match result {
                Ok(response) => TargetOutcome {
                    target: d.label,
                    kind: d.target.type_name(),
                    status: "ok",
                    attempts,
                    response: Some(response),
                    error: None,
                    elapsed_ms,
                },
                Err(error) => TargetOutcome {
                    target: d.label,
                    kind: d.target.type_name(),
                    status: "error",
                    attempts,
                    response: None,
                    error: Some(error.message),
                    elapsed_ms,
                },
            }
        }))
        .await
    }
}
