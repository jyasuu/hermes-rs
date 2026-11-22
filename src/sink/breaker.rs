//! Per-destination circuit breaker (closed → open → half-open → closed).

use crate::config::CircuitBreakerSettings;
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy)]
enum State {
    Closed { failures: u32 },
    Open { until: Instant },
    /// One probe is allowed through; `since` is a lease so a probe whose
    /// future was dropped (client disconnect) can't wedge the breaker.
    HalfOpen { since: Instant },
}

#[derive(Debug, Clone, Copy)]
pub struct BreakerConfig {
    pub enabled: bool,
    pub threshold: u32,
    pub cooldown: Duration,
}

impl From<&CircuitBreakerSettings> for BreakerConfig {
    fn from(s: &CircuitBreakerSettings) -> Self {
        Self {
            enabled: s.enabled,
            threshold: s.failure_threshold.max(1),
            cooldown: Duration::from_secs(s.cooldown_secs.max(1)),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Breaker {
    state: State,
}

impl Breaker {
    fn new() -> Self {
        Self { state: State::Closed { failures: 0 } }
    }

    /// `Ok(())` = go ahead; `Err(d)` = rejected, try again in about `d`.
    fn acquire(&mut self, now: Instant, cfg: &BreakerConfig) -> Result<(), Duration> {
        match self.state {
            State::Closed { .. } => Ok(()),
            State::Open { until } if now >= until => {
                self.state = State::HalfOpen { since: now };
                Ok(())
            }
            State::Open { until } => Err(until - now),
            State::HalfOpen { since } if now.duration_since(since) >= cfg.cooldown => {
                self.state = State::HalfOpen { since: now }; // lease expired, new probe
                Ok(())
            }
            State::HalfOpen { since } => Err(cfg.cooldown - now.duration_since(since)),
        }
    }

    fn record(&mut self, now: Instant, cfg: &BreakerConfig, healthy: bool) {
        self.state = match (self.state, healthy) {
            (_, true) => State::Closed { failures: 0 },
            (State::Closed { failures }, false) if failures + 1 < cfg.threshold => {
                State::Closed { failures: failures + 1 }
            }
            // threshold reached, or a half-open probe failed
            (_, false) => State::Open { until: now + cfg.cooldown },
        };
    }
}

/// Registry keyed by destination (e.g. `http POST http://x/y`).
pub struct Breakers {
    cfg: BreakerConfig,
    map: Mutex<HashMap<String, Breaker>>,
}

impl Breakers {
    pub fn new(cfg: BreakerConfig) -> Self {
        Self { cfg, map: Mutex::new(HashMap::new()) }
    }

    pub fn acquire(&self, key: &str) -> Result<(), Duration> {
        if !self.cfg.enabled {
            return Ok(());
        }
        let mut m = self.map.lock().unwrap_or_else(|e| e.into_inner());
        m.entry(key.to_string()).or_insert_with(Breaker::new).acquire(Instant::now(), &self.cfg)
    }

    pub fn record(&self, key: &str, healthy: bool) {
        if !self.cfg.enabled {
            return;
        }
        let mut m = self.map.lock().unwrap_or_else(|e| e.into_inner());
        m.entry(key.to_string()).or_insert_with(Breaker::new).record(Instant::now(), &self.cfg, healthy);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> BreakerConfig {
        BreakerConfig { enabled: true, threshold: 3, cooldown: Duration::from_secs(10) }
    }

    #[test]
    fn opens_after_threshold_consecutive_failures() {
        let (mut b, c, t0) = (Breaker::new(), cfg(), Instant::now());
        for _ in 0..2 {
            assert!(b.acquire(t0, &c).is_ok());
            b.record(t0, &c, false);
        }
        assert!(b.acquire(t0, &c).is_ok(), "2 failures < threshold 3");
        b.record(t0, &c, false);
        let wait = b.acquire(t0, &c).unwrap_err();
        assert_eq!(wait, Duration::from_secs(10));
    }

    #[test]
    fn success_resets_the_failure_count() {
        let (mut b, c, t0) = (Breaker::new(), cfg(), Instant::now());
        b.record(t0, &c, false);
        b.record(t0, &c, false);
        b.record(t0, &c, true);
        b.record(t0, &c, false);
        b.record(t0, &c, false);
        assert!(b.acquire(t0, &c).is_ok(), "never 3 in a row");
    }

    #[test]
    fn half_open_lets_exactly_one_probe_through() {
        let (mut b, c, t0) = (Breaker::new(), cfg(), Instant::now());
        for _ in 0..3 { b.record(t0, &c, false); }
        let later = t0 + Duration::from_secs(11);
        assert!(b.acquire(later, &c).is_ok(), "cooldown over: probe");
        assert!(b.acquire(later, &c).is_err(), "second caller rejected while probing");
    }

    #[test]
    fn probe_success_closes_probe_failure_reopens() {
        let (mut b, c, t0) = (Breaker::new(), cfg(), Instant::now());
        for _ in 0..3 { b.record(t0, &c, false); }
        let t1 = t0 + Duration::from_secs(11);
        b.acquire(t1, &c).unwrap();
        b.record(t1, &c, false);
        assert!(b.acquire(t1, &c).is_err(), "failed probe re-opens");

        let t2 = t1 + Duration::from_secs(11);
        b.acquire(t2, &c).unwrap();
        b.record(t2, &c, true);
        assert!(b.acquire(t2, &c).is_ok() && b.acquire(t2, &c).is_ok(), "closed again");
    }

    #[test]
    fn abandoned_probe_does_not_wedge_the_breaker() {
        let (mut b, c, t0) = (Breaker::new(), cfg(), Instant::now());
        for _ in 0..3 { b.record(t0, &c, false); }
        let t1 = t0 + Duration::from_secs(11);
        b.acquire(t1, &c).unwrap(); // probe starts… and its future is dropped: no record()
        assert!(b.acquire(t1 + Duration::from_secs(5), &c).is_err());
        assert!(b.acquire(t1 + Duration::from_secs(11), &c).is_ok(), "lease expired");
    }

    #[test]
    fn disabled_breaker_never_blocks() {
        let b = Breakers::new(BreakerConfig { enabled: false, ..cfg() });
        for _ in 0..50 { b.record("k", false); }
        assert!(b.acquire("k").is_ok());
    }

    #[test]
    fn keys_are_independent() {
        let b = Breakers::new(cfg());
        for _ in 0..3 { b.record("down", false); }
        assert!(b.acquire("down").is_err());
        assert!(b.acquire("up").is_ok());
    }
}
