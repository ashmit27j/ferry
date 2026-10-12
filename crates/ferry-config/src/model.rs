//! The on-disk configuration format.
//!
//! These types mirror the file a user writes (`examples/hello.yaml`). They are
//! deliberately permissive in shape and strict in vocabulary: unknown fields are
//! rejected, but cross-references (a route naming a cluster) and value ranges
//! are only checked by [`Config::into_snapshot`](crate::Config::into_snapshot),
//! which turns a `Config` into a validated, immutable [`Snapshot`](crate::Snapshot).
//!
//! The shape follows the Gateway API: listeners carry routes, routes carry
//! hostnames and rules, rules carry matches, filters and weighted backend
//! references. Field names are `snake_case` versions of the Gateway API names.
//!
//! Every struct and enum here is `#[non_exhaustive]`, so later milestones can add
//! optional fields and variants without breaking the frozen API (ADR 0013).

use std::net::SocketAddr;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::duration::Duration;
use crate::policy::{
    CircuitBreakersConfig, ListenerLimitsConfig, ListenerTimeoutsConfig, ListenerTlsConfig,
    OutlierDetectionConfig, RateLimitConfig, RetryBudgetConfig, RetryConfig, RouteTimeoutsConfig,
    UpstreamTlsConfig,
};

/// Root of a Ferry configuration file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
#[non_exhaustive]
pub struct Config {
    /// Sockets the proxy accepts traffic on, each with its own route table.
    pub listeners: Vec<ListenerConfig>,
    /// Named groups of upstream endpoints that routes forward to.
    pub clusters: Vec<ClusterConfig>,
}

/// A socket that accepts downstream connections.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
#[non_exhaustive]
pub struct ListenerConfig {
    /// Unique listener name, used in logs and metrics.
    pub name: String,
    /// Address to bind, for example `0.0.0.0:8080`.
    #[schemars(with = "String")]
    pub address: SocketAddr,
    /// Routes evaluated for requests on this listener.
    #[serde(default)]
    pub routes: Vec<RouteConfig>,
    /// Terminate TLS on this listener. Absent means cleartext (HTTP/1.1 and h2c).
    #[serde(default)]
    pub tls: Option<ListenerTlsConfig>,
    /// Expect a PROXY protocol header (v1 or v2) on every connection, as an
    /// AWS NLB sends, and take the client address from it.
    #[serde(default)]
    pub proxy_protocol: bool,
    /// Request size and concurrency limits.
    #[serde(default)]
    pub limits: Option<ListenerLimitsConfig>,
    /// Connection-level timeouts.
    #[serde(default)]
    pub timeouts: Option<ListenerTimeoutsConfig>,
}

/// A set of rules that apply to one or more hostnames (Gateway API `HTTPRoute`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
#[non_exhaustive]
pub struct RouteConfig {
    /// Unique route name within its listener.
    pub name: String,
    /// Hostnames this route serves. Empty means every hostname. A leading `*.`
    /// matches one or more labels in front of the suffix, but not the bare suffix.
    #[serde(default)]
    pub hostnames: Vec<String>,
    /// Rules evaluated in precedence order (see the router for the ordering).
    pub rules: Vec<RuleConfig>,
    /// Local rate limit applied to every request this route matches.
    #[serde(default)]
    pub rate_limit: Option<RateLimitConfig>,
}

/// One routing rule: if any match succeeds, apply the filters and forward to
/// the backends.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
#[non_exhaustive]
pub struct RuleConfig {
    /// Matches combined with OR. Empty means a single `PathPrefix: /` match,
    /// as in the Gateway API.
    #[serde(default)]
    pub matches: Vec<MatchConfig>,
    /// Filters applied in order to matching requests and their responses.
    #[serde(default)]
    pub filters: Vec<FilterConfig>,
    /// Weighted backends that matching requests are split across. Must be
    /// empty when the rule has a `RequestRedirect` filter, and non-empty otherwise.
    #[serde(default)]
    pub backends: Vec<BackendRefConfig>,
    /// Request timeouts.
    #[serde(default)]
    pub timeouts: Option<RouteTimeoutsConfig>,
    /// Retries beyond the always-on safe retries.
    #[serde(default)]
    pub retry: Option<RetryConfig>,
}

/// Conditions that must all hold for a request to match a rule.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
#[non_exhaustive]
pub struct MatchConfig {
    /// Path condition. Absent means `PathPrefix: /`.
    #[serde(default)]
    pub path: Option<PathMatchConfig>,
    /// HTTP method, for example `GET`. Absent matches every method.
    #[serde(default)]
    pub method: Option<String>,
    /// Header conditions, all of which must hold.
    #[serde(default)]
    pub headers: Vec<HeaderMatchConfig>,
    /// Query parameter conditions, all of which must hold.
    #[serde(default)]
    pub query_params: Vec<QueryParamMatchConfig>,
}

/// A path condition in Gateway API form: `{ type: PathPrefix, value: /api }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct PathMatchConfig {
    /// How `value` is compared with the request path.
    #[serde(rename = "type", default)]
    pub kind: PathMatchKind,
    /// The path or pattern to compare against. Literal paths must start with `/`.
    pub value: String,
}

/// Path comparison modes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub enum PathMatchKind {
    /// The request path equals `value` exactly.
    Exact,
    /// The request path starts with `value` on a `/` element boundary.
    #[default]
    PathPrefix,
    /// The whole request path matches the regular expression `value`
    /// (Rust `regex` syntax, implicitly anchored at both ends).
    RegularExpression,
}

/// A header condition: `{ name: x-env, value: canary }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct HeaderMatchConfig {
    /// How `value` is compared.
    #[serde(rename = "type", default)]
    pub kind: StringMatchKind,
    /// Header name, case-insensitive.
    pub name: String,
    /// Value or pattern. Repeated headers are joined with `,` before comparing.
    pub value: String,
}

/// A query parameter condition: `{ name: version, value: "2" }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct QueryParamMatchConfig {
    /// How `value` is compared.
    #[serde(rename = "type", default)]
    pub kind: StringMatchKind,
    /// Parameter name, case-sensitive. Only its first occurrence is compared.
    pub name: String,
    /// Value or pattern, compared with the raw (not percent-decoded) value.
    pub value: String,
}

/// Value comparison modes for headers and query parameters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub enum StringMatchKind {
    /// Byte-for-byte equality.
    #[default]
    Exact,
    /// The whole value matches the regular expression (implicitly anchored).
    RegularExpression,
}

/// A rule filter (Gateway API `HTTPRouteFilter`), tagged by `type`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum FilterConfig {
    /// Edits request headers before forwarding.
    RequestHeaderModifier(HeaderModifierConfig),
    /// Edits response headers before returning them downstream.
    ResponseHeaderModifier(HeaderModifierConfig),
    /// Answers with a redirect instead of forwarding.
    RequestRedirect(RedirectConfig),
    /// Rewrites the host and path before forwarding.
    #[serde(rename = "URLRewrite")]
    UrlRewrite(UrlRewriteConfig),
}

/// Header edits, applied in the order set, add, remove.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
#[non_exhaustive]
pub struct HeaderModifierConfig {
    /// Headers to overwrite, replacing every existing value.
    #[serde(default)]
    pub set: Vec<HeaderValueConfig>,
    /// Headers to append, keeping existing values.
    #[serde(default)]
    pub add: Vec<HeaderValueConfig>,
    /// Header names to delete.
    #[serde(default)]
    pub remove: Vec<String>,
}

/// A header name and value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct HeaderValueConfig {
    /// Header name, case-insensitive.
    pub name: String,
    /// Header value.
    pub value: String,
}

/// A redirect response. Absent fields keep the request's own value.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
#[non_exhaustive]
pub struct RedirectConfig {
    /// `http` or `https`.
    #[serde(default)]
    pub scheme: Option<String>,
    /// Hostname for the `Location` header.
    #[serde(default)]
    pub hostname: Option<String>,
    /// Path replacement.
    #[serde(default)]
    pub path: Option<PathRewriteConfig>,
    /// Port for the `Location` header. Absent derives it from the scheme.
    #[serde(default)]
    pub port: Option<u16>,
    /// One of 301, 302, 303, 307 or 308. Defaults to 302.
    #[serde(default)]
    pub status_code: Option<u16>,
}

/// Host and path rewriting before forwarding.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
#[non_exhaustive]
pub struct UrlRewriteConfig {
    /// Replaces the `Host` header (and `:authority`).
    #[serde(default)]
    pub hostname: Option<String>,
    /// Path replacement.
    #[serde(default)]
    pub path: Option<PathRewriteConfig>,
}

/// How a redirect or rewrite changes the path, tagged by `type`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", deny_unknown_fields)]
#[non_exhaustive]
pub enum PathRewriteConfig {
    /// Replace the whole path.
    ReplaceFullPath {
        /// The new path.
        replace_full_path: String,
    },
    /// Replace the part of the path that the rule's `PathPrefix` matched.
    ReplacePrefixMatch {
        /// The new prefix.
        replace_prefix_match: String,
    },
}

/// A reference from a rule to a cluster, with a traffic weight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
#[non_exhaustive]
pub struct BackendRefConfig {
    /// Name of a cluster in [`Config::clusters`].
    pub cluster: String,
    /// Relative share of traffic. Zero disables the backend.
    #[serde(default = "default_weight")]
    pub weight: u32,
}

/// A named group of upstream endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
#[non_exhaustive]
pub struct ClusterConfig {
    /// Unique cluster name.
    pub name: String,
    /// Protocol spoken to the endpoints.
    #[serde(default)]
    pub protocol: UpstreamProtocol,
    /// How requests are spread across endpoints.
    #[serde(default)]
    pub lb_policy: LbPolicy,
    /// What `maglev` hashes. Required for `maglev`, rejected otherwise.
    #[serde(default)]
    pub hash_policy: Option<HashPolicyConfig>,
    /// Active health checking. Absent means every endpoint is assumed healthy.
    #[serde(default)]
    pub health_check: Option<HealthCheckConfig>,
    /// When fewer than this percentage of endpoints are healthy, ignore health
    /// and balance across all of them (Envoy's panic mode). Defaults to 50.
    #[serde(default)]
    pub panic_threshold: Option<u8>,
    /// Upstream TCP connect timeout. Defaults to `5s`.
    #[serde(default)]
    pub connect_timeout: Option<Duration>,
    /// TLS to the endpoints, optionally with a client certificate (mTLS).
    #[serde(default)]
    pub tls: Option<UpstreamTlsConfig>,
    /// Concurrency limits; defaults apply when absent.
    #[serde(default)]
    pub circuit_breakers: Option<CircuitBreakersConfig>,
    /// Passive ejection of endpoints that keep failing. Absent disables it.
    #[serde(default)]
    pub outlier_detection: Option<OutlierDetectionConfig>,
    /// Cap on retries relative to traffic; defaults apply when absent.
    #[serde(default)]
    pub retry_budget: Option<RetryBudgetConfig>,
    /// Upstream endpoints. Must not be empty.
    pub endpoints: Vec<EndpointConfig>,
}

/// Upstream wire protocols. TLS arrives in M2.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum UpstreamProtocol {
    /// HTTP/1.1 with keep-alive.
    #[default]
    Http1,
    /// HTTP/2 over cleartext with prior knowledge (h2c). Required for gRPC.
    Http2,
}

/// Load-balancing policies (ADR 0010).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LbPolicy {
    /// Each healthy endpoint in turn, ignoring weights.
    #[default]
    RoundRobin,
    /// Each healthy endpoint in proportion to its weight, interleaved.
    WeightedRoundRobin,
    /// The less loaded of two random endpoints, by in-flight requests.
    LeastRequest,
    /// The cheaper of two random endpoints, by peak latency EWMA times load.
    P2cEwma,
    /// Consistent hashing with a Maglev table, keyed by `hash_policy`.
    Maglev,
}

/// The request property `maglev` hashes, for sticky routing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum HashPolicyConfig {
    /// The value of this request header. Requests without it are spread randomly.
    Header(String),
    /// The value of this cookie. When absent, Ferry generates one and sets it on
    /// the response, so the client sticks from its second request on.
    Cookie(CookieHashConfig),
    /// The downstream client IP address.
    SourceIp,
}

/// Cookie settings for cookie-based stickiness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
#[non_exhaustive]
pub struct CookieHashConfig {
    /// Cookie name.
    pub name: String,
    /// `Max-Age` of a generated cookie. Absent makes it a session cookie.
    #[serde(default)]
    pub ttl: Option<Duration>,
    /// `Path` of a generated cookie. Defaults to `/`.
    #[serde(default)]
    pub path: Option<String>,
}

/// Active health checking. Exactly one of `http` and `grpc` must be set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
#[non_exhaustive]
pub struct HealthCheckConfig {
    /// Time between checks of one endpoint. Defaults to `5s`.
    #[serde(default)]
    pub interval: Option<Duration>,
    /// Time allowed for one check. Defaults to `1s`.
    #[serde(default)]
    pub timeout: Option<Duration>,
    /// Consecutive failures that mark an endpoint unhealthy. Defaults to 3.
    #[serde(default)]
    pub unhealthy_threshold: Option<u32>,
    /// Consecutive successes that mark an endpoint healthy again. Defaults to 2.
    #[serde(default)]
    pub healthy_threshold: Option<u32>,
    /// Check with an HTTP GET.
    #[serde(default)]
    pub http: Option<HttpHealthCheckConfig>,
    /// Check with `grpc.health.v1.Health/Check`.
    #[serde(default)]
    pub grpc: Option<GrpcHealthCheckConfig>,
}

/// HTTP health check settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
#[non_exhaustive]
pub struct HttpHealthCheckConfig {
    /// Path to request, for example `/healthz`.
    pub path: String,
    /// `Host` header to send. Defaults to the cluster name.
    #[serde(default)]
    pub host: Option<String>,
    /// Statuses that count as healthy. Defaults to `[200]`.
    #[serde(default)]
    pub expected_statuses: Vec<u16>,
}

/// gRPC health check settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
#[non_exhaustive]
pub struct GrpcHealthCheckConfig {
    /// Service name sent in the request. Empty asks about the whole server.
    #[serde(default)]
    pub service: String,
    /// `:authority` to send. Defaults to the cluster name.
    #[serde(default)]
    pub authority: Option<String>,
}

/// A single upstream address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
#[non_exhaustive]
pub struct EndpointConfig {
    /// Socket address of the upstream, for example `10.0.3.17:8080`.
    #[schemars(with = "String")]
    pub address: SocketAddr,
    /// Relative weight for `weighted_round_robin` and `maglev`, `1..=128`.
    #[serde(default = "default_weight")]
    pub weight: u32,
}

const fn default_weight() -> u32 {
    1
}

// Constructors. The types are `#[non_exhaustive]`, so code outside this crate
// (the controller, tests) builds them with these and then sets optional fields.

impl Config {
    /// A config with the given listeners and clusters.
    #[must_use]
    pub fn new(listeners: Vec<ListenerConfig>, clusters: Vec<ClusterConfig>) -> Self {
        Self {
            listeners,
            clusters,
        }
    }
}

impl ListenerConfig {
    /// A listener with no routes.
    #[must_use]
    pub fn new(name: impl Into<String>, address: SocketAddr) -> Self {
        Self {
            name: name.into(),
            address,
            routes: Vec::new(),
            tls: None,
            proxy_protocol: false,
            limits: None,
            timeouts: None,
        }
    }
}

impl RouteConfig {
    /// A route for every hostname.
    #[must_use]
    pub fn new(name: impl Into<String>, rules: Vec<RuleConfig>) -> Self {
        Self {
            name: name.into(),
            hostnames: Vec::new(),
            rules,
            rate_limit: None,
        }
    }
}

impl RuleConfig {
    /// A catch-all rule forwarding to `backends`.
    #[must_use]
    pub fn forward(backends: Vec<BackendRefConfig>) -> Self {
        Self {
            backends,
            ..Self::default()
        }
    }
}

impl PathMatchConfig {
    /// A path condition.
    #[must_use]
    pub fn new(kind: PathMatchKind, value: impl Into<String>) -> Self {
        Self {
            kind,
            value: value.into(),
        }
    }
}

impl MatchConfig {
    /// A match on the path alone.
    #[must_use]
    pub fn path(path: PathMatchConfig) -> Self {
        Self {
            path: Some(path),
            ..Self::default()
        }
    }
}

impl HeaderMatchConfig {
    /// A header condition.
    #[must_use]
    pub fn new(kind: StringMatchKind, name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            kind,
            name: name.into(),
            value: value.into(),
        }
    }
}

impl QueryParamMatchConfig {
    /// A query parameter condition.
    #[must_use]
    pub fn new(kind: StringMatchKind, name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            kind,
            name: name.into(),
            value: value.into(),
        }
    }
}

impl HeaderValueConfig {
    /// A header name and value.
    #[must_use]
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
}

impl BackendRefConfig {
    /// A weighted reference to `cluster`.
    #[must_use]
    pub fn new(cluster: impl Into<String>, weight: u32) -> Self {
        Self {
            cluster: cluster.into(),
            weight,
        }
    }
}

impl ClusterConfig {
    /// A round-robin HTTP/1.1 cluster with default settings.
    #[must_use]
    pub fn new(name: impl Into<String>, endpoints: Vec<EndpointConfig>) -> Self {
        Self {
            name: name.into(),
            protocol: UpstreamProtocol::default(),
            lb_policy: LbPolicy::default(),
            hash_policy: None,
            health_check: None,
            panic_threshold: None,
            connect_timeout: None,
            tls: None,
            circuit_breakers: None,
            outlier_detection: None,
            retry_budget: None,
            endpoints,
        }
    }
}

impl CookieHashConfig {
    /// A session cookie at path `/`.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ttl: None,
            path: None,
        }
    }
}

impl HttpHealthCheckConfig {
    /// A check of `path` expecting 200.
    #[must_use]
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            host: None,
            expected_statuses: Vec::new(),
        }
    }
}

impl EndpointConfig {
    /// An endpoint with weight 1.
    #[must_use]
    pub fn new(address: SocketAddr) -> Self {
        Self {
            address,
            weight: default_weight(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SnapshotVersion;

    /// The constructors are how the controller (M4) builds configs; a config
    /// built with them must validate.
    #[test]
    fn constructors_build_a_valid_config() {
        let mut header_rule = RuleConfig::forward(vec![BackendRefConfig::new("c", 1)]);
        let mut m = MatchConfig::path(PathMatchConfig::new(PathMatchKind::Exact, "/x"));
        m.headers
            .push(HeaderMatchConfig::new(StringMatchKind::Exact, "x-a", "1"));
        m.query_params
            .push(QueryParamMatchConfig::new(StringMatchKind::Exact, "q", "2"));
        header_rule.matches.push(m);
        let mut modifier = HeaderModifierConfig::default();
        modifier.set.push(HeaderValueConfig::new("x-b", "2"));
        header_rule
            .filters
            .push(FilterConfig::RequestHeaderModifier(modifier));
        let mut redirect = RuleConfig::default();
        redirect
            .filters
            .push(FilterConfig::RequestRedirect(RedirectConfig::default()));
        let mut route = RouteConfig::new("r", vec![header_rule, redirect]);
        route.hostnames.push("a.example".into());
        let mut listener = ListenerConfig::new("l", "127.0.0.1:0".parse().unwrap());
        listener.routes.push(route);

        let mut cluster = ClusterConfig::new(
            "c",
            vec![EndpointConfig::new("10.0.0.1:80".parse().unwrap())],
        );
        cluster.lb_policy = LbPolicy::Maglev;
        cluster.hash_policy = Some(HashPolicyConfig::Cookie(CookieHashConfig::new("sid")));
        cluster.health_check = Some(HealthCheckConfig {
            http: Some(HttpHealthCheckConfig::new("/healthz")),
            ..HealthCheckConfig::default()
        });

        let snap = Config::new(vec![listener], vec![cluster])
            .into_snapshot(SnapshotVersion(3))
            .unwrap();
        assert_eq!(snap.listeners()[0].routes()[0].rules().len(), 2);
        assert!(snap.clusters()[0].health_check().is_some());
        let grpc = HealthCheckConfig {
            grpc: Some(GrpcHealthCheckConfig::default()),
            ..HealthCheckConfig::default()
        };
        assert!(grpc.http.is_none());
    }
}
