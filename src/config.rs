use clap::Parser;
use serde::{de::Error as _, Deserialize, Deserializer, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// Configuration file path
    #[arg(short, long, env = "HERMES_CONFIG_PATH", default_value = "config.yml")]
    pub config: PathBuf,

    /// Server bind address
    #[arg(long, env = "HERMES_BIND_ADDRESS", default_value = "0.0.0.0")]
    pub bind_address: String,

    /// Server port
    #[arg(short, long, env = "HERMES_PORT", default_value = "3000")]
    pub port: u16,

    /// Log level
    #[arg(long, env = "HERMES_LOG_LEVEL", default_value = "info")]
    pub log_level: String,

    /// Log format (json or pretty)
    #[arg(long, env = "HERMES_LOG_FORMAT", default_value = "pretty")]
    pub log_format: String,

    /// Request timeout in seconds
    #[arg(long, env = "HERMES_REQUEST_TIMEOUT", default_value = "30")]
    pub request_timeout: u64,

    /// Maximum concurrent requests
    #[arg(long, env = "HERMES_MAX_CONCURRENT_REQUESTS", default_value = "1000")]
    pub max_concurrent_requests: usize,

    /// Reject webhook bodies larger than this many bytes (HTTP 413)
    #[arg(long, env = "HERMES_MAX_BODY_BYTES", default_value = "2097152")]
    pub max_body_bytes: usize,

    /// Health check endpoint
    #[arg(long, env = "HERMES_HEALTH_CHECK_ENABLED", default_value = "true")]
    pub health_check_enabled: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub registers: Vec<WebhookRegister>,
    #[serde(default)]
    pub settings: AppSettings,
    /// Durable queue used by registers with `mode: async`.
    #[serde(default)]
    pub outbox: OutboxSettings,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AppSettings {
    #[serde(default = "default_retry_attempts")]
    pub retry_attempts: u32,
    #[serde(default = "default_retry_delay_ms")]
    pub retry_delay_ms: u64,
    /// Expose Prometheus metrics at `GET /metrics`.
    #[serde(default = "default_enable_metrics")]
    pub enable_metrics: bool,
    /// If set, `/metrics` requires `Authorization: Bearer <token>`.
    /// Use `${ENV_VAR}` to keep it out of the file.
    #[serde(default)]
    pub metrics_token: Option<String>,
    #[serde(default)]
    pub circuit_breaker: CircuitBreakerSettings,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            retry_attempts: default_retry_attempts(),
            retry_delay_ms: default_retry_delay_ms(),
            enable_metrics: default_enable_metrics(),
            metrics_token: None,
            circuit_breaker: CircuitBreakerSettings::default(),
        }
    }
}

/// Stops hammering a destination that is down. After `failure_threshold`
/// consecutive *unhealthy* attempts (network error, timeout, 5xx, nack) the
/// destination is skipped ("circuit open") for `cooldown_secs`; then a single
/// probe is let through. 4xx replies and unroutable messages mean the
/// destination is alive and never count as failures.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CircuitBreakerSettings {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_cb_threshold")]
    pub failure_threshold: u32,
    #[serde(default = "default_cb_cooldown")]
    pub cooldown_secs: u64,
}

impl Default for CircuitBreakerSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            failure_threshold: default_cb_threshold(),
            cooldown_secs: default_cb_cooldown(),
        }
    }
}

/// Durable queue (SQLite) behind `mode: async` registers.
///
/// The file holds rendered payloads in clear text: protect it like the
/// webhooks themselves (Hermes creates it with mode 0600 on Unix).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OutboxSettings {
    /// Database file. The `HERMES_OUTBOX_PATH` env var overrides this.
    #[serde(default = "default_outbox_path")]
    pub path: PathBuf,
    /// Concurrent delivery workers.
    #[serde(default = "default_outbox_workers")]
    pub workers: usize,
    /// How long a worker may hold a job before it is presumed crashed and
    /// the job is handed to another worker. Must exceed every target timeout.
    #[serde(default = "default_outbox_lease")]
    pub lease_secs: u64,
    /// How often idle workers look for due jobs.
    #[serde(default = "default_outbox_poll")]
    pub poll_interval_ms: u64,
    /// Reject new async webhooks with 503 once this many jobs are waiting,
    /// so an outage cannot fill the disk.
    #[serde(default = "default_outbox_max_pending")]
    pub max_pending: u64,
    /// Delivered jobs are kept this long (this is also the window in which a
    /// repeated sender `Idempotency-Key` is recognised as a duplicate).
    #[serde(default = "default_outbox_delivered_retention")]
    pub delivered_retention_secs: u64,
    /// Dead-lettered jobs are kept this long unless replayed or purged.
    #[serde(default = "default_outbox_dead_retention")]
    pub dead_retention_secs: u64,
    /// Retry policy for async deliveries (nobody is waiting, so it is far more
    /// patient than the sync default). Per-target `retry` and register
    /// `retry_config` still take precedence.
    #[serde(default)]
    pub retry: OutboxRetry,
}

/// Same knobs as [`RetryConfig`] but with defaults suited to background
/// delivery. A separate type so that overriding one field (say `attempts`)
/// keeps the *outbox* defaults for the rest instead of the sync ones.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OutboxRetry {
    #[serde(default = "ob_attempts")]
    pub attempts: u32,
    #[serde(default = "ob_delay_ms")]
    pub delay_ms: u64,
    #[serde(default = "default_backoff")]
    pub backoff_multiplier: f64,
    #[serde(default = "ob_max_delay_ms")]
    pub max_delay_ms: u64,
    /// `null` (default) = keep retrying until `attempts` is used up.
    #[serde(default)]
    pub max_elapsed_ms: Option<u64>,
    #[serde(default = "default_true")]
    pub jitter: bool,
}

impl Default for OutboxRetry {
    fn default() -> Self {
        Self {
            attempts: ob_attempts(),
            delay_ms: ob_delay_ms(),
            backoff_multiplier: default_backoff(),
            max_delay_ms: ob_max_delay_ms(),
            max_elapsed_ms: None,
            jitter: true,
        }
    }
}

impl OutboxRetry {
    pub fn to_config(&self) -> RetryConfig {
        RetryConfig {
            attempts: self.attempts,
            delay_ms: self.delay_ms,
            backoff_multiplier: self.backoff_multiplier,
            max_delay_ms: self.max_delay_ms,
            max_elapsed_ms: self.max_elapsed_ms,
            jitter: self.jitter,
        }
    }
}

impl Default for OutboxSettings {
    fn default() -> Self {
        Self {
            path: default_outbox_path(),
            workers: default_outbox_workers(),
            lease_secs: default_outbox_lease(),
            poll_interval_ms: default_outbox_poll(),
            max_pending: default_outbox_max_pending(),
            delivered_retention_secs: default_outbox_delivered_retention(),
            dead_retention_secs: default_outbox_dead_retention(),
            retry: OutboxRetry::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Register
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WebhookRegister {
    pub endpoint: String,
    pub method: String,
    /// Authenticate the *sender*. Without it anyone who can reach Hermes can
    /// trigger this endpoint.
    #[serde(default)]
    pub auth: Option<AuthConfig>,
    /// Single destination (original syntax, still supported).
    #[serde(default)]
    pub target: Option<Target>,
    /// Broadcast destinations: the rendered payload is sent to every one of
    /// them concurrently. May be combined with `target`.
    #[serde(default)]
    pub targets: Vec<Target>,
    /// Default template; individual targets may override it.
    pub template: String,
    /// `sync` (default): deliver while the caller waits and report the result.
    /// `async`: save to the durable outbox, answer `202`, deliver in the background.
    #[serde(default)]
    pub mode: DispatchMode,
    /// `sync` mode only: when does the request count as successfully delivered?
    #[serde(default)]
    pub delivery: DeliveryPolicy,
    #[serde(default)]
    pub retry_config: Option<RetryConfig>,
}

impl WebhookRegister {
    /// `target` (if any) followed by `targets`, in declaration order.
    pub fn all_targets(&self) -> impl Iterator<Item = &Target> {
        self.target.iter().chain(self.targets.iter())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchMode {
    #[default]
    Sync,
    Async,
}

/// Success criterion for a broadcast.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryPolicy {
    /// Every target must succeed (default).
    #[default]
    All,
    /// At least one target must succeed.
    Any,
    /// Always answer 200; per-target results are still reported.
    BestEffort,
}

impl DeliveryPolicy {
    pub fn is_satisfied(self, ok: usize, total: usize) -> bool {
        match self {
            DeliveryPolicy::All => ok == total,
            DeliveryPolicy::Any => ok >= 1,
            DeliveryPolicy::BestEffort => true,
        }
    }
}

// ---------------------------------------------------------------------------
// Inbound authentication
// ---------------------------------------------------------------------------

/// How a register verifies that a request really comes from its sender.
/// Verification happens on the raw body, before anything is parsed.
#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthConfig {
    /// HMAC signature of the request body (GitHub, Gitea, Shopify, …).
    Hmac(HmacAuth),
    /// Shared token (`Authorization: Bearer …`, or a custom header such as
    /// GitLab's `X-Gitlab-Token`).
    Bearer(BearerAuth),
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HmacAuth {
    /// Shared secret; use `${ENV_VAR}` to keep it out of the file.
    pub secret: String,
    /// Header carrying the signature.
    #[serde(default = "default_signature_header")]
    pub header: String,
    #[serde(default)]
    pub algorithm: HmacAlgorithm,
    #[serde(default)]
    pub encoding: SignatureEncoding,
    /// Text before the signature in the header. Default: `sha256=` / `sha1=`
    /// for hex (GitHub style), none for base64 (Shopify style).
    #[serde(default)]
    pub prefix: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BearerAuth {
    pub token: String,
    /// `None`: expect `Authorization: Bearer <token>`.
    /// `Some(h)`: expect the header `h` to hold exactly the token.
    #[serde(default)]
    pub header: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HmacAlgorithm {
    #[default]
    Sha256,
    /// Legacy senders only (e.g. GitHub's `X-Hub-Signature`).
    Sha1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SignatureEncoding {
    #[default]
    Hex,
    Base64,
}

impl HmacAuth {
    /// The prefix actually expected in front of the signature.
    pub fn expected_prefix(&self) -> String {
        match (&self.prefix, self.encoding, self.algorithm) {
            (Some(p), _, _) => p.clone(),
            (None, SignatureEncoding::Base64, _) => String::new(),
            (None, SignatureEncoding::Hex, HmacAlgorithm::Sha256) => "sha256=".to_string(),
            (None, SignatureEncoding::Hex, HmacAlgorithm::Sha1) => "sha1=".to_string(),
        }
    }
}

impl AuthConfig {
    pub fn kind(&self) -> &'static str {
        match self {
            AuthConfig::Hmac(_) => "hmac",
            AuthConfig::Bearer(_) => "bearer",
        }
    }
}

// Secrets must never reach a log line through a stray `{:?}`.
impl std::fmt::Debug for HmacAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HmacAuth")
            .field("secret", &"<redacted>")
            .field("header", &self.header)
            .field("algorithm", &self.algorithm)
            .field("encoding", &self.encoding)
            .field("prefix", &self.prefix)
            .finish()
    }
}
impl std::fmt::Debug for BearerAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BearerAuth")
            .field("token", &"<redacted>")
            .field("header", &self.header)
            .finish()
    }
}
impl std::fmt::Debug for AuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthConfig::Hmac(a) => a.fmt(f),
            AuthConfig::Bearer(a) => a.fmt(f),
        }
    }
}

fn default_signature_header() -> String {
    "X-Hub-Signature-256".to_string()
}

// ---------------------------------------------------------------------------
// Targets
// ---------------------------------------------------------------------------

/// One destination of a webhook. In YAML the kind is selected with `type:`
/// (`http` | `rabbitmq` | `nats`); when omitted it is `http`, so configs
/// written before multi-sink support keep working unchanged.
#[derive(Debug, Clone, Serialize)]
pub struct Target {
    /// Label used in logs and in the response; defaults to `<type>:<index>`.
    pub name: Option<String>,
    /// Overrides the register's template for this target only.
    pub template: Option<String>,
    /// Overrides the register's `retry_config` / global `settings` for this target.
    pub retry: Option<RetryConfig>,
    #[serde(flatten)]
    pub kind: TargetKind,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum TargetKind {
    Http(HttpTarget),
    Rabbitmq(RabbitMqTarget),
    Nats(NatsTarget),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HttpTarget {
    pub url: String,
    #[serde(default = "default_http_method")]
    pub method: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RabbitMqTarget {
    /// `amqp://user:pass@host:5672/%2f` or `amqps://…`
    pub url: String,
    /// Exchange to publish to. Empty = the default exchange, in which case
    /// `routing_key` is the destination queue name.
    #[serde(default)]
    pub exchange: String,
    /// If set, the exchange is (idempotently) declared on first use as a
    /// durable exchange of this type: direct | fanout | topic | headers.
    #[serde(default)]
    pub exchange_type: Option<String>,
    #[serde(default)]
    pub routing_key: String,
    /// If set, a durable queue of this name is declared on first use and
    /// bound to `exchange` with `routing_key` (when `exchange` is non-empty).
    #[serde(default)]
    pub queue: Option<String>,
    /// Mark messages persistent (delivery_mode=2).
    #[serde(default = "default_true")]
    pub persistent: bool,
    /// Treat "no queue matched" as a failure instead of silently dropping.
    #[serde(default = "default_true")]
    pub mandatory: bool,
    /// Extra AMQP message headers.
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NatsTarget {
    /// `nats://host:4222` (credentials may be embedded as `user:pass@`).
    pub url: String,
    pub subject: String,
    /// Publish through JetStream and wait for the stream's ack.
    #[serde(default)]
    pub jetstream: bool,
    /// NATS message headers (requires server ≥ 2.2).
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

impl<'de> Deserialize<'de> for Target {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde_yaml::Value;

        let mut map = match Value::deserialize(d)? {
            Value::Mapping(m) => m,
            _ => return Err(D::Error::custom("target must be a mapping")),
        };

        let mut take_string = |key: &str| -> Result<Option<String>, D::Error> {
            match map.remove(key) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(s)) => Ok(Some(s)),
                Some(_) => Err(D::Error::custom(format!("target.{key} must be a string"))),
            }
        };
        let kind_name = take_string("type")?.unwrap_or_else(|| "http".to_string());
        let name = take_string("name")?;
        let template = take_string("template")?;

        let retry = match map.remove("retry") {
            None | Some(Value::Null) => None,
            Some(v) => Some(serde_yaml::from_value(v).map_err(|e| D::Error::custom(format!("target.retry: {e}")))?),
        };

        let rest = Value::Mapping(map);
        let kind = match kind_name.to_ascii_lowercase().as_str() {
            "http" | "https" => TargetKind::Http(serde_yaml::from_value(rest).map_err(D::Error::custom)?),
            "rabbitmq" | "amqp" => {
                TargetKind::Rabbitmq(serde_yaml::from_value(rest).map_err(D::Error::custom)?)
            }
            "nats" => TargetKind::Nats(serde_yaml::from_value(rest).map_err(D::Error::custom)?),
            other => {
                return Err(D::Error::custom(format!(
                    "unknown target type '{other}' (expected http, rabbitmq or nats)"
                )))
            }
        };

        Ok(Target { name, template, retry, kind })
    }
}

impl Target {
    pub fn type_name(&self) -> &'static str {
        match self.kind {
            TargetKind::Http(_) => "http",
            TargetKind::Rabbitmq(_) => "rabbitmq",
            TargetKind::Nats(_) => "nats",
        }
    }

    /// Stable display label for logs/responses.
    pub fn label(&self, index: usize) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("{}:{}", self.type_name(), index))
    }

    /// Human-readable destination with credentials stripped. Safe to log.
    pub fn describe(&self) -> String {
        match &self.kind {
            TargetKind::Http(h) => format!("{} {}", h.method.to_uppercase(), redact_url(&h.url)),
            TargetKind::Rabbitmq(r) => format!(
                "{} exchange='{}' key='{}'",
                redact_url(&r.url),
                r.exchange,
                r.routing_key
            ),
            TargetKind::Nats(n) => format!(
                "{} subject='{}'{}",
                redact_url(&n.url),
                n.subject,
                if n.jetstream { " (jetstream)" } else { "" }
            ),
        }
    }
}

/// Strips `user:password@` from a URL so it can be logged.
pub fn redact_url(raw: &str) -> String {
    match reqwest::Url::parse(raw) {
        Ok(mut u) => {
            let _ = u.set_username("");
            let _ = u.set_password(None);
            u.to_string()
        }
        Err(_) => raw.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Misc config
// ---------------------------------------------------------------------------

/// Retry policy for failed deliveries. Only *transient* failures are retried
/// (network errors, timeouts, HTTP 5xx/408/429, broker nacks); a 4xx reply or
/// an unroutable message fails immediately.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RetryConfig {
    /// Total tries including the first one (`1` = never retry).
    #[serde(default = "default_retry_attempts")]
    pub attempts: u32,
    /// Delay before the first retry.
    #[serde(default = "default_retry_delay_ms")]
    pub delay_ms: u64,
    /// Each further retry waits this much longer than the previous one.
    #[serde(default = "default_backoff")]
    pub backoff_multiplier: f64,
    /// Upper bound for a single wait.
    #[serde(default = "default_max_delay_ms")]
    pub max_delay_ms: u64,
    /// Give up retrying once this much time has passed since the first try
    /// (bounds how long the webhook caller waits). `null` = no bound.
    #[serde(default = "default_max_elapsed_ms")]
    pub max_elapsed_ms: Option<u64>,
    /// Randomise each wait to between 50% and 100% of its value so many
    /// failing deliveries don't retry in lock-step.
    #[serde(default = "default_true")]
    pub jitter: bool,
}

fn default_retry_attempts() -> u32 {
    3
}
fn default_retry_delay_ms() -> u64 {
    1000
}
fn default_enable_metrics() -> bool {
    false
}
fn default_backoff() -> f64 {
    2.0
}
fn default_max_delay_ms() -> u64 {
    30_000
}
fn default_max_elapsed_ms() -> Option<u64> {
    Some(15_000)
}
fn default_cb_threshold() -> u32 {
    5
}
fn default_cb_cooldown() -> u64 {
    30
}
fn default_outbox_path() -> PathBuf {
    PathBuf::from("hermes-outbox.db")
}
fn default_outbox_workers() -> usize {
    4
}
fn default_outbox_lease() -> u64 {
    300
}
fn default_outbox_poll() -> u64 {
    500
}
fn default_outbox_max_pending() -> u64 {
    100_000
}
fn default_outbox_delivered_retention() -> u64 {
    24 * 3600
}
fn default_outbox_dead_retention() -> u64 {
    7 * 24 * 3600
}
fn ob_attempts() -> u32 {
    8
}
fn ob_delay_ms() -> u64 {
    1_000
}
fn ob_max_delay_ms() -> u64 {
    300_000
}
fn default_http_method() -> String {
    "POST".to_string()
}
fn default_true() -> bool {
    true
}

/// `${VAR}`, `${VAR:-default}`, and `$$` for a literal `$`.
/// An unset variable without a default is an error: failing at startup beats
/// quietly running with an empty secret.
pub fn expand_env(input: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some((_, '$')) => {
                chars.next();
                out.push('$');
            }
            Some((_, '{')) => {
                chars.next();
                let start = i + 2;
                let end = input[start..].find('}').map(|e| start + e).ok_or("unterminated '${'")?;
                let (name, default) = match input[start..end].split_once(":-") {
                    Some((n, d)) => (n, Some(d)),
                    None => (&input[start..end], None),
                };
                let valid = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                    && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
                if !valid {
                    return Err(format!("invalid variable name '{name}' in '${{…}}'"));
                }
                match lookup(name).filter(|v| !v.is_empty()).or(default.map(str::to_string)) {
                    Some(v) => out.push_str(&v),
                    None => return Err(format!("environment variable {name} is not set")),
                }
                // skip past the closing brace
                while chars.peek().is_some_and(|(j, _)| *j <= end) {
                    chars.next();
                }
            }
            _ => out.push('$'),
        }
    }
    Ok(out)
}

impl Config {
    pub async fn load(path: &PathBuf) -> Result<Self, Box<dyn std::error::Error>> {
        let content = tokio::fs::read_to_string(path).await?;
        Self::from_yaml(&content)
    }

    pub fn from_yaml(content: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let mut config: Config = serde_yaml::from_str(content)?;
        config.expand_env_vars(&|name| std::env::var(name).ok())?;
        Ok(config)
    }

    /// Replaces `${VAR}` / `${VAR:-default}` in the fields that hold
    /// credentials and addresses: auth secrets, target URLs and header
    /// values, the metrics token. Templates are deliberately NOT touched, so a
    /// `${…}` inside a payload template stays literal. `$$` is a literal `$`.
    pub fn expand_env_vars(&mut self, lookup: &dyn Fn(&str) -> Option<String>) -> Result<(), String> {
        fn ex(v: &mut String, at: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Result<(), String> {
            *v = expand_env(v, lookup).map_err(|e| format!("{at}: {e}"))?;
            Ok(())
        }
        if let Some(t) = &mut self.settings.metrics_token {
            ex(t, "settings.metrics_token", lookup)?;
        }
        for reg in &mut self.registers {
            let at = reg.endpoint.clone();
            match &mut reg.auth {
                Some(AuthConfig::Hmac(a)) => ex(&mut a.secret, &format!("{at}: auth.secret"), lookup)?,
                Some(AuthConfig::Bearer(a)) => ex(&mut a.token, &format!("{at}: auth.token"), lookup)?,
                None => {}
            }
            for t in reg.target.iter_mut().chain(reg.targets.iter_mut()) {
                let (url, headers) = match &mut t.kind {
                    TargetKind::Http(h) => (&mut h.url, &mut h.headers),
                    TargetKind::Rabbitmq(r) => (&mut r.url, &mut r.headers),
                    TargetKind::Nats(n) => (&mut n.url, &mut n.headers),
                };
                ex(url, &format!("{at}: target url"), lookup)?;
                for (k, v) in headers.iter_mut() {
                    ex(v, &format!("{at}: header '{k}'"), lookup)?;
                }
            }
        }
        Ok(())
    }

    /// Outbox database path; `HERMES_OUTBOX_PATH` wins over the file.
    pub fn outbox_path(&self) -> PathBuf {
        std::env::var_os("HERMES_OUTBOX_PATH")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| self.outbox.path.clone())
    }

    pub fn has_async_registers(&self) -> bool {
        self.registers.iter().any(|r| r.mode == DispatchMode::Async)
    }

    /// Semantic checks beyond what serde enforces. Used at server startup and
    /// by `hermes-admin validate-config`.
    pub fn validate(&self) -> Result<(), String> {
        let valid_methods = ["GET", "POST", "PUT", "DELETE", "PATCH"];
        let mut seen = HashSet::new();

        if self.settings.retry_attempts == 0 {
            return Err("settings.retry_attempts must be >= 1 (1 = no retries)".into());
        }
        let cb = &self.settings.circuit_breaker;
        if cb.enabled && (cb.failure_threshold == 0 || cb.cooldown_secs == 0) {
            return Err("settings.circuit_breaker: failure_threshold and cooldown_secs must be >= 1".into());
        }
        if self.settings.metrics_token.as_deref() == Some("") {
            return Err("settings.metrics_token cannot be empty".into());
        }
        let ob = &self.outbox;
        if ob.workers == 0 || ob.lease_secs == 0 || ob.poll_interval_ms == 0 || ob.max_pending == 0 {
            return Err("outbox: workers, lease_secs, poll_interval_ms and max_pending must be >= 1".into());
        }
        let check_retry = |at: &str, r: &RetryConfig| -> Result<(), String> {
            if r.attempts == 0 {
                return Err(format!("{at}: retry.attempts must be >= 1 (1 = no retries)"));
            }
            if !(r.backoff_multiplier.is_finite() && r.backoff_multiplier >= 1.0) {
                return Err(format!("{at}: retry.backoff_multiplier must be >= 1.0"));
            }
            Ok(())
        };

        for (i, reg) in self.registers.iter().enumerate() {
            let at = format!("Register {i} ({})", reg.endpoint);

            if !reg.endpoint.starts_with('/') {
                return Err(format!("{at}: endpoint must start with '/'"));
            }
            if !seen.insert(reg.endpoint.as_str()) {
                return Err(format!("{at}: duplicate endpoint"));
            }
            if ["/health", "/ready", "/metrics", "/debug"].contains(&reg.endpoint.as_str()) {
                return Err(format!("{at}: this path is reserved for Hermes itself"));
            }
            match &reg.auth {
                Some(AuthConfig::Hmac(a)) => {
                    if a.secret.is_empty() {
                        return Err(format!("{at}: auth.secret cannot be empty"));
                    }
                    reqwest::header::HeaderName::from_bytes(a.header.as_bytes())
                        .map_err(|e| format!("{at}: auth.header '{}' is not a valid header name: {e}", a.header))?;
                }
                Some(AuthConfig::Bearer(a)) => {
                    if a.token.is_empty() {
                        return Err(format!("{at}: auth.token cannot be empty"));
                    }
                    if let Some(h) = &a.header {
                        reqwest::header::HeaderName::from_bytes(h.as_bytes())
                            .map_err(|e| format!("{at}: auth.header '{h}' is not a valid header name: {e}"))?;
                    }
                }
                None => {}
            }
            if !valid_methods.contains(&reg.method.to_uppercase().as_str()) {
                return Err(format!("{at}: invalid HTTP method '{}'", reg.method));
            }

            let targets: Vec<&Target> = reg.all_targets().collect();
            if targets.is_empty() {
                return Err(format!("{at}: needs at least one of `target` or `targets`"));
            }

            let hbs = crate::template::new_handlebars();
            let check_template = |label: &str, tpl: &str| {
                let mut h = hbs.clone();
                h.register_template_string("t", tpl)
                    .map_err(|e| format!("{at}: {label} template error: {e}"))
            };
            check_template("register", &reg.template)?;
            if let Some(r) = &reg.retry_config {
                check_retry(&at, r)?;
            }
            if reg.mode == DispatchMode::Async {
                check_retry(&format!("{at} (outbox.retry)"), &ob.retry.to_config())?;
            }

            for (j, t) in targets.iter().enumerate() {
                let tl = format!("{at}, target '{}'", t.label(j));
                if let Some(tpl) = &t.template {
                    check_template(&format!("target '{}'", t.label(j)), tpl)?;
                }
                if let Some(r) = &t.retry {
                    check_retry(&tl, r)?;
                }
                match &t.kind {
                    TargetKind::Http(h) => {
                        if h.url.is_empty() {
                            return Err(format!("{tl}: target URL cannot be empty"));
                        }
                        if reg.mode == DispatchMode::Async && h.timeout_seconds.unwrap_or(0) >= ob.lease_secs {
                            return Err(format!(
                                "{tl}: timeout_seconds must be below outbox.lease_secs ({}), otherwise a slow \
                                 delivery could be handed to a second worker",
                                ob.lease_secs
                            ));
                        }
                        if !valid_methods.contains(&h.method.to_uppercase().as_str()) {
                            return Err(format!("{tl}: invalid HTTP method '{}'", h.method));
                        }
                    }
                    TargetKind::Rabbitmq(r) => {
                        if !(r.url.starts_with("amqp://") || r.url.starts_with("amqps://")) {
                            return Err(format!("{tl}: url must start with amqp:// or amqps://"));
                        }
                        if r.exchange.is_empty() && r.routing_key.is_empty() && r.queue.is_none() {
                            return Err(format!(
                                "{tl}: set `exchange`, or `routing_key`/`queue` (default exchange)"
                            ));
                        }
                        if r.exchange.is_empty() && r.exchange_type.is_some() {
                            return Err(format!("{tl}: `exchange_type` needs an `exchange` name"));
                        }
                    }
                    TargetKind::Nats(n) => {
                        if !(n.url.starts_with("nats://")
                            || n.url.starts_with("tls://")
                            || n.url.starts_with("ws://")
                            || n.url.starts_with("wss://"))
                        {
                            return Err(format!("{tl}: url must start with nats://, tls://, ws:// or wss://"));
                        }
                        if n.subject.trim().is_empty() || n.subject.contains(char::is_whitespace) {
                            return Err(format!("{tl}: subject must be non-empty and contain no spaces"));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> Config {
        Config::from_yaml(yaml).expect("yaml should parse")
    }

    #[test]
    fn legacy_target_without_type_is_http() {
        let cfg = parse(
            r#"
registers:
  - endpoint: /webhook/a
    method: POST
    target:
      url: http://localhost:3000/debug
      method: POST
      headers: { X-A: b }
    template: '{"a":1}'
"#,
        );
        cfg.validate().unwrap();
        let t = cfg.registers[0].all_targets().next().unwrap();
        assert!(matches!(&t.kind, TargetKind::Http(h) if h.url.ends_with("/debug")));
    }

    #[test]
    fn broadcast_mixed_targets_parse() {
        let cfg = parse(
            r#"
registers:
  - endpoint: /webhook/fan
    method: POST
    delivery: any
    template: '{"x":"{{ x }}"}'
    targets:
      - { name: audit, type: http, url: "http://a/b" }
      - { name: bus, type: rabbitmq, url: "amqp://u:p@mq:5672/%2f", exchange: events, exchange_type: topic, routing_key: hook.fan }
      - { name: stream, type: nats, url: "nats://n:4222", subject: hooks.fan, jetstream: true, template: '{"y":1}' }
"#,
        );
        cfg.validate().unwrap();
        let r = &cfg.registers[0];
        assert_eq!(r.delivery, DeliveryPolicy::Any);
        let kinds: Vec<_> = r.all_targets().map(|t| t.type_name()).collect();
        assert_eq!(kinds, ["http", "rabbitmq", "nats"]);
    }

    #[test]
    fn target_and_targets_are_merged_in_order() {
        let cfg = parse(
            r#"
registers:
  - endpoint: /w
    method: POST
    template: '{}'
    target: { url: "http://first" }
    targets:
      - { type: nats, url: "nats://n", subject: s }
"#,
        );
        cfg.validate().unwrap();
        let kinds: Vec<_> = cfg.registers[0].all_targets().map(|t| t.type_name()).collect();
        assert_eq!(kinds, ["http", "nats"]);
    }

    #[test]
    fn unknown_type_is_rejected() {
        let err = Config::from_yaml(
            "registers:\n  - endpoint: /w\n    method: POST\n    template: '{}'\n    target: { type: kafka, url: x }\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown target type 'kafka'"), "{err}");
    }

    #[test]
    fn typo_in_broker_field_is_rejected() {
        let res = Config::from_yaml(
            "registers:\n  - endpoint: /w\n    method: POST\n    template: '{}'\n    target: { type: nats, url: 'nats://n', subject: s, subjects: x }\n",
        );
        assert!(res.is_err());
    }

    #[test]
    fn validate_rejects_missing_targets_and_bad_urls() {
        let none = parse("registers:\n  - endpoint: /w\n    method: POST\n    template: '{}'\n");
        assert!(none.validate().unwrap_err().contains("at least one"));

        let bad_amqp = parse(
            "registers:\n  - endpoint: /w\n    method: POST\n    template: '{}'\n    target: { type: rabbitmq, url: 'http://x', routing_key: q }\n",
        );
        assert!(bad_amqp.validate().unwrap_err().contains("amqp://"));

        let bad_subject = parse(
            "registers:\n  - endpoint: /w\n    method: POST\n    template: '{}'\n    target: { type: nats, url: 'nats://x', subject: 'a b' }\n",
        );
        assert!(bad_subject.validate().unwrap_err().contains("subject"));
    }

    #[test]
    fn validate_rejects_duplicate_endpoints_and_bad_templates() {
        let dup = parse(
            "registers:\n  - { endpoint: /w, method: POST, template: '{}', target: { url: 'http://a' } }\n  - { endpoint: /w, method: POST, template: '{}', target: { url: 'http://b' } }\n",
        );
        assert!(dup.validate().unwrap_err().contains("duplicate"));

        let bad_tpl = parse(
            "registers:\n  - { endpoint: /w, method: POST, template: '{{#if}}', target: { url: 'http://a' } }\n",
        );
        assert!(bad_tpl.validate().unwrap_err().contains("template"));
    }

    #[test]
    fn delivery_policy_semantics() {
        assert!(DeliveryPolicy::All.is_satisfied(3, 3));
        assert!(!DeliveryPolicy::All.is_satisfied(2, 3));
        assert!(DeliveryPolicy::Any.is_satisfied(1, 3));
        assert!(!DeliveryPolicy::Any.is_satisfied(0, 3));
        assert!(DeliveryPolicy::BestEffort.is_satisfied(0, 3));
    }

    #[test]
    fn redaction_strips_credentials() {
        let r = redact_url("amqp://user:secret@mq.local:5672/%2f");
        assert!(!r.contains("secret") && !r.contains("user"), "{r}");
        assert!(r.contains("mq.local"));
    }

    #[test]
    fn retry_config_defaults_and_overrides() {
        let cfg = parse(
            r#"
settings:
  circuit_breaker: { failure_threshold: 2, cooldown_secs: 7 }
registers:
  - endpoint: /w
    method: POST
    template: '{}'
    retry_config: { attempts: 5 }
    targets:
      - { url: "http://a", retry: { attempts: 2, delay_ms: 50, backoff_multiplier: 3.0, jitter: false, max_elapsed_ms: null } }
      - { url: "http://b" }
"#,
        );
        cfg.validate().unwrap();
        let reg = &cfg.registers[0];
        let rc = reg.retry_config.as_ref().unwrap();
        assert_eq!((rc.attempts, rc.delay_ms, rc.max_delay_ms), (5, 1000, 30_000));
        assert_eq!(rc.max_elapsed_ms, Some(15_000));
        let t0 = reg.all_targets().next().unwrap().retry.as_ref().unwrap();
        assert_eq!((t0.attempts, t0.delay_ms, t0.jitter, t0.max_elapsed_ms), (2, 50, false, None));
        assert!(reg.all_targets().nth(1).unwrap().retry.is_none());
        assert_eq!(cfg.settings.circuit_breaker.failure_threshold, 2);
        assert!(cfg.settings.circuit_breaker.enabled);
    }

    #[test]
    fn validate_rejects_bad_retry_and_breaker() {
        let zero = parse("registers:\n  - { endpoint: /w, method: POST, template: '{}', target: { url: 'http://a', retry: { attempts: 0 } } }\n");
        assert!(zero.validate().unwrap_err().contains("attempts"));
        let mult = parse("registers:\n  - { endpoint: /w, method: POST, template: '{}', retry_config: { backoff_multiplier: 0.5 }, target: { url: 'http://a' } }\n");
        assert!(mult.validate().unwrap_err().contains("backoff_multiplier"));
        let cb = parse("settings: { circuit_breaker: { failure_threshold: 0 } }\nregisters:\n  - { endpoint: /w, method: POST, template: '{}', target: { url: 'http://a' } }\n");
        assert!(cb.validate().unwrap_err().contains("circuit_breaker"));
    }

    #[test]
    fn async_mode_and_outbox_defaults() {
        let cfg = parse(
            r#"
outbox: { path: /data/o.db, workers: 2 }
registers:
  - { endpoint: /a, method: POST, mode: async, template: '{}', target: { url: "http://x" } }
  - { endpoint: /b, method: POST, template: '{}', target: { url: "http://x" } }
"#,
        );
        cfg.validate().unwrap();
        assert_eq!(cfg.registers[0].mode, DispatchMode::Async);
        assert_eq!(cfg.registers[1].mode, DispatchMode::Sync);
        assert!(cfg.has_async_registers());
        assert_eq!((cfg.outbox.workers, cfg.outbox.lease_secs, cfg.outbox.max_pending), (2, 300, 100_000));
        assert_eq!(cfg.outbox.retry.attempts, 8);
        assert_eq!(cfg.outbox.retry.max_elapsed_ms, None);
        assert!(!parse("registers: []\n").has_async_registers());
    }

    #[test]
    fn validate_rejects_target_timeout_beyond_lease_in_async() {
        let cfg = parse(
            "outbox: { lease_secs: 10 }\nregisters:\n  - { endpoint: /a, method: POST, mode: async, template: '{}', target: { url: 'http://x', timeout_seconds: 10 } }\n",
        );
        assert!(cfg.validate().unwrap_err().contains("lease_secs"));
        let sync_ok = parse(
            "outbox: { lease_secs: 10 }\nregisters:\n  - { endpoint: /a, method: POST, template: '{}', target: { url: 'http://x', timeout_seconds: 99 } }\n",
        );
        sync_ok.validate().unwrap();
    }

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
    }

    #[test]
    fn env_expansion_rules() {
        let e = env(&[("A", "alpha"), ("EMPTY", "")]);
        assert_eq!(expand_env("x-${A}-y", &e).unwrap(), "x-alpha-y");
        assert_eq!(expand_env("${MISSING:-fallback}", &e).unwrap(), "fallback");
        assert_eq!(expand_env("${EMPTY:-d}", &e).unwrap(), "d", "empty counts as unset");
        assert_eq!(expand_env("cost $$5 and $ alone", &e).unwrap(), "cost $5 and $ alone");
        assert_eq!(expand_env("${A}${A}", &e).unwrap(), "alphaalpha");
        assert_eq!(expand_env("é${A}é", &e).unwrap(), "éalphaé", "multi-byte text survives");
        assert!(expand_env("${MISSING}", &e).unwrap_err().contains("MISSING is not set"));
        assert!(expand_env("${A", &e).unwrap_err().contains("unterminated"));
        assert!(expand_env("${1bad}", &e).unwrap_err().contains("invalid variable name"));
    }

    #[test]
    fn env_is_expanded_in_secrets_and_urls_but_never_in_templates() {
        let mut cfg: Config = serde_yaml::from_str(
            r#"
settings: { metrics_token: "${TOK}" }
registers:
  - endpoint: /w
    method: POST
    auth: { type: hmac, secret: "${SECRET}" }
    template: 'cost ${NOT_EXPANDED}'
    target: { type: rabbitmq, url: "amqp://u:${PW}@mq/%2f", routing_key: q, headers: { X-K: "${TOK}" } }
"#,
        )
        .unwrap();
        cfg.expand_env_vars(&env(&[("SECRET", "s3"), ("PW", "pw"), ("TOK", "tk")])).unwrap();
        assert_eq!(cfg.settings.metrics_token.as_deref(), Some("tk"));
        assert!(matches!(&cfg.registers[0].auth, Some(AuthConfig::Hmac(a)) if a.secret == "s3"));
        assert_eq!(cfg.registers[0].template, "cost ${NOT_EXPANDED}");
        match &cfg.registers[0].target.as_ref().unwrap().kind {
            TargetKind::Rabbitmq(r) => assert_eq!((r.url.as_str(), r.headers["X-K"].as_str()), ("amqp://u:pw@mq/%2f", "tk")),
            _ => panic!(),
        }
        let mut missing: Config = serde_yaml::from_str(
            "registers:\n  - { endpoint: /w, method: POST, template: '{}', auth: { type: bearer, token: '${NOPE}' }, target: { url: 'http://a' } }\n",
        )
        .unwrap();
        let err = missing.expand_env_vars(&env(&[])).unwrap_err();
        assert!(err.contains("/w") && err.contains("NOPE"), "{err}");
    }

    #[test]
    fn auth_parses_with_defaults_and_validates() {
        let cfg = parse(
            r#"
registers:
  - { endpoint: /gh, method: POST, template: '{}', target: { url: "http://a" }, auth: { type: hmac, secret: s } }
  - { endpoint: /sh, method: POST, template: '{}', target: { url: "http://a" }, auth: { type: hmac, secret: s, header: X-Shopify-Hmac-Sha256, encoding: base64 } }
  - { endpoint: /old, method: POST, template: '{}', target: { url: "http://a" }, auth: { type: hmac, secret: s, header: X-Hub-Signature, algorithm: sha1 } }
  - { endpoint: /gl, method: POST, template: '{}', target: { url: "http://a" }, auth: { type: bearer, token: t, header: X-Gitlab-Token } }
"#,
        );
        cfg.validate().unwrap();
        let prefix = |i: usize| match &cfg.registers[i].auth {
            Some(AuthConfig::Hmac(a)) => a.expected_prefix(),
            _ => String::new(),
        };
        assert_eq!((prefix(0), prefix(1), prefix(2)), ("sha256=".into(), "".into(), "sha1=".into()));
        assert!(matches!(&cfg.registers[0].auth, Some(AuthConfig::Hmac(a)) if a.header == "X-Hub-Signature-256"));
    }

    #[test]
    fn auth_errors_are_caught() {
        let empty = parse("registers:\n  - { endpoint: /w, method: POST, template: '{}', target: { url: 'http://a' }, auth: { type: hmac, secret: '' } }\n");
        assert!(empty.validate().unwrap_err().contains("secret cannot be empty"));
        let bad_header = parse("registers:\n  - { endpoint: /w, method: POST, template: '{}', target: { url: 'http://a' }, auth: { type: bearer, token: t, header: 'bad header' } }\n");
        assert!(bad_header.validate().unwrap_err().contains("valid header name"));
        assert!(Config::from_yaml("registers:\n  - { endpoint: /w, method: POST, template: '{}', target: { url: 'http://a' }, auth: { type: hmac, secret: s, heder: X } }\n").is_err(), "typo'd field rejected");
        assert!(Config::from_yaml("registers:\n  - { endpoint: /w, method: POST, template: '{}', target: { url: 'http://a' }, auth: { type: oauth } }\n").is_err());
    }

    #[test]
    fn reserved_paths_are_rejected_and_secrets_are_redacted_in_debug() {
        for p in ["/health", "/ready", "/metrics", "/debug"] {
            let cfg = parse(&format!("registers:\n  - {{ endpoint: {p}, method: POST, template: '{{}}', target: {{ url: 'http://a' }} }}\n"));
            assert!(cfg.validate().unwrap_err().contains("reserved"), "{p}");
        }
        let cfg = parse("registers:\n  - { endpoint: /w, method: POST, template: '{}', target: { url: 'http://a' }, auth: { type: hmac, secret: topsecret } }\n");
        let dbg = format!("{:?}", cfg);
        assert!(!dbg.contains("topsecret") && dbg.contains("redacted"), "{dbg}");
    }
}
