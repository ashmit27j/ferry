//! The admin API (ADR 0030).
//!
//! A separate, read-only HTTP server for operators, probes and Prometheus,
//! bound to its own address (localhost by default; the pod IP in
//! Kubernetes). It never shares a port with proxied traffic.
//!
//! | Path        | What                                                        |
//! | ----------- | ----------------------------------------------------------- |
//! | `/live`     | 200 while the process runs (liveness probe)                 |
//! | `/ready`    | 200 while serving, 503 once draining (readiness probe)      |
//! | `/metrics`  | Prometheus metrics, OpenMetrics text                        |
//! | `/config`   | The active snapshot as JSON, secrets redacted               |
//! | `/clusters` | Live endpoint state: health, ejection, load, pools, budgets |
//!
//! There are no endpoints that change state, so exposing the port to the
//! cluster network reveals topology but cannot alter routing.

use std::future::Future;
use std::time::Instant;

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Serialize;
use tokio::net::TcpListener;

use crate::server::ReloadHandle;
use crate::state::ClusterRuntime;

/// Conventional admin port (Envoy's).
pub const DEFAULT_ADMIN_ADDRESS: &str = "127.0.0.1:9901";

/// The admin routes, for embedding or testing.
pub fn router(handle: ReloadHandle) -> Router {
    Router::new()
        .route("/live", get(live))
        .route("/ready", get(ready))
        .route("/metrics", get(metrics))
        .route("/config", get(config))
        .route("/clusters", get(clusters))
        .with_state(handle)
}

/// Serves the admin API on `listener` until `shutdown` resolves.
///
/// # Errors
///
/// Fails if accepting connections fails fatally.
pub async fn serve(
    listener: TcpListener,
    handle: ReloadHandle,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(listener, router(handle))
        .with_graceful_shutdown(shutdown)
        .await
}

async fn live() -> &'static str {
    "live\n"
}

async fn ready(State(h): State<ReloadHandle>) -> Response {
    if h.is_draining() {
        (StatusCode::SERVICE_UNAVAILABLE, "draining\n").into_response()
    } else {
        (StatusCode::OK, "ready\n").into_response()
    }
}

async fn metrics(State(h): State<ReloadHandle>) -> Response {
    // Encoding reads procfs; keep it off the async workers.
    let metrics = std::sync::Arc::clone(&h.telemetry().metrics);
    match tokio::task::spawn_blocking(move || metrics.encode()).await {
        Ok(Ok(text)) => (
            [(header::CONTENT_TYPE, ferry_telemetry::metrics::CONTENT_TYPE)],
            text,
        )
            .into_response(),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "cannot encode metrics\n").into_response(),
    }
}

fn json(value: &impl Serialize) -> Response {
    match serde_json::to_string_pretty(value) {
        Ok(body) => ([(header::CONTENT_TYPE, "application/json")], body).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "cannot encode\n").into_response(),
    }
}

async fn config(State(h): State<ReloadHandle>) -> Response {
    json(&*h.snapshot())
}

#[derive(Serialize)]
struct ClustersView<'a> {
    version: u64,
    clusters: Vec<ClusterView<'a>>,
}

#[derive(Serialize)]
struct ClusterView<'a> {
    name: &'a str,
    policy: String,
    eligible_endpoints: usize,
    panic: bool,
    retry_budget_remaining: u64,
    circuit_breakers: [BreakerView; 4],
    endpoints: Vec<EndpointView>,
}

#[derive(Serialize)]
struct BreakerView {
    name: &'static str,
    active: u32,
    max: u32,
}

#[derive(Serialize)]
struct EndpointView {
    address: String,
    weight: u32,
    healthy: bool,
    ejected: bool,
    ejections: u64,
    active_requests: u64,
    latency_ewma_ms: f64,
    idle_connections: usize,
}

fn cluster_view(c: &ClusterRuntime, now: Instant) -> ClusterView<'_> {
    let set = c.balancer.host_set();
    let b = &c.breakers;
    let breaker = |name, l: &ferry_lb::Limit| BreakerView {
        name,
        active: l.active(),
        max: l.max(),
    };
    ClusterView {
        name: c.config.name(),
        policy: format!("{:?}", c.balancer.policy()),
        eligible_endpoints: set.len(),
        panic: set.is_panic(),
        retry_budget_remaining: c.budget.remaining(),
        circuit_breakers: [
            breaker("connections", &b.connections),
            breaker("pending", &b.pending),
            breaker("requests", &b.requests),
            breaker("retries", &b.retries),
        ],
        endpoints: c
            .balancer
            .hosts()
            .iter()
            .enumerate()
            .map(|(i, h)| EndpointView {
                address: h.state.address().to_string(),
                weight: h.weight,
                healthy: h.state.is_healthy(),
                ejected: h.state.is_ejected(),
                ejections: h.state.ejections_total(),
                active_requests: h.state.active_requests(),
                latency_ewma_ms: (h.state.latency_estimate(now) / 1e3).round() / 1e3,
                idle_connections: c.pools.get(i).map_or(0, |p| p.idle_count()),
            })
            .collect(),
    }
}

async fn clusters(State(h): State<ReloadHandle>) -> Response {
    let runtime = h.runtime();
    let now = Instant::now();
    json(&ClustersView {
        version: runtime.snapshot.version().0,
        clusters: runtime
            .clusters
            .iter()
            .map(|c| cluster_view(c, now))
            .collect(),
    })
}
