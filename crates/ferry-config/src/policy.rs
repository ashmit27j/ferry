//! File-format types for TLS, limits, timeouts, retries, rate limits and
//! upstream resilience (M2). Like the rest of the model they are
//! `#[non_exhaustive]` and validated by
//! [`Config::into_snapshot`](crate::Config::into_snapshot).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::duration::Duration;

/// TLS termination settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
#[non_exhaustive]
pub struct ListenerTlsConfig {
    /// Certificates offered, chosen by SNI against each certificate's DNS
    /// names. The first is the default for clients whose SNI matches none.
    pub certificates: Vec<CertificateConfig>,
    /// Lowest TLS version accepted. Defaults to 1.2.
    #[serde(default)]
    pub min_version: Option<TlsVersion>,
}

/// A certificate chain and private key, from PEM files or inline PEM.
/// Exactly one of the two forms must be given.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
#[non_exhaustive]
pub struct CertificateConfig {
    /// PEM certificate chain file. Re-read when it changes.
    #[serde(default)]
    pub cert_file: Option<String>,
    /// PEM private key file. Re-read when it changes.
    #[serde(default)]
    pub key_file: Option<String>,
    /// Inline PEM certificate chain (used by the controller).
    #[serde(default)]
    pub cert_pem: Option<String>,
    /// Inline PEM private key (used by the controller).
    #[serde(default)]
    pub key_pem: Option<String>,
}

/// TLS protocol versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub enum TlsVersion {
    /// TLS 1.2.
    #[serde(rename = "1.2")]
    Tls12,
    /// TLS 1.3.
    #[serde(rename = "1.3")]
    Tls13,
}

/// Limits that protect the proxy from oversized or abusive requests.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
#[non_exhaustive]
pub struct ListenerLimitsConfig {
    /// Largest request header block in KiB (8–1024). Defaults to 60, as in Envoy.
    #[serde(default)]
    pub max_request_headers_kb: Option<u32>,
    /// Most request headers (1–1024). Defaults to 100, as in Envoy.
    #[serde(default)]
    pub max_request_headers: Option<u32>,
    /// Largest request body in bytes; larger requests get 413. Absent means no limit.
    #[serde(default)]
    pub max_request_body_bytes: Option<u64>,
    /// Most concurrent HTTP/2 streams per connection (1–10000). Defaults to 100.
    #[serde(default)]
    pub h2_max_concurrent_streams: Option<u32>,
    /// Most open downstream connections (1–1000000); further connections are
    /// closed on accept. Defaults to 100000 (ADR 0037).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_connections: Option<u32>,
}

/// Connection-level timeouts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
#[non_exhaustive]
pub struct ListenerTimeoutsConfig {
    /// Time allowed for a client to send complete request headers. On
    /// HTTP/1.1 this also bounds how long an idle keep-alive connection stays
    /// open. Defaults to `30s`.
    #[serde(default)]
    pub header_read: Option<Duration>,
    /// Interval of HTTP/2 keep-alive pings to clients; a client that does not
    /// answer within the same time is disconnected. Defaults to `20s`.
    #[serde(default)]
    pub h2_keep_alive: Option<Duration>,
    /// How long an upgraded connection (WebSocket) may carry no bytes in
    /// either direction before both sides are closed. Defaults to `1h`
    /// (ADR 0037).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel_idle: Option<Duration>,
}

/// A token bucket per key: `requests` tokens refill every `per`, up to `burst`.
///
/// In Kubernetes the controller sets `requests` to this replica's share of a
/// cluster-wide limit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
#[non_exhaustive]
pub struct RateLimitConfig {
    /// Requests allowed per `per`.
    pub requests: u32,
    /// The refill period, for example `1s` or `1m`.
    pub per: Duration,
    /// Bucket size. Defaults to `requests`.
    #[serde(default)]
    pub burst: Option<u32>,
    /// What the buckets are keyed by. Defaults to the client IP.
    #[serde(default)]
    pub key: RateLimitKeyConfig,
}

/// What separates one rate-limit bucket from another.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum RateLimitKeyConfig {
    /// The client IP (from PROXY protocol when enabled).
    #[default]
    ClientIp,
    /// The value of this request header; requests without it share one bucket.
    Header(String),
}

/// Per-request timeouts (Gateway API `HTTPRouteTimeouts`, plus `idle`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
#[non_exhaustive]
pub struct RouteTimeoutsConfig {
    /// Whole request, from receipt to the response headers, retries included.
    /// Absent means no limit. Expiry answers 504.
    #[serde(default)]
    pub request: Option<Duration>,
    /// One upstream attempt, to its response headers. Must not exceed `request`.
    #[serde(default)]
    pub backend_request: Option<Duration>,
    /// Longest silence on an open request or response body before the stream
    /// is reset. Defaults to `5m`, as in Envoy.
    #[serde(default)]
    pub idle: Option<Duration>,
}

/// Retries for responses and failures that may have reached a backend
/// (Gateway API GEP-1731). Requests that provably never arrived are always
/// retried, with or without this policy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
#[non_exhaustive]
pub struct RetryConfig {
    /// Most retries after the first attempt (0–10). Defaults to 2.
    #[serde(default)]
    pub attempts: Option<u32>,
    /// Response statuses that trigger a retry (500–599). Defaults to
    /// `[502, 503, 504]`.
    #[serde(default)]
    pub codes: Vec<u16>,
    /// Base delay before a retry, doubled each time, with full jitter.
    /// Defaults to `25ms`.
    #[serde(default)]
    pub backoff: Option<Duration>,
}

/// TLS to upstream endpoints.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
#[non_exhaustive]
pub struct UpstreamTlsConfig {
    /// Server name sent in SNI and verified against the certificate.
    /// Defaults to the cluster name.
    #[serde(default)]
    pub sni: Option<String>,
    /// PEM file of trusted CA certificates. With neither `ca_file` nor
    /// `ca_pem`, the Mozilla root store is trusted.
    #[serde(default)]
    pub ca_file: Option<String>,
    /// Inline PEM of trusted CA certificates.
    #[serde(default)]
    pub ca_pem: Option<String>,
    /// Client certificate for mutual TLS.
    #[serde(default)]
    pub client_certificate: Option<CertificateConfig>,
}

/// Per-cluster concurrency limits (Envoy's circuit breakers). Requests over a
/// limit fail fast with 503 and `x-ferry-overloaded: true`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
#[non_exhaustive]
pub struct CircuitBreakersConfig {
    /// Most upstream connections. Defaults to 1024.
    #[serde(default)]
    pub max_connections: Option<u32>,
    /// Most requests waiting for a new connection. Defaults to 1024.
    #[serde(default)]
    pub max_pending_requests: Option<u32>,
    /// Most requests in flight. Defaults to 1024.
    #[serde(default)]
    pub max_requests: Option<u32>,
    /// Most retries in flight. Defaults to 3.
    #[serde(default)]
    pub max_retries: Option<u32>,
}

/// Passive outlier detection, as in Envoy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
#[non_exhaustive]
pub struct OutlierDetectionConfig {
    /// Consecutive 5xx responses that eject an endpoint. Defaults to 5.
    #[serde(default)]
    pub consecutive_5xx: Option<u32>,
    /// Consecutive gateway failures (502, 503, 504, connect and reset errors)
    /// that eject an endpoint. Defaults to 5.
    #[serde(default)]
    pub consecutive_gateway_failure: Option<u32>,
    /// First ejection length; the n-th ejection lasts n times this.
    /// Defaults to `30s`.
    #[serde(default)]
    pub base_ejection_time: Option<Duration>,
    /// Longest ejection. Defaults to `300s`.
    #[serde(default)]
    pub max_ejection_time: Option<Duration>,
    /// Most endpoints ejected at once, as a percentage; one endpoint may
    /// always be ejected. Defaults to 10.
    #[serde(default)]
    pub max_ejection_percent: Option<u8>,
}

/// Retry budget (Linkerd's model): retries may add at most `budget_percent`
/// of recent requests, plus `min_retries_per_second` for quiet clusters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
#[non_exhaustive]
pub struct RetryBudgetConfig {
    /// Retries as a percentage of requests (0–100). Defaults to 20.
    #[serde(default)]
    pub budget_percent: Option<u8>,
    /// Retries always allowed per second. Defaults to 10.
    #[serde(default)]
    pub min_retries_per_second: Option<u32>,
    /// Window over which requests and retries are counted. Defaults to `10s`.
    #[serde(default)]
    pub ttl: Option<Duration>,
}

impl ListenerTlsConfig {
    /// TLS with these certificates, accepting TLS 1.2 and 1.3.
    #[must_use]
    pub fn new(certificates: Vec<CertificateConfig>) -> Self {
        Self {
            certificates,
            min_version: None,
        }
    }
}

impl CertificateConfig {
    /// A certificate read from PEM files.
    #[must_use]
    pub fn files(cert_file: impl Into<String>, key_file: impl Into<String>) -> Self {
        Self {
            cert_file: Some(cert_file.into()),
            key_file: Some(key_file.into()),
            ..Self::default()
        }
    }

    /// A certificate given as inline PEM.
    #[must_use]
    pub fn pem(cert_pem: impl Into<String>, key_pem: impl Into<String>) -> Self {
        Self {
            cert_pem: Some(cert_pem.into()),
            key_pem: Some(key_pem.into()),
            ..Self::default()
        }
    }
}

impl RateLimitConfig {
    /// `requests` per `per`, keyed by client IP, with a burst of `requests`.
    #[must_use]
    pub fn new(requests: u32, per: Duration) -> Self {
        Self {
            requests,
            per,
            burst: None,
            key: RateLimitKeyConfig::ClientIp,
        }
    }
}
