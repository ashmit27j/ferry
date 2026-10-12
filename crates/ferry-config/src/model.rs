//! The on-disk configuration format.
//!
//! These types mirror the file a user writes (`examples/hello.yaml`). They are
//! deliberately permissive in shape and strict in vocabulary: unknown fields are
//! rejected, but cross-references (a route naming a cluster) are only checked by
//! [`Config::into_snapshot`](crate::Config::into_snapshot), which turns a
//! `Config` into a validated, immutable [`Snapshot`](crate::Snapshot).
//!
//! The shape follows the Gateway API: listeners carry routes, routes carry
//! hostnames and rules, rules carry matches and weighted backend references.

use std::net::SocketAddr;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Root of a Ferry configuration file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct Config {
    /// Sockets the proxy accepts traffic on, each with its own route table.
    pub listeners: Vec<ListenerConfig>,
    /// Named groups of upstream endpoints that routes forward to.
    pub clusters: Vec<ClusterConfig>,
}

/// A socket that accepts downstream connections.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct ListenerConfig {
    /// Unique listener name, used in logs and metrics.
    pub name: String,
    /// Address to bind, for example `0.0.0.0:8080`.
    #[schemars(with = "String")]
    pub address: SocketAddr,
    /// Routes evaluated for requests on this listener.
    #[serde(default)]
    pub routes: Vec<RouteConfig>,
}

/// A set of rules that apply to one or more hostnames (Gateway API `HTTPRoute`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct RouteConfig {
    /// Unique route name within its listener.
    pub name: String,
    /// Hostnames this route serves. Empty means every hostname. A leading `*.`
    /// matches one or more labels in front of the suffix, but not the bare suffix.
    #[serde(default)]
    pub hostnames: Vec<String>,
    /// Rules evaluated in precedence order (see the router for the ordering).
    pub rules: Vec<RuleConfig>,
}

/// One routing rule: if any match succeeds, forward to the backends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct RuleConfig {
    /// Matches combined with OR. Empty means a single `PathPrefix: /` match,
    /// as in the Gateway API.
    #[serde(default)]
    pub matches: Vec<MatchConfig>,
    /// Weighted backends that matching requests are split across.
    pub backends: Vec<BackendRefConfig>,
}

/// Conditions that must all hold for a request to match a rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct MatchConfig {
    /// Path condition. Absent means `PathPrefix: /`.
    #[serde(default)]
    pub path: Option<PathMatchConfig>,
}

/// A path condition in Gateway API form: `{ type: PathPrefix, value: /api }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PathMatchConfig {
    /// How `value` is compared with the request path.
    #[serde(rename = "type", default)]
    pub kind: PathMatchKind,
    /// The path to compare against. Must start with `/`.
    pub value: String,
}

/// Path comparison modes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum PathMatchKind {
    /// The request path equals `value` exactly.
    Exact,
    /// The request path starts with `value` on a `/` element boundary.
    #[default]
    PathPrefix,
}

/// A reference from a rule to a cluster, with a traffic weight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
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
pub struct ClusterConfig {
    /// Unique cluster name.
    pub name: String,
    /// How requests are spread across endpoints.
    #[serde(default)]
    pub lb_policy: LbPolicy,
    /// Upstream endpoints. Must not be empty.
    pub endpoints: Vec<EndpointConfig>,
}

/// Load-balancing policies. More arrive in M1.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LbPolicy {
    /// Each endpoint in turn.
    #[default]
    RoundRobin,
}

/// A single upstream address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct EndpointConfig {
    /// Socket address of the upstream, for example `10.0.3.17:8080`.
    #[schemars(with = "String")]
    pub address: SocketAddr,
}

const fn default_weight() -> u32 {
    1
}
