//! `FerryPolicy`: settings the Gateway API does not cover (ADR 0034).
//!
//! A FerryPolicy uses direct policy attachment (GEP-713): `targetRefs` name
//! objects in the policy's own namespace, and the fields that apply to the
//! target's kind take effect.
//!
//! | Target kind            | Fields                                                   |
//! | ---------------------- | -------------------------------------------------------- |
//! | `Gateway`              | `listener` (limits, timeouts, TLS minimum version)       |
//! | `HTTPRoute`/`GRPCRoute`| `rateLimit` (cluster-wide), `retry`, `timeouts.idle`     |
//! | `Service`              | `loadBalancer`, `healthCheck`, `panicThreshold`, `connectTimeout`, `circuitBreakers`, `outlierDetection`, `retryBudget`, `backendTLS` |
//!
//! When two policies target the same object, the older one wins (then the
//! one first by namespace and name), as GEP-713 asks; the other reports
//! `Accepted=False, reason=Conflicted`.

use std::str::FromStr;

use ferry_config::{
    CircuitBreakersConfig, ClusterConfig, CookieHashConfig, Duration, GrpcHealthCheckConfig,
    HashPolicyConfig, HealthCheckConfig, HttpHealthCheckConfig, LbPolicy, ListenerConfig,
    ListenerLimitsConfig, ListenerTimeoutsConfig, OutlierDetectionConfig, RateLimitConfig,
    RateLimitKeyConfig, RetryBudgetConfig, RetryConfig, TlsVersion,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The FerryPolicy API group.
pub const GROUP: &str = "gateway.ferry.dev";

/// Ferry's settings for a Gateway, route or Service.
#[derive(CustomResource, Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "gateway.ferry.dev",
    version = "v1alpha1",
    kind = "FerryPolicy",
    plural = "ferrypolicies",
    shortname = "fp",
    namespaced,
    status = "FerryPolicyStatus",
    printcolumn = r#"{"name":"Accepted","type":"string","jsonPath":".status.conditions[?(@.type==\"Accepted\")].status"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct FerryPolicySpec {
    /// Objects in this namespace the policy applies to.
    pub target_refs: Vec<TargetRef>,
    /// Gateway targets: settings of every listener.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listener: Option<ListenerPolicy>,
    /// Route targets: a cluster-wide token bucket, divided between the ready
    /// proxy replicas of each Gateway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitPolicy>,
    /// Route targets: retries on failures that may have reached a backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryPolicy>,
    /// Route targets: timeouts the Gateway API does not define.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeouts: Option<TimeoutsPolicy>,
    /// Service targets: how requests are spread across endpoints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_balancer: Option<LoadBalancerPolicy>,
    /// Service targets: active health checking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_check: Option<HealthCheckPolicy>,
    /// Service targets: below this percentage of healthy endpoints, balance
    /// across all of them (0-100, default 50).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0, max = 100))]
    pub panic_threshold: Option<u8>,
    /// Service targets: upstream TCP connect timeout, e.g. `2s`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connect_timeout: Option<String>,
    /// Service targets: concurrency limits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub circuit_breakers: Option<CircuitBreakersPolicy>,
    /// Service targets: passive ejection of failing endpoints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outlier_detection: Option<OutlierDetectionPolicy>,
    /// Service targets: cap on retries relative to traffic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_budget: Option<RetryBudgetPolicy>,
    /// Service targets: TLS (optionally mutual) to the endpoints.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "backendTLS"
    )]
    pub backend_tls: Option<BackendTlsPolicy>,
}

/// A reference to an object in the policy's namespace.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TargetRef {
    /// API group: `gateway.networking.k8s.io`, or `""` for a Service.
    #[serde(default)]
    pub group: String,
    /// `Gateway`, `HTTPRoute`, `GRPCRoute` or `Service`.
    pub kind: String,
    /// Object name.
    pub name: String,
}

/// What a target reference points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TargetKind {
    /// A Gateway.
    Gateway,
    /// An HTTPRoute.
    HttpRoute,
    /// A GRPCRoute.
    GrpcRoute,
    /// A Service.
    Service,
}

impl TargetRef {
    /// The kind this reference targets, if Ferry supports it.
    #[must_use]
    pub fn target_kind(&self) -> Option<TargetKind> {
        match (self.group.as_str(), self.kind.as_str()) {
            (crate::gateway_api::GROUP, "Gateway") => Some(TargetKind::Gateway),
            (crate::gateway_api::GROUP, "HTTPRoute") => Some(TargetKind::HttpRoute),
            (crate::gateway_api::GROUP, "GRPCRoute") => Some(TargetKind::GrpcRoute),
            ("" | "core", "Service") => Some(TargetKind::Service),
            _ => None,
        }
    }
}

/// Listener settings for Gateway targets.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ListenerPolicy {
    /// Largest request header block in KiB (8-1024, default 60).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_request_headers_kb: Option<u32>,
    /// Most request headers (1-1024, default 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_request_headers: Option<u32>,
    /// Largest request body in bytes; larger requests get 413.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_request_body_bytes: Option<u64>,
    /// Most concurrent HTTP/2 streams per connection (default 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub h2_max_concurrent_streams: Option<u32>,
    /// Time allowed for request headers, e.g. `30s`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header_read_timeout: Option<String>,
    /// HTTP/2 keep-alive ping interval, e.g. `20s`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub h2_keep_alive: Option<String>,
    /// Lowest TLS version accepted on HTTPS listeners: `"1.2"` or `"1.3"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_min_version: Option<String>,
}

/// A cluster-wide token bucket.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitPolicy {
    /// Requests allowed per `per` across all proxy replicas.
    pub requests: u32,
    /// Refill period, e.g. `1s` or `1m`.
    pub per: String,
    /// Bucket size across all replicas. Defaults to `requests`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<u32>,
    /// Request header whose value keys the buckets. Absent keys by client IP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_header: Option<String>,
}

/// Retries (Gateway API GEP-1731 semantics).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RetryPolicy {
    /// Most retries after the first attempt (0-10, default 2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempts: Option<u32>,
    /// Statuses that trigger a retry (500-599; default 502, 503, 504).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub codes: Vec<u16>,
    /// Base backoff, doubled per retry with full jitter (default `25ms`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backoff: Option<String>,
}

/// Route timeouts beyond the Gateway API's `request` and `backendRequest`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TimeoutsPolicy {
    /// Longest silence on a request or response body (default `5m`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle: Option<String>,
}

/// Load-balancing policy names.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum BalancerKind {
    /// Each endpoint in turn.
    #[default]
    RoundRobin,
    /// In proportion to endpoint weight.
    WeightedRoundRobin,
    /// Fewer in-flight requests of two random endpoints.
    LeastRequest,
    /// Lower latency EWMA times load of two random endpoints.
    P2cEwma,
    /// Consistent hashing (sticky), keyed by `hash`.
    Maglev,
}

/// What Maglev hashes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HashPolicy {
    /// Hash this request header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    /// Hash this cookie, generating it when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cookie: Option<CookiePolicy>,
    /// Hash the client IP.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub source_ip: bool,
}

/// Cookie stickiness settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CookiePolicy {
    /// Cookie name.
    pub name: String,
    /// Max-Age of a generated cookie; absent makes it a session cookie.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<String>,
    /// Path of a generated cookie (default `/`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// Balancing settings for Service targets.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoadBalancerPolicy {
    /// The policy.
    #[serde(default)]
    pub kind: BalancerKind,
    /// Required for `Maglev`, rejected otherwise. Exactly one field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<HashPolicy>,
}

/// Active health checks. Exactly one of `http` and `grpc`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HealthCheckPolicy {
    /// Time between checks (default `5s`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval: Option<String>,
    /// Time allowed per check (default `1s`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
    /// Failures that mark an endpoint unhealthy (default 3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unhealthy_threshold: Option<u32>,
    /// Successes that mark it healthy again (default 2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub healthy_threshold: Option<u32>,
    /// HTTP GET check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpHealthCheck>,
    /// `grpc.health.v1` check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcHealthCheck>,
}

/// HTTP health check.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HttpHealthCheck {
    /// Path, e.g. `/healthz`.
    pub path: String,
    /// Host header (default: the cluster name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Healthy statuses (default `[200]`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expected_statuses: Vec<u16>,
}

/// gRPC health check.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GrpcHealthCheck {
    /// Service name; empty asks about the server.
    #[serde(default)]
    pub service: String,
    /// `:authority` (default: the cluster name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authority: Option<String>,
}

/// Circuit breakers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CircuitBreakersPolicy {
    /// Most upstream connections per proxy (default 1024).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_connections: Option<u32>,
    /// Most requests waiting for a connection (default 1024).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_pending_requests: Option<u32>,
    /// Most requests in flight (default 1024).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_requests: Option<u32>,
    /// Most retries in flight (default 3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<u32>,
}

/// Outlier detection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OutlierDetectionPolicy {
    /// Consecutive 5xx that eject (default 5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consecutive5xx: Option<u32>,
    /// Consecutive gateway failures that eject (default 5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consecutive_gateway_failure: Option<u32>,
    /// First ejection length (default `30s`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_ejection_time: Option<String>,
    /// Longest ejection (default `300s`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ejection_time: Option<String>,
    /// Most endpoints ejected at once, in percent (default 10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ejection_percent: Option<u8>,
}

/// Retry budget.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RetryBudgetPolicy {
    /// Retries as a percentage of requests (default 20).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_percent: Option<u8>,
    /// Retries always allowed per second (default 10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_retries_per_second: Option<u32>,
    /// Counting window (default `10s`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<String>,
}

/// TLS to a Service's endpoints. Secrets live in the policy's namespace.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackendTlsPolicy {
    /// Server name to send and verify (default: the cluster name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sni: Option<String>,
    /// A `kubernetes.io/tls` Secret whose `ca.crt` lists trusted CAs. Absent
    /// trusts the Mozilla roots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_secret: Option<String>,
    /// A `kubernetes.io/tls` Secret with the client certificate for mTLS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_certificate_secret: Option<String>,
}

/// FerryPolicy status.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FerryPolicyStatus {
    /// `Accepted`: True when every target resolved and every field applies.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(schema_with = "conditions_schema")]
    pub conditions: Vec<Condition>,
}

fn conditions_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    serde_json::from_value(serde_json::json!({
        "type": "array",
        "x-kubernetes-list-type": "map",
        "x-kubernetes-list-map-keys": ["type"],
        "items": {
            "type": "object",
            "required": ["type", "status", "lastTransitionTime", "reason", "message"],
            "properties": {
                "type": { "type": "string" },
                "status": { "type": "string", "enum": ["True", "False", "Unknown"] },
                "observedGeneration": { "type": "integer", "format": "int64" },
                "lastTransitionTime": { "type": "string", "format": "date-time" },
                "reason": { "type": "string" },
                "message": { "type": "string" }
            }
        }
    }))
    .unwrap_or_default()
}

fn duration(field: &str, value: Option<&String>) -> Result<Option<Duration>, String> {
    value
        .map(|v| Duration::from_str(v).map_err(|e| format!("{field}: {e}")))
        .transpose()
}

impl FerryPolicySpec {
    /// Checks that every set field applies to `kind` and parses. The message
    /// names the first problem.
    ///
    /// # Errors
    ///
    /// Returns a message for the policy's `Accepted=False` condition.
    pub fn check(&self, kind: TargetKind) -> Result<(), String> {
        let gateway_only = self.listener.is_some();
        let route_only =
            self.rate_limit.is_some() || self.retry.is_some() || self.timeouts.is_some();
        let service_only = self.load_balancer.is_some()
            || self.health_check.is_some()
            || self.panic_threshold.is_some()
            || self.connect_timeout.is_some()
            || self.circuit_breakers.is_some()
            || self.outlier_detection.is_some()
            || self.retry_budget.is_some()
            || self.backend_tls.is_some();
        let (allowed, what) = match kind {
            TargetKind::Gateway => (
                !route_only && !service_only,
                "Gateway targets take only `listener`",
            ),
            TargetKind::HttpRoute | TargetKind::GrpcRoute => (
                !gateway_only && !service_only,
                "route targets take only `rateLimit`, `retry` and `timeouts`",
            ),
            TargetKind::Service => (
                !gateway_only && !route_only,
                "Service targets take only backend settings",
            ),
        };
        if !allowed {
            return Err(what.into());
        }
        match kind {
            TargetKind::Gateway => {
                let mut l =
                    ListenerConfig::new("check", std::net::SocketAddr::from(([0, 0, 0, 0], 0)));
                self.apply_to_listener(&mut l)
            }
            TargetKind::HttpRoute | TargetKind::GrpcRoute => {
                self.rate_limit(1)?;
                self.retry_config()?;
                self.idle_timeout()?;
                Ok(())
            }
            TargetKind::Service => {
                let mut c = ClusterConfig::new("check", Vec::new());
                self.apply_to_cluster(&mut c)
            }
        }
    }

    /// Applies `listener` settings.
    ///
    /// # Errors
    ///
    /// Fails on an unparsable duration or TLS version.
    pub fn apply_to_listener(&self, l: &mut ListenerConfig) -> Result<(), String> {
        let Some(p) = &self.listener else {
            return Ok(());
        };
        let mut limits = ListenerLimitsConfig::default();
        limits.max_request_headers_kb = p.max_request_headers_kb;
        limits.max_request_headers = p.max_request_headers;
        limits.max_request_body_bytes = p.max_request_body_bytes;
        limits.h2_max_concurrent_streams = p.h2_max_concurrent_streams;
        l.limits = Some(limits);
        let mut timeouts = ListenerTimeoutsConfig::default();
        timeouts.header_read =
            duration("listener.headerReadTimeout", p.header_read_timeout.as_ref())?;
        timeouts.h2_keep_alive = duration("listener.h2KeepAlive", p.h2_keep_alive.as_ref())?;
        l.timeouts = Some(timeouts);
        if let (Some(v), Some(tls)) = (&p.tls_min_version, l.tls.as_mut()) {
            tls.min_version = Some(tls_version(v)?);
        } else if let Some(v) = &p.tls_min_version {
            tls_version(v)?;
        }
        Ok(())
    }

    /// The rate limit for one of `replicas` proxies: the cluster-wide limit
    /// divided between them, rounded up so the total never falls below it.
    ///
    /// # Errors
    ///
    /// Fails on an unparsable period or a zero rate.
    pub fn rate_limit(&self, replicas: u32) -> Result<Option<RateLimitConfig>, String> {
        let Some(p) = &self.rate_limit else {
            return Ok(None);
        };
        Ok(Some(split_rate_limit(p, replicas)?))
    }

    /// The retry policy, if set.
    ///
    /// # Errors
    ///
    /// Fails on an unparsable backoff.
    pub fn retry_config(&self) -> Result<Option<RetryConfig>, String> {
        let Some(p) = &self.retry else {
            return Ok(None);
        };
        let mut r = RetryConfig::default();
        r.attempts = p.attempts;
        r.codes.clone_from(&p.codes);
        r.backoff = duration("retry.backoff", p.backoff.as_ref())?;
        Ok(Some(r))
    }

    /// The idle timeout, if set.
    ///
    /// # Errors
    ///
    /// Fails on an unparsable duration.
    pub fn idle_timeout(&self) -> Result<Option<Duration>, String> {
        duration(
            "timeouts.idle",
            self.timeouts.as_ref().and_then(|t| t.idle.as_ref()),
        )
    }

    /// Applies every backend setting except TLS, whose Secrets the
    /// translator resolves.
    ///
    /// # Errors
    ///
    /// Fails on an unparsable duration or an inconsistent hash policy.
    pub fn apply_to_cluster(&self, c: &mut ClusterConfig) -> Result<(), String> {
        if let Some(lb) = &self.load_balancer {
            c.lb_policy = match lb.kind {
                BalancerKind::RoundRobin => LbPolicy::RoundRobin,
                BalancerKind::WeightedRoundRobin => LbPolicy::WeightedRoundRobin,
                BalancerKind::LeastRequest => LbPolicy::LeastRequest,
                BalancerKind::P2cEwma => LbPolicy::P2cEwma,
                BalancerKind::Maglev => LbPolicy::Maglev,
            };
            c.hash_policy = match (&lb.hash, lb.kind) {
                (None, BalancerKind::Maglev) => {
                    return Err("loadBalancer.hash is required for Maglev".into());
                }
                (Some(_), kind) if kind != BalancerKind::Maglev => {
                    return Err("loadBalancer.hash only applies to Maglev".into());
                }
                (None, _) => None,
                (Some(h), _) => Some(hash_policy(h)?),
            };
        }
        if let Some(hc) = &self.health_check {
            let mut out = HealthCheckConfig::default();
            out.interval = duration("healthCheck.interval", hc.interval.as_ref())?;
            out.timeout = duration("healthCheck.timeout", hc.timeout.as_ref())?;
            out.unhealthy_threshold = hc.unhealthy_threshold;
            out.healthy_threshold = hc.healthy_threshold;
            match (&hc.http, &hc.grpc) {
                (Some(h), None) => {
                    let mut http = HttpHealthCheckConfig::new(h.path.clone());
                    http.host.clone_from(&h.host);
                    http.expected_statuses.clone_from(&h.expected_statuses);
                    out.http = Some(http);
                }
                (None, Some(g)) => {
                    let mut grpc = GrpcHealthCheckConfig::default();
                    grpc.service.clone_from(&g.service);
                    grpc.authority.clone_from(&g.authority);
                    out.grpc = Some(grpc);
                }
                _ => return Err("healthCheck needs exactly one of http and grpc".into()),
            }
            c.health_check = Some(out);
        }
        c.panic_threshold = self.panic_threshold.or(c.panic_threshold);
        if let Some(t) = duration("connectTimeout", self.connect_timeout.as_ref())? {
            c.connect_timeout = Some(t);
        }
        if let Some(cb) = &self.circuit_breakers {
            let mut out = CircuitBreakersConfig::default();
            out.max_connections = cb.max_connections;
            out.max_pending_requests = cb.max_pending_requests;
            out.max_requests = cb.max_requests;
            out.max_retries = cb.max_retries;
            c.circuit_breakers = Some(out);
        }
        if let Some(o) = &self.outlier_detection {
            let mut out = OutlierDetectionConfig::default();
            out.consecutive_5xx = o.consecutive5xx;
            out.consecutive_gateway_failure = o.consecutive_gateway_failure;
            out.base_ejection_time = duration(
                "outlierDetection.baseEjectionTime",
                o.base_ejection_time.as_ref(),
            )?;
            out.max_ejection_time = duration(
                "outlierDetection.maxEjectionTime",
                o.max_ejection_time.as_ref(),
            )?;
            out.max_ejection_percent = o.max_ejection_percent;
            c.outlier_detection = Some(out);
        }
        if let Some(b) = &self.retry_budget {
            let mut out = RetryBudgetConfig::default();
            out.budget_percent = b.budget_percent;
            out.min_retries_per_second = b.min_retries_per_second;
            out.ttl = duration("retryBudget.ttl", b.ttl.as_ref())?;
            c.retry_budget = Some(out);
        }
        Ok(())
    }
}

fn tls_version(v: &str) -> Result<TlsVersion, String> {
    match v {
        "1.2" => Ok(TlsVersion::Tls12),
        "1.3" => Ok(TlsVersion::Tls13),
        other => Err(format!(
            "listener.tlsMinVersion: {other:?} is not \"1.2\" or \"1.3\""
        )),
    }
}

fn hash_policy(h: &HashPolicy) -> Result<HashPolicyConfig, String> {
    match (&h.header, &h.cookie, h.source_ip) {
        (Some(name), None, false) => Ok(HashPolicyConfig::Header(name.clone())),
        (None, Some(c), false) => {
            let mut cookie = CookieHashConfig::new(c.name.clone());
            cookie.ttl = duration("loadBalancer.hash.cookie.ttl", c.ttl.as_ref())?;
            cookie.path.clone_from(&c.path);
            Ok(HashPolicyConfig::Cookie(cookie))
        }
        (None, None, true) => Ok(HashPolicyConfig::SourceIp),
        _ => Err("loadBalancer.hash needs exactly one of header, cookie and sourceIP".into()),
    }
}

/// Divides a cluster-wide limit between `replicas` proxies (ADR 0034).
///
/// Each replica gets `ceil(requests / replicas)` per period and
/// `ceil(burst / replicas)` of bucket, so the fleet as a whole allows at
/// least the configured rate. With uneven load across replicas a client can
/// be limited early; NLB flow hashing keeps that small.
///
/// # Errors
///
/// Fails on an unparsable period or a zero rate.
pub fn split_rate_limit(p: &RateLimitPolicy, replicas: u32) -> Result<RateLimitConfig, String> {
    if p.requests == 0 {
        return Err("rateLimit.requests must be at least 1".into());
    }
    let per = Duration::from_str(&p.per).map_err(|e| format!("rateLimit.per: {e}"))?;
    let n = replicas.max(1);
    let share = |total: u32| total.div_ceil(n).max(1);
    let mut out = RateLimitConfig::new(share(p.requests), per);
    out.burst = p.burst.map(share);
    if let Some(h) = &p.key_header {
        out.key = RateLimitKeyConfig::Header(h.clone());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::CustomResourceExt;
    use proptest::prelude::*;

    fn spec(yaml: &str) -> FerryPolicySpec {
        serde_saphyr::from_str(yaml).unwrap()
    }

    #[test]
    fn fields_must_match_the_target_kind() {
        let s = spec(
            "targetRefs: [{ kind: Service, name: api }]\nrateLimit: { requests: 10, per: 1s }\n",
        );
        assert!(s.check(TargetKind::HttpRoute).is_ok());
        assert!(
            s.check(TargetKind::Service)
                .unwrap_err()
                .contains("Service")
        );
        assert!(s.check(TargetKind::Gateway).is_err());
    }

    #[test]
    fn service_settings_apply() {
        let s = spec(
            r"
targetRefs: [{ kind: Service, name: api }]
loadBalancer: { kind: Maglev, hash: { cookie: { name: sid, ttl: 1h } } }
healthCheck: { interval: 2s, http: { path: /healthz } }
connectTimeout: 250ms
circuitBreakers: { maxRequests: 50 }
outlierDetection: { consecutive5xx: 3, baseEjectionTime: 10s }
retryBudget: { budgetPercent: 5 }
panicThreshold: 30
",
        );
        let mut c = ClusterConfig::new("c", Vec::new());
        s.apply_to_cluster(&mut c).unwrap();
        assert_eq!(c.lb_policy, LbPolicy::Maglev);
        assert!(matches!(c.hash_policy, Some(HashPolicyConfig::Cookie(_))));
        assert_eq!(c.health_check.unwrap().interval.unwrap().to_string(), "2s");
        assert_eq!(c.circuit_breakers.unwrap().max_requests, Some(50));
        assert_eq!(c.outlier_detection.unwrap().consecutive_5xx, Some(3));
        assert_eq!(c.panic_threshold, Some(30));
    }

    #[test]
    fn inconsistent_settings_are_rejected() {
        let maglev = spec("targetRefs: []\nloadBalancer: { kind: Maglev }\n");
        assert!(
            maglev
                .check(TargetKind::Service)
                .unwrap_err()
                .contains("required")
        );
        let two = spec(
            "targetRefs: []\nloadBalancer: { kind: Maglev, hash: { header: a, sourceIp: true } }\n",
        );
        assert!(two.check(TargetKind::Service).is_err());
        let hc = spec("targetRefs: []\nhealthCheck: {}\n");
        assert!(hc.check(TargetKind::Service).is_err());
        let bad = spec("targetRefs: []\nretry: { backoff: soon }\n");
        assert!(
            bad.check(TargetKind::HttpRoute)
                .unwrap_err()
                .contains("retry.backoff")
        );
        let tls = spec("targetRefs: []\nlistener: { tlsMinVersion: '1.1' }\n");
        assert!(tls.check(TargetKind::Gateway).is_err());
    }

    #[test]
    fn rate_limits_split_between_replicas() {
        let p = RateLimitPolicy {
            requests: 100,
            per: "1s".into(),
            burst: Some(250),
            key_header: Some("x-user".into()),
        };
        let one = split_rate_limit(&p, 3).unwrap();
        assert_eq!(one.requests, 34);
        assert_eq!(one.burst, Some(84));
        assert_eq!(one.key, RateLimitKeyConfig::Header("x-user".into()));
        assert_eq!(
            split_rate_limit(&p, 0).unwrap().requests,
            100,
            "no replicas counts as one"
        );
    }

    proptest! {
        /// The fleet never allows less than the configured limit, and never
        /// more than one extra request per replica.
        #[test]
        fn split_covers_the_total(requests in 1u32..1_000_000, replicas in 1u32..500) {
            let p = RateLimitPolicy { requests, per: "1s".into(), ..RateLimitPolicy::default() };
            let share = split_rate_limit(&p, replicas).unwrap().requests;
            let total = u64::from(share) * u64::from(replicas);
            prop_assert!(total >= u64::from(requests));
            prop_assert!(total < u64::from(requests) + u64::from(replicas) + u64::from(replicas));
        }
    }

    #[test]
    fn crd_is_namespaced_with_status() {
        let crd = FerryPolicy::crd();
        assert_eq!(crd.spec.group, GROUP);
        assert_eq!(crd.spec.scope, "Namespaced");
        assert!(
            crd.spec.versions[0]
                .subresources
                .as_ref()
                .unwrap()
                .status
                .is_some()
        );
    }
}
