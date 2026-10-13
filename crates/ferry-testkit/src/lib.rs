//! Mock backends for Ferry's tests and benchmarks.
//!
//! Each mock binds a real TCP socket (use port 0 for an ephemeral port) and
//! returns a [`MockServer`] handle. Dropping or calling [`MockServer::kill`]
//! stops it abruptly, closing open connections, which is how tests simulate a
//! backend dying.
//!
//! | Mock | Protocols | Behaviour |
//! | --- | --- | --- |
//! | [`http::spawn`] | HTTP/1.1, h2c (prior knowledge) | JSON echo of method, path, version and headers |
//! | [`grpc::spawn`] | gRPC over h2c | `Echo` service: unary, server, client and bidi streaming |
//! | [`ws::spawn`] | WebSocket over HTTP/1.1 | echoes text and binary frames |
//!
//! All three honour [`Behavior`]: a fixed latency and a random error rate.
//! The HTTP mock also accepts per-request overrides through the
//! `x-mock-latency-ms` and `x-mock-status` request headers.
//!
//! [`MockServer::set_healthy`] flips what the mocks report to health checks:
//! `GET /healthz` on the HTTP mock (200 or 503) and `grpc.health.v1.Health/Check`
//! on the gRPC mock (`SERVING` or `NOT_SERVING`). Health requests are not
//! counted in [`Stats`].

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub mod grpc;
pub mod http;
pub mod ws;

/// Latency and error injection shared by every mock.
#[derive(Debug, Clone)]
pub struct Behavior {
    /// Name reported in responses, so tests can tell backends apart.
    pub name: String,
    /// Delay added before every response.
    pub latency: Duration,
    /// Probability in `[0, 1]` that a request fails.
    pub error_rate: f64,
    /// HTTP status used for injected HTTP errors.
    pub error_status: u16,
    /// When set, the HTTP mock records a server span per request,
    /// continuing the caller's `traceparent`, as an instrumented service would.
    pub tracing: Option<Arc<ferry_telemetry::Tracing>>,
}

impl Behavior {
    /// A healthy, zero-latency backend called `name`.
    #[must_use]
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            latency: Duration::ZERO,
            error_rate: 0.0,
            error_status: 503,
            tracing: None,
        }
    }

    /// Records a server span for every HTTP request.
    #[must_use]
    pub fn with_tracing(mut self, tracing: Arc<ferry_telemetry::Tracing>) -> Self {
        self.tracing = Some(tracing);
        self
    }

    /// Sets the injected latency.
    #[must_use]
    pub fn with_latency(mut self, latency: Duration) -> Self {
        self.latency = latency;
        self
    }

    /// Sets the injected error rate and status.
    #[must_use]
    pub fn with_errors(mut self, rate: f64, status: u16) -> Self {
        self.error_rate = rate.clamp(0.0, 1.0);
        self.error_status = status;
        self
    }

    /// Rolls the dice for one request.
    pub(crate) fn should_fail(&self) -> bool {
        self.error_rate > 0.0 && fastrand::f64() < self.error_rate
    }
}

/// Counters a mock updates as it serves, plus its reported health.
#[derive(Debug)]
pub struct Stats {
    requests: AtomicU64,
    errors: AtomicU64,
    healthy: AtomicBool,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            requests: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            healthy: AtomicBool::new(true),
        }
    }
}

impl Stats {
    /// What the mock currently reports to health checks.
    #[must_use]
    pub fn healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    /// Requests (or gRPC calls, or WebSocket messages) handled.
    #[must_use]
    pub fn requests(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    /// Injected errors returned.
    #[must_use]
    pub fn errors(&self) -> u64 {
        self.errors.load(Ordering::Relaxed)
    }

    pub(crate) fn record(&self, failed: bool) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        if failed {
            self.errors.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// A running mock. Stops when dropped.
#[derive(Debug)]
pub struct MockServer {
    addr: SocketAddr,
    stats: Arc<Stats>,
    token: CancellationToken,
    task: Option<JoinHandle<()>>,
}

impl MockServer {
    pub(crate) fn new(
        addr: SocketAddr,
        stats: Arc<Stats>,
        token: CancellationToken,
        task: JoinHandle<()>,
    ) -> Self {
        Self {
            addr,
            stats,
            token,
            task: Some(task),
        }
    }

    /// Bound address.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Live counters.
    #[must_use]
    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    /// Sets what health checks see. Normal requests are unaffected.
    pub fn set_healthy(&self, healthy: bool) {
        self.stats.healthy.store(healthy, Ordering::Relaxed);
    }

    /// Stops accepting and closes every open connection immediately, then
    /// waits for the server task to finish.
    pub async fn kill(mut self) {
        self.token.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.token.cancel();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}
