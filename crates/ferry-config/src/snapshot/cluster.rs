//! Clusters: endpoints, balancing, stickiness and health checking.

use std::net::SocketAddr;
use std::time::Duration;

use http::HeaderName;
use serde::Serialize;

use super::policy::{CircuitBreakers, OutlierDetection, RetryBudget, UpstreamTls};
use super::ser;
pub use crate::model::{LbPolicy, UpstreamProtocol};

/// A group of upstream endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Cluster {
    pub(crate) name: String,
    pub(crate) protocol: UpstreamProtocol,
    pub(crate) lb_policy: LbPolicy,
    pub(crate) hash_policy: Option<HashPolicy>,
    pub(crate) health_check: Option<HealthCheck>,
    pub(crate) panic_threshold: u8,
    pub(crate) connect_timeout: Duration,
    pub(crate) tls: Option<UpstreamTls>,
    pub(crate) circuit_breakers: CircuitBreakers,
    pub(crate) outlier_detection: Option<OutlierDetection>,
    pub(crate) retry_budget: RetryBudget,
    pub(crate) endpoints: Vec<Endpoint>,
}

impl Cluster {
    /// TLS to the endpoints, if enabled.
    #[must_use]
    pub fn tls(&self) -> Option<&UpstreamTls> {
        self.tls.as_ref()
    }

    /// Concurrency limits.
    #[must_use]
    pub fn circuit_breakers(&self) -> &CircuitBreakers {
        &self.circuit_breakers
    }

    /// Passive outlier detection, if enabled.
    #[must_use]
    pub fn outlier_detection(&self) -> Option<&OutlierDetection> {
        self.outlier_detection.as_ref()
    }

    /// Retry budget.
    #[must_use]
    pub fn retry_budget(&self) -> &RetryBudget {
        &self.retry_budget
    }

    /// Cluster name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Protocol spoken to the endpoints.
    #[must_use]
    pub fn protocol(&self) -> UpstreamProtocol {
        self.protocol
    }

    /// Load-balancing policy.
    #[must_use]
    pub fn lb_policy(&self) -> LbPolicy {
        self.lb_policy
    }

    /// What `maglev` hashes. Always `Some` for `maglev`, `None` otherwise.
    #[must_use]
    pub fn hash_policy(&self) -> Option<&HashPolicy> {
        self.hash_policy.as_ref()
    }

    /// Active health checking, if configured.
    #[must_use]
    pub fn health_check(&self) -> Option<&HealthCheck> {
        self.health_check.as_ref()
    }

    /// Percentage of healthy endpoints below which health is ignored, `0..=100`.
    #[must_use]
    pub fn panic_threshold(&self) -> u8 {
        self.panic_threshold
    }

    /// Upstream connect timeout.
    #[must_use]
    pub fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }

    /// Endpoints. Never empty after validation.
    #[must_use]
    pub fn endpoints(&self) -> &[Endpoint] {
        &self.endpoints
    }
}

/// One upstream address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Endpoint {
    pub(crate) address: SocketAddr,
    pub(crate) weight: u32,
}

impl Endpoint {
    /// Socket address.
    #[must_use]
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// Relative weight, `1..=128`.
    #[must_use]
    pub fn weight(&self) -> u32 {
        self.weight
    }
}

/// The request property `maglev` hashes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub enum HashPolicy {
    /// A request header's value.
    Header(#[serde(serialize_with = "ser::header_name")] HeaderName),
    /// A cookie, generated on the response when the request lacks it.
    Cookie {
        /// Cookie name.
        name: String,
        /// `Max-Age` for a generated cookie; `None` makes a session cookie.
        ttl: Option<Duration>,
        /// `Path` for a generated cookie.
        path: String,
    },
    /// The downstream client IP.
    SourceIp,
}

/// Active health check settings, with defaults filled in.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct HealthCheck {
    pub(crate) interval: Duration,
    pub(crate) timeout: Duration,
    pub(crate) unhealthy_threshold: u32,
    pub(crate) healthy_threshold: u32,
    pub(crate) kind: HealthCheckKind,
}

impl HealthCheck {
    /// Time between checks.
    #[must_use]
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// Time allowed for one check.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Consecutive failures before an endpoint is marked unhealthy.
    #[must_use]
    pub fn unhealthy_threshold(&self) -> u32 {
        self.unhealthy_threshold
    }

    /// Consecutive successes before an endpoint is marked healthy.
    #[must_use]
    pub fn healthy_threshold(&self) -> u32 {
        self.healthy_threshold
    }

    /// Protocol-specific settings.
    #[must_use]
    pub fn kind(&self) -> &HealthCheckKind {
        &self.kind
    }
}

/// How an endpoint is probed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[non_exhaustive]
pub enum HealthCheckKind {
    /// HTTP/1.1 `GET`.
    Http {
        /// Path requested.
        path: String,
        /// `Host` header sent.
        host: String,
        /// Statuses counted as healthy.
        expected_statuses: Vec<u16>,
    },
    /// `grpc.health.v1.Health/Check` over h2c.
    Grpc {
        /// Service name asked about; empty means the whole server.
        service: String,
        /// `:authority` sent.
        authority: String,
    },
}
