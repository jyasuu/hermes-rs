//! Retry policy: exponential backoff with optional jitter and a time budget.

use crate::config::{AppSettings, RetryConfig};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RetryPolicy {
    /// Total tries including the first (>= 1).
    pub attempts: u32,
    pub base_delay: Duration,
    pub multiplier: f64,
    pub max_delay: Duration,
    /// Stop retrying once this long has passed since the first try.
    pub max_elapsed: Option<Duration>,
    pub jitter: bool,
}

impl RetryPolicy {
    pub fn from_config(c: &RetryConfig) -> Self {
        Self {
            attempts: c.attempts.max(1),
            base_delay: Duration::from_millis(c.delay_ms),
            multiplier: c.backoff_multiplier.max(1.0),
            max_delay: Duration::from_millis(c.max_delay_ms),
            max_elapsed: c.max_elapsed_ms.map(Duration::from_millis),
            jitter: c.jitter,
        }
    }

    /// Global default used when neither the target nor the register sets one.
    pub fn from_settings(s: &AppSettings) -> Self {
        Self::from_config(&RetryConfig {
            attempts: s.retry_attempts,
            delay_ms: s.retry_delay_ms,
            backoff_multiplier: 2.0,
            max_delay_ms: 30_000,
            max_elapsed_ms: Some(15_000),
            jitter: true,
        })
    }

    pub fn no_retry() -> Self {
        Self { attempts: 1, ..Self::from_settings(&AppSettings::default()) }
    }

    /// Deterministic wait before retry number `n` (1 = first retry), no jitter.
    pub fn base_delay_for(&self, n: u32) -> Duration {
        let exp = self.base_delay.as_secs_f64() * self.multiplier.powi(n.saturating_sub(1) as i32);
        Duration::from_secs_f64(exp.min(self.max_delay.as_secs_f64()))
    }

    /// Wait before retry `n`, honouring jitter and a server `Retry-After`
    /// (never waiting less than the server asked for, never more than
    /// `max_delay`).
    pub fn delay_for(&self, n: u32, retry_after: Option<Duration>) -> Duration {
        let mut d = self.base_delay_for(n);
        if self.jitter {
            // "equal jitter": keep half, randomise the other half.
            d = d.mul_f64(0.5 + fastrand::f64() * 0.5);
        }
        if let Some(ra) = retry_after {
            d = d.max(ra.min(self.max_delay));
        }
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> RetryPolicy {
        RetryPolicy {
            attempts: 5,
            base_delay: Duration::from_millis(100),
            multiplier: 2.0,
            max_delay: Duration::from_millis(500),
            max_elapsed: None,
            jitter: false,
        }
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        let p = policy();
        let ms: Vec<_> = (1..=5).map(|n| p.base_delay_for(n).as_millis()).collect();
        assert_eq!(ms, [100, 200, 400, 500, 500]);
    }

    #[test]
    fn huge_exponent_does_not_overflow() {
        let p = policy();
        assert_eq!(p.base_delay_for(10_000).as_millis(), 500);
    }

    #[test]
    fn jitter_stays_within_half_to_full() {
        let p = RetryPolicy { jitter: true, ..policy() };
        for _ in 0..200 {
            let d = p.delay_for(2, None).as_millis();
            assert!((100..=200).contains(&d), "{d}");
        }
    }

    #[test]
    fn retry_after_raises_the_wait_but_is_capped() {
        let p = policy();
        assert_eq!(p.delay_for(1, Some(Duration::from_millis(300))).as_millis(), 300);
        assert_eq!(p.delay_for(1, Some(Duration::from_secs(3600))).as_millis(), 500);
        assert_eq!(p.delay_for(3, Some(Duration::from_millis(50))).as_millis(), 400);
    }

    #[test]
    fn from_config_clamps_nonsense() {
        let p = RetryPolicy::from_config(&RetryConfig {
            attempts: 0, delay_ms: 10, backoff_multiplier: 0.1,
            max_delay_ms: 1000, max_elapsed_ms: None, jitter: false,
        });
        assert_eq!((p.attempts, p.multiplier), (1, 1.0));
    }
}
