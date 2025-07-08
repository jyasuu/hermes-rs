use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
    routing::{any, get},
    Router,
};
use clap::Parser;
use handlebars::Handlebars;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::net::TcpListener;
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;
use tracing::{info, error, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

pub mod config;
pub mod health;
pub mod admin;

use config::{Args, Config, WebhookRegister, Target};


#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

#[derive(Clone)]
struct AppState {
    registers: HashMap<String, WebhookRegister>,
    handlebars: Arc<Handlebars<'static>>,
    http_client: Client,
    config: Config,
}

impl AppState {
    fn new(config: Config, args: &Args) -> Self {
        let mut registers = HashMap::new();
        let mut handlebars = Handlebars::new();

        // Register templates and build endpoint map
        for (index, register) in config.registers.iter().enumerate() {
            let template_name = format!("template_{}", index);
            handlebars
                .register_template_string(&template_name, &register.template)
                .expect("Failed to register template");

            let mut register_with_template = register.clone();
            register_with_template.template = template_name;
            registers.insert(register.endpoint.clone(), register_with_template);
        }

        // Configure HTTP client with timeout
        let http_client = Client::builder()
            .timeout(Duration::from_secs(args.request_timeout))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            registers,
            handlebars: Arc::new(handlebars),
            http_client,
            config,
        }
    }
}

async fn handle_webhook(
    State(state): State<AppState>,
    Path(path): Path<String>,
    body: String,
) -> Result<Json<Value>, (StatusCode, Json<ErrorResponse>)> {
    let endpoint = format!("/{}", path);
    
    info!(
        endpoint = %endpoint,
        payload_size = body.len(),
        "Processing webhook request"
    );

    // Find the matching register
    let register = state.registers.get(&endpoint).ok_or_else(|| {
        warn!(endpoint = %endpoint, "Webhook endpoint not found");
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "Endpoint not found".to_string(),
            }),
        )
    })?;

    // Parse incoming JSON
    let request_data: Value = serde_json::from_str(&body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: format!("Invalid JSON: {}", e),
            }),
        )
    })?;

    // Convert JSON value to a map for template rendering
    let template_data = json_to_template_data(&request_data);

    // Render the template
    let rendered_payload = state
        .handlebars
        .render(&register.template, &template_data)
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("Template rendering failed: {}", e),
                }),
            )
        })?;

    // Parse the rendered payload as JSON to validate it
    let payload_json: Value = serde_json::from_str(&rendered_payload).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Rendered template is not valid JSON: {}", e),
            }),
        )
    })?;

    // Send request to target
    let method = register.target.method.to_uppercase();
    let request_builder = match method.as_str() {
        "GET" => state.http_client.get(&register.target.url),
        "POST" => state.http_client.post(&register.target.url),
        "PUT" => state.http_client.put(&register.target.url),
        "DELETE" => state.http_client.delete(&register.target.url),
        "PATCH" => state.http_client.patch(&register.target.url),
        _ => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("Unsupported HTTP method: {}", method),
                }),
            ))
        }
    };

    let response = request_builder
        .header("Content-Type", "application/json")
        .json(&payload_json)
        .send()
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("Failed to send request to target: {}", e),
                }),
            )
        })?;

    // Get response body
    let response_text = response.text().await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Failed to read response: {}", e),
            }),
        )
    })?;

    // Try to parse response as JSON, if it fails, return as string
    let response_json = serde_json::from_str::<Value>(&response_text)
        .unwrap_or_else(|_| Value::String(response_text));

    Ok(Json(serde_json::json!({
        "status": "success",
        "target_response": response_json
    })))
}

// New handler for debug endpoint
async fn handle_debug_request(
    body: String,
) -> Result<Json<Value>, (StatusCode, Json<ErrorResponse>)> {
    tracing::info!("Debug request payload: {}", body);
    Ok(Json(
        serde_json::json!({"status": "success", "message": "Payload logged"}),
    ))
}

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

fn init_logging(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let env_filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(&args.log_level))?;

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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load environment variables from .env file if present
    dotenvy::dotenv().ok();

    // Parse command line arguments and environment variables
    let args = Args::parse();

    // Initialize structured logging
    init_logging(&args)?;

    info!(
        service = "hermes-rs",
        version = env!("CARGO_PKG_VERSION"),
        "Starting webhook proxy server"
    );

    // Load configuration
    let config = Config::load(&args.config).await?;
    info!(
        config_path = %args.config.display(),
        webhook_count = config.registers.len(),
        "Loaded configuration"
    );

    // Log registered endpoints
    for register in &config.registers {
        info!(
            method = %register.method,
            endpoint = %register.endpoint,
            target_method = %register.target.method,
            target_url = %register.target.url,
            "Registered webhook endpoint"
        );
    }

    // Create application state
    let state = AppState::new(config, &args);

    // Build the router with health checks
    let mut app = Router::new()
        .route("/*path", any(handle_webhook))
        .route("/debug", axum::routing::post(handle_debug_request))
        .layer(ServiceBuilder::new().layer(TraceLayer::new_for_http()))
        .with_state(state);

    // Add health check endpoints if enabled
    if args.health_check_enabled {
        app = app
            .route("/health", get(health::health_check))
            .route("/ready", get(health::readiness_check));
        info!("Health check endpoints enabled");
    }

    let addr = format!("{}:{}", args.bind_address, args.port);

    // Start the server
    info!(
        bind_address = %args.bind_address,
        port = args.port,
        addr = %addr,
        "Starting webhook proxy server"
    );

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_json_to_template_data() {
        let json = serde_json::json!({
            "name": "John",
            "age": 30
        });

        let template_data = json_to_template_data(&json);
        assert_eq!(
            template_data.get("name").unwrap(),
            &Value::String("John".to_string())
        );
        assert_eq!(
            template_data.get("age").unwrap(),
            &Value::Number(serde_json::Number::from(30))
        );
    }

    #[tokio::test]
    async fn test_config_loading() {
        // This would require a test config file
        // You can create a test with a temporary file
    }
}
