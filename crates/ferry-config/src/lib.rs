//! Ferry's configuration model.
//!
//! Configuration flows one way: YAML text → [`Config`] (parsed, unvalidated) →
//! [`Snapshot`] (validated, versioned, immutable). The data plane only ever
//! holds `Snapshot`s, behind an `ArcSwap`, so a bad file can never reach the
//! request path.
//!
//! ```
//! use ferry_config::{from_yaml_str, SnapshotVersion};
//!
//! let yaml = r#"
//! listeners:
//!   - name: http
//!     address: 127.0.0.1:8080
//!     routes:
//!       - name: hello
//!         rules:
//!           - backends: [{ cluster: hello }]
//! clusters:
//!   - name: hello
//!     endpoints: [{ address: 127.0.0.1:9001 }]
//! "#;
//! let snapshot = from_yaml_str(yaml)?.into_snapshot(SnapshotVersion(1))?;
//! assert_eq!(snapshot.listeners()[0].name(), "http");
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # API stability
//!
//! The public API is frozen as of M1 ([ADR 0013]). Structs and enums are
//! `#[non_exhaustive]` and snapshot fields are private, so new optional
//! fields, variants and accessors are additive changes. Removing or changing
//! anything listed here needs a superseding ADR.
//!
//! [ADR 0013]: https://github.com/ashmit27j/ferry/blob/main/docs/adr/0013-freeze-ferry-config-api.md

mod duration;
mod error;
mod load;
mod model;
mod policy;
mod snapshot;
mod validate;

pub use duration::{Duration, ParseDurationError};
pub use error::{ConfigError, ValidationError, ValidationErrors};
pub use load::{MAX_CONFIG_BYTES, from_yaml_str, load_file, load_snapshot};
pub use model::{
    BackendRefConfig, ClusterConfig, Config, CookieHashConfig, EndpointConfig, FilterConfig,
    GrpcHealthCheckConfig, HashPolicyConfig, HeaderMatchConfig, HeaderModifierConfig,
    HeaderValueConfig, HealthCheckConfig, HttpHealthCheckConfig, LbPolicy, ListenerConfig,
    MatchConfig, PathMatchConfig, PathMatchKind, PathRewriteConfig, QueryParamMatchConfig,
    RedirectConfig, RouteConfig, RuleConfig, StringMatchKind, UpstreamProtocol, UrlRewriteConfig,
};
pub use policy::{
    CertificateConfig, CircuitBreakersConfig, ListenerLimitsConfig, ListenerTimeoutsConfig,
    ListenerTlsConfig, OutlierDetectionConfig, RateLimitConfig, RateLimitKeyConfig,
    RetryBudgetConfig, RetryConfig, RouteTimeoutsConfig, TlsVersion, UpstreamTlsConfig,
};
pub use snapshot::{
    CaSource, CertificateSource, CircuitBreakers, Limits, ListenerTimeouts, ListenerTls,
    OutlierDetection, RateLimit, RateLimitKey, RetryBudget, RetryPolicy, RouteTimeouts, Secret,
    UpstreamTls,
};
pub use snapshot::{
    Cluster, ClusterId, Endpoint, Filter, HashPolicy, HeaderMatch, HeaderModifier, HealthCheck,
    HealthCheckKind, HostMatch, HostSpecificity, Listener, MatchPrecedence, PathMatch, PathRewrite,
    Pattern, QueryParamMatch, Redirect, RedirectTarget, RequestMatch, Route, Rule, Snapshot,
    SnapshotVersion, StringMatch, UrlRewrite, WeightedBackend,
};
pub use validate::{
    MAX_BACKEND_WEIGHT, MAX_CONDITIONS, MAX_ENDPOINT_WEIGHT, MAX_NAME_LEN, MAX_PATH_LEN,
};

/// JSON Schema for the configuration file format, for editors and `check-config --schema`.
#[must_use]
pub fn json_schema() -> serde_json::Value {
    schemars::schema_for!(Config).to_value()
}
