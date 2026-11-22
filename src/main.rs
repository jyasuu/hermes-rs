use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::Json,
    routing::{any, get},
    Router,
};
use clap::Parser;
use handlebars::Handlebars;
use hermes_rs::{
    config::{Args, Config, DeliveryPolicy, Target},
    health,
    idempotency::resolve_message_id,
    sink::{BroadcastReport, Delivery, Dispatcher, RetryPolicy},
    template::{json_to_template_data, new_handlebars},
};
use serde::Serialize;
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};
use tokio::net::TcpListener;
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;
use tracing::{info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

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
// Application state
// ---------------------------------------------------------------------------

/// A target plus the name of the template compiled for it.
struct CompiledTarget {
    label: String,
    target: Target,
    /// `Some` when the target overrides the register's template.
    own_template: Option<String>,
    /// Effective retry policy: target > register `retry_config` > `settings`.
    retry: RetryPolicy,
}

struct CompiledRegister {
    delivery: DeliveryPolicy,
    default_template: String,
    targets: Vec<CompiledTarget>,
}

struct Inner {
    registers: HashMap<String, CompiledRegister>,
    handlebars: Handlebars<'static>,
    dispatcher: Dispatcher,
}

/// Cheap to clone: axum clones state for every request.
#[derive(Clone)]
struct AppState(Arc<Inner>);

impl AppState {
    /// Compiles every template up front. `Config::validate` must have run.
    fn new(config: &Config, args: &Args) -> Self {
        let mut hbs = new_handlebars();
        let mut registers = HashMap::new();
        let global_policy = RetryPolicy::from_settings(&config.settings);

        for (i, reg) in config.registers.iter().enumerate() {
            let register_policy = reg
                .retry_config
                .as_ref()
                .map(RetryPolicy::from_config)
                .unwrap_or(global_policy);

            let default_template = format!("reg{i}");
            hbs.register_template_string(&default_template, &reg.template)
                .unwrap_or_else(|e| panic!("Failed to register template '{default_template}': {e}"));

            let targets = reg
                .all_targets()
                .enumerate()
                .map(|(j, t)| {
                    let own_template = t.template.as_ref().map(|tpl| {
                        let name = format!("reg{i}_t{j}");
                        hbs.register_template_string(&name, tpl)
                            .unwrap_or_else(|e| panic!("Failed to register template '{name}': {e}"));
                        name
                    });
                    let retry = t.retry.as_ref().map(RetryPolicy::from_config).unwrap_or(register_policy);
                    CompiledTarget { label: t.label(j), target: t.clone(), own_template, retry }
                })
                .collect();

            registers.insert(
                reg.endpoint.clone(),
                CompiledRegister { delivery: reg.delivery, default_template, targets },
            );
        }

        AppState(Arc::new(Inner {
            registers,
            handlebars: hbs,
            dispatcher: Dispatcher::new(args.request_timeout, &config.settings),
        }))
    }
}

// ---------------------------------------------------------------------------
// Template rendering  (Single Responsibility)
// ---------------------------------------------------------------------------

fn render_template(
    handlebars: &Handlebars,
    template_name: &str,
    data: &serde_json::Map<String, Value>,
) -> Result<Value, String> {
    let rendered = handlebars
        .render(template_name, data)
        .map_err(|e| format!("Template rendering failed: {e}"))?;

    serde_json::from_str(&rendered)
        .map_err(|e| format!("Rendered template is not valid JSON: {e}"))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn handle_webhook(
    State(state): State<AppState>,
    Path(path): Path<String>,
    headers: HeaderMap,
    body: String,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let endpoint = format!("/{}", path);
    // One id for this request: shared by every target and every retry.
    let message_id = resolve_message_id(&headers);

    info!(endpoint = %endpoint, message_id = %message_id, payload_size = body.len(), "Processing webhook request");

    let register = state.0.registers.get(&endpoint).ok_or_else(|| {
        warn!(endpoint = %endpoint, "Webhook endpoint not found");
        ErrorResponse::not_found("Endpoint not found")
    })?;

    let request_data: Value = serde_json::from_str(&body)
        .map_err(|e| ErrorResponse::bad_request(format!("Invalid JSON: {}", e)))?;
    let template_data = json_to_template_data(&request_data);

    // Render the register-level template once and share it between all
    // targets that don't bring their own.
    let default_payload = render_template(&state.0.handlebars, &register.default_template, &template_data);
    if let Err(e) = &default_payload {
        if register.targets.iter().all(|t| t.own_template.is_none()) {
            return Err(ErrorResponse::internal(e.clone()));
        }
    }

    let deliveries: Vec<Delivery> = register
        .targets
        .iter()
        .map(|t| Delivery {
            label: t.label.clone(),
            target: &t.target,
            payload: match &t.own_template {
                Some(name) => render_template(&state.0.handlebars, name, &template_data),
                None => default_payload.clone(),
            },
            retry: t.retry,
        })
        .collect();

    let total = deliveries.len();
    let outcomes = state.0.dispatcher.broadcast(&message_id, deliveries).await;
    let report = BroadcastReport::new(register.delivery, outcomes);

    for r in &report.results {
        match &r.error {
            None => info!(endpoint = %endpoint, target = %r.target, kind = r.kind, attempts = r.attempts, elapsed_ms = r.elapsed_ms, "Delivered"),
            Some(err) => warn!(endpoint = %endpoint, target = %r.target, kind = r.kind, attempts = r.attempts, elapsed_ms = r.elapsed_ms, error = %err, "Delivery failed"),
        }
    }

    let status_text = if report.ok == total {
        "success"
    } else if report.satisfied && report.ok > 0 {
        "partial"
    } else {
        "failed"
    };

    let mut resp = serde_json::json!({
        "status": status_text,
        "message_id": message_id,
        "delivered": report.ok,
        "total": report.total,
        "results": report.results,
    });

    // Backward compatibility: the original single-target response shape.
    if total == 1 {
        if let Some(r) = report.results[0].response.clone() {
            resp["target_response"] = r;
        }
    }

    if !report.satisfied {
        resp["error"] = Value::String(
            report
                .results
                .iter()
                .filter_map(|r| r.error.as_ref().map(|e| format!("{}: {}", r.target, e)))
                .collect::<Vec<_>>()
                .join("; "),
        );
        return Ok((StatusCode::BAD_GATEWAY, Json(resp)));
    }

    Ok((StatusCode::OK, Json(resp)))
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

    config.validate().map_err(|e| format!("Invalid configuration: {e}"))?;

    for register in &config.registers {
        for (j, target) in register.all_targets().enumerate() {
            info!(
                method = %register.method,
                endpoint = %register.endpoint,
                target = %target.label(j),
                kind = target.type_name(),
                destination = %target.describe(),
                delivery = ?register.delivery,
                "Registered webhook endpoint"
            );
        }
    }

    let state = AppState::new(&config, &args);
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
