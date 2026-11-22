//! Delivery error that knows whether trying again could help.

use std::{fmt, time::Duration};

#[derive(Debug, Clone)]
pub struct SinkError {
    pub message: String,
    /// Transient failure (network, timeout, 5xx, nack): worth retrying.
    pub retryable: bool,
    /// Server-requested wait before the next try (`Retry-After`).
    pub retry_after: Option<Duration>,
}

impl SinkError {
    pub fn retryable(message: impl Into<String>) -> Self {
        Self { message: message.into(), retryable: true, retry_after: None }
    }

    pub fn permanent(message: impl Into<String>) -> Self {
        Self { message: message.into(), retryable: false, retry_after: None }
    }

    pub fn with_retry_after(mut self, d: Option<Duration>) -> Self {
        self.retry_after = d;
        self
    }

    /// Did the destination look *alive*? Permanent errors (a 4xx reply, an
    /// unroutable message, a bad config) still prove the other end answered,
    /// so they must not trip a circuit breaker.
    pub fn destination_healthy(&self) -> bool {
        !self.retryable
    }
}

impl fmt::Display for SinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SinkError {}
