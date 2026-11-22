//! Background delivery of outbox jobs.

use super::{now_ms, Job, Outbox};
use crate::config::OutboxSettings;
use crate::metrics::Metrics;
use crate::registry::Registry;
use crate::sink::{Attempt, DeliveryCtx, Dispatcher};
use serde_json::Value;
use std::{sync::Arc, time::Duration};
use tokio::{sync::watch, task::JoinHandle};
use tracing::{error, info, warn};

/// Everything a worker needs; cheap to clone.
#[derive(Clone)]
pub struct WorkerContext {
    pub outbox: Outbox,
    pub registry: Arc<Registry>,
    pub dispatcher: Arc<Dispatcher>,
    pub settings: OutboxSettings,
    pub metrics: Arc<Metrics>,
}

/// What to do with a job after one attempt.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Delivered,
    /// Try again at `at` (unix ms); `attempts` is the new total.
    Retry { at: i64, attempts: u32 },
    /// Retry later without having used up an attempt (breaker open).
    Defer { at: i64 },
    Dead,
}

/// Pure decision logic, separated from I/O so it can be unit-tested.
pub fn decide_after_failure(
    error: &crate::sink::SinkError,
    job_attempts: u32,
    created_at: i64,
    now: i64,
    policy: &crate::sink::RetryPolicy,
) -> Verdict {
    let attempts = job_attempts + 1;
    if !error.retryable || attempts >= policy.attempts {
        return Verdict::Dead;
    }
    let delay = policy.delay_for(attempts, error.retry_after);
    if let Some(budget) = policy.max_elapsed {
        if (now - created_at) + delay.as_millis() as i64 > budget.as_millis() as i64 {
            return Verdict::Dead;
        }
    }
    Verdict::Retry { at: now + delay.as_millis() as i64, attempts }
}

/// Starts the workers plus a maintenance task. They run until `shutdown`
/// flips to `true`, finishing the job in hand first.
pub async fn spawn(ctx: WorkerContext, shutdown: watch::Receiver<bool>) -> Vec<JoinHandle<()>> {
    // Single-instance assumption: anything still marked in-flight belonged to
    // a process that is gone. Redeliver it now rather than after the lease.
    match ctx.outbox.reclaim_all_in_flight().await {
        Ok(0) => {}
        Ok(n) => warn!(jobs = n, "Recovered in-flight outbox jobs from a previous run"),
        Err(e) => error!(error = %e, "Could not recover in-flight outbox jobs"),
    }
    if let Ok(st) = ctx.outbox.stats().await {
        info!(pending = st.pending, dead = st.dead, "Outbox opened");
    }

    let mut handles = Vec::new();
    for n in 0..ctx.settings.workers {
        handles.push(tokio::spawn(worker_loop(n, ctx.clone(), shutdown.clone())));
    }
    handles.push(tokio::spawn(maintenance_loop(ctx, shutdown)));
    handles
}

async fn worker_loop(n: usize, ctx: WorkerContext, mut shutdown: watch::Receiver<bool>) {
    let lease_ms = (ctx.settings.lease_secs * 1000) as i64;
    let poll = Duration::from_millis(ctx.settings.poll_interval_ms);

    while !*shutdown.borrow() {
        match ctx.outbox.claim(lease_ms).await {
            Ok(Some(job)) => process(&ctx, job).await,
            Ok(None) => {
                tokio::select! {
                    _ = ctx.outbox.wake.notified() => {}
                    _ = tokio::time::sleep(poll) => {}
                    _ = shutdown.changed() => {}
                }
            }
            Err(e) => {
                error!(worker = n, error = %e, "Outbox claim failed");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

async fn maintenance_loop(ctx: WorkerContext, mut shutdown: watch::Receiver<bool>) {
    let delivered = (ctx.settings.delivered_retention_secs * 1000) as i64;
    let dead = (ctx.settings.dead_retention_secs * 1000) as i64;
    while !*shutdown.borrow() {
        match ctx.outbox.reclaim_expired().await {
            Ok(0) => {}
            Ok(n) => warn!(jobs = n, "Lease expired: returned stuck jobs to the queue"),
            Err(e) => error!(error = %e, "Lease reclaim failed"),
        }
        match ctx.outbox.prune(delivered, dead).await {
            Ok(0) => {}
            Ok(n) => info!(jobs = n, "Pruned old outbox rows"),
            Err(e) => error!(error = %e, "Outbox prune failed"),
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(30)) => {}
            _ = shutdown.changed() => {}
        }
    }
}

async fn process(ctx: &WorkerContext, job: Job) {
    let kill = |reason: String| async {
        error!(job = job.id, endpoint = %job.endpoint, target = %job.target_label, reason = %reason, "Outbox job dead-lettered");
        ctx.metrics.record_dead_letter(&job.endpoint, &job.target_label);
        if let Err(e) = ctx.outbox.dead(job.id, job.attempts, reason).await {
            error!(job = job.id, error = %e, "Could not mark job dead");
        }
    };

    // Re-resolve the target from the *current* config. The label guards
    // against the config having been reordered or edited since enqueue.
    let compiled = ctx
        .registry
        .registers
        .get(&job.endpoint)
        .and_then(|r| r.targets.get(job.target_index))
        .filter(|t| t.label == job.target_label);
    let Some(compiled) = compiled else {
        return kill(format!(
            "target '{}' of {} is no longer in the configuration",
            job.target_label, job.endpoint
        ))
        .await;
    };
    let payload: Value = match serde_json::from_str(&job.payload) {
        Ok(v) => v,
        Err(e) => return kill(format!("stored payload is not valid JSON: {e}")).await,
    };

    let dctx = DeliveryCtx { endpoint: &job.endpoint, target: &job.target_label };
    let outcome = ctx.dispatcher.attempt(dctx, &compiled.target, &payload, &job.message_id).await;
    let now = now_ms();
    let verdict = match outcome {
        Attempt::Delivered(_) => Verdict::Delivered,
        Attempt::CircuitOpen { retry_in, .. } => Verdict::Defer {
            at: now + retry_in.as_millis().max(1000) as i64,
        },
        Attempt::Failed(ref e) => {
            decide_after_failure(e, job.attempts, job.created_at, now, &compiled.async_retry)
        }
    };

    let result = match (&verdict, &outcome) {
        (Verdict::Delivered, _) => {
            info!(job = job.id, endpoint = %job.endpoint, target = %job.target_label,
                  message_id = %job.message_id, attempts = job.attempts + 1, "Outbox job delivered");
            ctx.metrics.record_outbox_delivered(&job.endpoint, &job.target_label);
            ctx.outbox.complete(job.id, job.attempts + 1).await
        }
        (Verdict::Defer { at }, _) => {
            // The destination is known to be down: wait for the breaker's
            // probe without burning one of the job's attempts.
            ctx.outbox.reschedule(job.id, job.attempts, *at, "circuit open".to_string()).await
        }
        (Verdict::Retry { at, attempts }, Attempt::Failed(e)) => {
            warn!(job = job.id, target = %job.target_label, attempt = attempts,
                  max_attempts = compiled.async_retry.attempts, retry_in_ms = at - now,
                  error = %e, "Outbox delivery failed, will retry");
            ctx.outbox.reschedule(job.id, *attempts, *at, e.message.clone()).await
        }
        (Verdict::Dead, Attempt::Failed(e)) => {
            error!(job = job.id, endpoint = %job.endpoint, target = %job.target_label,
                   attempts = job.attempts + 1, error = %e, "Outbox job dead-lettered");
            ctx.metrics.record_dead_letter(&job.endpoint, &job.target_label);
            ctx.outbox.dead(job.id, job.attempts + 1, e.message.clone()).await
        }
        _ => Ok(()),
    };
    if let Err(e) = result {
        // The row stays in_flight; the lease will return it to the queue.
        error!(job = job.id, error = %e, "Could not record delivery outcome");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sink::{RetryPolicy, SinkError};

    fn policy(attempts: u32, max_elapsed_ms: Option<u64>) -> RetryPolicy {
        RetryPolicy {
            attempts,
            base_delay: Duration::from_millis(1000),
            multiplier: 2.0,
            max_delay: Duration::from_secs(60),
            max_elapsed: max_elapsed_ms.map(Duration::from_millis),
            jitter: false,
        }
    }

    #[test]
    fn transient_error_is_retried_with_backoff() {
        let e = SinkError::retryable("boom");
        let v = decide_after_failure(&e, 0, 0, 10_000, &policy(5, None));
        assert_eq!(v, Verdict::Retry { at: 11_000, attempts: 1 });
        let v = decide_after_failure(&e, 2, 0, 10_000, &policy(5, None));
        assert_eq!(v, Verdict::Retry { at: 14_000, attempts: 3 });
    }

    #[test]
    fn permanent_error_and_exhausted_attempts_are_dead() {
        assert_eq!(decide_after_failure(&SinkError::permanent("400"), 0, 0, 0, &policy(5, None)), Verdict::Dead);
        let e = SinkError::retryable("boom");
        assert_eq!(decide_after_failure(&e, 4, 0, 0, &policy(5, None)), Verdict::Dead, "5th attempt used");
        assert_eq!(decide_after_failure(&e, 0, 0, 0, &policy(1, None)), Verdict::Dead, "attempts=1 never retries");
    }

    #[test]
    fn retry_after_is_honoured() {
        let e = SinkError::retryable("429").with_retry_after(Some(Duration::from_secs(30)));
        let v = decide_after_failure(&e, 0, 0, 0, &policy(5, None));
        assert_eq!(v, Verdict::Retry { at: 30_000, attempts: 1 });
    }

    #[test]
    fn time_budget_is_measured_from_creation_not_from_now() {
        let e = SinkError::retryable("boom");
        // created 9.5s ago, budget 10s, next wait 1s => would end at 10.5s
        assert_eq!(decide_after_failure(&e, 0, 0, 9_500, &policy(5, Some(10_000))), Verdict::Dead);
        assert!(matches!(decide_after_failure(&e, 0, 0, 1_000, &policy(5, Some(10_000))), Verdict::Retry { .. }));
    }
}
