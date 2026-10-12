//! Validated TLS, limits, timeouts, retries, rate limits and upstream
//! resilience settings, with defaults filled in.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use http::HeaderName;
use serde::Serialize;

use super::ser;
pub use crate::policy::TlsVersion;

/// A value that must never be logged or exported, such as a private key.
/// `Debug` and `Serialize` print `<redacted>`.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub(crate) fn new(s: String) -> Self {
        Self(s)
    }

    /// The secret value. Call only where it is actually needed.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl Serialize for Secret {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("<redacted>")
    }
}

/// Where a certificate chain and key come from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub enum CertificateSource {
    /// PEM files, re-read when they change.
    Files {
        /// Certificate chain.
        cert: PathBuf,
        /// Private key.
        key: PathBuf,
    },
    /// Inline PEM.
    Pem {
        /// Certificate chain.
        cert: String,
        /// Private key.
        key: Secret,
    },
}

/// Where trusted CA certificates come from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub enum CaSource {
    /// A PEM file.
    File(PathBuf),
    /// Inline PEM.
    Pem(String),
}

/// TLS termination on a listener.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ListenerTls {
    pub(crate) certificates: Vec<CertificateSource>,
    pub(crate) min_version: TlsVersion,
}

impl ListenerTls {
    /// Certificates in config order; the first is the default.
    #[must_use]
    pub fn certificates(&self) -> &[CertificateSource] {
        &self.certificates
    }

    /// Lowest accepted TLS version.
    #[must_use]
    pub fn min_version(&self) -> TlsVersion {
        self.min_version
    }
}

/// Request limits of a listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Limits {
    pub(crate) max_request_headers_bytes: usize,
    pub(crate) max_request_headers: usize,
    pub(crate) max_request_body_bytes: Option<u64>,
    pub(crate) h2_max_concurrent_streams: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_request_headers_bytes: 60 * 1024,
            max_request_headers: 100,
            max_request_body_bytes: None,
            h2_max_concurrent_streams: 100,
        }
    }
}

impl Limits {
    /// Largest request header block in bytes.
    #[must_use]
    pub fn max_request_headers_bytes(&self) -> usize {
        self.max_request_headers_bytes
    }

    /// Most request headers.
    #[must_use]
    pub fn max_request_headers(&self) -> usize {
        self.max_request_headers
    }

    /// Largest request body, if limited.
    #[must_use]
    pub fn max_request_body_bytes(&self) -> Option<u64> {
        self.max_request_body_bytes
    }

    /// Most concurrent HTTP/2 streams per connection.
    #[must_use]
    pub fn h2_max_concurrent_streams(&self) -> u32 {
        self.h2_max_concurrent_streams
    }
}

/// Connection-level timeouts of a listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ListenerTimeouts {
    pub(crate) header_read: Duration,
    pub(crate) h2_keep_alive: Duration,
}

impl Default for ListenerTimeouts {
    fn default() -> Self {
        Self {
            header_read: Duration::from_secs(30),
            h2_keep_alive: Duration::from_secs(20),
        }
    }
}

impl ListenerTimeouts {
    /// Time allowed for request headers (and HTTP/1.1 keep-alive idleness).
    #[must_use]
    pub fn header_read(&self) -> Duration {
        self.header_read
    }

    /// HTTP/2 keep-alive ping interval and timeout.
    #[must_use]
    pub fn h2_keep_alive(&self) -> Duration {
        self.h2_keep_alive
    }
}

/// A local token-bucket rate limit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RateLimit {
    pub(crate) requests: u32,
    pub(crate) per: Duration,
    pub(crate) burst: u32,
    pub(crate) key: RateLimitKey,
}

impl RateLimit {
    /// Tokens added per `per`.
    #[must_use]
    pub fn requests(&self) -> u32 {
        self.requests
    }

    /// Refill period.
    #[must_use]
    pub fn per(&self) -> Duration {
        self.per
    }

    /// Bucket size.
    #[must_use]
    pub fn burst(&self) -> u32 {
        self.burst
    }

    /// Bucket key.
    #[must_use]
    pub fn key(&self) -> &RateLimitKey {
        &self.key
    }
}

/// What keys a rate-limit bucket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub enum RateLimitKey {
    /// Client IP.
    ClientIp,
    /// A request header's value.
    Header(#[serde(serialize_with = "ser::header_name")] HeaderName),
}

/// Per-request timeouts of a rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RouteTimeouts {
    pub(crate) request: Option<Duration>,
    pub(crate) backend_request: Option<Duration>,
    pub(crate) idle: Duration,
}

impl Default for RouteTimeouts {
    fn default() -> Self {
        Self {
            request: None,
            backend_request: None,
            idle: Duration::from_secs(300),
        }
    }
}

impl RouteTimeouts {
    /// Whole-request timeout, if any.
    #[must_use]
    pub fn request(&self) -> Option<Duration> {
        self.request
    }

    /// Per-attempt timeout, if any.
    #[must_use]
    pub fn backend_request(&self) -> Option<Duration> {
        self.backend_request
    }

    /// Body idle timeout.
    #[must_use]
    pub fn idle(&self) -> Duration {
        self.idle
    }
}

/// Retries for responses and failures that may have reached a backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RetryPolicy {
    pub(crate) attempts: u32,
    pub(crate) codes: Vec<u16>,
    pub(crate) backoff: Duration,
}

impl RetryPolicy {
    /// Most retries after the first attempt.
    #[must_use]
    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    /// Statuses that trigger a retry.
    #[must_use]
    pub fn codes(&self) -> &[u16] {
        &self.codes
    }

    /// Base backoff.
    #[must_use]
    pub fn backoff(&self) -> Duration {
        self.backoff
    }
}

/// TLS to a cluster's endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpstreamTls {
    pub(crate) sni: String,
    pub(crate) ca: Option<CaSource>,
    pub(crate) client_certificate: Option<CertificateSource>,
}

impl UpstreamTls {
    /// Server name sent and verified.
    #[must_use]
    pub fn sni(&self) -> &str {
        &self.sni
    }

    /// Trusted CAs; `None` means the Mozilla root store.
    #[must_use]
    pub fn ca(&self) -> Option<&CaSource> {
        self.ca.as_ref()
    }

    /// Client certificate for mTLS.
    #[must_use]
    pub fn client_certificate(&self) -> Option<&CertificateSource> {
        self.client_certificate.as_ref()
    }
}

/// Per-cluster concurrency limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CircuitBreakers {
    pub(crate) max_connections: u32,
    pub(crate) max_pending_requests: u32,
    pub(crate) max_requests: u32,
    pub(crate) max_retries: u32,
}

impl Default for CircuitBreakers {
    fn default() -> Self {
        Self {
            max_connections: 1024,
            max_pending_requests: 1024,
            max_requests: 1024,
            max_retries: 3,
        }
    }
}

impl CircuitBreakers {
    /// Most upstream connections.
    #[must_use]
    pub fn max_connections(&self) -> u32 {
        self.max_connections
    }

    /// Most requests waiting for a new connection.
    #[must_use]
    pub fn max_pending_requests(&self) -> u32 {
        self.max_pending_requests
    }

    /// Most requests in flight.
    #[must_use]
    pub fn max_requests(&self) -> u32 {
        self.max_requests
    }

    /// Most retries in flight.
    #[must_use]
    pub fn max_retries(&self) -> u32 {
        self.max_retries
    }
}

/// Passive outlier detection settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct OutlierDetection {
    pub(crate) consecutive_5xx: u32,
    pub(crate) consecutive_gateway_failure: u32,
    pub(crate) base_ejection_time: Duration,
    pub(crate) max_ejection_time: Duration,
    pub(crate) max_ejection_percent: u8,
}

impl OutlierDetection {
    /// Consecutive 5xx responses that eject.
    #[must_use]
    pub fn consecutive_5xx(&self) -> u32 {
        self.consecutive_5xx
    }

    /// Consecutive gateway failures that eject.
    #[must_use]
    pub fn consecutive_gateway_failure(&self) -> u32 {
        self.consecutive_gateway_failure
    }

    /// First ejection length.
    #[must_use]
    pub fn base_ejection_time(&self) -> Duration {
        self.base_ejection_time
    }

    /// Longest ejection.
    #[must_use]
    pub fn max_ejection_time(&self) -> Duration {
        self.max_ejection_time
    }

    /// Most endpoints ejected at once, in percent.
    #[must_use]
    pub fn max_ejection_percent(&self) -> u8 {
        self.max_ejection_percent
    }
}

/// Retry budget settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RetryBudget {
    pub(crate) budget_percent: u8,
    pub(crate) min_retries_per_second: u32,
    pub(crate) ttl: Duration,
}

impl Default for RetryBudget {
    fn default() -> Self {
        Self {
            budget_percent: 20,
            min_retries_per_second: 10,
            ttl: Duration::from_secs(10),
        }
    }
}

impl RetryBudget {
    /// Retries as a percentage of requests.
    #[must_use]
    pub fn budget_percent(&self) -> u8 {
        self.budget_percent
    }

    /// Retries always allowed per second.
    #[must_use]
    pub fn min_retries_per_second(&self) -> u32 {
        self.min_retries_per_second
    }

    /// Counting window.
    #[must_use]
    pub fn ttl(&self) -> Duration {
        self.ttl
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_never_print() {
        let s = Secret::new("-----BEGIN PRIVATE KEY-----".into());
        assert_eq!(format!("{s:?}"), "<redacted>");
        assert_eq!(serde_json::to_string(&s).unwrap(), "\"<redacted>\"");
        assert!(s.expose().starts_with("-----BEGIN"));
        let src = CertificateSource::Pem {
            cert: "c".into(),
            key: s,
        };
        assert!(!format!("{src:?}").contains("BEGIN"));
    }

    #[test]
    fn defaults_match_envoy() {
        let l = Limits::default();
        assert_eq!(l.max_request_headers_bytes(), 61_440);
        assert_eq!(l.max_request_headers(), 100);
        assert_eq!(CircuitBreakers::default().max_retries(), 3);
        assert_eq!(RouteTimeouts::default().idle(), Duration::from_secs(300));
        assert_eq!(RetryBudget::default().budget_percent(), 20);
    }
}
