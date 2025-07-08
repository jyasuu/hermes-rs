use axum::{http::StatusCode, response::Json};
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

pub async fn health_check() -> Result<Json<Value>, StatusCode> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    Ok(Json(json!({
        "status": "healthy",
        "timestamp": timestamp,
        "service": "hermes-rs",
        "version": env!("CARGO_PKG_VERSION")
    })))
}

pub async fn readiness_check() -> Result<Json<Value>, StatusCode> {
    // Add any readiness checks here (database connections, etc.)
    Ok(Json(json!({
        "status": "ready",
        "checks": {
            "config": "ok"
        }
    })))
}