use clap::Parser;
use serde::{Deserialize, Serialize};
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

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WebhookRegister {
    pub endpoint: String,
    pub method: String,
    pub target: Target,
    pub template: String,
    #[serde(default)]
    pub retry_config: Option<RetryConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Target {
    pub url: String,
    pub method: String,
    #[serde(default)]
    pub headers: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RetryConfig {
    pub attempts: u32,
    pub delay_ms: u64,
    pub backoff_multiplier: f64,
}

fn default_retry_attempts() -> u32 { 3 }
fn default_retry_delay_ms() -> u64 { 1000 }
fn default_enable_metrics() -> bool { false }

impl Config {
    pub async fn load(path: &PathBuf) -> Result<Self, Box<dyn std::error::Error>> {
        let content = tokio::fs::read_to_string(path).await?;
        let config: Config = serde_yaml::from_str(&content)?;
        Ok(config)
    }
}