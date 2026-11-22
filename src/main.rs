use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Json, Response},
    routing::{any, get},
    Router,
};
use clap::Parser;
use hermes_rs::{
    config::{Args, Config, DispatchMode},
    health,
    idempotency::resolve_message_id,
    outbox::{
        worker::{self, WorkerContext},
        Enqueued, NewJob, Outbox,
    },
    registry::{CompiledRegister, Registry},
    sink::{BroadcastReport, Delivery, Dispatcher},
    template::{json_to_template_data, render_template},
};
use serde::Serialize;
use serde_json::Value;
use std::{sync::Arc, time::Duration};
use tokio::{net::TcpListener, sync::watch};
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;
use tracing::{error, info, warn};
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

struct Inner {
    registry: Arc<Registry>,
    dispatcher: Arc<Dispatcher>,
    /// Present when at least one register uses `mode: async`.
    outbox: Option<Outbox>,
    max_pending: u64,
}

/// Cheap to clone: axum clones state for every request.
#[derive(Clone)]
struct AppState(Arc<Inner>);

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn handle_webhook(
    State(state): State<AppState>,
    Path(path): Path<String>,
    headers: HeaderMap,
    body: String,
) -> ApiResult<Response> {
    let endpoint = format!("/{}", path);
    // One id for this request: shared by every target and every retry.
    let message_id = resolve_message_id(&headers);

    info!(endpoint = %endpoint, message_id = %message_id, payload_size = body.len(), "Processing webhook request");

    let register = state.0.registry.registers.get(&endpoint).ok_or_else(|| {
        warn!(endpoint = %endpoint, "Webhook endpoint not found");
        ErrorResponse::not_found("Endpoint not found")
    })?;

    let request_data: Value = serde_json::from_str(&body)
        .map_err(|e| ErrorResponse::bad_request(format!("Invalid JSON: {}", e)))?;
    let template_data = json_to_template_data(&request_data);
    let hbs = &state.0.registry.handlebars;

    // Render the register-level template once and share it between all
    // targets that don't bring their own.
    let default_payload = render_template(hbs, &register.default_template, &template_data);
    if let Err(e) = &default_payload {
        if register.targets.iter().all(|t| t.own_template.is_none()) {
            return Err(ErrorResponse::internal(e.clone()));
        }
    }
    let payloads: Vec<Result<Value, String>> = register
        .targets
        .iter()
        .map(|t| match &t.own_template {
            Some(name) => render_template(hbs, name, &template_data),
            None => default_payload.clone(),
        })
        .collect();

    match register.mode {
        DispatchMode::Sync => deliver_sync(&state, &endpoint, register, &message_id, payloads).await,
        DispatchMode::Async => accept_async(&state, &endpoint, register, &message_id, payloads).await,
    }
}

/// `mode: sync` — deliver now, report what happened to every target.
async fn deliver_sync(
    state: &AppState,
    endpoint: &str,
    register: &CompiledRegister,
    message_id: &str,
    payloads: Vec<Result<Value, String>>,
) -> ApiResult<Response> {
    let deliveries: Vec<Delivery> = register
        .targets
        .iter()
        .zip(payloads)
        .map(|(t, payload)| Delivery {
            label: t.label.clone(),
            target: &t.target,
            payload,
            retry: t.retry,
        })
        .collect();

    let total = deliveries.len();
    let outcomes = state.0.dispatcher.broadcast(message_id, deliveries).await;
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
        return Ok((StatusCode::BAD_GATEWAY, Json(resp)).into_response());
    }

    Ok((StatusCode::OK, Json(resp)).into_response())
}

/// `mode: async` — persist one job per target, answer 202, deliver later.
/// The reply is only sent after the jobs are safely on disk.
async fn accept_async(
    state: &AppState,
    endpoint: &str,
    register: &CompiledRegister,
    message_id: &str,
    payloads: Vec<Result<Value, String>>,
) -> ApiResult<Response> {
    let outbox = state
        .0
        .outbox
        .as_ref()
        .ok_or_else(|| ErrorResponse::internal("Async mode is enabled but the outbox is not available"))?;

    // All-or-nothing: never accept a message we already know we can't deliver.
    let mut jobs = Vec::with_capacity(payloads.len());
    for (j, (t, payload)) in register.targets.iter().zip(payloads).enumerate() {
        let payload = payload.map_err(|e| ErrorResponse::internal(format!("target '{}': {e}", t.label)))?;
        jobs.push(NewJob { target_index: j, target_label: t.label.clone(), payload: payload.to_string() });
    }

    match outbox
        .enqueue(message_id.to_string(), endpoint.to_string(), jobs, state.0.max_pending)
        .await
    {
        Ok(Enqueued::Accepted(n)) => {
            info!(endpoint = %endpoint, message_id = %message_id, jobs = n, "Webhook accepted (async)");
            Ok((
                StatusCode::ACCEPTED,
                Json(serde_json::json!({
                    "status": "accepted",
                    "message_id": message_id,
                    "queued": n,
                    "duplicate": false,
                })),
            )
                .into_response())
        }
        Ok(Enqueued::Duplicate) => {
            info!(endpoint = %endpoint, message_id = %message_id, "Duplicate webhook ignored (already accepted)");
            Ok((
                StatusCode::ACCEPTED,
                Json(serde_json::json!({
                    "status": "accepted",
                    "message_id": message_id,
                    "queued": 0,
                    "duplicate": true,
                })),
            )
                .into_response())
        }
        Ok(Enqueued::Full) => {
            warn!(endpoint = %endpoint, max_pending = state.0.max_pending, "Outbox full, shedding load");
            Ok((
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::RETRY_AFTER, "5")],
                Json(ErrorResponse::new("Outbox is full, retry later")),
            )
                .into_response())
        }
        Err(e) => {
            error!(endpoint = %endpoint, error = %e, "Could not persist webhook");
            Err(ErrorResponse::internal("Could not persist webhook"))
        }
    }
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
                mode = ?register.mode,
                target = %target.label(j),
                kind = target.type_name(),
                destination = %target.describe(),
                delivery = ?register.delivery,
                "Registered webhook endpoint"
            );
        }
    }

    let registry = Arc::new(Registry::build(&config));
    let dispatcher = Arc::new(Dispatcher::new(args.request_timeout, &config.settings));

    // Async mode needs the durable outbox and its delivery workers.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut worker_handles = Vec::new();
    let outbox = if config.has_async_registers() {
        if args.request_timeout >= config.outbox.lease_secs {
            return Err(format!(
                "HERMES_REQUEST_TIMEOUT ({}s) must be below outbox.lease_secs ({}s), otherwise a slow \
                 delivery could be handed to a second worker",
                args.request_timeout, config.outbox.lease_secs
            )
            .into());
        }
        let path = config.outbox_path();
        info!(path = %path.display(), workers = config.outbox.workers, "Opening outbox");
        let outbox = Outbox::open(&path)?;
        worker_handles = worker::spawn(
            WorkerContext {
                outbox: outbox.clone(),
                registry: registry.clone(),
                dispatcher: dispatcher.clone(),
                settings: config.outbox.clone(),
            },
            shutdown_rx,
        )
        .await;
        Some(outbox)
    } else {
        None
    };

    let state = AppState(Arc::new(Inner {
        registry,
        dispatcher,
        outbox,
        max_pending: config.outbox.max_pending,
    }));
    let app = build_router(state, &args);

    let addr = format!("{}:{}", args.bind_address, args.port);
    info!(addr = %addr, "Starting webhook proxy server");

    let listener = TcpListener::bind(&addr).await?;
    info!("Webhook proxy server is ready to accept connections");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    // Stop the outbox workers: each finishes the delivery in hand, and
    // anything unfinished is recovered on the next start.
    let _ = shutdown_tx.send(true);
    if !worker_handles.is_empty() {
        info!("Waiting for outbox workers to finish");
        let all = futures_util::future::join_all(worker_handles);
        if tokio::time::timeout(Duration::from_secs(30), all).await.is_err() {
            warn!("Outbox workers did not stop within 30s; unfinished jobs will be retried on next start");
        }
    }

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
