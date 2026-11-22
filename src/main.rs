use axum::{
    extract::{DefaultBodyLimit, Path, State},
    http::{header, HeaderMap, Method, StatusCode},
    response::{IntoResponse, Json, Response},
    routing::{any, get},
    Router,
};
use clap::Parser;
use hermes_rs::{
    auth,
    config::{Args, Config, DispatchMode},
    health,
    idempotency::resolve_message_id,
    metrics::Metrics,
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
use tokio::{
    net::TcpListener,
    sync::{watch, Semaphore},
};
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

    fn respond(status: StatusCode, msg: &str) -> Response {
        (status, Json(Self::new(msg))).into_response()
    }
}

type ApiResult<T> = Result<T, (StatusCode, Json<ErrorResponse>)>;

// ---------------------------------------------------------------------------
// Application state
// ---------------------------------------------------------------------------

struct Inner {
    registry: Arc<Registry>,
    dispatcher: Arc<Dispatcher>,
    /// Present when at least one register uses `mode: async`.
    outbox: Option<Outbox>,
    max_pending: u64,
    metrics: Arc<Metrics>,
    /// `Authorization: Bearer` token required by `/metrics`, if configured.
    metrics_token: Option<String>,
    /// Bounds in-flight webhook requests (`HERMES_MAX_CONCURRENT_REQUESTS`).
    limiter: Semaphore,
}

/// Cheap to clone: axum clones state for every request.
#[derive(Clone)]
struct AppState(Arc<Inner>);

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// A finished request plus the label under which it is counted in metrics.
type Outcome = (Response, &'static str);

fn reject(status: StatusCode, outcome: &'static str, msg: &str) -> Outcome {
    (ErrorResponse::respond(status, msg), outcome)
}

async fn handle_webhook(
    State(state): State<AppState>,
    method: Method,
    Path(path): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let endpoint = format!("/{}", path);
    // Only configured endpoints become metric labels, so scanners can't
    // inflate the number of time series.
    let label = if state.0.registry.registers.contains_key(&endpoint) { endpoint.as_str() } else { "unknown" };

    let (response, outcome) = match state.0.limiter.try_acquire() {
        Ok(_permit) => process_webhook(&state, &method, &endpoint, &headers, body).await,
        Err(_) => {
            warn!(endpoint = %endpoint, "Too many concurrent requests, shedding load");
            let (mut r, o) = reject(StatusCode::SERVICE_UNAVAILABLE, "overloaded", "Server is busy, retry later");
            r.headers_mut().insert(header::RETRY_AFTER, header::HeaderValue::from_static("1"));
            (r, o)
        }
    };
    state.0.metrics.record_request(label, outcome);
    response
}

async fn process_webhook(
    state: &AppState,
    method: &Method,
    endpoint: &str,
    headers: &HeaderMap,
    body: String,
) -> Outcome {
    let Some(register) = state.0.registry.registers.get(endpoint) else {
        warn!(endpoint = %endpoint, "Webhook endpoint not found");
        return reject(StatusCode::NOT_FOUND, "not_found", "Endpoint not found");
    };

    // The declared method is now enforced (it used to be validated but ignored).
    if method.as_str() != register.method {
        warn!(endpoint = %endpoint, got = %method, expected = %register.method, "Method not allowed");
        let (mut r, o) = reject(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed", "Method not allowed");
        if let Ok(v) = header::HeaderValue::from_str(&register.method) {
            r.headers_mut().insert(header::ALLOW, v);
        }
        return (r, o);
    }

    // Authenticate on the raw bytes BEFORE parsing anything, so a caller who
    // can't prove who they are learns nothing (not even JSON parse errors).
    // The reply is always the same generic 401; the reason is logged only.
    if let Some(a) = &register.auth {
        if let Err(e) = auth::verify(a, headers, body.as_bytes()) {
            warn!(endpoint = %endpoint, scheme = a.kind(), reason = e.reason(), "Webhook authentication failed");
            return reject(StatusCode::UNAUTHORIZED, "unauthorized", "Unauthorized");
        }
    }

    // One id for this request: shared by every target and every retry.
    let message_id = resolve_message_id(headers);
    info!(endpoint = %endpoint, message_id = %message_id, payload_size = body.len(), "Processing webhook request");

    let request_data: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => return reject(StatusCode::BAD_REQUEST, "bad_request", &format!("Invalid JSON: {e}")),
    };
    let template_data = json_to_template_data(&request_data);
    let hbs = &state.0.registry.handlebars;

    // Render the register-level template once and share it between all
    // targets that don't bring their own.
    let default_payload = render_template(hbs, &register.default_template, &template_data);
    if let Err(e) = &default_payload {
        if register.targets.iter().all(|t| t.own_template.is_none()) {
            return reject(StatusCode::INTERNAL_SERVER_ERROR, "error", e);
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
        DispatchMode::Sync => deliver_sync(state, endpoint, register, &message_id, payloads).await,
        DispatchMode::Async => accept_async(state, endpoint, register, &message_id, payloads).await,
    }
}

/// `mode: sync` — deliver now, report what happened to every target.
async fn deliver_sync(
    state: &AppState,
    endpoint: &str,
    register: &CompiledRegister,
    message_id: &str,
    payloads: Vec<Result<Value, String>>,
) -> Outcome {
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
    let outcomes = state.0.dispatcher.broadcast(endpoint, message_id, deliveries).await;
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
        return ((StatusCode::BAD_GATEWAY, Json(resp)).into_response(), "failed");
    }

    ((StatusCode::OK, Json(resp)).into_response(), status_text)
}

/// `mode: async` — persist one job per target, answer 202, deliver later.
/// The reply is only sent after the jobs are safely on disk.
async fn accept_async(
    state: &AppState,
    endpoint: &str,
    register: &CompiledRegister,
    message_id: &str,
    payloads: Vec<Result<Value, String>>,
) -> Outcome {
    let Some(outbox) = state.0.outbox.as_ref() else {
        return reject(StatusCode::INTERNAL_SERVER_ERROR, "error", "Async mode is enabled but the outbox is not available");
    };

    // All-or-nothing: never accept a message we already know we can't deliver.
    let mut jobs = Vec::with_capacity(payloads.len());
    for (j, (t, payload)) in register.targets.iter().zip(payloads).enumerate() {
        match payload {
            Ok(p) => jobs.push(NewJob { target_index: j, target_label: t.label.clone(), payload: p.to_string() }),
            Err(e) => return reject(StatusCode::INTERNAL_SERVER_ERROR, "error", &format!("target '{}': {e}", t.label)),
        }
    }

    let accepted = |queued: usize, duplicate: bool| {
        (
            (
                StatusCode::ACCEPTED,
                Json(serde_json::json!({
                    "status": "accepted",
                    "message_id": message_id,
                    "queued": queued,
                    "duplicate": duplicate,
                })),
            )
                .into_response(),
            if duplicate { "duplicate" } else { "accepted" },
        )
    };

    match outbox
        .enqueue(message_id.to_string(), endpoint.to_string(), jobs, state.0.max_pending)
        .await
    {
        Ok(Enqueued::Accepted(n)) => {
            info!(endpoint = %endpoint, message_id = %message_id, jobs = n, "Webhook accepted (async)");
            accepted(n, false)
        }
        Ok(Enqueued::Duplicate) => {
            info!(endpoint = %endpoint, message_id = %message_id, "Duplicate webhook ignored (already accepted)");
            accepted(0, true)
        }
        Ok(Enqueued::Full) => {
            warn!(endpoint = %endpoint, max_pending = state.0.max_pending, "Outbox full, shedding load");
            let (mut r, o) = reject(StatusCode::SERVICE_UNAVAILABLE, "overloaded", "Outbox is full, retry later");
            r.headers_mut().insert(header::RETRY_AFTER, header::HeaderValue::from_static("5"));
            (r, o)
        }
        Err(e) => {
            error!(endpoint = %endpoint, error = %e, "Could not persist webhook");
            reject(StatusCode::INTERNAL_SERVER_ERROR, "error", "Could not persist webhook")
        }
    }
}

/// `GET /metrics` (only mounted when `settings.enable_metrics` is true).
async fn handle_metrics(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(token) = &state.0.metrics_token {
        if !auth::verify_bearer_token(&headers, token) {
            return ErrorResponse::respond(StatusCode::UNAUTHORIZED, "Unauthorized");
        }
    }
    // Outbox gauges come straight from the database so they are never stale.
    if let Some(outbox) = &state.0.outbox {
        match outbox.stats().await {
            Ok(stats) => state.0.metrics.set_outbox(&stats),
            Err(e) => error!(error = %e, "Could not read outbox stats for /metrics"),
        }
    }
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        state.0.metrics.render(),
    )
        .into_response()
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

fn build_router(state: AppState, args: &Args, enable_metrics: bool) -> Router {
    let mut app = Router::new()
        .route("/*path", any(handle_webhook))
        .route("/debug", axum::routing::post(handle_debug_request))
        .layer(DefaultBodyLimit::max(args.max_body_bytes))
        .layer(ServiceBuilder::new().layer(TraceLayer::new_for_http()))
        .with_state(state.clone());

    if enable_metrics {
        app = app.route("/metrics", get(handle_metrics).with_state(state));
        info!("Prometheus metrics enabled at /metrics");
    }

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
    let metrics = Arc::new(Metrics::new());
    let dispatcher = Arc::new(Dispatcher::new(args.request_timeout, &config.settings, metrics.clone()));

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
                metrics: metrics.clone(),
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
        metrics,
        metrics_token: config.settings.metrics_token.clone(),
        limiter: Semaphore::new(args.max_concurrent_requests.max(1)),
    }));
    let app = build_router(state, &args, config.settings.enable_metrics);

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
