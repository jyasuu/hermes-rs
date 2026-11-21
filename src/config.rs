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

    /// Health check endpoint
    #[arg(long, env = "HERMES_HEALTH_CHECK_ENABLED", default_value = "true")]
    pub health_check_enabled: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub registers: Vec<WebhookRegister>,
    #[serde(default)]
    pub settings: AppSettings,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AppSettings {
    #[serde(default = "default_retry_attempts")]
    pub retry_attempts: u32,
    #[serde(default = "default_retry_delay_ms")]
    pub retry_delay_ms: u64,
    #[serde(default = "default_enable_metrics")]
    pub enable_metrics: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            retry_attempts: default_retry_attempts(),
            retry_delay_ms: default_retry_delay_ms(),
            enable_metrics: default_enable_metrics(),
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
    /// Single destination (original syntax, still supported).
    #[serde(default)]
    pub target: Option<Target>,
    /// Broadcast destinations: the rendered payload is sent to every one of
    /// them concurrently. May be combined with `target`.
    #[serde(default)]
    pub targets: Vec<Target>,
    /// Default template; individual targets may override it.
    pub template: String,
    /// When does the inbound request count as successfully delivered?
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

        Ok(Target { name, template, kind })
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

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RetryConfig {
    pub attempts: u32,
    pub delay_ms: u64,
    pub backoff_multiplier: f64,
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
fn default_http_method() -> String {
    "POST".to_string()
}
fn default_true() -> bool {
    true
}

impl Config {
    pub async fn load(path: &PathBuf) -> Result<Self, Box<dyn std::error::Error>> {
        let content = tokio::fs::read_to_string(path).await?;
        Self::from_yaml(&content)
    }

    pub fn from_yaml(content: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let config: Config = serde_yaml::from_str(content)?;
        Ok(config)
    }

    /// Semantic checks beyond what serde enforces. Used at server startup and
    /// by `hermes-admin validate-config`.
    pub fn validate(&self) -> Result<(), String> {
        let valid_methods = ["GET", "POST", "PUT", "DELETE", "PATCH"];
        let mut seen = HashSet::new();

        for (i, reg) in self.registers.iter().enumerate() {
            let at = format!("Register {i} ({})", reg.endpoint);

            if !reg.endpoint.starts_with('/') {
                return Err(format!("{at}: endpoint must start with '/'"));
            }
            if !seen.insert(reg.endpoint.as_str()) {
                return Err(format!("{at}: duplicate endpoint"));
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

            for (j, t) in targets.iter().enumerate() {
                let tl = format!("{at}, target '{}'", t.label(j));
                if let Some(tpl) = &t.template {
                    check_template(&format!("target '{}'", t.label(j)), tpl)?;
                }
                match &t.kind {
                    TargetKind::Http(h) => {
                        if h.url.is_empty() {
                            return Err(format!("{tl}: target URL cannot be empty"));
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
}
