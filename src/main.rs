use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
    routing::{any, get},
    Router,
};
use clap::Parser;
use handlebars::{Context, Handlebars, Helper, HelperResult, Output, RenderContext};
use reqwest::Client;
use serde::Serialize;
use serde_json::{Map, Value};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::net::TcpListener;
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;
use tracing::{info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

pub mod admin;
pub mod config;
pub mod health;

use config::{Args, Config, WebhookRegister};

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

impl ErrorResponse {
    fn new(msg: impl Into<String>) -> Self {
        Self { error: msg.into() }
    }

    fn bad_request(msg: impl Into<String>) -> (StatusCode, Json<Self>) {
        (StatusCode::BAD_REQUEST, Json(Self::new(msg)))
    }

    fn not_found(msg: impl Into<String>) -> (StatusCode, Json<Self>) {
        (StatusCode::NOT_FOUND, Json(Self::new(msg)))
    }

    fn internal(msg: impl Into<String>) -> (StatusCode, Json<Self>) {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(Self::new(msg)))
    }
}

type ApiError = (StatusCode, Json<ErrorResponse>);
type ApiResult<T> = Result<T, ApiError>;

// ---------------------------------------------------------------------------
// JSON string escaping  (Single Responsibility)
// ---------------------------------------------------------------------------

/// Escapes a raw string so it is safe to embed inside a JSON string literal.
/// Covers all U+0000–U+001F control characters as required by RFC 8259.
pub fn escape_json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"), // backspace
            '\x0C' => out.push_str("\\f"), // form feed
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Handlebars helper that delegates to `escape_json_string`.
/// Usage in templates: `{{jsonEscape someField}}`
pub fn json_escape_helper(
    h: &Helper,
    _: &Handlebars,
    _: &Context,
    _: &mut RenderContext,
    out: &mut dyn Output,
) -> HelperResult {
    if let Some(param) = h.param(0) {
        let raw = param.value().as_str().unwrap_or("");
        out.write(&escape_json_string(raw))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Template engine factory  (Open/Closed — add helpers without touching state)
// ---------------------------------------------------------------------------

/// Builds a `Handlebars` instance pre-loaded with all application helpers
/// and the templates from `registers`.
fn build_handlebars(registers: &[(String, &WebhookRegister)]) -> Handlebars<'static> {
    let mut hbs = Handlebars::new();

    // Register helpers here — adding a new helper never touches other code.
    hbs.register_helper("jsonEscape", Box::new(json_escape_helper));
    // Backward-compat alias so existing templates keep working.
    hbs.register_helper("escapeNewlines", Box::new(json_escape_helper));

    for (template_name, register) in registers {
        hbs.register_template_string(template_name, &register.template)
            .unwrap_or_else(|e| panic!("Failed to register template '{}': {}", template_name, e));
    }

    hbs
}

// ---------------------------------------------------------------------------
// HTTP client factory  (Single Responsibility)
// ---------------------------------------------------------------------------

fn build_http_client(timeout_secs: u64) -> Client {
    Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .build()
        .expect("Failed to create HTTP client")
}

// ---------------------------------------------------------------------------
// Application state
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct AppState {
    /// Maps endpoint path → register config (with template name substituted).
    registers: HashMap<String, WebhookRegister>,
    handlebars: Arc<Handlebars<'static>>,
    http_client: Client,
    config: Config,
}

impl AppState {
    fn new(config: Config, args: &Args) -> Self {
        // Build (template_name, register) pairs first so we can pass them to
        // `build_handlebars` without borrowing issues.
        let named: Vec<(String, WebhookRegister)> = config
            .registers
            .iter()
            .enumerate()
            .map(|(i, r)| (format!("template_{}", i), r.clone()))
            .collect();

        let hbs_input: Vec<(String, &WebhookRegister)> =
            named.iter().map(|(n, r)| (n.clone(), r)).collect();

        let handlebars = build_handlebars(&hbs_input);

        let registers: HashMap<String, WebhookRegister> = named
            .into_iter()
            .map(|(template_name, mut r)| {
                let endpoint = r.endpoint.clone();
                r.template = template_name;
                (endpoint, r)
            })
            .collect();

        Self {
            registers,
            handlebars: Arc::new(handlebars),
            http_client: build_http_client(args.request_timeout),
            config,
        }
    }
}

// ---------------------------------------------------------------------------
// Template rendering  (Single Responsibility)
// ---------------------------------------------------------------------------

fn render_template(
    handlebars: &Handlebars,
    template_name: &str,
    data: &Map<String, Value>,
) -> ApiResult<Value> {
    let rendered = handlebars
        .render(template_name, data)
        .map_err(|e| ErrorResponse::internal(format!("Template rendering failed: {}", e)))?;

    serde_json::from_str(&rendered)
        .map_err(|e| ErrorResponse::internal(format!("Rendered template is not valid JSON: {}", e)))
}

// ---------------------------------------------------------------------------
// HTTP forwarding  (Single Responsibility)
// ---------------------------------------------------------------------------

fn build_request_builder(
    client: &Client,
    method: &str,
    url: &str,
) -> ApiResult<reqwest::RequestBuilder> {
    let builder = match method.to_uppercase().as_str() {
        "GET" => client.get(url),
        "POST" => client.post(url),
        "PUT" => client.put(url),
        "DELETE" => client.delete(url),
        "PATCH" => client.patch(url),
        other => {
            return Err(ErrorResponse::internal(format!(
                "Unsupported HTTP method: {}",
                other
            )))
        }
    };
    Ok(builder)
}

fn build_header_map(
    raw_headers: &HashMap<String, String>,
) -> ApiResult<reqwest::header::HeaderMap> {
    let mut map = reqwest::header::HeaderMap::new();

    for (key, value) in raw_headers {
        let name = reqwest::header::HeaderName::from_bytes(key.as_bytes()).map_err(|e| {
            ErrorResponse::internal(format!("Invalid header name '{}': {}", key, e))
        })?;
        let val = reqwest::header::HeaderValue::from_str(value).map_err(|e| {
            ErrorResponse::internal(format!("Invalid header value for '{}': {}", key, e))
        })?;
        map.insert(name, val);
    }

    if !map.contains_key(reqwest::header::CONTENT_TYPE) {
        map.insert(
            reqwest::header::CONTENT_TYPE,
            "application/json".parse().unwrap(),
        );
    }

    Ok(map)
}

async fn forward_request(
    client: &Client,
    register: &WebhookRegister,
    payload: &Value,
) -> ApiResult<Value> {
    let builder = build_request_builder(client, &register.target.method, &register.target.url)?;
    let headers = build_header_map(&register.target.headers)?;

    let response = builder
        .headers(headers)
        .json(payload)
        .send()
        .await
        .map_err(|e| ErrorResponse::internal(format!("Failed to send request to target: {}", e)))?;

    let text = response
        .text()
        .await
        .map_err(|e| ErrorResponse::internal(format!("Failed to read target response: {}", e)))?;

    // Return parsed JSON when possible, otherwise a plain string.
    Ok(serde_json::from_str::<Value>(&text).unwrap_or_else(|_| Value::String(text)))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn handle_webhook(
    State(state): State<AppState>,
    Path(path): Path<String>,
    body: String,
) -> ApiResult<Json<Value>> {
    let endpoint = format!("/{}", path);

    info!(endpoint = %endpoint, payload_size = body.len(), "Processing webhook request");

    let register = state.registers.get(&endpoint).ok_or_else(|| {
        warn!(endpoint = %endpoint, "Webhook endpoint not found");
        ErrorResponse::not_found("Endpoint not found")
    })?;

    let request_data: Value = serde_json::from_str(&body)
        .map_err(|e| ErrorResponse::bad_request(format!("Invalid JSON: {}", e)))?;

    let template_data = json_to_template_data(&request_data);

    let payload_json = render_template(&state.handlebars, &register.template, &template_data)?;

    let target_response = forward_request(&state.http_client, register, &payload_json).await?;

    Ok(Json(serde_json::json!({
        "status": "success",
        "target_response": target_response,
    })))
}

async fn handle_debug_request(
    headers: axum::http::HeaderMap,
    body: String,
) -> ApiResult<Json<Value>> {
    info!("Debug request headers: {:?}", headers);
    info!("Debug request payload: {}", body);
    Ok(Json(
        serde_json::json!({"status": "success", "message": "Payload logged"}),
    ))
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

pub fn json_to_template_data(value: &Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map.clone(),
        _ => {
            let mut result = Map::new();
            result.insert("data".to_string(), value.clone());
            result
        }
    }
}

// ---------------------------------------------------------------------------
// Logging  (Single Responsibility)
// ---------------------------------------------------------------------------

fn init_logging(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let env_filter =
        EnvFilter::try_from_default_env().or_else(|_| EnvFilter::try_new(&args.log_level))?;

    match args.log_format.as_str() {
        "json" => {
            tracing_subscriber::registry()
                .with(env_filter)
                .with(tracing_subscriber::fmt::layer().json())
                .init();
        }
        _ => {
            tracing_subscriber::registry()
                .with(env_filter)
                .with(tracing_subscriber::fmt::layer().pretty())
                .init();
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Router  (Open/Closed — add routes without touching handler logic)
// ---------------------------------------------------------------------------

fn build_router(state: AppState, args: &Args) -> Router {
    let mut app = Router::new()
        .route("/*path", any(handle_webhook))
        .route("/debug", axum::routing::post(handle_debug_request))
        .layer(ServiceBuilder::new().layer(TraceLayer::new_for_http()))
        .with_state(state);

    if args.health_check_enabled {
        app = app
            .route("/health", get(health::health_check))
            .route("/ready", get(health::readiness_check));
        info!("Health check endpoints enabled");
    }

    app
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();

    let args = Args::parse();
    init_logging(&args)?;

    info!(
        service = "hermes-rs",
        version = env!("CARGO_PKG_VERSION"),
        "Starting webhook proxy server"
    );

    let config = Config::load(&args.config).await?;
    info!(
        config_path = %args.config.display(),
        webhook_count = config.registers.len(),
        "Loaded configuration"
    );

    for register in &config.registers {
        info!(
            method = %register.method,
            endpoint = %register.endpoint,
            target_method = %register.target.method,
            target_url = %register.target.url,
            "Registered webhook endpoint"
        );
    }

    let state = AppState::new(config, &args);
    let app = build_router(state, &args);

    let addr = format!("{}:{}", args.bind_address, args.port);
    info!(addr = %addr, "Starting webhook proxy server");

    let listener = TcpListener::bind(&addr).await?;
    info!("Webhook proxy server is ready to accept connections");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    info!("Server shutdown complete");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- escape_json_string ---

    #[test]
    fn test_escape_newline() {
        assert_eq!(escape_json_string("a\nb"), "a\\nb");
    }

    #[test]
    fn test_escape_tab() {
        assert_eq!(escape_json_string("a\tb"), "a\\tb");
    }

    #[test]
    fn test_escape_carriage_return() {
        assert_eq!(escape_json_string("a\rb"), "a\\rb");
    }

    #[test]
    fn test_escape_backslash() {
        assert_eq!(escape_json_string("a\\b"), "a\\\\b");
    }

    #[test]
    fn test_escape_double_quote() {
        assert_eq!(escape_json_string("say \"hi\""), "say \\\"hi\\\"");
    }

    #[test]
    fn test_escape_null_control_char() {
        assert_eq!(escape_json_string("\x00"), "\\u0000");
    }

    #[test]
    fn test_escape_other_control_char() {
        // U+001B = ESC
        assert_eq!(escape_json_string("\x1B"), "\\u001B");
    }

    #[test]
    fn test_escape_plain_string_unchanged() {
        assert_eq!(escape_json_string("hello world"), "hello world");
    }

    #[test]
    fn test_escape_combined() {
        let input = "line1\nline2\ttabbed\r\n\"quoted\"";
        let expected = "line1\\nline2\\ttabbed\\r\\n\\\"quoted\\\"";
        assert_eq!(escape_json_string(input), expected);
    }

    // --- json_to_template_data ---

    #[test]
    fn test_json_to_template_data_object() {
        let json = serde_json::json!({"name": "John", "age": 30});
        let data = json_to_template_data(&json);
        assert_eq!(data.get("name").unwrap(), &Value::String("John".into()));
        assert_eq!(
            data.get("age").unwrap(),
            &Value::Number(serde_json::Number::from(30))
        );
    }

    #[test]
    fn test_json_to_template_data_non_object_wrapped() {
        let json = serde_json::json!("just a string");
        let data = json_to_template_data(&json);
        assert_eq!(
            data.get("data").unwrap(),
            &Value::String("just a string".into())
        );
    }
}
