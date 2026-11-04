//! Management HTTP server (separate from gRPC): `/livez`, `/readyz`, `/metrics`.
//! Plain HTTP/1.1 for the kubelet probes, the load balancer health check and Prometheus.

use crate::{cache::MessageCache, metrics::Metrics};
use axum::{Router, extract::State, http::StatusCode, response::IntoResponse, routing::get};
use sqlx::PgPool;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

#[derive(Clone)]
pub struct MgmtState {
    pub pool: PgPool,
    pub cache: MessageCache,
    pub metrics: Metrics,
    /// Flipped on SIGTERM so the pod leaves load balancing while in-flight calls drain.
    pub shutting_down: Arc<AtomicBool>,
}

pub fn router(state: MgmtState) -> Router {
    Router::new()
        .route("/livez", get(livez))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .with_state(state)
}

/// The process is up and serving. (Unlike the Hazelcast client, the Redis client reconnects by
/// itself, so there is no "gave up for good" state to fail liveness on.)
async fn livez() -> impl IntoResponse {
    (StatusCode::OK, "{\"status\":\"UP\"}")
}

/// Ready = not shutting down and the database answers. Redis is reported but never fatal.
async fn readyz(State(s): State<MgmtState>) -> impl IntoResponse {
    if s.shutting_down.load(Ordering::Relaxed) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "{\"status\":\"OUT_OF_SERVICE\"}".to_owned(),
        );
    }
    let db = tokio::time::timeout(
        Duration::from_secs(2),
        sqlx::query_scalar::<_, i32>("SELECT 1").fetch_one(&s.pool),
    )
    .await
    .is_ok_and(|r| r.is_ok());
    let cache = match s.cache.ping().await {
        None => "DISABLED",
        Some(true) => "UP",
        Some(false) => "DOWN",
    };
    let body = format!(
        "{{\"status\":\"{}\",\"components\":{{\"db\":\"{}\",\"cache\":\"{}\"}}}}",
        if db { "UP" } else { "DOWN" },
        if db { "UP" } else { "DOWN" },
        cache
    );
    (
        if db {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        body,
    )
}

async fn metrics(State(s): State<MgmtState>) -> impl IntoResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        s.metrics.render(),
    )
}
