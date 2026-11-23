//! Prometheus metrics.
//!
//! Label values only ever come from the configuration (endpoint paths, target
//! names) or from fixed enums, never from request data, so a scanner hitting
//! random URLs cannot create new time series.

use crate::outbox::Stats;
use prometheus::{
    Encoder, Gauge, HistogramOpts, HistogramVec, IntCounterVec, IntGaugeVec, Opts, Registry, TextEncoder,
};

/// Outcome of one delivery attempt, as exposed in `result=`.
pub const ATTEMPT_RESULTS: [&str; 4] = ["ok", "retryable_error", "permanent_error", "circuit_open"];

pub struct Metrics {
    registry: Registry,
    requests: IntCounterVec,
    attempts: IntCounterVec,
    attempt_seconds: HistogramVec,
    dead_lettered: IntCounterVec,
    outbox_delivered: IntCounterVec,
    outbox_jobs: IntGaugeVec,
    outbox_oldest: Gauge,
    reloads: IntCounterVec,
    last_reload: Gauge,
}

impl Metrics {
    pub fn new() -> Self {
        let registry = Registry::new();
        let reg = |c: Box<dyn prometheus::core::Collector>| registry.register(c).expect("metric names are unique");

        let requests = IntCounterVec::new(
            Opts::new("hermes_webhook_requests_total", "Inbound webhook requests by endpoint and outcome."),
            &["endpoint", "outcome"],
        )
        .unwrap();
        let attempts = IntCounterVec::new(
            Opts::new("hermes_delivery_attempts_total", "Delivery attempts per target (retries count separately)."),
            &["endpoint", "target", "kind", "result"],
        )
        .unwrap();
        let attempt_seconds = HistogramVec::new(
            HistogramOpts::new("hermes_delivery_attempt_duration_seconds", "Duration of a single delivery attempt.")
                .buckets(vec![0.005, 0.025, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0]),
            &["kind"],
        )
        .unwrap();
        let dead_lettered = IntCounterVec::new(
            Opts::new("hermes_outbox_dead_lettered_total", "Async jobs moved to the dead-letter state. Alert on rate > 0."),
            &["endpoint", "target"],
        )
        .unwrap();
        let outbox_delivered = IntCounterVec::new(
            Opts::new("hermes_outbox_jobs_delivered_total", "Async jobs delivered successfully."),
            &["endpoint", "target"],
        )
        .unwrap();
        let outbox_jobs = IntGaugeVec::new(
            Opts::new("hermes_outbox_jobs", "Jobs currently in the outbox, by state (read from the database at scrape time)."),
            &["state"],
        )
        .unwrap();
        let outbox_oldest = Gauge::new(
            "hermes_outbox_oldest_waiting_job_age_seconds",
            "Age of the oldest job still waiting for delivery; grows during an outage.",
        )
        .unwrap();
        let reloads = IntCounterVec::new(
            Opts::new("hermes_config_reloads_total", "Configuration reload attempts (SIGHUP) by result."),
            &["result"],
        )
        .unwrap();
        let last_reload = Gauge::new(
            "hermes_config_last_success_timestamp_seconds",
            "Unix time the running configuration was loaded (startup or last successful reload).",
        )
        .unwrap();
        let info = IntGaugeVec::new(Opts::new("hermes_build_info", "Build information."), &["version"]).unwrap();
        info.with_label_values(&[env!("CARGO_PKG_VERSION")]).set(1);

        reg(Box::new(requests.clone()));
        reg(Box::new(attempts.clone()));
        reg(Box::new(attempt_seconds.clone()));
        reg(Box::new(dead_lettered.clone()));
        reg(Box::new(outbox_delivered.clone()));
        reg(Box::new(outbox_jobs.clone()));
        reg(Box::new(outbox_oldest.clone()));
        reg(Box::new(reloads.clone()));
        reg(Box::new(last_reload.clone()));
        reg(Box::new(info));

        Self { registry, requests, attempts, attempt_seconds, dead_lettered, outbox_delivered, outbox_jobs, outbox_oldest, reloads, last_reload }
    }

    pub fn record_request(&self, endpoint: &str, outcome: &str) {
        self.requests.with_label_values(&[endpoint, outcome]).inc();
    }

    pub fn record_attempt(&self, endpoint: &str, target: &str, kind: &str, result: &str, seconds: f64) {
        self.attempts.with_label_values(&[endpoint, target, kind, result]).inc();
        // A skipped attempt (circuit open) took no time and would distort the histogram.
        if result != "circuit_open" {
            self.attempt_seconds.with_label_values(&[kind]).observe(seconds);
        }
    }

    pub fn record_dead_letter(&self, endpoint: &str, target: &str) {
        self.dead_lettered.with_label_values(&[endpoint, target]).inc();
    }

    pub fn record_outbox_delivered(&self, endpoint: &str, target: &str) {
        self.outbox_delivered.with_label_values(&[endpoint, target]).inc();
    }

    /// Refreshes the outbox gauges; called right before rendering so they are never stale.
    pub fn set_outbox(&self, s: &Stats) {
        for (state, n) in [("pending", s.pending), ("in_flight", s.in_flight), ("delivered", s.delivered), ("dead", s.dead)] {
            self.outbox_jobs.with_label_values(&[state]).set(n as i64);
        }
        self.outbox_oldest.set(s.oldest_pending_age_ms.map(|ms| ms as f64 / 1000.0).unwrap_or(0.0));
    }

    /// `ok == true` also stamps "configuration loaded now".
    pub fn record_reload(&self, ok: bool) {
        self.reloads.with_label_values(&[if ok { "success" } else { "failure" }]).inc();
        if ok {
            self.mark_config_loaded();
        }
    }

    pub fn mark_config_loaded(&self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        self.last_reload.set(now);
    }

    pub fn render(&self) -> String {
        let mut buf = Vec::new();
        TextEncoder::new().encode(&self.registry.gather(), &mut buf).expect("text encoding cannot fail");
        String::from_utf8(buf).unwrap_or_default()
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str, prefix: &str) -> Option<String> {
        text.lines().find(|l| l.starts_with(prefix)).map(str::to_string)
    }

    #[test]
    fn counters_render_with_labels_and_accumulate() {
        let m = Metrics::new();
        m.record_request("/w", "delivered");
        m.record_request("/w", "delivered");
        m.record_request("/w", "unauthorized");
        let out = m.render();
        assert_eq!(line(&out, r#"hermes_webhook_requests_total{endpoint="/w",outcome="delivered"}"#).unwrap().split(' ').last(), Some("2"));
        assert!(line(&out, r#"hermes_webhook_requests_total{endpoint="/w",outcome="unauthorized"} 1"#).is_some());
        assert!(out.contains("# TYPE hermes_webhook_requests_total counter"));
    }

    #[test]
    fn attempts_feed_histogram_except_skipped_ones() {
        let m = Metrics::new();
        m.record_attempt("/w", "api", "http", "ok", 0.2);
        m.record_attempt("/w", "api", "http", "circuit_open", 0.0);
        let out = m.render();
        assert!(line(&out, r#"hermes_delivery_attempts_total{endpoint="/w",kind="http",result="circuit_open",target="api"} 1"#).is_some(), "{out}");
        assert!(line(&out, r#"hermes_delivery_attempt_duration_seconds_count{kind="http"} 1"#).is_some(), "skipped attempt must not be observed");
    }

    #[test]
    fn outbox_gauges_reflect_latest_stats() {
        let m = Metrics::new();
        m.set_outbox(&Stats { pending: 5, in_flight: 1, delivered: 9, dead: 2, oldest_pending_age_ms: Some(12_500) });
        let out = m.render();
        assert!(line(&out, r#"hermes_outbox_jobs{state="dead"} 2"#).is_some());
        assert!(line(&out, r#"hermes_outbox_jobs{state="pending"} 5"#).is_some());
        assert!(line(&out, "hermes_outbox_oldest_waiting_job_age_seconds 12.5").is_some());
        m.set_outbox(&Stats::default());
        let out = m.render();
        assert!(line(&out, r#"hermes_outbox_jobs{state="dead"} 0"#).is_some(), "gauge goes back down");
        assert!(line(&out, "hermes_outbox_oldest_waiting_job_age_seconds 0").is_some());
    }

    #[test]
    fn dead_letter_and_build_info() {
        let m = Metrics::new();
        m.record_dead_letter("/w", "api");
        m.record_outbox_delivered("/w", "api");
        let out = m.render();
        assert!(line(&out, r#"hermes_outbox_dead_lettered_total{endpoint="/w",target="api"} 1"#).is_some());
        assert!(line(&out, r#"hermes_outbox_jobs_delivered_total{endpoint="/w",target="api"} 1"#).is_some());
        assert!(line(&out, "hermes_build_info{version=").is_some());
    }

    #[test]
    fn reload_counters_and_timestamp() {
        let m = Metrics::new();
        m.mark_config_loaded();
        m.record_reload(true);
        m.record_reload(false);
        m.record_reload(false);
        let out = m.render();
        assert!(line(&out, r#"hermes_config_reloads_total{result="success"} 1"#).is_some());
        assert!(line(&out, r#"hermes_config_reloads_total{result="failure"} 2"#).is_some());
        let ts: f64 = line(&out, "hermes_config_last_success_timestamp_seconds ").unwrap().rsplit(' ').next().unwrap().parse().unwrap();
        assert!(ts > 1.7e9, "{ts}");
    }
}
